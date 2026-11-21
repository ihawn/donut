//! Read the model catalog exposed by a configured compatible endpoint.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::providers::validate;

/// The models a configured endpoint lists, as [{"id", "name"}].
pub fn endpoint_models(profile: &Value) -> Result<Vec<Value>> {
    let key_env = profile
        .get("key_env")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = std::env::var(key_env).ok();
    endpoint_models_with(profile, key.as_deref(), &get_json)
}

/// GET the url with a bearer token, and parse the body as JSON. Fifteen seconds at most.
fn get_json(url: &str, authorization: &str) -> Result<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .header("Authorization", authorization)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let body = response.body_mut().read_to_string()?;
    Ok(serde_json::from_str(&body)?)
}

/// endpoint_models() with the key and the HTTP GET given: get(url, authorization header value) -> JSON body.
pub fn endpoint_models_with(
    profile: &Value,
    key: Option<&str>,
    get: &dyn Fn(&str, &str) -> Result<Value>,
) -> Result<Vec<Value>> {
    validate(profile)?;
    let key_env = profile
        .get("key_env")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(key) = key.filter(|key| !key.is_empty()) else {
        bail!("Set {key_env} to discover this connection\u{2019}s models.");
    };
    let base = profile
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let url = format!("{}/models", base.trim_end_matches('/'));
    let payload = get(&url, &format!("Bearer {key}"))?;
    let Some(rows) = payload.get("data").and_then(Value::as_array) else {
        bail!("Provider returned an unsupported model catalog.");
    };
    Ok(rows
        .iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str))
        .map(|id| json!({"id": id, "name": id}))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn test_catalog_uses_connection_key_and_validates_model_records() {
        let profile = json!({
            "engine": "openai",
            "model": "existing",
            "base_url": "https://example.com/v1",
            "key_env": "DONUT_TEST_KEY",
        });
        let seen = RefCell::new(Vec::new());
        let get = |url: &str, authorization: &str| -> Result<Value> {
            seen.borrow_mut()
                .push((url.to_string(), authorization.to_string()));
            Ok(serde_json::from_str(
                r#"{"data":[{"id":"one"},{"id":"two"},{"id":4},null]}"#,
            )?)
        };
        let models = endpoint_models_with(&profile, Some("fixture"), &get).unwrap();
        let ids: Vec<&str> = models
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["one", "two"]);
        assert_eq!(
            seen.borrow()[0],
            (
                "https://example.com/v1/models".to_string(),
                "Bearer fixture".to_string()
            )
        );
    }

    #[test]
    fn test_missing_key_and_unsupported_catalogs_are_refused() {
        let profile = json!({
            "engine": "openai", "model": "m", "base_url": "https://example.com/v1", "key_env": "DONUT_TEST_KEY"
        });
        let never = |_: &str, _: &str| -> Result<Value> { panic!("no request without a key") };
        let error = endpoint_models_with(&profile, None, &never).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("Set DONUT_TEST_KEY to discover")
        );
        let odd = |_: &str, _: &str| -> Result<Value> { Ok(json!({"data": "nope"})) };
        let error = endpoint_models_with(&profile, Some("k"), &odd).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Provider returned an unsupported model catalog."
        );
    }
}
