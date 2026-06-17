//! Shared UI helpers for layout math, text formatting, and launching external helpers.

use std::{path::Path, process::Command, time::Duration};

use reqwest::Client;

use crate::data::{
    APP_USER_AGENT, is_google_news_rss_article_url, resolve_embedded_google_news_url,
    resolve_google_news_url,
};

const ARTICLE_LINK_RESOLVE_TIMEOUT_SECS: u64 = 4;

pub(crate) fn resolve_article_url_for_open(url: &str) -> std::result::Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("article URL is empty".to_string());
    }

    if is_google_news_rss_article_url(url)
        && let Some(article_url) = resolve_google_news_url_sync(url)
    {
        return Ok(article_url);
    }

    Ok(url.to_string())
}

fn resolve_google_news_url_sync(url: &str) -> Option<String> {
    if let Some(article_url) = resolve_embedded_google_news_url(url) {
        return Some(article_url);
    }

    let url = url.to_string();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(async {
            let client = Client::builder()
                .timeout(Duration::from_secs(ARTICLE_LINK_RESOLVE_TIMEOUT_SECS))
                .user_agent(APP_USER_AGENT)
                .build()
                .ok()?;
            resolve_google_news_url(&client, &url).await
        })
    })
    .join()
    .ok()
    .flatten()
}

pub(crate) fn open_url_in_browser(url: &str) -> std::result::Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(url);
        command
    };

    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err("unsupported platform".to_string());

    command
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("could not open {url}: {err}"))
}

fn system_editor_command(path: &Path, wait: bool) -> std::result::Result<Command, String> {
    let _ = wait;
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        if wait {
            command.arg("-W");
        }
        command.args(["-a", "TextEdit"]);
        command
    };

    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");

    #[cfg(target_os = "windows")]
    let mut command = Command::new("notepad");

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err("unsupported platform".to_string());

    command.arg(path);
    Ok(command)
}

pub(crate) fn open_path_in_editor(path: &Path) -> std::result::Result<(), String> {
    system_editor_command(path, false)?
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("could not open editor for {}: {err}", path.display()))
}

pub(crate) fn open_path_in_editor_and_wait(path: &Path) -> std::result::Result<(), String> {
    let mut command = match std::env::var_os("VISUAL").or_else(|| std::env::var_os("EDITOR")) {
        Some(editor) => {
            let mut command = Command::new(editor);
            command.arg(path);
            command
        }
        None => system_editor_command(path, true)?,
    };
    command
        .status()
        .map_err(|err| format!("could not open editor for {}: {err}", path.display()))
        .and_then(|status| {
            status
                .success()
                .then_some(())
                .ok_or_else(|| format!("editor exited with status {status}"))
        })
}

pub(crate) fn normalize_view_lines(mut lines: Vec<String>, count: usize) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    if lines.len() > count {
        lines = lines.split_off(lines.len() - count);
    }
    while lines.len() < count {
        lines.push(String::new());
    }
    lines
}

pub(crate) fn window_view_lines(mut lines: Vec<String>, count: usize, top: usize) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }

    if lines.is_empty() {
        return vec![String::new(); count];
    }

    let max_top = lines.len().saturating_sub(count);
    let top = top.min(max_top);
    let end = (top + count).min(lines.len());
    lines = lines[top..end].to_vec();
    while lines.len() < count {
        lines.push(String::new());
    }
    lines
}

pub(crate) fn center_target_top(target: usize, height: usize, total_lines: usize) -> usize {
    if height == 0 {
        return 0;
    }
    let max_top = total_lines.saturating_sub(height);
    target.saturating_sub(height / 2).min(max_top)
}

pub(crate) fn format_int_with_commas(value: usize) -> String {
    let s = value.to_string();
    let mut out = String::new();
    for (idx, ch) in s.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

pub(crate) fn truncate_footer_text(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let runes = text.chars().collect::<Vec<_>>();
    if runes.len() <= width {
        return text.to_string();
    }
    if width <= 3 {
        return runes.into_iter().take(width).collect();
    }
    runes.into_iter().take(width - 3).collect::<String>() + "..."
}

pub(crate) fn truncate_with_ellipsis(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    text.chars()
        .take(width - 1)
        .chain(std::iter::once('…'))
        .collect()
}
