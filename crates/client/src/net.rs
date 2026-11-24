//! Networking runs on its own thread with a small single-threaded tokio
//! runtime, so the UI thread never blocks on I/O. The UI talks to it through
//! channels and gets woken up (`request_repaint`) only when something arrives.
//!
//! Each session has a TLS control connection and, once welcomed, a UDP voice
//! link to the same server address.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use t7_proto::identity::IdentityKey;
use t7_proto::tls::{self, ConnectError, Fingerprint};
use t7_proto::voice::{self, ToClient, ToServer};
use t7_proto::{
    C2S, ClientId, DEFAULT_PORT, FrameError, PING_INTERVAL_SECS, PROTOCOL_VERSION, S2C, read_frame, write_frame,
};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;

use crate::audio::{Playback, VoiceSink};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// Also keeps NAT mappings alive, so it must stay well under typical UDP timeouts.
const VOICE_PING_INTERVAL: Duration = Duration::from_secs(1);

enum Command {
    Connect(Attempt),
    Send(C2S),
    Voice { seq: u32, payload: Vec<u8> },
    Disconnect,
}

/// What a session connects to and with.
struct Attempt {
    address: String,
    nickname: String,
    password: Option<String>,
    /// The certificate fingerprint seen here before, if any.
    pinned: Option<Fingerprint>,
    key: IdentityKey,
}

/// What a session sends.
enum Out {
    Control(C2S),
    Voice { seq: u32, payload: Vec<u8> },
}

/// Each connection attempt gets a fresh session id; the UI ignores events
/// from stale sessions.
pub type SessionId = u64;

pub enum NetEvent {
    Message(SessionId, S2C),
    Ping(SessionId, Duration),
    /// Round trip of a voice datagram: proof that UDP gets through.
    VoicePing(SessionId, Duration),
    /// TLS is up, and this is the server's certificate.
    Secured(SessionId, Fingerprint),
    Closed(SessionId, Ended),
}

/// Why a session ended.
pub enum Ended {
    /// The user disconnected on purpose.
    ByUser,
    Failed(String),
    /// The server's certificate is not the one pinned for its address.
    CertChanged {
        pinned: Fingerprint,
        presented: Fingerprint,
    },
}

pub struct Net {
    tx: mpsc::UnboundedSender<Command>,
    pub rx: std_mpsc::Receiver<NetEvent>,
    session: SessionId,
}

impl Net {
    pub fn spawn(ctx: egui::Context, playback: Arc<Playback>) -> Self {
        let (tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, rx) = std_mpsc::channel();
        std::thread::Builder::new()
            .name("t7-net".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to start network runtime");
                rt.block_on(command_loop(cmd_rx, Events { tx: ev_tx, ctx }, playback));
            })
            .expect("failed to spawn network thread");
        Self { tx, rx, session: 0 }
    }

    /// Starts a new connection and returns its session id. `pinned` is the
    /// certificate fingerprint to insist on; with `None` the server's is trusted.
    pub fn connect(
        &mut self,
        address: String,
        nickname: String,
        password: Option<String>,
        pinned: Option<Fingerprint>,
        key: IdentityKey,
    ) -> SessionId {
        self.session += 1;
        let _ = self.tx.send(Command::Connect(Attempt { address, nickname, password, pinned, key }));
        self.session
    }

    pub fn send(&self, msg: C2S) {
        let _ = self.tx.send(Command::Send(msg));
    }

    pub fn disconnect(&self) {
        let _ = self.tx.send(Command::Disconnect);
    }

    /// Where the capture thread sends encoded frames.
    pub fn voice_sink(&self) -> VoiceSink {
        let tx = self.tx.clone();
        Arc::new(move |seq, payload: &[u8]| {
            let _ = tx.send(Command::Voice { seq, payload: payload.to_vec() });
        })
    }
}

#[derive(Clone)]
struct Events {
    tx: std_mpsc::Sender<NetEvent>,
    ctx: egui::Context,
}

impl Events {
    fn emit(&self, ev: NetEvent) {
        if self.tx.send(ev).is_ok() {
            self.ctx.request_repaint();
        }
    }
}

async fn command_loop(mut cmds: mpsc::UnboundedReceiver<Command>, events: Events, playback: Arc<Playback>) {
    let mut session: SessionId = 0;
    let mut current: Option<(mpsc::UnboundedSender<Out>, tokio::task::JoinHandle<()>)> = None;

    while let Some(cmd) = cmds.recv().await {
        match cmd {
            Command::Connect(attempt) => {
                if let Some((tx, task)) = current.take() {
                    let _ = tx.send(Out::Control(C2S::Goodbye));
                    // Give the goodbye a moment to flush, then stop the old session.
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        task.abort();
                    });
                }
                session += 1;
                let (tx, rx) = mpsc::unbounded_channel();
                let task = tokio::spawn(run_session(session, attempt, rx, events.clone(), playback.clone()));
                current = Some((tx, task));
            }
            Command::Send(msg) => {
                if let Some((tx, _)) = &current {
                    let _ = tx.send(Out::Control(msg));
                }
            }
            Command::Voice { seq, payload } => {
                if let Some((tx, _)) = &current {
                    let _ = tx.send(Out::Voice { seq, payload });
                }
            }
            Command::Disconnect => {
                if let Some((tx, _)) = current.take() {
                    let _ = tx.send(Out::Control(C2S::Goodbye));
                }
            }
        }
    }
}

async fn run_session(
    id: SessionId,
    attempt: Attempt,
    outgoing: mpsc::UnboundedReceiver<Out>,
    events: Events,
    playback: Arc<Playback>,
) {
    let ended = session(id, attempt, outgoing, &events, &playback).await;
    playback.clear();
    events.emit(NetEvent::Closed(id, ended));
}

/// The key under which a server's certificate is pinned: one per host and port.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServerKey(String);

impl ServerKey {
    pub fn of(address: &str) -> ServerKey {
        ServerKey(with_default_port(address).to_ascii_lowercase())
    }
}

/// Adds the default port when the user typed just a host name.
pub fn with_default_port(address: &str) -> String {
    let a = address.trim();
    let has_port = match a.rsplit_once(':') {
        // IPv6 literals contain colons; require brackets for a port there.
        Some((host, port)) => port.parse::<u16>().is_ok() && (!host.contains(':') || host.ends_with(']')),
        None => false,
    };
    if has_port {
        a.to_owned()
    } else if a.contains(':') && !a.starts_with('[') {
        format!("[{a}]:{DEFAULT_PORT}")
    } else {
        format!("{a}:{DEFAULT_PORT}")
    }
}

/// The UDP side of a session, available once the server has welcomed us.
struct VoiceLink {
    sock: Arc<UdpSocket>,
    crypto: voice::ClientSide,
    buf: Vec<u8>,
}

impl VoiceLink {
    async fn open(server: SocketAddr, id: ClientId, key: &voice::VoiceKey) -> std::io::Result<Self> {
        let local: SocketAddr = if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse().expect("valid");
        let sock = UdpSocket::bind(local).await?;
        sock.connect(server).await?;
        let crypto = voice::ClientSide::new(id, key);
        Ok(Self { sock: Arc::new(sock), crypto, buf: Vec::with_capacity(voice::MAX_DATAGRAM) })
    }

    async fn send(&mut self, msg: ToServer<'_>) {
        self.crypto.seal(msg, &mut self.buf);
        // Voice is best effort: a failed send is just a lost frame.
        let _ = self.sock.send(&self.buf).await;
    }
}

/// Runs one connection to completion and says why it ended.
async fn session(
    id: SessionId,
    attempt: Attempt,
    mut outgoing: mpsc::UnboundedReceiver<Out>,
    events: &Events,
    playback: &Playback,
) -> Ended {
    let target = with_default_port(&attempt.address);
    let stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&target)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Ended::Failed(format!("Could not connect to {target}: {e}")),
        Err(_) => return Ended::Failed(format!("Connection to {target} timed out")),
    };
    let _ = stream.set_nodelay(true);
    let server = stream.peer_addr().ok();
    let (stream, fingerprint) = match tokio::time::timeout(CONNECT_TIMEOUT, tls::connect(stream, attempt.pinned)).await
    {
        Ok(Ok(secured)) => secured,
        Ok(Err(ConnectError::CertChanged { pinned, presented })) => return Ended::CertChanged { pinned, presented },
        Ok(Err(e)) => {
            return Ended::Failed(format!("Could not connect securely to {target} ({e}). Is it an older T7 server?"));
        }
        // A server from before TLS waits for the rest of a frame instead of answering.
        Err(_) => {
            return Ended::Failed(format!("{target} did not answer the secure handshake. Is it an older T7 server?"));
        }
    };
    events.emit(NetEvent::Secured(id, fingerprint));
    let identity = attempt.key.prove(&tls::exporter(stream.get_ref().1));
    let (read, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);

    let hello =
        C2S::Hello { protocol: PROTOCOL_VERSION, nickname: attempt.nickname, password: attempt.password, identity };
    if let Err(e) = write_frame(&mut writer, &hello).await {
        return Ended::Failed(format!("Connection lost: {e}"));
    }

    // Reading happens in its own task: `read_frame` must not be cancelled mid-frame.
    let (in_tx, mut incoming) = mpsc::unbounded_channel::<Result<S2C, FrameError>>();
    let read_task = tokio::spawn(async move {
        let mut scratch = Vec::with_capacity(1024);
        loop {
            let res = read_frame::<_, S2C>(&mut reader, &mut scratch).await;
            let stop = res.is_err();
            if in_tx.send(res).is_err() || stop {
                break;
            }
        }
    });
    let _guard = AbortOnDrop(read_task);

    let epoch = Instant::now();
    let mut ping = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut voice_ping = tokio::time::interval(VOICE_PING_INTERVAL);
    voice_ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut voice: Option<VoiceLink> = None;

    loop {
        let sock = voice.as_ref().map(|v| v.sock.clone());
        let udp_recv = async move {
            match sock {
                Some(sock) => {
                    let mut buf = [0u8; 1500];
                    let n = sock.recv(&mut buf).await;
                    (n, buf)
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            msg = incoming.recv() => match msg {
                Some(Ok(S2C::Pong { nonce })) => {
                    let sent = Duration::from_micros(nonce);
                    events.emit(NetEvent::Ping(id, epoch.elapsed().saturating_sub(sent)));
                }
                Some(Ok(S2C::Rejected { reason })) => return Ended::Failed(reason),
                Some(Ok(msg)) => {
                    if let (S2C::Welcome { your_id, voice_key, .. }, Some(server)) = (&msg, server) {
                        match VoiceLink::open(server, *your_id, voice_key).await {
                            Ok(link) => voice = Some(link),
                            Err(e) => tracing::warn!("voice socket: {e}"),
                        }
                        voice_ping.reset_immediately();
                    }
                    events.emit(NetEvent::Message(id, msg));
                }
                Some(Err(FrameError::Io(_))) | None => return Ended::Failed("Connection to the server was lost".into()),
                Some(Err(e)) => return Ended::Failed(format!("Protocol error: {e}")),
            },
            (res, mut buf) = udp_recv => {
                // Errors here are usually ICMP "unreachable" bounces on Windows; keep listening.
                if let (Ok(n), Some(v)) = (res, &mut voice) {
                    match v.crypto.open(&mut buf[..n]) {
                        Some(ToClient::Audio { from, seq, payload }) => playback.push(from, seq, payload),
                        Some(ToClient::Pong { nonce }) => {
                            let sent = Duration::from_micros(nonce);
                            events.emit(NetEvent::VoicePing(id, epoch.elapsed().saturating_sub(sent)));
                        }
                        Some(ToClient::Video { .. }) | None => {}
                    }
                }
            }
            out = outgoing.recv() => match out {
                Some(Out::Voice { seq, payload }) => {
                    if let Some(v) = &mut voice {
                        v.send(ToServer::Audio { seq, payload: &payload }).await;
                    }
                }
                Some(Out::Control(msg)) => {
                    let bye = matches!(msg, C2S::Goodbye);
                    if let Err(e) = write_frame(&mut writer, &msg).await {
                        return Ended::Failed(format!("Connection lost: {e}"));
                    }
                    if bye {
                        let _ = writer.shutdown().await;
                        return Ended::ByUser;
                    }
                }
                None => {
                    let _ = write_frame(&mut writer, &C2S::Goodbye).await;
                    return Ended::ByUser;
                }
            },
            _ = ping.tick() => {
                let nonce = epoch.elapsed().as_micros() as u64;
                if let Err(e) = write_frame(&mut writer, &C2S::Ping { nonce }).await {
                    return Ended::Failed(format!("Connection lost: {e}"));
                }
            }
            _ = voice_ping.tick() => {
                if let Some(v) = &mut voice {
                    v.send(ToServer::Ping { nonce: epoch.elapsed().as_micros() as u64 }).await;
                }
            }
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::{ServerKey, with_default_port};

    #[test]
    fn default_port() {
        assert_eq!(with_default_port("example.com"), "example.com:9988");
        assert_eq!(with_default_port(" example.com:1234 "), "example.com:1234");
        assert_eq!(with_default_port("::1"), "[::1]:9988");
        assert_eq!(with_default_port("[::1]:5"), "[::1]:5");
    }

    #[test]
    fn one_pin_per_server() {
        assert_eq!(ServerKey::of(" Example.COM "), ServerKey::of("example.com:9988"));
        assert_ne!(ServerKey::of("example.com:9989"), ServerKey::of("example.com:9988"));
    }
}
