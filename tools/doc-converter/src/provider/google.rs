//! Google Gemini generateContent — inline-image request.
//!
//!   POST https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key=<KEY>
//!   body: {
//!     "contents":          [{ "role": "user",
//!                             "parts": [ {"inlineData":{"mimeType","data"}}, {"text"} ] }],
//!     "systemInstruction": { "parts": [{"text": "<system>"}] },
//!     "generationConfig":  { "temperature", "maxOutputTokens", "thinkingConfig"? }
//!   }

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

use crate::settings::RetryCfg;
use super::{BatchRequest, BatchResult, BatchStatus, ModelCfg, Provider, ProviderError, UserContent, retry_call};

pub struct GoogleProvider {
    pub client:        Client,
    pub api_key:       String,
    pub endpoint_base: String,
    /// Retry policy applied to every HTTP call. Set at construction;
    /// see `Settings.retry` for the user-facing schema.
    pub retry:         RetryCfg,
}

impl GoogleProvider {
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
            endpoint_base: "https://generativelanguage.googleapis.com".to_string(),
            retry,
        }
    }
    pub fn with_endpoint(mut self, endpoint_base: String) -> Self {
        self.endpoint_base = endpoint_base; self
    }

    /// Test-only constructor with a 30s timeout and retry disabled
    /// (max_attempts=1). Production code paths always go through `new`.
    #[cfg(test)]
    pub fn new_test(api_key: String) -> Self {
        let retry = RetryCfg { max_attempts: 1, ..RetryCfg::default() };
        Self::new(api_key, Duration::from_secs(30), retry)
    }

    pub fn build_url(&self, model: &str) -> String {
        format!(
            "{}/v1beta/models/{}:generateContent?key={}",
            self.endpoint_base,
            urlencode(model),
            urlencode(&self.api_key),
        )
    }

    /// Batch submit URL — one POST to start a batch job (inline mode).
    /// Per Gemini docs: `/v1beta/models/{model}:batchGenerateContent`.
    pub fn build_batch_submit_url(&self, model: &str) -> String {
        format!(
            "{}/v1beta/models/{}:batchGenerateContent?key={}",
            self.endpoint_base,
            urlencode(model),
            urlencode(&self.api_key),
        )
    }

    /// Batch status URL. `batch_name` is the `name` field from the
    /// submission response, formatted like `batches/12345`. Not just
    /// the bare ID — Gemini's REST conventions take the resource path.
    pub fn build_batch_status_url(&self, batch_name: &str) -> String {
        format!(
            "{}/v1beta/{}?key={}",
            self.endpoint_base,
            batch_name,                   // already includes `batches/...`
            urlencode(&self.api_key),
        )
    }

    pub fn build_body(&self, system: &str, user: &UserContent, cfg: &ModelCfg) -> Value {
        let parts = build_parts(user);
        let mut gen_cfg = json!({ "maxOutputTokens": cfg.max_tokens });
        if let Some(t) = cfg.temperature {
            gen_cfg["temperature"] = json!(t);
        }
        if cfg.thinking_budget.unwrap_or(0) > 0 {
            gen_cfg["thinkingConfig"] = json!({ "thinkingBudget": cfg.thinking_budget });
        }
        json!({
            "contents": [{ "role": "user", "parts": parts }],
            "systemInstruction": { "parts": [{ "text": system }] },
            "generationConfig": gen_cfg,
        })
    }
}

fn urlencode(s: &str) -> String {
    // Minimal URL-encoding for query-string + path segment (alphanumerics, dashes, underscores pass through).
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let ok = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if ok { out.push(b as char); }
        else  { out.push_str(&format!("%{:02X}", b)); }
    }
    out
}

fn build_parts(user: &UserContent) -> Value {
    match user {
        UserContent::TextOnly(t) => json!([{"text": t}]),
        UserContent::InlineImage { bytes, mime, text } => {
            let data = STANDARD.encode(bytes);
            let img = json!({
                "inlineData": { "mimeType": mime, "data": data }
            });
            if let Some(t) = text {
                json!([img, {"text": t}])
            } else {
                json!([img])
            }
        }
        UserContent::InlineDocument { bytes, mime, text } => {
            // Gemini accepts PDFs via the same `inlineData` shape as
            // images — only the mime type changes. The model handles
            // page extraction internally; we just stream the raw bytes.
            let data = STANDARD.encode(bytes);
            let doc = json!({
                "inlineData": { "mimeType": mime, "data": data }
            });
            if let Some(t) = text {
                json!([doc, {"text": t}])
            } else {
                json!([doc])
            }
        }
    }
}

pub fn extract_text(data: &Value) -> Result<String, ProviderError> {
    // No candidates at all — surface the response body. Common when the
    // upstream rejected the request shape (auth, rate limit shapes that
    // don't 4xx, malformed body) instead of returning a content-empty
    // candidate.
    let cands = data.get("candidates").and_then(|c| c.as_array())
        .filter(|a| !a.is_empty())
        .ok_or_else(|| ProviderError::Parse(format!(
            "google: no candidates: {}", truncate(&data.to_string(), 400))))?;

    // Missing parts on candidate[0] — almost always means the model hit
    // a finishReason that aborted generation. MAX_TOKENS, SAFETY,
    // RECITATION, OTHER each need different mitigations; surface the
    // finishReason and the candidate body so the caller can pick the
    // right one (bump max_tokens vs switch model vs different prompt).
    let parts = cands[0].get("content").and_then(|c| c.get("parts")).and_then(|p| p.as_array())
        .ok_or_else(|| {
            let finish = cands[0].get("finishReason")
                .and_then(|v| v.as_str()).unwrap_or("?");
            let cand = serde_json::to_string(&cands[0]).unwrap_or_default();
            ProviderError::Parse(format!(
                "google: missing candidate.content.parts (finishReason={}, candidate={})",
                finish, truncate(&cand, 400),
            ))
        })?;
    let mut out = String::new();
    for p in parts {
        if let Some(t) = p.get("text").and_then(|t| t.as_str()) { out.push_str(t); }
    }
    if out.is_empty() {
        // Parts existed but were all non-text (image, audio, etc) — same
        // diagnostics value: surface finishReason + candidate so the
        // caller knows whether the model returned non-text output (e.g.
        // a tool-use call) vs an empty text response.
        let finish = cands[0].get("finishReason")
            .and_then(|v| v.as_str()).unwrap_or("?");
        let cand = serde_json::to_string(&cands[0]).unwrap_or_default();
        return Err(ProviderError::Parse(format!(
            "google: no text parts (finishReason={}, candidate={})",
            finish, truncate(&cand, 400),
        )));
    }
    Ok(out)
}

fn truncate(s: &str, n: usize) -> String {
    // Byte-level slicing can land mid-UTF-8 codepoint and panic on
    // multi-byte chars. Walk char boundaries instead so error-message
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
impl Provider for GoogleProvider {
    async fn message_single(
        &self,
        system: &str,
        user: UserContent,
        cfg: &ModelCfg,
    ) -> Result<String, ProviderError> {
        let url = self.build_url(&cfg.model);
        let body = self.build_body(system, &user, cfg);
        retry_call(&self.retry, "google.message_single", || async {
            let resp = self.client.post(&url)
                .header("content-type", "application/json")
                .json(&body)
                .send().await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(ProviderError::Api {
                    provider: "google",
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
        // Inline-mode submission. Per Gemini docs:
        //   POST /v1beta/models/{model}:batchGenerateContent
        //   { "batch": { "input_config": { "requests": { "requests": [
        //     {"request": <generateContent body>, "metadata": {"key": "<custom_id>"}}
        //   ]}}}}
        // Returns: { "name": "batches/12345", "state": "JOB_STATE_PENDING", ... }
        let mut req_arr: Vec<Value> = Vec::with_capacity(requests.len());
        for r in requests {
            // For batch we pack the request body WITHOUT systemInstruction
            // at the URL level; instead it goes inside `request.system_instruction`
            // exactly the same shape as the streaming path. build_body already
            // produces the right shape, so we reuse it and rename keys to
            // snake_case (Gemini batch endpoint requires snake_case for nested
            // request fields, mirroring the JSONL file-mode shape).
            let body = self.build_body(&r.system, &r.user, cfg);
            // build_body emits camelCase (systemInstruction, generationConfig);
            // batch inline accepts both for the inner `request`, but to mirror
            // the documented shape we pass it through as-is.
            req_arr.push(json!({
                "request":  body,
                "metadata": { "key": r.custom_id },
            }));
        }
        let url = self.build_batch_submit_url(&cfg.model);
        let body = json!({
            "batch": {
                "display_name": "doc-converter-batch",
                "input_config": {
                    "requests": { "requests": req_arr }
                }
            }
        });
        retry_call(&self.retry, "google.submit_batch", || async {
            let resp = self.client.post(&url)
                .header("content-type", "application/json")
                .json(&body)
                .send().await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(ProviderError::Api {
                    provider: "google",
                    status: status.as_u16(),
                    body: truncate(&body, 500),
                });
            }
            let json: Value = resp.json().await?;
            let name = json.get("name").and_then(|n| n.as_str())
                .ok_or_else(|| ProviderError::Parse(format!(
                    "google: batch submission missing name: {}",
                    truncate(&json.to_string(), 200))))?;
            Ok(name.to_string())
        }).await
    }

    async fn poll_batch(
        &self,
        batch_id: &str,
    ) -> Result<BatchStatus, ProviderError> {
        let url = self.build_batch_status_url(batch_id);
        let json: Value = retry_call(&self.retry, "google.poll_batch", || async {
            let resp = self.client.get(&url).send().await?;
            let http_status = resp.status();
            if !http_status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(ProviderError::Api {
                    provider: "google",
                    status: http_status.as_u16(),
                    body: truncate(&body, 500),
                });
            }
            let json: Value = resp.json().await?;
            Ok(json)
        }).await?;

        // State name. Gemini exposes it as either a bare string at
        // `state` OR (some SDKs) `state.name`. Try both.
        let state_str = json.get("state")
            .and_then(|s| s.as_str()
                .map(|s| s.to_string())
                .or_else(|| s.get("name").and_then(|n| n.as_str()).map(|n| n.to_string())))
            .ok_or_else(|| ProviderError::Parse(format!(
                "google: batch status missing state: {}",
                truncate(&json.to_string(), 200))))?;

        match state_str.as_str() {
            "JOB_STATE_PENDING" | "JOB_STATE_RUNNING" => {
                // Gemini doesn't expose request_counts in the status.
                // Caller ignores 0/0 — use the terminal variants.
                Ok(BatchStatus::InProgress { processed: 0, total: 0 })
            }
            "JOB_STATE_SUCCEEDED" => {
                let inlined = json
                    .get("dest")
                    .and_then(|d| d.get("inlined_responses"))
                    .and_then(|i| i.as_array())
                    .ok_or_else(|| ProviderError::Parse(format!(
                        "google: batch SUCCEEDED but no dest.inlined_responses: {}",
                        truncate(&json.to_string(), 200))))?;

                // Per the Gemini docs the per-item `metadata.key` we
                // sent at submission is echoed in each inlined_response;
                // try that first, fall back to position-based "key_<i>".
                let mut results = Vec::with_capacity(inlined.len());
                for (idx, item) in inlined.iter().enumerate() {
                    let custom_id = item
                        .get("metadata").and_then(|m| m.get("key"))
                        .and_then(|k| k.as_str())
                        .map(String::from)
                        // Some response shapes lift `key` to the top level.
                        .or_else(|| item.get("key")
                                       .and_then(|k| k.as_str())
                                       .map(String::from))
                        .unwrap_or_else(|| format!("key_{idx}"));

                    let outcome = if let Some(err) = item.get("error") {
                        let msg = err.get("message").and_then(|m| m.as_str())
                            .unwrap_or("(no error message)");
                        Err(format!("provider error: {msg}"))
                    } else if let Some(resp_obj) = item.get("response") {
                        // The same shape generateContent returns.
                        extract_text(resp_obj).map_err(|e| format!("{e}"))
                    } else {
                        Err(format!("malformed item at position {idx}: {}",
                            truncate(&item.to_string(), 200)))
                    };
                    results.push(BatchResult { custom_id, outcome });
                }
                Ok(BatchStatus::Completed { results })
            }
            "JOB_STATE_FAILED" | "JOB_STATE_CANCELLED" => {
                let reason = json.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("(no reason given)");
                Ok(BatchStatus::Failed { reason: reason.to_string() })
            }
            "JOB_STATE_EXPIRED" => Ok(BatchStatus::Expired),
            other => Err(ProviderError::Other(format!(
                "google: unknown batch state '{}'", other
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{MockServer, Mock, ResponseTemplate};
    use wiremock::matchers::{method, path};

    fn cfg() -> ModelCfg {
        ModelCfg {
            model: "gemini-3-flash-preview".into(),
            temperature: Some(0.0),
            max_tokens: 1000,
            thinking_budget: None,
            thinking_adaptive: false,
            effort: None,
        }
    }

    #[test]
    fn url_has_model_and_key() {
        let p = GoogleProvider::new_test("SECRET".into());
        let u = p.build_url("gemini-3-flash-preview");
        assert!(u.ends_with("gemini-3-flash-preview:generateContent?key=SECRET"));
    }

    #[test]
    fn body_inline_image_parts_shape() {
        let p = GoogleProvider::new_test("k".into());
        let body = p.build_body("S",
            &UserContent::InlineImage {
                bytes: vec![1, 2, 3],
                mime: "image/jpeg".into(),
                text: Some("caption".into()),
            },
            &cfg(),
        );
        let parts = &body["contents"][0]["parts"];
        assert_eq!(parts[0]["inlineData"]["mimeType"], "image/jpeg");
        assert_eq!(parts[0]["inlineData"]["data"], "AQID");
        assert_eq!(parts[1]["text"], "caption");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "S");
    }

    #[test]
    fn body_inline_document_parts_shape() {
        // Phase 3: whole-doc mode sends the raw PDF via the same
        // `inlineData` shape as images — Gemini doesn't have a
        // separate document block type; only the mime distinguishes.
        let p = GoogleProvider::new_test("k".into());
        let body = p.build_body("S",
            &UserContent::InlineDocument {
                bytes: vec![0x25, 0x50, 0x44, 0x46],   // "%PDF"
                mime: "application/pdf".into(),
                text: None,
            },
            &cfg(),
        );
        let parts = &body["contents"][0]["parts"];
        assert!(parts.is_array());
        assert_eq!(parts.as_array().unwrap().len(), 1);
        assert_eq!(parts[0]["inlineData"]["mimeType"], "application/pdf",
            "PDF mime indicates document, not image");
        assert_eq!(parts[0]["inlineData"]["data"], "JVBERg==");
    }

    #[test]
    fn body_inline_document_with_text_caption() {
        let p = GoogleProvider::new_test("k".into());
        let body = p.build_body("S",
            &UserContent::InlineDocument {
                bytes: vec![0x25, 0x50, 0x44, 0x46],
                mime: "application/pdf".into(),
                text: Some("convert this".into()),
            },
            &cfg(),
        );
        let parts = &body["contents"][0]["parts"];
        assert_eq!(parts.as_array().unwrap().len(), 2);
        assert_eq!(parts[0]["inlineData"]["mimeType"], "application/pdf");
        assert_eq!(parts[1]["text"], "convert this");
    }

    #[test]
    fn thinking_budget_populates_thinking_config() {
        let p = GoogleProvider::new_test("k".into());
        let c = ModelCfg { thinking_budget: Some(10_000), ..cfg() };
        let body = p.build_body("s", &UserContent::TextOnly("u".into()), &c);
        assert_eq!(body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 10_000);
    }

    #[test]
    fn extract_text_parses_first_candidate() {
        let d: Value = serde_json::from_str(r#"{
            "candidates":[{"content":{"parts":[{"text":"A"},{"text":"B"}]}}]
        }"#).unwrap();
        assert_eq!(extract_text(&d).unwrap(), "AB");
    }

    #[test]
    fn extract_text_ignores_non_text_parts() {
        let d: Value = serde_json::from_str(r#"{
            "candidates":[{"content":{"parts":[{"inlineData":{}},{"text":"visible"}]}}]
        }"#).unwrap();
        assert_eq!(extract_text(&d).unwrap(), "visible");
    }

    #[test]
    fn extract_text_empty_candidates_errors() {
        let d: Value = serde_json::from_str(r#"{"candidates":[]}"#).unwrap();
        assert!(matches!(extract_text(&d), Err(ProviderError::Parse(_))));
    }

    // ─── error diagnostics (regression) ──────────────────
    //
    // Google's Gemini API returns candidates without `content.parts`
    // when the model aborted: MAX_TOKENS (truncated by budget),
    // SAFETY (blocked by safety filter), RECITATION (model thinks
    // it's reciting training data), OTHER (refusal / unknown). Each
    // needs a different mitigation. Pre-fix we discarded the entire
    // candidate. Post-fix, the error string carries finishReason +
    // a truncated candidate body so the caller can pick the right
    // mitigation.

    #[test]
    fn extract_text_missing_parts_includes_finish_reason() {
        let d: Value = serde_json::from_str(r#"{
            "candidates":[{
                "finishReason":"MAX_TOKENS",
                "tokenCount":64000,
                "safetyRatings":[]
            }]
        }"#).unwrap();
        let err = extract_text(&d).unwrap_err();
        let msg = format!("{:?}", err);
        assert!(msg.contains("finishReason=MAX_TOKENS"),
            "should surface finishReason: {}", msg);
        assert!(msg.contains("tokenCount") || msg.contains("64000"),
            "should include candidate body for further triage: {}", msg);
    }

    #[test]
    fn extract_text_no_text_parts_includes_finish_reason() {
        // parts array exists but is empty / non-text — same need:
        // finishReason + body so we know whether the model emitted
        // structured output (tool call), inline media, or just nothing.
        let d: Value = serde_json::from_str(r#"{
            "candidates":[{
                "finishReason":"SAFETY",
                "content":{"parts":[]}
            }]
        }"#).unwrap();
        let err = extract_text(&d).unwrap_err();
        let msg = format!("{:?}", err);
        assert!(msg.contains("no text parts"),
            "preserves the symptom name: {}", msg);
        assert!(msg.contains("finishReason=SAFETY"),
            "surfaces finishReason: {}", msg);
    }

    #[test]
    fn extract_text_missing_parts_recitation_case() {
        // Real-world Gemini RECITATION (model abandoned because it
        // matched training data). Must surface so caller knows to
        // either reword the prompt or switch provider.
        let d: Value = serde_json::from_str(r#"{
            "candidates":[{
                "finishReason":"RECITATION",
                "citationMetadata":{"citationSources":[{"startIndex":0,"endIndex":42}]}
            }]
        }"#).unwrap();
        let err = extract_text(&d).unwrap_err();
        let msg = format!("{:?}", err);
        assert!(msg.contains("RECITATION"),
            "RECITATION must be visible to caller: {}", msg);
    }

    #[test]
    fn extract_text_missing_parts_unknown_finish_falls_back() {
        // Defensive: if finishReason is missing/null, error still
        // mentions "?" so caller knows the field was absent rather
        // than thinking the field name itself is wrong.
        let d: Value = serde_json::from_str(r#"{
            "candidates":[{"index":0}]
        }"#).unwrap();
        let err = extract_text(&d).unwrap_err();
        let msg = format!("{:?}", err);
        assert!(msg.contains("finishReason=?"),
            "missing finishReason → ?, got: {}", msg);
    }

    #[tokio::test]
    async fn end_to_end_wiremock() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-3-flash-preview:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates":[{"content":{"parts":[{"text":"ok"}]}}]
            })))
            .mount(&server).await;

        let p = GoogleProvider::new_test("KEY".into()).with_endpoint(server.uri().to_string());
        let out = p.message_single("s", UserContent::TextOnly("u".into()), &cfg()).await.unwrap();
        assert_eq!(out, "ok");
    }

    #[tokio::test]
    async fn http_500_becomes_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-3-flash-preview:generateContent"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server).await;
        let p = GoogleProvider::new_test("K".into()).with_endpoint(server.uri().to_string());
        let err = p.message_single("s", UserContent::TextOnly("u".into()), &cfg()).await.unwrap_err();
        match err {
            ProviderError::Api { status, .. } => assert_eq!(status, 500),
            _ => panic!("wrong error variant"),
        }
    }
}
