use super::MIN_EVENT_INTERVAL_MS;
use block2::RcBlock;
use objc2_app_kit::{NSEvent, NSEventMask};
use objc2_foundation::NSRunLoop;
use std::cell::Cell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub fn start(counter: Arc<AtomicU64>) {
    thread::spawn(move || watch_loop(counter));
}

/// Install a global mouse-moved monitor. Global monitors only fire for
/// events directed at OTHER apps, so movement over our own windows is not
/// counted (the agent has no UI, so this misses nothing in practice).
/// Requires the Accessibility (TCC) permission — the same caveat as the
/// global paste monitor; without it the monitor cannot be installed, so we
/// log one warning and the counter stays 0.
fn watch_loop(counter: Arc<AtomicU64>) {
    // Handler runs on this thread's run loop only, so Cell is enough.
    let last = Cell::new(Instant::now());
    let block = RcBlock::new(move |_event: NonNull<NSEvent>| {
        let now = Instant::now();
        if now.duration_since(last.get()) >= Duration::from_millis(MIN_EVENT_INTERVAL_MS) {
            last.set(now);
            counter.fetch_add(1, Ordering::Relaxed);
        }
    });

    let monitor = unsafe {
        NSEvent::addGlobalMonitorForEventsMatchingMask_handler(NSEventMask::MouseMoved, &block)
    };
    if monitor.is_none() {
        tracing::warn!(
            "could not install the global mouse monitor (grant the Accessibility \
             permission to enable it); mouse activity will be reported as 0"
        );
        return;
    }

    // The monitor's handler is delivered on this thread's run loop.
    let run_loop = unsafe { NSRunLoop::currentRunLoop() };
    unsafe { run_loop.run() };
}
