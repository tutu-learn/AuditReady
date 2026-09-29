//! Ratatui UI for the action client.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::{ActionCard, AppState};

pub fn draw(f: &mut Frame, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
        .split(f.area());

    // Header.
    let header = Paragraph::new("Audit Ready Actions")
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title("Action Client"));
    f.render_widget(header, chunks[0]);

    // Footer / status.
    let help = Line::from(vec![
        Span::styled("↑/↓/k/j", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" navigate  "),
        Span::styled("←/→/h/l", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" buttons  "),
        Span::styled("Enter", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" execute  "),
        Span::styled("r", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" refresh  "),
        Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" quit"),
    ]);
    let status = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled("Status: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&state.status),
        ]),
        help,
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(status, chunks[2]);

    // Action list.
    let list_area = chunks[1];
    if state.cards.is_empty() {
        let empty = Paragraph::new("No pending actions.")
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(empty, list_area);
        return;
    }

    let card_height = 12u16;
    let visible_count = (list_area.height / card_height).max(1) as usize;
    let scroll = state
        .selected_card
        .saturating_sub(visible_count / 2)
        .min(state.cards.len().saturating_sub(visible_count));

    let cards_rect = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Length(card_height); visible_count])
        .split(list_area.inner(Margin {
            horizontal: 1,
            vertical: 1,
        }));

    for (idx, card_rect) in cards_rect.iter().enumerate() {
        let card_idx = scroll + idx;
        let Some(card) = state.cards.get(card_idx) else {
            break;
        };
        let is_selected = card_idx == state.selected_card;
        draw_card(f, card, *card_rect, is_selected, state.selected_button);
    }
}

fn draw_card(
    f: &mut Frame,
    card: &ActionCard,
    area: Rect,
    is_selected: bool,
    selected_button: usize,
) {
    let border_style = if is_selected {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Gray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(format!(
            " {} [{}] ",
            card.action.title, card.action.action_type
        ))
        .title_style(Style::default().add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    let payload = serde_json::to_string(&card.action.payload).unwrap_or_else(|_| "{}".into());
    let payload_short = if payload.len() > 80 {
        format!("{}...", &payload[..80])
    } else {
        payload
    };

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Status: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&card.action.status),
        ])),
        layout[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Payload: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(payload_short),
        ])),
        layout[1],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Assigned: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(if card.action.assigned_user.is_empty() {
                "-"
            } else {
                &card.action.assigned_user
            }),
        ])),
        layout[2],
    );

    // Buttons.
    let mut spans: Vec<Span> = vec![Span::styled("Actions: ", Style::default().add_modifier(Modifier::BOLD))];
    for (idx, button) in card.buttons.iter().enumerate() {
        let style = if is_selected && idx == selected_button {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        spans.push(Span::styled(format!(" [{}] ", button.label), style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), layout[4]);
}
