use std::{collections::HashSet, time::Duration};

use crate::data::NewsArticle;
use crate::ui::*;

type NewsArticleTableLayout = (usize, usize, usize, usize, usize);

const MAX_NEWS_PUBLISHER_WIDTH: usize = 18;
const MAX_NEWS_LABEL_WIDTH: usize = 25;
const MIN_NEWS_LABEL_WIDTH: usize = "Label".len();
const MIN_NEWS_TITLE_WIDTH: usize = 28;
// How long the head, then the tail, of an overflowing title stays on screen.
const TITLE_TOGGLE_PERIOD: Duration = Duration::from_millis(3000);

struct NewsArticleTableState {
    layout: NewsArticleTableLayout,
    rows: Vec<usize>,
}

pub(crate) struct NewsArticleTableLines {
    pub(crate) header: String,
    pub(crate) body: Vec<String>,
}

impl NewsArticleTableLines {
    pub(crate) fn into_lines(self) -> Vec<String> {
        std::iter::once(self.header).chain(self.body).collect()
    }
}

impl AppModel {
    pub(crate) fn should_render_news_table(&self) -> bool {
        self.active_news_articles().is_some()
    }

    pub(crate) fn active_news_articles(&self) -> Option<&[NewsArticle]> {
        if let Some(articles) = &self.live_articles {
            return Some(articles.as_slice());
        }
        let ticker = self.company_tickers.first()?.to_ascii_uppercase();
        let cached = self.cached_news.get(&ticker)?;
        (!cached.articles.is_empty()).then_some(cached.articles.as_slice())
    }

    pub(crate) fn news_article_rows(&self) -> Vec<usize> {
        self.active_news_articles()
            .map(news_article_rows_for)
            .unwrap_or_default()
    }

    pub(crate) fn news_article_row_count(&self) -> usize {
        self.news_article_rows().len()
    }

    pub(crate) fn selected_news_article_url(&self, row_idx: usize) -> Option<String> {
        let articles = self.active_news_articles()?;
        let article = articles.get(*self.news_article_rows().get(row_idx)?)?;
        (!article.url.trim().is_empty()).then(|| article.url.clone())
    }

    pub(crate) fn news_article_table_lines(&self) -> NewsArticleTableLines {
        let Some(articles) = self.active_news_articles() else {
            return NewsArticleTableLines {
                header: self.news_article_table_header_line(),
                body: Vec::new(),
            };
        };
        let state = self.news_article_table_state(articles);
        let body = (0..state.rows.len())
            .filter_map(|row_idx| self.render_news_article_row(articles, &state, row_idx))
            .collect();
        NewsArticleTableLines {
            header: news_article_table_header_line_with_layout(state.layout),
            body,
        }
    }

    pub(crate) fn news_article_table_header_line(&self) -> String {
        news_article_table_header_line_with_layout(self.news_article_table_layout())
    }

    pub(crate) fn news_article_table_row_line_with_layout(
        &self,
        layout: NewsArticleTableLayout,
        has_marker: bool,
        date: String,
        publisher: &str,
        label: &str,
        title: &str,
        title_scroll_elapsed: Option<Duration>,
    ) -> String {
        let (_, date_width, publisher_width, label_width, title_width) = layout;
        let row_marker = if has_marker { "•  " } else { "   " };
        let date = truncate_with_ellipsis(&date, date_width);
        let publisher = truncate_with_ellipsis(publisher, publisher_width);
        let label = truncate_with_ellipsis(label, label_width);
        let title = title_scroll_elapsed
            .map(|elapsed| scrolling_title_window(title, title_width, elapsed))
            .unwrap_or_else(|| truncate_with_ellipsis(title, title_width));
        format!(
            "{row_marker}{date:date_width$}  {publisher:publisher_width$}  {label:label_width$}  {title:title_width$}"
        )
    }

    pub(crate) fn news_article_table_line_for_index(&self, row_idx: usize) -> Option<String> {
        let articles = self.active_news_articles()?;
        let state = self.news_article_table_state(articles);
        self.render_news_article_row(articles, &state, row_idx)
    }

    fn render_news_article_row(
        &self,
        articles: &[NewsArticle],
        state: &NewsArticleTableState,
        row_idx: usize,
    ) -> Option<String> {
        let article = articles.get(*state.rows.get(row_idx)?)?;
        Some(self.news_article_table_row_line_with_layout(
            state.layout,
            self.should_mark_news_article_row(row_idx),
            self.format_news_article_age(article.published_at),
            &article.publisher,
            article.label.trim(),
            &article.title,
            self.news_article_title_scroll_elapsed(row_idx),
        ))
    }

    pub(crate) fn news_article_table_layout(&self) -> NewsArticleTableLayout {
        let Some(articles) = self.active_news_articles() else {
            return news_article_table_layout_for(self.width, &[], &[], |_| String::new());
        };
        self.news_article_table_state(articles).layout
    }

    fn news_article_table_state(&self, articles: &[NewsArticle]) -> NewsArticleTableState {
        let rows = news_article_rows_for(articles);
        let layout = news_article_table_layout_for(self.width, articles, &rows, |published_at| {
            self.format_news_article_age(published_at)
        });
        NewsArticleTableState { layout, rows }
    }

    pub(crate) fn is_news_article_table_header(&self, line: &str) -> bool {
        line == self.news_article_table_header_line()
    }

    pub(crate) fn format_news_article_age(
        &self,
        published_at: Option<std::time::SystemTime>,
    ) -> String {
        let Some(published_at) = published_at else {
            return String::new();
        };
        let age = std::time::SystemTime::now()
            .duration_since(published_at)
            .unwrap_or_default();
        let minutes = age.as_secs() / 60;
        if minutes < 60 {
            return format!("{}m ago", minutes.max(1));
        }
        let hours = age.as_secs() / 3_600;
        if hours < 24 {
            return format!("{hours}hrs ago");
        }
        let days = age.as_secs() / 86_400;
        if days < 7 {
            return format!("{days}d ago");
        }
        if days < 35 {
            return format!("{}w ago", days / 7);
        }
        if days < 365 {
            return format!("{}mo ago", (days / 30).max(1));
        }
        format!("{}y ago", days / 365)
    }

    fn should_mark_news_article_row(&self, row_idx: usize) -> bool {
        if row_idx != 0 {
            return false;
        }
        let Some(ticker) = self.company_tickers.first() else {
            return false;
        };
        self.new_article_keys
            .get(&ticker.to_ascii_uppercase())
            .is_some_and(|keys| !keys.is_empty())
    }

    fn news_article_title_scroll_elapsed(&self, row_idx: usize) -> Option<Duration> {
        (self.workflow_events.is_none()
            && self.live_articles.is_none()
            && self.story_menu_focused
            && self.story_title_scroll_row == Some(row_idx))
        .then(|| {
            self.story_title_scroll_frame_at
                .checked_duration_since(self.story_title_scroll_started_at)
                .unwrap_or_default()
        })
    }
}

// A terminal can't scroll sub-character, so a crawling marquee always reads as
// choppy. Instead, dwell on the head of an overflowing title, then cleanly swap
// to the tail and back — two static frames, no per-letter stepping.
fn scrolling_title_window(title: &str, width: usize, elapsed: Duration) -> String {
    let chars = title.chars().collect::<Vec<_>>();
    if chars.len() <= width {
        return title.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }

    let period_ms = TITLE_TOGGLE_PERIOD.as_millis().max(1);
    let showing_tail = (elapsed.as_millis() / period_ms) % 2 == 1;
    if !showing_tail {
        return truncate_with_ellipsis(title, width);
    }

    let tail: String = chars[chars.len() - (width - 1)..].iter().collect();
    format!("…{tail}")
}

fn news_article_rows_for(articles: &[NewsArticle]) -> Vec<usize> {
    let mut seen = HashSet::<String>::new();
    let mut rows = (0..articles.len())
        .filter(|idx| {
            let article = &articles[*idx];
            seen.insert(news_article_row_dedupe_key(
                &article.publisher,
                &article.title,
            ))
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left_idx, right_idx| {
        crate::data::compare_news_articles(&articles[*left_idx], &articles[*right_idx])
    });
    rows
}

fn news_article_table_header_line_with_layout(layout: NewsArticleTableLayout) -> String {
    let (_, date_width, publisher_width, label_width, title_width) = layout;
    format!(
        "   {:date_width$}  {:publisher_width$}  {:label_width$}  {:title_width$}",
        "Date", "Publisher", "Label", "Title"
    )
}

fn news_article_table_layout_for(
    width: usize,
    articles: &[NewsArticle],
    rows: &[usize],
    format_age: impl Fn(Option<std::time::SystemTime>) -> String,
) -> NewsArticleTableLayout {
    let total_width = width.max(56);
    let mut date_width = "Date".len();
    let mut publisher_width = "Publisher".len();
    let mut label_width = "Label".len();
    for idx in rows {
        let Some(article) = articles.get(*idx) else {
            continue;
        };
        date_width = date_width.max(format_age(article.published_at).chars().count());
        publisher_width = publisher_width.max(article.publisher.chars().count());
        label_width = label_width.max(article.label.trim().chars().count());
    }

    let mut publisher_width = publisher_width.min(MAX_NEWS_PUBLISHER_WIDTH);
    let mut label_width = label_width.clamp(MIN_NEWS_LABEL_WIDTH, MAX_NEWS_LABEL_WIDTH);
    let fixed_non_columns = 3 + date_width + 2 + 2 + 2;
    let available_columns = total_width.saturating_sub(fixed_non_columns).max(1);
    publisher_width = publisher_width.min(available_columns.saturating_sub(2).max(1));
    label_width = label_width.min(
        available_columns
            .saturating_sub(publisher_width)
            .saturating_sub(1)
            .max(MIN_NEWS_LABEL_WIDTH),
    );

    let target_title_width = MIN_NEWS_TITLE_WIDTH.min(
        available_columns
            .saturating_sub(publisher_width + MIN_NEWS_LABEL_WIDTH)
            .max(1),
    );
    let title_width = available_columns
        .saturating_sub(publisher_width + label_width)
        .max(1);
    if title_width < target_title_width {
        let deficit = target_title_width - title_width;
        let label_shrink = deficit.min(label_width.saturating_sub(MIN_NEWS_LABEL_WIDTH));
        label_width -= label_shrink;
        let publisher_shrink =
            (deficit - label_shrink).min(publisher_width.saturating_sub("Publisher".len()));
        publisher_width -= publisher_shrink;
    }

    let title_width = available_columns
        .saturating_sub(publisher_width + label_width)
        .max(1);
    (
        total_width,
        date_width,
        publisher_width,
        label_width,
        title_width,
    )
}

fn news_article_row_dedupe_key(publisher: &str, title: &str) -> String {
    format!(
        "{}:{}",
        publisher
            .to_ascii_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        title
            .to_ascii_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn scrolling_title_window_toggles_head_and_tail_for_overflow() {
        let title = "abcdefghijkl";

        // Short title fits untouched.
        assert_eq!(scrolling_title_window("abc", 6, Duration::ZERO), "abc");
        // Head while in the first period, tail in the second, head again after.
        assert_eq!(scrolling_title_window(title, 6, Duration::ZERO), "abcde…");
        assert_eq!(
            scrolling_title_window(title, 6, TITLE_TOGGLE_PERIOD),
            "…hijkl"
        );
        assert_eq!(
            scrolling_title_window(title, 6, TITLE_TOGGLE_PERIOD * 2),
            "abcde…"
        );
    }
}
