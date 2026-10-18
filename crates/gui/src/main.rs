#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod connection;
mod curve_plot;
mod curves;
mod fans;
mod format;
mod sensors;
mod settings;
mod tray;

use eframe::egui;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FANFORGE_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FanForge")
            .with_inner_size([980.0, 680.0])
            .with_min_inner_size([720.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "FanForge",
        options,
        Box::new(|creation| Ok(Box::new(app::FanForgeApp::new(&creation.egui_ctx)))),
    )
}
