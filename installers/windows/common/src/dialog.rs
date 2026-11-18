use crate::wide::wide;
use anyhow::{Result, bail};
use std::mem::size_of;
use windows_sys::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TD_ERROR_ICON, TD_INFORMATION_ICON,
    TD_SHIELD_ICON, TD_WARNING_ICON, TDCBF_CANCEL_BUTTON, TDCBF_OK_BUTTON,
    TDF_ALLOW_DIALOG_CANCELLATION, TDF_USE_COMMAND_LINKS, TDF_VERIFICATION_FLAG_CHECKED,
    TaskDialogIndirect,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

/// `IDCANCEL`, returned when the user dismisses a dialog without a choice.
pub const DIALOG_CANCEL: i32 = 2;

#[derive(Clone, Copy)]
pub enum DialogIcon {
    Info,
    Warning,
    Error,
    Shield,
}

#[derive(Clone, Copy, Default)]
pub enum CommonButtons {
    #[default]
    None,
    Ok,
    OkCancel,
    Cancel,
}

pub struct Button {
    pub id: i32,
    pub text: String,
}

pub struct Dialog {
    pub title: String,
    pub instruction: String,
    pub content: String,
    pub icon: DialogIcon,
    pub buttons: Vec<Button>,
    pub common: CommonButtons,
    /// Renders custom buttons as command links (a title, optionally followed
    /// by a newline and a note).
    pub command_links: bool,
    pub allow_cancel: bool,
    /// Checkbox text and its initial state.
    pub verification: Option<(String, bool)>,
    /// Button that receives Enter; `0` lets TaskDialog pick the first one.
    pub default_button: i32,
}

impl Dialog {
    pub fn new(title: impl Into<String>, instruction: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            instruction: instruction.into(),
            content: String::new(),
            icon: DialogIcon::Info,
            buttons: Vec::new(),
            common: CommonButtons::None,
            command_links: false,
            allow_cancel: true,
            verification: None,
            default_button: 0,
        }
    }
}

pub struct DialogResult {
    pub button: i32,
    pub checked: bool,
}

pub fn show(dialog: &Dialog) -> Result<DialogResult> {
    let title = wide(&dialog.title);
    let instruction = wide(&dialog.instruction);
    let content = if dialog.content.is_empty() {
        None
    } else {
        Some(wide(&dialog.content))
    };
    let verification = dialog.verification.as_ref().map(|(text, _)| wide(text));

    // TaskDialog keeps the pointers rather than copying the labels, so every
    // label buffer has to outlive the struct that points at it.
    let labels: Vec<Vec<u16>> = dialog
        .buttons
        .iter()
        .map(|button| wide(&button.text))
        .collect();
    let buttons: Vec<TASKDIALOG_BUTTON> = dialog
        .buttons
        .iter()
        .zip(&labels)
        .map(|(button, label)| TASKDIALOG_BUTTON {
            nButtonID: button.id,
            pszButtonText: label.as_ptr(),
        })
        .collect();

    let mut flags = TDF_ALLOW_DIALOG_CANCELLATION;
    if dialog.command_links {
        flags |= TDF_USE_COMMAND_LINKS;
    }
    if matches!(&dialog.verification, Some((_, true))) {
        flags |= TDF_VERIFICATION_FLAG_CHECKED;
    }
    if !dialog.allow_cancel {
        flags &= !TDF_ALLOW_DIALOG_CANCELLATION;
    }

    let config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        dwFlags: flags,
        dwCommonButtons: match dialog.common {
            CommonButtons::None => 0,
            CommonButtons::Ok => TDCBF_OK_BUTTON,
            CommonButtons::OkCancel => TDCBF_OK_BUTTON | TDCBF_CANCEL_BUTTON,
            CommonButtons::Cancel => TDCBF_CANCEL_BUTTON,
        },
        pszWindowTitle: title.as_ptr(),
        Anonymous1: TASKDIALOGCONFIG_0 {
            pszMainIcon: match dialog.icon {
                DialogIcon::Info => TD_INFORMATION_ICON,
                DialogIcon::Warning => TD_WARNING_ICON,
                DialogIcon::Error => TD_ERROR_ICON,
                DialogIcon::Shield => TD_SHIELD_ICON,
            },
        },
        pszMainInstruction: instruction.as_ptr(),
        pszContent: content
            .as_ref()
            .map_or(std::ptr::null(), |text| text.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: dialog.default_button,
        pszVerificationText: verification
            .as_ref()
            .map_or(std::ptr::null(), |text| text.as_ptr()),
        ..Default::default()
    };

    let mut button = DIALOG_CANCEL;
    let mut checked = 0;
    // SAFETY: the config and every pointer inside it are valid for the whole
    // call, and only the out-parameters are written by TaskDialog.
    let hr =
        unsafe { TaskDialogIndirect(&config, &mut button, std::ptr::null_mut(), &mut checked) };
    if hr < 0 {
        bail!("the setup dialog could not be shown (HRESULT {hr:#x})");
    }

    Ok(DialogResult {
        button,
        checked: checked != 0,
    })
}

/// Last-resort error display for when a TaskDialog cannot be created.
pub fn show_error(message: &str) {
    let message = wide(message);
    let title = wide(crate::APP_NAME);
    // SAFETY: both strings are null-terminated and outlive the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}
