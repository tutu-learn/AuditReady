//! Tray icon + stats dashboard window for client mode.
//!
//! Runs on the process's main thread (required for the tray icon on macOS,
//! and simplest cross-platform). Background report loops run on their own
//! threads/tasks and publish into `stats::SharedStats`, which this UI polls
//! on a timer — there is no other coupling between the two.
//!
//! The tray icon alone answers "is it running"; clicking it (or its "Open
//! Dashboard" item) opens a small window with connection status and the
//! activity counters from `stats::ClientStats`.
//!
//! Supported on Windows and macOS only, matching where client mode's
//! clipboard/mouse monitoring already works (see `super::clipboard`,
//! `super::mouse`). `tray-icon`/`muda` pull in GTK + libappindicator +
//! libxdo on Linux, which don't link against the static-musl target the
//! Linux release build uses — rather than break that build, Linux keeps
//! running headless, same as clipboard/mouse already do there.

#[cfg(any(target_os = "macos", windows))]
pub use supported::run;

#[cfg(not(any(target_os = "macos", windows)))]
pub fn run(_stats: super::stats::SharedStats) -> anyhow::Result<()> {
    tracing::warn!(
        "client tray/dashboard UI is not supported on this platform (Windows or macOS only); running headless"
    );
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

#[cfg(any(target_os = "macos", windows))]
mod supported {
    use crate::client::stats::{self, ClientStats};
    use crate::client::stats::SharedStats;
    use iced::widget::{column, container, row, text};
    use iced::window;
    use iced::{Element, Length, Subscription, Task, Theme};
    use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use std::time::Duration;
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    #[derive(Debug, Clone)]
    enum Message {
        Tick,
        OpenDashboard,
        Quit,
        WindowOpened,
        WindowClosed(window::Id),
    }

    struct State {
        stats: SharedStats,
        // Kept alive for as long as the tray icon should be shown; dropping
        // it removes the icon.
        _tray: Option<TrayIcon>,
        open_item_id: MenuId,
        quit_item_id: MenuId,
        dashboard: Option<window::Id>,
        snapshot: ClientStats,
    }

    /// Blocks the calling thread until the user quits from the tray menu.
    pub fn run(stats: SharedStats) -> iced::Result {
        iced::daemon(move || boot(stats.clone()), update, view)
            .subscription(subscription)
            .title(|_state: &State, _id| "AuditReady — Client Dashboard".to_string())
            .theme(|_state: &State, _id| Theme::Dark)
            .run()
    }

    fn boot(stats: SharedStats) -> (State, Task<Message>) {
        let open_item = MenuItem::new("Open Dashboard", true, None);
        let quit_item = MenuItem::new("Quit AuditReady", true, None);
        let open_item_id = open_item.id().clone();
        let quit_item_id = quit_item.id().clone();

        let menu = Menu::new();
        let _ = menu.append(&open_item);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&quit_item);

        let tray = TrayIconBuilder::new()
            .with_tooltip("AuditReady — running, connected")
            .with_icon(tray_icon_image())
            .with_menu(Box::new(menu))
            // Left-click opens the dashboard directly (handled as a
            // TrayIconEvent below) instead of showing the menu; right-click
            // still shows the menu (Open Dashboard / Quit). Linux ignores
            // this and always shows the menu on click, but Linux never
            // reaches this module (see the file-level doc comment).
            .with_menu_on_left_click(false)
            .build()
            .map_err(|e| tracing::warn!("failed to create tray icon: {}", e))
            .ok();

        let state = State {
            stats,
            _tray: tray,
            open_item_id,
            quit_item_id,
            dashboard: None,
            snapshot: ClientStats::default(),
        };
        (state, Task::none())
    }

    fn update(state: &mut State, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                state.snapshot = stats::snapshot(&state.stats);
                if let Some(tray) = &state._tray {
                    let tooltip = if state.snapshot.connected {
                        "AuditReady — running, connected".to_string()
                    } else {
                        "AuditReady — running, connection lost".to_string()
                    };
                    let _ = tray.set_tooltip(Some(tooltip));
                }

                if let Ok(event) = MenuEvent::receiver().try_recv() {
                    if event.id == state.open_item_id {
                        return Task::done(Message::OpenDashboard);
                    } else if event.id == state.quit_item_id {
                        return Task::done(Message::Quit);
                    }
                }
                if let Ok(TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }) = TrayIconEvent::receiver().try_recv()
                {
                    return Task::done(Message::OpenDashboard);
                }
                Task::none()
            }
            Message::OpenDashboard => {
                if state.dashboard.is_some() {
                    return Task::none();
                }
                let (id, open) = window::open(window::Settings {
                    size: iced::Size::new(420.0, 520.0),
                    resizable: true,
                    ..window::Settings::default()
                });
                state.dashboard = Some(id);
                open.map(|_| Message::WindowOpened)
            }
            Message::WindowOpened => Task::none(),
            Message::WindowClosed(id) => {
                if state.dashboard == Some(id) {
                    state.dashboard = None;
                }
                Task::none()
            }
            Message::Quit => iced::exit(),
        }
    }

    fn subscription(_state: &State) -> Subscription<Message> {
        Subscription::batch(vec![
            iced::time::every(Duration::from_millis(500)).map(|_| Message::Tick),
            window::close_events().map(Message::WindowClosed),
        ])
    }

    fn view(state: &State, _id: window::Id) -> Element<'_, Message> {
        let s = &state.snapshot;

        let status_line = if s.connected {
            text("● Connected").size(18)
        } else {
            text("● Disconnected").size(18)
        };

        let last_report = s
            .last_report_at
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "never".to_string());
        let next_report = s
            .next_report_at
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "—".to_string());

        let mut content = column![
            text("AuditReady Client").size(22),
            status_line,
            text(format!("Last report: {}", last_report)),
            text(format!("Next report: {}", next_report)),
        ]
        .spacing(6);

        if let Some(err) = &s.last_error {
            content = content.push(text(format!("Last error: {}", err)));
        }

        content = content
            .push(text(" "))
            .push(text("Activity (since start)").size(16))
            .push(
                row![
                    stat_tile("Clipboard events", s.clipboard_events),
                    stat_tile("Mouse events", s.mouse_events),
                ]
                .spacing(12),
            )
            .push(
                row![
                    stat_tile("Files scanned", s.files_scanned),
                    stat_tile("Sensitive hits", s.sensitive_hits),
                ]
                .spacing(12),
            )
            .push(text(" "))
            .push(text("Processes / network (latest)").size(16))
            .push(
                row![
                    stat_tile("Processes", s.total_processes as u64),
                    stat_tile("Flagged", s.flagged_processes as u64),
                    stat_tile("Connections", s.network_connections as u64),
                ]
                .spacing(12),
            );

        container(content)
            .padding(16)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn stat_tile(label: &str, value: u64) -> Element<'static, Message> {
        column![
            text(value.to_string()).size(20),
            text(label.to_string()).size(12)
        ]
        .spacing(2)
        .into()
    }

    /// A small solid-color circle used as the tray icon. Generated in code
    /// so the build doesn't need a bundled asset file.
    fn tray_icon_image() -> tray_icon::Icon {
        const SIZE: u32 = 32;
        let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
        let center = SIZE as f32 / 2.0;
        let radius = center - 2.0;
        for y in 0..SIZE {
            for x in 0..SIZE {
                let dx = x as f32 + 0.5 - center;
                let dy = y as f32 + 0.5 - center;
                let idx = ((y * SIZE + x) * 4) as usize;
                if dx * dx + dy * dy <= radius * radius {
                    // Blue-ish circle, matches nothing in particular — just
                    // needs to read clearly as "this app" in a tray full of
                    // monochrome icons.
                    rgba[idx] = 0x2f;
                    rgba[idx + 1] = 0x80;
                    rgba[idx + 2] = 0xed;
                    rgba[idx + 3] = 0xff;
                }
            }
        }
        tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("valid tray icon buffer")
    }
}
