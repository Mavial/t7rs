//! T7 server library: config loading and the async server itself.

mod bans;
mod channels;
pub mod config;
mod conn;
mod roles;
mod state;

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use t7_proto::{ServerInfo, tls};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;

use crate::bans::Bans;
use crate::channels::Channels;
use crate::config::Config;
use crate::roles::RoleStore;
use crate::state::{Event, Server};

/// Accepts connections on `listener`, and voice on UDP at the same address, forever.
///
/// `data_dir` holds `channels.toml` (the live channel tree), `bans.toml`,
/// `roles.toml` and the TLS certificate. Without it, they last only this run.
pub async fn serve(listener: TcpListener, cfg: &Config, data_dir: Option<&Path>) -> std::io::Result<()> {
    let udp = Arc::new(UdpSocket::bind(listener.local_addr()?).await?);
    let channels_file = data_dir.map(|d| d.join("channels.toml"));
    let channels = match &channels_file {
        Some(file) if file.exists() => {
            Channels::load(file).map_err(|e| std::io::Error::other(format!("cannot read {}: {e}", file.display())))?
        }
        _ => {
            let channels = Channels::from_config(&cfg.channels);
            if let Some(file) = &channels_file {
                channels.save(file)?;
                tracing::info!("wrote the starting channels to {}", file.display());
            }
            channels
        }
    };
    let bans = match data_dir.map(|d| d.join("bans.toml")) {
        Some(file) if file.exists() => Bans::load(&file, state::unix_now())
            .map_err(|e| std::io::Error::other(format!("cannot read {}: {e}", file.display())))?,
        _ => Bans::default(),
    };
    let roles = roles(cfg, data_dir)?;
    let (cert, key) = tls_certificate(data_dir)?;
    tracing::info!("TLS certificate fingerprint (SHA-256): {}", tls::Fingerprint::of(&cert));
    let acceptor = tls::acceptor(cert, key).map_err(|e| {
        std::io::Error::other(format!(
            "unusable TLS certificate ({e}); delete tls_cert.der and tls_key.der to make a new one"
        ))
    })?;

    let info = ServerInfo { name: cfg.name.clone(), welcome: cfg.welcome.clone(), max_clients: cfg.max_clients };
    let server = Server::new(
        info,
        cfg.password().map(str::to_owned),
        channels,
        bans,
        data_dir.map(Path::to_owned),
        roles,
        udp.clone(),
    );

    let (core_tx, core_rx) = mpsc::channel(4096);
    tokio::spawn(server.run(core_rx));
    tokio::spawn(voice_loop(udp, core_tx.clone()));

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                tokio::spawn(conn::handle(stream, addr, acceptor.clone(), core_tx.clone()));
            }
            Err(e) => tracing::warn!("accept failed: {e}"),
        }
    }
}

fn roles(cfg: &Config, data_dir: Option<&Path>) -> std::io::Result<RoleStore> {
    if cfg.admin_token.is_some() {
        tracing::warn!("the config's admin_token is no longer used and can be deleted; claim ownership instead");
    }
    if let Some(old) = data_dir.map(|d| d.join("admin_token.txt")).filter(|f| f.exists()) {
        std::fs::remove_file(&old)?;
        tracing::info!("deleted {}: claim ownership instead", old.display());
    }
    let file = data_dir.map(|d| d.join("roles.toml"));
    let (mut roles, mut changed) = match &file {
        Some(file) if file.exists() => (
            RoleStore::load(file).map_err(|e| std::io::Error::other(format!("cannot read {}: {e}", file.display())))?,
            false,
        ),
        _ => (RoleStore::default(), true),
    };
    if roles.roles().owner.is_none() && roles.claim_token().is_none() {
        roles.set_claim_token(claim_token()?);
        changed = true;
    }
    if let Some(file) = file.filter(|_| changed) {
        roles.save(&file)?;
    }
    if let Some(token) = roles.claim_token() {
        tracing::info!("claim token: {token}  (in T7: the menu next to CHANNELS, then Claim ownership; it works once)");
    }
    Ok(roles)
}

/// Saves a new claim token in `data_dir`'s `roles.toml` and returns it. Whoever
/// redeems it becomes the owner in place of the current one, who stays until
/// then. A running server neither sees it nor keeps it, so stop it first.
pub fn reset_owner_claim(data_dir: &Path) -> std::io::Result<String> {
    let file = data_dir.join("roles.toml");
    let mut roles = if file.exists() { RoleStore::load(&file)? } else { RoleStore::default() };
    let token = claim_token()?;
    roles.set_claim_token(token.clone());
    roles.save(&file)?;
    Ok(token)
}

fn claim_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(format!("no system randomness: {e}")))?;
    Ok(t7_proto::update::to_hex(&bytes))
}

/// The certificate and key (DER) saved in `data_dir`, else a new self-signed
/// pair, saved there if there is one.
fn tls_certificate(data_dir: Option<&Path>) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let files = data_dir.map(|d| (d.join("tls_cert.der"), d.join("tls_key.der")));
    if let Some((cert, key)) = &files
        && let (Ok(cert), Ok(key)) = (std::fs::read(cert), std::fs::read(key))
    {
        return Ok((cert, key));
    }
    let pair = rcgen::generate_simple_self_signed(["t7".to_owned()]).map_err(std::io::Error::other)?;
    let (cert, key) = (pair.cert.der().to_vec(), pair.signing_key.serialize_der());
    if let Some((cert_file, key_file)) = &files {
        write_secret(key_file, &key)?;
        std::fs::write(cert_file, &cert)?;
        tracing::info!("wrote a new TLS certificate to {}", cert_file.display());
    }
    Ok((cert, key))
}

/// Writes a file only its owner may read, where the OS has such permissions.
fn write_secret(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(data)
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
