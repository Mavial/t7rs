//! The server core: a single task that owns all state.
//!
//! Connection tasks send [`Event`]s in; the core mutates state and pushes
//! pre-encoded frames out to each connection's writer queue. One owner means
//! no locks and a total order of events, and every broadcast is serialised
//! exactly once no matter how many clients receive it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use t7_proto::{
    C2S, ChannelId, ChannelInfo, ClientId, ClientInfo, ClientStatus, LeaveReason,
    MAX_TEXT_LEN, PROTOCOL_VERSION, S2C, ServerInfo, TextTarget, encode_frame, validate_nickname,
};
use tokio::sync::{mpsc, oneshot};

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
        outbox: mpsc::Sender<Frame>,
        /// Receives the assigned id, or `None` if the handshake was rejected.
        reply: oneshot::Sender<Option<ClientId>>,
    },
    Message { id: ClientId, msg: C2S },
    Left { id: ClientId, reason: LeaveReason },
}

struct Client {
    info: ClientInfo,
    addr: SocketAddr,
    outbox: mpsc::Sender<Frame>,
    text_bucket: TokenBucket,
}

/// Simple anti-spam limiter for chat messages.
struct TokenBucket {
    tokens: f32,
    last: Instant,
}

impl TokenBucket {
    const BURST: f32 = 8.0;
    const PER_SEC: f32 = 2.0;

    fn new() -> Self {
        Self { tokens: Self::BURST, last: Instant::now() }
    }

    fn take(&mut self) -> bool {
        let now = Instant::now();
        let refill = now.duration_since(self.last).as_secs_f32() * Self::PER_SEC;
        self.tokens = (self.tokens + refill).min(Self::BURST);
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
    channels: HashMap<ChannelId, ChannelInfo>,
    channel_list: Vec<ChannelInfo>,
    default_channel: ChannelId,
    clients: HashMap<ClientId, Client>,
    next_id: ClientId,
    /// Clients whose outbox overflowed or closed; removed after the current event.
    dead: Vec<ClientId>,
}

impl Server {
    pub fn new(
        info: ServerInfo,
        password: Option<String>,
        channels: Vec<ChannelInfo>,
        default_channel: ChannelId,
    ) -> Self {
        Self {
            info,
            password,
            channels: channels.iter().map(|c| (c.id, c.clone())).collect(),
            channel_list: channels,
            default_channel,
            clients: HashMap::new(),
            next_id: 1,
            dead: Vec::new(),
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
            Event::Join { addr, protocol, nickname, password, outbox, reply } => {
                let id = self.join(addr, protocol, nickname, password, outbox);
                let _ = reply.send(id);
            }
            Event::Message { id, msg } => self.message(id, msg),
            Event::Left { id, reason } => self.remove(id, reason),
        }
    }

    fn join(
        &mut self,
        addr: SocketAddr,
        protocol: u16,
        nickname: String,
        password: Option<String>,
        outbox: mpsc::Sender<Frame>,
    ) -> Option<ClientId> {
        let reject = |reason: &str| {
            tracing::info!(%addr, reason, "rejected connection");
            let _ = outbox.try_send(encode(&S2C::Rejected { reason: reason.into() }));
            None
        };
        if protocol != PROTOCOL_VERSION {
            return reject(&format!(
                "protocol mismatch (server {PROTOCOL_VERSION}, client {protocol}); please update"
            ));
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
        let channel = self.default_channel_for_join();
        let info = ClientInfo { id, nickname, channel, status: ClientStatus::default() };

        let mut clients: Vec<ClientInfo> = self.clients.values().map(|c| c.info.clone()).collect();
        clients.push(info.clone());
        let welcome = S2C::Welcome {
            your_id: id,
            server: self.info.clone(),
            channels: self.channel_list.clone(),
            clients,
        };
        if outbox.try_send(encode(&welcome)).is_err() {
            return None;
        }

        tracing::info!(%addr, id, nickname = %info.nickname, "client joined");
        self.broadcast(&S2C::ClientJoined(info.clone()), |_| true);
        self.clients.insert(id, Client { info, addr, outbox, text_bucket: TokenBucket::new() });
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

    /// The default channel, or the first channel with room if it's full.
    fn default_channel_for_join(&self) -> ChannelId {
        std::iter::once(self.default_channel)
            .chain(self.channel_list.iter().map(|c| c.id))
            .find(|&ch| self.has_room(ch))
            .unwrap_or(self.default_channel)
    }

    fn has_room(&self, channel: ChannelId) -> bool {
        match self.channels.get(&channel).and_then(|c| c.max_clients) {
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
            C2S::JoinChannel { channel } => self.move_client(id, channel),
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
        }
    }

    fn move_client(&mut self, id: ClientId, channel: ChannelId) {
        if !self.channels.contains_key(&channel) {
            return self.error(id, "channel does not exist");
        }
        if self.clients[&id].info.channel == channel {
            return;
        }
        if !self.has_room(channel) {
            return self.error(id, "channel is full");
        }
        self.clients.get_mut(&id).expect("exists").info.channel = channel;
        self.broadcast(&S2C::ClientMoved { id, channel }, |_| true);
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
