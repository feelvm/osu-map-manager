#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
mod collection;
mod local;
mod osu_api;
mod osu_db;
mod query;

use anyhow::Result;

fn main() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([1160.0, 620.0]),
        ..Default::default()
    };

    eframe::run_native(
        "osu! Map Manager",
        options,
        Box::new(|cc| Box::new(app::MapManagerApp::new(cc))),
    )
    .map_err(|err| anyhow::anyhow!(err.to_string()))
}
