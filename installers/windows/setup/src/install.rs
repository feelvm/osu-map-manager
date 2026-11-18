use anyhow::{Context, Result};
use common::{APP_EXE, APP_NAME, UNINSTALLER_EXE};
use flate2::read::DeflateDecoder;
use std::{
    fmt, fs,
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::Path,
};

/// Embedded by `build.rs` from the release exe and the built uninstaller.
const PAYLOAD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/payload.deflate"));
const UNINSTALLER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/uninstaller.exe"));

/// The app executable is locked by a running instance, so the swap has to
/// wait for it to close.
#[derive(Debug)]
pub struct AppRunning;

impl fmt::Display for AppRunning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("osu! Map Manager is running")
    }
}

impl std::error::Error for AppRunning {}

pub fn install(dir: &Path, all_users: bool, desktop: bool, had_desktop: bool) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let exe = dir.join(APP_EXE);
    if is_running(&exe) {
        return Err(AppRunning.into());
    }
    write_payload(&exe)?;

    let uninstaller = dir.join(UNINSTALLER_EXE);
    if is_running(&uninstaller) {
        return Err(AppRunning.into());
    }
    write_uninstaller(&uninstaller)?;

    crate::shortcut::create(dir, all_users, desktop, had_desktop)?;

    let size_kb = (fs::metadata(&exe)?.len() + fs::metadata(&uninstaller)?.len()) / 1024;
    common::write_uninstall_entry(dir, all_users, size_kb as u32, desktop)?;

    cleanup_legacy();
    Ok(())
}

fn is_running(exe: &Path) -> bool {
    match OpenOptions::new().write(true).open(exe) {
        Ok(_) => false,
        // Windows reports a file held open without write sharing as a sharing
        // or lock violation; `ErrorKind` maps both to `Uncategorized`.
        Err(err) => matches!(err.raw_os_error(), Some(32) | Some(33)),
    }
}

/// Decompresses the embedded payload into `<exe>.new` and moves it over the
/// old executable in one step, so a failed install never leaves a half-written
/// app behind.
fn write_payload(exe: &Path) -> Result<()> {
    let temp = exe.with_extension("exe.new");
    let file = File::create(&temp).with_context(|| format!("creating {}", temp.display()))?;
    let mut writer = BufWriter::new(file);
    let mut decoder = DeflateDecoder::new(PAYLOAD);
    std::io::copy(&mut decoder, &mut writer)
        .with_context(|| format!("unpacking the payload into {}", temp.display()))?;
    writer.flush()?;
    drop(writer);
    replace(&temp, exe)
}

fn write_uninstaller(uninstaller: &Path) -> Result<()> {
    let temp = uninstaller.with_extension("exe.new");
    fs::write(&temp, UNINSTALLER).with_context(|| format!("writing {}", temp.display()))?;
    replace(&temp, uninstaller)
}

fn replace(temp: &Path, target: &Path) -> Result<()> {
    if let Err(err) = fs::rename(temp, target) {
        let _ = fs::remove_file(temp);
        // Replacing a running image fails with access denied; a sharing or
        // lock violation is the other way Windows reports it.
        if matches!(err.raw_os_error(), Some(5) | Some(32) | Some(33)) {
            return Err(AppRunning.into());
        }
        return Err(err).with_context(|| format!("replacing {}", target.display()));
    }
    Ok(())
}

/// Removes what a previous Inno Setup install of the app left behind, so
/// Programs & Features shows a single entry after upgrading.
fn cleanup_legacy() {
    for all_users in [false, true] {
        let Some(old) = common::legacy_install_dir(all_users) else {
            continue;
        };
        for name in ["unins000.exe", "unins000.dat", "unins000.msg"] {
            let _ = fs::remove_file(old.join(name));
        }
        common::remove_legacy_uninstall_entry(all_users);
        if let Ok(menu) = common::legacy_start_menu_dir(all_users) {
            let _ = fs::remove_file(menu.join(format!("{APP_NAME}.lnk")));
            let _ = fs::remove_dir(&menu);
        }
    }
}
