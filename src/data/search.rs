//! Google News RSS retrieval (with Bing News RSS fallback) and headline normalization helpers.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, SystemTime},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use quick_xml::de::from_str as from_xml_str;
use reqwest::{Client, header};
use serde_json::Value;

use crate::config::AllowlistEntry;
use crate::data::{
    ARTICLE_WINDOW_DAYS, BING_NEWS_RSS, DEFAULT_HTTP_TIMEOUT_SECS, GOOGLE_NEWS_RSS, NewsArticle,
    RssChannel, SearchResult, compare_news_articles, normalize_url,
};

fn build_rss_url(query: &str) -> String {
    let encoded = urlencoding::encode(query);
    format!("{GOOGLE_NEWS_RSS}?q={encoded}&hl=en-US&gl=US&ceid=US:en")
}

fn build_bing_rss_url(query: &str) -> String {
    let encoded = urlencoding::encode(query);
    format!("{BING_NEWS_RSS}?q={encoded}&format=rss")
}

const GOOGLE_NEWS_BATCHEXECUTE_URL: &str =
    "https://news.google.com/_/DotsSplashUi/data/batchexecute";

fn parse_rss_pub_date(raw: &str) -> Option<SystemTime> {
    let timestamp = chrono::DateTime::parse_from_rfc2822(raw)
        .map(|dt| dt.timestamp())
        .ok()
        .filter(|timestamp| *timestamp >= 0)
        .or_else(|| {
            [
                "%a, %d %b %Y %H:%M:%S %Z",
                "%a, %d %b %Y %H:%M:%S %z",
                "%d %b %y %H:%M %Z",
                "%d %b %y %H:%M %z",
            ]
            .into_iter()
            .find_map(|layout| {
                chrono::DateTime::parse_from_str(raw, layout)
                    .map(|dt| dt.timestamp())
                    .ok()
                    .filter(|timestamp| *timestamp >= 0)
            })
        })?;

    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp as u64))
}

pub(crate) async fn resolve_google_news_url(client: &Client, url: &str) -> Option<String> {
    if !is_google_news_rss_article_url(url) {
        return None;
    }
    if let Some(article_url) = resolve_embedded_google_news_url(url) {
        return Some(article_url);
    }
    let payload = fetch_google_news_resolve_payload(client, url).await?;
    resolve_google_news_payload(client, &payload).await
}

pub(crate) fn is_google_news_rss_article_url(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("https://news.google.com/rss/articles/")
        || url.starts_with("http://news.google.com/rss/articles/")
}

fn google_news_article_token(url: &str) -> Option<&str> {
    let path = url.split_once('?').map(|(path, _)| path).unwrap_or(url);
    path.rsplit_once('/').map(|(_, token)| token.trim())
}

pub(crate) fn resolve_embedded_google_news_url(url: &str) -> Option<String> {
    if !is_google_news_rss_article_url(url) {
        return None;
    }
    decode_embedded_google_news_url(url)
}

fn decode_embedded_google_news_url(url: &str) -> Option<String> {
    let token = google_news_article_token(url)?;
    let decoded = URL_SAFE_NO_PAD.decode(token).ok()?;
    find_embedded_http_url(&decoded)
}

fn find_embedded_http_url(bytes: &[u8]) -> Option<String> {
    for prefix in [b"https://".as_slice(), b"http://".as_slice()] {
        let Some(start) = bytes
            .windows(prefix.len())
            .position(|window| window == prefix)
        else {
            continue;
        };
        let end = bytes[start..]
            .iter()
            .position(|byte| {
                byte.is_ascii_whitespace() || matches!(byte, b'\0' | b'"' | b'\'' | b'<' | b'>')
            })
            .map(|idx| start + idx)
            .unwrap_or(bytes.len());
        let url = String::from_utf8_lossy(&bytes[start..end])
            .trim()
            .to_string();
        if !url.is_empty() {
            return Some(url);
        }
    }
    None
}

async fn fetch_google_news_resolve_payload(client: &Client, url: &str) -> Option<String> {
    let response = client
        .get(url)
        .header(header::USER_AGENT, "Mozilla/5.0")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.text().await.ok()?;
    let data_p = extract_google_news_data_p(&body)?;
    google_news_resolve_payload_from_data_p(&data_p)
}

fn extract_google_news_data_p(body: &str) -> Option<String> {
    let marker = "data-p=\"";
    let start = body.find(marker)? + marker.len();
    let end = body[start..].find('"')? + start;
    Some(html_attr_unescape(&body[start..end]))
}

fn html_attr_unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn google_news_resolve_payload_from_data_p(data_p: &str) -> Option<String> {
    let json = data_p.replacen("%.@.", "[\"garturlreq\",", 1);
    let values = serde_json::from_str::<Vec<Value>>(&json).ok()?;
    if values.len() < 6 {
        return None;
    }

    let mut payload = values[..values.len() - 6].to_vec();
    payload.extend_from_slice(&values[values.len() - 2..]);
    serde_json::to_string(&payload).ok()
}

async fn resolve_google_news_payload(client: &Client, payload: &str) -> Option<String> {
    let rpc_call = Value::Array(vec![
        Value::String("Fbv4je".to_string()),
        Value::String(payload.to_string()),
        Value::String("null".to_string()),
        Value::String("generic".to_string()),
    ]);
    let request_body = match serde_json::to_string(&vec![vec![rpc_call]]) {
        Ok(body) => body,
        Err(_) => return None,
    };

    let response = client
        .post(GOOGLE_NEWS_BATCHEXECUTE_URL)
        .header(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded;charset=UTF-8",
        )
        .header(header::USER_AGENT, "Mozilla/5.0")
        .form(&[("f.req", request_body)])
        .send()
        .await;
    let Ok(response) = response else {
        return None;
    };
    if !response.status().is_success() {
        return None;
    }
    let Ok(body) = response.text().await else {
        return None;
    };

    parse_google_news_batchexecute_response(&body)
        .into_iter()
        .next()
        .flatten()
}

fn parse_google_news_batchexecute_response(body: &str) -> Vec<Option<String>> {
    let body = body
        .trim_start()
        .strip_prefix(")]}'")
        .unwrap_or(body)
        .trim_start();
    let Ok(rows) = serde_json::from_str::<Vec<Value>>(body) else {
        return Vec::new();
    };

    rows.into_iter()
        .filter_map(|row| {
            let row = row.as_array()?;
            if row.first()?.as_str()? != "wrb.fr" || row.get(1)?.as_str()? != "Fbv4je" {
                return None;
            }
            let result = row.get(2)?.as_str()?;
            let result = serde_json::from_str::<Vec<Value>>(result).ok()?;
            let article_url = result.get(1)?.as_str()?.to_string();
            Some((!article_url.trim().is_empty()).then_some(article_url))
        })
        .collect()
}

async fn fetch_rss(query: &str) -> Vec<SearchResult> {
    // RSS fetching is best-effort and intentionally quiet: network/XML failures just yield an
    // empty result set for that query.
    let client = match Client::builder()
        .timeout(Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECS))
        .build()
    {
        Ok(client) => client,
        Err(_) => return Vec::new(),
    };
    let results = fetch_rss_feed(&client, &build_rss_url(query)).await;
    if !results.is_empty() {
        return results;
    }
    fetch_rss_feed(&client, &build_bing_rss_url(query)).await
}

async fn fetch_rss_feed(client: &Client, rss_url: &str) -> Vec<SearchResult> {
    let response = match client.get(rss_url).send().await {
        Ok(response) => response,
        Err(_) => return Vec::new(),
    };
    if !response.status().is_success() {
        return Vec::new();
    }
    let body = match response.text().await {
        Ok(body) => body,
        Err(_) => return Vec::new(),
    };
    let feed = match from_xml_str::<RssChannel>(&body) {
        Ok(feed) => feed,
        Err(_) => return Vec::new(),
    };

    let max_age = Duration::from_secs(ARTICLE_WINDOW_DAYS * 24 * 60 * 60);
    let now = SystemTime::now();
    let mut results = Vec::new();
    let mut feed_seen_urls = HashSet::<String>::new();
    for item in feed.channel.items {
        let url = unwrap_bing_news_redirect(item.link.trim());
        if url.is_empty() {
            continue;
        }
        let normalized = normalize_url(&url);
        if !feed_seen_urls.insert(normalized) {
            continue;
        }
        let published_at = parse_rss_pub_date(item.pub_date.trim());
        if !is_published_within_age_window(published_at, now, max_age) {
            continue;
        }
        let source_name = {
            let source = item.source.trim();
            if source.is_empty() {
                "Unknown".to_string()
            } else {
                source.to_string()
            }
        };
        let article_title = item.title.trim().to_string();
        results.push(SearchResult {
            source_name,
            article_title,
            url,
            published_at,
        });
    }
    results
}

/// Bing News RSS links point at a `bing.com/news/apiclick.aspx` redirect with the publisher URL
/// in the `url` query parameter.
fn unwrap_bing_news_redirect(url: &str) -> String {
    let stripped = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.");
    if !stripped.starts_with("bing.com/news/apiclick.aspx") {
        return url.to_string();
    }
    url.split_once('?')
        .map(|(_, query)| query)
        .unwrap_or_default()
        .split('&')
        .find_map(|pair| {
            let value = pair.strip_prefix("url=")?;
            let decoded = urlencoding::decode(value).ok()?;
            let decoded = decoded.trim();
            decoded.starts_with("http").then(|| decoded.to_string())
        })
        .unwrap_or_else(|| url.to_string())
}

fn is_published_within_age_window(
    published_at: Option<SystemTime>,
    now: SystemTime,
    max_age: Duration,
) -> bool {
    let Some(published_at) = published_at else {
        return true;
    };
    now.duration_since(published_at)
        .map(|age| age <= max_age)
        .unwrap_or(true)
}

fn search_result_dedupe_key(result: &SearchResult) -> String {
    let normalized = normalize_url(&result.url);
    let title_key = title_publisher_key(&result.article_title, &result.source_name);
    if title_key.is_empty() {
        normalized
    } else {
        format!("{normalized}:{title_key}")
    }
}

/// Searches the allowlist, falling back to the exact query when no allowlist is configured.
pub async fn search_all_sites(query: &str, sources: &[AllowlistEntry]) -> Vec<SearchResult> {
    search_news(query, sources, sources.is_empty()).await
}

/// Searches the exact query plus allowlist-specific queries.
pub async fn search_exact_and_allowlisted_sites(
    query: &str,
    sources: &[AllowlistEntry],
) -> Vec<SearchResult> {
    let mut seen_urls = HashSet::<String>::new();
    let mut all_results = Vec::new();

    for term in split_news_query_terms(query) {
        for result in search_news(&term, sources, true).await {
            if seen_urls.insert(search_result_dedupe_key(&result)) {
                all_results.push(result);
            }
        }
    }

    sort_search_results(&mut all_results);
    all_results
}

pub(crate) fn split_news_query_terms(query: &str) -> Vec<String> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }

    let delimiters = query
        .char_indices()
        .filter_map(|(idx, ch)| {
            if ch != '/' {
                return None;
            }
            let previous = query[..idx].chars().next_back();
            let next = query[idx + ch.len_utf8()..].chars().next();
            (previous.is_some_and(char::is_whitespace) && next.is_some_and(char::is_whitespace))
                .then_some(idx)
        })
        .collect::<Vec<_>>();
    if delimiters.is_empty() {
        return vec![query.to_string()];
    }

    let mut terms = Vec::with_capacity(delimiters.len() + 1);
    let mut start = 0;
    for delimiter in delimiters {
        let term = query[start..delimiter].trim();
        if term.is_empty() {
            return vec![query.to_string()];
        }
        terms.push(term.to_string());
        start = delimiter + 1;
    }
    let term = query[start..].trim();
    if term.is_empty() {
        return vec![query.to_string()];
    }
    terms.push(term.to_string());
    terms
}

async fn search_news(
    query: &str,
    sources: &[AllowlistEntry],
    include_exact_query: bool,
) -> Vec<SearchResult> {
    let mut seen_urls = HashSet::<String>::new();
    let mut all_results = Vec::new();

    if include_exact_query {
        for result in fetch_rss(query).await {
            if seen_urls.insert(search_result_dedupe_key(&result))
                && (sources.is_empty()
                    || matches_any_allowlisted_source(&result.source_name, &result.url, sources))
            {
                all_results.push(result);
            }
        }
    }

    for source in sources {
        let source_query = allowlist_query(query, source);
        for result in fetch_rss(&source_query).await {
            if !matches_allowlisted_source(&result.source_name, &result.url, source) {
                continue;
            }
            if seen_urls.insert(search_result_dedupe_key(&result)) {
                all_results.push(result);
            }
        }
    }

    sort_search_results(&mut all_results);
    all_results
}

pub fn build_company_news_search_query(company_name: &str) -> String {
    let full_name = company_name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if full_name.is_empty() {
        return String::new();
    }

    let short_name = company_search_alias(&full_name);
    if short_name.eq_ignore_ascii_case(&full_name) || short_name.is_empty() {
        return full_name;
    }

    format!(
        "({} OR {})",
        quote_search_phrase(&full_name),
        quote_search_phrase_if_needed(&short_name)
    )
}

pub fn build_news_articles(
    results: &[SearchResult],
    allowlist: &[AllowlistEntry],
) -> Vec<NewsArticle> {
    let mut articles = dedupe_news_articles(results, allowlist);
    articles.sort_by(compare_news_articles);
    articles
}

fn dedupe_news_articles(
    results: &[SearchResult],
    allowlist: &[AllowlistEntry],
) -> Vec<NewsArticle> {
    let mut best_by_key = HashMap::<String, NewsArticle>::new();
    let mut title_publisher_to_key = HashMap::<String, String>::new();
    for result in results {
        if !allowlist.is_empty()
            && !matches_any_allowlisted_source(&result.source_name, &result.url, allowlist)
        {
            continue;
        }
        let title = clean_news_title(&result.article_title, &result.source_name);
        if title.is_empty() {
            continue;
        }
        let url = result.url.trim().to_string();
        if url.is_empty() {
            continue;
        }
        if is_non_article_news_result(&title, &url) {
            continue;
        }
        let publisher = normalize_publisher(&result.source_name, &url);
        let title_publisher_key = title_publisher_key(&title, &publisher);
        let key = news_article_dedupe_key(
            &url,
            &publisher,
            &title,
            &title_publisher_key,
            &title_publisher_to_key,
        );
        let article = NewsArticle {
            title,
            label: String::new(),
            publisher,
            url,
            published_at: result.published_at,
            source_rank: allowlist_rank(result, allowlist),
        };
        match best_by_key.get(&key) {
            Some(existing) if existing.source_rank >= article.source_rank => {}
            _ => {
                if !title_publisher_key.is_empty() {
                    title_publisher_to_key.insert(title_publisher_key, key.clone());
                }
                best_by_key.insert(key, article);
            }
        }
    }
    best_by_key.into_values().collect()
}

fn news_article_dedupe_key(
    url: &str,
    publisher: &str,
    title: &str,
    title_publisher_key: &str,
    title_publisher_to_key: &HashMap<String, String>,
) -> String {
    if !title_publisher_key.is_empty()
        && let Some(existing_key) = title_publisher_to_key.get(title_publisher_key)
    {
        return existing_key.clone();
    }

    let canonical_url = normalize_url(url);
    if !canonical_url.is_empty() {
        return canonical_url;
    }

    let title_key = grouping_tokens(title).join(" ");
    format!("{}:{title_key}", publisher.to_ascii_lowercase())
}

fn title_publisher_key(title: &str, publisher: &str) -> String {
    let title_tokens = grouping_tokens(title);
    let publisher_key = publisher_match_key(publisher);
    if title_tokens.is_empty() || publisher_key.is_empty() {
        return String::new();
    }
    format!("{publisher_key}:{}", title_tokens.join(" "))
}

fn clean_news_title(title: &str, source_name: &str) -> String {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let Some((head, tail)) = title.rsplit_once(" - ") else {
        return title;
    };
    let tail = tail.trim().to_ascii_lowercase();
    let source = source_name.trim().to_ascii_lowercase();
    if tail == source
        || tail == compact_publisher_name(&source)
        || tail.ends_with(".com")
        || matches!(tail.as_str(), "reuters" | "cnbc" | "barron's")
    {
        head.trim().to_string()
    } else {
        title
    }
}

fn is_non_article_news_result(title: &str, url: &str) -> bool {
    let title = title.trim().to_ascii_lowercase();
    let normalized_url = normalize_url(url);
    let path = normalized_url
        .split_once('/')
        .map(|(_, path)| path)
        .unwrap_or("");
    if path
        .split('/')
        .any(|segment| matches!(segment, "video" | "videos"))
    {
        return true;
    }

    let non_article_title_phrases = [
        "stock overview",
        "advanced charts",
        "company & people",
        "company and people",
        "financials",
        "historical prices",
        "analyst ratings",
        "stock price",
        "quote",
        "options chain",
        "stock grades",
    ];
    if non_article_title_phrases
        .iter()
        .any(|phrase| title.contains(phrase))
    {
        return true;
    }

    let non_article_path_parts = [
        "/quote/",
        "/market-data/stocks/",
        "/stocks/",
        "/investing/stock/",
        "/companies/",
        "/equities/",
        "/securities/",
        "/chart",
        "/charts",
        "/financials",
        "/profile",
    ];
    if non_article_path_parts
        .iter()
        .any(|part| path.contains(part.trim_start_matches('/')))
    {
        return true;
    }

    false
}

fn normalize_publisher(source_name: &str, url: &str) -> String {
    let source = source_name.trim();
    if !source.is_empty() && !source.eq_ignore_ascii_case("unknown") {
        return compact_publisher_name(source);
    }
    let normalized = normalize_url(url);
    compact_publisher_name(normalized.split('/').next().unwrap_or("Unknown").trim())
}

fn compact_publisher_name(source: &str) -> String {
    let trimmed = source
        .trim()
        .trim_start_matches("www.")
        .trim_end_matches(".com")
        .trim_end_matches(".net")
        .trim_end_matches(".org");
    trimmed.to_string()
}

fn allowlist_rank(result: &SearchResult, sources: &[AllowlistEntry]) -> u32 {
    if matches_any_allowlisted_source(&result.source_name, &result.url, sources) {
        1000
    } else {
        400
    }
}

fn matches_any_allowlisted_source(
    source_name: &str,
    url: &str,
    sources: &[AllowlistEntry],
) -> bool {
    sources
        .iter()
        .any(|source| matches_allowlisted_source(source_name, url, source))
}

fn allowlist_query(query: &str, source: &AllowlistEntry) -> String {
    format!("{query} site:{}", source.domain)
}

fn company_search_alias(company_name: &str) -> String {
    let mut words = company_name
        .split_whitespace()
        .map(|word| word.trim_matches(|ch: char| matches!(ch, ',' | '.')))
        .collect::<Vec<_>>();

    while words
        .last()
        .is_some_and(|word| is_company_legal_suffix(word))
    {
        words.pop();
    }

    words.join(" ")
}

fn is_company_legal_suffix(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "inc"
            | "incorporated"
            | "corp"
            | "corporation"
            | "co"
            | "company"
            | "ltd"
            | "limited"
            | "llc"
            | "plc"
            | "lp"
            | "l.p"
            | "sa"
            | "s.a"
            | "nv"
            | "n.v"
            | "ag"
    )
}

fn quote_search_phrase(value: &str) -> String {
    format!("\"{}\"", value.replace('"', ""))
}

fn quote_search_phrase_if_needed(value: &str) -> String {
    if value.split_whitespace().nth(1).is_some() {
        quote_search_phrase(value)
    } else {
        value.replace('"', "")
    }
}

fn matches_allowlisted_source(source_name: &str, url: &str, source: &AllowlistEntry) -> bool {
    let normalized_url = normalize_url(url);
    let domain = normalized_url.split('/').next().unwrap_or_default();
    let source_domain = source
        .domain
        .trim()
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    if !domain.is_empty()
        && !source_domain.is_empty()
        && (domain == source_domain
            || domain.ends_with(&format!(".{source_domain}"))
            || source_domain.ends_with(&format!(".{domain}")))
    {
        return true;
    }

    let publisher_key = publisher_match_key(source_name);
    allowlist_publisher_keys(&source_domain)
        .iter()
        .any(|source_key| publisher_key_matches_source(&publisher_key, source_key))
}

fn allowlist_publisher_keys(source_domain: &str) -> Vec<String> {
    let mut keys = vec![publisher_match_key(
        source_domain.split('.').next().unwrap_or_default(),
    )];
    match source_domain {
        "ft.com" => keys.push("financialtimes".to_string()),
        "nytimes.com" => keys.push("newyorktimes".to_string()),
        "wsj.com" => keys.push("wallstreetjournal".to_string()),
        _ => {}
    }
    keys.sort();
    keys.dedup();
    keys.retain(|key| !key.is_empty());
    keys
}

fn publisher_key_matches_source(publisher_key: &str, weight_key: &str) -> bool {
    if publisher_key.is_empty() || weight_key.is_empty() {
        return false;
    }
    if publisher_key.len() <= 3 || weight_key.len() <= 3 {
        return publisher_key == weight_key;
    }
    publisher_key.contains(weight_key) || weight_key.contains(publisher_key)
}

fn publisher_match_key(value: &str) -> String {
    value
        .to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty() && *part != "com" && *part != "net" && *part != "org")
        .collect::<Vec<_>>()
        .join("")
}

fn grouping_tokens(title: &str) -> Vec<String> {
    let mut seen = HashSet::<String>::new();
    title
        .to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| token.len() > 2)
        .map(ToString::to_string)
        .filter(|token| seen.insert(token.clone()))
        .collect()
}

fn sort_search_results(results: &mut [SearchResult]) {
    results.sort_by_cached_key(|result| {
        (
            std::cmp::Reverse(result.published_at),
            result.source_name.clone(),
            normalize_url(&result.url),
        )
    });
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    use crate::config::AllowlistEntry;
    use crate::data::{SearchResult, build_news_articles};

    fn result(source: &str, title: &str, url: &str, age_days: u64) -> SearchResult {
        SearchResult {
            source_name: source.to_string(),
            article_title: title.to_string(),
            url: url.to_string(),
            published_at: Some(SystemTime::now() - Duration::from_secs(age_days * 86_400)),
        }
    }

    #[test]
    fn article_age_window_excludes_only_confirmed_older_articles() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(200 * 86_400);
        let max_age = Duration::from_secs(90 * 86_400);

        assert!(super::is_published_within_age_window(
            Some(now - max_age),
            now,
            max_age
        ));
        assert!(!super::is_published_within_age_window(
            Some(now - max_age - Duration::from_secs(1)),
            now,
            max_age
        ));
        assert!(super::is_published_within_age_window(None, now, max_age));
        assert!(super::is_published_within_age_window(
            Some(now + Duration::from_secs(1)),
            now,
            max_age
        ));
    }

    #[test]
    fn allowlist_exclude_newer_non_allowlisted_items() {
        let articles = build_news_articles(
            &[
                result(
                    "Low Quality Blog",
                    "AMD launches unrelated product roundup",
                    "https://low.example.com/amd-product-roundup",
                    0,
                ),
                result(
                    "Reuters",
                    "AMD server CPU revenue outlook improves",
                    "https://www.reuters.com/technology/amd-server-cpu-outlook",
                    4,
                ),
            ],
            &[AllowlistEntry {
                domain: "reuters.com".to_string(),
            }],
        );

        let publishers = articles
            .iter()
            .map(|article| article.publisher.as_str())
            .collect::<Vec<_>>();
        assert_eq!(publishers, vec!["Reuters"]);
    }

    #[test]
    fn slash_separated_queries_are_split_into_independent_terms() {
        assert_eq!(
            super::split_news_query_terms("msft / aapl / nvda"),
            vec!["msft", "aapl", "nvda"]
        );
        assert_eq!(
            super::split_news_query_terms("gpu costs  /  dram / compute"),
            vec!["gpu costs", "dram", "compute"]
        );
    }

    #[test]
    fn slashes_without_surrounding_whitespace_remain_part_of_the_query() {
        assert_eq!(
            super::split_news_query_terms("AI/ML infrastructure"),
            vec!["AI/ML infrastructure"]
        );
        assert_eq!(
            super::split_news_query_terms("https://example.com/news"),
            vec!["https://example.com/news"]
        );
    }

    #[test]
    fn company_news_query_uses_full_name_or_short_alias() {
        assert_eq!(
            super::build_company_news_search_query("Apple Inc."),
            "(\"Apple Inc.\" OR Apple)"
        );
        assert_eq!(
            super::build_company_news_search_query("Taiwan Semiconductor Manufacturing Co Ltd"),
            "(\"Taiwan Semiconductor Manufacturing Co Ltd\" OR \"Taiwan Semiconductor Manufacturing\")"
        );
    }

    #[test]
    fn allowlist_matching_accepts_publisher_or_domain() {
        let reuters = AllowlistEntry {
            domain: "reuters.com".to_string(),
        };
        let publisher_match = result(
            "Reuters",
            "Amazon opens logistics network",
            "https://news.google.com/rss/articles/example",
            1,
        );
        let domain_match = result(
            "Unknown",
            "Amazon opens logistics network",
            "https://www.reuters.com/business/retail-consumer/amazon-logistics",
            1,
        );
        let non_match = result(
            "CNBC",
            "Amazon expands same-day delivery network",
            "https://www.cnbc.com/amazon-delivery-network",
            1,
        );

        assert!(super::matches_allowlisted_source(
            &publisher_match.source_name,
            &publisher_match.url,
            &reuters
        ));
        assert!(super::matches_allowlisted_source(
            &domain_match.source_name,
            &domain_match.url,
            &reuters
        ));
        assert!(!super::matches_allowlisted_source(
            &non_match.source_name,
            &non_match.url,
            &reuters
        ));
    }

    #[test]
    fn allowlist_matching_accepts_known_publisher_aliases() {
        let financial_times = result(
            "Financial Times",
            "Anthropic weighs fundraising at new valuation",
            "https://news.google.com/rss/articles/example",
            1,
        );
        let new_york_times = result(
            "The New York Times",
            "OpenAI releases new model",
            "https://news.google.com/rss/articles/example",
            1,
        );
        let wall_street_journal = result(
            "The Wall Street Journal",
            "Amazon expands AI partnership",
            "https://news.google.com/rss/articles/example",
            1,
        );

        assert!(super::matches_allowlisted_source(
            &financial_times.source_name,
            &financial_times.url,
            &AllowlistEntry {
                domain: "ft.com".to_string(),
            }
        ));
        assert!(super::matches_allowlisted_source(
            &new_york_times.source_name,
            &new_york_times.url,
            &AllowlistEntry {
                domain: "nytimes.com".to_string(),
            }
        ));
        assert!(super::matches_allowlisted_source(
            &wall_street_journal.source_name,
            &wall_street_journal.url,
            &AllowlistEntry {
                domain: "wsj.com".to_string(),
            }
        ));
    }

    #[test]
    fn allowlist_matching_does_not_substring_match_short_domains() {
        let microsoft = result(
            "Microsoft",
            "Microsoft announces OpenAI partnership",
            "https://news.google.com/rss/articles/example",
            1,
        );

        assert!(!super::matches_allowlisted_source(
            &microsoft.source_name,
            &microsoft.url,
            &AllowlistEntry {
                domain: "ft.com".to_string(),
            }
        ));
    }

    #[test]
    fn bing_news_redirect_links_unwrap_to_publisher_urls() {
        assert_eq!(
            super::unwrap_bing_news_redirect(
                "https://www.bing.com/news/apiclick.aspx?ref=FexRss&aid=&tid=abc&url=https%3A%2F%2Fwww.reuters.com%2Ftechnology%2Famd-outlook&c=123&mkt=en-us"
            ),
            "https://www.reuters.com/technology/amd-outlook"
        );
        assert_eq!(
            super::unwrap_bing_news_redirect("https://www.reuters.com/technology/amd-outlook"),
            "https://www.reuters.com/technology/amd-outlook"
        );
    }

    #[test]
    fn bing_news_rss_items_parse_namespaced_source() {
        let body = r#"<rss xmlns:News="https://www.bing.com:443/news/search?q=amd&format=rss" version="2.0"><channel><title>amd</title><item><title>AMD outlook improves</title><link>https://www.bing.com/news/apiclick.aspx?url=https%3A%2F%2Fwww.reuters.com%2Famd</link><pubDate>Tue, 09 Jun 2026 12:00:00 GMT</pubDate><News:Source>Reuters</News:Source></item></channel></rss>"#;

        let feed = quick_xml::de::from_str::<crate::data::RssChannel>(body).unwrap();
        assert_eq!(feed.channel.items.len(), 1);
        assert_eq!(feed.channel.items[0].source, "Reuters");
    }

    #[test]
    fn google_news_local_decoder_extracts_embedded_urls() {
        let publisher_url = "https://www.reuters.com/technology/example-article";
        let token = URL_SAFE_NO_PAD.encode(format!("\u{8}\u{13}\"{publisher_url}"));
        let google_url = format!("https://news.google.com/rss/articles/{token}?oc=5");

        assert_eq!(
            super::decode_embedded_google_news_url(&google_url).as_deref(),
            Some(publisher_url)
        );
    }

    #[test]
    fn google_news_batchexecute_response_preserves_result_order() {
        let body = r#")]}'

[["wrb.fr","Fbv4je","[\"garturlres\",\"https://www.barrons.com/articles/openai\",1]",null,null,null,"generic"],["wrb.fr","Fbv4je","[\"garturlres\",\"https://www.bloomberg.com/news/articles/msft\",1]",null,null,null,"generic"],["di",24]]"#;

        assert_eq!(
            super::parse_google_news_batchexecute_response(body),
            vec![
                Some("https://www.barrons.com/articles/openai".to_string()),
                Some("https://www.bloomberg.com/news/articles/msft".to_string())
            ]
        );
    }

    #[test]
    fn google_news_data_p_extraction_unescapes_html_attributes() {
        let body = r#"<html><c-wiz data-p="%.@.[&quot;alpha&amp;beta&quot;]]"></c-wiz></html>"#;

        assert_eq!(
            super::extract_google_news_data_p(body).as_deref(),
            Some("%.@.[\"alpha&beta\"]]")
        );
    }

    #[test]
    fn news_articles_dedupe_exact_title_and_publisher_with_different_urls() {
        let articles = build_news_articles(
            &[
                result(
                    "Reuters",
                    "Brazil regulator approves deeper probe into Google's news content use",
                    "https://www.reuters.com/sustainability/society-equity/italys-media-regulator-asks-eu-investigate-google-ai-search-tools-over-publisher-2026-04-30/",
                    14,
                ),
                result(
                    "Reuters",
                    "Brazil regulator approves deeper probe into Google's news content use",
                    "https://www.reuters.com/legal/transactional/swiss-lawmakers-seek-fast-track-ubs-capital-decision-debate-begins-2026-05-04/",
                    14,
                ),
                result(
                    "Reuters",
                    "Brazil regulator approves deeper probe into Google's news content use",
                    "https://www.reuters.com/legal/litigation/merck-partner-with-google-cloud-ai-initiatives-2026-04-22/",
                    14,
                ),
            ],
            &[],
        );

        assert_eq!(articles.len(), 1);
        assert_eq!(
            articles[0].title,
            "Brazil regulator approves deeper probe into Google's news content use"
        );
    }

    #[test]
    fn news_articles_strip_google_news_title_source_suffixes() {
        let articles = build_news_articles(
            &[
                result(
                    "Barron's",
                    "OpenAI Expands Partnership With Amazon AWS After Ending Exclusivity Deal With Microsoft - Barron's",
                    "https://www.barrons.com/articles/openai-amazon-aws",
                    1,
                ),
                result(
                    "Bloomberg.com",
                    "Amazon Launches AI Productivity Software for Office Workers - Bloomberg.com",
                    "https://www.bloomberg.com/news/articles/amazon-ai-productivity",
                    1,
                ),
            ],
            &[],
        );

        let titles = articles
            .iter()
            .map(|article| article.title.as_str())
            .collect::<Vec<_>>();
        assert!(
            titles.iter().all(
                |title| !title.ends_with(" - Barron's") && !title.ends_with(" - Bloomberg.com")
            )
        );
    }

    #[test]
    fn news_articles_drop_stock_quote_pages() {
        let articles = build_news_articles(
            &[
                result(
                    "Barron's",
                    "AAPL | Apple Inc. Stock Overview (U.S.: Nasdaq)",
                    "https://www.barrons.com/market-data/stocks/aapl",
                    1,
                ),
                result(
                    "Barron's",
                    "Apple Inc. Advanced Charts | AAPL",
                    "https://www.barrons.com/market-data/stocks/aapl/charts",
                    1,
                ),
                result(
                    "Barron's",
                    "Apple Inc. Financials | AAPL",
                    "https://www.barrons.com/market-data/stocks/aapl/financials",
                    1,
                ),
                result(
                    "Reuters",
                    "Apple settles lawsuit over late Siri AI features for $250 million",
                    "https://www.reuters.com/legal/apple-siri-ai-lawsuit-settlement",
                    1,
                ),
            ],
            &[],
        );

        let titles = articles
            .iter()
            .map(|article| article.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            titles,
            vec!["Apple settles lawsuit over late Siri AI features for $250 million"]
        );
    }

    #[test]
    fn news_articles_drop_video_pages_by_path_segment() {
        let articles = build_news_articles(
            &[
                result(
                    "CNBC",
                    "Lilly's powerful new weight loss drug impresses",
                    "https://www.cnbc.com/video/2026/06/08/lillys-powerful-new-weight-loss-drug-impresses.html",
                    1,
                ),
                result(
                    "Bloomberg",
                    "Novo raises forecast as Wegovy pill fuels sales",
                    "https://www.bloomberg.com/news/videos/2026-05-06/novo-raises-forecast-video",
                    1,
                ),
                result(
                    "Reuters",
                    "Video game sales rise on strong console demand",
                    "https://www.reuters.com/technology/video-game-sales-rise",
                    1,
                ),
            ],
            &[],
        );

        let titles = articles
            .iter()
            .map(|article| article.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            titles,
            vec!["Video game sales rise on strong console demand"]
        );
    }
}
