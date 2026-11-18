//! Pieces shared by the native Windows setup and uninstaller: install paths,
//! the uninstall registry entry, UAC elevation and TaskDialog dialogs.

mod dialog;
mod elevate;
mod paths;
mod registry;
mod wide;

use anyhow::{Context, Result};

pub use dialog::{
    Button, CommonButtons, DIALOG_CANCEL, Dialog, DialogIcon, DialogResult, show, show_error,
};
pub use elevate::{is_elevated, relaunch_elevated};
pub use paths::{
    desktop_shortcut, install_dir, legacy_start_menu_dir, program_files, start_menu_shortcut,
};
pub use registry::{
    Installed, find_installed, installed_desktop_shortcut, installed_dir, installed_version,
    legacy_install_dir, remove_legacy_uninstall_entry, remove_uninstall_entry,
    write_uninstall_entry,
};

pub const APP_NAME: &str = "osu! Map Manager";
pub const APP_EXE: &str = "osu-map-manager.exe";
pub const UNINSTALLER_EXE: &str = "uninstall.exe";
pub const APP_ID: &str = "{39302626-0751-446A-B69C-61480EF09C17}";
/// Uninstall key the previous Inno Setup installer wrote; removed on upgrade.
pub const LEGACY_APP_ID: &str = "{39302626-0751-446A-B69C-61480EF09C17}_is1";
pub const PUBLISHER: &str = "Stanislav";
pub const PROJECT_URL: &str = "https://github.com/feelvm/osu-map-manager";

/// Stamped by `build.rs` from the app's root `Cargo.toml` (or
/// `OSU_MAP_MANAGER_VERSION` during a release build).
pub const VERSION: &str = env!("OSU_MAP_MANAGER_VERSION");

/// Opens the app from its install directory, so the setup can offer a launch
/// after installing without knowing anything else about it.
pub fn launch_app(dir: &std::path::Path) -> Result<()> {
    std::process::Command::new(dir.join(APP_EXE))
        .current_dir(dir)
        .spawn()
        .with_context(|| format!("starting {}", dir.join(APP_EXE).display()))?;
    Ok(())
}
