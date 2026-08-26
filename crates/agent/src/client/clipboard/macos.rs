use super::{ClipboardEvent, EventKind, MAX_CAPTURE_BYTES};
use block2::RcBlock;
use chrono::Utc;
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSPasteboard, NSWorkspace};
use objc2_foundation::{NSRunLoop, NSString};
use std::ptr::NonNull;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::Duration;

/// Key code for the "V" key (Cmd+V = paste).
const KEY_CODE_V: u16 = 9;
/// UTI for plain text on the pasteboard (same as NSPasteboardTypeString).
const TEXT_TYPE: &str = "public.utf8-plain-text";

pub fn start() -> Receiver<ClipboardEvent> {
    let (tx, rx) = std::sync::mpsc::channel();

    {
        let tx = tx.clone();
        thread::spawn(move || copy_watch_loop(tx));
    }
    thread::spawn(move || paste_watch_loop(tx));

    rx
}

/// Poll NSPasteboard.changeCount; a change means something was copied.
fn copy_watch_loop(tx: Sender<ClipboardEvent>) {
    let pasteboard = unsafe { NSPasteboard::generalPasteboard() };
    let mut last_change_count = unsafe { pasteboard.changeCount() };

    loop {
        thread::sleep(Duration::from_secs(1));
        autoreleasepool(|_| {
            let change_count = unsafe { pasteboard.changeCount() };
            if change_count == last_change_count {
                return;
            }
            last_change_count = change_count;

            let text = clipboard_text(&pasteboard);
            let size = text.as_ref().map(|t| t.len() as u64).unwrap_or(0);
            let event = ClipboardEvent {
                ts: Utc::now(),
                kind: EventKind::Copy,
                app: frontmost_app_name(),
                size_bytes: size,
                text,
            };
            if tx.send(event).is_err() {
                // Receiver gone; nothing more to do.
                last_change_count = change_count;
            }
        });
    }
}

/// Install a global Cmd+V key monitor for paste events. Requires the
/// Accessibility (TCC) permission; without it the monitor cannot be
/// installed, so we log one warning and continue copy-only.
fn paste_watch_loop(tx: Sender<ClipboardEvent>) {
    let block = RcBlock::new(move |event: NonNull<NSEvent>| {
        autoreleasepool(|_| {
            let event = unsafe { event.as_ref() };
            let is_cmd_v = unsafe { event.keyCode() } == KEY_CODE_V
                && unsafe { event.modifierFlags() }
                    .contains(NSEventModifierFlags::NSEventModifierFlagCommand);
            if !is_cmd_v {
                return;
            }

            let pasteboard = unsafe { NSPasteboard::generalPasteboard() };
            let text = clipboard_text(&pasteboard);
            let size = text.as_ref().map(|t| t.len() as u64).unwrap_or(0);
            let event = ClipboardEvent {
                ts: Utc::now(),
                kind: EventKind::Paste,
                app: frontmost_app_name(),
                size_bytes: size,
                text,
            };
            let _ = tx.send(event);
        });
    });

    let monitor = unsafe {
        NSEvent::addGlobalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block)
    };
    if monitor.is_none() {
        tracing::warn!(
            "could not install the global paste monitor (grant the Accessibility \
             permission to enable it); continuing with copy events only"
        );
        return;
    }

    // The monitor's handler is delivered on this thread's run loop.
    let run_loop = unsafe { NSRunLoop::currentRunLoop() };
    unsafe { run_loop.run() };
}

/// Read the current clipboard text, capped at [`MAX_CAPTURE_BYTES`].
fn clipboard_text(pasteboard: &NSPasteboard) -> Option<String> {
    let text_type = NSString::from_str(TEXT_TYPE);
    let text = unsafe { pasteboard.stringForType(&text_type) }?;
    Some(truncate_utf8(&text.to_string(), MAX_CAPTURE_BYTES))
}

/// Name of the frontmost application (source for copies, destination for
/// pastes); `None` when it cannot be determined.
fn frontmost_app_name() -> Option<String> {
    let workspace = unsafe { NSWorkspace::sharedWorkspace() };
    let app = unsafe { workspace.frontmostApplication() }?;
    let name = unsafe { app.localizedName() }?;
    Some(name.to_string())
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
