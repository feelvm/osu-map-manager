#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
mod app_icon;
mod app_update;
mod collection;
mod extras;
mod local;
mod osu_db;
mod osu_oauth;
mod query;
mod shrink;
mod skin_editor;
mod updates;

use anyhow::Result;

fn main() -> Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([1180.0, 640.0]);
    // The same app.ico embedded in the exe: the title bar and taskbar match
    // the file users see in Explorer.
    if let Some(icon) = app_icon::window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "osu! Map Manager",
        options,
        Box::new(|cc| Box::new(app::MapManagerApp::new(cc))),
    )
    .map_err(|err| anyhow::anyhow!(err.to_string()))
}
