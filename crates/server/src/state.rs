//! The server core: a single task that owns all state.
//!
//! Connection tasks send [`Event`]s in; the core mutates state and pushes
//! pre-encoded frames out to each connection's writer queue. One owner means
//! no locks and a total order of events, and every broadcast is serialised
//! exactly once no matter how many clients receive it.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use t7_proto::perms::{Override, Perm, Permissions, Rank, RoleChange, RoleId, Standing, Subject};
use t7_proto::voice::{self, ToClient, ToServer};
use t7_proto::{
    BanId, BanInfo, C2S, ChannelId, ChannelInfo, ChannelKind, ChannelSettings, ClientId, ClientInfo, ClientStatus,
    Identity, LeaveReason, MAX_POKE_LEN, MAX_REASON_LEN, MAX_TEXT_LEN, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, S2C,
    ServerInfo, TextTarget, encode_frame, validate_nickname,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

use crate::bans::{self, Bans, NewBan, describe_duration};
use crate::channels::Channels;
use crate::roles::RoleStore;

/// A complete, ready-to-write frame shared between all recipients.
pub type Frame = Arc<[u8]>;

/// Outgoing frames buffered per client before we consider it too slow.
pub const OUTBOX_CAPACITY: usize = 1024;

pub enum Event {
    Join {
        addr: SocketAddr,
        protocol: u16,
        nickname: String,
        password: Option<String>,
        /// Proven on this connection; `None` if the proof didn't verify.
        identity: Option<Identity>,
        outbox: mpsc::Sender<Frame>,
        /// Receives the assigned id, or `None` if the handshake was rejected.
        reply: oneshot::Sender<Option<ClientId>>,
    },
    Message {
        id: ClientId,
        msg: C2S,
    },
    Left {
        id: ClientId,
        reason: LeaveReason,
    },
    /// A raw voice datagram, not yet authenticated.
    Voice {
        from: SocketAddr,
        data: Vec<u8>,
    },
}

struct Client {
    info: ClientInfo,
    addr: SocketAddr,
    outbox: mpsc::Sender<Frame>,
    text_bucket: TokenBucket,
    voice: voice::ServerSide,
    /// Learned from the first authenticated datagram (works behind NAT).
    voice_addr: Option<SocketAddr>,
    voice_bucket: TokenBucket,
    permissions: Permissions,
    known: HashMap<ClientId, ChannelId>,
}

impl Client {
    fn subject(&self) -> Subject {
        Subject { client: self.info.id, identity: self.info.identity }
    }
}

/// Rate limiter: `burst` events at once, refilled at `per_sec`.
struct TokenBucket {
    tokens: f32,
    burst: f32,
    per_sec: f32,
    last: Instant,
}

impl TokenBucket {
    /// Chat: 8 messages at once, then 2 per second.
    fn text() -> Self {
        Self::new(8.0, 2.0)
    }

    /// Voice: 50 frames per second is real time; allow some jitter on top.
    fn voice() -> Self {
        Self::new(40.0, 60.0)
    }

    fn new(burst: f32, per_sec: f32) -> Self {
        Self { tokens: burst, burst, per_sec, last: Instant::now() }
    }

    fn take(&mut self) -> bool {
        let now = Instant::now();
        let refill = now.duration_since(self.last).as_secs_f32() * self.per_sec;
        self.tokens = (self.tokens + refill).min(self.burst);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub struct Server {
    info: ServerInfo,
    password: Option<String>,
    channels: Channels,
    bans: Bans,
    roles: RoleStore,
    data_dir: Option<PathBuf>,
    clients: HashMap<ClientId, Client>,
    next_id: ClientId,
    /// Clients whose outbox overflowed or closed; removed after the current event.
    dead: Vec<ClientId>,
    udp: Arc<UdpSocket>,
    datagram: Vec<u8>,
}

impl Server {
    pub fn new(
        info: ServerInfo,
        password: Option<String>,
        channels: Channels,
        bans: Bans,
        data_dir: Option<PathBuf>,
        roles: RoleStore,
        udp: Arc<UdpSocket>,
    ) -> Self {
        Self {
            info,
            password,
            channels,
            bans,
            roles,
            data_dir,
            clients: HashMap::new(),
            next_id: 1,
            dead: Vec::new(),
            udp,
            datagram: Vec::with_capacity(voice::MAX_DATAGRAM),
        }
    }

    pub async fn run(mut self, mut events: mpsc::Receiver<Event>) {
        while let Some(ev) = events.recv().await {
            self.handle(ev);
            self.reap();
        }
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Join { addr, protocol, nickname, password, identity, outbox, reply } => {
                let id = self.join(addr, protocol, nickname, password, identity, outbox);
                let _ = reply.send(id);
            }
            Event::Message { id, msg } => self.message(id, msg),
            Event::Left { id, reason } => self.remove(id, reason),
            Event::Voice { from, mut data } => self.voice(from, &mut data),
        }
    }

    fn voice(&mut self, from: SocketAddr, data: &mut [u8]) {
        let Some(id) = voice::sender(data) else { return };
        let Some(sender) = self.clients.get_mut(&id) else { return };
        let Some(msg) = sender.voice.open(data) else { return };
        if sender.voice_addr != Some(from) {
            tracing::debug!(id, %from, "voice address learned");
            sender.voice_addr = Some(from);
        }
        match msg {
            ToServer::Ping { nonce } => {
                sender.voice.seal(ToClient::Pong { nonce }, &mut self.datagram);
                let _ = self.udp.try_send_to(&self.datagram, from);
            }
            ToServer::Audio { seq, payload } => {
                let status = sender.info.status;
                if status.muted || status.deafened || !sender.voice_bucket.take() {
                    return;
                }
                let channel = sender.info.channel;
                let audio = ToClient::Audio { from: id, seq, payload };
                let listeners = self
                    .clients
                    .values_mut()
                    .filter(|c| c.info.id != id && c.info.channel == channel && !c.info.status.deafened);
                for listener in listeners {
                    let Some(addr) = listener.voice_addr else { continue };
                    // Sealed per listener, with their key.
                    listener.voice.seal(audio, &mut self.datagram);
                    // Non-blocking: a full socket buffer drops the frame, like any lost packet.
                    let _ = self.udp.try_send_to(&self.datagram, addr);
                }
            }
        }
    }

    fn join(
        &mut self,
        addr: SocketAddr,
        protocol: u16,
        nickname: String,
        password: Option<String>,
        identity: Option<Identity>,
        outbox: mpsc::Sender<Frame>,
    ) -> Option<ClientId> {
        let reject = |reason: &str| {
            tracing::info!(%addr, reason, "rejected connection");
            let _ = outbox.try_send(encode(&S2C::Rejected { reason: reason.into() }));
            None
        };
        if protocol < MIN_PROTOCOL_VERSION {
            return reject(&format!(
                "protocol mismatch (server {PROTOCOL_VERSION}, client {protocol}); please update your T7"
            ));
        }
        if protocol > PROTOCOL_VERSION {
            return reject(&format!(
                "protocol mismatch (server {PROTOCOL_VERSION}, client {protocol}); this server needs an update"
            ));
        }
        let Some(identity) = identity else { return reject("your identity proof is invalid") };
        let now = unix_now();
        if let Some(ban) = self.bans.find(identity, addr.ip(), now) {
            return reject(&banned_message(ban, now));
        }
        if let Some(pw) = &self.password
            && password.as_deref() != Some(pw.as_str())
        {
            return reject("wrong server password");
        }
        if self.clients.len() as u32 >= self.info.max_clients {
            return reject("server is full");
        }
        let nickname = match validate_nickname(&nickname) {
            Ok(n) => self.unique_nickname(n),
            Err(e) => return reject(e),
        };

        let id = self.next_id;
        self.next_id += 1;
        let renamed = self.roles.seen(identity, &nickname);
        let role = self.roles.roles().title(identity);
        let permissions = self.permissions_of(Subject { client: id, identity });
        let channel = self.landing_channel(&permissions, None);
        let info = ClientInfo { id, nickname, channel, status: ClientStatus::default(), role, identity };
        let editor = permissions.edits_permissions();

        let seen = self.clients.values().filter(|c| permissions.sees(c.info.channel));
        let mut known: HashMap<ClientId, ChannelId> = seen.map(|c| (c.info.id, c.info.channel)).collect();
        known.insert(id, channel);
        let mut clients: Vec<ClientInfo> =
            self.clients.values().filter(|c| known.contains_key(&c.info.id)).map(|c| c.info.clone()).collect();
        clients.push(info.clone());
        let mut voice_key = [0u8; 32];
        if let Err(e) = getrandom::fill(&mut voice_key) {
            tracing::error!("no system randomness for the voice key: {e}");
            return reject("server error");
        }
        let welcome = S2C::Welcome {
            your_id: id,
            voice_key,
            server: self.info.clone(),
            channels: self.channel_list(&permissions),
            clients,
            permissions: permissions.clone(),
        };
        if outbox.try_send(encode(&welcome)).is_err() {
            return None;
        }

        tracing::info!(%addr, id, nickname = %info.nickname, "client joined");
        for c in self.clients.values_mut().filter(|c| c.permissions.sees(channel)) {
            c.known.insert(id, channel);
        }
        self.broadcast(&S2C::ClientJoined(info.clone()), |c| c.known.contains_key(&id));
        self.clients.insert(
            id,
            Client {
                info,
                addr,
                outbox,
                text_bucket: TokenBucket::text(),
                voice: voice::ServerSide::new(&voice_key),
                voice_addr: None,
                voice_bucket: TokenBucket::voice(),
                permissions,
                known,
            },
        );
        if renamed {
            self.save_roles();
        }
        if renamed || editor {
            let editors = |c: &Client| c.permissions.edits_permissions() && (renamed || c.info.id == id);
            self.broadcast(&S2C::Roles(self.roles.roles().clone()), editors);
        }
        Some(id)
    }

    /// If the name is taken, append a number instead of refusing.
    fn unique_nickname(&self, base: String) -> String {
        let taken = |n: &str| self.clients.values().any(|c| c.info.nickname.eq_ignore_ascii_case(n));
        if !taken(&base) {
            return base;
        }
        (1..).map(|i| format!("{base}{i}")).find(|n| !taken(n)).expect("infinite iterator")
    }

    fn landing_channel(&self, permissions: &Permissions, leaving: Option<ChannelId>) -> ChannelId {
        let default = self.channels.default_id();
        std::iter::once(default)
            .chain(self.channels.ids_in_order())
            .find(|&ch| {
                let joinable = self.channels.get(ch).is_some_and(|c| c.kind == ChannelKind::Voice);
                let open = self.has_room(ch) && self.channels.password_ok(ch, None);
                Some(ch) != leaving && joinable && permissions.may_in(ch, Perm::View) && open
            })
            .unwrap_or(default)
    }

    fn has_room(&self, channel: ChannelId) -> bool {
        match self.channels.get(channel).and_then(|c| c.max_clients) {
            Some(max) => self.count_in(channel) < max,
            None => true,
        }
    }

    fn count_in(&self, channel: ChannelId) -> u32 {
        self.clients.values().filter(|c| c.info.channel == channel).count() as u32
    }

    fn message(&mut self, id: ClientId, msg: C2S) {
        let Some(client) = self.clients.get(&id) else { return };
        let named = match &msg {
            C2S::JoinChannel { channel, .. }
            | C2S::EditChannel { id: channel, .. }
            | C2S::DeleteChannel { id: channel }
            | C2S::SetDefaultChannel { id: channel }
            | C2S::SetModerator { channel, .. }
            | C2S::SetOverride { channel, .. } => [Some(*channel), None],
            C2S::CreateChannel { parent, .. } => [*parent, None],
            C2S::MoveChannel { id: channel, parent, .. } => [Some(*channel), *parent],
            _ => [None, None],
        };
        if named.into_iter().flatten().any(|c| !client.permissions.sees(c)) {
            return self.error(id, "channel does not exist");
        }
        match msg {
            C2S::Hello { .. } => self.error(id, "already connected"),
            C2S::JoinChannel { channel, password } => self.join_channel(id, channel, password.as_deref()),
            C2S::SendText { target, text } => self.text(id, target, text),
            C2S::SetStatus(status) => {
                let client = self.clients.get_mut(&id).expect("checked above");
                if client.info.status != status {
                    client.info.status = status;
                    self.broadcast(&S2C::ClientStatus { id, status }, |c| c.known.contains_key(&id));
                }
            }
            C2S::Ping { nonce } => self.send(id, &S2C::Pong { nonce }),
            C2S::Goodbye => self.remove(id, LeaveReason::Disconnected),
            C2S::Poke { target, message } => self.poke(id, target, message),
            C2S::CreateChannel { parent, settings, temporary, kind } => {
                self.create_channel(id, parent, &settings, temporary, kind);
            }
            C2S::EditChannel { id: channel, settings } => self.edit_channel(id, channel, &settings),
            C2S::MoveChannel { id: channel, parent, position } => {
                let old_parent = self.channels.get(channel).and_then(|c| c.parent);
                let manages = |at| self.may(id, Perm::ManageChannels, at);
                if !(manages(Some(channel)) && manages(parent) && manages(old_parent)) {
                    return self.error(id, "you can't move this channel there");
                }
                match self.channels.move_to(channel, parent, position) {
                    Ok(()) => self.channels_changed(true),
                    Err(e) => self.error(id, &e.message()),
                }
            }
            C2S::DeleteChannel { id: channel } => self.delete_channel(id, channel),
            C2S::SetDefaultChannel { id: channel } => {
                if !self.may(id, Perm::ManageChannels, None) {
                    return self.error(id, "you can't change the default channel");
                }
                match self.channels.set_default(channel) {
                    Ok(()) => self.channels_changed(true),
                    Err(e) => self.error(id, &e.message()),
                }
            }
            C2S::ClaimOwnership { token } => self.claim(id, &token),
            C2S::SetModerator { channel, identity, moderator } => {
                let mine = self.standing(id, Some(channel));
                let roles = self.roles.roles();
                if !mine.perms.has(Perm::ManagePermissions) || mine.rank <= roles.height(RoleId::MODERATOR) {
                    return self.error(id, "you can't choose who moderates this channel");
                }
                if mine.rank <= roles.rank(identity) {
                    return self.error(id, "you can only choose moderators whose role is below yours");
                }
                let permanent = self.channels.get(channel).is_some_and(|c| !c.temporary);
                match self.channels.set_moderator(channel, identity, moderator) {
                    Ok(true) => self.channels_changed(permanent),
                    Ok(false) => {}
                    Err(e) => self.error(id, &e.message()),
                }
            }
            C2S::Kick { target, from_server, reason } => self.kick(id, target, from_server, &reason),
            C2S::Ban { target, duration_secs, reason, include_ip } => {
                self.ban(id, target, duration_secs, &reason, include_ip);
            }
            C2S::ListBans => self.list_bans(id),
            C2S::Unban { id: ban } => self.unban(id, ban),
            C2S::ChangeRoles(change) => self.change_roles(id, change),
            C2S::SetOverride { channel, role, allow, deny } => {
                self.set_override(id, channel, Override::new(role, allow, deny));
            }
        }
    }

    fn set_override(&mut self, id: ClientId, channel: ChannelId, set: Override) {
        let mine = self.standing(id, Some(channel));
        if !mine.perms.has(Perm::ManagePermissions) {
            return self.error(id, "you can't change this channel's permissions");
        }
        let roles = self.roles.roles();
        if set.role != RoleId::EVERYONE && roles.role(set.role).is_none() {
            return self.error(id, "that role does not exist");
        }
        if mine.rank <= roles.height(set.role) {
            return self.error(id, "you can only set permissions for roles below your own");
        }
        let permanent = self.channels.get(channel).is_some_and(|c| !c.temporary);
        match self.channels.set_override(channel, set) {
            Ok(true) => self.channels_changed(permanent),
            Ok(false) => {}
            Err(e) => self.error(id, &e.message()),
        }
    }

    fn change_roles(&mut self, id: ClientId, change: RoleChange) {
        let mine = self.standing(id, None);
        if !mine.perms.has(Perm::ManageRoles) {
            return self.error(id, "you can't manage roles");
        }
        let roles = self.roles.roles();
        let highest = match &change {
            RoleChange::Create { .. } => Rank::EVERYONE,
            RoleChange::Rename { id, .. } | RoleChange::SetPermissions { id, .. } | RoleChange::Delete { id } => {
                roles.height(*id)
            }
            RoleChange::Move { id, position } => roles.height(*id).max(roles.height_at(*position as usize)),
            RoleChange::Assign { identity, role, .. } => roles.height(*role).max(roles.rank(*identity)),
        };
        if mine.rank <= highest {
            return self.error(id, "you can only manage roles, and the roles of people, below your own");
        }
        let mut forgot = false;
        let changed = match change {
            RoleChange::Create { name } => self.roles.create(&name).map(drop),
            RoleChange::Rename { id, name } => self.roles.rename(id, &name),
            RoleChange::SetPermissions { id, perms } => self.roles.set_perms(id, perms),
            RoleChange::Move { id, position } => self.roles.move_to(id, position as usize),
            RoleChange::Delete { id } => self.roles.delete(id).map(|()| forgot = self.channels.forget_role(id)),
            RoleChange::Assign { identity, role, held: false } => self.roles.revoke(identity, role).map(drop),
            RoleChange::Assign { identity, role, held: true } => {
                let online = self.clients.values().find(|c| c.info.identity == identity);
                let Some(nickname) = online.map(|c| c.info.nickname.clone()) else {
                    return self.error(id, "they need to be online to get a role");
                };
                self.roles.grant(identity, &nickname, role).map(drop)
            }
        };
        match changed {
            Ok(()) => {
                self.save_roles();
                if forgot {
                    self.save_channels();
                }
                self.refresh(forgot, true);
            }
            Err(e) => self.error(id, &e.message()),
        }
    }

    fn join_channel(&mut self, id: ClientId, channel: ChannelId, password: Option<&str>) {
        let Some(info) = self.channels.get(channel) else { return self.error(id, "channel does not exist") };
        if info.kind == ChannelKind::Category {
            return self.error(id, "a category can't be joined; join a channel in it");
        }
        if self.clients[&id].info.channel == channel {
            return;
        }
        let may = self.standing(id, Some(channel)).perms;
        if !may.has(Perm::View) || !may.has(Perm::Join) {
            return self.error(id, "you can't join this channel");
        }
        if !may.has(Perm::Bypass) && !self.channels.password_ok(channel, password) {
            let msg = if password.is_some() { "wrong channel password" } else { "this channel needs a password" };
            return self.error(id, msg);
        }
        if !may.has(Perm::Bypass) && !self.has_room(channel) {
            return self.error(id, "channel is full");
        }
        self.move_to(id, channel);
        self.remove_abandoned_channels();
    }

    fn move_to(&mut self, id: ClientId, channel: ChannelId) {
        self.clients.get_mut(&id).expect("exists").info.channel = channel;
        self.sync_about(id);
    }

    fn sync_about(&mut self, subject: ClientId) {
        let info = self.clients[&subject].info.clone();
        let mut told = [Vec::new(), Vec::new(), Vec::new()];
        for c in self.clients.values_mut() {
            let sees = c.info.id == subject || c.permissions.sees(info.channel);
            let was = if sees { c.known.insert(subject, info.channel) } else { c.known.remove(&subject) };
            let news = match (was, sees) {
                (Some(at), true) if at != info.channel => 0,
                (None, true) => 1,
                (Some(_), false) => 2,
                _ => continue,
            };
            told[news].push(c.info.id);
        }
        let channel = info.channel;
        let news =
            [S2C::ClientMoved { id: subject, channel }, S2C::ClientShown(info), S2C::ClientHidden { id: subject }];
        for (msg, to) in news.iter().zip(told) {
            if !to.is_empty() {
                self.broadcast(msg, |c| to.contains(&c.info.id));
            }
        }
    }

    fn standing(&self, id: ClientId, channel: Option<ChannelId>) -> Standing {
        self.roles.roles().resolve(self.clients[&id].subject(), channel, &self.channels)
    }

    fn may(&self, id: ClientId, perm: Perm, channel: Option<ChannelId>) -> bool {
        self.standing(id, channel).perms.has(perm)
    }

    fn permissions_of(&self, who: Subject) -> Permissions {
        self.roles.roles().permissions(who, &self.channels)
    }

    fn refresh(&mut self, channels_changed: bool, roles_changed: bool) {
        let ids: Vec<ClientId> = self.clients.keys().copied().collect();
        let (mut regranted, mut lists, mut editors, mut retitled) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for &id in &ids {
            let client = &self.clients[&id];
            let (was, now) = (&client.permissions, self.permissions_of(client.subject()));
            let editor = now.edits_permissions();
            if channels_changed || editor != was.edits_permissions() || !was.channels.keys().eq(now.channels.keys()) {
                lists.push(id);
            }
            if editor && (roles_changed || !was.edits_permissions()) {
                editors.push(id);
            }
            if now != *was {
                regranted.push(id);
            }
            let role = self.roles.roles().title(client.info.identity);
            if role != client.info.role {
                retitled.push(id);
            }
            let client = self.clients.get_mut(&id).expect("exists");
            client.permissions = now;
            client.info.role = role;
        }
        let default = self.channels.default_id();
        let mut evicted = Vec::new();
        for c in self.clients.values_mut().filter(|c| !c.permissions.may_in(c.info.channel, Perm::View)) {
            c.info.channel = default;
            evicted.push(c.info.id);
        }

        for id in regranted {
            self.send(id, &S2C::Permissions(self.clients[&id].permissions.clone()));
        }
        self.send_channel_lists(&lists);
        for &id in evicted.iter().chain(ids.iter().filter(|id| !evicted.contains(id))) {
            self.sync_about(id);
        }
        if !editors.is_empty() {
            self.broadcast(&S2C::Roles(self.roles.roles().clone()), |c| editors.contains(&c.info.id));
        }
        for id in retitled {
            let role = self.clients[&id].info.role.clone();
            self.broadcast(&S2C::ClientRole { id, role }, |c| c.known.contains_key(&id));
        }
        if !evicted.is_empty() {
            self.remove_abandoned_channels();
        }
    }

    fn channel_list(&self, permissions: &Permissions) -> Vec<ChannelInfo> {
        let editor = permissions.edits_permissions();
        let list = permissions.channels.keys().filter_map(|c| self.channels.get(*c));
        list.map(|c| ChannelInfo { overrides: if editor { c.overrides.clone() } else { Vec::new() }, ..c.clone() })
            .collect()
    }

    fn send_channel_lists(&mut self, to: &[ClientId]) {
        let mut frames: HashMap<(Vec<ChannelId>, bool), Frame> = HashMap::new();
        for id in to {
            let permissions = &self.clients[id].permissions;
            let key = (permissions.channels.keys().copied().collect(), permissions.edits_permissions());
            let frame =
                frames.entry(key).or_insert_with(|| encode(&S2C::Channels(self.channel_list(permissions)))).clone();
            deliver(&mut self.dead, &self.clients[id], frame);
        }
    }

    fn create_channel(
        &mut self,
        id: ClientId,
        parent: Option<ChannelId>,
        settings: &ChannelSettings,
        temporary: bool,
        kind: ChannelKind,
    ) {
        let (needed, refusal) = if temporary && kind == ChannelKind::Voice {
            (Perm::CreateTemporary, "you can't create channels here")
        } else {
            (Perm::ManageChannels, "you can't create permanent channels here")
        };
        if !self.may(id, needed, parent) {
            return self.error(id, refusal);
        }
        if !self.clients.get_mut(&id).expect("exists").text_bucket.take() {
            return self.error(id, "you are creating channels too fast");
        }
        match self.channels.create(parent, settings, temporary, kind, id) {
            Ok(channel) => {
                self.channels_changed(!temporary);
                if kind == ChannelKind::Voice && self.clients[&id].permissions.may_in(channel, Perm::View) {
                    self.move_to(id, channel);
                }
                self.remove_abandoned_channels();
            }
            Err(e) => self.error(id, &e.message()),
        }
    }

    fn edit_channel(&mut self, id: ClientId, channel: ChannelId, settings: &ChannelSettings) {
        if !self.may(id, Perm::ManageChannels, Some(channel)) {
            return self.error(id, "you can't edit this channel");
        }
        let permanent = self.channels.get(channel).is_some_and(|c| !c.temporary);
        match self.channels.edit(channel, settings) {
            Ok(()) => self.channels_changed(permanent),
            Err(e) => self.error(id, &e.message()),
        }
    }

    fn delete_channel(&mut self, id: ClientId, channel: ChannelId) {
        if !self.may(id, Perm::ManageChannels, Some(channel)) {
            return self.error(id, "you can't delete this channel");
        }
        let permanent = self.channels.get(channel).is_some_and(|c| !c.temporary);
        let removed: HashSet<ChannelId> = match self.channels.delete(channel) {
            Ok(removed) => removed.into_iter().collect(),
            Err(e) => return self.error(id, &e.message()),
        };
        let default = self.channels.default_id();
        let evicted: Vec<ClientId> =
            self.clients.values().filter(|c| removed.contains(&c.info.channel)).map(|c| c.info.id).collect();
        for client in evicted {
            self.move_to(client, default);
        }
        self.channels_changed(permanent);
    }

    fn claim(&mut self, id: ClientId, token: &str) {
        let client = self.clients.get_mut(&id).expect("exists");
        if !client.text_bucket.take() {
            return self.error(id, "too many attempts, wait a moment");
        }
        let (identity, nickname) = (client.info.identity, client.info.nickname.clone());
        if !self.roles.claim(token, identity, &nickname) {
            return self.error(id, "that claim token is wrong or was already used");
        }
        tracing::info!(id, %nickname, identity = %identity.to_hex(), "server claimed: this is its owner now");
        self.save_roles();
        self.refresh(false, true);
    }

    fn refusal(
        &self,
        id: ClientId,
        target: ClientId,
        (perm, verb): (Perm, &str),
        channel: Option<ChannelId>,
    ) -> Option<String> {
        let (mine, theirs) = (self.standing(id, channel), self.standing(target, channel));
        let (actor, target) = (&self.clients[&id].info, &self.clients[&target].info);
        let name = &target.nickname;
        if !mine.perms.has(perm) {
            Some(format!("you can't {verb} {name}"))
        } else if actor.identity == target.identity {
            Some(format!("you can't {verb} yourself"))
        } else if mine.rank <= theirs.rank {
            Some(format!("you can't {verb} {name}: their role isn't below yours"))
        } else {
            None
        }
    }

    fn kick(&mut self, id: ClientId, target: ClientId, from_server: bool, reason: &str) {
        let Some(reason) = self.reason(id, reason) else { return };
        let Some(t) = self.clients.get(&target).filter(|_| self.knows(id, target)) else {
            return self.error(id, "that user is no longer online");
        };
        let channel = (!from_server).then_some(t.info.channel);
        if let Some(msg) = self.refusal(id, target, (Perm::Kick, "kick"), channel) {
            return self.error(id, &msg);
        }
        if channel == Some(self.channels.default_id()) {
            let msg = format!("{} is in the default channel: there is nowhere to kick them to", t.info.nickname);
            return self.error(id, &msg);
        }
        if !self.clients.get_mut(&id).expect("exists").text_bucket.take() {
            return self.error(id, "you are kicking too fast");
        }
        self.eject(target, id, from_server, reason);
    }

    /// Tells everyone, then moves `target` where a joining client would land, or disconnects it.
    fn eject(&mut self, target: ClientId, by: ClientId, from_server: bool, reason: String) {
        tracing::info!(target, by, from_server, reason, "client kicked");
        let notice = S2C::ClientKicked { id: target, by, from_server, reason };
        self.broadcast(&notice, |c| c.known.contains_key(&target));
        if from_server {
            // Its writer sends the notice before it closes the connection.
            self.remove(target, LeaveReason::Kicked);
        } else {
            let t = &self.clients[&target];
            let landing = self.landing_channel(&t.permissions, Some(t.info.channel));
            self.move_to(target, landing);
            self.remove_abandoned_channels();
        }
    }

    /// A kick or ban reason, trimmed; `None` (with an error sent) if it's too long.
    fn reason(&mut self, id: ClientId, raw: &str) -> Option<String> {
        let reason: String = raw.trim().chars().filter(|c| !c.is_control()).collect();
        if reason.chars().count() > MAX_REASON_LEN {
            self.error(id, &format!("reasons can have at most {MAX_REASON_LEN} characters"));
            return None;
        }
        Some(reason)
    }

    fn ban(&mut self, id: ClientId, target: ClientId, duration_secs: Option<u64>, reason: &str, include_ip: bool) {
        let Some(reason) = self.reason(id, reason) else { return };
        let Some(t) = self.clients.get(&target).filter(|_| self.knows(id, target)) else {
            return self.error(id, "that user is no longer online");
        };
        if let Some(msg) = self.refusal(id, target, (Perm::Ban, "ban"), None) {
            return self.error(id, &msg);
        }
        if duration_secs == Some(0) {
            return self.error(id, "a ban must last at least a second");
        }
        let actor = &self.clients[&id];
        // On a shared connection (a LAN, a household) this would lock the admin out too.
        if include_ip && t.addr.ip().to_canonical() == actor.addr.ip().to_canonical() {
            return self.error(id, &format!("{} connects from your IP address; ban them without it", t.info.nickname));
        }
        let ban = NewBan {
            identity: t.info.identity,
            ip: include_ip.then(|| t.addr.ip()),
            nickname: t.info.nickname.clone(),
            reason: reason.clone(),
            banned_by: actor.info.nickname.clone(),
            duration_secs,
        };
        let ban = self.bans.add(ban, unix_now()).clone();
        tracing::info!(id = ban.id, identity = %ban.identity.to_hex(), ip = ?ban.ip, by = %ban.banned_by, "ban added");
        self.save_bans();

        let length = match duration_secs {
            Some(secs) => format!("for {}", describe_duration(secs)),
            None => "for good".into(),
        };
        let notice = if reason.is_empty() { format!("banned {length}") } else { format!("banned {length} ({reason})") };
        // Everyone the ban would keep out if they connected now: with an IP
        // ban, that includes other identities at the address.
        let connected: Vec<ClientId> = self
            .clients
            .values()
            .filter(|c| bans::covers(&ban, c.info.identity, c.addr.ip()))
            .map(|c| c.info.id)
            .collect();
        for client in connected {
            self.eject(client, id, true, notice.clone());
        }
    }

    fn list_bans(&mut self, id: ClientId) {
        if !self.may(id, Perm::Ban, None) {
            return self.error(id, "you can't see the ban list");
        }
        if self.bans.prune(unix_now()) {
            self.save_bans();
        }
        self.send(id, &S2C::Bans(self.bans.list().to_vec()));
    }

    fn unban(&mut self, id: ClientId, ban: BanId) {
        if !self.may(id, Perm::Ban, None) {
            return self.error(id, "you can't lift bans");
        }
        match self.bans.remove(ban) {
            Some(lifted) => {
                tracing::info!(id = ban, identity = %lifted.identity.to_hex(), "ban lifted");
                self.save_bans();
            }
            None => self.error(id, "that ban has already ended"),
        }
        self.list_bans(id);
    }

    fn save_roles(&self) {
        if let Some(path) = self.data_dir.as_ref().map(|d| d.join("roles.toml"))
            && let Err(e) = self.roles.save(&path)
        {
            tracing::error!("cannot save roles to {}: {e}", path.display());
        }
    }

    fn save_bans(&self) {
        if let Some(path) = self.data_dir.as_ref().map(|d| d.join("bans.toml"))
            && let Err(e) = self.bans.save(&path)
        {
            tracing::error!("cannot save bans to {}: {e}", path.display());
        }
    }

    fn channels_changed(&mut self, save: bool) {
        self.refresh(true, false);
        if save {
            self.save_channels();
        }
    }

    fn save_channels(&self) {
        if let Some(path) = self.data_dir.as_ref().map(|d| d.join("channels.toml"))
            && let Err(e) = self.channels.save(&path)
        {
            tracing::error!("cannot save channels to {}: {e}", path.display());
        }
    }

    /// Temporary channels disappear once nobody is in them (or below them).
    fn remove_abandoned_channels(&mut self) {
        let occupied: HashSet<ChannelId> = self.clients.values().map(|c| c.info.channel).collect();
        let abandoned = self.channels.abandoned_temporaries(&occupied);
        if abandoned.is_empty() {
            return;
        }
        for channel in abandoned {
            let _ = self.channels.delete(channel);
        }
        self.channels_changed(false);
    }

    fn poke(&mut self, id: ClientId, target: ClientId, message: String) {
        let message = message.trim();
        if message.chars().count() > MAX_POKE_LEN {
            return self.error(id, "poke message is too long");
        }
        if let Some(why) = self.unreachable(id, target) {
            return self.error(id, &why);
        }
        if !self.clients.get_mut(&id).expect("exists").text_bucket.take() {
            return self.error(id, "you are poking too fast");
        }
        self.send(target, &S2C::Poked { from: id, message: message.to_owned() });
    }

    fn text(&mut self, id: ClientId, target: TextTarget, text: String) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if text.chars().count() > MAX_TEXT_LEN {
            return self.error(id, "message is too long");
        }
        let channel = self.clients[&id].info.channel;
        let place = matches!(target, TextTarget::Channel).then_some(channel);
        if !self.may(id, Perm::SendMessages, place) {
            return self.error(id, "you can't send messages here");
        }
        if !self.clients.get_mut(&id).expect("exists").text_bucket.take() {
            return self.error(id, "you are sending messages too fast");
        }
        let msg = S2C::Text { from: id, target, channel, text: text.to_owned() };
        match target {
            TextTarget::Server => self.broadcast(&msg, |c| c.known.contains_key(&id)),
            TextTarget::Channel => self.broadcast(&msg, |c| c.info.channel == channel),
            TextTarget::Client(to) => {
                if let Some(why) = self.unreachable(id, to) {
                    return self.error(id, &why);
                }
                // Recipient and sender (echo, so the sender sees it confirmed).
                self.broadcast(&msg, |c| c.info.id == to || c.info.id == id);
            }
        }
    }

    fn knows(&self, id: ClientId, other: ClientId) -> bool {
        self.clients.get(&id).is_some_and(|c| c.known.contains_key(&other))
    }

    fn unreachable(&self, id: ClientId, target: ClientId) -> Option<String> {
        if target == id || !self.knows(id, target) {
            Some("that user is no longer online".into())
        } else if !self.knows(target, id) {
            Some(format!("{} can't see you", self.clients[&target].info.nickname))
        } else {
            None
        }
    }

    fn remove(&mut self, id: ClientId, reason: LeaveReason) {
        if let Some(client) = self.clients.remove(&id) {
            tracing::info!(addr = %client.addr, id, nickname = %client.info.nickname, ?reason, "client left");
            // Dropping `client.outbox` ends its writer task, which closes the socket.
            self.broadcast(&S2C::ClientLeft { id, reason }, |c| c.known.contains_key(&id));
            for c in self.clients.values_mut() {
                c.known.remove(&id);
            }
            self.remove_abandoned_channels();
        }
    }

    fn reap(&mut self) {
        while let Some(id) = self.dead.pop() {
            self.remove(id, LeaveReason::TimedOut);
        }
    }

    fn error(&mut self, id: ClientId, message: &str) {
        self.send(id, &S2C::Error { message: message.into() });
    }

    fn send(&mut self, id: ClientId, msg: &S2C) {
        self.broadcast(msg, |c| c.info.id == id);
    }

    fn broadcast(&mut self, msg: &S2C, filter: impl Fn(&Client) -> bool) {
        let frame = encode(msg);
        for client in self.clients.values().filter(|c| filter(c)) {
            deliver(&mut self.dead, client, frame.clone());
        }
    }
}

fn deliver(dead: &mut Vec<ClientId>, client: &Client, frame: Frame) {
    if client.outbox.try_send(frame).is_err() && !dead.contains(&client.info.id) {
        dead.push(client.info.id);
    }
}

fn encode(msg: &S2C) -> Frame {
    encode_frame(msg).into()
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// What a banned client is told when it tries to join.
fn banned_message(ban: &BanInfo, now: u64) -> String {
    let mut msg = "You are banned from this server".to_owned();
    if !ban.reason.is_empty() {
        let _ = write!(msg, " ({})", ban.reason);
    }
    msg.push('.');
    if let Some(end) = ban.expires_unix {
        let _ = write!(msg, " The ban ends in {}.", describe_duration(end.saturating_sub(now)));
    }
    msg
}
