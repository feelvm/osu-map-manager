use crate::APP_NAME;
use anyhow::{Context, Result, bail};
use std::env;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{
    FOLDERID_CommonPrograms, FOLDERID_Desktop, FOLDERID_Programs, FOLDERID_PublicDesktop,
    SHGetKnownFolderPath,
};
use windows_sys::core::GUID;

/// `%LOCALAPPDATA%\Programs\osu! Map Manager` or
/// `%ProgramFiles%\osu! Map Manager`.
pub fn install_dir(all_users: bool) -> Result<PathBuf> {
    let base = if all_users {
        program_files()?
    } else {
        env_path("LOCALAPPDATA")?.join("Programs")
    };
    Ok(base.join(APP_NAME))
}

pub fn program_files() -> Result<PathBuf> {
    env_path("ProgramFiles").or_else(|_| env_path("ProgramW6432"))
}

pub fn start_menu_shortcut(all_users: bool) -> Result<PathBuf> {
    let folder = if all_users {
        known_folder(&FOLDERID_CommonPrograms)?
    } else {
        known_folder(&FOLDERID_Programs)?
    };
    Ok(folder.join(shortcut_name()))
}

pub fn desktop_shortcut(all_users: bool) -> Result<PathBuf> {
    let folder = if all_users {
        known_folder(&FOLDERID_PublicDesktop)?
    } else {
        known_folder(&FOLDERID_Desktop)?
    };
    Ok(folder.join(shortcut_name()))
}

/// Folder the old Inno Setup build put its Start Menu shortcut in. Inno
/// creates `Programs\osu! Map Manager\osu! Map Manager.lnk`, while this setup
/// puts the shortcut straight into `Programs`.
pub fn legacy_start_menu_dir(all_users: bool) -> Result<PathBuf> {
    let programs = if all_users {
        known_folder(&FOLDERID_CommonPrograms)?
    } else {
        known_folder(&FOLDERID_Programs)?
    };
    Ok(programs.join(APP_NAME))
}

fn shortcut_name() -> String {
    format!("{APP_NAME}.lnk")
}

fn env_path(name: &str) -> Result<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .with_context(|| format!("{name} is not set"))
}

fn known_folder(id: &GUID) -> Result<PathBuf> {
    // SAFETY: on success SHGetKnownFolderPath hands back one null-terminated
    // UTF-16 path allocated with CoTaskMemAlloc, which is copied out and freed
    // exactly once below.
    unsafe {
        let mut raw: *mut u16 = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut raw);
        if hr < 0 || raw.is_null() {
            bail!("resolving a shell folder failed (HRESULT {hr:#x})");
        }
        let mut len = 0;
        while *raw.add(len) != 0 {
            len += 1;
        }
        let path = PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(raw, len)));
        CoTaskMemFree(raw.cast());
        Ok(path)
    }
}
