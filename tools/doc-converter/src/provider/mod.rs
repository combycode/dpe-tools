//! Provider abstraction — one uniform interface for Anthropic + Google.
//!
//! For doc-converter we send exactly one request per page, containing:
//!   - system prompt (composed per-page)
//!   - user content: one inline image (base64 JPEG/PNG) + optional text
//!
//! Inline is always used; no Files API.

pub mod anthropic;
pub mod google;

use async_trait::async_trait;

/// HTTP client builder for the providers: rustls with the `ring` crypto provider and the
/// bundled Mozilla root set, the same TLS setup as reqwest 0.12 `rustls-tls`. reqwest 0.13
/// otherwise defaults to aws-lc-rs and the OS certificate store, which minimal containers
/// without `ca-certificates` do not have.
pub(crate) fn http_client_builder() -> reqwest::ClientBuilder {
    // Err only means a provider is already installed for this process, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .filter_map(|der| reqwest::Certificate::from_der(der.as_ref()).ok());
    reqwest::Client::builder().tls_certs_only(roots)
}
use std::time::Duration;

use crate::settings::{RetryCfg, RetryTrigger};

#[derive(Debug, Clone)]
pub struct ModelCfg {
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub thinking_budget: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum UserContent {
    /// Inline image bytes + mime type + optional accompanying text.
    /// Used by per-page dispatch — one rendered JPEG per page or one
    /// standalone image.
    InlineImage { bytes: Vec<u8>, mime: String, text: Option<String> },
    /// Whole document attachment (Phase 3 / whole-doc mode). The
    /// provider does its own page parsing — we send raw PDF bytes,
    /// no client-side rendering. Anthropic accepts `application/pdf`
    /// as a `document` content block; Gemini accepts via `inlineData`
    /// with the same mime.
    InlineDocument { bytes: Vec<u8>, mime: String, text: Option<String> },
    /// Text-only (rare for this tool; used for image-less pages or smoke tests).
    TextOnly(String),
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Single inference call. Returns the raw text response.
    async fn message_single(
        &self,
        system: &str,
        user: UserContent,
        cfg: &ModelCfg,
    ) -> Result<String, ProviderError>;

    /// Submit a batch of inference requests. Returns the
    /// provider-issued batch identifier; the caller persists it and
    /// polls via `poll_batch`. Each request must carry a unique
    /// `custom_id` so per-item results can be correlated back.
    /// Default impl returns `not implemented`; concrete providers
    /// override.
    async fn submit_batch(
        &self,
        _requests: Vec<BatchRequest>,
        _cfg: &ModelCfg,
    ) -> Result<String /* batch_id */, ProviderError> {
        Err(ProviderError::Other("batch submit not implemented for this provider".into()))
    }

    /// Poll a previously-submitted batch. Returns `InProgress` while
    /// the provider is still working; on `Completed`, the per-item
    /// results are populated (success bodies + per-item errors).
    /// `Failed`/`Expired` are terminal — the caller clears the state
    /// file and surfaces the error.
    async fn poll_batch(
        &self,
        _batch_id: &str,
    ) -> Result<BatchStatus, ProviderError> {
        Err(ProviderError::Other("batch poll not implemented for this provider".into()))
    }
}

/// One request inside a batch submission. `custom_id` is echoed back
/// by the provider in the result, letting the caller map results
/// back to source pages (we use `"page-<n>"`).
#[derive(Debug, Clone)]
pub struct BatchRequest {
    pub custom_id: String,
    pub system:    String,
    pub user:      UserContent,
}

/// Outcome of polling a batch.
#[derive(Debug, Clone)]
pub enum BatchStatus {
    /// Still running. `processed`/`total` are advisory progress
    /// counters — providers expose them inconsistently, so callers
    /// shouldn't gate on them. The "is it done" signal is the
    /// terminal variants.
    InProgress { processed: u32, total: u32 },
    /// All items finished (each may individually have succeeded or
    /// errored — the caller inspects `results`).
    Completed  { results: Vec<BatchResult> },
    /// Batch-level failure (auth, malformed submit, infra). Distinct
    /// from per-item errors which live inside `Completed.results`.
    Failed     { reason: String },
    /// Provider TTL hit. Anthropic = 24 h, Gemini = 48 h.
    Expired,
}

/// Per-item result inside a completed batch.
#[derive(Debug, Clone)]
pub struct BatchResult {
    pub custom_id: String,
    /// Either the text body (success) or a per-item error message.
    pub outcome:   Result<String, String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    // Formatted via `format_reqwest_err` so operators see the actual root
    // cause (source-chain walk: DNS, TLS, io::Error) and the reqwest
    // classifier flags (kinds=[connect,timeout,...]) that drive retry
    // eligibility. Default reqwest Display is famously terse
    // ("error sending request for url (X)") which hides everything.
    #[error("http: {}", format_reqwest_err(.0))]
    Http(#[from] reqwest::Error),
    #[error("{provider} {status}: {body}")]
    Api { provider: &'static str, status: u16, body: String },
    #[error("parse: {0}")]
    Parse(String),
    #[error("{0}")]
    Other(String),
}

/// Render a reqwest::Error with (a) the reqwest classifier flags that
/// determine retry eligibility, and (b) the full `.source()` chain so
/// the OS-level cause (io::Error kind, TLS handshake failure, DNS
/// NoRecordsFound, etc.) is visible. Without this we get reqwest's
/// bare "error sending request for url (X)" — which is exactly the
/// unhelpful message a failing request surfaces.
pub(crate) fn format_reqwest_err(re: &reqwest::Error) -> String {
    let mut s = String::new();
    s.push_str(&re.to_string());

    // Classifier flags — these are what is_retryable() consults. If none
    // fire (e.g. an is_request()-only error not covered by our retry
    // triggers), the operator can see why retry didn't kick in.
    let mut kinds = Vec::with_capacity(4);
    if re.is_timeout()  { kinds.push("timeout"); }
    if re.is_connect()  { kinds.push("connect"); }
    if re.is_request()  { kinds.push("request"); }
    if re.is_body()     { kinds.push("body"); }
    if re.is_decode()   { kinds.push("decode"); }
    if re.is_redirect() { kinds.push("redirect"); }
    if re.is_status()   { kinds.push("status"); }
    if !kinds.is_empty() {
        s.push_str(" [kinds=");
        s.push_str(&kinds.join(","));
        s.push(']');
    }
    if let Some(st) = re.status() {
        s.push_str(&format!(" [status={}]", st.as_u16()));
    }

    // Walk the source chain — reqwest hides io::Error / TLS / DNS
    // details behind one Display level. Cap at 10 to guard against a
    // pathological cycle (shouldn't happen in practice).
    let mut src: Option<&(dyn std::error::Error + 'static)> =
        std::error::Error::source(re);
    for _ in 0..10 {
        match src {
            Some(e) => {
                s.push_str(" -> ");
                s.push_str(&e.to_string());
                src = e.source();
            }
            None => break,
        }
    }
    s
}

// ─── Retry wrapper ──────────────────────────────────────────────────

/// True iff `err` is in one of the configured retry trigger classes.
/// `4xx other than 429`, `Parse`, and `Other` are never retryable.
pub(crate) fn is_retryable(err: &ProviderError, triggers: &[RetryTrigger]) -> bool {
    match err {
        ProviderError::Api { status, .. } => {
            (*status == 429 && triggers.contains(&RetryTrigger::RateLimit))
            || (*status >= 500 && *status < 600
                && triggers.contains(&RetryTrigger::ServerError))
        }
        ProviderError::Http(re) => {
            (re.is_timeout()  && triggers.contains(&RetryTrigger::Timeout))
            || (re.is_connect() && triggers.contains(&RetryTrigger::ConnectionError))
        }
        ProviderError::Parse(_) | ProviderError::Other(_) => false,
    }
}

/// Run `f` up to `retry.max_attempts` times with exponential backoff.
/// Each retry emits a `{"type":"log","level":"warn",...}` line to
/// stderr so the runner's `logs` channel records the attempt. The
/// final failure is returned verbatim — no wrapping — so the existing
/// error-handling paths see the same `ProviderError` shape they did
/// before retry was added.
///
/// `op` is a short identifier ("anthropic.message_single") used in
/// the log line so operators can correlate retries with call sites.
pub(crate) async fn retry_call<F, Fut, T>(
    retry: &RetryCfg,
    op:    &str,
    mut f: F,
) -> Result<T, ProviderError>
where
    F:   FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderError>>,
{
    let mut attempt = 1u32;
    let mut backoff_ms = retry.initial_backoff_ms;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt >= retry.max_attempts.max(1)
                     || !is_retryable(&e, &retry.retry_on) => {
                return Err(e);
            }
            Err(e) => {
                let safe_msg = e.to_string().replace('"', "\\\"").replace('\n', " ");
                eprintln!(
                    r#"{{"type":"log","level":"warn","msg":"{op}: retry {a}/{max} after error ({err}); waiting {ms}ms"}}"#,
                    op  = op,
                    a   = attempt,
                    max = retry.max_attempts,
                    err = safe_msg,
                    ms  = backoff_ms,
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = backoff_ms.saturating_mul(2).min(retry.max_backoff_ms);
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every bundled root certificate must load: one that fails is dropped silently by
    /// `http_client_builder`, and with none left every request fails as "unknown issuer".
    #[test]
    fn bundled_root_certificates_all_parse() {
        let all = webpki_root_certs::TLS_SERVER_ROOT_CERTS;
        assert!(!all.is_empty());
        let ok = all.iter().filter(|der| reqwest::Certificate::from_der(der.as_ref()).is_ok()).count();
        assert_eq!(ok, all.len());
    }

    /// Live check of the TLS setup (ring + bundled roots) against both provider hosts: any HTTP
    /// status means the handshake and certificate verification succeeded. Needs network, so it
    /// is ignored by default: `cargo test -p combycode-dpe-tool-doc-converter -- --ignored live_tls`.
    #[tokio::test]
    #[ignore]
    async fn live_tls_handshake_with_provider_hosts() {
        let client = http_client_builder().timeout(Duration::from_secs(20)).build().unwrap();
        for url in ["https://api.anthropic.com/v1/messages", "https://generativelanguage.googleapis.com/v1beta/models"] {
            let resp = client.get(url).send().await.unwrap_or_else(|e| panic!("{url}: {}", format_reqwest_err(&e)));
            assert!(resp.status().as_u16() >= 200, "{url}: {}", resp.status());
        }
    }

    /// Sanity check: `format_reqwest_err` is what the Display impl calls
    /// through the `#[error("http: {}", format_reqwest_err(.0))]` attr.
    /// We can't easily synthesise a `reqwest::Error` (private ctor), so
    /// this test forces a real one — a connect error to an address that
    /// won't accept TCP (RFC 5737 TEST-NET-1 is guaranteed non-routable)
    /// with a 1-second timeout — and asserts the enriched string carries
    /// classifier flags and at least one `->` source-chain step.
    #[tokio::test]
    async fn format_reqwest_err_carries_kinds_and_source_chain() {
        let client = http_client_builder()
            .connect_timeout(Duration::from_millis(500))
            .timeout(Duration::from_millis(1500))
            .build()
            .unwrap();
        // 192.0.2.1 is TEST-NET-1 per RFC 5737 — reserved, never
        // routable, guaranteed to fail connect. Port 81 avoids any
        // local dev server on 80.
        let re = client.get("http://192.0.2.1:81/x").send().await
            .expect_err("request should not succeed");
        let s = format_reqwest_err(&re);
        // Must contain at least one classifier flag block.
        assert!(s.contains("[kinds="),
                "missing [kinds=...] block, got: {}", s);
        // Must expose SOME classifier — timeout, connect, or request
        // depending on OS+DNS behaviour. If NONE of these three fires,
        // the enricher isn't doing its job.
        assert!(s.contains("timeout") || s.contains("connect")
                || s.contains("request"),
                "expected at least one of timeout/connect/request in kinds, got: {}", s);
        // Should have walked the source chain at least one step deeper
        // than reqwest's own Display; the underlying hyper/rustls/io
        // error lives there.
        assert!(s.contains(" -> "),
                "missing source-chain step, got: {}", s);
    }
}
