use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// `true` when the process runs with an elevated (administrator) token.
pub fn is_elevated() -> bool {
    // SAFETY: the token handle is closed before returning and the buffer size
    // passed to GetTokenInformation matches TOKEN_ELEVATION.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Starts this executable again with `extra_args` appended, through the UAC
/// prompt. `false` means the user declined or the shell refused to start it.
pub fn relaunch_elevated(extra_args: &[String]) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };

    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    args.extend(extra_args.iter().map(OsString::from));
    let parameters = args
        .iter()
        .map(|arg| format!("\"{}\"", arg.to_string_lossy().replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ");

    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let parameters = wide(&parameters);
    // SAFETY: all three strings stay alive for the call; no output is used
    // beyond the shell's success value (> 32).
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            parameters.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    result as isize > 32
}

fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(Some(0))
        .collect()
}
