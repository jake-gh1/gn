use std::collections::HashSet;

use crate::data::NewsArticle;
use crate::ui::*;

type NewsArticleTableLayout = (usize, usize, usize, usize, usize);

const MAX_NEWS_PUBLISHER_WIDTH: usize = 18;
const MAX_NEWS_LABEL_WIDTH: usize = 25;
const MIN_NEWS_LABEL_WIDTH: usize = "Label".len();
const MIN_NEWS_TITLE_WIDTH: usize = 28;
const FRESH_NEWS_ARTICLE_WINDOW: std::time::Duration = std::time::Duration::from_secs(12 * 60 * 60);

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
        is_fresh: bool,
        date: String,
        publisher: &str,
        label: &str,
        title: &str,
    ) -> String {
        let (_, date_width, publisher_width, label_width, title_width) = layout;
        let row_marker = if is_fresh { "•  " } else { "   " };
        let date = truncate_with_ellipsis(&date, date_width);
        let publisher = truncate_with_ellipsis(publisher, publisher_width);
        let label = truncate_with_ellipsis(label, label_width);
        let title = truncate_with_ellipsis(title, title_width);
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
            self.is_fresh_news_article(article),
            self.format_news_article_age(article.published_at),
            &article.publisher,
            article.label.trim(),
            &article.title,
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

    fn is_fresh_news_article(&self, article: &NewsArticle) -> bool {
        let within_window = article.published_at.is_some_and(|published_at| {
            std::time::SystemTime::now()
                .duration_since(published_at)
                .unwrap_or_default()
                < FRESH_NEWS_ARTICLE_WINDOW
        });
        if !within_window {
            return false;
        }
        let Some(ticker) = self.company_tickers.first() else {
            return false;
        };
        self.new_article_keys
            .get(&ticker.to_ascii_uppercase())
            .is_some_and(|keys| keys.contains(&article.cache_key()))
    }
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
