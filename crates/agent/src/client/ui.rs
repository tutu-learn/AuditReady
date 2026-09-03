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
    use iced::{Background, Border, Color, Element, Font, Length, Subscription, Task, Theme};

    // SEBRUS::OPS ops-console palette (matches the web dashboard).
    const BG: Color = Color::from_rgb8(0x03, 0x06, 0x0c);
    const PANEL: Color = Color::from_rgb8(0x0a, 0x0f, 0x1c);
    const BORDER: Color = Color::from_rgb8(0x1a, 0x24, 0x38);
    const TEXT: Color = Color::from_rgb8(0xe8, 0xef, 0xff);
    const DIM: Color = Color::from_rgb8(0x5f, 0x6e, 0x8c);
    const CYAN: Color = Color::from_rgb8(0x3f, 0xe0, 0xff);
    const GREEN: Color = Color::from_rgb8(0x4f, 0xf3, 0x9a);
    const RED: Color = Color::from_rgb8(0xff, 0x5f, 0x7a);
    const AMBER: Color = Color::from_rgb8(0xff, 0xb5, 0x47);
    const MONO: Font = Font::MONOSPACE;

    fn sebrus_theme() -> Theme {
        Theme::custom(
            "SebrusOps".to_string(),
            iced::theme::Palette {
                background: BG,
                text: TEXT,
                primary: CYAN,
                success: GREEN,
                warning: AMBER,
                danger: RED,
            },
        )
    }

    fn root_style(_theme: &Theme) -> container::Style {
        container::Style {
            background: Some(Background::Color(BG)),
            text_color: Some(TEXT),
            ..container::Style::default()
        }
    }

    fn tile_style(_theme: &Theme) -> container::Style {
        container::Style {
            background: Some(Background::Color(PANEL)),
            border: Border {
                color: BORDER,
                width: 1.0,
                radius: 3.0.into(),
            },
            ..container::Style::default()
        }
    }

    fn chip_style(color: Color) -> impl Fn(&Theme) -> container::Style {
        move |_| container::Style {
            background: Some(Background::Color(PANEL)),
            border: Border {
                color,
                width: 1.0,
                radius: 3.0.into(),
            },
            ..container::Style::default()
        }
    }
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
            .theme(|_state: &State, _id| sebrus_theme())
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

        let last_report = s
            .last_report_at
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "never".to_string());
        let next_report = s
            .next_report_at
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "—".to_string());

        let mut content = column![
            row![
                text("AUDITREADY").size(18).font(MONO).color(CYAN),
                text("::").size(18).font(MONO).color(DIM),
                text("CLIENT").size(18).font(MONO).color(TEXT),
            ],
            status_chip(s.connected),
            meta_row("LAST REPORT", last_report),
            meta_row("NEXT REPORT", next_report),
        ]
        .spacing(10);

        if let Some(err) = &s.last_error {
            content = content.push(
                text(format!("LAST ERROR  {}", err))
                    .size(11)
                    .font(MONO)
                    .color(RED),
            );
        }

        content = content
            .push(section("Activity — since start"))
            .push(
                row![
                    stat_tile("Clipboard events", s.clipboard_events, CYAN),
                    stat_tile("Mouse events", s.mouse_events, CYAN),
                ]
                .spacing(10),
            )
            .push(
                row![
                    stat_tile("Files scanned", s.files_scanned, CYAN),
                    stat_tile(
                        "Sensitive hits",
                        s.sensitive_hits,
                        if s.sensitive_hits > 0 { RED } else { CYAN },
                    ),
                ]
                .spacing(10),
            )
            .push(section("Processes / network — latest"))
            .push(
                row![
                    stat_tile("Processes", s.total_processes as u64, CYAN),
                    stat_tile(
                        "Flagged",
                        s.flagged_processes as u64,
                        if s.flagged_processes > 0 { RED } else { CYAN },
                    ),
                    stat_tile("Connections", s.network_connections as u64, CYAN),
                ]
                .spacing(10),
            );

        container(content)
            .style(root_style)
            .padding(18)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// Connection status rendered like the web dashboard's status chips.
    fn status_chip(connected: bool) -> Element<'static, Message> {
        let (label, color) = if connected {
            ("● CONNECTED", GREEN)
        } else {
            ("● DISCONNECTED", RED)
        };
        container(text(label).size(11).font(MONO).color(color))
            .style(chip_style(color))
            .padding([6, 10])
            .into()
    }

    fn meta_row(label: &'static str, value: String) -> Element<'static, Message> {
        row![
            text(label)
                .size(11)
                .font(MONO)
                .color(DIM)
                .width(Length::Fixed(110.0)),
            text(value).size(11).font(MONO).color(TEXT),
        ]
        .into()
    }

    fn section(title: &str) -> Element<'static, Message> {
        text(title.to_uppercase())
            .size(11)
            .font(MONO)
            .color(DIM)
            .into()
    }

    fn stat_tile(label: &str, value: u64, accent: Color) -> Element<'static, Message> {
        container(
            column![
                text(value.to_string()).size(22).font(MONO).color(accent),
                text(label.to_uppercase()).size(10).font(MONO).color(DIM),
            ]
            .spacing(4),
        )
        .style(tile_style)
        .padding([10, 12])
        .width(Length::Fill)
        .into()
    }

    /// A blocky "S" (SEBRUS::OPS) in the theme cyan, used as the tray icon.
    /// Rasterized from a 5x7 pixel-font bitmap so the build doesn't need a
    /// bundled asset file.
    fn tray_icon_image() -> tray_icon::Icon {
        const SIZE: u32 = 32;
        const SCALE: u32 = 4;
        const GLYPH: [&str; 7] = [
            ".###.",
            "#...#",
            "#....",
            ".###.",
            "....#",
            "#...#",
            ".###.",
        ];
        let offset_x = (SIZE - 5 * SCALE) / 2;
        let offset_y = (SIZE - 7 * SCALE) / 2;
        let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
        for (gy, row) in GLYPH.iter().enumerate() {
            for (gx, ch) in row.chars().enumerate() {
                if ch != '#' {
                    continue;
                }
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        let x = offset_x + gx as u32 * SCALE + dx;
                        let y = offset_y + gy as u32 * SCALE + dy;
                        let idx = ((y * SIZE + x) * 4) as usize;
                        rgba[idx] = 0x3f;
                        rgba[idx + 1] = 0xe0;
                        rgba[idx + 2] = 0xff;
                        rgba[idx + 3] = 0xff;
                    }
                }
            }
        }
        tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("valid tray icon buffer")
    }
}
