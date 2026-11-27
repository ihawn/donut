//! /effort: how hard the selected model thinks, in each provider's own terms and levels.
//!
//! Claude calls it effort (low, medium, high, xhigh, max) and says per model which levels it takes; Codex calls it
//! reasoning effort and lists each model's levels with its own descriptions. Qwen Code has no such setting.

use serde_json::Value;

/// The Claude Agent SDK's own descriptions of each level.
pub const CLAUDE_LEVELS: [(&str, &str); 5] = [
    ("low", "Minimal thinking, fastest responses"),
    ("medium", "Moderate thinking"),
    ("high", "Deep reasoning (default)"),
    ("xhigh", "Extended reasoning depth"),
    ("max", "Maximum effort"),
];

/// (term, [(level, description)], default level).
pub type Options = (String, Vec<(String, String)>, Option<String>);

/// Python's str(value).lower() for a JSON value.
fn shown(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::Null) => "none".into(),
        Some(Value::String(text)) => text.to_lowercase(),
        Some(Value::Bool(true)) => "true".into(),
        Some(Value::Bool(false)) => "false".into(),
        Some(other) => other.to_string().to_lowercase(),
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64() != Some(0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(list)) => !list.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

fn matches(entry: &Value, model: &str) -> bool {
    let model = model.to_lowercase();
    if model.is_empty() {
        return false;
    }
    let names: Vec<String> = ["value", "id", "model", "displayName"]
        .iter()
        .map(|key| shown(entry.get(*key)))
        .collect();
    names.contains(&model)
        || names
            .iter()
            .any(|name| !name.is_empty() && model.starts_with(name.as_str()))
}

fn all_levels() -> Vec<String> {
    CLAUDE_LEVELS
        .iter()
        .map(|(level, _)| level.to_string())
        .collect()
}

/// ("Effort", [(level, description)], default) for a Claude model, from Claude Code's own model list.
///
/// An empty list means the model takes no effort setting. A model Claude Code does not list gets every level.
pub fn claude_options(info: &Value, model: &str) -> Options {
    let entries: Vec<&Value> = info
        .get("models")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter(|e| e.is_object()).collect())
        .unwrap_or_default();
    let mut entry = entries.iter().find(|entry| matches(entry, model)).copied();
    if entry.is_none() && model.is_empty() {
        entry = entries
            .iter()
            .find(|entry| shown(entry.get("value")) == "default")
            .copied();
    }
    let levels = match entry {
        Some(entry) => {
            if entry.get("supportsEffort") == Some(&Value::Bool(false)) {
                return ("Effort".into(), Vec::new(), None); // Claude Code says this model takes no effort setting.
            }
            let listed: Vec<String> = entry
                .get("supportedEffortLevels")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|level| level.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if listed.is_empty() {
                all_levels()
            } else {
                listed
            }
        }
        None => all_levels(),
    };
    let described = levels
        .into_iter()
        .map(|level| {
            let words = CLAUDE_LEVELS
                .iter()
                .find(|(key, _)| *key == level)
                .map_or("", |(_, words)| words);
            (level, words.to_string())
        })
        .collect();
    ("Effort".into(), described, Some("high".into()))
}

/// ("Thinking", [(level, description)], default) for an Ollama model, from Ollama's `/api/show` answer.
///
/// Ollama lists each thinking model's levels: `[false, true]` for most, named levels (low, medium, high) for some.
/// A model without a thinking capability takes no setting.
pub fn ollama_options(show: &Value) -> Options {
    let term = "Thinking".to_string();
    let thinking = show.get("thinking");
    let capable = show
        .get("capabilities")
        .and_then(Value::as_array)
        .is_some_and(|list| list.iter().any(|c| c.as_str() == Some("thinking")));
    if !capable {
        return (term, Vec::new(), None);
    }
    let level = |value: &Value| match value {
        Value::Bool(false) => Some("off".to_string()),
        Value::Bool(true) => Some("on".to_string()),
        Value::String(text) if !text.is_empty() => Some(text.to_lowercase()),
        _ => None,
    };
    let mut values: Vec<String> = thinking
        .and_then(|t| t.get("values"))
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(level).collect())
        .unwrap_or_default();
    if values.is_empty() {
        values = vec!["off".into(), "on".into()];
    }
    let words = |level: &str| match level {
        "off" => "No thinking: fastest answers",
        "on" => "Thinks before it answers",
        "low" => "Light thinking",
        "medium" => "Moderate thinking",
        "high" => "Deep thinking",
        _ => "",
    };
    let described = values
        .into_iter()
        .map(|value| {
            let text = words(&value).to_string();
            (value, text)
        })
        .collect();
    let default = thinking.and_then(|t| t.get("default")).and_then(level);
    (term, described, default)
}

/// The `reasoning_effort` an OpenAI-compatible endpoint takes for a Donut level: off is none, on is high.
pub fn wire_level(level: &str) -> &str {
    match level {
        "off" => "none",
        "on" => "high",
        other => other,
    }
}

/// ("Reasoning effort", [(level, description)], default) for a Codex model, from Codex's model list.
pub fn codex_options(models: &Value, model: &str) -> Options {
    let term = "Reasoning effort".to_string();
    let entries: Vec<&Value> = models
        .as_array()
        .map(|list| list.iter().filter(|e| e.is_object()).collect())
        .unwrap_or_default();
    let entry = entries
        .iter()
        .find(|entry| matches(entry, model))
        .or_else(|| entries.iter().find(|entry| truthy(entry.get("isDefault"))))
        .copied();
    let Some(entry) = entry else {
        return (term, Vec::new(), None);
    };
    let mut levels = Vec::new();
    for option in entry
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(object) = option.as_object() {
            if let Some(level) = object
                .get("reasoningEffort")
                .and_then(Value::as_str)
                .filter(|l| !l.is_empty())
            {
                let words = object
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                levels.push((level.to_string(), words.to_string()));
            }
        } else if let Some(level) = option.as_str() {
            levels.push((level.to_string(), String::new()));
        }
    }
    let default = entry
        .get("defaultReasoningEffort")
        .and_then(Value::as_str)
        .map(str::to_string);
    (term, levels, default)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::providers;

    fn claude_info() -> Value {
        json!({"models": [
            {"value": "opus", "displayName": "Opus", "supportsEffort": true,
             "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]},
            {"value": "sonnet", "displayName": "Sonnet", "supportsEffort": true,
             "supportedEffortLevels": ["low", "medium", "high", "max"]},
            {"value": "haiku", "displayName": "Haiku", "supportsEffort": false},
        ]})
    }

    fn codex_models() -> Value {
        json!([{
            "id": "gpt-6.1", "model": "gpt-6.1", "isDefault": true, "defaultReasoningEffort": "medium",
            "supportedReasoningEfforts": [
                {"reasoningEffort": "low", "description": "Fast responses with lighter reasoning"},
                {"reasoningEffort": "medium", "description": "Balances speed and reasoning depth"},
                {"reasoningEffort": "high", "description": "Greater reasoning depth for complex problems"},
            ],
        }])
    }

    fn names(levels: &[(String, String)]) -> Vec<&str> {
        levels.iter().map(|(level, _)| level.as_str()).collect()
    }

    #[test]
    fn test_claude_uses_each_models_own_levels_and_words() {
        let (term, levels, default) = claude_options(&claude_info(), "opus");
        assert_eq!(
            (term.as_str(), default.as_deref()),
            ("Effort", Some("high"))
        );
        assert_eq!(names(&levels), ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(
            levels.iter().find(|(l, _)| l == "high").unwrap().1,
            "Deep reasoning (default)"
        );
        assert!(!names(&claude_options(&claude_info(), "sonnet").1).contains(&"xhigh"));
        assert!(claude_options(&claude_info(), "haiku").1.is_empty());
        assert_eq!(claude_options(&json!({}), "claude-opus-5-5").1.len(), 5);
    }

    #[test]
    fn test_codex_uses_its_reasoning_effort_list() {
        let (term, levels, default) = codex_options(&codex_models(), "gpt-6.1");
        assert_eq!(
            (term.as_str(), default.as_deref()),
            ("Reasoning effort", Some("medium"))
        );
        assert_eq!(
            levels[2],
            (
                "high".to_string(),
                "Greater reasoning depth for complex problems".to_string()
            )
        );
        assert_eq!(codex_options(&codex_models(), "").1, levels);
    }

    fn build(profile: Value, env: &[(&str, &str)], readonly: bool, review: bool) -> Vec<String> {
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let which = |_: &str| Some("/bin/agent".to_string());
        providers::command_with(&profile, "task", readonly, None, review, &env, &which)
            .unwrap()
            .0
    }

    fn after<'a>(args: &'a [String], flag: &str) -> &'a str {
        &args[args.iter().position(|a| a == flag).unwrap() + 1]
    }

    #[test]
    fn test_workers_get_effort_on_their_command_line() {
        let key = [("KEY", "x")];
        let claude = build(
            json!({"engine": "claude", "model": "opus", "effort": "max"}),
            &key,
            false,
            false,
        );
        let codex = build(
            json!({"engine": "codex", "model": "gpt-6.1", "effort": "high"}),
            &key,
            false,
            false,
        );
        let qwen = build(
            json!({"engine": "ollama", "model": "q", "effort": "high"}),
            &key,
            false,
            false,
        );
        assert_eq!(after(&claude, "--effort"), "max");
        assert!(!claude.contains(&"--chrome".to_string()));
        assert!(!claude.contains(&"--strict-mcp-config".to_string()));
        let plain = json!({"engine": "claude", "model": "opus"});
        let summary = build(plain.clone(), &[], true, false);
        let review = build(plain.clone(), &[], false, true);
        assert!(summary.contains(&"--strict-mcp-config".to_string()));
        assert_eq!(after(&summary, "--setting-sources"), "");
        assert!(review.contains(&"--strict-mcp-config".to_string()));
        assert!(!review.contains(&"--setting-sources".to_string()));
        let chrome = [("DONUT_CHROME", "1")];
        let worker = build(plain.clone(), &chrome, false, false);
        let reviewer = build(plain, &chrome, false, true);
        let other = build(
            json!({"engine": "codex", "model": "gpt-6.1"}),
            &chrome,
            false,
            false,
        );
        assert!(worker.contains(&"--chrome".to_string()));
        assert!(!reviewer.contains(&"--chrome".to_string()));
        assert!(!other.contains(&"--chrome".to_string()));
        assert_eq!(after(&codex, "-c"), "model_reasoning_effort=\"high\"");
        assert!(!qwen.contains(&"--effort".to_string()));
        assert!(
            providers::validate(&json!({"engine": "claude", "effort": "high; rm -rf"})).is_err()
        );
    }

    #[test]
    fn ollama_levels_come_from_the_models_own_list() {
        let show = json!({"capabilities": ["completion", "thinking"], "thinking": {"values": [false, true], "default": true}});
        let (term, levels, default) = ollama_options(&show);
        assert_eq!(term, "Thinking");
        assert_eq!(
            levels.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
            ["off", "on"]
        );
        assert_eq!(default.as_deref(), Some("on"));
        let named = json!({"capabilities": ["thinking"], "thinking": {"values": ["low", "medium", "high"], "default": "medium"}});
        assert_eq!(ollama_options(&named).1.len(), 3);
        assert!(
            ollama_options(&json!({"capabilities": ["completion"]}))
                .1
                .is_empty()
        );
        assert_eq!(
            (wire_level("off"), wire_level("on"), wire_level("low")),
            ("none", "high", "low")
        );
    }
}
