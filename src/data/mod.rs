//! Data retrieval, grouping, and cache primitives used by the news workflow.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    time::SystemTime,
};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

mod search;
mod store;

// Shared constants live here so the retrieval modules use one set of thresholds and endpoints.
pub(crate) const SEC_TICKER_URL: &str = "https://www.sec.gov/files/company_tickers.json";
pub(crate) const GOOGLE_NEWS_RSS: &str = "https://news.google.com/rss/search";
pub(crate) const BING_NEWS_RSS: &str = "https://www.bing.com/news/search";
pub(crate) const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 10;
pub(crate) const APP_USER_AGENT: &str = "gn/1.0 research@gn.dev";
pub(crate) const ARTICLE_WINDOW_DAYS: u64 = 90;

pub(crate) fn is_symbol_candidate(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.chars().count() <= 8
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-'))
}

pub(crate) fn company_analysis_context_key(ticker: &str) -> String {
    format!("company:{}", ticker.trim().to_ascii_uppercase())
}

pub(crate) fn query_analysis_context_key(query: &str) -> String {
    format!(
        "query:{}",
        query
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase()
    )
}

pub use search::{
    build_company_news_search_query, build_news_articles, search_all_sites,
    search_exact_and_allowlisted_sites,
};
pub(crate) use search::{
    is_google_news_rss_article_url, resolve_embedded_google_news_url, resolve_google_news_url,
    split_news_query_terms,
};
pub use store::{
    DefaultSourceStore, SearchHistoryEntry, SearchHistoryRecord, delete_search_history_term,
    record_search_history_run, search_history_entries,
};

/// Full cached coverage dataset for one company.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CachedNews {
    #[serde(default)]
    pub articles: Vec<NewsArticle>,
    pub source_urls: Vec<String>,
    pub total_tokens: u32,
}

impl CachedNews {
    pub fn article_keys(&self) -> HashSet<String> {
        self.articles
            .iter()
            .map(|article| article.cache_key())
            .collect()
    }
}

/// Source-grounded article row discovered through news RSS.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct NewsArticle {
    pub title: String,
    #[serde(default)]
    pub label: String,
    pub publisher: String,
    pub url: String,
    pub published_at: Option<SystemTime>,
    pub source_rank: u32,
}

impl NewsArticle {
    pub fn cache_identity(&self) -> ArticleCacheIdentity {
        article_cache_identity(&self.publisher, &self.title, &self.url, self.published_at)
    }

    pub fn cache_key(&self) -> String {
        self.cache_identity().article_key
    }
}

/// Display order for news rows: newest first, then allowlisted items, then stable text order.
pub fn compare_news_articles(left: &NewsArticle, right: &NewsArticle) -> Ordering {
    match (left.published_at, right.published_at) {
        (Some(left_ts), Some(right_ts)) if left_ts != right_ts => right_ts.cmp(&left_ts),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        _ => Ordering::Equal,
    }
    .then_with(|| right.source_rank.cmp(&left.source_rank))
    .then_with(|| left.publisher.cmp(&right.publisher))
    .then_with(|| left.title.cmp(&right.title))
}

/// RSS/news result before article-body extraction.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SearchResult {
    pub source_name: String,
    pub article_title: String,
    pub url: String,
    pub published_at: Option<SystemTime>,
}

/// Stable cache identity for one article returned by the current RSS search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArticleCacheIdentity {
    pub article_key: String,
    pub normalized_url: String,
    pub publisher: String,
    pub title: String,
    pub published_at: Option<SystemTime>,
}

/// Reusable model output for one article under one company or exact-query context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CachedArticleAnalysis {
    pub relevant: Option<bool>,
    pub label: Option<String>,
}

/// Partial cache update produced by one workflow stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArticleAnalysisUpdate {
    pub article: ArticleCacheIdentity,
    pub relevant: Option<bool>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RssChannel {
    #[serde(rename = "channel")]
    pub(crate) channel: RssItems,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RssItems {
    #[serde(rename = "item", default)]
    pub(crate) items: Vec<RssItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RssItem {
    #[serde(rename = "title", default)]
    pub(crate) title: String,
    #[serde(rename = "link", default)]
    pub(crate) link: String,
    // Bing News RSS uses a namespaced `News:Source` element instead of `source`.
    #[serde(rename = "source", alias = "News:Source", alias = "Source", default)]
    pub(crate) source: String,
    #[serde(rename = "pubDate", default)]
    pub(crate) pub_date: String,
}

/// Resolved company identity used by the news workflow.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CompanyIdentity {
    pub ticker: String,
    pub company_name: String,
}

/// Data source abstraction used by the workflow engine.
#[async_trait]
pub trait SourceStore: Send + Sync {
    async fn resolve_company_identity(&self, ticker: &str) -> Result<CompanyIdentity>;
    async fn search_company_news(
        &self,
        query: &str,
        sources: &[crate::config::AllowlistEntry],
    ) -> Result<Vec<SearchResult>> {
        Ok(search_all_sites(query, sources).await)
    }
    async fn search_exact_news(
        &self,
        query: &str,
        sources: &[crate::config::AllowlistEntry],
    ) -> Result<Vec<SearchResult>> {
        Ok(search_exact_and_allowlisted_sites(query, sources).await)
    }
    async fn load_cached_news(&self, ticker: &str) -> Result<Option<CachedNews>>;
    async fn store_cached_news(&self, ticker: &str, cached: CachedNews) -> Result<()>;
    async fn load_article_analysis(
        &self,
        _context_key: &str,
        _articles: &[ArticleCacheIdentity],
    ) -> Result<HashMap<String, CachedArticleAnalysis>> {
        Ok(HashMap::new())
    }
    async fn store_article_analysis(
        &self,
        _context_key: &str,
        _updates: &[ArticleAnalysisUpdate],
    ) -> Result<()> {
        Ok(())
    }
}

pub fn article_cache_identity(
    publisher: &str,
    title: &str,
    url: &str,
    published_at: Option<SystemTime>,
) -> ArticleCacheIdentity {
    let normalized_url = normalize_url(url);
    let article_key = if normalized_url.is_empty() || is_google_news_rss_article_url(url) {
        format!(
            "title:{}|{}",
            normalize_cache_text(publisher),
            normalize_cache_text(title)
        )
    } else {
        format!("url:{normalized_url}")
    };
    ArticleCacheIdentity {
        article_key,
        normalized_url,
        publisher: publisher.trim().to_string(),
        title: title.trim().to_string(),
        published_at,
    }
}

fn normalize_cache_text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

pub fn normalize_url(raw: &str) -> String {
    // Normalize aggressively for dedupe: protocol, `www`, query strings, and trailing slashes do
    // not matter for gn's cache and source matching.
    let lowered = raw
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.")
        .to_lowercase();
    lowered
        .split('?')
        .next()
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string()
}

