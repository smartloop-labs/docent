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

use smartloop::agents::web_search_agent::WebSearchAgent;
use smartloop::search::providers::SearchProviderError;

/// The tool's name, as the model calls it.
pub const TOOL: &str = "web_search";
/// Pages handed back for one search.
const TOP: usize = 3;
/// Links asked for: spares for pages that won't load. Only fetched as
/// needed, so spares cost nothing unless a top page fails.
const CANDIDATES: usize = 10;

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
pub fn search(query: &str) -> String {
    let agent = WebSearchAgent::new(None, TOP, CANDIDATES, None);
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
    agent.fetch_top(query, &links, TOP, TOP, None)
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
}

#[cfg(test)]
mod live {
    /// Against the hosted endpoint, signed in: `cargo test live_search -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_search() {
        let text = super::search("rust tokio select macro documentation");
        println!("{}", text.chars().take(1500).collect::<String>());
        assert!(!text.is_empty());
    }
}
