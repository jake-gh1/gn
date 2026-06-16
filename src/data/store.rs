//! Data-store implementation for company identity resolution and news cache management.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::config::debug_log;
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::data::{
    APP_USER_AGENT, ArticleAnalysisUpdate, ArticleCacheIdentity, CachedArticleAnalysis, CachedNews,
    CompanyIdentity, SEC_TICKER_URL, SourceStore,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHistoryEntry {
    pub search_term: String,
    pub display_term: String,
    pub searched_at: SystemTime,
    pub articles_found: usize,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHistoryRecord {
    pub search_term: String,
    pub display_term: String,
    pub context_key: String,
    pub articles_found: usize,
}

/// Default source store used by the news workflow. It hides SEC company-name lookups and local
/// cached news state behind one async interface.
pub struct DefaultSourceStore {
    sec_http: Client,
    news_cache: Mutex<HashMap<String, CachedNews>>,
    company_cache: Mutex<HashMap<String, CompanyIdentity>>,
    sec_ticker_cache: Mutex<Option<HashMap<String, SecCompanyEntry>>>,
    article_cache_path: PathBuf,
    article_cache_cleaned: AtomicBool,
}

impl DefaultSourceStore {
    pub fn new() -> Self {
        Self::with_article_cache_path(crate::config::news_cache_path())
    }

    pub(crate) fn with_article_cache_path(article_cache_path: PathBuf) -> Self {
        let sec_http = Client::builder()
            .use_rustls_tls()
            .no_proxy()
            .timeout(Duration::from_secs(crate::data::DEFAULT_HTTP_TIMEOUT_SECS))
            .build()
            .expect("sec http client");
        Self {
            sec_http,
            news_cache: Mutex::new(HashMap::new()),
            company_cache: Mutex::new(HashMap::new()),
            sec_ticker_cache: Mutex::new(None),
            article_cache_path,
            article_cache_cleaned: AtomicBool::new(false),
        }
    }

    async fn with_article_cache<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let path = self.article_cache_path.clone();
        let run_cleanup = !self.article_cache_cleaned.swap(true, Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            let mut connection = open_article_cache(&path)?;
            if run_cleanup {
                cleanup_article_cache(&connection)?;
            }
            operation(&mut connection)
        })
        .await?
    }

    async fn sec_ticker_map(&self) -> Result<HashMap<String, SecCompanyEntry>> {
        if let Some(cached) = self.sec_ticker_cache.lock().await.clone() {
            debug_log("data", "sec ticker map cache hit");
            return Ok(cached);
        }
        debug_log(
            "data",
            format!("fetching SEC ticker map url={SEC_TICKER_URL}"),
        );
        let resp = self
            .sec_http
            .get(SEC_TICKER_URL)
            .header("User-Agent", APP_USER_AGENT)
            .header("Accept", "application/json")
            .send()
            .await?;
        let status = resp.status();
        debug_log("data", format!("sec ticker map response status={status}"));
        let body = resp.error_for_status()?.text().await?;
        debug_log(
            "data",
            format!("sec ticker map response body_chars={}", body.len()),
        );
        let fetched: HashMap<String, SecCompanyEntry> =
            serde_json::from_str(&body).with_context(|| {
                let preview: String = body.chars().take(200).collect();
                format!("deserialize response from {SEC_TICKER_URL}: {preview}")
            })?;
        debug_log(
            "data",
            format!("loaded SEC ticker map entries={}", fetched.len()),
        );
        *self.sec_ticker_cache.lock().await = Some(fetched.clone());
        Ok(fetched)
    }
}

impl Default for DefaultSourceStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SourceStore for DefaultSourceStore {
    async fn resolve_company_identity(&self, ticker: &str) -> Result<CompanyIdentity> {
        let ticker = ticker.trim().to_ascii_uppercase();
        debug_log("data", format!("resolve company identity ticker={ticker}"));
        if let Some(cached) = self.company_cache.lock().await.get(&ticker).cloned() {
            debug_log(
                "data",
                format!("company identity cache hit ticker={}", cached.ticker),
            );
            return Ok(cached);
        }
        let companies = match self.sec_ticker_map().await {
            Ok(map) => {
                debug_log("data", format!("sec ticker map ok entries={}", map.len()));
                Some(map)
            }
            Err(err) => {
                debug_log("data", format!("sec ticker map failed err={err:#}"));
                None
            }
        };
        let company = companies
            .as_ref()
            .into_iter()
            .flat_map(|companies| companies.values())
            .find(|entry| entry.ticker.eq_ignore_ascii_case(&ticker))
            .cloned();

        let identity = CompanyIdentity {
            ticker: ticker.clone(),
            company_name: company
                .as_ref()
                .map(|entry| normalize_sec_company_title(&entry.title))
                .unwrap_or_else(|| ticker.clone()),
        };
        self.company_cache
            .lock()
            .await
            .insert(ticker, identity.clone());
        debug_log(
            "data",
            format!(
                "resolved company identity ticker={} company={}",
                identity.ticker, identity.company_name
            ),
        );
        Ok(identity)
    }

    async fn load_cached_news(&self, ticker: &str) -> Result<Option<CachedNews>> {
        Ok(self
            .news_cache
            .lock()
            .await
            .get(&cache_key(ticker))
            .cloned())
    }

    async fn store_cached_news(&self, ticker: &str, cached: CachedNews) -> Result<()> {
        self.news_cache
            .lock()
            .await
            .insert(cache_key(ticker), cached);
        Ok(())
    }

    async fn load_article_analysis(
        &self,
        context_key: &str,
        articles: &[ArticleCacheIdentity],
    ) -> Result<HashMap<String, CachedArticleAnalysis>> {
        if articles.is_empty() {
            return Ok(HashMap::new());
        }
        let context_key = context_key.to_string();
        let article_keys = articles
            .iter()
            .map(|article| article.article_key.clone())
            .collect::<Vec<_>>();
        self.with_article_cache(move |connection| {
            let mut statement = connection.prepare(
                "SELECT relevant, label
                 FROM article_analysis
                 WHERE article_key = ?1 AND context_key = ?2",
            )?;
            let mut analyses = HashMap::new();
            for article_key in article_keys {
                let cached = statement
                    .query_row(params![&article_key, &context_key], |row| {
                        Ok(CachedArticleAnalysis {
                            relevant: row.get::<_, Option<bool>>(0)?,
                            label: row.get(1)?,
                        })
                    })
                    .optional()?;
                if let Some(cached) = cached {
                    analyses.insert(article_key, cached);
                }
            }
            Ok(analyses)
        })
        .await
    }

    async fn store_article_analysis(
        &self,
        context_key: &str,
        updates: &[ArticleAnalysisUpdate],
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let context_key = context_key.to_string();
        let updates = updates.to_vec();
        self.with_article_cache(move |connection| {
            let transaction = connection.transaction()?;
            let now = unix_timestamp(SystemTime::now());
            for update in updates {
                transaction.execute(
                    "INSERT INTO articles (
                        article_key, normalized_url, publisher, title, published_at,
                        first_seen_at, last_seen_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                     ON CONFLICT(article_key) DO UPDATE SET
                        normalized_url = excluded.normalized_url,
                        publisher = excluded.publisher,
                        title = excluded.title,
                        published_at = COALESCE(excluded.published_at, articles.published_at),
                        last_seen_at = excluded.last_seen_at",
                    params![
                        &update.article.article_key,
                        &update.article.normalized_url,
                        &update.article.publisher,
                        &update.article.title,
                        update.article.published_at.map(unix_timestamp),
                        now,
                    ],
                )?;
                transaction.execute(
                    "INSERT INTO article_analysis (
                        article_key, context_key, relevant, label, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(article_key, context_key) DO UPDATE SET
                        relevant = COALESCE(excluded.relevant, article_analysis.relevant),
                        label = COALESCE(excluded.label, article_analysis.label),
                        updated_at = excluded.updated_at",
                    params![
                        &update.article.article_key,
                        &context_key,
                        update.relevant,
                        update.label,
                        now,
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }
}

fn cache_key(ticker: &str) -> String {
    ticker.trim().to_ascii_uppercase()
}

fn open_article_cache(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS articles (
            article_key TEXT PRIMARY KEY,
            normalized_url TEXT NOT NULL,
            publisher TEXT NOT NULL,
            title TEXT NOT NULL,
            published_at INTEGER,
            first_seen_at INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS article_analysis (
            article_key TEXT NOT NULL,
            context_key TEXT NOT NULL,
            relevant INTEGER,
            label TEXT,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (article_key, context_key),
            FOREIGN KEY (article_key) REFERENCES articles(article_key) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_articles_last_seen_at ON articles(last_seen_at);
         CREATE TABLE IF NOT EXISTS search_history (
            normalized_term TEXT PRIMARY KEY,
            search_term TEXT NOT NULL,
            searched_at INTEGER NOT NULL,
            display_term TEXT,
            articles_found INTEGER,
            model TEXT,
            context_key TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_search_history_searched_at
            ON search_history(searched_at DESC);",
    )?;
    Ok(connection)
}

pub fn record_search_history_run(
    path: &Path,
    records: &[SearchHistoryRecord],
    model: Option<&str>,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut connection = open_article_cache(path)?;
    let transaction = connection.transaction()?;
    let searched_at = unix_timestamp_nanos(SystemTime::now());
    let model = model.map(str::trim).filter(|value| !value.is_empty());
    for (idx, record) in records.iter().enumerate() {
        let search_term = record.search_term.trim();
        if search_term.is_empty() {
            continue;
        }
        let display_term = record.display_term.trim();
        let context_key = record.context_key.trim();
        let record_searched_at = searched_at.saturating_sub(i64::try_from(idx).unwrap_or(i64::MAX));
        transaction.execute(
            "INSERT INTO search_history (
                normalized_term, search_term, display_term, searched_at,
                articles_found, model, context_key
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(normalized_term) DO UPDATE SET
                search_term = excluded.search_term,
                display_term = excluded.display_term,
                searched_at = excluded.searched_at,
                articles_found = excluded.articles_found,
                model = excluded.model,
                context_key = excluded.context_key",
            params![
                search_term.to_lowercase(),
                search_term,
                if display_term.is_empty() {
                    search_term
                } else {
                    display_term
                },
                record_searched_at,
                i64::try_from(record.articles_found).unwrap_or(i64::MAX),
                model,
                (!context_key.is_empty()).then_some(context_key),
            ],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

pub fn search_history_entries(path: &Path) -> Result<Vec<SearchHistoryEntry>> {
    let connection = open_article_cache(path)?;
    let mut statement = connection.prepare(
        "SELECT search_term, COALESCE(display_term, search_term), searched_at,
                articles_found, model
         FROM search_history
         ORDER BY searched_at DESC, normalized_term ASC",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok(SearchHistoryEntry {
                search_term: row.get(0)?,
                display_term: row.get(1)?,
                searched_at: UNIX_EPOCH + Duration::from_nanos(row.get::<_, i64>(2)?.max(0) as u64),
                articles_found: row
                    .get::<_, Option<i64>>(3)?
                    .and_then(|count| count.try_into().ok())
                    .unwrap_or(0),
                model: row.get::<_, Option<String>>(4)?.and_then(|value| {
                    let value = value.trim().to_string();
                    (!value.is_empty()).then_some(value)
                }),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn delete_search_history_term(path: &Path, search_term: &str) -> Result<()> {
    let search_term = search_term.trim();
    if search_term.is_empty() {
        return Ok(());
    }
    let mut connection = open_article_cache(path)?;
    let transaction = connection.transaction()?;
    let deleted_row = transaction
        .query_row(
            "SELECT context_key FROM search_history WHERE normalized_term = ?1",
            [search_term.to_lowercase()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    let Some(deleted_context) = deleted_row else {
        return Ok(());
    };
    transaction.execute(
        "DELETE FROM search_history WHERE normalized_term = ?1",
        [search_term.to_lowercase()],
    )?;

    if let Some(deleted_context) = deleted_context {
        let retained_contexts = {
            let mut statement = transaction
                .prepare("SELECT context_key FROM search_history WHERE context_key IS NOT NULL")?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<HashSet<_>>>()?
        };
        if !retained_contexts.contains(&deleted_context) {
            transaction.execute(
                "DELETE FROM article_analysis WHERE context_key = ?1",
                [deleted_context],
            )?;
        }
    }
    transaction.execute(
        "DELETE FROM articles
         WHERE NOT EXISTS (
             SELECT 1
             FROM article_analysis
             WHERE article_analysis.article_key = articles.article_key
        )",
        [],
    )?;
    transaction.commit()?;
    Ok(())
}

fn cleanup_article_cache(connection: &Connection) -> Result<()> {
    const RETENTION_SECS: i64 = 180 * 24 * 60 * 60;
    let cutoff = unix_timestamp(SystemTime::now()).saturating_sub(RETENTION_SECS);
    connection.execute("DELETE FROM articles WHERE last_seen_at < ?1", [cutoff])?;
    Ok(())
}

fn unix_timestamp(value: SystemTime) -> i64 {
    value
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn unix_timestamp_nanos(value: SystemTime) -> i64 {
    value
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .try_into()
        .unwrap_or(i64::MAX)
}

pub(crate) fn normalize_sec_company_title(title: &str) -> String {
    title
        .split_whitespace()
        .map(title_case_word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn title_case_word(word: &str) -> String {
    let lower = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.');
    let normalized = match lower.to_ascii_lowercase().as_str() {
        "inc" => "Inc".to_string(),
        "corp" => "Corp".to_string(),
        "co" => "Co".to_string(),
        "ltd" => "Ltd".to_string(),
        "llc" => "LLC".to_string(),
        "plc" => "PLC".to_string(),
        "lp" | "lp." => "L.P.".to_string(),
        "sa" => "SA".to_string(),
        "nv" => "NV".to_string(),
        "ag" => "AG".to_string(),
        other
            if other.chars().any(|c| c.is_ascii_digit())
                || other.contains('&')
                || other.contains('/') =>
        {
            lower.to_ascii_uppercase()
        }
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => format!(
                    "{}{}",
                    first.to_ascii_uppercase(),
                    chars.as_str().to_ascii_lowercase()
                ),
                None => String::new(),
            }
        }
    };
    word.replacen(lower, &normalized, 1)
}

#[derive(Debug, Deserialize, Clone)]
struct SecCompanyEntry {
    ticker: String,
    title: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{ArticleAnalysisUpdate, article_cache_identity};

    fn temp_db(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "gn-{name}-{}-{}.sqlite3",
            std::process::id(),
            unix_timestamp_nanos(SystemTime::now())
        ))
    }

    fn cleanup_db(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    }

    fn record_history(
        path: &Path,
        search_term: &str,
        display_term: &str,
        context_key: &str,
        articles_found: usize,
        model: Option<&str>,
    ) {
        record_search_history_run(
            path,
            &[history_record(
                search_term,
                display_term,
                context_key,
                articles_found,
            )],
            model,
        )
        .expect("record history");
    }

    fn history_record(
        search_term: &str,
        display_term: &str,
        context_key: &str,
        articles_found: usize,
    ) -> SearchHistoryRecord {
        SearchHistoryRecord {
            search_term: search_term.to_string(),
            display_term: display_term.to_string(),
            context_key: context_key.to_string(),
            articles_found,
        }
    }

    async fn store_analysis(
        store: &DefaultSourceStore,
        context_key: &str,
        article: &ArticleCacheIdentity,
        label: Option<&str>,
    ) {
        store
            .store_article_analysis(
                context_key,
                &[ArticleAnalysisUpdate {
                    article: article.clone(),
                    relevant: Some(true),
                    label: label.map(str::to_string),
                }],
            )
            .await
            .expect("store analysis");
    }

    async fn analysis_keys(
        store: &DefaultSourceStore,
        context_key: &str,
        article: &ArticleCacheIdentity,
    ) -> Vec<String> {
        store
            .load_article_analysis(context_key, std::slice::from_ref(article))
            .await
            .expect("load analysis")
            .into_keys()
            .collect()
    }

    #[test]
    fn normalize_sec_title_keeps_suffix_case() {
        assert_eq!(
            normalize_sec_company_title("NVIDIA CORP"),
            "Nvidia Corp".to_string()
        );
        assert_eq!(
            normalize_sec_company_title("TAIWAN SEMICONDUCTOR MANUFACTURING CO LTD"),
            "Taiwan Semiconductor Manufacturing Co Ltd".to_string()
        );
        assert_eq!(
            normalize_sec_company_title("NOVO NORDISK A S"),
            "Novo Nordisk A S".to_string()
        );
    }

    #[tokio::test]
    async fn article_analysis_persists_across_store_instances() {
        let path = temp_db("news-cache");
        let article = article_cache_identity(
            "Reuters",
            "Nvidia faces AI chip export rules",
            "https://example.com/nvda-export-rules",
            None,
        );
        let first = DefaultSourceStore::with_article_cache_path(path.clone());
        store_analysis(&first, "company:NVDA", &article, Some("AI Chip Rules")).await;
        drop(first);

        let second = DefaultSourceStore::with_article_cache_path(path.clone());
        let loaded = second
            .load_article_analysis("company:NVDA", std::slice::from_ref(&article))
            .await
            .expect("load analysis");
        assert_eq!(
            loaded.get(&article.article_key),
            Some(&CachedArticleAnalysis {
                relevant: Some(true),
                label: Some("AI Chip Rules".to_string()),
            })
        );

        drop(second);
        cleanup_db(&path);
    }

    #[test]
    fn search_history_is_case_insensitive_and_most_recent_first() {
        let path = temp_db("search-history");

        record_history(&path, "NVDA", "NVDA", "company:NVDA", 0, None);
        record_history(&path, "ai chips", "ai chips", "query:ai chips", 0, None);
        record_history(&path, "nvda", "nvda", "company:NVDA", 0, None);

        assert_eq!(
            search_history_entries(&path)
                .expect("load search history")
                .into_iter()
                .map(|entry| entry.search_term)
                .collect::<Vec<_>>(),
            vec!["nvda".to_string(), "ai chips".to_string()]
        );

        cleanup_db(&path);
    }

    #[tokio::test]
    async fn search_history_entries_include_counts_and_delete_selected_term() {
        let path = temp_db("search-history-counts");

        record_history(
            &path,
            "NVDA",
            "Nvidia Corp",
            "company:NVDA",
            12,
            Some("gpt-5.5"),
        );
        let article = article_cache_identity(
            "Reuters",
            "Nvidia faces AI chip export rules",
            "https://example.com/nvda-export-rules",
            None,
        );
        let store = DefaultSourceStore::with_article_cache_path(path.clone());
        store_analysis(&store, "company:NVDA", &article, Some("AI Chip Rules")).await;
        record_history(&path, "ai chips", "ai chips", "query:ai chips", 0, None);

        let entries = search_history_entries(&path).expect("load search history entries");
        assert_eq!(entries[0].search_term, "ai chips");
        assert_eq!(entries[0].articles_found, 0);
        assert_eq!(entries[1].search_term, "NVDA");
        assert_eq!(entries[1].display_term, "Nvidia Corp");
        assert_eq!(entries[1].articles_found, 12);
        assert_eq!(entries[1].model.as_deref(), Some("gpt-5.5"));

        delete_search_history_term(&path, "nvda").expect("delete search term");
        assert_eq!(
            search_history_entries(&path)
                .expect("load search history")
                .into_iter()
                .map(|entry| entry.search_term)
                .collect::<Vec<_>>(),
            vec!["ai chips".to_string()]
        );
        assert!(
            analysis_keys(&store, "company:NVDA", &article)
                .await
                .is_empty()
        );

        cleanup_db(&path);
    }

    #[tokio::test]
    async fn deleting_mixed_search_entries_uses_exact_recorded_contexts() {
        let path = temp_db("mixed-history-delete");
        record_search_history_run(
            &path,
            &[
                history_record("Compute", "Compute", "query:compute", 1),
                history_record("glw", "Corning Inc", "company:GLW", 1),
            ],
            Some("gpt-5.5"),
        )
        .expect("record mixed search");
        let query_article = article_cache_identity(
            "Reuters",
            "Cloud providers increase compute capacity",
            "https://example.com/cloud-compute-capacity",
            None,
        );
        let company_article = article_cache_identity(
            "Bloomberg",
            "Corning expands optical fiber production",
            "https://example.com/corning-optical-fiber",
            None,
        );
        let store = DefaultSourceStore::with_article_cache_path(path.clone());
        store_analysis(&store, "query:compute", &query_article, None).await;
        store_analysis(&store, "company:GLW", &company_article, None).await;

        delete_search_history_term(&path, "missing").expect("ignore missing history term");
        assert!(
            !analysis_keys(&store, "query:compute", &query_article)
                .await
                .is_empty()
        );

        delete_search_history_term(&path, "Compute").expect("delete exact query");

        assert!(
            analysis_keys(&store, "query:compute", &query_article)
                .await
                .is_empty()
        );
        assert!(
            !analysis_keys(&store, "company:GLW", &company_article)
                .await
                .is_empty()
        );

        drop(store);
        cleanup_db(&path);
    }
}
