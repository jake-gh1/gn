//! Codex HTTP client backed by the local Codex auth cache.

use std::path::PathBuf;

use crate::config::debug_log;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use crate::llm::{CompletionRequest, CompletionResponse, LlmClient, TokenUsage};

const CODEX_BACKEND_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const CODEX_HEADER_BETA_VALUE: &str = "responses=experimental";
const CODEX_HEADER_ORIGINATOR: &str = "codex_cli_rs";

const CODEX_INSTRUCTIONS: &str = "You are Codex, based on GPT-5. You are running as a coding agent in the Codex CLI on a user's computer.";

/// Cached auth state persisted in Codex's `auth.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CodexSession {
    #[serde(default)]
    tokens: CodexSessionToken,
}

/// Token bundle persisted for ChatGPT-backed Codex sessions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CodexSessionToken {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    access_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    account_id: String,
}

#[derive(Debug, Deserialize)]
struct CodexStreamEvent {
    #[serde(rename = "type", default)]
    event_type: String,
    #[serde(default)]
    delta: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    response: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CodexCompletedResponse {
    usage: Option<CodexUsage>,
    #[serde(default)]
    output: Vec<CodexOutput>,
}

#[derive(Debug, Deserialize)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct CodexOutput {
    #[serde(default)]
    content: Vec<CodexContent>,
}

#[derive(Debug, Deserialize)]
struct CodexContent {
    #[serde(rename = "type", default)]
    content_type: String,
    #[serde(default)]
    text: String,
}

fn default_codex_auth_path() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("CODEX_HOME") {
        let custom = custom.trim();
        if !custom.is_empty() {
            return Ok(PathBuf::from(custom).join("auth.json"));
        }
    }
    let home = dirs_home().context("resolve home dir")?;
    Ok(home.join(".codex").join("auth.json"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
}

struct AuthenticatedCodexSession {
    session: CodexSession,
    credential: String,
}

fn load_authenticated_codex_session() -> Result<AuthenticatedCodexSession> {
    let path = default_codex_auth_path()?;
    let body = std::fs::read_to_string(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            anyhow!("codex is not logged in")
        } else {
            anyhow!("read auth cache: {err}")
        }
    })?;

    let mut session: CodexSession = serde_json::from_str(&body).context("decode auth cache")?;
    if session.tokens.account_id.is_empty() {
        session.tokens.account_id = extract_codex_account_id(&session.tokens.access_token);
    }

    let credential = session.tokens.access_token.trim().to_string();
    if credential.is_empty() {
        anyhow::bail!("codex is not logged in");
    }
    Ok(AuthenticatedCodexSession {
        session,
        credential,
    })
}

fn extract_codex_account_id(token: &str) -> String {
    if token.trim().is_empty() {
        return String::new();
    }
    let Ok(claims) = decode_codex_token_claims(token) else {
        return String::new();
    };
    let Some(auth) = claims
        .get("https://api.openai.com/auth")
        .and_then(|v| v.as_object())
    else {
        return String::new();
    };
    auth.get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn decode_codex_token_claims(token: &str) -> Result<serde_json::Map<String, serde_json::Value>> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        anyhow::bail!("invalid JWT");
    }
    let payload = URL_SAFE_NO_PAD
        .decode(parts[1])
        .context("decode JWT payload")?;
    let claims: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&payload).context("parse JWT payload")?;
    Ok(claims)
}

/// LLM client that talks to Codex's backend API using the saved auth cache.
pub struct CodexClient {
    provider: String,
    model_id: String,
}

impl CodexClient {
    pub fn new(provider: &str, model_id: &str) -> Self {
        Self {
            provider: provider.to_string(),
            model_id: model_id.trim().to_string(),
        }
    }

    fn build_request(&self, prompt: &str) -> serde_json::Value {
        serde_json::json!({
            "model": self.model_id,
            "store": false,
            "stream": true,
            "instructions": CODEX_INSTRUCTIONS,
            "input": [
                {
                    "role": "user",
                    "content": prompt
                }
            ],
            "reasoning": {
                "effort": "low"
            },
            "text": {
                "verbosity": "medium"
            }
        })
    }

    async fn do_request(
        &self,
        auth: &AuthenticatedCodexSession,
        body_bytes: &[u8],
    ) -> Result<CompletionResponse> {
        let started_at = std::time::Instant::now();
        debug_log(
            "llm",
            format!(
                "codex.chat start model={} url={}",
                self.model_id, CODEX_BACKEND_URL
            ),
        );

        let client = reqwest::Client::new();
        let mut req = client
            .post(CODEX_BACKEND_URL)
            .header("Authorization", format!("Bearer {}", auth.credential))
            .header("originator", CODEX_HEADER_ORIGINATOR)
            .header("Accept", "text/event-stream")
            .header("Content-Type", "application/json");

        if !auth.session.tokens.account_id.trim().is_empty() {
            req = req.header("chatgpt-account-id", &auth.session.tokens.account_id);
        }
        req = req.header("OpenAI-Beta", CODEX_HEADER_BETA_VALUE);

        let resp = req
            .body(body_bytes.to_vec())
            .send()
            .await
            .context("send Codex request")?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            let body = resp.text().await.unwrap_or_default();
            let msg = body.trim();
            let msg = if msg.is_empty() {
                status.as_str().to_string()
            } else {
                msg.to_string()
            };
            anyhow::bail!("codex request unauthorized: {msg}");
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Codex API error (status {status}): {}", body.trim());
        }

        let body = resp.text().await.context("read Codex response body")?;
        let result = parse_codex_sse(&body)?;

        debug_log(
            "llm",
            format!(
                "codex.chat done after_ms={} in_tokens={} out_tokens={} text_chars={}",
                started_at.elapsed().as_millis(),
                result.usage.input_tokens,
                result.usage.output_tokens,
                result.text.len()
            ),
        );

        Ok(result)
    }
}

#[async_trait]
impl LlmClient for CodexClient {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        let auth = load_authenticated_codex_session()?;
        let body_bytes = serde_json::to_vec(&self.build_request(&req.prompt))
            .context("marshal Codex request")?;
        self.do_request(&auth, &body_bytes).await
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn provider(&self) -> &str {
        &self.provider
    }
}

fn parse_codex_sse(body: &str) -> Result<CompletionResponse> {
    // The backend returns an SSE transcript. Collect deltas when present, but also fall back to
    // the final completed payload so non-delta responses still parse cleanly.
    let mut result_text = String::new();
    let mut input_tokens: u32 = 0;
    let mut output_tokens: u32 = 0;
    let mut total_tokens: u32 = 0;
    let mut seen_delta = false;
    let mut data_lines: Vec<String> = Vec::new();

    let mut flush_event = |lines: &mut Vec<String>| -> Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let data = lines.join("\n");
        lines.clear();
        let event: CodexStreamEvent = serde_json::from_str(&data).context("decode Codex SSE")?;

        match event.event_type.as_str() {
            "response.output_text.delta" => {
                if !event.delta.is_empty() {
                    result_text.push_str(&event.delta);
                    seen_delta = true;
                }
            }
            "response.completed" => {
                if let Some(resp) = event.response {
                    let completed: CodexCompletedResponse =
                        serde_json::from_value(resp).context("decode completed response")?;
                    if let Some(usage) = completed.usage {
                        input_tokens = usage.input_tokens;
                        output_tokens = usage.output_tokens;
                        total_tokens = usage.total_tokens;
                    }
                    if !seen_delta {
                        result_text = completed
                            .output
                            .into_iter()
                            .flat_map(|item| item.content)
                            .filter(|item| item.content_type == "output_text")
                            .map(|item| item.text)
                            .collect::<Vec<_>>()
                            .join("");
                    }
                }
            }
            "response.output_text.done" => {
                if !seen_delta && !event.text.is_empty() {
                    result_text.push_str(&event.text);
                }
            }
            _ => {}
        }
        Ok(())
    };

    for line in body.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            flush_event(&mut data_lines)?;
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let payload = rest.trim_start();
            if payload == "[DONE]" {
                flush_event(&mut data_lines)?;
                break;
            }
            data_lines.push(payload.to_string());
        }
    }
    flush_event(&mut data_lines)?;

    Ok(CompletionResponse {
        text: result_text.trim().to_string(),
        usage: TokenUsage {
            input_tokens,
            output_tokens,
            total_tokens,
        },
    })
}
