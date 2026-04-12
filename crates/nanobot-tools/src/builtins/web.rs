//! Web tools — search and fetch.

use crate::trait_def::{Tool, ToolError};
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::debug;

// ─── WebSearchTool ────────────────────────────────────────────

pub struct WebSearchTool {
    provider: SearchProvider,
}

#[derive(Debug, Clone)]
enum SearchProvider {
    Brave,
    Tavily,
    Google,
    Disabled,
}

impl WebSearchTool {
    pub fn new() -> Self {
        let provider = if std::env::var("BRAVE_API_KEY").is_ok() {
            SearchProvider::Brave
        } else if std::env::var("TAVILY_API_KEY").is_ok() {
            SearchProvider::Tavily
        } else if std::env::var("GOOGLE_API_KEY").is_ok() {
            SearchProvider::Google
        } else {
            SearchProvider::Disabled
        };
        Self { provider }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web for information. Returns search results with titles, URLs, and snippets."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "count": { "type": "integer", "description": "Number of results (default: 5)" },
            },
            "required": ["query"],
        })
    }

    fn is_available(&self) -> bool {
        !matches!(self.provider, SearchProvider::Disabled)
    }

    fn required_env_vars(&self) -> Vec<&str> {
        match self.provider {
            SearchProvider::Brave => vec!["BRAVE_API_KEY"],
            SearchProvider::Tavily => vec!["TAVILY_API_KEY"],
            SearchProvider::Google => vec!["GOOGLE_API_KEY"],
            SearchProvider::Disabled => vec![],
        }
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'query'".to_string()))?;

        match &self.provider {
            SearchProvider::Brave => self.search_brave(query).await,
            SearchProvider::Tavily => self.search_tavily(query).await,
            SearchProvider::Google => Err(ToolError::NotAvailable(
                "Google search not yet implemented".to_string(),
            )),
            SearchProvider::Disabled => Err(ToolError::NotAvailable(
                "No search API key configured. Set BRAVE_API_KEY or TAVILY_API_KEY.".to_string(),
            )),
        }
    }
}

impl WebSearchTool {
    async fn search_brave(&self, query: &str) -> Result<String, ToolError> {
        let api_key = std::env::var("BRAVE_API_KEY")
            .map_err(|_| ToolError::NotAvailable("BRAVE_API_KEY not set".to_string()))?;

        let client = reqwest::Client::new();
        let resp = client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", &api_key)
            .query(&[("q", query), ("count", "5")])
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("Search request failed: {}", e)))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to parse search response: {}", e)))?;

        let mut results = Vec::new();
        if let Some(web_results) = body["web"]["results"].as_array() {
            for (i, r) in web_results.iter().enumerate() {
                results.push(format!(
                    "{}. {} — {}\n   {}",
                    i + 1,
                    r["title"].as_str().unwrap_or(""),
                    r["url"].as_str().unwrap_or(""),
                    r["description"].as_str().unwrap_or(""),
                ));
            }
        }

        if results.is_empty() {
            Ok("No results found.".to_string())
        } else {
            Ok(results.join("\n\n"))
        }
    }

    async fn search_tavily(&self, query: &str) -> Result<String, ToolError> {
        let api_key = std::env::var("TAVILY_API_KEY")
            .map_err(|_| ToolError::NotAvailable("TAVILY_API_KEY not set".to_string()))?;

        let client = reqwest::Client::new();
        let resp = client
            .post("https://api.tavily.com/search")
            .json(&json!({
                "api_key": api_key,
                "query": query,
                "max_results": 5,
            }))
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("Search request failed: {}", e)))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to parse search response: {}", e)))?;

        let mut results = Vec::new();
        if let Some(res) = body["results"].as_array() {
            for (i, r) in res.iter().enumerate() {
                results.push(format!(
                    "{}. {} — {}\n   {}",
                    i + 1,
                    r["title"].as_str().unwrap_or(""),
                    r["url"].as_str().unwrap_or(""),
                    r["content"].as_str().unwrap_or(""),
                ));
            }
        }

        if results.is_empty() {
            Ok("No results found.".to_string())
        } else {
            Ok(results.join("\n\n"))
        }
    }
}

// ─── WebFetchTool ────────────────────────────────────────────

pub struct WebFetchTool;

impl WebFetchTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch and extract text content from a web page URL."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to fetch" },
                "format": { "type": "string", "description": "Output format: 'text' or 'html' (default: 'text')" },
            },
            "required": ["url"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'url'".to_string()))?;

        debug!("Fetching URL: {}", url);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let resp = client
            .get(url)
            .header("User-Agent", "nanobot/0.1.0")
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to fetch URL: {}", e)))?;

        if !resp.status().is_success() {
            return Err(ToolError::Execution(format!("HTTP {}", resp.status())));
        }

        let html = resp
            .text()
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to read response: {}", e)))?;

        // Simple HTML to text extraction (strip tags)
        let text = html_to_text(&html);

        // Truncate if too long
        let text = if text.len() > 50_000 {
            format!("{}...\n(content truncated)", &text[..50_000])
        } else {
            text
        };

        Ok(text)
    }
}

/// Very basic HTML to text conversion.
fn html_to_text(html: &str) -> String {
    let re = regex::Regex::new(r"<[^>]+>").unwrap();
    let text = re.replace_all(html, "");
    // Collapse whitespace
    let ws = regex::Regex::new(r"\s+").unwrap();
    ws.replace_all(&text, " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trait_def::Tool;

    #[test]
    fn test_web_search_tool_disabled() {
        // Without any API key env vars, the tool should be disabled
        let tool = WebSearchTool::new();
        // Note: this test may pass or fail depending on whether env vars are set
        // in the test environment. In CI without keys, it should be disabled.
        let has_key = std::env::var("BRAVE_API_KEY").is_ok()
            || std::env::var("TAVILY_API_KEY").is_ok()
            || std::env::var("GOOGLE_API_KEY").is_ok();
        assert_eq!(tool.is_available(), has_key);
    }

    #[test]
    fn test_web_fetch_tool_schema() {
        let tool = WebFetchTool::new();
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["url"].is_object());
        assert!(schema["properties"]["format"].is_object());
        let required = schema["required"].as_array().unwrap();
        let required_names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(required_names.contains(&"url"));
    }

    #[test]
    fn test_html_to_text() {
        let html = "<html><body><h1>Hello</h1><p>World</p></body></html>";
        let text = html_to_text(html);
        assert_eq!(text, "HelloWorld");

        let html_with_spaces = "<div>Multiple   spaces   here</div>";
        let text = html_to_text(html_with_spaces);
        assert_eq!(text, "Multiple spaces here");

        let plain = "no html tags";
        assert_eq!(html_to_text(plain), "no html tags");
    }
}
