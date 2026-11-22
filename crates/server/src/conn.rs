//! Per-connection I/O: one reader loop and one writer task per TCP socket.

use std::net::SocketAddr;
use std::time::Duration;

use t7_proto::{C2S, FrameError, IDLE_TIMEOUT_SECS, LeaveReason, read_frame};
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use crate::state::{Event, Frame, OUTBOX_CAPACITY};

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn handle(stream: TcpStream, addr: SocketAddr, core: mpsc::Sender<Event>) {
    let _ = stream.set_nodelay(true);
    let (read, write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut scratch = Vec::with_capacity(512);

    let (outbox, rx) = mpsc::channel::<Frame>(OUTBOX_CAPACITY);
    let mut writer = tokio::spawn(write_loop(write, rx));

    // Handshake: the first frame must be Hello.
    let hello = timeout(HELLO_TIMEOUT, read_frame::<_, C2S>(&mut reader, &mut scratch)).await;
    let (protocol, nickname, password) = match hello {
        Ok(Ok(C2S::Hello { protocol, nickname, password })) => (protocol, nickname, password),
        Ok(Ok(_)) => return tracing::debug!(%addr, "first message was not Hello"),
        Ok(Err(e)) => return tracing::debug!(%addr, "handshake failed: {e}"),
        Err(_) => return tracing::debug!(%addr, "handshake timed out"),
    };

    let (reply, id) = oneshot::channel();
    let join = Event::Join { addr, protocol, nickname, password, outbox, reply };
    if core.send(join).await.is_err() {
        return;
    }
    let Ok(Some(id)) = id.await else {
        // Rejected: let the writer flush the rejection before we close.
        let _ = timeout(Duration::from_secs(2), writer).await;
        return;
    };

    let idle = Duration::from_secs(IDLE_TIMEOUT_SECS);
    let reason = loop {
        tokio::select! {
            res = timeout(idle, read_frame::<_, C2S>(&mut reader, &mut scratch)) => match res {
                Ok(Ok(msg)) => {
                    let bye = matches!(msg, C2S::Goodbye);
                    if core.send(Event::Message { id, msg }).await.is_err() || bye {
                        return;
                    }
                }
                Ok(Err(FrameError::Io(_))) => break LeaveReason::Disconnected,
                Ok(Err(e)) => {
                    tracing::debug!(%addr, id, "protocol error: {e}");
                    break LeaveReason::Disconnected;
                }
                Err(_) => break LeaveReason::TimedOut,
            },
            // The core dropped our outbox (kicked / too slow): stop reading.
            _ = &mut writer => return,
        }
    };
    let _ = core.send(Event::Left { id, reason }).await;
}

/// Drains the outbox into the socket, batching everything that is already
/// queued into a single flush (one syscall for a burst of messages).
async fn write_loop(write: OwnedWriteHalf, mut rx: mpsc::Receiver<Frame>) {
    let mut w = BufWriter::with_capacity(16 * 1024, write);
    while let Some(frame) = rx.recv().await {
        if w.write_all(&frame).await.is_err() {
            return;
        }
        while let Ok(frame) = rx.try_recv() {
            if w.write_all(&frame).await.is_err() {
                return;
            }
        }
        if w.flush().await.is_err() {
            return;
        }
    }
    let _ = w.shutdown().await;
}
