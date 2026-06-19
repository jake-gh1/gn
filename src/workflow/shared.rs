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

pub(crate) fn build_news_relevance_prompt(
    identity: &CompanyIdentity,
    items: &[String],
) -> String {
    let mut body = build_news_relevance_prompt_header(identity);
    append_titles_block(&mut body, items);
    body
}

pub(crate) fn build_query_news_relevance_prompt(query: &str, items: &[String]) -> String {
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
            "{query_context}\n\nDecide whether each news article title is relevant enough to show for this news search.\n\nRules:\n- Return ONLY a JSON object listing the numbers of the titles to include: {{\"include\":[title numbers]}}. Use [] when none qualify.\n- Use the search terms and title only; do not infer from outside knowledge.\n- Include a title when it clearly matches the intent, entity, topic, or phrase in at least one search term.\n- Exclude unrelated politics, entertainment, sports, culture, market-color, listicle, or broad macro titles unless they directly match a search term.\n- Exclude titles that only match generic words such as news, latest, update, today, market, or stocks.\n- Exclude company profile, tag, topic, landing, or directory pages, including generic titles that are only a company, product, or ticker name with no concrete news event.\n- Exclude titles that appear to be video content or link to a video, including titles beginning with \"Watch\" or explicitly labeled \"Video\".\n- Exclude stock quote, analyst estimates or ratings, financial statements, income statement, balance sheet, and cash flow pages.\n- Exclude titles written in a language other than English.\n\nTitles:\n"
        )
    } else {
        format!(
            "Search query: {}\n\nDecide whether each news article title is relevant enough to show for this news query.\n\nRules:\n- Return ONLY a JSON object listing the numbers of the titles to include: {{\"include\":[title numbers]}}. Use [] when none qualify.\n- Use the search query and title only; do not infer from outside knowledge.\n- Include a title when it clearly matches the intent, entity, topic, or phrase in the search query.\n- Exclude unrelated politics, entertainment, sports, culture, market-color, listicle, or broad macro titles unless they directly match the query.\n- Exclude titles that only match generic words such as news, latest, update, today, market, or stocks.\n- Exclude company profile, tag, topic, landing, or directory pages, including generic titles that are only a company, product, or ticker name with no concrete news event.\n- Exclude titles that appear to be video content or link to a video, including titles beginning with \"Watch\" or explicitly labeled \"Video\".\n- Exclude stock quote, analyst estimates or ratings, financial statements, income statement, balance sheet, and cash flow pages.\n- Exclude titles written in a language other than English.\n\nTitles:\n",
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

fn build_news_relevance_prompt_header(identity: &CompanyIdentity) -> String {
    format!(
        "Company context: {name} ({ticker})\n\nDecide whether each news article title is relevant enough to show in a company-specific news list.\n\nRules:\n- Return ONLY a JSON object listing the numbers of the titles to include: {{\"include\":[title numbers]}}. Use [] when none qualify.\n- Use the title only; do not infer from outside knowledge unless the title names a recognizable brand, subsidiary, product, executive, regulator, customer, supplier, partner, or competitor relationship for {name}.\n- Include a title only when it is clearly about {name}, its ticker {ticker}, or a concrete business relationship affecting {name}.\n- Exclude unrelated politics, entertainment, sports, culture, market-color, listicle, or broad macro titles.\n- Exclude titles where the company is only one ticker in a generic mover/watchlist item.\n- Exclude company profile, tag, topic, landing, or directory pages, including generic titles that are only a company, product, or ticker name with no concrete news event.\n- Exclude titles that appear to be video content or link to a video, including titles beginning with \"Watch\" or explicitly labeled \"Video\".\n- Exclude stock quote, analyst estimates or ratings, financial statements, income statement, balance sheet, and cash flow pages.\n- Exclude titles written in a language other than English.\n\nTitles:\n",
        name = identity.company_name,
        ticker = identity.ticker
    )
}

pub(crate) fn parse_news_relevance_decisions(text: &str, expected_len: usize) -> Option<Vec<bool>> {
    let value = parse_first_json_value(text)?;
    let indices = json_items_from_value(&value, &["include"], serde_json::Value::as_u64)?;
    let mut decisions = vec![false; expected_len];
    for index in indices {
        // Titles are numbered from 1 in the prompt; out-of-range numbers are dropped rather than
        // failing the whole chunk.
        if (1..=expected_len as u64).contains(&index) {
            decisions[index as usize - 1] = true;
        }
    }
    Some(decisions)
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

pub(crate) fn json_items_from_value<T>(
    value: &serde_json::Value,
    container_keys: &[&str],
    parse_leaf: fn(&serde_json::Value) -> Option<T>,
) -> Option<Vec<T>> {
    match value {
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(parse_leaf(item)?);
            }
            Some(out)
        }
        serde_json::Value::Object(map) => {
            if let Some(item) = parse_leaf(value) {
                return Some(vec![item]);
            }
            container_keys
                .iter()
                .filter_map(|key| map.get(*key))
                .find_map(|item| json_items_from_value(item, container_keys, parse_leaf))
        }
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn news_relevance_decisions_parse_included_title_numbers() {
        assert_eq!(
            parse_news_relevance_decisions(r#"{"include":[1,3]}"#, 3),
            Some(vec![true, false, true])
        );
        assert_eq!(
            parse_news_relevance_decisions(r#"{"include":[]}"#, 2),
            Some(vec![false, false])
        );
        assert_eq!(
            parse_news_relevance_decisions(r#"{"include":[2,7,0]}"#, 2),
            Some(vec![false, true])
        );
        assert_eq!(
            parse_news_relevance_decisions(r#"[1,2]"#, 2),
            Some(vec![true, true])
        );
        assert!(parse_news_relevance_decisions(r#"{"include":[true,false]}"#, 2).is_none());
        assert!(parse_news_relevance_decisions(r#"{"items":[{"include":true}]}"#, 1).is_none());
    }
}
