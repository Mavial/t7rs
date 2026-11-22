//! T7 server library: config loading and the async server itself.

mod channels;
pub mod config;
mod conn;
mod state;

use std::path::Path;
use std::sync::Arc;

use t7_proto::ServerInfo;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;

use crate::channels::Channels;
use crate::config::Config;
use crate::state::{Event, Server};

/// Accepts connections on `listener`, and voice on UDP at the same address, forever.
///
/// `data_dir` holds `channels.toml` (the live channel tree) and the generated
/// admin token. Without it, channel edits and the token last only this run.
pub async fn serve(listener: TcpListener, cfg: &Config, data_dir: Option<&Path>) -> std::io::Result<()> {
    let udp = Arc::new(UdpSocket::bind(listener.local_addr()?).await?);
    let channels_file = data_dir.map(|d| d.join("channels.toml"));
    let channels = match &channels_file {
        Some(file) if file.exists() => Channels::load(file)
            .map_err(|e| std::io::Error::other(format!("cannot read {}: {e}", file.display())))?,
        _ => {
            let channels = Channels::from_config(&cfg.channels);
            if let Some(file) = &channels_file {
                channels.save(file)?;
                tracing::info!("wrote the starting channels to {}", file.display());
            }
            channels
        }
    };
    let admin_token = admin_token(cfg, data_dir)?;
    tracing::info!("admin token: {admin_token}  (in T7: the menu next to CHANNELS, then Admin login)");

    let info = ServerInfo {
        name: cfg.name.clone(),
        welcome: cfg.welcome.clone(),
        max_clients: cfg.max_clients,
    };
    let server = Server::new(
        info,
        cfg.password().map(str::to_owned),
        channels,
        channels_file,
        admin_token,
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

/// The configured token, else the one saved in `admin_token.txt`, else a new one.
fn admin_token(cfg: &Config, data_dir: Option<&Path>) -> std::io::Result<String> {
    if !cfg.admin_token.trim().is_empty() {
        return Ok(cfg.admin_token.trim().to_owned());
    }
    let file = data_dir.map(|d| d.join("admin_token.txt"));
    if let Some(saved) = file.as_ref().and_then(|f| std::fs::read_to_string(f).ok()) {
        let saved = saved.trim();
        if !saved.is_empty() {
            return Ok(saved.to_owned());
        }
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(format!("no system randomness: {e}")))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(file) = &file {
        std::fs::write(file, &token)?;
    }
    Ok(token)
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
