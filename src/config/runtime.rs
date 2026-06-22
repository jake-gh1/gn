use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::validate_runtime_provider;

const DEFAULT_RUNTIME_CONFIG_TEMPLATE: &str = r#"# Allowlist narrows news results.
allowlist = [
  "theverge.com",
  "bloomberg.com",
]

# API providers require keys. Keys can be read from this file or the shell.
ANTHROPIC_API_KEY = "sk-ant-..."
COHERE_API_KEY = "..."
GOOGLE_API_KEY = "..."
MISTRAL_API_KEY = "..."
NVIDIA_API_KEY = "nvapi-..."
OPENAI_API_KEY = "sk-..."
OPENROUTER_API_KEY = "sk-or-..."

[[models]]
provider = "nvidia"
models = ["glm-5.1"]

[[models]]
provider = "ollama"
models = ["gemma4:26b-mlx", "gemma4:12b-mlx"]

# Local auth providers use local credentials instead of API keys.
# Must be installed to use.
[[models]]
provider = "codex"
models = ["gpt-5.5"]
"#;

const USER_CONFIG_FILE: &str = "config.json";
const RUNTIME_CONFIG_FILE: &str = "runtime.toml";

/// Fully resolved model entry used by the UI/runtime after reading `runtime.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelConfig {
    pub provider: String,
    pub model_id: String,
    pub api_key: String,
    pub label: String,
}

/// Allowlist entry used to constrain and rank source selection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AllowlistEntry {
    pub domain: String,
}

/// Runtime configuration assembled from gn's runtime config file.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct RuntimeConfig {
    pub models: Vec<ModelConfig>,
    pub allowlist: Vec<AllowlistEntry>,
    pub active_model: Option<String>,
}

/// User preferences stored outside the runtime config so editing providers/sources does not
/// silently carry CLI state inside the provider configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct UserConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawRuntimeConfig {
    #[serde(default)]
    models: Vec<RawModelConfig>,
    #[serde(default)]
    allowlist: Vec<String>,
    #[serde(flatten)]
    extra: HashMap<String, toml::Value>,
}

#[derive(Debug, Deserialize)]
struct RawModelConfig {
    provider: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    // Allowlist keys placed after a [[models]] table land inside that table in TOML; the
    // documented config format treats them as top-level wherever they appear.
    #[serde(default)]
    allowlist: Vec<String>,
    #[serde(flatten)]
    extra: HashMap<String, toml::Value>,
}

pub fn runtime_config_path() -> PathBuf {
    runtime_dir().join(RUNTIME_CONFIG_FILE)
}

pub fn user_config_path() -> PathBuf {
    runtime_dir().join(USER_CONFIG_FILE)
}

pub fn news_cache_path() -> PathBuf {
    runtime_dir().join("news-cache.sqlite3")
}

pub fn runtime_dir() -> PathBuf {
    default_env_dir()
}

pub fn ensure_runtime_config_file(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, DEFAULT_RUNTIME_CONFIG_TEMPLATE)?;
    Ok(true)
}

fn load_runtime_config(path: PathBuf) -> Result<RuntimeConfig> {
    if !path.exists() {
        return Ok(RuntimeConfig::default());
    }
    let content = fs::read_to_string(path)?;
    parse_explicit_runtime_config(&content)
}

fn parse_explicit_runtime_config(content: &str) -> Result<RuntimeConfig> {
    let raw: RawRuntimeConfig = toml::from_str(content).context("parse runtime config")?;

    let mut env_values = HashMap::<String, String>::new();
    collect_env_values(&mut env_values, raw.extra, "top-level runtime config")?;
    // Env keys can appear in any table and may be referenced by any model, so split them out of
    // every model before resolving API keys.
    let mut source_domains = raw.allowlist;
    let mut drafts = Vec::with_capacity(raw.models.len());
    for model in raw.models {
        collect_env_values(&mut env_values, model.extra, "model config")?;
        source_domains.extend(model.allowlist);
        drafts.push((
            model.provider,
            model.model,
            model.models,
            model.api_key,
            model.api_key_env,
        ));
    }

    let mut cfg = RuntimeConfig {
        allowlist: source_domains
            .iter()
            .map(|domain| AllowlistEntry {
                domain: normalize_source_domain(domain),
            })
            .collect(),
        ..RuntimeConfig::default()
    };

    for (index, (provider, model, models, api_key, api_key_env)) in drafts.into_iter().enumerate() {
        let provider = provider.trim().to_ascii_lowercase();
        validate_runtime_provider(&provider)?;
        let api_key = resolve_model_api_key(&provider, api_key, api_key_env, &env_values);
        let model_ids = model
            .into_iter()
            .chain(models)
            .map(|model_id| model_id.trim().to_string())
            .filter(|model_id| !model_id.is_empty())
            .collect::<Vec<_>>();
        if model_ids.is_empty() {
            bail!("model {} is missing model or models", index + 1);
        }

        for model_id in model_ids {
            cfg.models.push(ModelConfig {
                provider: provider.clone(),
                model_id: model_id.clone(),
                api_key: api_key.clone(),
                label: format!("{provider}/{model_id}"),
            });
        }
    }

    sort_and_dedup_sources(&mut cfg.allowlist);
    Ok(cfg)
}

fn collect_env_values(
    env_values: &mut HashMap<String, String>,
    extra: HashMap<String, toml::Value>,
    context: &str,
) -> Result<()> {
    for (key, value) in extra {
        if !is_env_key(&key) {
            bail!("invalid {context} key: {key}");
        }
        let toml::Value::String(value) = value else {
            bail!("invalid {context} key {key}: expected a string value");
        };
        env_values.insert(key, value);
    }
    Ok(())
}

pub fn load_runtime_config_with_user_config(
    runtime_config_path: PathBuf,
    user_config_path: PathBuf,
) -> Result<RuntimeConfig> {
    let mut cfg = load_runtime_config(runtime_config_path)?;
    let user_config = load_user_config(&user_config_path)?;
    cfg.active_model = user_config.active_model;
    Ok(cfg)
}

fn load_user_config(path: &Path) -> Result<UserConfig> {
    if !path.exists() {
        return Ok(UserConfig::default());
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("read user config at {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(UserConfig::default());
    }
    serde_json::from_str(&content)
        .with_context(|| format!("parse user config at {}", path.display()))
}

pub fn save_active_model_selection(path: &Path, model_label: &str) -> Result<()> {
    let mut cfg = load_user_config(path)?;
    cfg.active_model = Some(model_label.trim().to_string());
    save_user_config(path, &cfg)
}

fn save_user_config(path: &Path, cfg: &UserConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(cfg)?;
    fs::write(path, format!("{content}\n"))?;
    Ok(())
}

fn resolve_model_api_key(
    provider: &str,
    api_key: Option<String>,
    api_key_env: Option<String>,
    env_values: &HashMap<String, String>,
) -> String {
    api_key
        .or_else(|| {
            api_key_env
                .as_ref()
                .and_then(|key| resolve_env_key(key, env_values))
        })
        .or_else(|| {
            let default_key = default_api_key_env(provider);
            resolve_env_key(&default_key, env_values)
        })
        .unwrap_or_default()
}

fn resolve_env_key(key: &str, env_values: &HashMap<String, String>) -> Option<String> {
    env_values
        .get(key)
        .cloned()
        .or_else(|| env::var(key).ok())
        .filter(|value| !value.trim().is_empty())
}

fn default_api_key_env(provider: &str) -> String {
    format!("{}_API_KEY", provider.trim().to_ascii_uppercase())
}

fn is_env_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn sort_and_dedup_sources(sources: &mut Vec<AllowlistEntry>) {
    sources.sort_by(|a, b| a.domain.cmp(&b.domain));
    sources.dedup_by(|left, right| left.domain == right.domain);
}

fn normalize_source_domain(value: &str) -> String {
    value.trim().trim_start_matches("www.").to_ascii_lowercase()
}

fn default_env_dir() -> PathBuf {
    resolve_env_dir(
        std::env::consts::OS,
        env::var_os("HOME").map(PathBuf::from),
        env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        env::var_os("APPDATA").map(PathBuf::from),
        env::var_os("LOCALAPPDATA").map(PathBuf::from),
        env::var_os("USERPROFILE").map(PathBuf::from),
    )
    .unwrap_or_else(|| PathBuf::from(".gn"))
}

fn resolve_env_dir(
    os: &str,
    home: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    appdata: Option<PathBuf>,
    local_appdata: Option<PathBuf>,
    user_profile: Option<PathBuf>,
) -> Option<PathBuf> {
    match os {
        "macos" => home.map(|home| home.join("Library").join("Application Support").join("gn")),
        "windows" => appdata
            .or(local_appdata)
            .or_else(|| user_profile.map(|profile| profile.join("AppData").join("Roaming")))
            .map(|base| base.join("gn")),
        _ => xdg_config_home
            .map(|base| base.join("gn"))
            .or_else(|| home.map(|home| home.join(".config").join("gn"))),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    static TEST_FILE_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn write_runtime_config_file(body: &str) -> PathBuf {
        let counter = TEST_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gn-runtime-config-test-{}-{}.toml",
            std::process::id(),
            counter
        ));
        fs::write(&path, body).expect("write runtime config file");
        path
    }

    fn write_user_config_file(body: &str) -> PathBuf {
        let counter = TEST_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gn-user-config-test-{}-{}.json",
            std::process::id(),
            counter
        ));
        fs::write(&path, body).expect("write user config file");
        path
    }

    #[test]
    fn load_runtime_config_parses_explicit_models_and_sources() {
        let path = write_runtime_config_file(
            r#"OPENAI_API_KEY = "test-openai-key"
CUSTOM_OPENROUTER_KEY = "test-openrouter-key"

[[models]]
provider = "openai"
model = "gpt-5.4"

[[models]]
provider = "mistral"
model = "mistral-large-latest"
api_key = "test-mistral-key"

[[models]]
provider = "ollama"
models = ["gemma4:31b", "gemma4:26b"]

[[models]]
provider = "openrouter"
models = ["google/gemini-3.1-flash-lite"]
api_key_env = "CUSTOM_OPENROUTER_KEY"

allowlist = ["reuters.com", "www.bloomberg.com", "cnbc.com"]
"#,
        );

        let cfg = load_runtime_config(path).expect("load runtime config");

        assert_eq!(
            cfg.models
                .iter()
                .map(|model| model.label.as_str())
                .collect::<Vec<_>>(),
            vec![
                "openai/gpt-5.4",
                "mistral/mistral-large-latest",
                "ollama/gemma4:31b",
                "ollama/gemma4:26b",
                "openrouter/google/gemini-3.1-flash-lite"
            ]
        );
        assert_eq!(cfg.models[0].api_key, "test-openai-key");
        assert_eq!(cfg.models[1].api_key, "test-mistral-key");
        assert_eq!(cfg.models[4].api_key, "test-openrouter-key");
        assert_eq!(
            cfg.allowlist
                .iter()
                .map(|source| source.domain.as_str())
                .collect::<Vec<_>>(),
            vec!["bloomberg.com", "cnbc.com", "reuters.com"]
        );
    }

    #[test]
    fn load_runtime_config_rejects_legacy_preferred_sources_key() {
        let path = write_runtime_config_file(
            r#"OPENAI_API_KEY = "test-openai-key"
preferred_sources = ["www.reuters.com"]

[[models]]
provider = "openai"
model = "gpt-5.4"
"#,
        );

        let err = load_runtime_config(path)
            .expect_err("legacy preferred_sources key should be rejected")
            .to_string();

        assert!(err.contains("invalid top-level runtime config key: preferred_sources"));
    }

    #[test]
    fn load_runtime_config_overlays_user_config_active_model() {
        let runtime_config_path = write_runtime_config_file(
            r#"OPENAI_API_KEY = "test-openai-key"
GOOGLE_API_KEY = "test-google-key"

[[models]]
provider = "openai"
model = "gpt-5.4"

[[models]]
provider = "google"
model = "gemini-3.1-flash-lite"
"#,
        );
        let config_path = write_user_config_file(
            r#"{
  "active_model": "google/gemini-3.1-flash-lite"
}"#,
        );

        let cfg = load_runtime_config_with_user_config(runtime_config_path, config_path)
            .expect("load config");

        assert_eq!(
            cfg.active_model,
            Some("google/gemini-3.1-flash-lite".to_string())
        );
        assert_eq!(cfg.models.len(), 2);
    }

    #[test]
    fn save_active_model_selection_replaces_existing_config_value() {
        let path = std::env::temp_dir().join(format!(
            "gn-user-config-new-{}-{}.json",
            std::process::id(),
            TEST_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_file(&path);

        save_active_model_selection(&path, "openai/gpt-5.4").expect("save new active model");
        let cfg = load_user_config(&path).expect("load user config");
        assert_eq!(cfg.active_model, Some("openai/gpt-5.4".to_string()));

        save_active_model_selection(&path, "google/gemini-3.1-flash-lite")
            .expect("replace active model");
        let content = fs::read_to_string(&path).expect("read user config file");
        let cfg = load_user_config(&path).expect("load user config");

        assert!(content.contains(r#""active_model": "google/gemini-3.1-flash-lite""#));
        assert_eq!(
            cfg.active_model,
            Some("google/gemini-3.1-flash-lite".to_string())
        );
    }

    #[test]
    fn load_runtime_config_rejects_unknown_provider_models() {
        let path = write_runtime_config_file(
            r#"
[[models]]
provider = "something"
model = "custom-model"
"#,
        );

        let err = load_runtime_config(path)
            .expect_err("unknown provider in explicit config should be rejected")
            .to_string();

        assert!(err.contains("unsupported provider"));
        assert!(err.contains("something"));
    }

    #[test]
    fn load_runtime_config_returns_default_when_config_is_missing() {
        let path = std::env::temp_dir().join(format!(
            "gn-missing-config-{}-{}.toml",
            std::process::id(),
            TEST_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_file(&path);

        let cfg = load_runtime_config(path).expect("missing config should load default config");
        assert_eq!(cfg, RuntimeConfig::default());
    }
}
