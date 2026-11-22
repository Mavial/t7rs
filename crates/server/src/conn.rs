//! Per-connection I/O: one reader loop and one writer task per TLS connection.

use std::net::SocketAddr;
use std::time::Duration;

use t7_proto::tls::{self, TlsAcceptor};
use t7_proto::{C2S, FrameError, IDLE_TIMEOUT_SECS, LeaveReason, MIN_PROTOCOL_VERSION, S2C, encode_frame, read_frame};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout, timeout_at};

use crate::state::{Event, Frame, OUTBOX_CAPACITY};

/// For the TLS handshake and the Hello together.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn handle(stream: TcpStream, addr: SocketAddr, acceptor: TlsAcceptor, core: mpsc::Sender<Event>) {
    let _ = stream.set_nodelay(true);
    let deadline = Instant::now() + HELLO_TIMEOUT;
    match timeout_at(deadline, starts_with_tls(&stream)).await {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => return reject_plaintext(stream, addr).await,
        Ok(Err(_)) | Err(_) => return,
    }
    let stream = match timeout_at(deadline, acceptor.accept(stream)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => return tracing::debug!(%addr, "TLS handshake failed: {e}"),
        Err(_) => return tracing::debug!(%addr, "TLS handshake timed out"),
    };
    let exporter = tls::exporter(stream.get_ref().1);
    let (read, write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    let mut scratch = Vec::with_capacity(512);

    let (outbox, rx) = mpsc::channel::<Frame>(OUTBOX_CAPACITY);
    let mut writer = tokio::spawn(write_loop(write, rx));

    // Handshake: the first frame must be Hello.
    let hello = timeout_at(deadline, read_frame::<_, C2S>(&mut reader, &mut scratch)).await;
    let (protocol, nickname, password, proof) = match hello {
        Ok(Ok(C2S::Hello { protocol, nickname, password, identity })) => (protocol, nickname, password, identity),
        Ok(Ok(_)) => return tracing::debug!(%addr, "first message was not Hello"),
        Ok(Err(e)) => return tracing::debug!(%addr, "handshake failed: {e}"),
        Err(_) => return tracing::debug!(%addr, "handshake timed out"),
    };

    let (reply, id) = oneshot::channel();
    let identity = proof.verify(&exporter);
    let join = Event::Join { addr, protocol, nickname, password, identity, outbox, reply };
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

/// Whether the client opened with a TLS record. Clients before protocol 5
/// start with a frame length instead; for a 22-byte Hello its first byte is
/// also 0x16, but the second is 0 where a TLS record has its version, 3.
async fn starts_with_tls(stream: &TcpStream) -> std::io::Result<bool> {
    let mut head = [0u8; 2];
    loop {
        match stream.peek(&mut head).await? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            1 if head[0] == tls::HANDSHAKE_RECORD => {
                // Only half of the record header has arrived; wait for the rest.
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            _ => return Ok(head == [tls::HANDSHAKE_RECORD, 3]),
        }
    }
}

/// Tells a client too old for TLS why it can't connect, in the plain frame it understands.
async fn reject_plaintext(mut stream: TcpStream, addr: SocketAddr) {
    let reason = format!("this server needs T7 protocol {MIN_PROTOCOL_VERSION} or newer; please update your T7");
    tracing::info!(%addr, reason, "rejected connection without TLS");
    let frame = encode_frame(&S2C::Rejected { reason });
    if stream.write_all(&frame).await.is_ok() && stream.shutdown().await.is_ok() {
        // Closing with their Hello still unread would reset the connection,
        // which can discard the rejection before they read it. Read until they hang up.
        let _ = timeout(Duration::from_secs(2), tokio::io::copy(&mut stream, &mut tokio::io::sink())).await;
    }
}

/// Drains the outbox into the socket, batching everything that is already
/// queued into a single flush (one syscall for a burst of messages).
async fn write_loop(write: impl AsyncWrite + Unpin, mut rx: mpsc::Receiver<Frame>) {
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
