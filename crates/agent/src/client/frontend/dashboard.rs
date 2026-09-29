//! iced-based stats dashboard window.

use iced::widget::{column, container, row, text};
use iced::window;
use iced::{Element, Length, Subscription, Task};
use std::time::Duration;

use crate::client::frontend::{theme, tray};
use crate::client::stats::{self, ClientStats, SharedStats};

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    OpenDashboard,
    Quit,
    WindowOpened,
    WindowClosed(window::Id),
}

pub struct State {
    stats: SharedStats,
    tray: tray::Handles,
    dashboard: Option<window::Id>,
    snapshot: ClientStats,
}

/// Blocks the calling thread until the user quits from the tray menu.
pub fn run(stats: SharedStats) -> iced::Result {
    iced::daemon(move || boot(stats.clone()), update, view)
        .subscription(subscription)
        .title(|_state: &State, _id| "AuditReady — Client Dashboard".to_string())
        .theme(|_state: &State, _id| theme::sebrus_theme())
        .run()
}

fn boot(stats: SharedStats) -> (State, Task<Message>) {
    let tray_handles = tray::build("AuditReady — running, connected");

    let state = State {
        stats,
        tray: tray_handles,
        dashboard: None,
        snapshot: ClientStats::default(),
    };
    (state, Task::none())
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            state.snapshot = stats::snapshot(&state.stats);
            if let Some(tray) = &state.tray.tray {
                let tooltip = if state.snapshot.connected {
                    "AuditReady — running, connected".to_string()
                } else {
                    "AuditReady — running, connection lost".to_string()
                };
                tray::set_tooltip(tray, &tooltip);
            }

            if let Some(action) = tray::poll(&state.tray.open_item_id, &state.tray.quit_item_id) {
                match action {
                    tray::Action::OpenDashboard => return Task::done(Message::OpenDashboard),
                    tray::Action::Quit => return Task::done(Message::Quit),
                }
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
            text("AUDITREADY").size(18).font(theme::MONO).color(theme::CYAN),
            text("::").size(18).font(theme::MONO).color(theme::DIM),
            text("CLIENT").size(18).font(theme::MONO).color(theme::TEXT),
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
                .font(theme::MONO)
                .color(theme::RED),
        );
    }

    content = content
        .push(section("Activity — since start"))
        .push(
            row![
                stat_tile("Clipboard events", s.clipboard_events, theme::CYAN),
                stat_tile("Mouse events", s.mouse_events, theme::CYAN),
            ]
            .spacing(10),
        )
        .push(
            row![
                stat_tile("Files scanned", s.files_scanned, theme::CYAN),
                stat_tile(
                    "Sensitive hits",
                    s.sensitive_hits,
                    if s.sensitive_hits > 0 { theme::RED } else { theme::CYAN },
                ),
            ]
            .spacing(10),
        )
        .push(section("Processes / network — latest"))
        .push(
            row![
                stat_tile("Processes", s.total_processes as u64, theme::CYAN),
                stat_tile(
                    "Flagged",
                    s.flagged_processes as u64,
                    if s.flagged_processes > 0 { theme::RED } else { theme::CYAN },
                ),
                stat_tile("Connections", s.network_connections as u64, theme::CYAN),
            ]
            .spacing(10),
        );

    container(content)
        .style(theme::root_style)
        .padding(18)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Connection status rendered like the web dashboard's status chips.
fn status_chip(connected: bool) -> Element<'static, Message> {
    let (label, color) = if connected {
        ("● CONNECTED", theme::GREEN)
    } else {
        ("● DISCONNECTED", theme::RED)
    };
    container(text(label).size(11).font(theme::MONO).color(color))
        .style(theme::chip_style(color))
        .padding([6, 10])
        .into()
}

fn meta_row(label: &'static str, value: String) -> Element<'static, Message> {
    row![
        text(label)
            .size(11)
            .font(theme::MONO)
            .color(theme::DIM)
            .width(Length::Fixed(110.0)),
        text(value).size(11).font(theme::MONO).color(theme::TEXT),
    ]
    .into()
}

fn section(title: &str) -> Element<'static, Message> {
    text(title.to_uppercase())
        .size(11)
        .font(theme::MONO)
        .color(theme::DIM)
        .into()
}

fn stat_tile(label: &str, value: u64, accent: iced::Color) -> Element<'static, Message> {
    container(
        column![
            text(value.to_string()).size(22).font(theme::MONO).color(accent),
            text(label.to_uppercase()).size(10).font(theme::MONO).color(theme::DIM),
        ]
        .spacing(4),
    )
    .style(theme::tile_style)
    .padding([10, 12])
    .width(Length::Fill)
    .into()
}
