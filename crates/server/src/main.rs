use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use t7_server::config::Config;
use tokio::net::TcpListener;

/// T7 voice server.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Path to the config file (created with defaults if missing).
    #[arg(short, long, default_value = "t7-server.toml", global = true)]
    config: PathBuf,
    /// Override the bind address from the config, e.g. 0.0.0.0:9988.
    #[arg(short, long)]
    bind: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// The server's owner, who may do everything.
    Owner {
        #[command(subcommand)]
        action: OwnerAction,
    },
}

#[derive(Subcommand)]
enum OwnerAction {
    /// Print a new claim token. Whoever redeems it in T7 becomes the owner in
    /// place of the current one, say after they lost their key. Run it while
    /// the server is stopped.
    Reset,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        // No color codes when logs go to a file or a container log collector.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let args = Args::parse();
    let data_dir = data_dir(&args.config);
    if let Some(Command::Owner { action: OwnerAction::Reset }) = args.command {
        let token =
            t7_server::reset_owner_claim(&data_dir).unwrap_or_else(|e| fail(format!("owner reset failed: {e}")));
        println!("claim token: {token}");
        println!("Start the server. In T7, the ⋯ next to the server's name, then Claim ownership: it works once.");
        return;
    }
    let mut cfg = Config::load_or_create(&args.config).unwrap_or_else(|e| fail(e));
    if let Some(bind) = args.bind {
        cfg.bind = bind;
    }

    let runtime =
        tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("failed to start tokio runtime");
    runtime.block_on(async {
        let listener =
            TcpListener::bind(&cfg.bind).await.unwrap_or_else(|e| fail(format!("cannot bind {}: {e}", cfg.bind)));
        tracing::info!(
            "\"{}\" listening on {} (TCP control + UDP voice{})",
            cfg.name,
            cfg.bind,
            if cfg.password().is_some() { ", password protected" } else { "" }
        );
        tokio::select! {
            res = t7_server::serve(listener, &cfg, Some(&data_dir)) => {
                if let Err(e) = res {
                    fail(format!("server failed: {e}"));
                }
            }
            _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
        }
    });
}

fn data_dir(config: &Path) -> PathBuf {
    match config.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_owned(),
        _ => PathBuf::from("."),
    }
}

fn fail(msg: impl std::fmt::Display) -> ! {
    tracing::error!("{msg}");
    std::process::exit(1);
}
