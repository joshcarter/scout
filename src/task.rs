use serde_json::{json, Value};
use std::time::Instant;

use crate::client::{LlmClient, LlmError};

/// The generic task primitive: given `system` and `user` prompt text, call the
/// local LLM and return its response. Backs both the ad-hoc `scout task`
/// CLI escape hatch and (in a later step) preset dispatch.
pub fn handle(client: &LlmClient, params: &Value) -> Result<Value, LlmError> {
    let system = params["system"]
        .as_str()
        .ok_or_else(|| LlmError::Internal("task: missing required 'system' param".into()))?;
    let user = params["user"]
        .as_str()
        .ok_or_else(|| LlmError::Internal("task: missing required 'user' param".into()))?;

    let messages = vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": user}),
    ];

    let max_tokens = params["max_tokens"].as_u64();

    let start = Instant::now();
    let (text, usage) = client.complete(messages, max_tokens)?;
    let duration_ms = start.elapsed().as_millis() as u64;

    if text.trim().is_empty() {
        return Err(LlmError::Internal("LLM returned empty response".into()));
    }

    Ok(json!({
        "text": text.trim(),
        "usage": usage,
        "model": client.model(),
        "duration_ms": duration_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Config;
    use std::time::Duration;

    fn dead_client() -> LlmClient {
        LlmClient::new(Config {
            endpoint: "http://127.0.0.1:1/v1".into(),
            model: "test-model".into(),
            timeout: Duration::from_secs(2),
            api_key: None,
            max_tokens: None,
        })
    }

    #[test]
    fn handle_missing_system_returns_error() {
        let client = dead_client();
        let err = handle(&client, &serde_json::json!({"user": "hello"})).unwrap_err();
        assert!(matches!(err, LlmError::Internal(_)));
    }

    #[test]
    fn handle_missing_user_returns_error() {
        let client = dead_client();
        let err = handle(&client, &serde_json::json!({"system": "be helpful"})).unwrap_err();
        assert!(matches!(err, LlmError::Internal(_)));
    }

    #[test]
    fn handle_dead_endpoint_returns_endpoint_unavailable() {
        let client = dead_client();
        let err = handle(
            &client,
            &serde_json::json!({
                "system": "You are a helpful assistant.",
                "user": "Say hello."
            }),
        )
        .unwrap_err();
        assert!(matches!(err, LlmError::EndpointUnavailable { .. }));
    }
}
