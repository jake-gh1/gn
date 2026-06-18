//! News coverage workflow entrypoints and cached headline synthesis.

use crate::config::{AllowlistEntry, debug_log};
use crate::data::{
    ArticleAnalysisUpdate, CachedNews, CompanyIdentity, SearchResult, SourceStore,
    build_company_news_search_query, build_news_articles, company_analysis_context_key,
    compare_news_articles, is_symbol_candidate, query_analysis_context_key, split_news_query_terms,
};
use crate::llm::{CompletionRequest, CompletionResponse, LlmClient, TokenUsage};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::data::NewsArticle;
use crate::workflow::shared::*;
use crate::workflow::{
    ProgressSink, StartWorkflow, WorkflowEngine, WorkflowHistoryEntry, WorkflowProgress,
    WorkflowResult,
};

fn emit(progress: &ProgressSink, event: WorkflowProgress) {
    (**progress)(event);
}

fn emit_usage(progress: &ProgressSink, usage: &TokenUsage) {
    if usage.input_tokens > 0 || usage.output_tokens > 0 || usage.total_tokens > 0 {
        emit(progress, WorkflowProgress::Usage(usage.clone()));
    }
}

const NEWS_LLM_CHUNK_SIZE: usize = 20;
const NEWS_LLM_MAX_CONCURRENCY: usize = 5;
const NEWS_LABEL_MIN_WORDS: usize = 2;
const NEWS_LABEL_MAX_WORDS: usize = 3;
const NEWS_LABEL_MAX_CHARS: usize = 42;
const NEWS_LABEL_PLACEHOLDER: &str = "-";

/// Production workflow engine: one shared LLM client, one shared data store, and a per-run allowlist.
pub struct DefaultWorkflowEngine {
    llm: Arc<dyn LlmClient>,
    sources: Arc<dyn SourceStore>,
    allowlist: Vec<AllowlistEntry>,
}

impl DefaultWorkflowEngine {
    pub fn new(
        llm: Arc<dyn LlmClient>,
        sources: Arc<dyn SourceStore>,
        allowlist: Vec<AllowlistEntry>,
    ) -> Self {
        Self {
            llm,
            sources,
            allowlist,
        }
    }
}

#[async_trait]
impl WorkflowEngine for DefaultWorkflowEngine {
    async fn resolve_company_identity(&self, ticker: &str) -> Result<CompanyIdentity> {
        self.sources.resolve_company_identity(ticker).await
    }

    async fn start_with_progress(
        &self,
        req: StartWorkflow,
        progress: ProgressSink,
    ) -> Result<WorkflowResult> {
        self.start_coverage(req, &progress).await
    }
}

#[derive(Debug, Clone)]
struct NewsEditorialLabelItem {
    article_idx: usize,
    publisher: String,
    title: String,
}

struct NewsAnalysisContext<'a> {
    context_key: &'a str,
    label_context: &'a str,
    excluded_label_terms: &'a HashSet<String>,
    error_label: &'a str,
}

#[derive(Debug, Clone)]
enum CoverageQueryTerm {
    Company {
        search_term: String,
        identity: CompanyIdentity,
    },
    Exact(String),
}

#[derive(Debug, Deserialize)]
struct RawNewsEditorialLabel {
    #[serde(default, alias = "index", alias = "number", alias = "article")]
    id: Option<serde_json::Value>,
    #[serde(default, alias = "tag", alias = "title", alias = "name")]
    label: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum NewsLabelInvalidReason {
    Empty,
    WrongLength,
    TooLong,
    ExcludedTerm,
    Generic,
    PublisherLeakage,
}

impl NewsLabelInvalidReason {
    fn prompt_text(self) -> &'static str {
        match self {
            Self::Empty => "The label is empty or contains a line break.",
            Self::WrongLength => "The label is not 2-3 words.",
            Self::TooLong => "The label is too long for the table.",
            Self::ExcludedTerm => {
                "The label repeats the active company name or ticker already shown in context."
            }
            Self::Generic => {
                "The label is too generic; use a concrete event, product, policy, deal, market, executive, or compensation topic."
            }
            Self::PublisherLeakage => {
                "The label uses a publisher name that is not part of the title."
            }
        }
    }
}

impl DefaultWorkflowEngine {
    pub(crate) async fn start_coverage(
        &self,
        req: StartWorkflow,
        progress: &ProgressSink,
    ) -> Result<WorkflowResult> {
        if let Some(query) = req.exact_query() {
            if let Some(terms) = self.resolve_query_terms(query).await? {
                return self
                    .start_multi_query_coverage(query, terms, progress)
                    .await;
            }
            return self.start_query_coverage(query, progress).await;
        }

        // RSS provides discovery, the allowlist shapes grouping/order, and the UI renders the
        // resulting article queue directly.
        let identity = self.sources.resolve_company_identity(&req.ticker).await?;
        emit(progress, WorkflowProgress::Identity(identity.clone()));
        debug_log(
            "coverage",
            format!(
                "start ticker={} company={}",
                identity.ticker, identity.company_name
            ),
        );
        let (cached, workflow_usage, new_article_keys) = self
            .fetch_and_store_company_coverage(&identity, progress)
            .await?;

        let mut result = coverage_workflow_result(
            news_coverage_title(&identity.company_name),
            Some(identity.clone()),
            cached,
            workflow_usage,
        );
        result.new_article_keys = Some(new_article_keys);
        result.history_entries = vec![WorkflowHistoryEntry {
            search_term: identity.ticker.clone(),
            display_term: identity.company_name.clone(),
            context_key: company_analysis_context_key(&identity.ticker),
            articles_found: cached_news_article_count(&result),
        }];
        Ok(result)
    }

    // Returns None when the query should run as one plain exact-news search; otherwise each term
    // resolves to either a company workflow or an exact-query workflow. Single-term inputs that
    // resolve to a known ticker take the company path too, so callers can submit raw user input.
    async fn resolve_query_terms(&self, query: &str) -> Result<Option<Vec<CoverageQueryTerm>>> {
        let terms = split_news_query_terms(query);
        if terms.is_empty() {
            return Ok(None);
        }

        let mut resolved = Vec::with_capacity(terms.len());
        let mut seen_contexts = HashSet::<String>::new();
        for term in terms {
            let resolved_term = if is_symbol_candidate(&term) {
                let identity = self.sources.resolve_company_identity(&term).await?;
                if identity.company_name.eq_ignore_ascii_case(&identity.ticker) {
                    CoverageQueryTerm::Exact(term)
                } else {
                    CoverageQueryTerm::Company {
                        search_term: term,
                        identity,
                    }
                }
            } else {
                CoverageQueryTerm::Exact(term)
            };
            let context_key = analysis_context_key(&resolved_term);
            if seen_contexts.insert(context_key) {
                resolved.push(resolved_term);
            }
        }

        if let [CoverageQueryTerm::Exact(_)] = resolved.as_slice() {
            return Ok(None);
        }
        Ok((!resolved.is_empty()).then_some(resolved))
    }

    async fn start_multi_query_coverage(
        &self,
        query: &str,
        terms: Vec<CoverageQueryTerm>,
        progress: &ProgressSink,
    ) -> Result<WorkflowResult> {
        debug_log(
            "coverage",
            format!(
                "start multi_query query={query:?} contexts={}",
                terms
                    .iter()
                    .map(analysis_context_key)
                    .collect::<Vec<_>>()
                    .join(" / ")
            ),
        );

        // All-company queries resolve to a combined identity; surface it immediately so the UI
        // shows company names instead of raw tickers while results load.
        let identity = terms
            .iter()
            .map(|term| match term {
                CoverageQueryTerm::Company { identity, .. } => Some(identity),
                CoverageQueryTerm::Exact(_) => None,
            })
            .collect::<Option<Vec<_>>>()
            .map(|identities| CompanyIdentity {
                ticker: identities
                    .iter()
                    .map(|identity| identity.ticker.trim())
                    .collect::<Vec<_>>()
                    .join(" / "),
                company_name: identities
                    .iter()
                    .map(|identity| identity.company_name.trim())
                    .collect::<Vec<_>>()
                    .join(" / "),
            });
        if let Some(identity) = &identity {
            emit(progress, WorkflowProgress::Identity(identity.clone()));
        }

        let mut usage = TokenUsage::default();
        let mut history_entries = Vec::with_capacity(terms.len());
        let mut cached_results = Vec::with_capacity(terms.len());
        let mut new_article_keys = Vec::new();
        let mut seen_new_article_keys = HashSet::<String>::new();
        let mut accumulated_articles = Vec::<NewsArticle>::new();
        for (term_idx, term) in terms.iter().enumerate() {
            if terms.len() > 1 {
                let term_label = match term {
                    CoverageQueryTerm::Company { identity, .. } => &identity.company_name,
                    CoverageQueryTerm::Exact(term) => term,
                };
                emit(
                    progress,
                    WorkflowProgress::Stage(format!(
                        "Searching {} ({}/{})…",
                        term_label.trim(),
                        term_idx + 1,
                        terms.len()
                    )),
                );
            }
            // Each term's snapshots are merged with the completed terms so the table grows
            // instead of being replaced per term.
            let term_progress: ProgressSink = {
                let outer = Arc::clone(progress);
                let accumulated = accumulated_articles.clone();
                Arc::new(move |event| match event {
                    WorkflowProgress::Snapshot(articles) => {
                        outer(WorkflowProgress::Snapshot(merge_article_lists([
                            accumulated.clone(),
                            articles,
                        ])))
                    }
                    other => outer(other),
                })
            };
            let (cached, workflow_usage, term_new_article_keys) = match term {
                CoverageQueryTerm::Company { identity, .. } => {
                    self.fetch_and_store_company_coverage(identity, &term_progress)
                        .await?
                }
                CoverageQueryTerm::Exact(term) => {
                    self.fetch_query_coverage(term, &term_progress).await?
                }
            };
            accumulated_articles =
                merge_article_lists([accumulated_articles, cached.articles.clone()]);
            for key in term_new_article_keys {
                if seen_new_article_keys.insert(key.clone()) {
                    new_article_keys.push(key);
                }
            }
            add_token_usage(&mut usage, &workflow_usage);
            let (search_term, display_term) = match term {
                CoverageQueryTerm::Company {
                    search_term,
                    identity,
                } => (
                    search_term.clone(),
                    identity.company_name.trim().to_string(),
                ),
                CoverageQueryTerm::Exact(term) => (term.clone(), term.trim().to_string()),
            };
            history_entries.push(WorkflowHistoryEntry {
                search_term,
                display_term,
                context_key: analysis_context_key(term),
                articles_found: cached.articles.len(),
            });
            cached_results.push(cached);
        }

        let cached = merge_cached_news(cached_results);
        let display_term = terms
            .iter()
            .map(|term| match term {
                CoverageQueryTerm::Company { identity, .. } => identity.company_name.trim(),
                CoverageQueryTerm::Exact(term) => term.trim(),
            })
            .collect::<Vec<_>>()
            .join(" / ");
        let title_context = identity
            .as_ref()
            .map(|identity| identity.company_name.as_str())
            .unwrap_or(&display_term);

        let mut result =
            coverage_workflow_result(news_coverage_title(title_context), identity, cached, usage);
        result.history_entries = history_entries;
        result.new_article_keys = Some(new_article_keys);
        if !display_term.eq_ignore_ascii_case(query.trim()) {
            result.display_term = Some(display_term);
        }
        Ok(result)
    }

    async fn fetch_and_store_company_coverage(
        &self,
        identity: &CompanyIdentity,
        progress: &ProgressSink,
    ) -> Result<(CachedNews, TokenUsage, Vec<String>)> {
        let label_context = company_news_label_context(identity);
        let excluded_label_terms =
            news_label_excluded_terms(&[&identity.ticker, &identity.company_name]);
        let previous_cached = match self.sources.load_cached_news(&identity.ticker).await {
            Ok(cached) => cached,
            Err(err) => {
                debug_log(
                    "coverage",
                    format!(
                        "cached news read failed ticker={} err={err:#}",
                        identity.ticker
                    ),
                );
                None
            }
        };
        let (cached, workflow_usage, analysis_new_article_keys) = self
            .fetch_fresh_coverage(identity, &label_context, &excluded_label_terms, progress)
            .await?;
        let mut new_article_keys = previous_cached
            .as_ref()
            .map(|previous| new_article_keys_between(previous, &cached))
            .unwrap_or_default();
        let mut seen_new_keys = new_article_keys.iter().cloned().collect::<HashSet<_>>();
        for key in analysis_new_article_keys {
            if seen_new_keys.insert(key.clone()) {
                new_article_keys.push(key);
            }
        }
        self.sources
            .store_cached_news(&identity.ticker, cached.clone())
            .await?;
        Ok((cached, workflow_usage, new_article_keys))
    }

    async fn fetch_fresh_coverage(
        &self,
        identity: &CompanyIdentity,
        label_context: &str,
        excluded_label_terms: &HashSet<String>,
        progress: &ProgressSink,
    ) -> Result<(CachedNews, TokenUsage, Vec<String>)> {
        emit(
            progress,
            WorkflowProgress::Stage("Searching Google News…".to_string()),
        );
        let query = build_company_news_search_query(&identity.company_name);
        let all_results = self
            .sources
            .search_company_news(&query, &self.allowlist)
            .await?;
        let result_count = all_results.len();
        let context_key = company_analysis_context_key(&identity.ticker);
        let analysis_context = NewsAnalysisContext {
            context_key: &context_key,
            label_context,
            excluded_label_terms,
            error_label: "news relevance",
        };
        let (cached, workflow_usage, new_article_keys) = self
            .analyze_current_results(
                analysis_context,
                all_results,
                |chunk| build_news_relevance_prompt(identity, chunk),
                progress,
            )
            .await?;
        debug_log(
            "coverage",
            format!(
                "start articles_built ticker={} results={} articles={} source_urls={}",
                identity.ticker,
                result_count,
                cached.articles.len(),
                cached.source_urls.len()
            ),
        );

        Ok((cached, workflow_usage, new_article_keys))
    }

    async fn start_query_coverage(
        &self,
        query: &str,
        progress: &ProgressSink,
    ) -> Result<WorkflowResult> {
        debug_log("coverage", format!("start query={query}"));
        let (cached, workflow_usage, new_article_keys) =
            self.fetch_query_coverage(query, progress).await?;
        let mut result =
            coverage_workflow_result(news_coverage_title(query), None, cached, workflow_usage);
        result.new_article_keys = Some(new_article_keys);
        result.history_entries = vec![WorkflowHistoryEntry {
            search_term: query.trim().to_string(),
            display_term: query.trim().to_string(),
            context_key: query_analysis_context_key(query),
            articles_found: cached_news_article_count(&result),
        }];
        Ok(result)
    }

    async fn fetch_query_coverage(
        &self,
        query: &str,
        progress: &ProgressSink,
    ) -> Result<(CachedNews, TokenUsage, Vec<String>)> {
        emit(
            progress,
            WorkflowProgress::Stage("Searching Google News…".to_string()),
        );
        let all_results = self
            .sources
            .search_exact_news(query, &self.allowlist)
            .await?;
        let result_count = all_results.len();
        let label_context = format!("Search query: {}", query.trim());
        let excluded_label_terms = HashSet::new();
        let context_key = query_analysis_context_key(query);
        let analysis_context = NewsAnalysisContext {
            context_key: &context_key,
            label_context: &label_context,
            excluded_label_terms: &excluded_label_terms,
            error_label: "query news relevance",
        };
        let (cached, workflow_usage, new_article_keys) = self
            .analyze_current_results(
                analysis_context,
                all_results,
                |chunk| build_query_news_relevance_prompt(query, chunk),
                progress,
            )
            .await?;
        debug_log(
            "coverage",
            format!(
                "start query_results query={:?} results={} articles={} source_urls={}",
                query,
                result_count,
                cached.articles.len(),
                cached.source_urls.len()
            ),
        );
        Ok((cached, workflow_usage, new_article_keys))
    }

    async fn analyze_current_results(
        &self,
        context: NewsAnalysisContext<'_>,
        all_results: Vec<SearchResult>,
        build_prompt: impl Fn(&[(String, String)]) -> String,
        progress: &ProgressSink,
    ) -> Result<(CachedNews, TokenUsage, Vec<String>)> {
        let mut articles = build_news_articles(&all_results, &self.allowlist);
        emit(progress, WorkflowProgress::Snapshot(articles.clone()));
        let original_articles = articles
            .iter()
            .map(|article| article.cache_identity())
            .collect::<Vec<_>>();
        let analyses = match self
            .sources
            .load_article_analysis(context.context_key, &original_articles)
            .await
        {
            Ok(analyses) => analyses,
            Err(err) => {
                debug_log(
                    "coverage",
                    format!(
                        "article analysis cache read failed context={:?} err={err:#}",
                        context.context_key
                    ),
                );
                HashMap::new()
            }
        };

        let prompt_items = articles
            .iter()
            .map(|article| (article.publisher.clone(), article.title.clone()))
            .collect::<Vec<_>>();
        let mut decisions = vec![None; prompt_items.len()];
        let mut missing_indices = Vec::new();
        let mut missing_items = Vec::new();
        for (idx, (article, prompt_item)) in original_articles
            .iter()
            .zip(prompt_items.iter())
            .enumerate()
        {
            if let Some(relevant) = analyses
                .get(&article.article_key)
                .and_then(|cached| cached.relevant)
            {
                decisions[idx] = Some(relevant);
                continue;
            }
            missing_indices.push(idx);
            missing_items.push(prompt_item.clone());
        }

        let mut workflow_usage = TokenUsage::default();
        if !missing_items.is_empty() {
            emit(
                progress,
                WorkflowProgress::Stage(format!("Filtering {} headlines…", missing_items.len())),
            );
            let (fresh_decisions, usage) = self
                .news_relevance_decisions(
                    &missing_items,
                    build_prompt,
                    context.error_label,
                    progress,
                )
                .await?;
            add_token_usage(&mut workflow_usage, &usage);
            for (idx, relevant) in missing_indices.into_iter().zip(fresh_decisions) {
                decisions[idx] = Some(relevant);
            }
        }
        let decisions = decisions
            .into_iter()
            .map(|decision| decision.unwrap_or(true))
            .collect::<Vec<_>>();
        let mut decision_iter = decisions.iter().copied();
        articles.retain(|_| decision_iter.next().unwrap_or(false));

        for article in &mut articles {
            let Some(cached) = analyses.get(&article.cache_key()) else {
                continue;
            };
            let Some(label) = cached.label.as_deref() else {
                continue;
            };
            if let Ok(label) = validate_editorial_label(
                label,
                &article.publisher,
                &article.title,
                context.excluded_label_terms,
            ) {
                article.label = label;
            }
        }
        emit(progress, WorkflowProgress::Snapshot(articles.clone()));

        let mut cached = CachedNews {
            articles,
            source_urls: Vec::new(),
            total_tokens: workflow_usage.total_tokens,
        };
        let label_usage = self
            .editorialize_news_article_labels(
                &mut cached,
                context.label_context,
                context.excluded_label_terms,
                progress,
            )
            .await?;
        add_token_usage(&mut workflow_usage, &label_usage);
        cached.total_tokens = cached.total_tokens.saturating_add(label_usage.total_tokens);
        cached.source_urls = news_source_urls(&cached.articles);
        emit(
            progress,
            WorkflowProgress::Snapshot(cached.articles.clone()),
        );

        let labels = cached
            .articles
            .iter()
            .map(|article| (article.cache_key(), article.label.clone()))
            .collect::<HashMap<_, _>>();
        let new_article_keys = if analyses.is_empty() {
            Vec::new()
        } else {
            original_articles
                .iter()
                .zip(decisions.iter())
                .filter(|(article, relevant)| {
                    **relevant
                        && labels.contains_key(&article.article_key)
                        && !analyses.contains_key(&article.article_key)
                })
                .map(|(article, _)| article.article_key.clone())
                .collect()
        };
        let updates = original_articles
            .into_iter()
            .zip(decisions)
            .map(|(article, relevant)| {
                let label = relevant
                    .then(|| labels.get(&article.article_key).cloned())
                    .flatten();
                ArticleAnalysisUpdate {
                    article,
                    relevant: Some(relevant),
                    label,
                }
            })
            .collect::<Vec<_>>();
        if let Err(err) = self
            .sources
            .store_article_analysis(context.context_key, &updates)
            .await
        {
            debug_log(
                "coverage",
                format!(
                    "article analysis cache write failed context={:?} err={err:#}",
                    context.context_key
                ),
            );
        }
        debug_log(
            "coverage",
            format!(
                "article analysis context={:?} articles={} relevance_misses={} tokens={}",
                context.context_key,
                updates.len(),
                missing_items.len(),
                workflow_usage.total_tokens
            ),
        );
        Ok((cached, workflow_usage, new_article_keys))
    }

    async fn news_relevance_decisions(
        &self,
        items: &[(String, String)],
        build_prompt: impl Fn(&[(String, String)]) -> String,
        error_label: &str,
        progress: &ProgressSink,
    ) -> Result<(Vec<bool>, TokenUsage)> {
        if items.is_empty() {
            return Ok((Vec::new(), TokenUsage::default()));
        }

        let chunks: Vec<&[(String, String)]> = items.chunks(NEWS_LLM_CHUNK_SIZE).collect();
        let prompts = chunks
            .iter()
            .map(|chunk| build_prompt(chunk))
            .collect::<Vec<_>>();
        let responses = complete_news_chunks_in_parallel(&self.llm, prompts, true, progress).await;

        let mut decisions = Vec::with_capacity(items.len());
        let mut total_usage = TokenUsage::default();
        for (chunk, response) in chunks.iter().zip(responses.into_iter()) {
            let completion = response?;
            add_token_usage(&mut total_usage, &completion.usage);
            match parse_news_relevance_decisions(&completion.text, chunk.len()) {
                Some(parsed) => decisions.extend(parsed),
                None => {
                    debug_log(
                        "coverage",
                        format!(
                            "{} chunk unparsable; keeping unfiltered chunk chunk_size={} output_chars={}",
                            error_label,
                            chunk.len(),
                            completion.text.len()
                        ),
                    );
                    decisions.extend(std::iter::repeat_n(true, chunk.len()));
                }
            }
        }

        Ok((decisions, total_usage))
    }
    async fn editorialize_news_article_labels(
        &self,
        cached: &mut CachedNews,
        context: &str,
        excluded_terms: &HashSet<String>,
        progress: &ProgressSink,
    ) -> Result<TokenUsage> {
        let items = cached
            .articles
            .iter()
            .enumerate()
            .filter(|(_, article)| {
                validate_editorial_label(
                    &article.label,
                    &article.publisher,
                    &article.title,
                    excluded_terms,
                )
                .is_err()
            })
            .map(|(article_idx, article)| NewsEditorialLabelItem {
                article_idx,
                publisher: article.publisher.clone(),
                title: article.title.clone(),
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            return Ok(TokenUsage::default());
        }
        emit(
            progress,
            WorkflowProgress::Stage(format!("Labeling {} articles…", items.len())),
        );

        let chunks: Vec<&[NewsEditorialLabelItem]> = items.chunks(NEWS_LLM_CHUNK_SIZE).collect();
        let prompts: Vec<String> = chunks
            .iter()
            .map(|chunk| build_news_editorial_label_prompt(context, chunk))
            .collect();
        let responses = complete_news_chunks_in_parallel(&self.llm, prompts, true, progress).await;

        let mut total_usage = TokenUsage::default();
        let mut accepted_labels = Vec::with_capacity(items.len());
        for (chunk, response) in chunks.iter().zip(responses.into_iter()) {
            let completion = response?;
            add_token_usage(&mut total_usage, &completion.usage);
            let labels = parse_news_editorial_labels(&completion.text, chunk.len());
            if labels.is_none() {
                debug_log(
                    "coverage",
                    format!(
                        "news label editorial chunk unparsable; retrying items individually chunk_size={} output_chars={}",
                        chunk.len(),
                        completion.text.len()
                    ),
                );
            }
            let labels = labels.unwrap_or_else(|| vec![None; chunk.len()]);
            for (item, raw_label) in chunk.iter().zip(labels.into_iter()) {
                let label = self
                    .validated_or_repaired_editorial_label(
                        context,
                        item,
                        raw_label.as_deref().unwrap_or_default(),
                        excluded_terms,
                        &mut total_usage,
                        progress,
                    )
                    .await?;
                accepted_labels.push((item.article_idx, label));
            }
        }

        debug_assert_eq!(accepted_labels.len(), items.len());
        for (article_idx, label) in accepted_labels {
            cached.articles[article_idx].label = label;
        }
        Ok(total_usage)
    }

    async fn validated_or_repaired_editorial_label(
        &self,
        context: &str,
        item: &NewsEditorialLabelItem,
        raw_label: &str,
        excluded_terms: &HashSet<String>,
        total_usage: &mut TokenUsage,
        progress: &ProgressSink,
    ) -> Result<String> {
        let current_reason =
            match validate_editorial_label(raw_label, &item.publisher, &item.title, excluded_terms)
            {
                Ok(label) => return Ok(label),
                Err(reason) => reason,
            };

        let prompt = build_news_editorial_label_repair_prompt(
            context,
            item,
            raw_label,
            current_reason,
            excluded_terms,
        );
        let completion = self
            .llm
            .complete(CompletionRequest {
                prompt,
                json_mode: true,
            })
            .await?;
        add_token_usage(total_usage, &completion.usage);
        emit_usage(progress, &completion.usage);
        // Prefer the structurally parsed label, then fall back to any raw
        // label fragments (latest correction first) the model emitted.
        let mut candidates: Vec<String> = parse_news_editorial_labels(&completion.text, 1)
            .and_then(|labels| labels.into_iter().next().flatten())
            .into_iter()
            .collect();
        for fragment in editorial_label_fragments(&completion.text).into_iter().rev() {
            if !candidates.contains(&fragment) {
                candidates.push(fragment);
            }
        }
        if candidates.is_empty() {
            debug_log(
                "coverage",
                format!(
                    "news label editorial repair unparsable; using placeholder title={:?} output_chars={}",
                    item.title,
                    completion.text.len()
                ),
            );
            return Ok(NEWS_LABEL_PLACEHOLDER.to_string());
        }

        let mut final_reason = None;
        for candidate in &candidates {
            match validate_editorial_label(candidate, &item.publisher, &item.title, excluded_terms) {
                Ok(label) => return Ok(label),
                Err(reason) => {
                    final_reason.get_or_insert(reason);
                }
            }
        }

        debug_log(
            "coverage",
            format!(
                "news label editorial request returned invalid label; using placeholder title={:?} label={:?} reason={}",
                item.title,
                raw_label,
                final_reason.map_or("", NewsLabelInvalidReason::prompt_text)
            ),
        );
        Ok(NEWS_LABEL_PLACEHOLDER.to_string())
    }
}

fn merge_cached_news(caches: Vec<CachedNews>) -> CachedNews {
    let mut total_tokens = 0u32;
    let mut article_lists = Vec::with_capacity(caches.len());
    for cached in caches {
        total_tokens = total_tokens.saturating_add(cached.total_tokens);
        article_lists.push(cached.articles);
    }
    let articles = merge_article_lists(article_lists);
    let source_urls = news_source_urls(&articles);

    CachedNews {
        articles,
        source_urls,
        total_tokens,
    }
}

fn merge_article_lists(lists: impl IntoIterator<Item = Vec<NewsArticle>>) -> Vec<NewsArticle> {
    let mut best_by_key = HashMap::new();
    for article in lists.into_iter().flatten() {
        match best_by_key.get(&article.cache_key()) {
            Some(existing) if compare_news_articles(existing, &article).is_le() => {}
            _ => {
                best_by_key.insert(article.cache_key(), article);
            }
        }
    }
    let mut articles = best_by_key.into_values().collect::<Vec<_>>();
    articles.sort_by(compare_news_articles);
    articles
}

fn analysis_context_key(term: &CoverageQueryTerm) -> String {
    match term {
        CoverageQueryTerm::Company { identity, .. } => {
            company_analysis_context_key(&identity.ticker)
        }
        CoverageQueryTerm::Exact(query) => query_analysis_context_key(query),
    }
}

fn cached_news_article_count(result: &WorkflowResult) -> usize {
    result
        .cached_news
        .as_ref()
        .map(|cached| cached.articles.len())
        .unwrap_or(0)
}

fn new_article_keys_between(previous: &CachedNews, current: &CachedNews) -> Vec<String> {
    let previous_keys = previous.article_keys();
    current
        .article_keys()
        .difference(&previous_keys)
        .cloned()
        .collect()
}

fn company_news_label_context(identity: &CompanyIdentity) -> String {
    format!(
        "Company context: {} ({})",
        identity.company_name.trim(),
        identity.ticker.trim()
    )
}

fn news_label_excluded_terms(values: &[&str]) -> HashSet<String> {
    values
        .iter()
        .flat_map(|value| label_token_variant_set(value))
        .collect()
}

fn build_news_editorial_label_prompt(context: &str, items: &[NewsEditorialLabelItem]) -> String {
    let mut body = format!(
        "{context}\n\nWrite polished editorial tags for each news row in a dense news table.\n\nRules:\n- Return ONLY a JSON object: {{\"items\":[{{\"id\":1,\"label\":\"2-3 word tag\"}}]}}.\n- Return exactly one item for each title and preserve each title's numeric ID.\n- Use Title Case with no trailing punctuation.\n- Prefer the central business event, transaction, policy issue, product, market, executive action, or compensation topic.\n- Never use \"News\" as a label.\n- Avoid generic headline words unless they are paired with a specific topic.\n- Avoid vague abstractions that describe an outlook, strategy, sector, brief, analysis, takeaway, or technology trend instead of the concrete event.\n- If the context names an active company, do not include that company name or ticker unless needed to distinguish it from another named company.\n- Do not use publisher names unless the title itself is about that publisher.\n- Use only the title and publisher below; do not add outside facts.\n\nTitles:\n"
    );
    for (idx, item) in items.iter().enumerate() {
        body.push_str(&format!(
            "{}. [{}]\nTitle: {}\n",
            idx + 1,
            item.publisher.trim(),
            item.title.trim()
        ));
        body.push('\n');
    }
    body
}

fn build_news_editorial_label_repair_prompt(
    context: &str,
    item: &NewsEditorialLabelItem,
    invalid_label: &str,
    reason: NewsLabelInvalidReason,
    excluded_terms: &HashSet<String>,
) -> String {
    let disallowed_terms = sorted_label_terms(excluded_terms);
    let disallowed_line = if disallowed_terms.is_empty() {
        String::new()
    } else {
        format!(
            "\n- Disallowed label terms: {}.",
            disallowed_terms.join(", ")
        )
    };
    format!(
        "{context}\n\nRepair one editorial tag for a dense news table.\n\nRules:\n- Return ONLY this JSON shape with no markdown fences: {{\"items\":[{{\"label\":\"2-3 word tag\"}}]}}.\n- Return exactly one item.\n- The label must be 2 or 3 words; never return a one-word label.\n- Use Title Case with no trailing punctuation.\n- Never use \"News\" as a label.\n- Do not repeat the active company name or ticker from the context.{disallowed_line}\n- If the invalid label repeats a disallowed term, remove that term and preserve the remaining concrete action or topic when it still satisfies the rules.\n- Avoid vague abstractions; use the concrete event, product, policy, deal, market, executive, or compensation topic when available.\n- Use only the title and publisher below; do not add outside facts.\n\nInvalid label: {}\nProblem: {}\n\nPublisher: {}\nTitle: {}\n",
        invalid_label.trim(),
        reason.prompt_text(),
        item.publisher.trim(),
        item.title.trim()
    )
}

fn sorted_label_terms(terms: &HashSet<String>) -> Vec<String> {
    let mut terms = terms
        .iter()
        .filter(|term| !term.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>();
    terms.sort();
    terms.dedup();
    terms
}

fn parse_news_editorial_labels(text: &str, expected_len: usize) -> Option<Vec<Option<String>>> {
    if let Some(labels) = parse_first_json_value(text)
        .and_then(|value| news_editorial_labels_from_value(&value, expected_len))
    {
        return Some(labels);
    }
    labels_from_editorial_label_fragments(text, expected_len)
}

fn news_editorial_labels_from_value(
    value: &serde_json::Value,
    expected_len: usize,
) -> Option<Vec<Option<String>>> {
    match value {
        serde_json::Value::Array(items) => labels_from_editorial_label_items(items, expected_len),
        serde_json::Value::Object(map) => {
            for nested in editorial_label_container_values(map) {
                if let Some(labels) = news_editorial_labels_from_value(nested, expected_len) {
                    return Some(labels);
                }
            }

            if let Some(labels) = labels_from_editorial_label_map(map, expected_len) {
                return Some(labels);
            }

            (expected_len == 1)
                .then(|| editorial_label_from_value(value).map(|label| vec![Some(label)]))
                .flatten()
        }
        serde_json::Value::String(label) => (expected_len == 1).then(|| vec![Some(label.clone())]),
        _ => None,
    }
}

fn labels_from_editorial_label_items(
    items: &[serde_json::Value],
    expected_len: usize,
) -> Option<Vec<Option<String>>> {
    let has_ids = items.iter().any(editorial_label_has_id);
    if has_ids {
        let mut labels = vec![None; expected_len];
        for item in items {
            let Some((id, label)) = editorial_label_with_id(item) else {
                continue;
            };
            if (1..=expected_len).contains(&id) && labels[id - 1].is_none() {
                labels[id - 1] = Some(label);
            }
        }
        return Some(labels);
    }

    let labels = items
        .iter()
        .map(|item| editorial_label_from_value(item).map(Some))
        .collect::<Option<Vec<_>>>()?;
    (labels.len() == expected_len).then_some(labels)
}

fn editorial_label_container_values(
    map: &serde_json::Map<String, serde_json::Value>,
) -> impl Iterator<Item = &serde_json::Value> {
    const CONTAINER_KEYS: &[&str] = &["labels", "tags", "articles", "items", "output"];
    CONTAINER_KEYS.iter().filter_map(|key| map.get(*key))
}

fn labels_from_editorial_label_map(
    map: &serde_json::Map<String, serde_json::Value>,
    expected_len: usize,
) -> Option<Vec<Option<String>>> {
    let mut labels = vec![None; expected_len];
    let mut saw_numeric_key = false;
    for (key, value) in map {
        let Ok(id) = key.trim().parse::<usize>() else {
            continue;
        };
        saw_numeric_key = true;
        if !(1..=expected_len).contains(&id) || labels[id - 1].is_some() {
            continue;
        }
        if let Some(label) = editorial_label_from_value(value) {
            labels[id - 1] = Some(label);
        }
    }
    saw_numeric_key.then_some(labels)
}

fn labels_from_editorial_label_fragments(
    text: &str,
    expected_len: usize,
) -> Option<Vec<Option<String>>> {
    let labels = editorial_label_fragments(text);
    if labels.len() == expected_len {
        return Some(labels.into_iter().map(Some).collect());
    }
    if expected_len == 1 {
        return labels.into_iter().last().map(|label| vec![Some(label)]);
    }
    None
}

fn editorial_label_fragments(text: &str) -> Vec<String> {
    const KEYS: &[&str] = &["label", "tag", "title", "name"];
    let mut labels = Vec::new();
    for (idx, _) in text.char_indices() {
        for key in KEYS {
            let Some(label) = label_fragment_at(text, idx, key) else {
                continue;
            };
            labels.push((idx, label));
        }
    }
    labels.sort_by_key(|(idx, _)| *idx);
    labels.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
    labels.into_iter().map(|(_, label)| label).collect()
}

fn label_fragment_at(text: &str, idx: usize, key: &str) -> Option<String> {
    let rest = text.get(idx..)?;
    let key_start = rest
        .strip_prefix(&format!(r#""{key}""#))
        .or_else(|| rest.strip_prefix(&format!(r#""""{key}""#)))?;
    let value_start = key_start.trim_start().strip_prefix(':')?.trim_start();
    let value_start = value_start.strip_prefix('"')?;
    let mut value = String::new();
    let mut escaped = false;
    for ch in value_start.chars() {
        if escaped {
            value.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(value),
            _ => value.push(ch),
        }
    }
    None
}

fn editorial_label_has_id(value: &serde_json::Value) -> bool {
    value.as_object().is_some_and(|map| {
        ["id", "index", "number"]
            .iter()
            .any(|key| map.contains_key(*key))
    })
}

fn editorial_label_with_id(value: &serde_json::Value) -> Option<(usize, String)> {
    let raw = RawNewsEditorialLabel::deserialize(value.clone()).ok()?;
    let id = match raw.id? {
        serde_json::Value::Number(number) => number.as_u64()?.try_into().ok()?,
        serde_json::Value::String(number) => number.trim().parse().ok()?,
        _ => return None,
    };
    Some((id, raw.label?))
}

fn editorial_label_from_value(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(label) => Some(label.clone()),
        serde_json::Value::Object(_) => {
            let raw = RawNewsEditorialLabel::deserialize(value.clone()).ok()?;
            raw.label
        }
        _ => None,
    }
}

fn validate_editorial_label(
    raw_label: &str,
    publisher: &str,
    title: &str,
    excluded_terms: &HashSet<String>,
) -> std::result::Result<String, NewsLabelInvalidReason> {
    let label = normalize_editorial_label(raw_label).ok_or(NewsLabelInvalidReason::Empty)?;
    let word_count = label.split_whitespace().count();
    if !(NEWS_LABEL_MIN_WORDS..=NEWS_LABEL_MAX_WORDS).contains(&word_count) {
        return Err(NewsLabelInvalidReason::WrongLength);
    }
    if label.chars().count() > NEWS_LABEL_MAX_CHARS {
        return Err(NewsLabelInvalidReason::TooLong);
    }

    let label_tokens = label_validation_tokens(&label);
    if label_tokens.is_empty() {
        return Err(NewsLabelInvalidReason::Empty);
    }
    if label_tokens
        .iter()
        .any(|token| excluded_terms.contains(token))
    {
        return Err(NewsLabelInvalidReason::ExcludedTerm);
    }
    if label_tokens
        .iter()
        .all(|token| is_generic_label_token(token))
    {
        return Err(NewsLabelInvalidReason::Generic);
    }

    let title_tokens = label_token_variant_set(title);
    let publisher_tokens = label_token_variant_set(publisher);
    if label_tokens
        .iter()
        .flat_map(|token| token_variants(token))
        .any(|token| publisher_tokens.contains(&token) && !title_tokens.contains(&token))
    {
        return Err(NewsLabelInvalidReason::PublisherLeakage);
    }

    Ok(label)
}

fn label_token_variant_set(text: &str) -> HashSet<String> {
    label_validation_tokens(text)
        .into_iter()
        .flat_map(|token| token_variants(&token))
        .collect()
}

fn normalize_editorial_label(raw_label: &str) -> Option<String> {
    let trimmed = raw_label
        .trim()
        .trim_matches(|ch: char| matches!(ch, '"' | '\'' | '`' | '[' | ']'))
        .trim()
        .trim_matches(|ch: char| matches!(ch, '.' | ',' | ':' | ';' | '-' | '!' | '?'))
        .trim();
    if trimmed.is_empty() || trimmed.contains('\n') || trimmed.contains('\r') {
        return None;
    }
    let words = trimmed
        .split_whitespace()
        .map(title_case_label_word)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    (!words.is_empty()).then(|| words.join(" "))
}

fn title_case_label_word(word: &str) -> String {
    let word = word.trim_matches(|ch: char| matches!(ch, ',' | ':' | ';' | '!' | '?'));
    if word.is_empty() {
        return String::new();
    }
    if word.chars().any(|ch| ch.is_ascii_digit()) {
        return word.to_string();
    }
    if matches!(word.to_ascii_lowercase().as_str(), "ai" | "us" | "uk") {
        return word.to_ascii_uppercase();
    }
    if word.contains('.') && word.chars().all(|ch| ch.is_ascii_alphabetic() || ch == '.') {
        return word.to_ascii_uppercase();
    }
    if word.chars().all(|ch| ch.is_ascii_uppercase()) && word.chars().count() <= 4 {
        return word.to_string();
    }
    if word.chars().skip(1).any(|ch| ch.is_ascii_uppercase()) {
        return word.to_string();
    }
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => {
            first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase()
        }
        None => String::new(),
    }
}

async fn complete_news_chunks_in_parallel(
    llm: &Arc<dyn LlmClient>,
    prompts: Vec<String>,
    json_mode: bool,
    progress: &ProgressSink,
) -> Vec<Result<CompletionResponse>> {
    let semaphore = Arc::new(Semaphore::new(NEWS_LLM_MAX_CONCURRENCY));
    let mut handles = Vec::with_capacity(prompts.len());
    for prompt in prompts {
        let llm = Arc::clone(llm);
        let semaphore = Arc::clone(&semaphore);
        let progress = Arc::clone(progress);
        handles.push(tokio::spawn(async move {
            let _permit = semaphore
                .acquire()
                .await
                .expect("news chunk semaphore closed");
            debug_log(
                "workflow",
                format!("send llm chunk prompt_chars={}", prompt.len()),
            );
            let result = llm.complete(CompletionRequest { prompt, json_mode }).await;
            if let Ok(response) = &result {
                emit_usage(&progress, &response.usage);
            }
            result
        }));
    }
    let mut out = Vec::with_capacity(handles.len());
    for handle in handles {
        out.push(match handle.await {
            Ok(result) => result,
            Err(join_err) => Err(anyhow::anyhow!("news chunk task join error: {join_err}")),
        });
    }
    out
}

#[cfg(test)]
mod unit_tests {
    use std::collections::HashSet;

    use super::*;

    fn label_item() -> NewsEditorialLabelItem {
        NewsEditorialLabelItem {
            article_idx: 0,
            publisher: "Bloomberg".to_string(),
            title: "China's Hot, Unprofitable AI Stocks Are Hard to Short Until July".to_string(),
        }
    }

    fn validate_item_label(
        raw_label: &str,
        item: &NewsEditorialLabelItem,
        excluded_terms: &HashSet<String>,
    ) -> std::result::Result<String, NewsLabelInvalidReason> {
        validate_editorial_label(raw_label, &item.publisher, &item.title, excluded_terms)
    }

    #[test]
    fn editorial_labels_must_be_two_to_three_words() {
        assert!(validate_item_label("Shorts", &label_item(), &HashSet::new()).is_err());
        assert!(
            validate_item_label(
                "Unprofitable AI Stock Shorts",
                &label_item(),
                &HashSet::new()
            )
            .is_err()
        );
    }

    #[test]
    fn editorial_labels_map_partial_out_of_order_ids() {
        let labels = parse_news_editorial_labels(
            r#"{"items":[{"id":3,"label":"July Short Ban"},{"id":"1","label":"AI Stock Shorts"}]}"#,
            3,
        )
        .expect("label items");

        assert_eq!(
            labels,
            vec![
                Some("AI Stock Shorts".to_string()),
                None,
                Some("July Short Ban".to_string())
            ]
        );
    }

    #[test]
    fn editorial_labels_reject_partial_positional_output() {
        assert!(
            parse_news_editorial_labels(r#"{"items":[{"label":"AI Stock Shorts"}]}"#, 2).is_none()
        );
    }

    #[test]
    fn editorial_labels_parse_single_repair_object() {
        let labels = parse_news_editorial_labels(r#"{"label":"AI Stock Shorts"}"#, 1)
            .expect("single repair label");

        assert_eq!(labels, vec![Some("AI Stock Shorts".to_string())]);
    }

    #[test]
    fn editorial_labels_parse_nested_single_repair_string() {
        let labels = parse_news_editorial_labels(r#"{"labels":"AI Stock Shorts"}"#, 1)
            .expect("nested single repair label");

        assert_eq!(labels, vec![Some("AI Stock Shorts".to_string())]);
    }

    #[test]
    fn editorial_labels_parse_numeric_map_output() {
        let labels = parse_news_editorial_labels(
            r#"{"items":{"2":"Export Rule Changes","1":{"label":"Data Center Expansion"}}}"#,
            2,
        )
        .expect("numeric map labels");

        assert_eq!(
            labels,
            vec![
                Some("Data Center Expansion".to_string()),
                Some("Export Rule Changes".to_string())
            ]
        );
    }

    #[test]
    fn editorial_labels_repair_duplicated_key_quotes() {
        let labels = parse_news_editorial_labels(
            "```json\n{\"items\":[{\"\"label\":\"Secret Society\"}]}\n```",
            1,
        )
        .expect("duplicated key quote label");

        assert_eq!(labels, vec![Some("Secret Society".to_string())]);
    }

    #[test]
    fn editorial_labels_extract_malformed_single_label_fragment() {
        let labels = parse_news_editorial_labels(
            "```json\n{\"items\":[{\"\"label\":\"Solar Power\"]]}\n```",
            1,
        )
        .expect("malformed single label fragment");

        assert_eq!(labels, vec![Some("Solar Power".to_string())]);
    }

    #[test]
    fn editorial_label_fragments_preserve_correction_order() {
        let labels = editorial_label_fragments(
            "```json\n{\"items\":[{\"\"label\":\"Relocation News\"}]}\n```\n\
             Correction:\n```json\n{\"items\":[{\"\"label\":\"Relocation Move\"}]}\n```",
        );

        assert_eq!(
            labels,
            vec!["Relocation News".to_string(), "Relocation Move".to_string()]
        );
    }
}

#[cfg(test)]
mod workflow_tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use crate::config::AllowlistEntry;
    use crate::data::{
        ArticleAnalysisUpdate, ArticleCacheIdentity, CachedArticleAnalysis, CachedNews,
        CompanyIdentity, NewsArticle, SearchResult, SourceStore,
    };
    use crate::llm::{CompletionRequest, CompletionResponse, LlmClient, TokenUsage};
    use anyhow::Result;
    use async_trait::async_trait;

    use super::*;

    #[derive(Default)]
    struct FakeLlm {
        prompts: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl LlmClient for FakeLlm {
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
            self.prompts
                .lock()
                .expect("prompts")
                .push(req.prompt.clone());
            let text = if req
                .prompt
                .contains("Decide whether each news article title is relevant enough")
            {
                let include = req
                    .prompt
                    .lines()
                    .filter(|line| {
                        let trimmed = line.trim_start();
                        trimmed
                            .chars()
                            .next()
                            .map(|ch| ch.is_ascii_digit())
                            .unwrap_or(false)
                            && trimmed.contains(". [")
                    })
                    .enumerate()
                    .filter(|(_, line)| !line.contains("Jimmy Kimmel"))
                    .map(|(idx, _)| (idx + 1).to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                format!(r#"{{"include":[{include}]}}"#)
            } else if req
                .prompt
                .contains("Write polished editorial tags for each news row")
            {
                let items = req
                    .prompt
                    .lines()
                    .filter_map(|line| line.trim_start().strip_prefix("Title: "))
                    .map(|title| {
                        if title.contains("Blackstone") {
                            return serde_json::json!({
                                "label": "AI Models To Firms",
                            })
                            .to_string();
                        }
                        if title.contains("AI & Tech Brief") {
                            return serde_json::json!({
                                "label": "Nvidia Issues Warning",
                            })
                            .to_string();
                        }
                        if title.contains("Week Ahead") {
                            return serde_json::json!({
                                "label": "Upcoming Nvidia Moment",
                            })
                            .to_string();
                        }
                        serde_json::json!({
                            "label": "AI Chip Rules",
                        })
                        .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                format!(r#"{{"items":[{items}]}}"#)
            } else if req.prompt.contains("Repair one editorial tag") {
                if req.prompt.contains("Blackstone") {
                    return Ok(CompletionResponse {
                        text: serde_json::json!({
                            "items": [{
                                "label": "AI Models To Firms",
                            }],
                        })
                        .to_string(),
                        usage: TokenUsage {
                            input_tokens: 1200,
                            output_tokens: 140,
                            total_tokens: 1340,
                        },
                    });
                }
                serde_json::json!({
                    "items": [{
                        "label": "AI Chip Rules",
                    }],
                })
                .to_string()
            } else {
                "Nvidia coverage says AI chip demand remains ahead of near-term supply.".to_string()
            };
            Ok(CompletionResponse {
                text,
                usage: TokenUsage {
                    input_tokens: 1200,
                    output_tokens: 140,
                    total_tokens: 1340,
                },
            })
        }

        fn model_id(&self) -> &str {
            "fake"
        }

        fn provider(&self) -> &str {
            "fake"
        }
    }

    #[derive(Default)]
    struct PartialLabelLlm {
        prompts: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl LlmClient for PartialLabelLlm {
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
            self.prompts
                .lock()
                .expect("prompts")
                .push(req.prompt.clone());
            let text = if req
                .prompt
                .contains("Write polished editorial tags for each news row")
            {
                serde_json::json!({
                    "items": [{
                        "id": 2,
                        "label": "Export Rule Changes",
                    }],
                })
                .to_string()
            } else if req.prompt.contains("Repair one editorial tag") {
                serde_json::json!({
                    "items": [{
                        "label": "Data Center Expansion",
                    }],
                })
                .to_string()
            } else {
                unreachable!("unexpected prompt")
            };
            Ok(CompletionResponse {
                text,
                usage: TokenUsage::default(),
            })
        }

        fn model_id(&self) -> &str {
            "partial-label-fake"
        }

        fn provider(&self) -> &str {
            "fake"
        }
    }

    #[derive(Default)]
    struct FakeSources {
        cached_news: std::sync::Mutex<HashMap<String, CachedNews>>,
        article_analysis: std::sync::Mutex<HashMap<(String, String), CachedArticleAnalysis>>,
        search_queries: std::sync::Mutex<Vec<String>>,
        search_count: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl SourceStore for FakeSources {
        async fn resolve_company_identity(&self, ticker: &str) -> Result<CompanyIdentity> {
            let upper = ticker.to_ascii_uppercase();
            let company_name = match upper.as_str() {
                "AMZN" => "Amazon.com Inc",
                "ASML" => "Asml Holding NV",
                "GLW" => "Corning Inc",
                "NVDA" => "Nvidia Corp",
                _ => upper.as_str(),
            }
            .to_string();
            Ok(CompanyIdentity {
                ticker: upper,
                company_name,
            })
        }

        async fn search_company_news(
            &self,
            _query: &str,
            sources: &[AllowlistEntry],
        ) -> Result<Vec<SearchResult>> {
            self.search_queries
                .lock()
                .expect("search_queries")
                .push(_query.to_string());
            self.search_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let query = _query.to_ascii_lowercase();
            let selected_key = if query.contains("nvidia") {
                Some("NVDA")
            } else if query.contains("asml") {
                Some("ASML")
            } else if query.contains("corning") {
                Some("GLW")
            } else if query == "compute" {
                Some("COMPUTE")
            } else if query.contains("amazon") {
                Some("AMZN")
            } else {
                None
            };
            let results = self
                .cached_news
                .lock()
                .expect("cached_news")
                .iter()
                .filter(|(ticker, _)| selected_key.is_none_or(|key| ticker.as_str() == key))
                .map(|(_, cached)| cached)
                .flat_map(|cached| &cached.articles)
                .filter(|article| {
                    sources.is_empty()
                        || sources
                            .iter()
                            .any(|source| article.url.contains(&source.domain))
                })
                .map(|article| SearchResult {
                    source_name: article.publisher.clone(),
                    article_title: article.title.clone(),
                    url: article.url.clone(),
                    published_at: article.published_at,
                })
                .collect();
            Ok(results)
        }

        async fn search_exact_news(
            &self,
            query: &str,
            sources: &[AllowlistEntry],
        ) -> Result<Vec<SearchResult>> {
            self.search_company_news(query, sources).await
        }

        async fn load_cached_news(&self, ticker: &str) -> Result<Option<CachedNews>> {
            Ok(self
                .cached_news
                .lock()
                .expect("cached_news")
                .get(&ticker.to_ascii_uppercase())
                .cloned())
        }

        async fn store_cached_news(&self, ticker: &str, cached: CachedNews) -> Result<()> {
            self.cached_news
                .lock()
                .expect("cached_news")
                .insert(ticker.to_ascii_uppercase(), cached);
            Ok(())
        }

        async fn load_article_analysis(
            &self,
            context_key: &str,
            articles: &[ArticleCacheIdentity],
        ) -> Result<HashMap<String, CachedArticleAnalysis>> {
            let cache = self.article_analysis.lock().expect("article_analysis");
            Ok(articles
                .iter()
                .filter_map(|article| {
                    cache
                        .get(&(context_key.to_string(), article.article_key.clone()))
                        .cloned()
                        .map(|cached| (article.article_key.clone(), cached))
                })
                .collect())
        }

        async fn store_article_analysis(
            &self,
            context_key: &str,
            updates: &[ArticleAnalysisUpdate],
        ) -> Result<()> {
            let mut cache = self.article_analysis.lock().expect("article_analysis");
            for update in updates {
                let cached = cache
                    .entry((context_key.to_string(), update.article.article_key.clone()))
                    .or_default();
                if let Some(relevant) = update.relevant {
                    cached.relevant = Some(relevant);
                }
                if let Some(ref label) = update.label {
                    cached.label = Some(label.clone());
                }
            }
            Ok(())
        }
    }

    fn article(title: &str, publisher: &str, url: &str) -> NewsArticle {
        NewsArticle {
            title: title.to_string(),
            publisher: publisher.to_string(),
            url: url.to_string(),
            ..NewsArticle::default()
        }
    }

    fn seed(sources: &FakeSources, ticker: &str, articles: Vec<NewsArticle>) {
        sources.cached_news.lock().expect("cached_news").insert(
            ticker.to_string(),
            CachedNews {
                articles,
                ..CachedNews::default()
            },
        );
    }

    #[tokio::test]
    async fn editorial_labels_retry_only_missing_ids() {
        let llm = Arc::new(PartialLabelLlm::default());
        let engine =
            DefaultWorkflowEngine::new(llm.clone(), Arc::new(FakeSources::default()), Vec::new());
        let mut cached = CachedNews {
            articles: vec![
                article(
                    "Amazon expands AWS data center capacity",
                    "Reuters",
                    "https://example.com/aws-capacity",
                ),
                article(
                    "Nvidia faces AI chip export rule changes",
                    "Reuters",
                    "https://example.com/export-rules",
                ),
            ],
            ..CachedNews::default()
        };
        let progress: ProgressSink = Arc::new(|_| {});

        engine
            .editorialize_news_article_labels(
                &mut cached,
                "Search query: AI infrastructure",
                &HashSet::new(),
                &progress,
            )
            .await
            .expect("editorial labels");

        assert_eq!(cached.articles[0].label, "Data Center Expansion");
        assert_eq!(cached.articles[1].label, "Export Rule Changes");
        let prompts = llm.prompts.lock().expect("prompts");
        assert_eq!(
            prompts
                .iter()
                .filter(|prompt| prompt.contains("Write polished editorial tags"))
                .count(),
            1
        );
        assert_eq!(
            prompts
                .iter()
                .filter(|prompt| prompt.contains("Repair one editorial tag"))
                .count(),
            1
        );
        assert!(
            prompts
                .iter()
                .any(|prompt| prompt.contains("Amazon expands AWS data center capacity"))
        );
        assert!(
            prompts
                .iter()
                .filter(|prompt| prompt.contains("Repair one editorial tag"))
                .all(|prompt| !prompt.contains("Nvidia faces AI chip export rule changes"))
        );
    }

    #[tokio::test]
    async fn coverage_workflow_emits_candidate_filtered_and_labeled_snapshots() {
        let sources = Arc::new(FakeSources::default());
        seed(
            &sources,
            "AMZN",
            vec![
                article(
                    "Amazon expands AWS data center capacity",
                    "Reuters",
                    "https://example.com/amzn-aws",
                ),
                article(
                    "Jimmy Kimmel's Journey From Bro-Comic to Trump's Late-Night Foil",
                    "WSJ",
                    "https://example.com/kimmel",
                ),
            ],
        );
        let llm = Arc::new(FakeLlm::default());
        let engine = DefaultWorkflowEngine::new(llm, sources, Vec::new());
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_sink = Arc::clone(&events);
        let sink: ProgressSink = Arc::new(move |event| {
            events_for_sink.lock().expect("events").push(event);
        });

        engine
            .start_with_progress(StartWorkflow::for_ticker("AMZN"), sink)
            .await
            .expect("workflow");

        let snapshots = events
            .lock()
            .expect("events")
            .iter()
            .filter_map(|event| match event {
                WorkflowProgress::Snapshot(articles) => Some(articles.clone()),
                WorkflowProgress::Stage(_)
                | WorkflowProgress::Identity(_)
                | WorkflowProgress::Usage(_) => None,
            })
            .collect::<Vec<_>>();
        assert!(snapshots.len() >= 3);
        assert_eq!(snapshots[0].len(), 2);
        assert!(
            snapshots[0]
                .iter()
                .any(|article| article.title.contains("Jimmy Kimmel"))
        );
        assert!(snapshots.iter().skip(1).any(|articles| {
            articles.len() == 1
                && articles
                    .iter()
                    .all(|article| !article.title.contains("Jimmy Kimmel"))
        }));
        assert!(
            snapshots
                .last()
                .expect("final snapshot")
                .iter()
                .all(|article| !article.label.trim().is_empty())
        );
    }

    #[tokio::test]
    async fn coverage_workflow_repairs_label_that_repeats_active_company() {
        let sources = Arc::new(FakeSources::default());
        seed(
            &sources,
            "NVDA",
            vec![
                article(
                    "Week Ahead: Nvidia's moment",
                    "CNBC",
                    "https://example.com/nvda-week-ahead",
                ),
                article(
                    "AI & Tech Brief: Nvidia's warning on the 'inflection point'",
                    "The Information",
                    "https://example.com/nvda-warning",
                ),
            ],
        );
        let llm = Arc::new(FakeLlm::default());
        let engine = DefaultWorkflowEngine::new(llm.clone(), sources.clone(), Vec::new());

        let result = engine
            .start(StartWorkflow::for_ticker("NVDA"))
            .await
            .expect("cached news coverage workflow should repair invalid label");

        for article in &result.cached_news.as_ref().unwrap().articles {
            assert!(!article.label.contains("Nvidia"));
            assert!((2..=3).contains(&article.label.split_whitespace().count()));
        }
        assert!(
            llm.prompts
                .lock()
                .expect("prompts")
                .iter()
                .any(|prompt| { prompt.contains("Repair one editorial tag") })
        );
        assert_eq!(
            llm.prompts
                .lock()
                .expect("prompts")
                .iter()
                .filter(|prompt| {
                    prompt.contains("Write polished editorial tags for each news row")
                })
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn coverage_workflow_filters_cached_articles_to_allowlist() {
        let sources = Arc::new(FakeSources::default());
        seed(
            &sources,
            "AMZN",
            vec![
                article(
                    "Amazon expands AWS data center capacity",
                    "Reuters",
                    "https://www.reuters.com/technology/amazon-aws",
                ),
                article(
                    "Amazon expands AWS data center capacity",
                    "Business Insider",
                    "https://www.businessinsider.com/amazon-aws",
                ),
            ],
        );
        let engine = DefaultWorkflowEngine::new(
            Arc::new(FakeLlm::default()),
            sources,
            vec![AllowlistEntry {
                domain: "reuters.com".to_string(),
            }],
        );

        let result = engine
            .start(StartWorkflow::for_ticker("AMZN"))
            .await
            .expect("cached news coverage workflow should filter allowlist");

        let cached = result.cached_news.as_ref().expect("cached news");
        let publishers = cached
            .articles
            .iter()
            .map(|article| article.publisher.as_str())
            .collect::<Vec<_>>();
        assert_eq!(publishers, vec!["Reuters"]);
    }

    #[tokio::test]
    async fn repeated_company_search_refreshes_sources_without_repeating_model_calls() {
        let sources = Arc::new(FakeSources::default());
        seed(
            &sources,
            "NVDA",
            vec![article(
                "Nvidia faces AI chip export rules in China",
                "Reuters",
                "https://example.com/nvda-export-rules",
            )],
        );
        let llm = Arc::new(FakeLlm::default());
        let engine = DefaultWorkflowEngine::new(llm.clone(), sources.clone(), Vec::new());

        engine
            .start(StartWorkflow::for_ticker("NVDA"))
            .await
            .expect("first workflow");
        let prompt_count = llm.prompts.lock().expect("prompts").len();
        let second = engine
            .start(StartWorkflow::for_ticker("NVDA"))
            .await
            .expect("second workflow");

        assert_eq!(
            sources
                .search_count
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
        assert_eq!(llm.prompts.lock().expect("prompts").len(), prompt_count);
        assert_eq!(second.usage.total_tokens, 0);
    }

    #[tokio::test]
    async fn single_ticker_query_routes_to_company_workflow() {
        let sources = Arc::new(FakeSources::default());
        seed(
            &sources,
            "NVDA",
            vec![article(
                "Nvidia faces AI chip export rules in China",
                "Reuters",
                "https://example.com/nvda-export-rules",
            )],
        );
        let engine =
            DefaultWorkflowEngine::new(Arc::new(FakeLlm::default()), sources.clone(), Vec::new());

        let result = engine
            .start(StartWorkflow::for_query("nvda"))
            .await
            .expect("single ticker query");

        let identity = result.company_identity.expect("company identity");
        assert_eq!(identity.ticker, "NVDA");
        assert_eq!(identity.company_name, "Nvidia Corp");
        assert_eq!(
            result.history_entries[0].context_key.as_str(),
            "company:NVDA"
        );
        assert!(
            sources
                .search_queries
                .lock()
                .expect("search_queries")
                .iter()
                .any(|query| query.contains("Nvidia Corp"))
        );
    }

    #[tokio::test]
    async fn repeated_search_only_analyzes_new_articles() {
        let sources = Arc::new(FakeSources::default());
        seed(
            &sources,
            "NVDA",
            vec![article(
                "Nvidia faces AI chip export rules in China",
                "Reuters",
                "https://example.com/nvda-export-rules",
            )],
        );
        let llm = Arc::new(FakeLlm::default());
        let engine = DefaultWorkflowEngine::new(llm.clone(), sources.clone(), Vec::new());
        engine
            .start(StartWorkflow::for_ticker("NVDA"))
            .await
            .expect("first workflow");
        let first_prompt_count = llm.prompts.lock().expect("prompts").len();

        sources
            .cached_news
            .lock()
            .expect("cached_news")
            .get_mut("NVDA")
            .expect("NVDA cache")
            .articles
            .push(article(
                "Nvidia introduces new AI inference chips",
                "Bloomberg",
                "https://example.com/nvda-inference-chips",
            ));

        engine
            .start(StartWorkflow::for_ticker("NVDA"))
            .await
            .expect("second workflow");
        let prompts = llm.prompts.lock().expect("prompts");
        assert_eq!(prompts.len() - first_prompt_count, 2);
        assert!(
            prompts[first_prompt_count..]
                .iter()
                .all(|prompt| !prompt.contains("export rules"))
        );
        assert!(
            prompts[first_prompt_count..]
                .iter()
                .all(|prompt| prompt.contains("inference chips"))
        );
    }

    #[test]
    fn new_article_keys_between_returns_only_current_additions() {
        let previous = CachedNews {
            articles: vec![article(
                "Nvidia faces AI chip export rules in China",
                "Reuters",
                "https://example.com/nvda-export-rules",
            )],
            ..CachedNews::default()
        };
        let new_article = article(
            "Nvidia introduces new AI inference chips",
            "CNBC",
            "https://example.com/nvda-inference-chips",
        );
        let expected_key = new_article.cache_key();
        let current = CachedNews {
            articles: vec![
                article(
                    "Nvidia faces AI chip export rules in China",
                    "Reuters",
                    "https://example.com/nvda-export-rules",
                ),
                new_article,
            ],
            ..CachedNews::default()
        };

        assert_eq!(
            new_article_keys_between(&previous, &current),
            vec![expected_key]
        );
    }

    #[tokio::test]
    async fn identical_article_is_analyzed_separately_for_different_queries() {
        let sources = Arc::new(FakeSources::default());
        seed(
            &sources,
            "QUERY",
            vec![article(
                "Nvidia faces AI chip export rules in China",
                "Reuters",
                "https://example.com/nvda-export-rules",
            )],
        );
        let llm = Arc::new(FakeLlm::default());
        let engine = DefaultWorkflowEngine::new(llm.clone(), sources, Vec::new());

        engine
            .start(StartWorkflow::for_query("H200 China"))
            .await
            .expect("first query");
        let first_prompt_count = llm.prompts.lock().expect("prompts").len();
        engine
            .start(StartWorkflow::for_query("AI export rules"))
            .await
            .expect("second query");

        assert_eq!(
            llm.prompts.lock().expect("prompts").len() - first_prompt_count,
            2
        );
    }

    #[tokio::test]
    async fn slash_separated_query_and_ticker_reuse_their_standalone_analysis() {
        let sources = Arc::new(FakeSources::default());
        seed(
            &sources,
            "COMPUTE",
            vec![article(
                "Cloud providers increase compute capacity",
                "Reuters",
                "https://example.com/cloud-compute-capacity",
            )],
        );
        seed(
            &sources,
            "GLW",
            vec![article(
                "Corning expands optical fiber production",
                "Bloomberg",
                "https://example.com/corning-optical-fiber",
            )],
        );
        let llm = Arc::new(FakeLlm::default());
        let engine = DefaultWorkflowEngine::new(llm.clone(), sources.clone(), Vec::new());

        engine
            .start(StartWorkflow::for_query("Compute"))
            .await
            .expect("prime Compute query");
        engine
            .start(StartWorkflow::for_ticker("GLW"))
            .await
            .expect("prime GLW");
        let prompt_count = llm.prompts.lock().expect("prompts").len();

        let result = engine
            .start(StartWorkflow::for_query("Compute / glw"))
            .await
            .expect("combined mixed workflow");

        assert_eq!(llm.prompts.lock().expect("prompts").len(), prompt_count);
        assert_eq!(result.usage.total_tokens, 0);
        assert_eq!(result.company_identity, None);
        assert_eq!(
            result.display_term.as_deref(),
            Some("Compute / Corning Inc")
        );
        assert_eq!(result.title, "Compute / Corning Inc — News Coverage");
        assert_eq!(
            result
                .history_entries
                .iter()
                .map(|entry| (
                    entry.search_term.as_str(),
                    entry.display_term.as_str(),
                    entry.context_key.as_str(),
                    entry.articles_found,
                ))
                .collect::<Vec<_>>(),
            vec![
                ("Compute", "Compute", "query:compute", 1),
                ("glw", "Corning Inc", "company:GLW", 1),
            ]
        );
        let titles = result
            .cached_news
            .as_ref()
            .expect("combined cached news")
            .articles
            .iter()
            .map(|article| article.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            titles,
            vec![
                "Corning expands optical fiber production",
                "Cloud providers increase compute capacity"
            ]
        );
        let queries = sources.search_queries.lock().expect("search_queries");
        assert!(queries.iter().all(|query| query != "Compute / glw"));
        assert!(queries.iter().any(|query| query == "Compute"));
        assert!(queries.iter().any(|query| query.contains("Corning Inc")));
    }
}
