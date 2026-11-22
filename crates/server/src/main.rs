use std::path::PathBuf;

use clap::Parser;
use t7_server::config::Config;
use tokio::net::TcpListener;

/// T7 voice server.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Path to the config file (created with defaults if missing).
    #[arg(short, long, default_value = "t7-server.toml")]
    config: PathBuf,
    /// Override the bind address from the config, e.g. 0.0.0.0:9987.
    #[arg(short, long)]
    bind: Option<String>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        // No color codes when logs go to a file or a container log collector.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let args = Args::parse();
    let mut cfg = Config::load_or_create(&args.config).unwrap_or_else(|e| fail(e));
    if let Some(bind) = args.bind {
        cfg.bind = bind;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start tokio runtime");
    runtime.block_on(async {
        let listener = TcpListener::bind(&cfg.bind)
            .await
            .unwrap_or_else(|e| fail(format!("cannot bind {}: {e}", cfg.bind)));
        tracing::info!(
            "\"{}\" listening on {} (TCP control + UDP voice, {} channels{})",
            cfg.name,
            cfg.bind,
            cfg.flatten_channels().0.len(),
            if cfg.password().is_some() { ", password protected" } else { "" }
        );
        tokio::select! {
            res = t7_server::serve(listener, &cfg) => {
                if let Err(e) = res {
                    fail(format!("cannot open voice socket on {}: {e}", cfg.bind));
                }
            }
            _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
        }
    });
}

fn fail(msg: impl std::fmt::Display) -> ! {
    tracing::error!("{msg}");
    std::process::exit(1);
}
