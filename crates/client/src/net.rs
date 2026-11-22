//! Networking runs on its own thread with a small single-threaded tokio
//! runtime, so the UI thread never blocks on I/O. The UI talks to it through
//! channels and gets woken up (`request_repaint`) only when something arrives.

use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use eframe::egui;
use t7_proto::{
    C2S, DEFAULT_PORT, FrameError, PING_INTERVAL_SECS, PROTOCOL_VERSION, S2C, read_frame,
    write_frame,
};
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);

enum Command {
    Connect { address: String, nickname: String, password: Option<String> },
    Send(C2S),
    Disconnect,
}

/// Each connection attempt gets a fresh session id; the UI ignores events
/// from stale sessions.
pub type SessionId = u64;

pub enum NetEvent {
    Message(SessionId, S2C),
    Ping(SessionId, Duration),
    /// The session ended. `None` means the user disconnected on purpose.
    Closed(SessionId, Option<String>),
}

pub struct Net {
    tx: mpsc::UnboundedSender<Command>,
    pub rx: std_mpsc::Receiver<NetEvent>,
    session: SessionId,
}

impl Net {
    pub fn spawn(ctx: egui::Context) -> Self {
        let (tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, rx) = std_mpsc::channel();
        std::thread::Builder::new()
            .name("t7-net".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to start network runtime");
                rt.block_on(command_loop(cmd_rx, Events { tx: ev_tx, ctx }));
            })
            .expect("failed to spawn network thread");
        Self { tx, rx, session: 0 }
    }

    /// Starts a new connection and returns its session id.
    pub fn connect(&mut self, address: String, nickname: String, password: Option<String>) -> SessionId {
        self.session += 1;
        let _ = self.tx.send(Command::Connect { address, nickname, password });
        self.session
    }

    pub fn send(&self, msg: C2S) {
        let _ = self.tx.send(Command::Send(msg));
    }

    pub fn disconnect(&self) {
        let _ = self.tx.send(Command::Disconnect);
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

async fn command_loop(mut cmds: mpsc::UnboundedReceiver<Command>, events: Events) {
    let mut session: SessionId = 0;
    let mut current: Option<(mpsc::UnboundedSender<C2S>, tokio::task::JoinHandle<()>)> = None;

    while let Some(cmd) = cmds.recv().await {
        match cmd {
            Command::Connect { address, nickname, password } => {
                if let Some((tx, task)) = current.take() {
                    let _ = tx.send(C2S::Goodbye);
                    // Give the goodbye a moment to flush, then stop the old session.
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        task.abort();
                    });
                }
                session += 1;
                let (tx, rx) = mpsc::unbounded_channel();
                let task = tokio::spawn(run_session(session, address, nickname, password, rx, events.clone()));
                current = Some((tx, task));
            }
            Command::Send(msg) => {
                if let Some((tx, _)) = &current {
                    let _ = tx.send(msg);
                }
            }
            Command::Disconnect => {
                if let Some((tx, _)) = current.take() {
                    let _ = tx.send(C2S::Goodbye);
                }
            }
        }
    }
}

async fn run_session(
    id: SessionId,
    address: String,
    nickname: String,
    password: Option<String>,
    outgoing: mpsc::UnboundedReceiver<C2S>,
    events: Events,
) {
    let reason = session(id, &address, nickname, password, outgoing, &events).await;
    events.emit(NetEvent::Closed(id, reason));
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

/// Runs one connection to completion. Returns the reason it ended
/// (`None` for a clean, user-requested disconnect).
async fn session(
    id: SessionId,
    address: &str,
    nickname: String,
    password: Option<String>,
    mut outgoing: mpsc::UnboundedReceiver<C2S>,
    events: &Events,
) -> Option<String> {
    let target = with_default_port(address);
    let stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&target)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Some(format!("Could not connect to {target}: {e}")),
        Err(_) => return Some(format!("Connection to {target} timed out")),
    };
    let _ = stream.set_nodelay(true);
    let (read, write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut writer = BufWriter::new(write);

    let hello = C2S::Hello { protocol: PROTOCOL_VERSION, nickname, password };
    if let Err(e) = send(&mut writer, &hello).await {
        return Some(format!("Connection lost: {e}"));
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

    loop {
        tokio::select! {
            msg = incoming.recv() => match msg {
                Some(Ok(S2C::Pong { nonce })) => {
                    let sent = Duration::from_micros(nonce);
                    events.emit(NetEvent::Ping(id, epoch.elapsed().saturating_sub(sent)));
                }
                Some(Ok(S2C::Rejected { reason })) => return Some(reason),
                Some(Ok(msg)) => events.emit(NetEvent::Message(id, msg)),
                Some(Err(FrameError::Io(_))) | None => return Some("Connection to the server was lost".into()),
                Some(Err(e)) => return Some(format!("Protocol error: {e}")),
            },
            out = outgoing.recv() => {
                let msg = out.unwrap_or(C2S::Goodbye);
                let bye = matches!(msg, C2S::Goodbye);
                if let Err(e) = send(&mut writer, &msg).await {
                    return Some(format!("Connection lost: {e}"));
                }
                if bye {
                    let _ = writer.shutdown().await;
                    return None;
                }
            }
            _ = ping.tick() => {
                let nonce = epoch.elapsed().as_micros() as u64;
                if let Err(e) = send(&mut writer, &C2S::Ping { nonce }).await {
                    return Some(format!("Connection lost: {e}"));
                }
            }
        }
    }
}

async fn send<W: tokio::io::AsyncWrite + Unpin>(w: &mut BufWriter<W>, msg: &C2S) -> Result<(), FrameError> {
    write_frame(w, msg).await?;
    w.flush().await?;
    Ok(())
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::with_default_port;

    #[test]
    fn default_port() {
        assert_eq!(with_default_port("example.com"), "example.com:9987");
        assert_eq!(with_default_port(" example.com:1234 "), "example.com:1234");
        assert_eq!(with_default_port("::1"), "[::1]:9987");
        assert_eq!(with_default_port("[::1]:5"), "[::1]:5");
    }
}
