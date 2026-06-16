//! LLM abstraction layer covering OpenAI-compatible providers plus the local Codex integration.

pub mod codex;

use crate::config::{ModelConfig, debug_log, provider_config_for};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::{Arc, Mutex};

use codex::CodexClient;

/// Token accounting normalized across the supported providers.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    #[serde(alias = "prompt_tokens")]
    pub input_tokens: u32,
    #[serde(alias = "completion_tokens")]
    pub output_tokens: u32,
    pub total_tokens: u32,
}

/// One prompt sent to the currently selected model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub prompt: String,
    #[serde(default)]
    pub json_mode: bool,
}

/// One non-streaming completion returned to the workflow layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub text: String,
    pub usage: TokenUsage,
}

/// Minimal interface the workflow layer needs from any model backend.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse>;
    fn model_id(&self) -> &str;
    fn provider(&self) -> &str;
}

// ---------------------------------------------------------------------------
// OpenAI-compatible client (for standard API-key providers)
// ---------------------------------------------------------------------------

/// Thin client for `/chat/completions`-style providers.
pub struct OpenAiCompatClient {
    provider: String,
    model_id: String,
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    disable_reasoning: bool,
}

impl OpenAiCompatClient {
    pub fn new(config: &ModelConfig) -> Result<Self> {
        let provider = provider_config_for(&config.provider)
            .ok_or_else(|| anyhow!("unsupported provider: {}", config.provider))?;
        let base_url = provider
            .base_url_env_key
            .and_then(|key| std::env::var(key).ok())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| provider.base_url.to_string());
        let api_key = provider
            .fixed_api_key
            .map(ToString::to_string)
            .unwrap_or_else(|| config.api_key.clone());
        Ok(Self {
            provider: config.provider.clone(),
            model_id: config.model_id.clone(),
            api_key,
            base_url,
            client: reqwest::Client::builder()
                .use_rustls_tls()
                .build()
                .context("build llm http client")?,
            disable_reasoning: provider.key == "ollama",
        })
    }
}

#[async_trait]
impl LlmClient for OpenAiCompatClient {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        // All supported API-key providers are normalized to the same chat-completions shape here.
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let started_at = std::time::Instant::now();
        debug_log(
            "llm",
            format!(
                "request provider={} model={} url={} prompt_chars={} json_mode={}",
                self.provider,
                self.model_id,
                url,
                req.prompt.len(),
                req.json_mode
            ),
        );
        let mut body = json!({
            "model": self.model_id,
            "messages": [
                {
                    "role": "user",
                    "content": req.prompt,
                }
            ],
            "stream": false,
            "temperature": 0.1
        });
        if self.disable_reasoning {
            body["reasoning_effort"] = json!("none");
        }
        if req.json_mode {
            body["response_format"] = json!({ "type": "json_object" });
        }
        let log_llm_error = |stage: &str, err: reqwest::Error| {
            debug_log(
                "llm",
                format!(
                    "{stage} provider={} model={} after_ms={} err={err}",
                    self.provider,
                    self.model_id,
                    started_at.elapsed().as_millis()
                ),
            );
            err
        };

        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|err| log_llm_error("send_error", err))
            .with_context(|| format!("send llm request (provider={})", self.provider))?
            .error_for_status()
            .map_err(|err| log_llm_error("status_error", err))
            .context("llm request failed")?;
        debug_log(
            "llm",
            format!(
                "response received provider={} model={} after_ms={}",
                self.provider,
                self.model_id,
                started_at.elapsed().as_millis()
            ),
        );

        let payload: ChatCompletionsResponse =
            response.json().await.context("decode llm response")?;
        let choice = payload
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("llm returned no choices"))?;
        let text = choice.message.content.trim().to_string();
        let usage = payload.usage.unwrap_or_default();
        debug_log(
            "llm",
            format!(
                "response decoded provider={} model={} after_ms={} output_chars={} total_tokens={}",
                self.provider,
                self.model_id,
                started_at.elapsed().as_millis(),
                text.len(),
                usage.total_tokens
            ),
        );
        Ok(CompletionResponse { text, usage })
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn provider(&self) -> &str {
        &self.provider
    }
}

// ---------------------------------------------------------------------------
// Client factory
// ---------------------------------------------------------------------------

pub fn new_client(config: &ModelConfig) -> Result<Box<dyn LlmClient>> {
    let provider_lower = config.provider.trim().to_ascii_lowercase();
    match provider_lower.as_str() {
        "codex" => Ok(Box::new(CodexClient::new(
            &config.provider,
            &config.model_id,
        ))),
        _ => Ok(Box::new(OpenAiCompatClient::new(config)?)),
    }
}

// ---------------------------------------------------------------------------
// Switchable client (runtime model switching)
// ---------------------------------------------------------------------------

/// Holds the configured model list and instantiates each concrete client on first use.
pub struct SwitchableLlmClient {
    configs: Vec<ModelConfig>,
    clients: Vec<Mutex<Option<Arc<dyn LlmClient>>>>,
    active: std::sync::atomic::AtomicUsize,
}

impl SwitchableLlmClient {
    pub fn new(configs: &[ModelConfig]) -> Result<Self> {
        if configs.is_empty() {
            anyhow::bail!("no models configured");
        }
        Ok(Self {
            configs: configs.to_vec(),
            clients: std::iter::repeat_with(|| Mutex::new(None))
                .take(configs.len())
                .collect(),
            active: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub fn set_active(&self, index: usize) {
        debug_assert!(index < self.configs.len(), "model index out of bounds");
        let clamped = index.min(self.configs.len().saturating_sub(1));
        self.active
            .store(clamped, std::sync::atomic::Ordering::Relaxed);
    }

    fn active_index(&self) -> usize {
        self.active.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn current_config(&self) -> &ModelConfig {
        &self.configs[self.active_index()]
    }

    fn client_for(&self, index: usize) -> Result<Arc<dyn LlmClient>> {
        // Clients are created lazily so startup stays cheap even when several providers are
        // configured.
        let cell = self
            .clients
            .get(index)
            .ok_or_else(|| anyhow!("model index {index} out of bounds"))?;
        let mut slot = cell.lock().expect("switchable client lock");
        if let Some(existing) = slot.as_ref() {
            return Ok(Arc::clone(existing));
        }

        let client = Arc::<dyn LlmClient>::from(new_client(&self.configs[index])?);
        *slot = Some(Arc::clone(&client));
        Ok(client)
    }
}

#[async_trait]
impl LlmClient for SwitchableLlmClient {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        let client = self.client_for(self.active_index())?;
        let prompt = req.prompt.clone();
        let response = client.complete(req).await?;
        crate::config::model_log(
            client.provider(),
            client.model_id(),
            &prompt,
            &response.text,
            (response.usage.input_tokens, response.usage.output_tokens),
        );
        Ok(response)
    }

    fn model_id(&self) -> &str {
        &self.current_config().model_id
    }

    fn provider(&self) -> &str {
        &self.current_config().provider
    }
}

// ---------------------------------------------------------------------------
// Internal response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ChatCompletionsResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<TokenUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessagePayload,
}

#[derive(Debug, Deserialize)]
struct ChatMessagePayload {
    content: String,
}
