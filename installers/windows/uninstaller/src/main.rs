#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use anyhow::{Context, Result, bail};
use common::{APP_EXE, APP_NAME, Button, Dialog, DialogIcon, UNINSTALLER_EXE, VERSION};
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

const BTN_UNINSTALL: i32 = 2001;
const BTN_RETRY: i32 = 2002;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // The interactive uninstaller copies itself to %TEMP% and starts this mode
    // so the install directory (including uninstall.exe) can be deleted.
    if args.iter().any(|arg| arg.eq_ignore_ascii_case("--cleanup")) {
        if let Some(dir) = cleanup_target(&args) {
            cleanup(&dir);
        }
        return;
    }

    let silent = args
        .iter()
        .any(|arg| is_flag(arg, &["s", "silent", "verysilent"]));
    let code = match run(silent) {
        Ok(code) => code,
        Err(err) => {
            let message = format!("{err:#}");
            if silent {
                let _ = std::fs::write(
                    std::env::temp_dir().join("osu-map-manager-uninstall.log"),
                    &message,
                );
            } else {
                common::show_error(&message);
            }
            1
        }
    };
    std::process::exit(code);
}

fn is_flag(arg: &str, names: &[&str]) -> bool {
    let flag = arg.trim_start_matches(['/', '-']);
    names.iter().any(|name| flag.eq_ignore_ascii_case(name))
}

/// Resolves the directory passed to the temporary cleanup copy. The flag is
/// only honored by a process running from %TEMP% under the name this program
/// copies itself to, and only for a folder that still looks like an install,
/// so an installed `uninstall.exe --cleanup <dir>` cannot wipe an arbitrary
/// directory.
fn cleanup_target(args: &[String]) -> Option<PathBuf> {
    let index = args
        .iter()
        .position(|arg| arg.eq_ignore_ascii_case("--cleanup"))?;
    let dir = PathBuf::from(args.get(index + 1)?);

    let exe = std::env::current_exe().ok()?;
    let name = exe.file_name()?.to_string_lossy();
    if !name.starts_with("osu-map-manager-uninstall-") || exe.parent()? != std::env::temp_dir() {
        return None;
    }
    if !dir.join(APP_EXE).exists() {
        return None;
    }
    Some(dir)
}

fn run(silent: bool) -> Result<i32> {
    let dir = std::env::current_exe()?
        .parent()
        .context("locating the install directory")?
        .to_path_buf();

    let registered_user = common::installed_dir(false).as_deref() == Some(dir.as_path());
    let registered_machine = common::installed_dir(true).as_deref() == Some(dir.as_path());
    if !registered_user
        && !registered_machine
        && !(dir.join(APP_EXE).exists() && dir.join(UNINSTALLER_EXE).exists())
    {
        bail!("this folder is not an osu! Map Manager installation");
    }

    let all_users =
        registered_machine || common::program_files().is_ok_and(|root| dir.starts_with(root));
    if all_users && !common::is_elevated() {
        let extra = if silent {
            vec!["/S".to_owned()]
        } else {
            Vec::new()
        };
        if !common::relaunch_elevated(&extra) {
            bail!("administrator approval is required to remove this install");
        }
        return Ok(0);
    }

    if !silent && common::show(&confirm_dialog(&dir))?.button != BTN_UNINSTALL {
        return Ok(2);
    }

    let exe = dir.join(APP_EXE);
    if is_running(&exe) {
        if silent {
            bail!("osu! Map Manager is running; close it before uninstalling");
        }
        while is_running(&exe) {
            if common::show(&running_dialog())?.button != BTN_RETRY {
                return Ok(2);
            }
        }
    }

    let desktop = common::installed_desktop_shortcut(all_users)
        || common::installed_desktop_shortcut(!all_users);
    remove_shortcuts(all_users, desktop);
    remove_registry(&dir);
    spawn_cleanup(&dir)?;
    Ok(0)
}

fn is_running(exe: &Path) -> bool {
    match std::fs::OpenOptions::new().write(true).open(exe) {
        Ok(_) => false,
        // Windows reports a file held open without write sharing as a sharing
        // or lock violation; `ErrorKind` maps both to `Uncategorized`.
        Err(err) => matches!(err.raw_os_error(), Some(32) | Some(33)),
    }
}

fn remove_registry(dir: &Path) {
    for all_users in [false, true] {
        if common::installed_dir(all_users).as_deref() == Some(dir) {
            common::remove_uninstall_entry(all_users);
        }
        if common::legacy_install_dir(all_users).as_deref() == Some(dir) {
            common::remove_legacy_uninstall_entry(all_users);
        }
    }
}

fn remove_shortcuts(all_users: bool, desktop: bool) {
    if let Ok(path) = common::start_menu_shortcut(all_users) {
        let _ = fs::remove_file(path);
    }
    if desktop && let Ok(path) = common::desktop_shortcut(all_users) {
        let _ = fs::remove_file(path);
    }
}

fn spawn_cleanup(dir: &Path) -> Result<()> {
    let temp = std::env::temp_dir().join(format!(
        "osu-map-manager-uninstall-{}.exe",
        std::process::id()
    ));
    fs::copy(std::env::current_exe()?, &temp)
        .with_context(|| format!("copying the uninstaller to {}", temp.display()))?;
    Command::new(&temp)
        .arg("--cleanup")
        .arg(dir)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| format!("starting {}", temp.display()))?;
    Ok(())
}

fn cleanup(dir: &Path) {
    // Let the original uninstaller process exit before removing its folder.
    std::thread::sleep(Duration::from_millis(400));
    for _ in 0..40 {
        if !dir.exists() || fs::remove_dir_all(dir).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    remove_self();
}

fn remove_self() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // raw_arg keeps the inner quotes intact; args() would wrap the whole
    // command string and cmd only deletes the first quoted path.
    let _ = Command::new("cmd")
        .raw_arg(format!(
            "/C ping -n 3 127.0.0.1 >NUL & del /F /Q \"{}\"",
            exe.display()
        ))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

fn confirm_dialog(dir: &Path) -> Dialog {
    let mut dialog = Dialog::new(APP_NAME, format!("Uninstall {APP_NAME} {VERSION}?"));
    dialog.content = dir.display().to_string();
    dialog.icon = DialogIcon::Warning;
    dialog.default_button = common::DIALOG_CANCEL;
    dialog.buttons = vec![
        Button {
            id: BTN_UNINSTALL,
            text: "Uninstall".to_owned(),
        },
        Button {
            id: common::DIALOG_CANCEL,
            text: "Cancel".to_owned(),
        },
    ];
    dialog
}

fn running_dialog() -> Dialog {
    let mut dialog = Dialog::new(APP_NAME, format!("{APP_NAME} is running"));
    dialog.content = "Close it, then choose Retry.".to_owned();
    dialog.icon = DialogIcon::Warning;
    dialog.default_button = BTN_RETRY;
    dialog.buttons = vec![
        Button {
            id: BTN_RETRY,
            text: "Retry".to_owned(),
        },
        Button {
            id: common::DIALOG_CANCEL,
            text: "Cancel".to_owned(),
        },
    ];
    dialog
}
