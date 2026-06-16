use anyhow::{Result, bail};

/// Static provider registry entry used for validation and default endpoint lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub key: &'static str,
    pub base_url: &'static str,
    pub base_url_env_key: Option<&'static str>,
    pub fixed_api_key: Option<&'static str>,
}

// Provider keys in the runtime config are validated against this registry before models are
// admitted into the runtime config.
const PROVIDER_CONFIGS: &[ProviderConfig] = &[
    ProviderConfig {
        key: "codex",
        base_url: "",
        base_url_env_key: None,
        fixed_api_key: None,
    },
    ProviderConfig {
        key: "anthropic",
        base_url: "https://api.anthropic.com/v1",
        base_url_env_key: Some("ANTHROPIC_BASE_URL"),
        fixed_api_key: None,
    },
    ProviderConfig {
        key: "cohere",
        base_url: "https://api.cohere.ai/compatibility/v1",
        base_url_env_key: Some("COHERE_BASE_URL"),
        fixed_api_key: None,
    },
    ProviderConfig {
        key: "google",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        base_url_env_key: Some("GOOGLE_BASE_URL"),
        fixed_api_key: None,
    },
    ProviderConfig {
        key: "mistral",
        base_url: "https://api.mistral.ai/v1",
        base_url_env_key: Some("MISTRAL_BASE_URL"),
        fixed_api_key: None,
    },
    ProviderConfig {
        key: "nvidia",
        base_url: "https://integrate.api.nvidia.com/v1",
        base_url_env_key: Some("NVIDIA_BASE_URL"),
        fixed_api_key: None,
    },
    ProviderConfig {
        key: "ollama",
        base_url: "http://localhost:11434/v1",
        base_url_env_key: Some("OLLAMA_BASE_URL"),
        fixed_api_key: Some("ollama"),
    },
    ProviderConfig {
        key: "openai",
        base_url: "https://api.openai.com/v1",
        base_url_env_key: None,
        fixed_api_key: None,
    },
    ProviderConfig {
        key: "openrouter",
        base_url: "https://openrouter.ai/api/v1",
        base_url_env_key: None,
        fixed_api_key: None,
    },
];

pub fn provider_config_for(provider: &str) -> Option<ProviderConfig> {
    let key = provider.trim().to_ascii_lowercase();
    PROVIDER_CONFIGS.iter().find(|cfg| cfg.key == key).cloned()
}

fn is_known_provider(provider: &str) -> bool {
    provider_config_for(provider).is_some()
}

fn supported_runtime_providers() -> Vec<String> {
    let mut providers = PROVIDER_CONFIGS
        .iter()
        .map(|cfg| cfg.key.to_string())
        .collect::<Vec<_>>();
    providers.sort();
    providers
}

pub fn validate_runtime_provider(provider: &str) -> Result<()> {
    if is_known_provider(provider) {
        Ok(())
    } else {
        bail!("{}", unsupported_provider_error(provider))
    }
}

fn unsupported_provider_error(provider: &str) -> String {
    format!(
        "unsupported provider \"{}\"; supported runtime providers: {}",
        provider,
        supported_runtime_providers().join(", ")
    )
}
