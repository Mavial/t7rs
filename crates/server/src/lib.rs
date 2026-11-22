//! T7 server library: config loading and the async server itself.

pub mod config;
mod conn;
mod state;

use std::sync::Arc;

use t7_proto::ServerInfo;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::state::{Event, Server};

/// Accepts connections on `listener`, and voice on UDP at the same address, forever.
pub async fn serve(listener: TcpListener, cfg: &Config) -> std::io::Result<()> {
    let udp = Arc::new(UdpSocket::bind(listener.local_addr()?).await?);
    let (channels, default_channel) = cfg.flatten_channels();
    let info = ServerInfo {
        name: cfg.name.clone(),
        welcome: cfg.welcome.clone(),
        max_clients: cfg.max_clients,
    };
    let server = Server::new(
        info,
        cfg.password().map(str::to_owned),
        channels,
        default_channel,
        udp.clone(),
    );

    let (core_tx, core_rx) = mpsc::channel(4096);
    tokio::spawn(server.run(core_rx));
    tokio::spawn(voice_loop(udp, core_tx.clone()));

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                tokio::spawn(conn::handle(stream, addr, core_tx.clone()));
            }
            Err(e) => tracing::warn!("accept failed: {e}"),
        }
    }
}

/// Hands every datagram to the core. When the core is backed up, voice is
/// dropped rather than queued: late audio is useless.
async fn voice_loop(udp: Arc<UdpSocket>, core: mpsc::Sender<Event>) {
    let mut buf = [0u8; 2048];
    loop {
        match udp.recv_from(&mut buf).await {
            Ok((n, from)) => {
                let ev = Event::Voice { from, data: buf[..n].to_vec() };
                if let Err(mpsc::error::TrySendError::Closed(_)) = core.try_send(ev) {
                    return;
                }
            }
            // On Windows an ICMP "port unreachable" from an earlier send surfaces here.
            Err(e) => tracing::debug!("voice recv: {e}"),
        }
    }
}
