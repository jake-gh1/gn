//! `AppModel` definition plus the runtime/bootstrap wiring for the terminal UI.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use crate::config::{RuntimeConfig, debug_log};
use crate::data::{CachedNews, DefaultSourceStore, SearchHistoryEntry};
use crate::llm::SwitchableLlmClient;
use crate::workflow::{DefaultWorkflowEngine, WorkflowEngine};
use anyhow::Result;
use chrono::{DateTime, Local};
#[cfg(unix)]
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend, widgets::Paragraph};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime as TokioRuntime;

use crate::ui::*;

const UI_FRAME_INTERVAL_MS: u64 = 33;

fn model_matches(model: &crate::config::ModelConfig, requested: &str) -> bool {
    model.model_id == requested
        || model.label == requested
        || model
            .label
            .rsplit_once('/')
            .is_some_and(|(_, id)| id == requested)
}

fn active_model_index(runtime: &RuntimeConfig, fallback: usize) -> usize {
    if runtime.models.is_empty() {
        return 0;
    }
    runtime
        .active_model
        .as_deref()
        .and_then(|requested| {
            runtime
                .models
                .iter()
                .position(|model| model_matches(model, requested))
        })
        .unwrap_or_else(|| fallback.min(runtime.models.len() - 1))
}

/// Central UI state container. It owns both the rendered transcript state and workflow runtime.
pub struct AppModel {
    pub(crate) runtime: RuntimeConfig,
    pub(crate) palette: UiPalette,
    pub(crate) workflow_runtime: Option<TokioRuntime>,
    pub(crate) workflow_engine: Option<Arc<dyn WorkflowEngine>>,
    pub(crate) active_model: usize,
    pub(crate) status_message: Option<String>,
    pub(crate) workflow_events: Option<std::sync::mpsc::Receiver<WorkflowUiEvent>>,
    pub(crate) pending_search: Option<PendingSearch>,
    pub(crate) live_articles: Option<Vec<crate::data::NewsArticle>>,
    pub(crate) progress_note: Option<String>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) view_ready: bool,
    pub(crate) total_input_tokens: usize,
    pub(crate) total_output_tokens: usize,
    pub(crate) started_at: Instant,
    pub(crate) run_started_at: SystemTime,
    pub(crate) completed_elapsed: Option<Duration>,
    pub(crate) cached_news: HashMap<String, CachedNews>,
    pub(crate) new_article_keys: HashMap<String, HashSet<String>>,
    pub(crate) company_tickers: Vec<String>,
    pub(crate) company_names: Vec<String>,
    pub(crate) story_menu_highlight: usize,
    pub(crate) story_menu_focused: bool,
    pub(crate) browser_opener: BrowserOpener,
    pub(crate) config_editor: ConfigEditor,
    pub(crate) history_editor: ConfigEditor,
    pub(crate) on_model_switch: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    pub(crate) codex_auth_in_flight: bool,
    pub(crate) lazy_runtime_enabled: bool,
    pub(crate) runtime_config_path_override: Option<PathBuf>,
    pub(crate) user_config_path_override: Option<PathBuf>,
    pub(crate) runtime_config_last_path: Option<PathBuf>,
    pub(crate) runtime_config_last_modified: Option<SystemTime>,
}

impl AppModel {
    pub(crate) fn focus_debug_state(&self) -> String {
        format!(
            "story_focused={} story_highlight={} rows={}",
            self.story_menu_focused,
            self.story_menu_highlight,
            self.news_article_row_count(),
        )
    }

    pub(crate) fn log_ui_state(&self, label: &str) {
        debug_log("ui", format!("state {label} {}", self.focus_debug_state()));
    }

    pub fn new(runtime: RuntimeConfig) -> Self {
        // The bare constructor keeps workflow runtime creation lazy so the UI can still start even
        // if background resources are not needed yet.
        let palette = UiPalette::standard();
        let active_model = active_model_index(&runtime, 0);

        Self {
            runtime,
            palette,
            workflow_runtime: None,
            workflow_engine: None,
            active_model,
            status_message: None,
            workflow_events: None,
            pending_search: None,
            live_articles: None,
            progress_note: None,
            width: 80,
            height: 14,
            view_ready: false,
            total_input_tokens: 0,
            total_output_tokens: 0,
            started_at: Instant::now(),
            run_started_at: SystemTime::now(),
            completed_elapsed: None,
            cached_news: HashMap::new(),
            new_article_keys: HashMap::new(),
            company_tickers: Vec::new(),
            company_names: Vec::new(),
            story_menu_highlight: 0,
            story_menu_focused: false,
            browser_opener: Arc::new(open_url_in_browser),
            config_editor: Arc::new(open_path_in_editor),
            history_editor: Arc::new(open_path_in_editor_and_wait),
            on_model_switch: None,
            codex_auth_in_flight: false,
            lazy_runtime_enabled: true,
            runtime_config_path_override: None,
            user_config_path_override: None,
            runtime_config_last_path: None,
            runtime_config_last_modified: None,
        }
    }

    pub fn with_workflow(
        runtime: RuntimeConfig,
        workflow_runtime: TokioRuntime,
        workflow_engine: Arc<dyn WorkflowEngine>,
    ) -> Self {
        // Tests and some call sites provide an already-built runtime/engine pair so startup skips
        // the lazy initialization path.
        let mut model = Self::new(runtime);
        model.workflow_runtime = Some(workflow_runtime);
        model.workflow_engine = Some(workflow_engine);
        model.lazy_runtime_enabled = false;
        model
    }

    pub fn set_active_model_by_id(&mut self, model_id: &str) -> Result<()> {
        let requested = model_id.trim();
        let Some(idx) = self
            .runtime
            .models
            .iter()
            .position(|model| model_matches(model, requested))
        else {
            anyhow::bail!("unknown model `{requested}`");
        };
        self.active_model = idx;
        if let Some(cb) = &self.on_model_switch {
            cb(idx);
        }
        Ok(())
    }

    pub fn active_model_label(&self) -> Option<String> {
        self.runtime
            .models
            .get(self.active_model)
            .map(|model| model.label.clone())
    }

    pub fn open_config_editor(&mut self) -> Result<()> {
        self.open_config_editor_from_ui()
    }

    pub fn open_history_json(&self, cache_path: &Path, export_path: &Path) -> Result<()> {
        let entries = crate::data::search_history_entries(cache_path)?;
        write_history_json(export_path, &entries)?;
        (self.history_editor)(export_path).map_err(anyhow::Error::msg)?;
        reconcile_history_json_deletions(cache_path, export_path, &entries)?;
        Ok(())
    }

    pub fn run(&mut self) -> Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        #[cfg(unix)]
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;

        loop {
            self.sync_ui_state();
            terminal.draw(|frame| {
                let area = frame.size();
                self.width = area.width as usize;
                self.height = area.height as usize;
                frame.render_widget(Paragraph::new(self.view_text_styled()), area);
            })?;

            if !self.view_ready {
                self.view_ready = true;
                continue;
            }

            if event::poll(Duration::from_millis(UI_FRAME_INTERVAL_MS))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && self.handle_key(key)?
            {
                break;
            }
        }

        disable_raw_mode()?;
        #[cfg(unix)]
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;
        Ok(())
    }

    pub(crate) fn ensure_runtime_ready(&mut self) -> Result<bool> {
        if self.workflow_runtime.is_some() && self.workflow_engine.is_some() {
            return Ok(true);
        }
        if !self.lazy_runtime_enabled {
            return Ok(false);
        }
        if self.runtime.models.is_empty() {
            anyhow::bail!("no models configured; run `gn --config`");
        }

        debug_log(
            "ui",
            format!(
                "lazy runtime init models={} active={}",
                self.runtime.models.len(),
                self.active_model
            ),
        );

        let workflow_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?;
        let llm = Arc::new(SwitchableLlmClient::new(&self.runtime.models)?);
        llm.set_active(self.active_model);
        let sources = Arc::new(DefaultSourceStore::new());
        let workflow = Arc::new(DefaultWorkflowEngine::new(
            Arc::clone(&llm) as Arc<dyn crate::llm::LlmClient>,
            sources,
            self.runtime.allowlist.clone(),
        ));

        self.workflow_runtime = Some(workflow_runtime);
        self.workflow_engine = Some(workflow);
        self.on_model_switch = Some(Arc::new(move |idx| llm.set_active(idx)));

        Ok(true)
    }

    pub(crate) fn runtime_config_path(&self) -> PathBuf {
        self.runtime_config_path_override
            .clone()
            .unwrap_or_else(crate::config::runtime_config_path)
    }

    pub(crate) fn user_config_path(&self) -> PathBuf {
        self.user_config_path_override
            .clone()
            .unwrap_or_else(crate::config::user_config_path)
    }

    pub(crate) fn apply_runtime_config(&mut self, runtime: RuntimeConfig) {
        self.runtime = runtime;
        self.workflow_runtime = None;
        self.workflow_engine = None;
        self.on_model_switch = None;
        self.codex_auth_in_flight = false;
        self.active_model = active_model_index(&self.runtime, 0);
    }
}

pub(crate) fn runtime_config_modified_at(path: &std::path::Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

#[derive(Deserialize, Serialize)]
struct SearchHistoryJson {
    history: Vec<SearchHistoryJsonEntry>,
}

#[derive(Deserialize, Serialize)]
struct SearchHistoryJsonEntry {
    #[serde(default)]
    query: String,
    search: String,
    articles: usize,
    searched_at: String,
    model: Option<String>,
}

fn write_history_json(path: &Path, entries: &[SearchHistoryEntry]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let history = SearchHistoryJson {
        history: entries
            .iter()
            .map(|entry| {
                let searched_at: DateTime<Local> = entry.searched_at.into();
                SearchHistoryJsonEntry {
                    query: entry.search_term.clone(),
                    search: entry.display_term.clone(),
                    articles: entry.articles_found,
                    searched_at: searched_at.to_rfc3339(),
                    model: entry.model.clone(),
                }
            })
            .collect(),
    };
    fs::write(path, serde_json::to_string_pretty(&history)? + "\n")?;
    Ok(())
}

fn reconcile_history_json_deletions(
    cache_path: &Path,
    export_path: &Path,
    original_entries: &[SearchHistoryEntry],
) -> Result<()> {
    let edited: SearchHistoryJson = serde_json::from_str(&fs::read_to_string(export_path)?)?;
    let mut retained_queries = edited
        .history
        .iter()
        .filter_map(|entry| {
            let query = entry.query.trim();
            (!query.is_empty()).then(|| query.to_lowercase())
        })
        .collect::<HashSet<_>>();

    for edited_entry in edited
        .history
        .iter()
        .filter(|entry| entry.query.trim().is_empty())
    {
        for original in original_entries.iter().filter(|original| {
            original
                .display_term
                .eq_ignore_ascii_case(edited_entry.search.trim())
        }) {
            retained_queries.insert(original.search_term.trim().to_lowercase());
        }
    }

    for original in original_entries {
        if !retained_queries.contains(&original.search_term.trim().to_lowercase()) {
            crate::data::delete_search_history_term(cache_path, &original.search_term)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use crate::config::{ModelConfig, RuntimeConfig};
    use crate::data::{
        ArticleAnalysisUpdate, CachedNews, CompanyIdentity, NewsArticle, SearchHistoryRecord,
        SourceStore, record_search_history_run, search_history_entries,
    };
    use crate::ui::{AppModel, WorkflowUiEvent};
    use crate::workflow::{WorkflowProgress, WorkflowResult};
    use ratatui::style::Style;

    fn temp_path(name: &str, ext: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "gn-{name}-{}-{}.{ext}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    fn cleanup_paths(cache_path: &std::path::Path, export_path: &std::path::Path) {
        let _ = fs::remove_file(cache_path);
        let _ = fs::remove_file(cache_path.with_extension("sqlite3-shm"));
        let _ = fs::remove_file(cache_path.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(export_path);
    }

    fn model_config(provider: &str, model_id: &str) -> ModelConfig {
        ModelConfig {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            api_key: String::new(),
            label: format!("{provider}/{model_id}"),
        }
    }

    fn two_model_runtime() -> RuntimeConfig {
        RuntimeConfig {
            models: vec![
                model_config("openai", "gpt-5.4"),
                model_config("google", "gemini-3.1-flash-lite"),
            ],
            ..RuntimeConfig::default()
        }
    }

    fn news_article(
        title: &str,
        publisher: &str,
        label: &str,
        published_at: SystemTime,
    ) -> NewsArticle {
        NewsArticle {
            title: title.to_string(),
            publisher: publisher.to_string(),
            label: label.to_string(),
            url: format!(
                "https://example.com/{}",
                title.to_ascii_lowercase().replace(' ', "-")
            ),
            published_at: Some(published_at),
            ..NewsArticle::default()
        }
    }

    #[test]
    fn new_uses_persisted_active_model_label() {
        let model = AppModel::new(RuntimeConfig {
            active_model: Some("google/gemini-3.1-flash-lite".to_string()),
            ..two_model_runtime()
        });

        assert_eq!(model.current_model_label(), "gemini-3.1-flash-lite");
        assert_eq!(
            model.active_model_label(),
            Some("google/gemini-3.1-flash-lite".to_string())
        );
    }

    #[test]
    fn apply_runtime_config_without_preference_resets_active_model() {
        let mut model = AppModel::new(two_model_runtime());
        model
            .set_active_model_by_id("google/gemini-3.1-flash-lite")
            .expect("set active");

        model.apply_runtime_config(two_model_runtime());

        assert_eq!(
            model.active_model_label(),
            Some("openai/gpt-5.4".to_string())
        );
    }

    #[test]
    fn footer_prints_run_timestamp() {
        let mut model = AppModel::new(RuntimeConfig::default());
        model.run_started_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        model.completed_elapsed = Some(Duration::from_secs(83));
        model.total_input_tokens = 1_408;
        model.total_output_tokens = 207;

        let line = model.footer_plain_line();
        let timestamp = line
            .rsplit(" · @")
            .next()
            .expect("timestamp field");

        assert!(!line.contains("Model:"));
        assert!(!line.contains("Tokens:"));
        assert!(!line.contains("Timestamp:"));
        assert!(line.contains(" · 1,408 → 207 · @"));
        assert_eq!(timestamp.chars().nth(2), Some(':'));
        assert_eq!(timestamp.chars().nth(5), Some(':'));
        assert_eq!(timestamp.chars().count(), 8);
        assert!(!timestamp.contains('T'));
        assert!(!line.contains(" · Ran: "));
        assert!(!line.contains(" · Time: "));
    }

    #[test]
    fn open_history_json_writes_export_and_opens_file() {
        let cache_path = temp_path("history-json-cache", "sqlite3");
        let export_path = temp_path("history-export", "json");
        record_search_history_run(
            &cache_path,
            &[SearchHistoryRecord {
                search_term: "NVDA".to_string(),
                display_term: "Nvidia Corp".to_string(),
                context_key: "company:NVDA".to_string(),
                articles_found: 12,
            }],
            Some("gpt-5.5"),
        )
        .expect("record run");

        let opened_path = Arc::new(Mutex::new(None));
        let opened_path_for_editor = Arc::clone(&opened_path);
        let mut model = AppModel::new(RuntimeConfig::default());
        model.history_editor = Arc::new(move |path| {
            *opened_path_for_editor.lock().expect("opened path") = Some(path.to_path_buf());
            Ok(())
        });

        model
            .open_history_json(&cache_path, &export_path)
            .expect("open history json");

        assert_eq!(
            *opened_path.lock().expect("opened path"),
            Some(export_path.clone())
        );
        let body = fs::read_to_string(&export_path).expect("read history export");
        assert!(body.contains(r#""history""#));
        assert!(body.contains(r#""query": "NVDA""#));
        assert!(body.contains(r#""search": "Nvidia Corp""#));
        assert!(body.contains(r#""articles": 12"#));
        assert!(body.contains(r#""searched_at""#));
        assert!(body.contains(r#""model": "gpt-5.5""#));

        cleanup_paths(&cache_path, &export_path);
    }

    #[tokio::test]
    async fn open_history_json_deletes_only_removed_mixed_search_context() {
        let cache_path = temp_path("history-json-mixed-cache", "sqlite3");
        let export_path = temp_path("history-json-mixed-export", "json");
        record_search_history_run(
            &cache_path,
            &[
                SearchHistoryRecord {
                    search_term: "Compute".to_string(),
                    display_term: "Compute".to_string(),
                    context_key: "query:compute".to_string(),
                    articles_found: 1,
                },
                SearchHistoryRecord {
                    search_term: "glw".to_string(),
                    display_term: "Corning Inc".to_string(),
                    context_key: "company:GLW".to_string(),
                    articles_found: 1,
                },
            ],
            Some("gpt-5.5"),
        )
        .expect("record mixed search");
        let query_article =
            news_article("compute capacity grows", "Reuters", "", SystemTime::now())
                .cache_identity();
        let company_article =
            news_article("Corning expands fiber", "Bloomberg", "", SystemTime::now())
                .cache_identity();
        let store = crate::data::DefaultSourceStore::with_article_cache_path(cache_path.clone());
        for (context, article) in [
            ("query:compute", &query_article),
            ("company:GLW", &company_article),
        ] {
            store
                .store_article_analysis(
                    context,
                    &[ArticleAnalysisUpdate {
                        article: article.clone(),
                        relevant: Some(true),
                        label: None,
                    }],
                )
                .await
                .expect("store analysis");
        }

        let mut model = AppModel::new(RuntimeConfig::default());
        model.history_editor = Arc::new(|path| {
            let mut history: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(path).map_err(|err| err.to_string())?)
                    .map_err(|err| err.to_string())?;
            history["history"]
                .as_array_mut()
                .expect("history array")
                .retain(|entry| entry["query"] == "glw");
            fs::write(
                path,
                serde_json::to_string_pretty(&history).map_err(|err| err.to_string())?
                    + "
",
            )
            .map_err(|err| err.to_string())
        });

        model
            .open_history_json(&cache_path, &export_path)
            .expect("edit mixed history");

        let entries = search_history_entries(&cache_path).expect("load reconciled history");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].search_term, "glw");
        assert!(
            store
                .load_article_analysis("query:compute", std::slice::from_ref(&query_article))
                .await
                .expect("load deleted query analysis")
                .is_empty()
        );
        assert!(
            store
                .load_article_analysis("company:GLW", std::slice::from_ref(&company_article))
                .await
                .expect("load retained company analysis")
                .contains_key(&company_article.article_key)
        );

        drop(store);
        cleanup_paths(&cache_path, &export_path);
    }

    #[test]
    fn new_article_keys_mark_only_top_row_when_new_articles_found() {
        let mut model = AppModel::new(RuntimeConfig::default());
        model.width = 120;
        model.company_tickers = vec!["NVDA".to_string()];
        let older = SystemTime::now() - Duration::from_secs(13 * 3_600);
        let fresh = news_article(
            "Nvidia introduces new AI inference chips",
            "CNBC",
            "AI Chip Competition",
            SystemTime::now(),
        );
        let export_rules = news_article(
            "Nvidia faces AI chip export rules",
            "Reuters",
            "AI Chip Rules",
            older,
        );
        let new_keys = vec![fresh.cache_key(), export_rules.cache_key()];

        model.remember_workflow_cache_snapshot(&workflow_result_with_articles(
            vec![fresh, export_rules],
            Some(new_keys),
        ));

        let lines = model.news_article_table_lines().body;
        let fresh_line = lines
            .iter()
            .find(|line| line.contains("inference chips"))
            .expect("fresh article row");
        let stale_line = lines
            .iter()
            .find(|line| line.contains("export rules"))
            .expect("stale article row");
        assert!(fresh_line.starts_with("•  1m ago"));
        assert!(stale_line.starts_with("   13hrs ago"));

        let styled_fresh = model
            .style_news_article_table_line(fresh_line, None)
            .expect("fresh style");
        let styled_stale = model
            .style_news_article_table_line(stale_line, None)
            .expect("stale style");
        assert_eq!(styled_fresh.spans[0].content.as_ref(), "•");
        assert_eq!(
            styled_fresh.spans[0].style,
            Style::default().fg(ratatui::style::Color::White)
        );
        assert_eq!(
            styled_fresh.spans[1].style,
            Style::default().fg(model.palette.dim)
        );
        assert_eq!(
            styled_stale.spans[0].style,
            Style::default().fg(model.palette.dim)
        );
    }

    #[test]
    fn workflow_new_article_keys_mark_top_row_even_when_stale() {
        let mut model = AppModel::new(RuntimeConfig::default());
        model.width = 120;
        model.company_tickers = vec!["NVDA".to_string()];
        let article = news_article(
            "Nvidia introduces new AI inference chips",
            "CNBC",
            "AI Chip Competition",
            SystemTime::now() - Duration::from_secs(13 * 3_600),
        );
        let article_key = article.cache_key();

        model.remember_workflow_cache_snapshot(&workflow_result_with_articles(
            vec![article],
            Some(vec![article_key]),
        ));

        let line = model
            .news_article_table_line_for_index(0)
            .expect("news table row");
        assert!(line.starts_with("•  13hrs ago"));
        let styled = model
            .style_news_article_table_line(&line, None)
            .expect("style");
        assert_eq!(styled.spans[0].content.as_ref(), "•");
        assert_eq!(
            styled.spans[0].style,
            Style::default().fg(ratatui::style::Color::White)
        );
    }

    #[test]
    fn workflow_event_polling_renders_live_rows_and_cleans_up_after_error() {
        let mut model = AppModel::new(RuntimeConfig::default());
        model.width = 120;
        model.company_tickers = vec!["NVDA".to_string()];
        model.company_names = vec!["NVDA".to_string()];
        let (sender, receiver) = std::sync::mpsc::channel();
        model.workflow_events = Some(receiver);
        model.pending_search = Some(crate::ui::PendingSearch {
            search_term: "NVDA".to_string(),
            fallback_label: "NVDA".to_string(),
        });

        sender
            .send(WorkflowUiEvent::Progress(WorkflowProgress::Snapshot(vec![
                news_article(
                    "Nvidia faces AI chip export rules",
                    "Reuters",
                    "",
                    SystemTime::now(),
                ),
            ])))
            .expect("send snapshot");
        sender
            .send(WorkflowUiEvent::Progress(WorkflowProgress::Stage(
                "Filtering 1 headlines…".to_string(),
            )))
            .expect("send stage");
        model.poll_workflow_events();

        let line = model
            .news_article_table_line_for_index(0)
            .expect("live row");
        assert!(line.contains("Nvidia faces AI chip export rules"));
        assert!(model.footer_plain_line().contains("Filtering 1 headlines…"));

        sender
            .send(WorkflowUiEvent::Done(Err("network failed".to_string())))
            .expect("send failure");
        model.poll_workflow_events();

        assert!(model.live_articles.is_none());
        assert!(model.workflow_events.is_none());
        assert!(model.pending_search.is_none());
        assert!(model.progress_note.is_none());
        assert_eq!(
            model.status_message.as_deref(),
            Some("Workflow failed: network failed")
        );
    }

    fn workflow_result_with_articles(
        articles: Vec<NewsArticle>,
        new_article_keys: Option<Vec<String>>,
    ) -> WorkflowResult {
        WorkflowResult {
            company_identity: Some(CompanyIdentity {
                ticker: "NVDA".to_string(),
                company_name: "Nvidia Corp".to_string(),
            }),
            cached_news: Some(CachedNews {
                articles,
                ..CachedNews::default()
            }),
            new_article_keys,
            ..WorkflowResult::default()
        }
    }
}
