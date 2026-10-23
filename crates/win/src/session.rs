use std::cell::RefCell;
use std::io;
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::SetProcessShutdownParameters;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, ENDSESSION_CLOSEAPP, GetMessageW, MSG,
    RegisterClassW, TranslateMessage, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::wide;

const CLASS_NAME: &str = "MostlyFansSessionWindow";
const EARLIEST_APPLICATION_SHUTDOWN_LEVEL: u32 = 0x3FF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    QueryEnd,
    Ending { close_app: bool },
    EndCancelled,
}

type Handler = Box<dyn FnMut(SessionEvent)>;

thread_local! {
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
}

fn dispatch(event: SessionEvent) {
    HANDLER.with(|handler| {
        if let Some(handler) = handler.borrow_mut().as_mut() {
            handler(event);
        }
    });
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_QUERYENDSESSION => {
            dispatch(SessionEvent::QueryEnd);
            LRESULT(1)
        }
        WM_ENDSESSION => {
            let event = if wparam.0 != 0 {
                SessionEvent::Ending {
                    close_app: (lparam.0 as u32) & ENDSESSION_CLOSEAPP != 0,
                }
            } else {
                SessionEvent::EndCancelled
            };
            dispatch(event);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

fn create_window() -> io::Result<HWND> {
    let instance = unsafe { GetModuleHandleW(PCWSTR::null()) }?;
    let class_name = wide(CLASS_NAME);
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let title = wide("MostlyFans session monitor");
    let window = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }?;
    Ok(window)
}

pub fn spawn_session_window(
    handler: impl FnMut(SessionEvent) + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    unsafe { SetProcessShutdownParameters(EARLIEST_APPLICATION_SHUTDOWN_LEVEL, 0) }.ok();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("session".into())
        .spawn(move || {
            HANDLER.with(|slot| *slot.borrow_mut() = Some(Box::new(handler)));
            match create_window() {
                Ok(_) => ready_tx.send(Ok(())).ok(),
                Err(error) => {
                    ready_tx.send(Err(error)).ok();
                    return;
                }
            };
            let mut message = MSG::default();
            while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
                unsafe {
                    TranslateMessage(&message).ok().ok();
                    DispatchMessageW(&message);
                }
            }
        })?;
    ready_rx
        .recv()
        .map_err(|_| io::Error::other("session thread exited before creating its window"))??;
    Ok(thread)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SendMessageW};

    use super::*;

    #[test]
    fn end_session_messages_reach_the_handler() {
        let (tx, rx) = mpsc::channel();
        spawn_session_window(move |event| {
            tx.send(event).ok();
        })
        .unwrap();
        let class_name = wide(CLASS_NAME);
        let window = unsafe { FindWindowW(PCWSTR(class_name.as_ptr()), PCWSTR::null()) }.unwrap();
        let answer = unsafe {
            SendMessageW(
                window,
                WM_QUERYENDSESSION,
                Some(WPARAM(0)),
                Some(LPARAM(ENDSESSION_CLOSEAPP as isize)),
            )
        };
        assert_eq!(answer, LRESULT(1));
        unsafe { SendMessageW(window, WM_ENDSESSION, Some(WPARAM(0)), Some(LPARAM(0))) };
        unsafe {
            SendMessageW(
                window,
                WM_ENDSESSION,
                Some(WPARAM(1)),
                Some(LPARAM(ENDSESSION_CLOSEAPP as isize)),
            )
        };
        let wait = Duration::from_secs(2);
        assert_eq!(rx.recv_timeout(wait).unwrap(), SessionEvent::QueryEnd);
        assert_eq!(rx.recv_timeout(wait).unwrap(), SessionEvent::EndCancelled);
        assert_eq!(
            rx.recv_timeout(wait).unwrap(),
            SessionEvent::Ending { close_app: true }
        );
    }
}
