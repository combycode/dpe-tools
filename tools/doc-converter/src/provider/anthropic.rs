//! Anthropic Messages API — inline-image request.
//!
//!   POST https://api.anthropic.com/v1/messages
//!   headers: x-api-key, anthropic-version: 2023-06-01, content-type: application/json
//!   body: { model, max_tokens, system, messages: [{role:"user", content:[<blocks>]}], temperature, thinking? }
//!
//! Modern Messages API. NOT Completions (the legacy /v1/complete endpoint is
//! not used anywhere in this tool).

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

use crate::settings::RetryCfg;
use super::{BatchRequest, BatchResult, BatchStatus, ModelCfg, Provider, ProviderError, UserContent, retry_call};

pub struct AnthropicProvider {
    pub client:           Client,
    pub api_key:          String,
    pub endpoint:         String,
    /// Base URL for the Messages Batches API. Default points at the
    /// production endpoint; tests override via `with_batch_endpoint`
    /// to direct submission/poll/results to a wiremock instance.
    /// Submission posts here directly; status polls hit
    /// `{batch_endpoint}/{id}`; results fetch hits
    /// `{batch_endpoint}/{id}/results`.
    pub batch_endpoint:   String,
    /// Retry policy applied to every HTTP call. Set at construction;
    /// see `Settings.retry` for the user-facing schema.
    pub retry:            RetryCfg,
}

impl AnthropicProvider {
    /// Construct with the user-specified per-request timeout and
    /// retry policy. The reqwest client is built with the timeout
    /// baked in; each call site wraps in `retry_call`.
    pub fn new(api_key: String, timeout: Duration, retry: RetryCfg) -> Self {
        let client = super::http_client_builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client");
        Self {
            client,
            api_key,
            endpoint:       "https://api.anthropic.com/v1/messages".to_string(),
            batch_endpoint: "https://api.anthropic.com/v1/messages/batches".to_string(),
            retry,
        }
    }

    pub fn with_endpoint(mut self, endpoint: String) -> Self {
        self.endpoint = endpoint; self
    }

    pub fn with_batch_endpoint(mut self, endpoint: String) -> Self {
        self.batch_endpoint = endpoint; self
    }

    /// Test-only constructor with a 30s timeout and retry disabled
    /// (max_attempts=1). Production code paths always go through `new`.
    #[cfg(test)]
    pub fn new_test(api_key: String) -> Self {
        let retry = RetryCfg { max_attempts: 1, ..RetryCfg::default() };
        Self::new(api_key, Duration::from_secs(30), retry)
    }

    pub fn build_body(&self, system: &str, user: &UserContent, cfg: &ModelCfg) -> Value {
        let content = build_content_blocks(user);
        let thinking_on = cfg.thinking_budget.unwrap_or(0) > 0;
        let temperature = if thinking_on { 1.0 } else { cfg.temperature };
        let mut body = json!({
            "model": cfg.model,
            "max_tokens": cfg.max_tokens,
            "system": system,
            "messages": [{"role": "user", "content": content}],
            "temperature": temperature,
        });
        if thinking_on {
            body["thinking"] = json!({"type": "enabled", "budget_tokens": cfg.thinking_budget});
        }
        body
    }
}

fn build_content_blocks(user: &UserContent) -> Value {
    match user {
        UserContent::TextOnly(t) => json!([
            {"type": "text", "text": t}
        ]),
        UserContent::InlineImage { bytes, mime, text } => {
            let data = STANDARD.encode(bytes);
            let img = json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime,
                    "data": data,
                }
            });
            if let Some(t) = text {
                json!([img, {"type": "text", "text": t}])
            } else {
                json!([img])
            }
        }
        UserContent::InlineDocument { bytes, mime, text } => {
            // Anthropic Messages API `document` content block. PDF
            // support is GA on Anthropic — no beta header needed.
            // The model parses pages itself; we send the raw bytes.
            let data = STANDARD.encode(bytes);
            let doc = json!({
                "type": "document",
                "source": {
                    "type": "base64",
                    "media_type": mime,
                    "data": data,
                }
            });
            if let Some(t) = text {
                json!([doc, {"type": "text", "text": t}])
            } else {
                json!([doc])
            }
        }
    }
}

pub fn extract_text(data: &Value) -> Result<String, ProviderError> {
    let content = data.get("content").and_then(|c| c.as_array())
        .ok_or_else(|| ProviderError::Parse(format!(
            "anthropic: no content array: {}",
            truncate(&data.to_string(), 200))))?;
    let mut out = String::new();
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
            }
        }
    }
    if out.is_empty() {
        return Err(ProviderError::Parse("anthropic: no text blocks in response".into()));
    }
    Ok(out)
}

fn truncate(s: &str, n: usize) -> String {
    // Byte-level slicing (`&s[..n]`) can land mid-UTF-8 codepoint and
    // panic when the response body contains multi-byte chars (any non-
    // ASCII payload). Walk char boundaries instead so error-message
    // formatting can't kill the tool.
    if s.len() <= n {
        s.to_string()
    } else {
        let prefix: String = s.chars().scan(0usize, |acc, c| {
            let next = *acc + c.len_utf8();
            if next > n { None } else { *acc = next; Some(c) }
        }).collect();
        format!("{prefix}...")
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn message_single(
        &self,
        system: &str,
        user: UserContent,
        cfg: &ModelCfg,
    ) -> Result<String, ProviderError> {
        let body = self.build_body(system, &user, cfg);
        retry_call(&self.retry, "anthropic.message_single", || async {
            let resp = self.client.post(&self.endpoint)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send().await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(ProviderError::Api {
                    provider: "anthropic",
                    status: status.as_u16(),
                    body: truncate(&body, 500),
                });
            }
            let json: Value = resp.json().await?;
            extract_text(&json)
        }).await
    }

    async fn submit_batch(
        &self,
        requests: Vec<BatchRequest>,
        cfg: &ModelCfg,
    ) -> Result<String, ProviderError> {
        // Anthropic Messages Batches API:
        //   POST /v1/messages/batches
        //   { "requests": [{ "custom_id": "...", "params": <single-message-body> }, ...] }
        // The `params` shape is the EXACT body shape of /v1/messages.
        let mut req_arr: Vec<Value> = Vec::with_capacity(requests.len());
        for r in requests {
            let params = self.build_body(&r.system, &r.user, cfg);
            req_arr.push(json!({
                "custom_id": r.custom_id,
                "params":    params,
            }));
        }
        let body = json!({ "requests": req_arr });

        retry_call(&self.retry, "anthropic.submit_batch", || async {
            let resp = self.client.post(&self.batch_endpoint)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send().await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(ProviderError::Api {
                    provider: "anthropic",
                    status: status.as_u16(),
                    body: truncate(&body, 500),
                });
            }
            let json: Value = resp.json().await?;
            let id = json.get("id").and_then(|v| v.as_str())
                .ok_or_else(|| ProviderError::Parse(format!(
                    "anthropic: batch submission missing id: {}", truncate(&json.to_string(), 200))))?;
            Ok(id.to_string())
        }).await
    }

    async fn poll_batch(
        &self,
        batch_id: &str,
    ) -> Result<BatchStatus, ProviderError> {
        // Step 1: GET /v1/messages/batches/{id} for status.
        let status_url = format!("{}/{}", self.batch_endpoint, batch_id);
        let status_json: Value = retry_call(&self.retry, "anthropic.poll_batch.status", || async {
            let resp = self.client.get(&status_url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .send().await?;
            let http_status = resp.status();
            if !http_status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(ProviderError::Api {
                    provider: "anthropic",
                    status: http_status.as_u16(),
                    body: truncate(&body, 500),
                });
            }
            let json: Value = resp.json().await?;
            Ok(json)
        }).await?;

        let processing = status_json.get("processing_status")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProviderError::Parse(format!(
                "anthropic: batch status missing processing_status: {}",
                truncate(&status_json.to_string(), 200))))?;

        // Counters are advisory — Anthropic exposes per-status counts
        // under `request_counts: {processing, succeeded, errored,
        // canceled, expired}`. We sum them for `total` and use
        // `processing` for the "still working" indicator.
        let counts = status_json.get("request_counts");
        let processed: u32 = counts
            .and_then(|c| c.as_object())
            .map(|m| m.iter()
                 .filter(|(k, _)| *k != "processing")
                 .filter_map(|(_, v)| v.as_u64()).sum::<u64>() as u32)
            .unwrap_or(0);
        let total: u32 = counts
            .and_then(|c| c.as_object())
            .map(|m| m.values().filter_map(|v| v.as_u64()).sum::<u64>() as u32)
            .unwrap_or(0);

        match processing {
            "in_progress" | "canceling" => {
                Ok(BatchStatus::InProgress { processed, total })
            }
            "ended" => {
                // Step 2: fetch results NDJSON.
                let results_url = format!("{}/{}/results", self.batch_endpoint, batch_id);
                let body: String = retry_call(&self.retry, "anthropic.poll_batch.results", || async {
                    let resp = self.client.get(&results_url)
                        .header("x-api-key", &self.api_key)
                        .header("anthropic-version", "2023-06-01")
                        .send().await?;
                    let http_status = resp.status();
                    if !http_status.is_success() {
                        let body = resp.text().await.unwrap_or_default();
                        return Err(ProviderError::Api {
                            provider: "anthropic",
                            status: http_status.as_u16(),
                            body: truncate(&body, 500),
                        });
                    }
                    let body = resp.text().await?;
                    Ok(body)
                }).await?;
                let results = parse_batch_results_ndjson(&body)?;
                Ok(BatchStatus::Completed { results })
            }
            other => Err(ProviderError::Other(format!(
                "anthropic: unknown processing_status '{}'", other
            ))),
        }
    }
}

/// Parse Anthropic batch results NDJSON (one JSON object per line).
/// Each line: `{"custom_id":"...","result":{"type":"succeeded"|"errored"|"canceled"|"expired", ...}}`.
/// "succeeded" carries `result.message.content[]` (same shape as
/// /v1/messages); "errored" carries `result.error.message`; others
/// surface as per-item errors with the type as the reason.
fn parse_batch_results_ndjson(body: &str) -> Result<Vec<BatchResult>, ProviderError> {
    let mut out = Vec::new();
    for (lineno, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let v: Value = serde_json::from_str(line).map_err(|e| ProviderError::Parse(format!(
            "anthropic: batch results line {} parse: {}", lineno + 1, e
        )))?;
        let custom_id = v.get("custom_id").and_then(|c| c.as_str())
            .ok_or_else(|| ProviderError::Parse(format!(
                "anthropic: batch results line {} missing custom_id", lineno + 1
            )))?
            .to_string();
        let result_obj = v.get("result").and_then(|r| r.as_object())
            .ok_or_else(|| ProviderError::Parse(format!(
                "anthropic: batch results line {} missing result obj", lineno + 1
            )))?;
        let kind = result_obj.get("type").and_then(|t| t.as_str()).unwrap_or("unknown");
        let outcome = match kind {
            "succeeded" => {
                let msg = result_obj.get("message")
                    .ok_or_else(|| ProviderError::Parse(format!(
                        "anthropic: batch line {} succeeded but no message", lineno + 1
                    )))?;
                extract_text(msg).map_err(|e| match e {
                    ProviderError::Parse(p) => ProviderError::Parse(format!(
                        "anthropic: batch line {} extract: {}", lineno + 1, p
                    )),
                    other => other,
                }).map_err(|e| format!("{e}"))
            }
            "errored" => {
                let msg = result_obj.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("(no error message)");
                Err(format!("provider error: {msg}"))
            }
            "canceled" => Err("canceled".to_string()),
            "expired"  => Err("expired".to_string()),
            other      => Err(format!("unknown result type: {other}")),
        };
        out.push(BatchResult { custom_id, outcome });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{MockServer, Mock, ResponseTemplate};
    use wiremock::matchers::{method, path, header};

    fn cfg() -> ModelCfg {
        ModelCfg {
            model: "claude-haiku-4-5-20251001".into(),
            temperature: 0.0,
            max_tokens: 1000,
            thinking_budget: None,
        }
    }

    #[test]
    fn body_uses_modern_messages_api_not_completions() {
        let p = AnthropicProvider::new_test("k".into());
        let body = p.build_body("SYSTEM", &UserContent::TextOnly("USER".into()), &cfg());
        // Modern API keys:
        assert!(body["model"].is_string());
        assert!(body["max_tokens"].is_number());
        assert_eq!(body["system"], "SYSTEM");
        assert_eq!(body["messages"][0]["role"], "user");
        // NOT legacy Completions API:
        assert!(body.get("prompt").is_none());
        assert!(body.get("max_tokens_to_sample").is_none());
    }

    #[test]
    fn body_inline_image_content_shape() {
        let p = AnthropicProvider::new_test("k".into());
        let body = p.build_body("S",
            &UserContent::InlineImage {
                bytes: vec![1, 2, 3],
                mime: "image/jpeg".into(),
                text: Some("caption".into()),
            },
            &cfg(),
        );
        let blocks = &body["messages"][0]["content"];
        assert!(blocks.is_array());
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["type"], "base64");
        assert_eq!(blocks[0]["source"]["media_type"], "image/jpeg");
        assert_eq!(blocks[0]["source"]["data"], "AQID");   // base64 of [1,2,3]
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "caption");
    }

    #[test]
    fn body_inline_document_content_shape() {
        // Phase 3: whole-doc mode sends the raw PDF as a `document`
        // content block. Different `type` from images, same base64
        // source shape.
        let p = AnthropicProvider::new_test("k".into());
        let body = p.build_body("S",
            &UserContent::InlineDocument {
                bytes: vec![0x25, 0x50, 0x44, 0x46],          // "%PDF" magic
                mime: "application/pdf".into(),
                text: None,
            },
            &cfg(),
        );
        let blocks = &body["messages"][0]["content"];
        assert!(blocks.is_array());
        assert_eq!(blocks.as_array().unwrap().len(), 1,
            "no text caption was provided; only the document block is sent");
        assert_eq!(blocks[0]["type"], "document",
            "PDF MUST be a document block, NOT an image block");
        assert_eq!(blocks[0]["source"]["type"], "base64");
        assert_eq!(blocks[0]["source"]["media_type"], "application/pdf");
        assert_eq!(blocks[0]["source"]["data"], "JVBERg==",   // base64 of "%PDF"
            "data must be raw-bytes base64");
    }

    #[test]
    fn body_inline_document_with_text_caption() {
        let p = AnthropicProvider::new_test("k".into());
        let body = p.build_body("S",
            &UserContent::InlineDocument {
                bytes: vec![0x25, 0x50, 0x44, 0x46],
                mime: "application/pdf".into(),
                text: Some("convert this".into()),
            },
            &cfg(),
        );
        let blocks = &body["messages"][0]["content"];
        assert_eq!(blocks.as_array().unwrap().len(), 2);
        assert_eq!(blocks[0]["type"], "document");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "convert this");
    }

    #[test]
    fn body_thinking_budget_sets_temperature_to_one() {
        let p = AnthropicProvider::new_test("k".into());
        let c = ModelCfg { thinking_budget: Some(8000), ..cfg() };
        let body = p.build_body("s", &UserContent::TextOnly("u".into()), &c);
        assert_eq!(body["temperature"], 1.0);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 8000);
    }

    #[test]
    fn extract_text_single_block() {
        let d: Value = serde_json::from_str(r#"{"content":[{"type":"text","text":"hi"}]}"#).unwrap();
        assert_eq!(extract_text(&d).unwrap(), "hi");
    }

    #[test]
    fn extract_text_joins_multiple_and_ignores_non_text() {
        let d: Value = serde_json::from_str(r#"{
            "content":[
                {"type":"thinking","thinking":"..."},
                {"type":"text","text":"A"},
                {"type":"text","text":"B"}
            ]
        }"#).unwrap();
        assert_eq!(extract_text(&d).unwrap(), "AB");
    }

    #[test]
    fn extract_text_missing_content_errors() {
        let d: Value = serde_json::from_str("{}").unwrap();
        assert!(matches!(extract_text(&d), Err(ProviderError::Parse(_))));
    }

    #[tokio::test]
    async fn sends_correct_headers_and_receives_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "SECRET"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type":"text","text":"ok"}]
            })))
            .mount(&server).await;

        let p = AnthropicProvider::new_test("SECRET".into())
            .with_endpoint(format!("{}/v1/messages", server.uri()));
        let out = p.message_single("s", UserContent::TextOnly("u".into()), &cfg()).await.unwrap();
        assert_eq!(out, "ok");
    }

    #[tokio::test]
    async fn http_4xx_becomes_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server).await;
        let p = AnthropicProvider::new_test("k".into())
            .with_endpoint(format!("{}/v1/messages", server.uri()));
        let err = p.message_single("s", UserContent::TextOnly("u".into()), &cfg()).await.unwrap_err();
        match err {
            ProviderError::Api { status, .. } => assert_eq!(status, 400),
            _ => panic!("wrong error variant"),
        }
    }

    // ─── Retry behavior ─────────────────────────────────

    /// Build an AnthropicProvider wired for retry tests: tiny backoff
    /// so the test finishes in milliseconds even if max_attempts > 1.
    fn retry_provider(max_attempts: u32) -> AnthropicProvider {
        let retry = RetryCfg {
            max_attempts,
            initial_backoff_ms: 1,
            max_backoff_ms:     1,
            retry_on: vec![
                crate::settings::RetryTrigger::RateLimit,
                crate::settings::RetryTrigger::ServerError,
                crate::settings::RetryTrigger::Timeout,
                crate::settings::RetryTrigger::ConnectionError,
            ],
        };
        AnthropicProvider::new("SECRET".into(), Duration::from_secs(30), retry)
    }

    #[tokio::test]
    async fn retry_on_429_then_succeeds() {
        // First call returns 429 (rate-limit); second returns 200. With
        // max_attempts=3, the retry wrapper retries once and succeeds.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .up_to_n_times(1)
            .mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type":"text","text":"after retry"}]
            })))
            .mount(&server).await;

        let p = retry_provider(3).with_endpoint(format!("{}/v1/messages", server.uri()));
        let out = p.message_single("s", UserContent::TextOnly("u".into()), &cfg()).await.unwrap();
        assert_eq!(out, "after retry");
    }

    #[tokio::test]
    async fn retry_exhausted_on_persistent_503() {
        // Every call returns 503. With max_attempts=2 the wrapper tries
        // twice, then surfaces the final 503 as an Api error.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
            .mount(&server).await;

        let p = retry_provider(2).with_endpoint(format!("{}/v1/messages", server.uri()));
        let err = p.message_single("s", UserContent::TextOnly("u".into()), &cfg()).await.unwrap_err();
        match err {
            ProviderError::Api { status, .. } => assert_eq!(status, 503),
            _ => panic!("wrong error variant: {err:?}"),
        }
    }

    #[tokio::test]
    async fn no_retry_on_400_bad_request() {
        // 400 is NOT in any retry trigger class. Even with
        // max_attempts=3 the wrapper surfaces 400 immediately on the
        // first failure. The mock is configured with `expect(1)` to
        // assert exactly one request hit the server.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .expect(1)
            .mount(&server).await;

        let p = retry_provider(3).with_endpoint(format!("{}/v1/messages", server.uri()));
        let err = p.message_single("s", UserContent::TextOnly("u".into()), &cfg()).await.unwrap_err();
        match err {
            ProviderError::Api { status, .. } => assert_eq!(status, 400),
            _ => panic!("wrong error variant: {err:?}"),
        }
        // Mock's expect(1) asserted at server drop — exactly one call.
    }

    #[test]
    fn is_retryable_classifies_correctly() {
        use crate::settings::RetryTrigger as T;
        let all = &[T::RateLimit, T::ServerError, T::Timeout, T::ConnectionError];
        let none: &[T] = &[];

        let api_429 = ProviderError::Api { provider: "anthropic", status: 429, body: "".into() };
        assert!(crate::provider::is_retryable(&api_429, all));
        assert!(!crate::provider::is_retryable(&api_429, none));
        assert!(crate::provider::is_retryable(&api_429, &[T::RateLimit]));
        assert!(!crate::provider::is_retryable(&api_429, &[T::ServerError]));

        let api_503 = ProviderError::Api { provider: "anthropic", status: 503, body: "".into() };
        assert!(crate::provider::is_retryable(&api_503, all));
        assert!(!crate::provider::is_retryable(&api_503, &[T::RateLimit]));
        assert!(crate::provider::is_retryable(&api_503, &[T::ServerError]));

        let api_400 = ProviderError::Api { provider: "anthropic", status: 400, body: "".into() };
        assert!(!crate::provider::is_retryable(&api_400, all));

        let api_404 = ProviderError::Api { provider: "anthropic", status: 404, body: "".into() };
        assert!(!crate::provider::is_retryable(&api_404, all));

        let parse = ProviderError::Parse("bad json".into());
        assert!(!crate::provider::is_retryable(&parse, all));

        let other = ProviderError::Other("misc".into());
        assert!(!crate::provider::is_retryable(&other, all));
    }
}
