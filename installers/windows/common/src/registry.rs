use crate::{
    APP_EXE, APP_ID, APP_NAME, LEGACY_APP_ID, PROJECT_URL, PUBLISHER, UNINSTALLER_EXE, VERSION,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ};

#[derive(Debug, Clone)]
pub struct Installed {
    pub all_users: bool,
    pub dir: PathBuf,
}

pub fn uninstall_key_path() -> String {
    format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{APP_ID}")
}

fn legacy_uninstall_key_path() -> String {
    format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{LEGACY_APP_ID}")
}

fn hive(all_users: bool) -> RegKey {
    RegKey::predef(if all_users {
        HKEY_LOCAL_MACHINE
    } else {
        HKEY_CURRENT_USER
    })
}

/// Finds an existing app install, preferring the current user's entry.
pub fn find_installed() -> Option<Installed> {
    for all_users in [false, true] {
        let hive = hive(all_users);
        for key in [uninstall_key_path(), legacy_uninstall_key_path()] {
            if let Some(dir) = read_install_dir(&hive, &key) {
                return Some(Installed { all_users, dir });
            }
        }
    }
    None
}

/// Install directory from this setup's own uninstall entry in `all_users`.
pub fn installed_dir(all_users: bool) -> Option<PathBuf> {
    read_install_dir(&hive(all_users), &uninstall_key_path())
}

/// Install directory recorded by the previous Inno Setup build.
pub fn legacy_install_dir(all_users: bool) -> Option<PathBuf> {
    read_install_dir(&hive(all_users), &legacy_uninstall_key_path())
}

/// Version recorded in this setup's uninstall entry.
pub fn installed_version(all_users: bool) -> Option<String> {
    let key = hive(all_users)
        .open_subkey_with_flags(uninstall_key_path(), KEY_READ)
        .ok()?;
    key.get_value("DisplayVersion").ok()
}

fn read_install_dir(hive: &RegKey, key: &str) -> Option<PathBuf> {
    let key = hive.open_subkey_with_flags(key, KEY_READ).ok()?;

    let location: Option<String> = key.get_value("InstallLocation").ok();
    if let Some(location) = location.filter(|value| !value.trim().is_empty()) {
        return Some(PathBuf::from(location));
    }

    // Inno Setup's uninstall entry can omit InstallLocation; the app path it
    // records and the quoted uninstaller path both name the install directory.
    let app_path: Option<String> = key.get_value("Inno Setup: App Path").ok();
    if let Some(app_path) = app_path.filter(|value| !value.trim().is_empty()) {
        return Some(PathBuf::from(app_path));
    }

    let uninstall: String = key.get_value("UninstallString").ok()?;
    uninstall_dir(&uninstall)
}

fn uninstall_dir(uninstall: &str) -> Option<PathBuf> {
    let uninstall = uninstall.trim();
    let exe = if let Some(rest) = uninstall.strip_prefix('"') {
        &rest[..rest.find('"')?]
    } else {
        uninstall.split(' ').next()?
    };
    Path::new(exe).parent().map(Path::to_path_buf)
}

pub fn write_uninstall_entry(
    dir: &Path,
    all_users: bool,
    size_kb: u32,
    desktop: bool,
) -> Result<()> {
    let (key, _) = hive(all_users)
        .create_subkey(uninstall_key_path())
        .context("writing the uninstall registry entry")?;
    let exe = dir.join(APP_EXE);
    let uninstaller = dir.join(UNINSTALLER_EXE);

    key.set_value("DisplayName", &APP_NAME)?;
    key.set_value("DisplayVersion", &VERSION)?;
    key.set_value("DisplayIcon", &format!("\"{}\",0", exe.display()))?;
    key.set_value("InstallLocation", &dir.display().to_string())?;
    key.set_value("UninstallString", &format!("\"{}\"", uninstaller.display()))?;
    key.set_value(
        "QuietUninstallString",
        &format!("\"{}\" /S", uninstaller.display()),
    )?;
    key.set_value("Publisher", &PUBLISHER)?;
    key.set_value("URLInfoAbout", &PROJECT_URL)?;
    key.set_value("HelpLink", &PROJECT_URL)?;
    key.set_value("EstimatedSize", &size_kb)?;
    key.set_value("DesktopShortcut", &u32::from(desktop))?;
    key.set_value("NoModify", &1u32)?;
    key.set_value("NoRepair", &1u32)?;
    Ok(())
}

/// Whether this install created a desktop shortcut (recorded so an
/// uninstall does not delete a shortcut owned by another install).
pub fn installed_desktop_shortcut(all_users: bool) -> bool {
    hive(all_users)
        .open_subkey_with_flags(uninstall_key_path(), KEY_READ)
        .and_then(|key| key.get_value::<u32, _>("DesktopShortcut"))
        .is_ok_and(|value| value != 0)
}

pub fn remove_uninstall_entry(all_users: bool) {
    let hive = hive(all_users);
    let _ = hive.delete_subkey(uninstall_key_path());
    let _ = hive.delete_subkey(legacy_uninstall_key_path());
}

pub fn remove_legacy_uninstall_entry(all_users: bool) {
    let _ = hive(all_users).delete_subkey(legacy_uninstall_key_path());
}
