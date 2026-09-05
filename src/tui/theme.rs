#![allow(clippy::missing_docs_in_private_items)] // 10 left to document
//! Colour theme.
//!
//! Ported from nx `packages/nx/src/native/tui/theme.rs`
//! (MIT, (c) 2017-2026 Narwhal Technologies Inc.)
//!
//! Only ANSI-16 named colours are used, so the UI inherits the user's terminal
//! palette. Dark and light differ solely in `secondary_fg`, exactly as upstream.

use ratatui::style::Color;
use std::sync::LazyLock;
use terminal_colorsaurus::{theme_mode, QueryOptions, ThemeMode};

pub struct Theme {
    pub primary_fg: Color,
    pub secondary_fg: Color,
    pub error: Color,
    pub success: Color,
    pub warning: Color,
    pub info: Color,
    #[allow(dead_code)]
    pub info_light: Color,
}

impl Theme {
    fn dark() -> Self {
        Self {
            primary_fg: Color::Reset,
            secondary_fg: Color::Gray,
            error: Color::Red,
            success: Color::Green,
            warning: Color::Yellow,
            info: Color::Cyan,
            info_light: Color::LightCyan,
        }
    }

    fn light() -> Self {
        Self {
            primary_fg: Color::Reset,
            secondary_fg: Color::DarkGray,
            error: Color::Red,
            success: Color::Green,
            warning: Color::Yellow,
            info: Color::Cyan,
            info_light: Color::LightCyan,
        }
    }
}

/// Detected once at startup via an OSC query; defaults to dark on error.
pub static THEME: LazyLock<Theme> = LazyLock::new(|| match theme_mode(QueryOptions::default()) {
    Ok(ThemeMode::Light) => Theme::light(),
    _ => Theme::dark(),
});
