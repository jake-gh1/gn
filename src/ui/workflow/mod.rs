//! Workflow submission handling for the UI.

use std::sync::Arc;
use std::sync::mpsc::TryRecvError;

use crate::config::debug_log;
use crate::ui::*;
use crate::workflow::{ProgressSink, StartWorkflow, WorkflowProgress, WorkflowResult};

const STEP_LABEL: &str = "News Coverage";

/// Event delivered from the background workflow task to the UI loop.
pub(crate) enum WorkflowUiEvent {
    Progress(WorkflowProgress),
    Done(Result<Box<WorkflowResult>, String>),
}

/// Context for the in-flight search, needed when the completion event arrives.
pub(crate) struct PendingSearch {
    pub(crate) search_term: String,
    pub(crate) fallback_label: String,
}

impl AppModel {
    pub fn launch_search_after_preparing(&mut self, value: &str) -> anyhow::Result<()> {
        let query = value.trim();
        if query.is_empty() {
            return Ok(());
        }
        // Ticker-vs-query routing happens inside the engine, off the UI thread.
        self.prepare_workflow_request(StartWorkflow::for_query(query), query)
    }

    fn record_search_run_nonfatal(&self, search_term: &str, result: &WorkflowResult) {
        let mut entries = result.history_entries.clone();
        if entries.len() == 1 {
            entries[0].search_term = search_term.trim().to_string();
        }
        if entries.is_empty() {
            let display_term = workflow_result_display_term(result)
                .unwrap_or_else(|| search_term.trim().to_string());
            let context_key = result
                .company_identity
                .as_ref()
                .map(|identity| crate::data::company_analysis_context_key(&identity.ticker))
                .unwrap_or_else(|| crate::data::query_analysis_context_key(search_term));
            entries.push(crate::workflow::WorkflowHistoryEntry {
                search_term: search_term.trim().to_string(),
                display_term,
                context_key,
                articles_found: cached_news_article_count(result),
            });
        }
        let records = entries
            .into_iter()
            .map(|entry| crate::data::SearchHistoryRecord {
                search_term: entry.search_term,
                display_term: entry.display_term,
                context_key: entry.context_key,
                articles_found: entry.articles_found,
            })
            .collect::<Vec<_>>();
        let model = self.current_model_label();
        if let Err(err) = crate::data::record_search_history_run(
            &crate::config::news_cache_path(),
            &records,
            Some(&model),
        ) {
            debug_log("ui", format!("search history run write failed: {err}"));
        }
    }

    fn prepare_workflow_request(
        &mut self,
        request: StartWorkflow,
        search_term: &str,
    ) -> anyhow::Result<()> {
        debug_log(
            "ui",
            format!(
                "workflow prepare ticker={} query={:?}",
                request.ticker, request.query
            ),
        );

        if request.ticker.trim().is_empty() && request.exact_query().is_none() {
            return Ok(());
        }

        if self.runtime.models.is_empty()
            && (self.workflow_runtime.is_none() || self.workflow_engine.is_none())
        {
            self.set_status_message(
                "No runtime config is configured. Run `gn` to create or edit gn's runtime config.",
            );
            return Ok(());
        }

        if !self.ensure_provider_auth_for_workflow() {
            return Ok(());
        }

        match self.ensure_runtime_ready() {
            Ok(true) => {}
            Ok(false) => {
                self.set_status_message("Workflow unavailable: no workflow engine is configured.");
                return Ok(());
            }
            Err(err) => {
                self.set_status_message(&format!("Workflow unavailable: {err}"));
                return Ok(());
            }
        }

        let fallback_label = workflow_fallback_label(&request);
        self.started_at = std::time::Instant::now();
        self.run_started_at = std::time::SystemTime::now();
        self.completed_elapsed = None;
        self.run_reported_input_tokens = 0;
        self.run_reported_output_tokens = 0;

        let Some(workflow_runtime) = self.workflow_runtime.as_ref() else {
            self.set_status_message("Workflow unavailable: no workflow engine is configured.");
            return Ok(());
        };
        let Some(workflow_engine) = self.workflow_engine.as_ref() else {
            self.set_status_message("Workflow unavailable: no workflow engine is configured.");
            return Ok(());
        };

        let (sender, receiver) = std::sync::mpsc::channel();
        let engine = Arc::clone(workflow_engine);
        let progress_sender = sender.clone();
        let progress: ProgressSink = Arc::new(move |progress| {
            let _ = progress_sender.send(WorkflowUiEvent::Progress(progress));
        });
        workflow_runtime.spawn(async move {
            let result = engine
                .start_with_progress(request, progress)
                .await
                .map(Box::new)
                .map_err(|err| format!("{err:#}"));
            let _ = sender.send(WorkflowUiEvent::Done(result));
        });

        self.workflow_events = Some(receiver);
        self.pending_search = Some(PendingSearch {
            search_term: search_term.trim().to_string(),
            fallback_label,
        });
        self.live_articles = None;
        self.progress_note = Some("Starting workflow…".to_string());
        self.status_message = None;
        self.company_tickers = vec![search_term.trim().to_string()];
        self.company_names = vec![search_term.trim().to_string()];
        self.story_menu_focused = false;
        self.story_menu_highlight = 0;
        self.wait_for_initial_workflow_snapshot();

        Ok(())
    }

    pub(crate) fn poll_workflow_events(&mut self) {
        loop {
            let event = match self
                .workflow_events
                .as_ref()
                .map(|receiver| receiver.try_recv())
            {
                Some(Ok(event)) => event,
                Some(Err(TryRecvError::Empty)) | None => break,
                Some(Err(TryRecvError::Disconnected)) => {
                    self.clear_live_workflow_state();
                    self.set_status_message("Workflow stopped before it completed.");
                    break;
                }
            };

            if self.handle_workflow_event(event) {
                break;
            }
        }
    }

    fn wait_for_initial_workflow_snapshot(&mut self) {
        while self.workflow_events.is_some() && self.live_articles.is_none() {
            let event = match self
                .workflow_events
                .as_ref()
                .map(|receiver| receiver.recv())
            {
                Some(Ok(event)) => event,
                Some(Err(_)) => {
                    self.clear_live_workflow_state();
                    self.set_status_message("Workflow stopped before it completed.");
                    break;
                }
                None => break,
            };
            if self.handle_workflow_event(event) {
                break;
            }
        }
    }

    fn handle_workflow_event(&mut self, event: WorkflowUiEvent) -> bool {
        match event {
            WorkflowUiEvent::Progress(WorkflowProgress::Stage(note)) => {
                debug_log("ui", format!("workflow progress stage={note}"));
                self.progress_note = Some(note);
                false
            }
            WorkflowUiEvent::Progress(WorkflowProgress::Identity(identity)) => {
                debug_log(
                    "ui",
                    format!(
                        "workflow progress identity ticker={} company={}",
                        identity.ticker, identity.company_name
                    ),
                );
                self.company_tickers = vec![identity.ticker];
                self.company_names = vec![identity.company_name];
                false
            }
            WorkflowUiEvent::Progress(WorkflowProgress::Snapshot(articles)) => {
                debug_log(
                    "ui",
                    format!("workflow progress snapshot articles={}", articles.len()),
                );
                self.live_articles = Some(articles);
                self.story_menu_focused = self.news_article_row_count() > 0;
                self.story_menu_highlight = self
                    .story_menu_highlight
                    .min(self.news_article_row_count().saturating_sub(1));
                false
            }
            WorkflowUiEvent::Progress(WorkflowProgress::Usage(usage)) => {
                debug_log(
                    "ui",
                    format!(
                        "workflow progress usage in_tokens={} out_tokens={}",
                        usage.input_tokens, usage.output_tokens
                    ),
                );
                self.record_progress_usage(usage.input_tokens, usage.output_tokens);
                true
            }
            WorkflowUiEvent::Done(Ok(result)) => {
                let result = *result;
                self.completed_elapsed = Some(self.started_at.elapsed());
                let pending = self.pending_search.take();
                let search_term = pending
                    .as_ref()
                    .map(|pending| pending.search_term.as_str())
                    .unwrap_or("");
                let fallback_label = pending
                    .as_ref()
                    .map(|pending| pending.fallback_label.as_str())
                    .unwrap_or(STEP_LABEL);
                self.record_search_run_nonfatal(search_term, &result);
                self.status_message = (!result.answer.is_empty()).then(|| result.answer.clone());
                self.handle_step_complete(STEP_LABEL, result, fallback_label);
                self.clear_live_workflow_state();
                true
            }
            WorkflowUiEvent::Done(Err(err)) => {
                self.completed_elapsed = Some(self.started_at.elapsed());
                debug_log("ui", format!("workflow failed err={err}"));
                self.clear_live_workflow_state();
                self.set_status_message(&format!("Workflow failed: {err}"));
                true
            }
        }
    }

    fn clear_live_workflow_state(&mut self) {
        self.workflow_events = None;
        self.pending_search = None;
        self.live_articles = None;
        self.progress_note = None;
    }

    pub(crate) fn sync_runtime_config_if_changed(&mut self) -> bool {
        let path = self.runtime_config_path();
        let modified = std::fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok());

        if self.runtime_config_last_path.as_ref() != Some(&path) {
            self.runtime_config_last_path = Some(path);
            self.runtime_config_last_modified = modified;
            return false;
        }

        if modified == self.runtime_config_last_modified {
            return false;
        }

        self.runtime_config_last_modified = modified;
        match crate::config::load_runtime_config_with_user_config(
            path.clone(),
            self.user_config_path(),
        ) {
            Ok(runtime) => {
                self.apply_runtime_config(runtime);
                if !self.runtime.models.is_empty() {
                    self.check_provider_auth("startup");
                }
            }
            Err(err) => {
                self.set_status_message(&format!(
                    "Runtime config at {} could not be loaded: {err}",
                    path.display()
                ));
            }
        }
        true
    }

    pub(crate) fn handle_step_complete(
        &mut self,
        label: &str,
        result: WorkflowResult,
        fallback_label: &str,
    ) {
        debug_log(
            "ui",
            format!(
                "step.complete label={} before {} sources={} out_tokens={}",
                label,
                self.focus_debug_state(),
                result.source_urls.len(),
                result.usage.output_tokens
            ),
        );
        let result_label = workflow_result_label(&result, fallback_label);
        self.company_tickers = vec![
            result
                .company_identity
                .as_ref()
                .map(|identity| identity.ticker.clone())
                .unwrap_or_else(|| result_label.clone()),
        ];
        self.company_names = vec![
            result
                .company_identity
                .as_ref()
                .map(|identity| identity.company_name.clone())
                .unwrap_or(result_label),
        ];
        self.record_usage_remainder(result.usage.input_tokens, result.usage.output_tokens);
        self.remember_workflow_cache_snapshot(&result);
        self.story_menu_focused = !self.news_article_rows().is_empty();
        self.story_menu_highlight = 0;
        debug_log(
            "ui",
            format!(
                "step.complete label={} after {}",
                label,
                self.focus_debug_state()
            ),
        );
    }

    pub(crate) fn remember_workflow_cache_snapshot(&mut self, result: &WorkflowResult) {
        let ticker = result
            .company_identity
            .as_ref()
            .map(|identity| identity.ticker.to_ascii_uppercase())
            .or_else(|| {
                self.company_tickers
                    .first()
                    .map(|ticker| ticker.to_ascii_uppercase())
            });
        let Some(ticker) = ticker else {
            return;
        };
        if let Some(cached) = &result.cached_news {
            self.cached_news.insert(ticker.clone(), cached.clone());
        }
        match &result.new_article_keys {
            Some(keys) => {
                self.new_article_keys
                    .insert(ticker, keys.iter().cloned().collect());
            }
            None => {
                self.new_article_keys.remove(&ticker);
            }
        }
    }

    pub(crate) fn sync_ui_state(&mut self) {
        self.poll_workflow_events();
        self.sync_token_display();
        self.sync_runtime_config_if_changed();
    }

    pub(crate) fn record_progress_usage(&mut self, input_tokens: u32, output_tokens: u32) {
        let input_tokens = input_tokens as usize;
        let output_tokens = output_tokens as usize;
        self.total_input_tokens += input_tokens;
        self.total_output_tokens += output_tokens;
        self.run_reported_input_tokens += input_tokens;
        self.run_reported_output_tokens += output_tokens;
    }

    pub(crate) fn record_usage_remainder(&mut self, input_tokens: u32, output_tokens: u32) {
        let input_tokens = input_tokens as usize;
        let output_tokens = output_tokens as usize;
        self.total_input_tokens += input_tokens.saturating_sub(self.run_reported_input_tokens);
        self.total_output_tokens += output_tokens.saturating_sub(self.run_reported_output_tokens);
    }

    pub(crate) fn select_story_root(&mut self, root_idx: usize) {
        self.open_story_root(root_idx);
    }

    fn open_story_root(&mut self, root_idx: usize) {
        if root_idx >= self.news_article_row_count() {
            return;
        }
        if let Some(url) = self.selected_news_article_url(root_idx) {
            let open_url = match resolve_article_url_for_open(&url) {
                Ok(url) => url,
                Err(err) => {
                    self.set_status_message(&format!("Could not open article: {err}"));
                    return;
                }
            };
            self.story_menu_focused = true;
            self.story_menu_highlight = root_idx;
            let opener = Arc::clone(&self.browser_opener);
            if let Err(err) = opener(&open_url) {
                self.set_status_message(&format!("Could not open article: {err}"));
            }
        }
    }
}

fn workflow_fallback_label(request: &StartWorkflow) -> String {
    if let Some(query) = request.exact_query() {
        query.to_string()
    } else {
        request.ticker.clone()
    }
}

fn workflow_result_display_term(result: &WorkflowResult) -> Option<String> {
    result.display_term.clone().or_else(|| {
        result.company_identity.as_ref().and_then(|identity| {
            let display = identity.company_name.trim();
            (!display.is_empty() && !display.eq_ignore_ascii_case(identity.ticker.trim()))
                .then(|| display.to_string())
        })
    })
}

fn workflow_result_label(result: &WorkflowResult, fallback_label: &str) -> String {
    workflow_result_display_term(result).unwrap_or_else(|| fallback_label.to_string())
}

fn cached_news_article_count(result: &WorkflowResult) -> usize {
    result
        .cached_news
        .as_ref()
        .map(|cached| cached.articles.len())
        .unwrap_or(0)
}
