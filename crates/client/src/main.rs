#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod icons;
mod net;
mod theme;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    // Separate settings per profile, e.g. to run two clients on one machine.
    let app_id = match std::env::var("T7_PROFILE") {
        Ok(p) if !p.trim().is_empty() => format!("t7-{}", p.trim()),
        _ => "t7".to_owned(),
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("T7")
            .with_app_id(&app_id)
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    let launch = parse_args();
    eframe::run_native(&app_id, options, Box::new(|cc| Ok(Box::new(app::App::new(cc, launch)))))
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
