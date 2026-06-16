//! Runtime configuration loading, provider validation, and opt-in debug logging.

mod debug;
mod providers;
mod runtime;

pub use debug::{debug_log, debug_log_destination, model_log};
pub use providers::{provider_config_for, validate_runtime_provider};
pub use runtime::{
    AllowlistEntry, ModelConfig, RuntimeConfig, ensure_runtime_config_file,
    load_runtime_config_with_user_config, news_cache_path, runtime_config_path, runtime_dir,
    save_active_model_selection, user_config_path,
};
