//! Run any model in the Claude Code or Codex harness (`/harness`).
//!
//! A profile's `engine` names the account a model comes from: `claude`, `codex`, `ollama` or `openai` (an
//! OpenAI-compatible endpoint). A profile's `harness` names the agent loop that drives it. `effective` turns a
//! profile with a harness into one the harness's session understands: `engine` becomes the harness, and the
//! model's endpoint goes in as environment (`env`, for Claude Code) or `-c` overrides (`codex_config`, for Codex).
//!
//! Local Ollama serves both the Anthropic Messages API and the OpenAI Responses API, so its models work in both
//! harnesses as they are. Neither vendor serves the other's API: a GPT model in Claude Code, or a Claude model in
//! Codex, goes through a gateway the user runs or rents (LiteLLM, OpenRouter, claude-code-router), set as the
//! alias's `gateway_url`, with its key in the variable `gateway_key_env` names. A subscription login stays with
//! its own vendor's harness.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::providers::ollama_base_url;

/// The harnesses `/harness` offers. `auto` is each model's own.
pub const HARNESSES: [&str; 3] = ["auto", "claude", "codex"];

fn text<'a>(profile: &'a Value, key: &str) -> Option<&'a str> {
    profile
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The harness the profile asks for, or None for the model's own.
pub fn chosen(profile: &Value) -> Option<&str> {
    text(profile, "harness").filter(|h| *h != "auto")
}

/// The profile a session should open: unchanged without a harness, else rewritten for it.
pub fn effective(profile: &Value) -> Result<Value> {
    let Some(harness) = chosen(profile) else {
        return Ok(profile.clone());
    };
    let engine = text(profile, "engine").unwrap_or("");
    if engine == harness {
        return Ok(profile.clone());
    }
    let model = text(profile, "model").unwrap_or("").to_string();
    let mut out = profile.clone();
    out["engine"] = json!(harness);
    out["model_engine"] = json!(engine);
    match harness {
        "claude" => {
            let (base, token) = match engine {
                "ollama" => (
                    ollama_base_url().trim_end_matches("/v1").to_string(),
                    "ollama".to_string(),
                ),
                _ => gateway(profile, harness, engine)?,
            };
            let mut env = json!({
                "ANTHROPIC_BASE_URL": base,
                "ANTHROPIC_AUTH_TOKEN": token,
                "ANTHROPIC_API_KEY": "",
            });
            if !model.is_empty() {
                // Claude Code calls its small and subagent models by Anthropic names; send them to this model.
                for key in [
                    "ANTHROPIC_MODEL",
                    "ANTHROPIC_DEFAULT_OPUS_MODEL",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL",
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
                    "CLAUDE_CODE_SUBAGENT_MODEL",
                ] {
                    env[key] = json!(model);
                }
            }
            out["env"] = env;
        }
        "codex" => {
            let (base, key_env) = match engine {
                "ollama" => (ollama_base_url(), None),
                "openai" if text(profile, "gateway_url").is_none() => (
                    text(profile, "base_url").unwrap_or("").to_string(),
                    text(profile, "key_env").map(str::to_string),
                ),
                _ => {
                    let (base, _) = gateway(profile, harness, engine)?;
                    (base, text(profile, "gateway_key_env").map(str::to_string))
                }
            };
            if base.is_empty() {
                bail!("{engine} has no base_url for the Codex harness.");
            }
            let mut provider = format!(
                "model_providers.donut={{name=\"Donut ({engine})\", base_url={}, wire_api=\"responses\"",
                toml_string(&base)
            );
            if let Some(key_env) = key_env {
                provider.push_str(&format!(", env_key={}", toml_string(&key_env)));
            }
            provider.push('}');
            out["codex_config"] = json!([provider, "model_provider=\"donut\""]);
        }
        other => bail!(
            "Unknown harness {other}. Use one of: {}.",
            HARNESSES.join(", ")
        ),
    }
    Ok(out)
}

/// The alias's gateway for a cross-vendor run: (base URL, key).
fn gateway(profile: &Value, harness: &str, engine: &str) -> Result<(String, String)> {
    let api = if harness == "claude" {
        "the Anthropic Messages API"
    } else {
        "the OpenAI Responses API"
    };
    let Some(url) = text(profile, "gateway_url") else {
        bail!(
            "A {engine} model needs a gateway to run in the {harness} harness: {engine} does not serve {api}. \
             Set gateway_url (and gateway_key_env) on this alias to a gateway that does, such as LiteLLM or \
             OpenRouter, or use /harness auto."
        );
    };
    let key = text(profile, "gateway_key_env")
        .and_then(|name| std::env::var(name).ok())
        .unwrap_or_default();
    Ok((url.trim_end_matches('/').to_string(), key))
}

fn toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_harness_or_the_same_one_leaves_the_profile_alone() {
        for profile in [
            json!({"engine": "claude", "model": "claude-opus-5-5"}),
            json!({"engine": "claude", "model": "m", "harness": "auto"}),
            json!({"engine": "codex", "model": "m", "harness": "codex"}),
        ] {
            assert_eq!(effective(&profile).unwrap(), profile);
        }
    }

    #[test]
    fn ollama_runs_in_claude_code_through_its_anthropic_api() {
        let out = effective(&json!({"engine": "ollama", "model": "qwen3.6", "harness": "claude"}))
            .unwrap();
        assert_eq!(out["engine"], "claude");
        assert_eq!(out["model"], "qwen3.6");
        assert!(
            !out["env"]["ANTHROPIC_BASE_URL"]
                .as_str()
                .unwrap()
                .ends_with("/v1")
        );
        assert_eq!(out["env"]["ANTHROPIC_AUTH_TOKEN"], "ollama");
        assert_eq!(out["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "qwen3.6");
    }

    #[test]
    fn ollama_and_openai_endpoints_run_in_codex_as_a_responses_provider() {
        let out = effective(&json!({"engine": "ollama", "model": "qwen3.6", "harness": "codex"}))
            .unwrap();
        assert_eq!(out["engine"], "codex");
        let config = out["codex_config"][0].as_str().unwrap();
        assert!(config.starts_with("model_providers.donut={"), "{config}");
        assert!(
            config.contains("/v1\", wire_api=\"responses\"}"),
            "{config}"
        );
        assert_eq!(out["codex_config"][1], "model_provider=\"donut\"");
        let out = effective(&json!({
            "engine": "openai", "model": "m", "harness": "codex",
            "base_url": "https://example.com/v1", "key_env": "EXAMPLE_KEY",
        }))
        .unwrap();
        let config = out["codex_config"][0].as_str().unwrap();
        assert!(
            config.contains("base_url=\"https://example.com/v1\""),
            "{config}"
        );
        assert!(config.contains("env_key=\"EXAMPLE_KEY\""), "{config}");
    }

    #[test]
    fn cross_vendor_needs_a_gateway() {
        let error = effective(&json!({"engine": "codex", "model": "gpt", "harness": "claude"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("gateway_url"), "{error}");
        let error = effective(&json!({"engine": "claude", "model": "opus", "harness": "codex"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("Responses API"), "{error}");
        let out = effective(&json!({
            "engine": "codex", "model": "gpt", "harness": "claude",
            "gateway_url": "http://localhost:4000/",
        }))
        .unwrap();
        assert_eq!(out["env"]["ANTHROPIC_BASE_URL"], "http://localhost:4000");
        let out = effective(&json!({
            "engine": "claude", "model": "opus", "harness": "codex",
            "gateway_url": "http://localhost:4000/v1", "gateway_key_env": "LITELLM_KEY",
        }))
        .unwrap();
        let config = out["codex_config"][0].as_str().unwrap();
        assert!(
            config.contains("base_url=\"http://localhost:4000/v1\""),
            "{config}"
        );
        assert!(config.contains("env_key=\"LITELLM_KEY\""), "{config}");
    }
}
