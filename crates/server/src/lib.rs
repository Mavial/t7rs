//! T7 server library: config loading and the async server itself.

pub mod config;
mod conn;
mod state;

use t7_proto::ServerInfo;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::state::Server;

/// Accepts connections on `listener` forever.
pub async fn serve(listener: TcpListener, cfg: &Config) {
    let (channels, default_channel) = cfg.flatten_channels();
    let info = ServerInfo {
        name: cfg.name.clone(),
        welcome: cfg.welcome.clone(),
        max_clients: cfg.max_clients,
    };
    let server = Server::new(info, cfg.password().map(str::to_owned), channels, default_channel);

    let (core_tx, core_rx) = mpsc::channel(4096);
    tokio::spawn(server.run(core_rx));

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                tokio::spawn(conn::handle(stream, addr, core_tx.clone()));
            }
            Err(e) => tracing::warn!("accept failed: {e}"),
        }
    }
}
