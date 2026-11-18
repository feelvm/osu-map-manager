use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

/// NUL-terminated UTF-16 for Win32 calls.
pub(crate) fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}
