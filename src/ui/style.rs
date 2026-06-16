//! Ratatui styling helpers for the news table and footer.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::ui::*;

const FOOTER_FIELD_SEPARATOR: &str = " · ";
const BREATH_PERIOD_MS: u128 = 2_080;
const BREATH_LOW_COLOR: Color = Color::Rgb(126, 126, 126);
const BREATH_HIGH_COLOR: Color = Color::Rgb(206, 206, 206);
const BREATH_MIN_AMOUNT: f32 = 0.18;
const BREATH_MAX_AMOUNT: f32 = 0.82;

impl AppModel {
    pub(crate) fn style_footer_line(&self, line: String) -> Line<'static> {
        let (pad, rest) = split_rendered_left_pad(&line);
        let mut spans = vec![Span::raw(pad.to_string())];

        if let Some(after_prefix) = rest.strip_prefix("gn  ") {
            spans.push(self.style_footer_brand("gn"));
            spans.push(Span::raw("  ".to_string()));
            if let Some((context, fields)) = after_prefix.split_once(FOOTER_FIELD_SEPARATOR) {
                spans.push(Span::styled(
                    context.to_string(),
                    Style::default().fg(self.palette.badge_gray),
                ));
                spans.push(Span::raw(FOOTER_FIELD_SEPARATOR.to_string()));
                self.push_footer_fields(&mut spans, fields);
            } else {
                spans.push(Span::styled(
                    after_prefix.to_string(),
                    Style::default().fg(self.palette.badge_gray),
                ));
            }
            return Line::from(spans);
        }

        if let Some(fields) = rest.strip_prefix("gn · ") {
            spans.push(self.style_footer_brand("gn"));
            spans.push(Span::raw(FOOTER_FIELD_SEPARATOR.to_string()));
            self.push_footer_fields(&mut spans, fields);
            return Line::from(spans);
        }

        spans.push(Span::styled(
            rest.to_string(),
            Style::default().fg(self.palette.badge_gray),
        ));
        Line::from(spans)
    }

    fn style_footer_brand(&self, value: &str) -> Span<'static> {
        Span::styled(
            value.to_string(),
            Style::default()
                .fg(self.palette.badge_gray)
                .add_modifier(Modifier::BOLD),
        )
    }

    fn push_footer_fields(&self, spans: &mut Vec<Span<'static>>, fields: &str) {
        let parts = fields.split(FOOTER_FIELD_SEPARATOR).collect::<Vec<_>>();
        let progress_idx = self
            .progress_note
            .as_deref()
            .filter(|note| !note.trim().is_empty())
            .and_then(|_| parts.len().checked_sub(1));
        for (idx, part) in parts.iter().enumerate() {
            if idx > 0 {
                spans.push(Span::raw(FOOTER_FIELD_SEPARATOR.to_string()));
            }
            if Some(idx) == progress_idx {
                self.push_breathing_footer_field(spans, part);
                continue;
            }
            spans.push(Span::styled(
                (*part).to_string(),
                Style::default().fg(self.palette.badge_gray),
            ));
        }
    }

    fn push_breathing_footer_field(&self, spans: &mut Vec<Span<'static>>, value: &str) {
        if value.is_empty() {
            return;
        }
        let elapsed_ms = self.started_at.elapsed().as_millis();
        spans.push(Span::styled(
            value.to_string(),
            Style::default().fg(breathing_color(elapsed_ms)),
        ));
    }

    pub(crate) fn style_news_article_table_line(
        &self,
        line: &str,
        selected_news_line: Option<&str>,
    ) -> Option<Line<'static>> {
        if !self.should_render_news_table() {
            return None;
        }

        if self.is_news_article_table_header(line) {
            return Some(Line::from(Span::styled(
                line.to_string(),
                Style::default()
                    .fg(self.palette.badge_gray)
                    .add_modifier(Modifier::BOLD),
            )));
        }

        let is_active = self.story_menu_focused && selected_news_line == Some(line);
        let style = if is_active {
            Style::default()
                .fg(self.palette.primary_text)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.palette.dim)
        };
        if let Some(rest) = line.strip_prefix("•") {
            return Some(Line::from(vec![
                Span::styled("•".to_string(), Style::default().fg(Color::White)),
                Span::styled(rest.to_string(), style),
            ]));
        }
        Some(Line::from(Span::styled(line.to_string(), style)))
    }

    pub(crate) fn style_conversation_line(
        &self,
        line: String,
        selected_news_line: Option<&str>,
    ) -> Line<'static> {
        if let Some(styled) = self.style_news_article_table_line(&line, selected_news_line) {
            return styled;
        }

        Line::from(Span::raw(line))
    }
}

fn split_rendered_left_pad(line: &str) -> (&str, &str) {
    let split_at = LEFT_PAD.min(line.len());
    line.split_at(split_at)
}

fn breathing_color(elapsed_ms: u128) -> Color {
    let phase = (elapsed_ms % BREATH_PERIOD_MS) as f32 / BREATH_PERIOD_MS as f32;
    let intensity = 0.5 - 0.5 * (phase * std::f32::consts::TAU).cos();
    let amount = BREATH_MIN_AMOUNT + (BREATH_MAX_AMOUNT - BREATH_MIN_AMOUNT) * intensity;
    interpolate_color(BREATH_LOW_COLOR, BREATH_HIGH_COLOR, amount)
}

fn interpolate_color(low: Color, high: Color, amount: f32) -> Color {
    let Color::Rgb(low_r, low_g, low_b) = low else {
        return low;
    };
    let Color::Rgb(high_r, high_g, high_b) = high else {
        return high;
    };
    Color::Rgb(
        interpolate_channel(low_r, high_r, amount),
        interpolate_channel(low_g, high_g, amount),
        interpolate_channel(low_b, high_b, amount),
    )
}

fn interpolate_channel(low: u8, high: u8, amount: f32) -> u8 {
    let value = low as f32 + (high as f32 - low as f32) * amount.clamp(0.0, 1.0);
    value.round() as u8
}
