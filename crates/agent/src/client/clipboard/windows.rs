use super::{ClipboardEvent, EventKind, MAX_CAPTURE_BYTES};
use chrono::Utc;
use std::cell::RefCell;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::Duration;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HGLOBAL, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, SetWindowsHookExW,
    KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN,
};

/// CF_UNICODETEXT clipboard format.
const CF_UNICODETEXT: u32 = 13;
/// Virtual-key code for "V".
const VK_V: u32 = 0x56;
/// HC_ACTION hook code.
const HC_ACTION: i32 = 0;

thread_local! {
    /// The keyboard-hook callback cannot capture state, so the paste
    /// sender lives in thread-local storage of the hook thread.
    static PASTE_SENDER: RefCell<Option<Sender<ClipboardEvent>>> = const { RefCell::new(None) };
}

pub fn start() -> Receiver<ClipboardEvent> {
    let (tx, rx) = std::sync::mpsc::channel();

    {
        let tx = tx.clone();
        thread::spawn(move || copy_watch_loop(tx));
    }
    thread::spawn(move || paste_watch_loop(tx));

    rx
}

/// Poll the clipboard sequence number; a change means something was copied.
fn copy_watch_loop(tx: Sender<ClipboardEvent>) {
    let mut last_seq = unsafe { GetClipboardSequenceNumber() };

    loop {
        thread::sleep(Duration::from_millis(500));
        let seq = unsafe { GetClipboardSequenceNumber() };
        if seq == last_seq {
            continue;
        }
        last_seq = seq;

        let text = clipboard_text();
        let size = text.as_ref().map(|t| t.len() as u64).unwrap_or(0);
        let event = ClipboardEvent {
            ts: Utc::now(),
            kind: EventKind::Copy,
            app: foreground_app_name(),
            size_bytes: size,
            text,
        };
        if tx.send(event).is_err() {
            return; // Receiver gone.
        }
    }
}

/// Install a WH_KEYBOARD_LL hook to observe Ctrl+V. Low-level keyboard
/// hooks are delivered on the installing thread's message loop.
fn paste_watch_loop(tx: Sender<ClipboardEvent>) {
    PASTE_SENDER.with(|s| *s.borrow_mut() = Some(tx));

    let hook =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), HMODULE::default(), 0) };
    if let Err(e) = hook {
        tracing::warn!(
            "could not install the keyboard hook for paste monitoring: {}",
            e
        );
        return;
    }

    // Pump messages forever; the hook callback runs on this thread.
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION && wparam.0 as u32 == WM_KEYDOWN as u32 {
        let kbd = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if kbd.vkCode == VK_V && ctrl_pressed() {
            PASTE_SENDER.with(|s| {
                if let Some(tx) = s.borrow().as_ref() {
                    let text = clipboard_text();
                    let size = text.as_ref().map(|t| t.len() as u64).unwrap_or(0);
                    let event = ClipboardEvent {
                        ts: Utc::now(),
                        kind: EventKind::Paste,
                        app: foreground_app_name(),
                        size_bytes: size,
                        text,
                    };
                    let _ = tx.send(event);
                }
            });
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn ctrl_pressed() -> bool {
    let state = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) };
    (state as u16 & 0x8000) != 0
}

/// Read the current clipboard text (CF_UNICODETEXT), capped at
/// [`MAX_CAPTURE_BYTES`]. `None` when the clipboard holds no text.
fn clipboard_text() -> Option<String> {
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT).is_err() {
            return None;
        }
        OpenClipboard(HWND::default()).ok()?;
        let result = read_clipboard_text_inner();
        let _ = CloseClipboard();
        result.map(|t| truncate_utf8(&t, MAX_CAPTURE_BYTES))
    }
}

unsafe fn read_clipboard_text_inner() -> Option<String> {
    unsafe {
        let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
        let hglobal = HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            return None;
        }
        let wide = ptr as *const u16;
        // Find the null terminator, but never scan further than the capture
        // cap so a malformed clipboard cannot cause an unbounded read.
        let max_chars = MAX_CAPTURE_BYTES / 2 + 1;
        let mut len = 0usize;
        while len < max_chars && *wide.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(wide, len));
        let _ = GlobalUnlock(hglobal);
        Some(text)
    }
}

/// Executable file name of the process owning the foreground window
/// (source for copies, destination for pastes).
fn foreground_app_name() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        result.ok()?;

        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\')
            .next()
            .filter(|name| !name.is_empty())
            .map(|name| name.to_string())
    }
}

fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
