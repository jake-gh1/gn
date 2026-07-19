//! Workflow-wide shared helpers.

use std::collections::HashSet;

use crate::data::{
    CachedNews, CompanyIdentity, NewsArticle, normalize_url, split_news_query_terms,
};
use crate::llm::TokenUsage;
use crate::workflow::WorkflowResult;
use serde::Deserialize;

const NEWS_COVERAGE_TITLE_SUFFIX: &str = " — News Coverage";

pub(crate) fn news_coverage_title(label: &str) -> String {
    format!("{}{}", label.trim(), NEWS_COVERAGE_TITLE_SUFFIX)
}

pub(crate) fn build_news_analysis_prompt(identity: &CompanyIdentity, items: &[String]) -> String {
    let mut body = build_news_analysis_prompt_header(identity);
    append_titles_block(&mut body, items);
    body
}

pub(crate) fn build_query_news_analysis_prompt(query: &str, items: &[String]) -> String {
    let terms = split_news_query_terms(query);
    let mut body = if terms.len() > 1 {
        let query_context = format!(
            "Search alternatives (an article may match any one):\n{}",
            terms
                .iter()
                .map(|term| format!("- {term}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        format!(
            "{query_context}\n\nFor each numbered news title, decide whether it belongs in this search and write a short label when it does.\n\nReturn ONLY JSON: {{\"items\":[{{\"id\":1,\"include\":true,\"label\":\"2-3 Word Tag\"}},{{\"id\":2,\"include\":false,\"label\":null}}]}}. Return exactly one item per title and preserve IDs.\n\nRelevance rules:\n- Use the search terms and title only; include a title when it clearly matches the intent, entity, topic, or phrase in at least one search term.\n- Exclude unrelated politics, entertainment, sports, culture, market-color, listicle, broad macro, generic update, landing/directory, video, stock quote, analyst rating, financial-statement, and non-English titles unless they directly match a search term.\n\nLabel rules:\n- For included titles, write a concrete 2-3 word Title Case tag with no trailing punctuation.\n- Never use \"News\"; avoid vague outlook, strategy, sector, brief, analysis, takeaway, or trend labels.\n- Use only the title and do not add outside facts. For excluded titles, use null.\n\nTitles:\n"
        )
    } else {
        format!(
            "Search query: {}\n\nFor each numbered news title, decide whether it belongs in this search and write a short label when it does.\n\nReturn ONLY JSON: {{\"items\":[{{\"id\":1,\"include\":true,\"label\":\"2-3 Word Tag\"}},{{\"id\":2,\"include\":false,\"label\":null}}]}}. Return exactly one item per title and preserve IDs.\n\nRelevance rules:\n- Use the query and title only; include a title when it clearly matches the query's intent, entity, topic, or phrase.\n- Exclude unrelated politics, entertainment, sports, culture, market-color, listicle, broad macro, generic update, landing/directory, video, stock quote, analyst rating, financial-statement, and non-English titles unless they directly match the query.\n\nLabel rules:\n- For included titles, write a concrete 2-3 word Title Case tag with no trailing punctuation.\n- Never use \"News\"; avoid vague outlook, strategy, sector, brief, analysis, takeaway, or trend labels.\n- Use only the title and do not add outside facts. For excluded titles, use null.\n\nTitles:\n",
            query.trim()
        )
    };
    append_titles_block(&mut body, items);
    body
}

fn append_titles_block(body: &mut String, items: &[String]) {
    for (idx, title) in items.iter().enumerate() {
        body.push_str(&format!("{}. {}\n", idx + 1, title.trim()));
    }
}

fn build_news_analysis_prompt_header(identity: &CompanyIdentity) -> String {
    format!(
        "Company context: {name} ({ticker})\n\nFor each numbered news title, decide whether it belongs in this company-specific list and write a short label when it does.\n\nReturn ONLY JSON: {{\"items\":[{{\"id\":1,\"include\":true,\"label\":\"2-3 Word Tag\"}},{{\"id\":2,\"include\":false,\"label\":null}}]}}. Return exactly one item per title and preserve IDs.\n\nRelevance rules:\n- Use the title only, except for recognizable relationships involving a brand, subsidiary, product, executive, regulator, customer, supplier, partner, or competitor of {name}.\n- Include only titles clearly about {name}, {ticker}, or a concrete business relationship affecting {name}.\n- Exclude unrelated politics, entertainment, sports, culture, market-color, listicle, broad macro, generic mover/watchlist, landing/directory, video, stock quote, analyst rating, financial-statement, and non-English titles.\n\nLabel rules:\n- For included titles, write a concrete 2-3 word Title Case tag with no trailing punctuation.\n- Do not use {name}, {ticker}, \"News\", publisher names, or vague outlook, strategy, sector, brief, analysis, takeaway, or trend labels.\n- Use only the title and do not add outside facts. For excluded titles, use null.\n\nTitles:\n",
        name = identity.company_name,
        ticker = identity.ticker
    )
}

pub(crate) fn parse_first_json_value(text: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(text.trim())
        .ok()
        .or_else(|| {
            for (idx, ch) in text.char_indices() {
                if ch != '[' && ch != '{' {
                    continue;
                }
                let mut deserializer = serde_json::Deserializer::from_str(&text[idx..]);
                if let Ok(value) = serde_json::Value::deserialize(&mut deserializer) {
                    return Some(value);
                }
            }
            None
        })
}

pub(crate) fn label_validation_tokens(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .filter(|token| token.len() > 1)
        .filter(|token| !is_label_stopword(token))
        .collect()
}

pub(crate) fn token_variants(token: &str) -> Vec<String> {
    let mut variants = vec![token.to_string()];
    if token.len() > 4 && token.ends_with("ies") {
        variants.push(format!("{}y", &token[..token.len() - 3]));
    } else if token.len() > 4 && token.ends_with("es") {
        variants.push(token[..token.len() - 2].to_string());
    } else if token.len() > 3 && token.ends_with('s') && token != "us" {
        variants.push(token[..token.len() - 1].to_string());
    }
    variants
}

fn is_label_stopword(token: &str) -> bool {
    matches!(
        token,
        "the"
            | "and"
            | "or"
            | "for"
            | "to"
            | "of"
            | "in"
            | "on"
            | "with"
            | "by"
            | "from"
            | "as"
            | "at"
            | "is"
            | "are"
            | "be"
            | "this"
            | "that"
            | "its"
            | "into"
            | "over"
            | "after"
            | "before"
            | "amid"
            | "about"
            | "than"
            | "will"
            | "may"
            | "can"
            | "could"
            | "would"
            | "should"
    )
}

pub(crate) fn is_generic_label_token(token: &str) -> bool {
    matches!(
        token,
        "stock"
            | "stocks"
            | "share"
            | "shares"
            | "market"
            | "markets"
            | "wall"
            | "street"
            | "week"
            | "ahead"
            | "focus"
            | "watch"
            | "watchlist"
            | "investor"
            | "investors"
            | "report"
            | "reports"
            | "news"
            | "today"
            | "latest"
            | "new"
            | "launch"
            | "launches"
            | "update"
            | "updates"
            | "meeting"
            | "meetings"
            | "mover"
            | "movers"
            | "scorching"
            | "strategic"
            | "strategy"
            | "industry"
            | "sector"
            | "outlook"
            | "brief"
            | "analysis"
            | "takeaway"
            | "takeaways"
    )
}

pub(crate) fn add_token_usage(total: &mut TokenUsage, usage: &TokenUsage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
}

pub(crate) fn coverage_workflow_result(
    title: String,
    company_identity: Option<CompanyIdentity>,
    cached: CachedNews,
    usage: TokenUsage,
) -> WorkflowResult {
    let source_urls = if cached.source_urls.is_empty() {
        news_source_urls(&cached.articles)
    } else {
        cached.source_urls.clone()
    };
    let answer = if cached.articles.is_empty() {
        "(No headlines found)".to_string()
    } else {
        String::new()
    };

    WorkflowResult {
        title,
        answer,
        display_term: None,
        history_entries: Vec::new(),
        company_identity,
        source_urls,
        usage,
        cached_news: Some(cached),
        new_article_keys: None,
    }
}

pub(crate) fn news_source_urls(articles: &[NewsArticle]) -> Vec<String> {
    let mut seen = HashSet::<String>::new();
    let mut source_urls = Vec::new();
    for article in articles {
        let raw_url = article.url.trim();
        if raw_url.is_empty() {
            continue;
        }
        if seen.insert(normalize_url(raw_url)) {
            source_urls.push(raw_url.to_string());
        }
    }
    source_urls
}
