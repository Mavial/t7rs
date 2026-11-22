#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod icons;
mod net;
mod ptt;
mod resample;
mod settings;
mod shell;
mod theme;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    // Separate settings per profile, e.g. to run two clients on one machine.
    let profile = std::env::var("T7_PROFILE").ok().filter(|p| !p.trim().is_empty());
    let (store, settings) = settings::Store::open(profile.as_deref().map(str::trim));
    let launch = parse_args();

    let result = shell::run(egui::Context::default(), store, |ctx| {
        app::App::new(ctx, settings, launch)
    });
    if let Err(e) = result {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

/// `t7-client [ADDRESS] [--nick NAME]`: optionally connect right away.
fn parse_args() -> Option<app::Launch> {
    let mut args = std::env::args().skip(1);
    let mut launch = app::Launch::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--nick" | "-n" => launch.nickname = args.next(),
            _ => launch.address = arg,
        }
    }
    (!launch.address.is_empty()).then_some(launch)
}
