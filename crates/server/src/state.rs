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

use t7_proto::perms::{Override, Perm, Permissions, Rank, RoleChange, RoleId, Roles, Scope, Standing, Subject};
use t7_proto::voice::{self, ToClient, ToServer};
use t7_proto::{
    BanId, BanInfo, C2S, ChannelId, ChannelInfo, ChannelKind, ChannelSettings, ClientId, ClientInfo, ClientStatus,
    Identity, LeaveReason, MAX_POKE_LEN, MAX_REASON_LEN, MAX_TEXT_LEN, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
    PasswordChange, S2C, ServerInfo, Speech, TextTarget, encode_frame, validate_nickname,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

use crate::bans::{self, Bans, NewBan, describe_duration};
use crate::channels::{ChannelError, Channels};
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
    watching: Option<ClientId>,
    video_bucket: TokenBucket,
    keyframe_asked: Option<Instant>,
    last_sent_permissions: Permissions,
    last_sent_locations: HashMap<ClientId, ChannelId>,
    arrived: Instant,
}

impl Client {
    fn subject(&self) -> Subject {
        Subject { client: self.info.id, identity: self.info.identity }
    }
}

const VIDEO_BITS_PER_SEC: u32 = 12_000_000;

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

    fn video() -> Self {
        Self::new(voice::MAX_ACCESS_UNIT as f32, VIDEO_BITS_PER_SEC as f32 / 8.0)
    }

    fn new(burst: f32, per_sec: f32) -> Self {
        Self { tokens: burst, burst, per_sec, last: Instant::now() }
    }

    fn take(&mut self) -> bool {
        self.take_n(1.0)
    }

    fn take_n(&mut self, cost: f32) -> bool {
        let now = Instant::now();
        let refill = now.duration_since(self.last).as_secs_f32() * self.per_sec;
        self.tokens = (self.tokens + refill).min(self.burst);
        self.last = now;
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RolesTo {
    EveryEditor,
    NewEditors,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChannelListsTo {
    Everyone,
    WhoseViewChanged,
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
    mutes: HashSet<Identity>,
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
            mutes: HashSet::new(),
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
                if sender.info.speech != Speech::Allowed
                    || status.muted
                    || status.deafened
                    || !sender.voice_bucket.take()
                {
                    return;
                }
                let channel = sender.info.channel;
                let listeners = self
                    .clients
                    .values_mut()
                    .filter(|c| c.info.id != id && c.info.channel == channel && !c.info.status.deafened);
                let audio = ToClient::Audio { from: id, seq, payload };
                forward(&self.udp, &mut self.datagram, audio, listeners);
            }
            ToServer::Video { frame, part, parts, key, payload } => {
                if !sender.info.sharing
                    || sender.info.speech != Speech::Allowed
                    || !sender.video_bucket.take_n(payload.len() as f32)
                {
                    return;
                }
                let channel = sender.info.channel;
                let viewers = self.clients.values_mut().filter(|c| c.watching == Some(id) && c.info.channel == channel);
                let video = ToClient::Video { from: id, frame, part, parts, key, payload };
                forward(&self.udp, &mut self.datagram, video, viewers);
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
        let renamed = self.roles.refresh_nickname(identity, &nickname);
        let role = self.roles.roles().title(identity);
        let permissions = self.permissions_of(Subject { client: id, identity });
        let channel = self.landing_channel(&permissions, None);
        let status = ClientStatus::default();
        let speech = speech(self.mutes.contains(&identity), &permissions, channel);
        let info = ClientInfo { id, nickname, channel, status, role, identity, speech, sharing: false };
        let editor = permissions.edits_permissions();

        let seen = self.clients.values().filter(|c| permissions.sees(c.info.channel));
        let mut locations: HashMap<ClientId, ChannelId> = seen.map(|c| (c.info.id, c.info.channel)).collect();
        locations.insert(id, channel);
        let mut clients: Vec<ClientInfo> =
            self.clients.values().filter(|c| locations.contains_key(&c.info.id)).map(|c| c.info.clone()).collect();
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
        for c in self.clients.values_mut().filter(|c| c.last_sent_permissions.sees(channel)) {
            c.last_sent_locations.insert(id, channel);
        }
        self.broadcast(&S2C::ClientJoined(info.clone()), |c| c.last_sent_locations.contains_key(&id));
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
                watching: None,
                video_bucket: TokenBucket::video(),
                keyframe_asked: None,
                last_sent_permissions: permissions,
                last_sent_locations: locations,
                arrived: Instant::now(),
            },
        );
        if renamed {
            self.save_roles();
        }
        if renamed || editor {
            let editors = |c: &Client| c.last_sent_permissions.edits_permissions() && (renamed || c.info.id == id);
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
        if channels_named(&msg).any(|c| !client.last_sent_permissions.sees(c)) {
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
                    self.broadcast(&S2C::ClientStatus { id, status }, |c| c.last_sent_locations.contains_key(&id));
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
                let manages = |scope| self.may(id, Perm::ManageChannels, scope);
                if !(manages(Scope::Channel(channel))
                    && manages(Scope::within(parent))
                    && manages(Scope::within(old_parent)))
                {
                    return self.error(id, "you can't move this channel there");
                }
                match self.channels.move_to(channel, parent, position) {
                    Ok(()) => {
                        self.channels_changed();
                        self.save_channels();
                    }
                    Err(e) => self.error(id, &e.message()),
                }
            }
            C2S::DeleteChannel { id: channel } => self.delete_channel(id, channel),
            C2S::SetDefaultChannel { id: channel } => {
                if !self.may(id, Perm::ManageChannels, Scope::Server) {
                    return self.error(id, "you can't change the default channel");
                }
                match self.channels.set_default(channel) {
                    Ok(()) => {
                        self.channels_changed();
                        self.save_channels();
                    }
                    Err(e) => self.error(id, &e.message()),
                }
            }
            C2S::ClaimOwnership { token } => self.claim(id, &token),
            C2S::SetModerator { channel, identity, moderator } => {
                let mine = self.standing(id, Scope::Channel(channel));
                let roles = self.roles.roles();
                if !mine.perms.has(Perm::ManagePermissions) || mine.rank <= roles.height(RoleId::MODERATOR) {
                    return self.error(id, "you can't choose who moderates this channel");
                }
                if mine.rank <= roles.rank(identity) {
                    return self.error(id, "you can only choose moderators whose role is below yours");
                }
                let permanent = self.channels.get(channel).is_some_and(|c| !c.temporary);
                match self.channels.set_moderator(channel, identity, moderator) {
                    Ok(true) => {
                        self.channels_changed();
                        if permanent {
                            self.save_channels();
                        }
                    }
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
                self.set_override(id, channel, Override::allowing(role, allow).and_denying(deny));
            }
            C2S::ServerMute { target, muted } => self.server_mute(id, target, muted),
            C2S::MoveMember { target, channel } => self.move_member(id, target, channel),
            C2S::SetSharing(sharing) => self.set_sharing(id, sharing),
            C2S::Watch(target) => self.watch(id, target),
            C2S::NeedKeyframe { from } => {
                if self.clients[&id].watching == Some(from) {
                    self.ask_for_keyframe(from);
                }
            }
        }
    }

    fn set_sharing(&mut self, id: ClientId, sharing: bool) {
        let client = self.clients.get_mut(&id).expect("exists");
        if sharing && client.info.speech != Speech::Allowed {
            return self.error(id, "you can't share your screen here");
        }
        if client.info.sharing == sharing {
            return;
        }
        client.info.sharing = sharing;
        if !sharing {
            for c in self.clients.values_mut().filter(|c| c.watching == Some(id)) {
                c.watching = None;
            }
        }
        self.broadcast(&S2C::ClientSharing { id, sharing }, |c| c.last_sent_locations.contains_key(&id));
    }

    fn watch(&mut self, id: ClientId, target: Option<ClientId>) {
        let channel = self.clients[&id].info.channel;
        let sharing_here =
            |t: &ClientId| self.clients.get(t).is_some_and(|c| c.info.sharing && c.info.channel == channel);
        let target = target.filter(|t| *t != id && sharing_here(t));
        self.clients.get_mut(&id).expect("exists").watching = target;
        if let Some(sharer) = target {
            self.ask_for_keyframe(sharer);
        }
    }

    fn ask_for_keyframe(&mut self, sharer: ClientId) {
        let client = self.clients.get_mut(&sharer).expect("a watched sharer exists");
        let now = Instant::now();
        if client.keyframe_asked.is_some_and(|at| now.duration_since(at) < voice::KEYFRAME_INTERVAL) {
            return;
        }
        client.keyframe_asked = Some(now);
        self.send(sharer, &S2C::SendKeyframe);
    }

    fn move_member(&mut self, id: ClientId, target: ClientId, channel: ChannelId) {
        let Some(t) = self.clients.get(&target).filter(|_| self.knows(id, target)) else {
            return self.error(id, "that user is no longer online");
        };
        let (from, name, sees) =
            (t.info.channel, t.info.nickname.clone(), t.last_sent_permissions.may_in(channel, Perm::View));
        let Some(to) = self.channels.get(channel) else { return self.error(id, "channel does not exist") };
        let (kind, to) = (to.kind, to.name.clone());
        if kind == ChannelKind::Category {
            return self.error(id, "nobody can be in a category; move them into a channel in it");
        }
        if from == channel {
            return;
        }
        if let Some(msg) = self.refusal(id, target, (Perm::MoveMembers, "move"), Scope::Channel(from)) {
            return self.error(id, &msg);
        }
        let mine = self.standing(id, Scope::Channel(channel)).perms;
        if !mine.has(Perm::MoveMembers) {
            return self.error(id, &format!("you can't move people into {to}"));
        }
        if !sees {
            return self.error(id, &format!("{name} can't see {to}"));
        }
        if !mine.has(Perm::Bypass) && !self.channels.password_ok(channel, None) {
            return self.error(id, &format!("{to} has a password"));
        }
        if !mine.has(Perm::Bypass) && !self.has_room(channel) {
            return self.error(id, &format!("{to} is full"));
        }
        if !self.clients.get_mut(&id).expect("exists").text_bucket.take() {
            return self.error(id, "you are moving people too fast");
        }
        tracing::info!(target, by = id, channel, "client moved");
        if let Err(e) = self.enter_or_create_room(target, channel) {
            self.error(id, &format!("no room for {name} there: {}", e.message()));
        }
    }

    fn server_mute(&mut self, id: ClientId, target: ClientId, muted: bool) {
        let Some(t) = self.clients.get(&target).filter(|_| self.knows(id, target)) else {
            return self.error(id, "that user is no longer online");
        };
        let (channel, identity) = (t.info.channel, t.info.identity);
        let verb = if muted { "mute" } else { "unmute" };
        if let Some(msg) = self.refusal(id, target, (Perm::MuteMembers, verb), Scope::Channel(channel)) {
            return self.error(id, &msg);
        }
        let changed = if muted { self.mutes.insert(identity) } else { self.mutes.remove(&identity) };
        if !changed {
            return;
        }
        tracing::info!(target, by = id, muted, "server mute");
        let theirs: Vec<ClientId> =
            self.clients.values().filter(|c| c.info.identity == identity).map(|c| c.info.id).collect();
        for client in theirs {
            self.sync_about(client);
        }
    }

    fn set_override(&mut self, id: ClientId, channel: ChannelId, set: Override) {
        let mine = self.standing(id, Scope::Channel(channel));
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
            Ok(true) => {
                self.channels_changed();
                if permanent {
                    self.save_channels();
                }
            }
            Ok(false) => {}
            Err(e) => self.error(id, &e.message()),
        }
    }

    fn change_roles(&mut self, id: ClientId, change: RoleChange) {
        let mine = self.standing(id, Scope::Server);
        if !mine.perms.has(Perm::ManageRoles) {
            return self.error(id, "you can't manage roles");
        }
        if mine.rank <= highest_rank_touched(self.roles.roles(), &change) {
            return self.error(id, "you can only manage roles, and the roles of people, below your own");
        }
        let mut overrides_forgotten_in = Vec::new();
        let changed = match change {
            RoleChange::Create { name } => self.roles.create(&name).map(drop),
            RoleChange::Rename { id, name } => self.roles.rename(id, &name),
            RoleChange::SetPermissions { id, perms } => self.roles.set_perms(id, perms),
            RoleChange::Move { id, position } => self.roles.move_to(id, position as usize),
            RoleChange::Delete { id } => {
                self.roles.delete(id).map(|()| overrides_forgotten_in = self.channels.forget_role(id))
            }
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
                if self.channels.any_permanent(&overrides_forgotten_in) {
                    self.save_channels();
                }
                let channel_lists_to = if overrides_forgotten_in.is_empty() {
                    ChannelListsTo::WhoseViewChanged
                } else {
                    ChannelListsTo::Everyone
                };
                self.refresh(channel_lists_to, RolesTo::EveryEditor);
            }
            Err(e) => self.error(id, &e.message()),
        }
    }

    fn join_channel(&mut self, id: ClientId, channel: ChannelId, password: Option<&str>) {
        let Some(kind) = self.channels.get(channel).map(|c| c.kind) else {
            return self.error(id, "channel does not exist");
        };
        if kind == ChannelKind::Category {
            return self.error(id, "a category can't be joined; join a channel in it");
        }
        if self.clients[&id].info.channel == channel {
            return;
        }
        let may = self.standing(id, Scope::Channel(channel)).perms;
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
        if kind == ChannelKind::Lobby && !self.clients.get_mut(&id).expect("exists").text_bucket.take() {
            return self.error(id, "you are opening rooms too fast");
        }
        if let Err(e) = self.enter_or_create_room(id, channel) {
            self.error(id, &format!("no room for you there: {}", e.message()));
        }
    }

    fn enter_or_create_room(&mut self, id: ClientId, channel: ChannelId) -> Result<(), ChannelError> {
        let lobby = self.channels.get(channel).is_some_and(|c| c.kind == ChannelKind::Lobby);
        let channel = if lobby {
            let name = self.channels.room_name(channel, &self.clients[&id].info.nickname);
            let room =
                ChannelSettings { name, topic: String::new(), max_clients: None, password: PasswordChange::Keep };
            let room = self.channels.create(Some(channel), &room, true, ChannelKind::Voice, id)?;
            self.channels_changed();
            room
        } else {
            channel
        };
        self.move_to(id, channel);
        self.tidy_rooms();
        Ok(())
    }

    fn move_to(&mut self, id: ClientId, channel: ChannelId) {
        self.place(id, channel);
        self.sync_about(id);
    }

    fn place(&mut self, id: ClientId, channel: ChannelId) {
        self.set_sharing(id, false);
        let client = self.clients.get_mut(&id).expect("exists");
        client.info.channel = channel;
        client.arrived = Instant::now();
        client.watching = None;
    }

    fn sync_about(&mut self, subject: ClientId) {
        let muted = self.mutes.contains(&self.clients[&subject].info.identity);
        let client = self.clients.get_mut(&subject).expect("exists");
        let now = speech(muted, &client.last_sent_permissions, client.info.channel);
        let was = std::mem::replace(&mut client.info.speech, now);
        let info = client.info.clone();
        let (mut moved, mut shown, mut hidden) = (Vec::new(), Vec::new(), Vec::new());
        for c in self.clients.values_mut() {
            let sees = c.info.id == subject || c.last_sent_permissions.sees(info.channel);
            let was = if sees {
                c.last_sent_locations.insert(subject, info.channel)
            } else {
                c.last_sent_locations.remove(&subject)
            };
            match (was, sees) {
                (Some(at), true) if at != info.channel => moved.push(c.info.id),
                (None, true) => shown.push(c.info.id),
                (Some(_), false) => hidden.push(c.info.id),
                _ => {}
            }
        }
        let channel = info.channel;
        let news = [
            (S2C::ClientMoved { id: subject, channel }, &moved),
            (S2C::ClientShown(info), &shown),
            (S2C::ClientHidden { id: subject }, &hidden),
        ];
        for (msg, to) in &news {
            if !to.is_empty() {
                self.broadcast(msg, |c| to.contains(&c.info.id));
            }
        }
        if now != was {
            let msg = S2C::ClientSpeech { id: subject, speech: now };
            self.broadcast(&msg, |c| c.last_sent_locations.contains_key(&subject) && !shown.contains(&c.info.id));
        }
    }

    fn standing(&self, id: ClientId, scope: Scope) -> Standing {
        self.roles.roles().resolve(self.clients[&id].subject(), scope, &self.channels)
    }

    fn may(&self, id: ClientId, perm: Perm, scope: Scope) -> bool {
        self.standing(id, scope).perms.has(perm)
    }

    fn permissions_of(&self, who: Subject) -> Permissions {
        self.roles.roles().permissions(who, &self.channels)
    }

    fn refresh(&mut self, channel_lists_to: ChannelListsTo, roles_to: RolesTo) {
        let ids: Vec<ClientId> = self.clients.keys().copied().collect();
        let (mut regranted, mut lists, mut editors, mut retitled) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for &id in &ids {
            let client = &self.clients[&id];
            let (was, now) = (&client.last_sent_permissions, self.permissions_of(client.subject()));
            let editor = now.edits_permissions();
            if channel_lists_to == ChannelListsTo::Everyone
                || editor != was.edits_permissions()
                || !was.channels.keys().eq(now.channels.keys())
            {
                lists.push(id);
            }
            if editor && (roles_to == RolesTo::EveryEditor || !was.edits_permissions()) {
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
            client.last_sent_permissions = now;
            client.info.role = role;
        }
        let default = self.channels.default_id();
        let evicted: Vec<ClientId> = self
            .clients
            .values()
            .filter(|c| !c.last_sent_permissions.may_in(c.info.channel, Perm::View))
            .map(|c| c.info.id)
            .collect();
        for &id in &evicted {
            self.place(id, default);
        }

        for id in regranted {
            self.send(id, &S2C::Permissions(self.clients[&id].last_sent_permissions.clone()));
        }
        self.send_channel_lists(&lists);
        self.sync_about_evicted_first(&evicted, &ids);
        if !editors.is_empty() {
            self.broadcast(&S2C::Roles(self.roles.roles().clone()), |c| editors.contains(&c.info.id));
        }
        for id in retitled {
            let role = self.clients[&id].info.role.clone();
            self.broadcast(&S2C::ClientRole { id, role }, |c| c.last_sent_locations.contains_key(&id));
        }
        if !evicted.is_empty() {
            self.tidy_rooms();
        }
    }

    fn sync_about_evicted_first(&mut self, evicted: &[ClientId], everyone: &[ClientId]) {
        for &id in evicted.iter().chain(everyone.iter().filter(|id| !evicted.contains(id))) {
            self.sync_about(id);
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
            let permissions = &self.clients[id].last_sent_permissions;
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
        if !self.may(id, needed, Scope::within(parent)) {
            return self.error(id, refusal);
        }
        if !self.clients.get_mut(&id).expect("exists").text_bucket.take() {
            return self.error(id, "you are creating channels too fast");
        }
        match self.channels.create(parent, settings, temporary, kind, id) {
            Ok(channel) => {
                self.channels_changed();
                if !temporary {
                    self.save_channels();
                }
                if kind == ChannelKind::Voice && self.clients[&id].last_sent_permissions.may_in(channel, Perm::View) {
                    self.move_to(id, channel);
                }
                self.tidy_rooms();
            }
            Err(e) => self.error(id, &e.message()),
        }
    }

    fn edit_channel(&mut self, id: ClientId, channel: ChannelId, settings: &ChannelSettings) {
        let mine = self.standing(id, Scope::Channel(channel));
        if !mine.rename {
            return self.error(id, "you can't edit this channel");
        }
        let Some(info) = self.channels.get(channel) else { return self.error(id, "channel does not exist") };
        let renames_only = settings.max_clients == info.max_clients && settings.password == PasswordChange::Keep;
        if !mine.perms.has(Perm::ManageChannels) && !renames_only {
            return self.error(id, "you can only change this channel's name and topic");
        }
        let permanent = !info.temporary;
        match self.channels.edit(channel, settings) {
            Ok(()) => {
                self.channels_changed();
                if permanent {
                    self.save_channels();
                }
            }
            Err(e) => self.error(id, &e.message()),
        }
    }

    fn delete_channel(&mut self, id: ClientId, channel: ChannelId) {
        if !self.may(id, Perm::ManageChannels, Scope::Channel(channel)) {
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
        self.channels_changed();
        if permanent {
            self.save_channels();
        }
        self.tidy_rooms();
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
        self.refresh(ChannelListsTo::WhoseViewChanged, RolesTo::EveryEditor);
    }

    fn refusal(&self, id: ClientId, target: ClientId, (perm, verb): (Perm, &str), scope: Scope) -> Option<String> {
        let (mine, theirs) = (self.standing(id, scope), self.standing(target, scope));
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
        let scope = if from_server { Scope::Server } else { Scope::Channel(t.info.channel) };
        if let Some(msg) = self.refusal(id, target, (Perm::Kick, "kick"), scope) {
            return self.error(id, &msg);
        }
        if scope == Scope::Channel(self.channels.default_id()) {
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
        self.broadcast(&notice, |c| c.last_sent_locations.contains_key(&target));
        if from_server {
            // Its writer sends the notice before it closes the connection.
            self.remove(target, LeaveReason::Kicked);
        } else {
            let t = &self.clients[&target];
            let landing = self.landing_channel(&t.last_sent_permissions, Some(t.info.channel));
            self.move_to(target, landing);
            self.tidy_rooms();
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
        if let Some(msg) = self.refusal(id, target, (Perm::Ban, "ban"), Scope::Server) {
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
        if !self.may(id, Perm::Ban, Scope::Server) {
            return self.error(id, "you can't see the ban list");
        }
        if self.bans.prune(unix_now()) {
            self.save_bans();
        }
        self.send(id, &S2C::Bans(self.bans.list().to_vec()));
    }

    fn unban(&mut self, id: ClientId, ban: BanId) {
        if !self.may(id, Perm::Ban, Scope::Server) {
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

    fn channels_changed(&mut self) {
        self.refresh(ChannelListsTo::Everyone, RolesTo::NewEditors);
    }

    fn save_channels(&self) {
        if let Some(path) = self.data_dir.as_ref().map(|d| d.join("channels.toml"))
            && let Err(e) = self.channels.save(&path)
        {
            tracing::error!("cannot save channels to {}: {e}", path.display());
        }
    }

    fn tidy_rooms(&mut self) {
        let deleted = self.delete_empty_rooms();
        let handed = self.hand_rooms_to_new_owners();
        if deleted || handed {
            self.channels_changed();
        }
    }

    fn delete_empty_rooms(&mut self) -> bool {
        let occupied: HashSet<ChannelId> = self.clients.values().map(|c| c.info.channel).collect();
        let abandoned = self.channels.abandoned_temporaries(&occupied);
        for &channel in &abandoned {
            let _ = self.channels.delete(channel);
        }
        !abandoned.is_empty()
    }

    fn hand_rooms_to_new_owners(&mut self) -> bool {
        let mut handed = false;
        for room in self.channels.temporaries() {
            let owner = self.channels.get(room).and_then(|c| c.owner);
            let inside = |c: &&Client| c.info.channel == room;
            if owner.is_some_and(|o| self.clients.get(&o).is_some_and(|c| inside(&c))) {
                continue;
            }
            let next = self.clients.values().filter(inside).min_by_key(|c| (c.arrived, c.info.id)).map(|c| c.info.id);
            if next != owner {
                tracing::info!(room, ?owner, ?next, "room handed over");
                self.channels.set_owner(room, next);
                handed = true;
            }
        }
        handed
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
        let scope = if target == TextTarget::Channel { Scope::Channel(channel) } else { Scope::Server };
        if !self.may(id, Perm::SendMessages, scope) {
            return self.error(id, "you can't send messages here");
        }
        if !self.clients.get_mut(&id).expect("exists").text_bucket.take() {
            return self.error(id, "you are sending messages too fast");
        }
        let msg = S2C::Text { from: id, target, channel, text: text.to_owned() };
        match target {
            TextTarget::Server => self.broadcast(&msg, |c| c.last_sent_locations.contains_key(&id)),
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
        self.clients.get(&id).is_some_and(|c| c.last_sent_locations.contains_key(&other))
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
            self.broadcast(&S2C::ClientLeft { id, reason }, |c| c.last_sent_locations.contains_key(&id));
            for c in self.clients.values_mut() {
                c.last_sent_locations.remove(&id);
                if c.watching == Some(id) {
                    c.watching = None;
                }
            }
            self.tidy_rooms();
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

fn speech(server_muted: bool, permissions: &Permissions, channel: ChannelId) -> Speech {
    if server_muted {
        Speech::ServerMuted
    } else if permissions.may_in(channel, Perm::Speak) {
        Speech::Allowed
    } else {
        Speech::NotHere
    }
}

fn forward<'a>(udp: &UdpSocket, buf: &mut Vec<u8>, msg: ToClient<'_>, to: impl Iterator<Item = &'a mut Client>) {
    for client in to {
        let Some(addr) = client.voice_addr else { continue };
        client.voice.seal(msg, buf);
        let _ = udp.try_send_to(buf, addr);
    }
}

fn deliver(dead: &mut Vec<ClientId>, client: &Client, frame: Frame) {
    if client.outbox.try_send(frame).is_err() && !dead.contains(&client.info.id) {
        dead.push(client.info.id);
    }
}

fn channels_named(msg: &C2S) -> impl Iterator<Item = ChannelId> {
    let named = match msg {
        C2S::JoinChannel { channel, .. }
        | C2S::EditChannel { id: channel, .. }
        | C2S::DeleteChannel { id: channel }
        | C2S::SetDefaultChannel { id: channel }
        | C2S::SetModerator { channel, .. }
        | C2S::SetOverride { channel, .. }
        | C2S::MoveMember { channel, .. } => [Some(*channel), None],
        C2S::CreateChannel { parent, .. } => [*parent, None],
        C2S::MoveChannel { id: channel, parent, .. } => [Some(*channel), *parent],
        _ => [None, None],
    };
    named.into_iter().flatten()
}

fn highest_rank_touched(roles: &Roles, change: &RoleChange) -> Rank {
    match change {
        RoleChange::Create { .. } => Rank::EVERYONE,
        RoleChange::Rename { id, .. } | RoleChange::SetPermissions { id, .. } | RoleChange::Delete { id } => {
            roles.height(*id)
        }
        RoleChange::Move { id, position } => roles.height(*id).max(roles.height_at(*position as usize)),
        RoleChange::Assign { identity, role, .. } => roles.height(*role).max(roles.rank(*identity)),
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
