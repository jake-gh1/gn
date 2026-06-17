//! Final viewport assembly and scroll targeting for the terminal UI.

use chrono::{DateTime, Local};
use ratatui::text::Text;
use std::time::SystemTime;

use crate::config::ModelConfig;
use crate::ui::{
    AppModel, BOTTOM_BLOCK_ROWS, LEFT_PAD, center_target_top, format_int_with_commas,
    normalize_view_lines, truncate_footer_text, window_view_lines,
};

impl AppModel {
    pub(crate) fn conversation_lines(&self) -> Vec<String> {
        if self.should_render_news_table() {
            return self.news_article_table_lines().into_lines();
        }

        self.status_message
            .as_ref()
            .map(|message| vec![format!("  {message}")])
            .unwrap_or_default()
    }

    pub(crate) fn render_view_lines(&self) -> Vec<String> {
        // Compose the conversation viewport first, then overlay the bottom region and transient
        // dropdown/help overlays in their reserved area.
        if !self.view_ready {
            return vec![String::new(), "  Loading...".to_string()];
        }

        let viewport_height = self.viewport_height_for_layout();
        let conversation_lines = self.conversation_lines();
        let mut lines =
            if self.should_render_news_table() && conversation_lines.len() <= viewport_height {
                conversation_lines
            } else {
                self.window_conversation_lines(conversation_lines, viewport_height)
            };
        let bottom_lines =
            normalize_view_lines(self.render_bottom_lines(), self.bottom_region_rows());
        lines.extend(bottom_lines);

        lines
    }

    pub(crate) fn window_conversation_lines(
        &self,
        lines: Vec<String>,
        count: usize,
    ) -> Vec<String> {
        if count == 0 {
            return Vec::new();
        }
        if self.should_render_news_table() {
            return self.window_news_table_lines(count);
        }
        let top = lines.len().saturating_sub(count);
        window_view_lines(lines, count, top)
    }

    pub(crate) fn window_news_table_lines(&self, count: usize) -> Vec<String> {
        if count == 0 {
            return Vec::new();
        }

        let table = self.news_article_table_lines();
        if count == 1 {
            return vec![table.header];
        }

        let body_count = count - 1;
        let top = if table.body.is_empty() {
            0
        } else {
            let target = self
                .story_menu_highlight
                .min(table.body.len().saturating_sub(1));
            center_target_top(target, body_count, table.body.len())
        };
        let mut visible = vec![table.header];
        visible.extend(window_view_lines(table.body, body_count, top));
        visible
    }

    pub(crate) fn view_text_styled(&self) -> Text<'static> {
        let lines = self.render_view_lines();
        let bottom_start = lines.len().saturating_sub(BOTTOM_BLOCK_ROWS);
        let selected_news_line = (self.should_render_news_table() && self.story_menu_focused)
            .then(|| self.news_article_table_line_for_index(self.story_menu_highlight))
            .flatten();

        let mut styled = Vec::with_capacity(lines.len());
        for (idx, line) in lines.into_iter().enumerate() {
            if idx >= bottom_start {
                styled.push(self.style_footer_line(line));
                continue;
            }

            styled.push(self.style_conversation_line(line, selected_news_line.as_deref()));
        }

        Text::from(styled)
    }

    pub(crate) fn bottom_region_rows(&self) -> usize {
        BOTTOM_BLOCK_ROWS + 1
    }

    pub(crate) fn viewport_height_for_layout(&self) -> usize {
        self.height.saturating_sub(self.bottom_region_rows()).max(1)
    }

    pub(crate) fn bottom_input_width(&self) -> usize {
        self.width.saturating_sub(LEFT_PAD).max(10)
    }

    pub(crate) fn render_bottom_lines(&self) -> Vec<String> {
        let width = self.bottom_input_width();
        let pad = " ".repeat(LEFT_PAD);

        let mut lines = vec![String::new()];
        lines.push(format!("{pad}{}", self.render_bottom_meta_line(width)));
        lines
    }

    pub(crate) fn render_bottom_meta_line(&self, width: usize) -> String {
        truncate_footer_text(&self.footer_plain_line(), width)
    }

    pub(crate) fn footer_plain_line(&self) -> String {
        let (input_tokens, output_tokens) = self.displayed_token_counts();
        let tokens = if input_tokens > 0 || output_tokens > 0 {
            format!(
                "{} → {}",
                format_int_with_commas(input_tokens),
                format_int_with_commas(output_tokens)
            )
        } else {
            "(none)".to_string()
        };

        let mut line = "gn".to_string();
        if let Some(value) = self.footer_context() {
            line.push_str("  ");
            line.push_str(value);
        }
        line.push_str(&format!(
            " · {} · {} · @{}",
            self.current_model_label(),
            tokens,
            format_run_timestamp(self.run_started_at)
        ));
        if let Some(note) = self
            .progress_note
            .as_deref()
            .filter(|note| !note.trim().is_empty())
        {
            line.push_str(" · ");
            line.push_str(note.trim());
        }
        line
    }

    fn footer_context(&self) -> Option<&str> {
        let company_name = self.company_names.first()?.trim();
        if company_name.is_empty() {
            return None;
        }
        if self
            .company_tickers
            .first()
            .is_some_and(|ticker| ticker.eq_ignore_ascii_case(company_name))
        {
            if is_ticker_symbol_like(company_name) {
                return None;
            }
            return Some(company_name);
        }
        Some(company_name)
    }

    pub fn current_model_label(&self) -> String {
        self.runtime
            .models
            .get(self.active_model)
            .map(strip_provider_label)
            .unwrap_or_else(|| "none".to_string())
    }
}

fn format_run_timestamp(value: SystemTime) -> String {
    let timestamp: DateTime<Local> = value.into();
    timestamp.format("%H:%M:%S").to_string()
}

fn is_ticker_symbol_like(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty()
        && trimmed.chars().count() <= 8
        && trimmed
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || matches!(ch, '.' | '-'))
}

fn strip_provider_label(model: &ModelConfig) -> String {
    model
        .label
        .split('/')
        .next_back()
        .unwrap_or(&model.model_id)
        .to_string()
}
