use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn debug_log(component: &str, message: impl AsRef<str>) {
    // Logging is completely opt-in. When enabled, writes are serialized so background tasks can
    // emit debug lines without interleaving file output.
    let Some(path) = debug_log_path() else {
        return;
    };

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let _guard = debug_log_lock().lock().expect("debug log lock");
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let _ = writeln!(
        file,
        "[{}.{}] {:<8} {}",
        now.as_secs(),
        format_args!("{:03}", now.subsec_millis()),
        component,
        message.as_ref()
    );
}

pub fn model_log(provider: &str, model: &str, prompt: &str, text: &str, tokens: (u32, u32)) {
    let Some(path) = model_log_path() else {
        return;
    };

    let _guard = debug_log_lock().lock().expect("debug log lock");
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let _ = writeln!(
        file,
        "===== [{}.{:03}] provider={provider} model={model} =====\n--- prompt ({} chars) ---\n{prompt}\n--- response ({} chars, in_tokens={}, out_tokens={}) ---\n{text}\n",
        now.as_secs(),
        now.subsec_millis(),
        prompt.len(),
        text.len(),
        tokens.0,
        tokens.1,
    );
}

pub fn debug_log_destination() -> Option<PathBuf> {
    debug_log_path()
}

fn debug_log_path() -> Option<PathBuf> {
    static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    LOG_PATH
        .get_or_init(|| resolve_log_path(std::env::var("GN_LOG"), "gn.log"))
        .clone()
}

fn model_log_path() -> Option<PathBuf> {
    static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    LOG_PATH
        .get_or_init(|| resolve_log_path(std::env::var("GN_MODEL_LOG"), "model_responses.log"))
        .clone()
}

fn resolve_log_path(
    raw: std::result::Result<String, std::env::VarError>,
    default_file_name: &str,
) -> Option<PathBuf> {
    match raw {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() || matches!(trimmed, "0" | "false" | "FALSE" | "no" | "NO") {
                None
            } else if matches!(trimmed, "1" | "true" | "TRUE" | "yes" | "YES") {
                Some(default_project_log_path(default_file_name))
            } else {
                Some(PathBuf::from(trimmed))
            }
        }
        Err(_) => None,
    }
}

fn default_project_log_path(file_name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(file_name)
}

fn debug_log_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}
