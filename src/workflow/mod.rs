//! Workflow orchestration for gn's news coverage workflow.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::data::{CachedNews, CompanyIdentity, NewsArticle};
use crate::llm::TokenUsage;

mod news_coverage_workflow;
mod shared;

pub use news_coverage_workflow::DefaultWorkflowEngine;

/// Input for a first-pass workflow run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartWorkflow {
    pub ticker: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

impl StartWorkflow {
    pub fn for_ticker(ticker: impl Into<String>) -> Self {
        Self {
            ticker: ticker.into(),
            query: None,
        }
    }

    pub fn for_query(query: impl Into<String>) -> Self {
        let query = query.into();
        Self {
            ticker: query.clone(),
            query: Some(query),
        }
    }

    pub fn exact_query(&self) -> Option<&str> {
        self.query
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty())
    }
}

/// Normalized workflow output consumed by the UI regardless of source type.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkflowResult {
    pub title: String,
    pub answer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_term: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history_entries: Vec<WorkflowHistoryEntry>,
    pub company_identity: Option<CompanyIdentity>,
    pub source_urls: Vec<String>,
    pub usage: TokenUsage,
    pub cached_news: Option<CachedNews>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_article_keys: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct WorkflowHistoryEntry {
    pub search_term: String,
    pub display_term: String,
    pub context_key: String,
    pub articles_found: usize,
}

/// Progress emitted while a coverage workflow runs, so the UI can render each stage live.
#[derive(Debug, Clone)]
pub enum WorkflowProgress {
    Stage(String),
    Identity(CompanyIdentity),
    Snapshot(Vec<NewsArticle>),
}

pub type ProgressSink = Arc<dyn Fn(WorkflowProgress) + Send + Sync>;

pub fn noop_progress() -> ProgressSink {
    Arc::new(|_| {})
}

/// Async interface the UI talks to; concrete implementations hide caching, prompting, and retrieval.
#[async_trait]
pub trait WorkflowEngine: Send + Sync {
    async fn resolve_company_identity(&self, ticker: &str) -> Result<CompanyIdentity>;
    async fn start(&self, req: StartWorkflow) -> Result<WorkflowResult> {
        self.start_with_progress(req, noop_progress()).await
    }
    async fn start_with_progress(
        &self,
        req: StartWorkflow,
        progress: ProgressSink,
    ) -> Result<WorkflowResult>;
}
