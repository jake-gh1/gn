//! Shared UI constants and state used across rendering and input code.

use std::{path::Path, sync::Arc};

use ratatui::style::Color;

pub(crate) const BOTTOM_BLOCK_ROWS: usize = 1;
pub(crate) const LEFT_PAD: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UiPalette {
    pub(crate) primary_text: Color,
    pub(crate) badge_gray: Color,
    pub(crate) dim: Color,
}

impl UiPalette {
    pub(crate) fn standard() -> Self {
        Self {
            primary_text: Color::Reset,
            badge_gray: Color::Rgb(142, 142, 142),
            dim: Color::Rgb(102, 102, 102),
        }
    }
}

pub(crate) type BrowserOpener = Arc<dyn Fn(&str) -> std::result::Result<(), String> + Send + Sync>;
pub(crate) type ConfigEditor = Arc<dyn Fn(&Path) -> std::result::Result<(), String> + Send + Sync>;
