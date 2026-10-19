use windows::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_YESNO,
    MESSAGEBOX_STYLE, MessageBoxW,
};
use windows::core::PCWSTR;

use crate::wide;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    Error,
    Info,
    Question,
}

pub fn message_box(title: &str, text: &str, kind: DialogKind) -> bool {
    let style: MESSAGEBOX_STYLE = match kind {
        DialogKind::Error => MB_OK | MB_ICONERROR,
        DialogKind::Info => MB_OK | MB_ICONINFORMATION,
        DialogKind::Question => MB_YESNO | MB_ICONWARNING,
    };
    let title = wide(title);
    let text = wide(text);
    let answer = unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            style | MB_SETFOREGROUND,
        )
    };
    answer == IDYES
}
