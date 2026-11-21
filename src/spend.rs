//! one spend figure for everything Donut runs.
//!
//! Each provider is counted with the most exact source it gives:
//!
//! - Claude Code reports a cost for every turn, at the list price of the platform the turn ran on. Donut uses
//!   that figure as it is.
//! - Codex and OpenAI-compatible endpoints report tokens only. Donut prices them with `PRICES`, or with "prices"
//!   in the configuration: `{"prices": {"model-name": {"input": 2, "cached_input": 0.1, "output": 10}}}`, in
//!   dollars for a million tokens. A model with no price counts as zero and is listed as not priced.
//! - A local model costs nothing.
//!
//! Work that a subscription covers has no price for each token. It counts at the list price;
//! `{"spend": {"plans": "free"}}` in the configuration counts it as zero.

use std::path::PathBuf;

use serde_json::Value;

/// Dollars for a million tokens: (input, cached input, output).
pub type Rates = (f64, f64, f64);

/// Dollars for a million tokens: (input, cached input, output). The longest name found in the model ID wins.
pub const PRICES: &[(&str, Rates)] = &[
    ("gpt-6-astra", (10.0, 1.0, 50.0)),
    ("gpt-6.1-sol", (2.0, 0.1, 10.0)),
    ("gpt-6.1-terra", (1.0, 0.05, 5.0)),
];

const INPUT: &[&str] = &["input_tokens", "inputTokens", "prompt_tokens"];
const CACHED: &[&str] = &[
    "cached_input_tokens",
    "cachedInputTokens",
    "cache_read_input_tokens",
];
const OUTPUT: &[&str] = &["output_tokens", "outputTokens", "completion_tokens"];

fn number(value: Option<&Value>, default: f64) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(default),
        _ => default,
    }
}

/// (input, cached input, output) prices for a model, or None when none is known.
pub fn rates(model: &str, config: Option<&Value>) -> Option<Rates> {
    let mut table: Vec<(String, Rates)> = PRICES
        .iter()
        .map(|(name, price)| (name.to_string(), *price))
        .collect();
    let prices = config
        .and_then(|c| c.get("prices"))
        .and_then(Value::as_object);
    for (name, price) in prices.into_iter().flatten() {
        let parsed = match price {
            Value::Object(map) => {
                let input = number(map.get("input"), 0.0);
                Some((
                    input,
                    number(map.get("cached_input"), input),
                    number(map.get("output"), 0.0),
                ))
            }
            Value::Array(items) if items.len() == 3 => Some((
                number(items.first(), 0.0),
                number(items.get(1), 0.0),
                number(items.get(2), 0.0),
            )),
            _ => None,
        };
        if let Some(parsed) = parsed {
            let name = name.to_lowercase();
            match table.iter_mut().find(|(known, _)| *known == name) {
                Some(entry) => entry.1 = parsed,
                None => table.push((name, parsed)),
            }
        }
    }
    let model = model.to_lowercase();
    let mut best: Option<&(String, Rates)> = None;
    for entry in &table {
        if model.contains(&entry.0)
            && best.is_none_or(|b| entry.0.chars().count() > b.0.chars().count())
        {
            best = Some(entry);
        }
    }
    best.map(|(_, price)| *price)
}

/// Dollars for a token report, or None when the model has no price.
///
/// The input count includes the cached tokens (OpenAI's and Codex's reports do), which have their own price.
pub fn cost(model: &str, usage: &Value, config: Option<&Value>) -> Option<f64> {
    let price = rates(model, config)?;
    let usage = usage.as_object()?;
    let count = |keys: &[&str]| -> i64 {
        keys.iter()
            .find_map(|key| usage.get(*key))
            .map(|value| match value {
                Value::Number(n) => n
                    .as_i64()
                    .or_else(|| n.as_f64().map(|f| f as i64))
                    .unwrap_or(0),
                Value::String(s) => s.trim().parse().unwrap_or(0),
                _ => 0,
            })
            .unwrap_or(0)
    };
    let cached = count(CACHED);
    let fresh = (count(INPUT) - cached).max(0);
    Some(
        (fresh as f64 * price.0 + cached as f64 * price.1 + count(OUTPUT) as f64 * price.2)
            / 1_000_000.0,
    )
}

/// Whether Codex is signed in with a ChatGPT plan, not an API key.
pub fn codex_plan() -> bool {
    let folder = std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::plugins::home_dir().join(".codex"));
    std::fs::read_to_string(folder.join("auth.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|data| data.get("auth_mode").and_then(Value::as_str) == Some("chatgpt"))
}

/// How a model's work is paid for: "local", "plan", "api", "bedrock" or "vertex".
///
/// `account` is what Claude Code says about the login: its apiKeySource ("none" for a plan login), or the
/// provider it names in its usage report ("bedrock", "vertex").
pub fn platform(profile: &Value, account: Option<&str>) -> &'static str {
    match profile.get("engine").and_then(Value::as_str) {
        Some("ollama") => "local",
        Some("codex") => {
            if codex_plan() {
                "plan"
            } else {
                "api"
            }
        }
        Some("claude") => match account {
            Some("bedrock") => "bedrock",
            Some("vertex") => "vertex",
            Some("none") => "plan",
            _ => "api",
        },
        _ => "api",
    }
}

/// Whether work on this platform adds to the figure.
pub fn counted(whence: &str, config: Option<&Value>) -> bool {
    if whence == "local" {
        return false;
    }
    let free = config
        .and_then(|c| c.get("spend"))
        .and_then(|s| s.get("plans"))
        .and_then(Value::as_str)
        == Some("free");
    !(whence == "plan" && free)
}

/// What says how a Claude turn is paid for, from the stream's init event and the result's modelUsage.
pub fn claude_account(init: Option<&Value>, usage: Option<&Value>) -> Option<String> {
    let providers: Vec<String> = usage
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| map.values())
        .filter_map(|entry| entry.as_object())
        .map(|entry| match entry.get("provider") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.to_lowercase(),
            Some(other) => other.to_string().to_lowercase(),
        })
        .collect();
    for name in ["bedrock", "vertex"] {
        if providers.iter().any(|provider| provider.contains(name)) {
            return Some(name.to_string());
        }
    }
    init.and_then(|i| i.get("apiKeySource"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-7
    }

    #[test]
    fn test_tokens_are_priced_with_the_cached_part_at_its_own_rate() {
        let usage = json!({"input_tokens": 21706, "cached_input_tokens": 13312, "output_tokens": 5, "reasoning_output_tokens": 0});
        let sol = cost("gpt-6.1-sol", &usage, None).unwrap();
        assert!(close(
            sol,
            (8394.0 * 2.0 + 13312.0 * 0.1 + 5.0 * 10.0) / 1e6
        ));
        let thread =
            json!({"inputTokens": 1_000_000, "cachedInputTokens": 0, "outputTokens": 100_000});
        assert!(close(
            cost("gpt-6-astra", &thread, None).unwrap(),
            10.0 + 5.0
        ));
        assert!(cost("some-new-model", &usage, None).is_none());
        let config = json!({
            "prices": {"some-new-model": {"input": 1, "cached_input": 0.5, "output": 4}, "gpt-6.1-sol": [4, 1, 20]}
        });
        let report = json!({"prompt_tokens": 2_000_000, "completion_tokens": 0});
        assert!(close(
            cost("some-new-model", &report, Some(&config)).unwrap(),
            2.0
        ));
        assert!(close(
            cost("gpt-6.1-sol", &thread, Some(&config)).unwrap(),
            4.0 + 2.0
        ));
        let pro = json!({"prices": {"gpt-6.1-sol-pro": [9, 9, 9]}});
        assert_eq!(rates("gpt-6.1-sol-pro", Some(&pro)), Some((9.0, 9.0, 9.0)));
    }

    #[test]
    fn test_the_platform_decides_whether_work_is_counted() {
        let guard = crate::conversations::env_lock();
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CODEX_HOME", home.path()) };
        assert_eq!(platform(&json!({"engine": "codex"}), None), "api");
        std::fs::write(home.path().join("auth.json"), r#"{"auth_mode": "chatgpt"}"#).unwrap();
        assert_eq!(platform(&json!({"engine": "codex"}), None), "plan");
        drop(guard);
        assert_eq!(platform(&json!({"engine": "ollama"}), None), "local");
        assert_eq!(platform(&json!({"engine": "claude"}), Some("none")), "plan");
        assert_eq!(
            platform(&json!({"engine": "claude"}), Some("ANTHROPIC_API_KEY")),
            "api"
        );
        assert_eq!(platform(&json!({"engine": "openai"}), None), "api");
        let report = json!({"claude-opus-5-5": {"provider": "bedrock", "costUSD": 1.0}});
        let init = json!({"apiKeySource": "none"});
        let account = claude_account(Some(&init), Some(&report));
        assert_eq!(
            platform(&json!({"engine": "claude"}), account.as_deref()),
            "bedrock"
        );
        let first_party = json!({"m": {"provider": "firstParty"}});
        assert_eq!(
            claude_account(Some(&init), Some(&first_party)).as_deref(),
            Some("none")
        );
        let free = json!({"spend": {"plans": "free"}});
        assert!(!counted("local", None));
        assert!(counted("plan", None));
        assert!(!counted("plan", Some(&free)));
        assert!(counted("bedrock", Some(&free)));
    }
}
