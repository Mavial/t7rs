//! The server core: a single task that owns all state.
//!
//! Connection tasks send [`Event`]s in; the core mutates state and pushes
//! pre-encoded frames out to each connection's writer queue. One owner means
//! no locks and a total order of events, and every broadcast is serialised
//! exactly once no matter how many clients receive it.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use t7_proto::voice::{self, ToClient, ToServer};
use t7_proto::{
    C2S, ChannelId, ChannelSettings, ClientId, ClientInfo, ClientStatus, Identity, LeaveReason, MAX_POKE_LEN,
    MAX_TEXT_LEN, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, S2C, ServerInfo, TextTarget, encode_frame,
    validate_nickname,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

use crate::channels::Channels;

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
    Message { id: ClientId, msg: C2S },
    Left { id: ClientId, reason: LeaveReason },
    /// A raw voice datagram, not yet authenticated.
    Voice { from: SocketAddr, data: Vec<u8> },
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
    /// Where permanent channel changes are saved (`None`: not saved).
    channels_file: Option<PathBuf>,
    admin_token: String,
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
        channels_file: Option<PathBuf>,
        admin_token: String,
        udp: Arc<UdpSocket>,
    ) -> Self {
        Self {
            info,
            password,
            channels,
            channels_file,
            admin_token,
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
                let listeners = self.clients.values_mut().filter(|c| {
                    c.info.id != id && c.info.channel == channel && !c.info.status.deafened
                });
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
        let channel = self.default_channel_for_join();
        let info = ClientInfo { id, nickname, channel, status: ClientStatus::default(), admin: false, identity };

        let mut clients: Vec<ClientInfo> = self.clients.values().map(|c| c.info.clone()).collect();
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
            channels: self.channels.snapshot(),
            clients,
        };
        if outbox.try_send(encode(&welcome)).is_err() {
            return None;
        }

        tracing::info!(%addr, id, nickname = %info.nickname, "client joined");
        self.broadcast(&S2C::ClientJoined(info.clone()), |_| true);
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
            },
        );
        Some(id)
    }

    /// If the name is taken, append a number instead of refusing.
    fn unique_nickname(&self, base: String) -> String {
        let taken = |n: &str| self.clients.values().any(|c| c.info.nickname.eq_ignore_ascii_case(n));
        if !taken(&base) {
            return base;
        }
        (1..)
            .map(|i| format!("{base}{i}"))
            .find(|n| !taken(n))
            .expect("infinite iterator")
    }

    /// The default channel, or the first open channel with room if it's full.
    fn default_channel_for_join(&self) -> ChannelId {
        let default = self.channels.default_id();
        std::iter::once(default)
            .chain(self.channels.ids_in_order())
            .find(|&ch| self.has_room(ch) && self.channels.password_ok(ch, None))
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
        if !self.clients.contains_key(&id) {
            return;
        }
        match msg {
            C2S::Hello { .. } => self.error(id, "already connected"),
            C2S::JoinChannel { channel, password } => self.join_channel(id, channel, password.as_deref()),
            C2S::SendText { target, text } => self.text(id, target, text),
            C2S::SetStatus(status) => {
                let client = self.clients.get_mut(&id).expect("checked above");
                if client.info.status != status {
                    client.info.status = status;
                    self.broadcast(&S2C::ClientStatus { id, status }, |_| true);
                }
            }
            C2S::Ping { nonce } => self.send(id, &S2C::Pong { nonce }),
            C2S::Goodbye => self.remove(id, LeaveReason::Disconnected),
            C2S::Poke { target, message } => self.poke(id, target, message),
            C2S::CreateChannel { parent, settings, temporary } => self.create_channel(id, parent, settings, temporary),
            C2S::EditChannel { id: channel, settings } => self.edit_channel(id, channel, &settings),
            C2S::MoveChannel { id: channel, parent, position } => {
                if !self.is_admin(id) {
                    return self.error(id, "only admins can rearrange channels");
                }
                match self.channels.move_to(channel, parent, position) {
                    Ok(()) => self.channels_changed(true),
                    Err(e) => self.error(id, &e.message()),
                }
            }
            C2S::DeleteChannel { id: channel } => self.delete_channel(id, channel),
            C2S::SetDefaultChannel { id: channel } => {
                if !self.is_admin(id) {
                    return self.error(id, "only admins can change the default channel");
                }
                match self.channels.set_default(channel) {
                    Ok(()) => self.channels_changed(true),
                    Err(e) => self.error(id, &e.message()),
                }
            }
            C2S::Authenticate { token } => self.authenticate(id, &token),
        }
    }

    fn join_channel(&mut self, id: ClientId, channel: ChannelId, password: Option<&str>) {
        if self.channels.get(channel).is_none() {
            return self.error(id, "channel does not exist");
        }
        if self.clients[&id].info.channel == channel {
            return;
        }
        if !self.is_admin(id) && !self.channels.password_ok(channel, password) {
            let msg = if password.is_some() { "wrong channel password" } else { "this channel needs a password" };
            return self.error(id, msg);
        }
        if !self.has_room(channel) {
            return self.error(id, "channel is full");
        }
        self.move_to(id, channel);
        self.remove_abandoned_channels();
    }

    fn move_to(&mut self, id: ClientId, channel: ChannelId) {
        self.clients.get_mut(&id).expect("exists").info.channel = channel;
        self.broadcast(&S2C::ClientMoved { id, channel }, |_| true);
    }

    fn is_admin(&self, id: ClientId) -> bool {
        self.clients.get(&id).is_some_and(|c| c.info.admin)
    }

    /// Admins manage every channel; anyone manages the temporary channels they made.
    fn may_manage(&self, id: ClientId, channel: ChannelId) -> bool {
        self.is_admin(id) || self.channels.get(channel).is_some_and(|c| c.temporary && c.creator == Some(id))
    }

    fn create_channel(&mut self, id: ClientId, parent: Option<ChannelId>, settings: ChannelSettings, temporary: bool) {
        if !temporary && !self.is_admin(id) {
            return self.error(id, "only admins can create permanent channels");
        }
        if !self.clients.get_mut(&id).expect("exists").text_bucket.take() {
            return self.error(id, "you are creating channels too fast");
        }
        match self.channels.create(parent, &settings, temporary, id) {
            Ok(channel) => {
                self.channels_changed(!temporary);
                self.move_to(id, channel);
                self.remove_abandoned_channels();
            }
            Err(e) => self.error(id, &e.message()),
        }
    }

    fn edit_channel(&mut self, id: ClientId, channel: ChannelId, settings: &ChannelSettings) {
        if !self.may_manage(id, channel) {
            return self.error(id, "you can't edit this channel");
        }
        let permanent = self.channels.get(channel).is_some_and(|c| !c.temporary);
        match self.channels.edit(channel, settings) {
            Ok(()) => self.channels_changed(permanent),
            Err(e) => self.error(id, &e.message()),
        }
    }

    fn delete_channel(&mut self, id: ClientId, channel: ChannelId) {
        if !self.may_manage(id, channel) {
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

    fn authenticate(&mut self, id: ClientId, token: &str) {
        if !self.clients.get_mut(&id).expect("exists").text_bucket.take() {
            return self.error(id, "too many attempts, wait a moment");
        }
        if self.admin_token.is_empty() || token.trim() != self.admin_token {
            return self.error(id, "wrong admin token");
        }
        let client = self.clients.get_mut(&id).expect("exists");
        if !client.info.admin {
            client.info.admin = true;
            tracing::info!(id, nickname = %client.info.nickname, "client is now admin");
            self.broadcast(&S2C::ClientAdmin { id, admin: true }, |_| true);
        }
    }

    /// Sends everyone the new channel list; saves it when permanent channels changed.
    fn channels_changed(&mut self, save: bool) {
        self.broadcast(&S2C::Channels(self.channels.snapshot()), |_| true);
        if save
            && let Some(path) = &self.channels_file
            && let Err(e) = self.channels.save(path)
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
        if target == id || !self.clients.contains_key(&target) {
            return self.error(id, "that user is no longer online");
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
        if !self.clients.get_mut(&id).expect("exists").text_bucket.take() {
            return self.error(id, "you are sending messages too fast");
        }
        let channel = self.clients[&id].info.channel;
        let msg = S2C::Text { from: id, target, channel, text: text.to_owned() };
        match target {
            TextTarget::Server => self.broadcast(&msg, |_| true),
            TextTarget::Channel => self.broadcast(&msg, |c| c.info.channel == channel),
            TextTarget::Client(to) => {
                if to == id || !self.clients.contains_key(&to) {
                    return self.error(id, "that user is no longer online");
                }
                // Recipient and sender (echo, so the sender sees it confirmed).
                self.broadcast(&msg, |c| c.info.id == to || c.info.id == id);
            }
        }
    }

    fn remove(&mut self, id: ClientId, reason: LeaveReason) {
        if let Some(client) = self.clients.remove(&id) {
            tracing::info!(addr = %client.addr, id, nickname = %client.info.nickname, ?reason, "client left");
            // Dropping `client.outbox` ends its writer task, which closes the socket.
            self.broadcast(&S2C::ClientLeft { id, reason }, |_| true);
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

    /// Encodes once and enqueues to every matching client. A client whose
    /// queue is full is too slow to keep up and gets disconnected.
    fn broadcast(&mut self, msg: &S2C, filter: impl Fn(&Client) -> bool) {
        let frame = encode(msg);
        for client in self.clients.values().filter(|c| filter(c)) {
            if client.outbox.try_send(frame.clone()).is_err() && !self.dead.contains(&client.info.id) {
                self.dead.push(client.info.id);
            }
        }
    }
}

fn encode(msg: &S2C) -> Frame {
    encode_frame(msg).into()
}
