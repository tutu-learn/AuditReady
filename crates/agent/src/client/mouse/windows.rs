use super::MIN_EVENT_INTERVAL_MS;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HMODULE, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, MSG, WH_MOUSE_LL, WM_MOUSEMOVE,
};

/// HC_ACTION hook code.
const HC_ACTION: i32 = 0;

thread_local! {
    /// The mouse-hook callback cannot capture state, so the counter and the
    /// throttle timestamp live in thread-local storage of the hook thread.
    static MOUSE_STATE: RefCell<Option<(Arc<AtomicU64>, Instant)>> = const { RefCell::new(None) };
}

pub fn start(counter: Arc<AtomicU64>) {
    thread::spawn(move || watch_loop(counter));
}

/// Install a WH_MOUSE_LL hook to observe WM_MOUSEMOVE. Low-level mouse
/// hooks are delivered on the installing thread's message loop.
fn watch_loop(counter: Arc<AtomicU64>) {
    MOUSE_STATE.with(|s| *s.borrow_mut() = Some((counter, Instant::now())));

    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), HMODULE::default(), 0) };
    if let Err(e) = hook {
        tracing::warn!(
            "could not install the mouse hook for activity monitoring: {}",
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

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION && wparam.0 as u32 == WM_MOUSEMOVE {
        MOUSE_STATE.with(|s| {
            if let Some((counter, last)) = s.borrow_mut().as_mut() {
                let now = Instant::now();
                if now.duration_since(*last) >= Duration::from_millis(MIN_EVENT_INTERVAL_MS) {
                    *last = now;
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
