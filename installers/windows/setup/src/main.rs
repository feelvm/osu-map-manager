#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod install;
mod shortcut;

use anyhow::{Result, bail};
use common::{APP_NAME, Button, CommonButtons, Dialog, DialogIcon, VERSION, desktop_shortcut};
use std::path::{Path, PathBuf};

const BTN_FOR_ME: i32 = 1001;
const BTN_FOR_ALL: i32 = 1002;
const BTN_INSTALL: i32 = 1003;
const BTN_RETRY: i32 = 1004;
const BTN_LAUNCH: i32 = 1005;
const BTN_CLOSE: i32 = 1006;

fn main() {
    let args = Args::parse();
    let code = match run(&args) {
        Ok(code) => code,
        Err(err) => {
            let message = format!("{err:#}");
            if args.silent {
                // No console and no dialog in silent mode; leave a trail for
                // unattended installs that fail.
                let _ = std::fs::write(
                    std::env::temp_dir().join("osu-map-manager-setup.log"),
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

#[derive(Default)]
struct Args {
    silent: bool,
    all_users: bool,
    desktop_icon: Option<bool>,
    dir: Option<PathBuf>,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self::default();
        for raw in std::env::args().skip(1) {
            let flag = raw.trim_start_matches(['/', '-']);
            let (name, value) = flag.split_once('=').map_or((flag, None), |(name, value)| {
                (name, Some(value.trim_matches('"')))
            });
            match name.to_ascii_lowercase().as_str() {
                "s" | "silent" | "verysilent" => args.silent = true,
                "allusers" => args.all_users = true,
                "desktopicon" => args.desktop_icon = Some(true),
                "nodesktopicon" => args.desktop_icon = Some(false),
                "d" | "dir" => {
                    if let Some(value) = value.filter(|value| !value.is_empty()) {
                        args.dir = Some(PathBuf::from(value));
                    }
                }
                _ => {}
            }
        }
        args
    }
}

fn run(args: &Args) -> Result<i32> {
    let existing = common::find_installed();
    let mut all_users = existing
        .as_ref()
        .map_or(args.all_users, |installed| installed.all_users);
    let mut dir = args
        .dir
        .clone()
        .or_else(|| existing.as_ref().map(|installed| installed.dir.clone()))
        .unwrap_or(common::install_dir(all_users)?);

    // An all-users install lives in Program Files and HKLM, so it needs an
    // elevated token before anything else happens.
    if all_users && !common::is_elevated() {
        if !common::relaunch_elevated(&[]) {
            bail!("administrator approval is required for an all-users install");
        }
        return Ok(0);
    }

    // Only a shortcut this app owned may be removed when the box is left
    // unchecked; a fresh install into a custom directory must not touch one.
    let had_desktop = common::installed_desktop_shortcut(all_users)
        || (existing.is_some() && desktop_shortcut(all_users).is_ok_and(|path| path.exists()));

    if args.silent {
        let desktop = args.desktop_icon.unwrap_or(had_desktop);
        install::install(&dir, all_users, desktop, had_desktop)?;
        return Ok(0);
    }

    let desktop;
    if existing.is_some() {
        let installed_version = common::installed_version(all_users).unwrap_or_default();
        let desktop_default = desktop_shortcut(all_users).is_ok_and(|path| path.exists());
        let answer = common::show(&install_dialog(
            &dir,
            all_users,
            &installed_version,
            desktop_default,
        ))?;
        if answer.button != BTN_INSTALL {
            return Ok(2);
        }
        desktop = answer.checked;
    } else if args.all_users {
        desktop = args.desktop_icon.unwrap_or(false);
    } else {
        let answer = common::show(&choose_dialog(args.desktop_icon.unwrap_or(false)))?;
        match answer.button {
            BTN_FOR_ME => desktop = answer.checked,
            BTN_FOR_ALL => {
                if common::is_elevated() {
                    all_users = true;
                    dir = common::install_dir(true)?;
                    desktop = answer.checked;
                } else {
                    let desktop_flag = if answer.checked {
                        "/DESKTOPICON"
                    } else {
                        "/NODESKTOPICON"
                    };
                    if !common::relaunch_elevated(&[
                        "/ALLUSERS".to_owned(),
                        desktop_flag.to_owned(),
                    ]) {
                        common::show_error(
                            "Administrator approval is required to install for all users.",
                        );
                        return Ok(1);
                    }
                    return Ok(0);
                }
            }
            _ => return Ok(2),
        }
    }

    loop {
        match install::install(&dir, all_users, desktop, had_desktop) {
            Ok(()) => break,
            Err(err) if err.downcast_ref::<install::AppRunning>().is_some() => {
                if common::show(&running_dialog())?.button != BTN_RETRY {
                    return Ok(2);
                }
            }
            Err(err) => return Err(err),
        }
    }

    if common::show(&done_dialog(&dir, all_users))?.button == BTN_LAUNCH {
        common::launch_app(&dir)?;
    }
    Ok(0)
}

fn choose_dialog(desktop_default: bool) -> Dialog {
    let mut dialog = Dialog::new(APP_NAME, format!("Install {APP_NAME} {VERSION}?"));
    dialog.icon = DialogIcon::Info;
    dialog.command_links = true;
    dialog.common = CommonButtons::Cancel;
    dialog.default_button = BTN_FOR_ME;
    dialog.verification = Some(("Create a desktop shortcut".to_owned(), desktop_default));
    dialog.buttons = vec![
        Button {
            id: BTN_FOR_ME,
            text: "Install for me".to_owned(),
        },
        Button {
            id: BTN_FOR_ALL,
            text: "Install for all users".to_owned(),
        },
    ];
    dialog
}

fn install_dialog(
    dir: &Path,
    all_users: bool,
    installed_version: &str,
    desktop_default: bool,
) -> Dialog {
    let action = if installed_version == VERSION {
        "Repair"
    } else {
        "Update"
    };
    let mut dialog = Dialog::new(APP_NAME, format!("{action} {APP_NAME} {VERSION}?"));
    dialog.content = format!(
        "{}\n{}",
        dir.display(),
        if all_users {
            "Installed for all users"
        } else {
            "Installed for you"
        }
    );
    dialog.icon = DialogIcon::Info;
    dialog.default_button = BTN_INSTALL;
    dialog.verification = Some(("Create a desktop shortcut".to_owned(), desktop_default));
    dialog.buttons = vec![
        Button {
            id: BTN_INSTALL,
            text: action.to_owned(),
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
    dialog.verification = None;
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

fn done_dialog(dir: &Path, all_users: bool) -> Dialog {
    let mut dialog = Dialog::new(APP_NAME, format!("{APP_NAME} {VERSION} is installed"));
    dialog.content = format!(
        "{}\n{}",
        dir.display(),
        if all_users {
            "Installed for all users"
        } else {
            "Installed for you"
        }
    );
    dialog.icon = DialogIcon::Info;
    dialog.default_button = BTN_LAUNCH;
    dialog.buttons = vec![
        Button {
            id: BTN_LAUNCH,
            text: "Launch".to_owned(),
        },
        Button {
            id: BTN_CLOSE,
            text: "Close".to_owned(),
        },
    ];
    dialog
}
