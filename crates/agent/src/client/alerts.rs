//! Modal alert popups for connection-state changes. Fired on the
//! connected/disconnected transitions detected in `super::stats`, not on
//! every failed cycle, so a prolonged outage does not spam the user.
//!
//! OS notification-center banners (the previous approach, via notify-rust)
//! turned out to be silently swallowed depending on the user's notification
//! permissions/Focus settings, with no way for the agent to detect that. A
//! native modal dialog the user must click "OK" on is not subject to that —
//! it force-shows on top of whatever the user is doing, same as any other
//! OS-level alert box. Each popup runs on its own detached thread so a user
//! leaving one open cannot stall the report loop that raised it.
//!
//! Supported on Windows and macOS only, same as `super::ui` — see that
//! module's doc comment for why Linux stays headless (logs only) instead.

use super::stats::Alert;

/// Show the popup (if any) called for by a stats update's returned [`Alert`].
#[cfg(any(target_os = "macos", windows))]
pub fn fire(alert: Alert) {
    use rfd::{MessageButtons, MessageDialog, MessageLevel};

    match alert {
        Alert::None => {}
        Alert::Disconnected(error) => {
            std::thread::spawn(move || {
                MessageDialog::new()
                    .set_title("AuditReady — connection lost")
                    .set_description(format!(
                        "Could not reach the AuditReady server:\n{error}\n\nMonitoring continues locally and will resume reporting automatically."
                    ))
                    .set_level(MessageLevel::Warning)
                    .set_buttons(MessageButtons::Ok)
                    .show();
            });
        }
        Alert::Reconnected => {
            std::thread::spawn(|| {
                MessageDialog::new()
                    .set_title("AuditReady — connection restored")
                    .set_description("Reporting to the AuditReady server has resumed.")
                    .set_level(MessageLevel::Info)
                    .set_buttons(MessageButtons::Ok)
                    .show();
            });
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn fire(alert: Alert) {
    match alert {
        Alert::None => {}
        Alert::Disconnected(error) => {
            tracing::warn!("connection lost (no popup on this platform): {}", error);
        }
        Alert::Reconnected => {
            tracing::info!("connection restored (no popup on this platform)");
        }
    }
}
