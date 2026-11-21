//! readable summaries of provider status replies (/mcp, /context) instead of raw
//! JSON. The lines are ratatui `Line`s in the Donut palette.

use std::sync::LazyLock;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use regex::Regex;
use serde_json::Value;

use crate::highlight::{CORAL, DIM, GRAY, ORANGE, RULE, TEAL, bold, fg};

/// (label, color) of an MCP status, as Claude Code and Codex name them.
pub fn status_label(status: &str) -> Option<(&'static str, Color)> {
    Some(match status {
        // Claude Code (McpServerConnectionStatus)
        "connected" => ("connected", TEAL),
        "failed" => ("failed", CORAL),
        "needs-auth" => ("needs login", ORANGE),
        "pending" => ("starting", ORANGE),
        "disabled" => ("disabled", DIM),
        // Codex (McpServerConnectionStatus)
        "notStarted" => ("not started", DIM),
        "starting" => ("starting", ORANGE),
        "authenticationRequired" => ("needs login", ORANGE),
        "cancelled" => ("cancelled", DIM),
        _ => return None,
    })
}

fn label_for(status: &str) -> (String, Color) {
    match status_label(status) {
        Some((label, color)) => (label.to_string(), color),
        None => (status.to_string(), GRAY),
    }
}

static TYPE_PATHS_SQUARE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[[^\[\]]*::[^\[\]]*\]").unwrap());
static TYPE_PATHS_ANGLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^<>]*::[^<>]*>").unwrap());
static ERROR_CLAUSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\berror:\s*").unwrap());

/// Keep the human part of a provider error: drop Rust type paths and keep the last "error:" clause. At most 90
/// characters.
pub fn short_error(message: Option<&str>) -> String {
    short_error_width(message, 90)
}

pub fn short_error_width(message: Option<&str>, width: usize) -> String {
    let Some(message) = message.filter(|m| !m.is_empty()) else {
        return String::new();
    };
    let text = TYPE_PATHS_SQUARE.replace_all(message, ""); // [codex_rmcp_client::...<...>] type names
    let text = TYPE_PATHS_ANGLE.replace_all(&text, "");
    let mut text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let clauses: Vec<&str> = ERROR_CLAUSE.split(&text).collect();
    if clauses.len() > 1 && !clauses[clauses.len() - 1].trim().is_empty() {
        text = clauses[clauses.len() - 1].trim().to_string();
    }
    let text = text
        .trim_matches(|c| matches!(c, ' ' | ':' | ';' | ','))
        .to_string();
    if text.chars().count() <= width {
        text
    } else {
        let kept: String = text.chars().take(width.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

fn error_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => short_error(Some(text)),
        Some(other) => short_error(Some(&other.to_string())),
    }
}

/// One MCP server in the shape both providers are turned into.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServer {
    pub name: String,
    pub status: String,
    pub tools: Option<usize>,
    pub source: String,
    pub version: String,
    pub reason: String,
}

fn count_of(value: Option<&Value>, objects_too: bool) -> Option<usize> {
    match value {
        Some(Value::Array(items)) => Some(items.len()),
        Some(Value::Object(map)) if objects_too => Some(map.len()),
        _ => None,
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn string_of(value: Option<&Value>, default: &str) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => default.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Normalize Claude's get_mcp_status and Codex's mcpServerStatus/list into one shape.
pub fn mcp_servers(data: &Value) -> Vec<McpServer> {
    let mut rows = Vec::new();
    if let Some(list) = data.get("mcpServers").and_then(Value::as_array) {
        // Claude
        for server in list.iter().filter(|s| s.is_object()) {
            let info = server.get("serverInfo");
            let status = server
                .get("status")
                .filter(|v| truthy(Some(v)))
                .map(|v| string_of(Some(v), "unknown"));
            rows.push(McpServer {
                name: string_of(server.get("name"), "?"),
                status: status.unwrap_or_else(|| "unknown".into()),
                tools: count_of(server.get("tools"), false),
                source: string_of(server.get("scope").filter(|v| truthy(Some(v))), ""),
                version: string_of(info.and_then(|i| i.get("version")), ""),
                reason: error_text(server.get("error")),
            });
        }
    } else if let Some(list) = data.get("data").and_then(Value::as_array) {
        // Codex
        for server in list.iter().filter(|s| s.is_object()) {
            let tools = server.get("tools");
            let error = server.get("toolsError");
            let not_logged_in =
                server.get("authStatus").and_then(Value::as_str) == Some("notLoggedIn");
            let mut status = match server.get("runtimeStatus").filter(|v| truthy(Some(v))) {
                Some(status) => string_of(Some(status), ""),
                None if not_logged_in => "needs-auth".into(),
                None if truthy(error) => "failed".into(),
                None if truthy(tools) => "connected".into(),
                None => "unknown".into(),
            };
            if status == "failed" && not_logged_in {
                status = "needs-auth".into();
            }
            let plugin = string_of(server.get("pluginId"), "");
            rows.push(McpServer {
                name: string_of(server.get("name"), "?"),
                status,
                tools: count_of(tools, true),
                source: plugin.split('@').next().unwrap_or("").to_string(),
                version: String::new(),
                reason: error_text(error),
            });
        }
    }
    rows
}

/// (name, status) from `claude mcp list`, whose lines read "name: address - ✓ Connected".
///
/// Names can hold colons (plugin:postman:postman) but never ": ", so the first ": " ends the name.
pub fn parse_mcp_list(text: &str) -> Vec<(String, String)> {
    let mut servers = Vec::new();
    for line in text.lines() {
        if !line.contains(": ") || !line.contains(" - ") {
            continue;
        }
        let Some((name, rest)) = line.split_once(": ") else {
            continue;
        };
        let status = rest
            .rsplit_once(" - ")
            .map(|(_, status)| status)
            .unwrap_or(rest)
            .trim();
        servers.push((name.trim().to_string(), status.to_string()));
    }
    servers
}

pub fn is_connected(status: &str) -> bool {
    let lowered = status.to_lowercase();
    lowered.contains("connected") && !lowered.contains("fail") && !lowered.contains("not")
}

fn padded(text: &str, width: usize) -> String {
    let count = text.chars().count();
    format!("{text}{}", " ".repeat(width.saturating_sub(count)))
}

fn right_aligned(text: &str, width: usize) -> String {
    let count = text.chars().count();
    format!("{}{text}", " ".repeat(width.saturating_sub(count)))
}

fn text_line(text: &str, style: Style) -> Line<'static> {
    Line::from(vec![Span::styled(text.to_string(), style)])
}

/// Lines for the transcript: a one-line tally, then one line per server.
pub fn render_mcp(servers: &[McpServer], provider: &str) -> Vec<Line<'static>> {
    if servers.is_empty() {
        return vec![text_line(
            &format!("  No MCP servers configured for {provider}."),
            fg(DIM),
        )];
    }
    let mut tally: Vec<(String, usize)> = Vec::new();
    for server in servers {
        let (label, _) = label_for(&server.status);
        match tally.iter_mut().find(|(known, _)| *known == label) {
            Some(entry) => entry.1 += 1,
            None => tally.push((label, 1)),
        }
    }
    let counts = tally
        .iter()
        .map(|(label, count)| format!("{count} {label}"))
        .collect::<Vec<_>>()
        .join(" · ");
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("  MCP servers · {provider}  "), bold(ORANGE)),
        Span::styled(counts, fg(GRAY)),
    ])];
    let width = servers
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut sorted: Vec<&McpServer> = servers.iter().collect();
    sorted.sort_by_key(|s| (s.status != "connected", s.name.to_lowercase()));
    for server in sorted {
        let (label, color) = label_for(&server.status);
        let mut spans = vec![
            Span::styled("    ● ".to_string(), fg(color)),
            Span::styled(padded(&server.name, width), bold(GRAY)),
            Span::styled(format!("  {}", padded(&label, 11)), fg(color)),
        ];
        if let Some(tools) = server.tools.filter(|t| *t > 0) {
            spans.push(Span::styled(
                format!("  {tools} tool{}", if tools != 1 { "s" } else { "" }),
                fg(DIM),
            ));
        }
        if !server.source.is_empty() {
            spans.push(Span::styled(format!("  {}", server.source), fg(DIM)));
        }
        if !server.reason.is_empty() {
            let color = if server.status == "failed" {
                CORAL
            } else {
                DIM
            };
            spans.push(Span::styled(format!("  {}", server.reason), fg(color)));
        }
        lines.push(Line::from(spans));
    }
    if servers.iter().any(|s| {
        matches!(
            s.status.as_str(),
            "needs-auth" | "authenticationRequired" | "failed"
        )
    }) {
        lines.push(text_line(
            "  Use /native to log in to or fix these servers in the provider's own client.",
            fg(DIM),
        ));
    }
    lines
}

fn tokens(count: i64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else {
        count.to_string()
    }
}

/// Categories Claude reports that are not things taking up the context window.
pub const NOT_USAGE: [&str; 2] = ["free space", "autocompact buffer"];

fn bar(percent: f64, width: usize) -> (Vec<Span<'static>>, Color) {
    let filled = ((percent / 100.0 * width as f64)
        .round_ties_even()
        .clamp(0.0, width as f64)) as usize;
    let color = if percent < 60.0 {
        TEAL
    } else if percent < 85.0 {
        ORANGE
    } else {
        CORAL
    };
    let mut spans = Vec::new();
    if filled > 0 {
        spans.push(Span::styled("━".repeat(filled), fg(color)));
    }
    if width > filled {
        spans.push(Span::styled("─".repeat(width - filled), fg(RULE)));
    }
    (spans, color)
}

fn number_of(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        _ => 0,
    }
}

/// A context-window bar, then the largest categories when the provider reports them. `used` and `window` are
/// token counts (a window of 0 means it was not reported); each category is an object with "name" and "tokens".
pub fn render_context(
    used: i64,
    window: i64,
    categories: &[Value],
    model: &str,
    note: &str,
) -> Vec<Line<'static>> {
    let mut header = vec![Span::styled("  Context".to_string(), bold(ORANGE))];
    if !model.is_empty() {
        header.push(Span::styled(format!(" · {model}"), fg(GRAY)));
    }
    let mut lines = vec![Line::from(header)];
    if window > 0 {
        let percent = used as f64 / window as f64 * 100.0;
        let (bar_spans, color) = bar(percent, 24);
        let mut spans = vec![Span::raw("    ".to_string())];
        spans.extend(bar_spans);
        spans.push(Span::styled(
            format!("  {} / {} tokens", tokens(used), tokens(window)),
            fg(GRAY),
        ));
        spans.push(Span::styled(format!("  {percent:.0}% used"), fg(color)));
        lines.push(Line::from(spans));
    } else if used != 0 {
        lines.push(text_line(
            &format!(
                "    {} tokens in context (window size not reported)",
                tokens(used)
            ),
            fg(GRAY),
        ));
    }
    let mut shown: Vec<&Value> = categories
        .iter()
        .filter(|c| number_of(c.get("tokens")) != 0)
        .collect();
    shown.sort_by_key(|c| -number_of(c.get("tokens")));
    shown.truncate(6);
    if !shown.is_empty() {
        let name_of = |c: &Value, default: &str| {
            c.get("name")
                .and_then(Value::as_str)
                .unwrap_or(default)
                .to_string()
        };
        let width = shown
            .iter()
            .map(|c| name_of(c, "").chars().count())
            .max()
            .unwrap_or(0);
        for category in shown {
            let count = number_of(category.get("tokens"));
            let mut spans = vec![Span::styled(
                format!("    {}", padded(&name_of(category, "?"), width)),
                fg(GRAY),
            )];
            spans.push(Span::styled(
                format!("  {}", right_aligned(&tokens(count), 7)),
                fg(DIM),
            ));
            if window > 0 {
                spans.push(Span::styled(
                    format!("  {:4.1}%", count as f64 / window as f64 * 100.0),
                    fg(DIM),
                ));
            }
            lines.push(Line::from(spans));
        }
    }
    if !note.is_empty() {
        lines.push(text_line(&format!("  {note}"), fg(DIM)));
    }
    lines
}

pub fn claude_context(data: &Value) -> Vec<Line<'static>> {
    if !data.is_object() {
        return vec![text_line("  Claude did not report context usage.", fg(DIM))];
    }
    let categories: Vec<Value> = data
        .get("categories")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| {
            c.is_object()
                && !truthy(c.get("isDeferred"))
                && !NOT_USAGE.contains(&string_of(c.get("name"), "").to_lowercase().as_str())
        })
        .cloned()
        .collect();
    let max = match number_of(data.get("maxTokens")) {
        0 => number_of(data.get("rawMaxTokens")),
        other => other,
    };
    render_context(
        number_of(data.get("totalTokens")),
        max,
        &categories,
        data.get("model").and_then(Value::as_str).unwrap_or(""),
        if truthy(data.get("isAutoCompactEnabled")) {
            "Auto-compact is on."
        } else {
            ""
        },
    )
}

/// Codex reports token usage per turn; the last turn's total is what fills the window.
pub fn codex_context(usage: &Value, model: &str) -> Vec<Line<'static>> {
    if usage.as_object().is_none_or(|map| map.is_empty()) {
        return vec![text_line(
            "  No Codex conversation yet: send a message first.",
            fg(DIM),
        )];
    }
    let empty = Value::Object(Default::default());
    let last = usage
        .get("last")
        .filter(|v| truthy(Some(v)))
        .unwrap_or(&empty);
    let total = usage
        .get("total")
        .filter(|v| truthy(Some(v)))
        .unwrap_or(&empty);
    let mut lines = render_context(
        number_of(last.get("totalTokens")),
        number_of(usage.get("modelContextWindow")),
        &[],
        model,
        "",
    );
    let total_tokens = number_of(total.get("totalTokens"));
    if total_tokens != 0 {
        lines.push(text_line(
            &format!(
                "    this conversation: {} tokens ({} in, {} out)",
                tokens(total_tokens),
                tokens(number_of(total.get("inputTokens"))),
                tokens(number_of(total.get("outputTokens")))
            ),
            fg(DIM),
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::plain;
    use serde_json::json;

    const RUST_ERROR: &str = "MCP startup failed: handshaking with MCP server failed: Send message error Transport \
[codex_rmcp_client::tool_list_changes::ToolListChanges<codex_rmcp_client::event_notification_transport\
<rmcp::transport::worker::WorkerTransport<rmcp::transport::streamable_http_client::StreamableHttpClientWorker\
<codex_rmcp_client::client_adapter::StreamableHttpClientAdapter>>>>] error: Auth required, when send initialize request";

    fn text_of(lines: &[Line]) -> String {
        lines.iter().map(plain).collect::<Vec<_>>().join("\n")
    }

    fn matches(pattern: &str, text: &str) -> bool {
        Regex::new(pattern).unwrap().is_match(text)
    }

    #[test]
    fn test_rust_errors_are_shortened_to_the_human_part() {
        assert_eq!(
            short_error(Some(RUST_ERROR)),
            "Auth required, when send initialize request"
        );
        assert_eq!(short_error(None), "");
        assert!(short_error(Some(&"x".repeat(500))).chars().count() <= 90);
    }

    #[test]
    fn test_codex_reply_becomes_one_line_per_server() {
        let reply = json!({
            "data": [
                {
                    "name": "github",
                    "runtimeStatus": null,
                    "pluginId": null,
                    "tools": {"a": {}, "b": {}},
                    "toolsError": null,
                    "resources": [],
                    "resourceTemplates": [],
                    "authStatus": "unsupported",
                },
                {
                    "name": "postman-mcp",
                    "runtimeStatus": null,
                    "pluginId": "postman@claude-plugins-official",
                    "httpOrigin": "https://mcp.postman.com",
                    "tools": {},
                    "toolsError": RUST_ERROR,
                    "resources": [],
                    "resourceTemplates": [],
                    "authStatus": "notLoggedIn",
                },
            ],
            "nextCursor": null,
        });
        let text = text_of(&render_mcp(&mcp_servers(&reply), "codex"));
        assert!(text.contains("MCP servers · codex"));
        assert!(text.contains("1 connected · 1 needs login"));
        assert!(matches(r"github\s+connected\s+2 tools", &text), "{text}");
        assert!(
            matches(
                r"postman-mcp\s+needs login\s+postman\s+Auth required",
                &text
            ),
            "{text}"
        );
        assert!(!text.contains("codex_rmcp_client"));
        assert!(!text.contains('{'));
        assert!(text.contains("/native"));
        assert!(text.lines().count() <= 4);
    }

    #[test]
    fn test_claude_reply() {
        let reply = json!({
            "mcpServers": [
                {"name": "linear", "status": "connected", "scope": "user", "tools": [{"name": "a"}]},
                {"name": "broken", "status": "failed", "error": "spawn npx ENOENT"},
            ]
        });
        let text = text_of(&render_mcp(&mcp_servers(&reply), "claude"));
        assert!(
            matches(r"linear\s+connected\s+1 tool\s+user", &text),
            "{text}"
        );
        assert!(
            matches(r"broken\s+failed\s+spawn npx ENOENT", &text),
            "{text}"
        );
        assert!(text_of(&render_mcp(&[], "claude")).contains("No MCP servers configured"));
    }

    #[test]
    fn test_claude_context() {
        let reply = json!({
            "categories": [
                {"name": "System prompt", "tokens": 3100, "color": "x"},
                {"name": "Messages", "tokens": 41000, "color": "x"},
                {"name": "Free space", "tokens": 150000, "color": "x"},
                {"name": "Autocompact buffer", "tokens": 33000, "color": "x"},
                {"name": "System tools (deferred)", "tokens": 19200, "color": "x", "isDeferred": true},
            ],
            "totalTokens": 44100,
            "maxTokens": 200000,
            "rawMaxTokens": 200000,
            "percentage": 22.0,
            "model": "claude-opus-5-5",
            "isAutoCompactEnabled": true,
        });
        let text = text_of(&claude_context(&reply));
        assert!(text.contains("Context · claude-opus-5-5"));
        assert!(text.contains("44.1k / 200.0k tokens  22% used"), "{text}");
        assert!(text.find("Messages").unwrap() < text.find("System prompt").unwrap());
        assert!(!text.contains("Free space"));
        assert!(!text.contains("Autocompact buffer"));
        assert!(!text.contains("deferred"));
    }

    #[test]
    fn test_large_windows_read_in_millions() {
        let text = text_of(&claude_context(
            &json!({"totalTokens": 14900, "maxTokens": 1_000_000, "categories": []}),
        ));
        assert!(text.contains("14.9k / 1.0M tokens  1% used"), "{text}");
    }

    #[test]
    fn test_codex_context() {
        let usage = json!({
            "last": {"totalTokens": 30000},
            "total": {"totalTokens": 90000, "inputTokens": 80000, "outputTokens": 10000},
            "modelContextWindow": 272000,
        });
        let text = text_of(&codex_context(&usage, "gpt-6.1-terra"));
        assert!(text.contains("30.0k / 272.0k tokens  11% used"), "{text}");
        assert!(text.contains("this conversation: 90.0k tokens (80.0k in, 10.0k out)"));
        assert!(text_of(&codex_context(&json!({}), "")).contains("No Codex conversation yet"));
    }

    #[test]
    fn test_mcp_list_is_parsed_from_the_claude_cli_output() {
        let listing = "Checking MCP server health…\n\n\
plugin:postman:postman: https://mcp.example.com/mcp (HTTP) - ! Needs authentication\n\
claude.ai Notes: https://notes.example.com/mcp (HTTP) - \u{2713} Connected\n";
        assert_eq!(
            parse_mcp_list(listing),
            [
                (
                    "plugin:postman:postman".to_string(),
                    "! Needs authentication".to_string()
                ),
                (
                    "claude.ai Notes".to_string(),
                    "\u{2713} Connected".to_string()
                ),
            ]
        );
        assert!(is_connected("\u{2713} Connected"));
        assert!(!is_connected("! Needs authentication"));
        assert!(!is_connected("\u{2717} Failed to connect"));
    }
}
