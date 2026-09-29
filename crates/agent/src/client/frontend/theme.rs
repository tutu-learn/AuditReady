//! SEBRUS::OPS ops-console theme (matches the web dashboard).

use iced::widget::container;
use iced::{theme::Palette, Background, Border, Color, Font, Theme};

pub const BG: Color = Color::from_rgb8(0x03, 0x06, 0x0c);
pub const PANEL: Color = Color::from_rgb8(0x0a, 0x0f, 0x1c);
pub const BORDER: Color = Color::from_rgb8(0x1a, 0x24, 0x38);
pub const TEXT: Color = Color::from_rgb8(0xe8, 0xef, 0xff);
pub const DIM: Color = Color::from_rgb8(0x5f, 0x6e, 0x8c);
pub const CYAN: Color = Color::from_rgb8(0x3f, 0xe0, 0xff);
pub const GREEN: Color = Color::from_rgb8(0x4f, 0xf3, 0x9a);
pub const RED: Color = Color::from_rgb8(0xff, 0x5f, 0x7a);
pub const AMBER: Color = Color::from_rgb8(0xff, 0xb5, 0x47);
pub const MONO: Font = Font::MONOSPACE;

pub fn sebrus_theme() -> Theme {
    Theme::custom(
        "SebrusOps".to_string(),
        Palette {
            background: BG,
            text: TEXT,
            primary: CYAN,
            success: GREEN,
            warning: AMBER,
            danger: RED,
        },
    )
}

pub fn root_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG)),
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

pub fn tile_style(_theme: &Theme) -> container::Style {
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

pub fn chip_style(color: Color) -> impl Fn(&Theme) -> container::Style {
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
