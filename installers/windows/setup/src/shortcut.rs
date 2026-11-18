use anyhow::Result;
use common::{APP_EXE, APP_NAME, desktop_shortcut, start_menu_shortcut};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::core::{Interface, PCWSTR};

pub fn create(dir: &Path, all_users: bool, desktop: bool, had_desktop: bool) -> Result<()> {
    // SAFETY: initializing COM twice on the same thread returns S_FALSE and is
    // harmless; IShellLink needs it before CoCreateInstance.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let exe = dir.join(APP_EXE);
    create_link(&exe, dir, &start_menu_shortcut(all_users)?)?;

    if desktop {
        create_link(&exe, dir, &desktop_shortcut(all_users)?)?;
    } else if had_desktop {
        // Only remove a shortcut this app created; a fresh install into a
        // custom directory must not delete someone else's.
        if let Ok(path) = desktop_shortcut(all_users) {
            let _ = std::fs::remove_file(path);
        }
    }

    Ok(())
}

fn create_link(exe: &Path, dir: &Path, link: &Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let exe = wide(exe.as_os_str());
    let dir = wide(dir.as_os_str());
    let name = wide(OsStr::new(APP_NAME));
    let link = wide(link.as_os_str());

    // SAFETY: every wide buffer lives until the link is saved, and the COM
    // objects are released when this scope ends.
    unsafe {
        let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        shell_link.SetPath(PCWSTR(exe.as_ptr()))?;
        shell_link.SetWorkingDirectory(PCWSTR(dir.as_ptr()))?;
        shell_link.SetDescription(PCWSTR(name.as_ptr()))?;
        shell_link.SetIconLocation(PCWSTR(exe.as_ptr()), 0)?;
        let file: IPersistFile = shell_link.cast()?;
        file.Save(PCWSTR(link.as_ptr()), true)?;
    }

    Ok(())
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}
