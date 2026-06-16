## Runtime Config

**[gn](https://github.com/jake-gh1/gn)** needs **at least one configured model**. Run `gn` (or `gn --config`) to create or edit `runtime.toml`; new files are seeded from a built-in template.

**Supported providers**: **[Anthropic](https://docs.anthropic.com/)**, **[Cohere](https://docs.cohere.com/)**, **[Google](https://ai.google.dev/)**, **[Mistral](https://docs.mistral.ai/)**, **[Nvidia](https://docs.nvidia.com/ai-enterprise/)**, **[Ollama](https://ollama.com/)**, **[OpenAI](https://platform.openai.com/docs/)**, **[OpenRouter](https://openrouter.ai/docs/)**, **[Codex](https://developers.openai.com/codex/)** (local auth).

```toml
# Allowlist narrows news results.
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
```

The active model selection (`gn --model`) is saved separately, not in `runtime.toml`.

On macOS, `runtime.toml` is saved in `~/Library/Application Support/gn`; on Windows, it is saved in `%APPDATA%\gn`, falling back to `%LOCALAPPDATA%\gn` and then `%USERPROFILE%\AppData\Roaming\gn`.

For workflow behavior, see **[workflow.md](workflow.md)**.
