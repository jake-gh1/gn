// vin-query

use anyhow::Result;
use gn::{
    config::{
        debug_log, debug_log_destination, load_runtime_config_with_user_config, news_cache_path,
        runtime_config_path, runtime_dir, save_active_model_selection, user_config_path,
    },
    ui::AppModel,
};

fn main() -> Result<()> {
    let cli = CliOptions::parse(std::env::args().skip(1))?;

    // Bootstrap config first so logging, model selection, and source settings are all available
    // before entering the TUI.
    let runtime_config_path = runtime_config_path();
    let user_config_path = user_config_path();
    let runtime = load_runtime_config_with_user_config(
        runtime_config_path.clone(),
        user_config_path.clone(),
    )?;
    if let Some(path) = debug_log_destination() {
        debug_log("main", format!("logging enabled path={}", path.display()));
    }
    debug_log(
        "main",
        format!(
            "startup runtime_config_path={} models={}",
            runtime_config_path.display(),
            runtime
                .models
                .iter()
                .map(|m| m.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
    if cli.history {
        AppModel::new(runtime)
            .open_history_json(&news_cache_path(), &runtime_dir().join("history.json"))?;
        return Ok(());
    }
    if cli.models {
        for model in &runtime.models {
            println!("{}", model.label);
        }
        return Ok(());
    }

    let mut model = AppModel::new(runtime);
    if let Some(Some(model_id)) = cli.model.as_ref() {
        model.set_active_model_by_id(model_id)?;
        if let Some(label) = model.active_model_label() {
            save_active_model_selection(&user_config_path, &label)?;
        }
    }
    if cli.model.is_some() {
        println!("{}", model.current_model_label());
        return Ok(());
    }
    // Bare `gn` opens the runtime config so first-run users land somewhere actionable.
    if cli.config || cli.query.is_none() {
        model.open_config_editor()?;
        return Ok(());
    }
    if let Some(query) = cli.query.as_deref() {
        model.launch_search_after_preparing(query)?;
    }
    debug_log("main", "entering ui loop");
    model.run()?;
    debug_log("main", "ui loop exited");
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CliOptions {
    model: Option<Option<String>>,
    models: bool,
    config: bool,
    history: bool,
    query: Option<String>,
}

impl CliOptions {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut options = Self::default();
        let mut query_parts = Vec::new();
        let mut args = args.into_iter().peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--config" => options.config = true,
                "--history" => options.history = true,
                "--model" => {
                    let model = args.next_if(|value| !value.starts_with("--"));
                    options.model = Some(model);
                }
                "--models" => options.models = true,
                _ if arg.starts_with("--model=") => {
                    let model = arg["--model=".len()..].trim();
                    if model.is_empty() {
                        anyhow::bail!("--model= requires a model id");
                    }
                    options.model = Some(Some(model.to_string()));
                }
                _ if arg.starts_with("--") => {
                    anyhow::bail!("unknown flag `{arg}`");
                }
                _ => query_parts.push(arg),
            }
        }
        if !query_parts.is_empty() {
            options.query = Some(query_parts.join(" "));
        }
        if options.config
            && (options.query.is_some()
                || options.model.is_some()
                || options.models
                || options.history)
        {
            anyhow::bail!(
                "--config cannot be combined with --model, --models, --history, or a query"
            );
        }
        if options.model.is_some() && options.query.is_some() {
            anyhow::bail!("--model cannot be combined with a query");
        }
        if options.models && (options.model.is_some() || options.query.is_some() || options.history)
        {
            anyhow::bail!("--models cannot be combined with --model, --history, or a query");
        }
        if options.history && (options.model.is_some() || options.query.is_some()) {
            anyhow::bail!("--history cannot be combined with --model or a query");
        }
        Ok(options)
    }
}

#[cfg(test)]
mod tests {
    use super::CliOptions;

    #[test]
    fn cli_options_parse_bare_model_flag() {
        let parsed = CliOptions::parse(["--model".to_string()]).expect("parse");

        assert_eq!(parsed.model, Some(None));
        assert_eq!(parsed.query, None);
    }

    #[test]
    fn cli_options_preserve_slash_separated_query_terms() {
        let parsed = CliOptions::parse([
            "msft".to_string(),
            "/".to_string(),
            "aapl".to_string(),
            "/".to_string(),
            "nvda".to_string(),
        ])
        .expect("parse");

        assert_eq!(parsed.query.as_deref(), Some("msft / aapl / nvda"));
    }

    #[test]
    fn cli_options_reject_help_flag() {
        let err = CliOptions::parse(["--help".to_string()]).expect_err("parse");

        assert_eq!(err.to_string(), "unknown flag `--help`");
    }
}
