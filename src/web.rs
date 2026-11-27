//! The `web_search` tool: the model looks up what it doesn't know, or
//! checks what it does, such as a library's current API or an error
//! message, through the framework's web search.
//!
//! The framework's `WebSearchAgent` asks Smartloop's hosted `/v1/websearch`
//! for links (signed in, within the month's allowance), then fetches the
//! pages as markdown. Docent calls it in process, the agent being built in,
//! and hands the pages to the model as the tool's result. Web search is on
//! by default: the tool is offered on every turn with tools, and signed
//! out, it says so and the model goes on without it.
//!
//! The `web_fetch` tool reads one page the model already has the URL of,
//! one the user gave or a search turned up, through the framework's
//! `FetchAgent` (HTML or PDF to markdown), so it needn't reach for curl.

use smartloop::agents::fetch_agent::FetchAgent;
use smartloop::agents::web_search_agent::WebSearchAgent;
use smartloop::search::providers::SearchProviderError;

/// The tool's name, as the model calls it.
pub const TOOL: &str = "web_search";
/// Pages handed back for one search.
const TOP: usize = 3;
/// Links asked for: spares for pages that won't load. Only fetched as
/// needed, so spares cost nothing unless a top page fails.
const CANDIDATES: usize = 10;

/// The fetch tool's name, as the model calls it.
pub const FETCH_TOOL: &str = "web_fetch";
/// Characters of a page one `web_fetch` call gives back; the rest is had
/// with `start`.
const PAGE: usize = 12_000;

/// The tool, as offered to the model.
pub fn tool() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": TOOL,
            "description": "Search the web and read the top pages, for what the project's files and your own knowledge don't tell or may have out of date: a library's or tool's current API and version, an error message, release notes, documentation. Returns the pages' text with their URLs; cite the URLs you use.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "What to search for, as you'd type it into a search engine"},
                },
                "required": ["query"],
            },
        },
    })
}

/// The fetch tool, as offered to the model.
pub fn fetch_tool() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": FETCH_TOOL,
            "description": format!("Read one web page or PDF as markdown, by its URL: a page the user gave, or one `{}` turned up that you need more of. Use it rather than curl or wget. Gives up to {} characters; a longer page says where to go on from, with `start`.", TOOL, PAGE),
            "parameters": {
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The page's full http(s) URL"},
                    "start": {"type": "integer", "description": "Character to start from, for the rest of a long page; 0 by default"},
                },
                "required": ["url"],
            },
        },
    })
}

/// The URL and start a `web_fetch` call of `arguments` asks for, or what
/// to tell the model.
pub fn fetch_args(arguments: &str) -> Result<(String, usize), String> {
    let arguments: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|e| format!("Error: the arguments aren't valid JSON ({}); give {{\"url\": \"https://...\"}}.", e))?;
    let url = arguments["url"].as_str().map(str::trim).filter(|u| !u.is_empty());
    let Some(url) = url else {
        return Err("Error: no `url` given; give {\"url\": \"https://...\"}.".to_string());
    };
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(format!("Error: `{}` isn't an http(s) URL; give the page's full address.", url));
    }
    let start = arguments["start"].as_u64().unwrap_or(0) as usize;
    Ok((url.to_string(), start))
}

/// Read the page at `url` from character `start`: what goes back to the
/// model. Blocking, a network round trip: run it off the UI thread. Never
/// fails: why the page couldn't be read is the result.
pub fn fetch(url: &str, start: usize) -> String {
    // One character past the page, to tell whether there's more.
    let text = FetchAgent::new().fetch(url, PAGE + 1, start, false);
    page(url, start, &text)
}

/// What goes back for `text`, read from `url` at `start` with one
/// character past the page asked for.
fn page(url: &str, start: usize, text: &str) -> String {
    if text.trim().is_empty() {
        return if start > 0 {
            format!("Nothing more at {} from character {}: that was the whole page.", url, start)
        } else {
            format!("Couldn't read {}: it failed to load, refused the request, or has no text. Go on without it.", url)
        };
    }
    let body: String = text.chars().take(PAGE).collect();
    let more = text.chars().count() > PAGE;
    let mut out = format!("{}\n\n", url);
    out.push_str(body.trim_end());
    if more {
        out.push_str(&format!(
            "\n\n[The page goes on: call `{}` with \"start\": {} for the next part.]",
            FETCH_TOOL,
            start + PAGE
        ));
    }
    out
}

/// The query a call of `arguments` asks for, or what to tell the model.
pub fn query(arguments: &str) -> Result<String, String> {
    let arguments: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|e| format!("Error: the arguments aren't valid JSON ({}); give {{\"query\": \"...\"}}.", e))?;
    arguments["query"]
        .as_str()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "Error: no `query` given; give {\"query\": \"...\"}.".to_string())
}

/// Search the web for `query` and read the top pages: what goes back to
/// the model. Blocking, a network round trip a page: run it off the UI
/// thread. Never fails: why it couldn't search is the result, so the model
/// goes on without it.
pub fn search(query: &str, said: impl Fn(&str) + Send + Sync + 'static) -> String {
    let agent = WebSearchAgent::new(None, TOP, CANDIDATES, None);
    // What the agent says of each step, a page at a time: "Reading docs.rs".
    let progress: smartloop::agents::web_search_agent::ProgressCallback = std::sync::Arc::new(move |_, _, message| said(message));
    let links = match agent.fetch_links(query, CANDIDATES) {
        Ok(links) => links,
        Err(SearchProviderError::NoSearchCredentials(_)) => {
            return "Web search isn't available: not signed in to Smartloop (/login signs in). Go on without it."
                .to_string();
        }
        Err(SearchProviderError::SearchAuthError(_)) => {
            return "Web search isn't available for this account, or this month's searches are used up. Go on \
                    without it."
                .to_string();
        }
        Err(e) => return format!("The web search failed ({}). Go on without it, or try another query.", e),
    };
    if links.is_empty() {
        return format!("No web results for \"{}\"; try other words.", query);
    }
    // No rerank model, so fetching more than TOP usable pages would only
    // read pages that get thrown away: stop at TOP, skipping failed links.
    agent.fetch_top(query, &links, TOP, TOP, Some(&progress))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_gives_its_query() {
        assert_eq!(query(r#"{"query": "  tokio 1.40 select macro "}"#), Ok("tokio 1.40 select macro".into()));
        assert!(query(r#"{"query": ""}"#).unwrap_err().contains("no `query` given"));
        assert!(query("{oops").unwrap_err().contains("aren't valid JSON"));
        assert_eq!(tool()["function"]["name"], TOOL);
    }

    #[test]
    fn a_fetch_gives_its_url_and_start() {
        assert_eq!(fetch_args(r#"{"url": " https://docs.rs/tokio "}"#), Ok(("https://docs.rs/tokio".into(), 0)));
        assert_eq!(fetch_args(r#"{"url": "https://a.b/c", "start": 12000}"#), Ok(("https://a.b/c".into(), 12000)));
        assert!(fetch_args(r#"{"url": "file:///etc/passwd"}"#).unwrap_err().contains("isn't an http(s) URL"));
        assert!(fetch_args(r#"{}"#).unwrap_err().contains("no `url` given"));
        assert_eq!(fetch_tool()["function"]["name"], FETCH_TOOL);
    }

    #[test]
    fn a_long_page_says_where_it_goes_on() {
        let long = "q".repeat(PAGE + 1);
        let shown = page("https://a.b", 0, &long);
        assert!(shown.contains(&format!("\"start\": {}", PAGE)), "{}", shown);
        assert_eq!(shown.matches('q').count(), PAGE);
        assert!(!page("https://a.b", 0, "short page").contains("goes on"));
        assert!(page("https://a.b", 0, "  ").starts_with("Couldn't read"));
        assert!(page("https://a.b", PAGE, "").contains("whole page"));
    }
}

#[cfg(test)]
mod live {
    /// Against the hosted endpoint, signed in: `cargo test live_search -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_search() {
        let text = super::search("rust tokio select macro documentation", |step| println!("{step}"));
        println!("{}", text.chars().take(1500).collect::<String>());
        assert!(!text.is_empty());
    }
}
