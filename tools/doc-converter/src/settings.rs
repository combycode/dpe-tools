//! Settings schema for doc-converter.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub provider: ProviderKind,
    pub model: String,
    /// Sampling temperature. Absent: 0.0. `null`: not sent at all (models that reject it).
    #[serde(default = "d_temp")]
    pub temperature: Option<f32>,
    #[serde(default = "d_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub thinking_budget: Option<u32>,
    /// Anthropic only: `adaptive` sends `thinking: {type: adaptive}` (models that reject
    /// `budget_tokens`). Mutually exclusive with `thinking_budget`.
    #[serde(default)]
    pub thinking: Option<ThinkingMode>,
    /// Anthropic only: `output_config.effort`.
    #[serde(default)]
    pub effort: Option<Effort>,

    pub prompts: PromptsCfg,

    /// Pipeline-level value for the `category` overlay name. Resolution: when
    /// `"category"` appears in `overlays`, this value wins over envelope and
    /// `overlay_defaults`. All OTHER overlay names are read from the envelope
    /// (`v.<name>`) and fall back to `overlay_defaults` -- no hard-coded names.
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub overlays: Vec<String>,
    #[serde(default)]
    pub overlay_defaults: std::collections::BTreeMap<String, String>,

    #[serde(default)]
    pub page_filter: Vec<String>,

    #[serde(default)]
    pub render: RenderCfg,

    pub output: OutputCfg,

    #[serde(default)]
    pub docx: DocxCfg,

    /// Per-page parallel dispatch (Phase 2). When more than one page
    /// needs to be processed, fire up to `parallel.limit` concurrent
    /// LLM calls — `1` = serial (the legacy behaviour), default `4`
    /// is a safe number for typical Anthropic Tier 2 / Gemini quotas.
    /// Bump only if you've measured your provider's rate-limit
    /// headroom.
    #[serde(default)]
    pub parallel: ParallelCfg,

    /// Whole-doc dispatch strategy (Phase 3). Short documents go to
    /// the LLM as one raw-PDF attachment in a single call; longer
    /// ones split into per-page parallel calls (or per-page batch in
    /// Phase 4).
    #[serde(default)]
    pub strategy: StrategyCfg,

    /// Batch-API dispatch (Phase 4). Activated only when
    /// `strategy.per_page_mode == "batch"`. Pages are submitted as a
    /// single batch request; the runner polls until completion (or
    /// the user opts into fire-and-forget by setting `poll: null`,
    /// in which case results are harvested on a subsequent run).
    #[serde(default)]
    pub batch: BatchCfg,

    /// Per-HTTP-request timeout (milliseconds). Applied uniformly to
    /// every provider call: whole-doc dispatch, per-page parallel,
    /// and the internal submit/poll/results requests of batch mode.
    /// Default `180_000` (180s). If the provider stalls past this,
    /// reqwest aborts the call with a timeout error; the retry
    /// wrapper treats that as a transient retryable failure (when
    /// `Timeout` is in `retry.retry_on`).
    #[serde(default = "d_request_timeout_ms")]
    pub request_timeout_ms: u64,

    /// Retry policy for transient HTTP failures (429 rate-limit, 5xx
    /// server errors, request timeouts, connection failures). Applied
    /// uniformly to every provider call. Defaults to 3 attempts
    /// (1 + up to 2 retries) with exponential backoff starting at 1s
    /// capped at 30s, retrying on all four trigger classes.
    #[serde(default)]
    pub retry: RetryCfg,

    /// How to surface terminal conversion failures (those that survived
    /// internal `retry` exhaustion + non-retryable failures like missing
    /// input file or parse errors).
    ///
    /// `stderr` (default): legacy behaviour — `ctx.error(&v, msg)` only.
    /// Envelope is dropped from the data stream; downstream sees nothing.
    ///
    /// `envelope`: emit a fail-envelope on the data channel with
    /// `v.failed=true`, `v.error_class`, `v.error_msg`, `v.error_attempt`,
    /// and the source-identity fields. Lets pipeline YAML route failures
    /// to a sink (or back into the converter after a cooldown — see the
    /// retry-loop example in README.md). No stderr emission.
    ///
    /// `both`: emit the fail-envelope AND call `ctx.error`. Useful while
    /// migrating an existing pipeline (operators still see the journal
    /// signal while the new routing stages are being wired).
    #[serde(default)]
    pub on_failure: OnFailureMode,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind { Anthropic, Google }

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingMode { Adaptive }

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Effort { Low, Medium, High, Xhigh, Max }

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self { Effort::Low => "low", Effort::Medium => "medium", Effort::High => "high",
                     Effort::Xhigh => "xhigh", Effort::Max => "max" }
    }
}

impl Settings {
    /// Cross-field checks for the model settings.
    pub fn validate_model(&self) -> Result<(), String> {
        if self.thinking.is_some() && self.thinking_budget.unwrap_or(0) > 0 {
            return Err("`thinking: adaptive` and `thinking_budget` are mutually exclusive".into());
        }
        if self.provider != ProviderKind::Anthropic && (self.thinking.is_some() || self.effort.is_some()) {
            return Err("`thinking` and `effort` are Anthropic-only settings".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptsCfg {
    pub dir: String,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RenderCfg {
    pub dpi: u16,
    pub quality: u8,
}
impl Default for RenderCfg {
    fn default() -> Self { Self { dpi: 150, quality: 85 } }
}

/// Top-level `output:` block. Tagged choice between writing the
/// merged markdown body to a file (Mode A) OR emitting it on an
/// envelope field (Mode B). Exactly one of `file` / `field` must be
/// present; validation at startup.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputCfg {
    /// Mode A — write the merged markdown to a file on disk; the file
    /// itself serves as the persistent cache.
    #[serde(default)]
    pub file: Option<FileOutputCfg>,
    /// Mode B — emit the merged markdown body on an envelope field.
    /// Framework cache at `$DPE_STORAGE/doc-converter/<hash>.json`
    /// stores the result (configurable in a future release).
    #[serde(default)]
    pub field: Option<FieldOutputCfg>,
    /// Whether to parse the leading `---` frontmatter block and promote
    /// each key as a separate `v.<key>` envelope field. Default true.
    /// The body itself ALWAYS retains the `---` block — extraction is
    /// purely for envelope promotion (routing / filtering).
    #[serde(default = "d_true")]
    pub extract_frontmatter: bool,
    /// Template injected before every page body when the merger combines
    /// per-page LLM outputs into a single markdown file. `{n}` is
    /// substituted with the 1-based page number from the original PDF.
    /// Empty string disables the marker (yields the legacy `\n\n`-only
    /// join). Whole-doc mode never injects this marker — the LLM emits
    /// one continuous response with no page boundaries to mark.
    /// Default `"<!-- PAGE: {n} -->"` — invisible in rendered Markdown,
    /// searchable in source via `grep "PAGE: 5"`.
    #[serde(default = "d_page_separator")]
    pub page_separator: String,
}

impl OutputCfg {
    /// Validation pass run after `serde::Deserialize`. Confirms exactly
    /// one of `file` / `field` is set and the inner shapes are
    /// well-formed (including parsing the filename template if Mode A).
    pub fn validate(&self) -> Result<(), String> {
        match (&self.file, &self.field) {
            (Some(_), Some(_)) => Err(
                "output: exactly one of `file` / `field` allowed (both were set)".into()
            ),
            (None, None) => Err(
                "output: exactly one of `file` / `field` required (neither was set)".into()
            ),
            (Some(f), None) => f.validate(),
            (None, Some(f)) => f.validate(),
        }
    }
}

/// Mode A — file output config.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileOutputCfg {
    /// Target directory. Resolved via runner path-prefix substitution
    /// (`$temp`, `$session`, etc.) before the tool is spawned.
    pub dir: String,
    /// Filename template. Either a shorthand `"v.<field>"` pointer OR
    /// a template string with `{field:filter:filter}` placeholders.
    /// See `crate::filename` for the full grammar.
    pub name: String,
    /// Extension appended to the resolved name when the name's LAST
    /// path segment has no dot. Default `.md`. Ignored when `name`
    /// already includes an extension (e.g. `name: "{hash}.md"`).
    #[serde(default = "d_md")]
    pub target_ext: String,
    /// Cache invalidation strategy. Default `strict` — file exists →
    /// cache HIT, no validation. `sidecar` writes a `<file>.dpe.json`
    /// for automatic settings-drift detection. `none` always
    /// regenerates.
    #[serde(default)]
    pub invalidation: InvalidationMode,
}

impl FileOutputCfg {
    pub fn validate(&self) -> Result<(), String> {
        if self.dir.is_empty() {
            return Err("output.file.dir: must not be empty".into());
        }
        if self.name.is_empty() {
            return Err("output.file.name: must not be empty".into());
        }
        // Parse the template eagerly so authoring errors surface at
        // settings load instead of on the first envelope.
        crate::filename::NameTemplate::parse(&self.name)
            .map_err(|e| format!("output.file.name: {}", e))?;
        Ok(())
    }
}

/// Mode B — field output config.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldOutputCfg {
    /// Envelope field name to carry the merged markdown body. Validated
    /// as a single top-level key (no dots — no nested paths).
    pub name: String,
}

impl FieldOutputCfg {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("output.field.name: must not be empty".into());
        }
        if self.name.contains('.') {
            return Err(format!(
                "output.field.name: must be a top-level key (got '{}' with dot)",
                self.name
            ));
        }
        Ok(())
    }
}

/// Mode A cache invalidation strategy.
///
/// - `strict` (default): file exists → cache HIT, no validation. Operator
///   includes a version in the `name` template (e.g. `"{hash}-v3.md"`) or
///   clears the output dir to invalidate.
/// - `sidecar`: file exists AND `<file>.dpe.json` matches current settings
///   → cache HIT. Otherwise regenerate. Adds one ~200 B sidecar per output.
/// - `none`: never reuse — always regenerate. Useful for testing / stability
///   runs.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InvalidationMode {
    #[default]
    Strict,
    Sidecar,
    None,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DocxCfg {
    pub soffice_bin: String,
    pub temp_dir: Option<String>,
}
impl Default for DocxCfg {
    fn default() -> Self { Self { soffice_bin: "soffice".into(), temp_dir: None } }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ParallelCfg {
    /// Max concurrent in-flight LLM calls during per-page dispatch.
    /// Validated `>= 1` at startup (rejected at settings parse if 0).
    pub limit: u32,
}
impl Default for ParallelCfg {
    fn default() -> Self { Self { limit: 4 } }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StrategyCfg {
    /// Page count under which the document is sent as ONE raw-PDF
    /// attachment to the LLM (whole-doc mode). Default `5`. Set to
    /// `0` to always use per-page dispatch.
    pub whole_doc_threshold: u32,
    /// Hard ceiling on whole-doc mode page count. Anthropic caps at
    /// 100, Gemini at 1000; default 100 covers both safely. Above
    /// this, the routing falls back to per-page even when the page
    /// count is ≤ `whole_doc_threshold`.
    pub whole_doc_max_pages: u32,
    /// Hard ceiling on whole-doc mode source-file size in bytes.
    /// Default `20 * 1024 * 1024 = 20 MB` — the smaller of Anthropic
    /// PDF cap (32 MB) and Gemini inline cap (20 MB), so a doc that
    /// fits whole-doc here is guaranteed to fit either provider's
    /// inline path without needing to fall back to file-upload.
    pub whole_doc_max_bytes: u64,
    /// How to dispatch when whole-doc isn't picked: `parallel`
    /// (default — N concurrent direct calls capped at
    /// `parallel.limit`) or `batch` (submit one batch-API request,
    /// poll for completion). Batch wins on cost (50% discount) and
    /// throughput at large N; parallel wins on latency for medium N.
    #[serde(default = "d_per_page_parallel")]
    pub per_page_mode: PerPageMode,
}
impl Default for StrategyCfg {
    fn default() -> Self {
        Self {
            whole_doc_threshold: 5,
            whole_doc_max_pages: 100,
            whole_doc_max_bytes: 20 * 1024 * 1024,
            per_page_mode: PerPageMode::Parallel,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PerPageMode {
    Parallel,
    Batch,
}
fn d_per_page_parallel() -> PerPageMode { PerPageMode::Parallel }

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BatchCfg {
    /// Where to persist `{doc_cache_key → batch_id}` so a re-run
    /// picks up an in-flight batch instead of submitting again.
    /// Default: `$storage/doc-converter/batches` (resolved by the
    /// runner's path-prefix table at spawn time). Files are JSON,
    /// one per pending batch, atomically written via tmp+rename.
    pub state_dir: String,
    /// Polling cadence after submission. `Some(...)` polls per the
    /// PollCfg shape until completion or `deadline_ms`; `None` is
    /// fire-and-forget (submit, emit `batch_submitted` meta, return
    /// without a data envelope; harvest on a future run).
    pub poll: Option<PollCfg>,
    /// Per-HTTP-request timeout (submission, status check, result
    /// fetch). Padded for upstream slowness; orthogonal to
    /// `poll.deadline_ms` which is end-to-end.
    pub request_timeout_ms: u64,
}
impl Default for BatchCfg {
    fn default() -> Self {
        Self {
            state_dir: "$storage/doc-converter/batches".to_string(),
            poll: Some(PollCfg::default()),
            request_timeout_ms: 120_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PollCfg {
    /// Wait this long after submission before the FIRST status poll.
    /// Anthropic batches typically need a few seconds to spin up;
    /// polling immediately wastes a request.
    pub start_after_ms: u64,
    /// Between subsequent polls. Provider rate-limits status checks;
    /// rejected at config-load if `< 1000` (1 s).
    pub interval_ms: u64,
    /// End-to-end deadline. After this much wall time, the runner
    /// stops polling, emits a `batch_pending` meta, and returns
    /// without a data envelope. The batch keeps running upstream;
    /// next run resumes from the saved state file.
    pub deadline_ms: u64,
}
impl Default for PollCfg {
    fn default() -> Self {
        Self {
            start_after_ms: 30_000,    // 30 s
            interval_ms:    10_000,    // 10 s
            deadline_ms:    600_000,   // 10 min
        }
    }
}

fn d_temp() -> Option<f32> { Some(0.0) }
fn d_max_tokens() -> u32 { 65536 }
fn d_true() -> bool { true }
fn d_md() -> String { ".md".to_string() }
fn d_page_separator() -> String { "<!-- PAGE: {n} -->".to_string() }
fn d_request_timeout_ms() -> u64 { 180_000 }

/// Retry policy for transient provider failures. See `Settings.retry`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RetryCfg {
    /// Total attempts including the first. `1` disables retry entirely.
    /// Default `3` = 1 initial attempt + up to 2 retries.
    pub max_attempts: u32,
    /// Backoff before the FIRST retry (i.e. after attempt 1 fails).
    /// Doubles each subsequent retry, capped at `max_backoff_ms`.
    /// Default `1_000` (1 second).
    pub initial_backoff_ms: u64,
    /// Cap on the per-retry wait. Default `30_000` (30 seconds).
    pub max_backoff_ms: u64,
    /// Which error classes are retryable. Default: all four.
    /// Other failure modes (HTTP 4xx other than 429, parse errors,
    /// validation errors, content-policy refusals) are NEVER retried.
    pub retry_on: Vec<RetryTrigger>,
}
impl Default for RetryCfg {
    fn default() -> Self {
        Self {
            max_attempts:       3,
            initial_backoff_ms: 1_000,
            max_backoff_ms:     30_000,
            retry_on: vec![
                RetryTrigger::RateLimit,
                RetryTrigger::ServerError,
                RetryTrigger::Timeout,
                RetryTrigger::ConnectionError,
            ],
        }
    }
}

/// One trigger class enabling retry. See `RetryCfg.retry_on`.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetryTrigger {
    /// HTTP 429.
    RateLimit,
    /// HTTP 5xx.
    ServerError,
    /// Per-request timeout exceeded (`request_timeout_ms`).
    Timeout,
    /// TCP reset / DNS failure / TLS handshake failure / connect refused.
    ConnectionError,
}

/// How `process_input` surfaces terminal conversion failures.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OnFailureMode {
    /// `ctx.error(&v, msg)` only. Legacy behaviour — envelope is dropped
    /// from the data stream.
    #[default]
    Stderr,
    /// Emit a fail-envelope on the data channel; no stderr error.
    Envelope,
    /// Emit fail-envelope AND call `ctx.error`. Useful during migration.
    Both,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_settings_parse() {
        let s = r#"{
            "provider": "google",
            "model": "gemini-3-flash-preview",
            "prompts": {"dir":"/p","name":"convert","version":"v1"},
            "output": {"file":{"dir":"/o","name":"v.path","target_ext":".md"}}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        assert_eq!(settings.provider, ProviderKind::Google);
        assert_eq!(settings.temperature, Some(0.0));
        assert_eq!(settings.max_tokens, 65536);
        assert_eq!(settings.render.dpi, 150);
        assert_eq!(settings.render.quality, 85);
        assert_eq!(settings.docx.soffice_bin, "soffice");
    }

    #[test]
    fn full_settings_parse() {
        let s = r#"{
            "provider": "anthropic",
            "model": "claude-opus-4-5",
            "temperature": 0.2,
            "max_tokens": 32000,
            "thinking_budget": 4000,
            "prompts": {"dir":"/p","name":"convert","version":"v2"},
            "category": "alpha",
            "overlays": ["category","tag"],
            "overlay_defaults": {"category":"default"},
            "page_filter": ["1-3","0-3"],
            "render": {"dpi":200,"quality":90},
            "output": {"file":{"dir":"/o","name":"v.path","target_ext":".md"},"extract_frontmatter":false},
            "docx": {"soffice_bin":"C:/bin/soffice.exe","temp_dir":"/t"}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        assert_eq!(settings.thinking_budget, Some(4000));
        assert_eq!(settings.page_filter, vec!["1-3".to_string(), "0-3".to_string()]);
        assert_eq!(settings.render.dpi, 200);
        assert_eq!(settings.overlays, vec!["category".to_string(), "tag".to_string()]);
        assert_eq!(settings.docx.soffice_bin, "C:/bin/soffice.exe");
    }

    #[test]
    fn unknown_field_rejected() {
        let s = r#"{
            "provider":"google","model":"m","bogus":1,
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"file":{"dir":"/","name":"v.path","target_ext":".md"}}
        }"#;
        assert!(serde_json::from_str::<Settings>(s).is_err());
    }

    #[test]
    fn parallel_default_when_field_omitted() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"file":{"dir":"/","name":"v.path","target_ext":".md"}}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        assert_eq!(settings.parallel.limit, 4, "Phase 2 default = 4");
    }

    #[test]
    fn parallel_custom_limit_parses() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"file":{"dir":"/","name":"v.path","target_ext":".md"}},
            "parallel":{"limit":8}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        assert_eq!(settings.parallel.limit, 8);
    }

    #[test]
    fn parallel_serial_limit_one() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"file":{"dir":"/","name":"v.path","target_ext":".md"}},
            "parallel":{"limit":1}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        assert_eq!(settings.parallel.limit, 1, "explicit serial mode");
    }

    #[test]
    fn parallel_unknown_field_rejected() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"file":{"dir":"/","name":"v.path","target_ext":".md"}},
            "parallel":{"limit":4,"bogus":true}
        }"#;
        assert!(serde_json::from_str::<Settings>(s).is_err());
    }

    // ─── strategy (Phase 3) ──────────────────────────────────────────

    #[test]
    fn strategy_defaults_when_omitted() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"file":{"dir":"/","name":"v.path","target_ext":".md"}}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        assert_eq!(settings.strategy.whole_doc_threshold, 5);
        assert_eq!(settings.strategy.whole_doc_max_pages, 100);
        assert_eq!(settings.strategy.whole_doc_max_bytes, 20 * 1024 * 1024);
    }

    #[test]
    fn strategy_custom_thresholds_parse() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"file":{"dir":"/","name":"v.path","target_ext":".md"}},
            "strategy":{"whole_doc_threshold":10,"whole_doc_max_pages":50,"whole_doc_max_bytes":1048576}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        assert_eq!(settings.strategy.whole_doc_threshold, 10);
        assert_eq!(settings.strategy.whole_doc_max_pages, 50);
        assert_eq!(settings.strategy.whole_doc_max_bytes, 1_048_576);
    }

    #[test]
    fn strategy_zero_threshold_disables_whole_doc() {
        // whole_doc_threshold = 0 means "always per-page". Routing
        // logic in main.rs implements this; here we just confirm the
        // value parses cleanly.
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"file":{"dir":"/","name":"v.path","target_ext":".md"}},
            "strategy":{"whole_doc_threshold":0,"whole_doc_max_pages":100,"whole_doc_max_bytes":20971520}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        assert_eq!(settings.strategy.whole_doc_threshold, 0);
    }

    #[test]
    fn strategy_unknown_field_rejected() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"file":{"dir":"/","name":"v.path","target_ext":".md"}},
            "strategy":{"whole_doc_threshold":5,"bogus":1}
        }"#;
        assert!(serde_json::from_str::<Settings>(s).is_err());
    }

    // ─── new output-mode validation ──────────────────────────────────

    #[test]
    fn output_mode_file_default_invalidation_strict() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"file":{"dir":"/","name":"v.path"}}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        let f = settings.output.file.as_ref().unwrap();
        assert_eq!(f.invalidation, InvalidationMode::Strict);
        // target_ext defaults to ".md"
        assert_eq!(f.target_ext, ".md");
        settings.output.validate().expect("Mode A valid");
    }

    #[test]
    fn output_mode_file_invalidation_sidecar() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"file":{"dir":"/","name":"v.hash","invalidation":"sidecar"}}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        assert_eq!(
            settings.output.file.as_ref().unwrap().invalidation,
            InvalidationMode::Sidecar
        );
    }

    #[test]
    fn output_mode_field_basic() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"field":{"name":"content"}}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        assert_eq!(settings.output.field.as_ref().unwrap().name, "content");
        assert!(settings.output.file.is_none());
        settings.output.validate().expect("Mode B valid");
    }

    #[test]
    fn output_mode_both_rejected() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"file":{"dir":"/","name":"v.hash"},"field":{"name":"content"}}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        let err = settings.output.validate().unwrap_err();
        assert!(err.contains("exactly one"), "{}", err);
    }

    #[test]
    fn output_mode_neither_rejected() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        let err = settings.output.validate().unwrap_err();
        assert!(err.contains("exactly one"), "{}", err);
    }

    #[test]
    fn output_field_name_with_dot_rejected() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"field":{"name":"nested.key"}}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        let err = settings.output.validate().unwrap_err();
        assert!(err.contains("dot"), "{}", err);
    }

    #[test]
    fn output_file_name_template_bad_filter_rejected_at_validate() {
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"file":{"dir":"/","name":"{hash:reverse}"}}
        }"#;
        let settings: Settings = serde_json::from_str(s).unwrap();
        let err = settings.output.validate().unwrap_err();
        assert!(err.contains("reverse"), "{}", err);
    }

    #[test]
    fn output_legacy_top_level_dir_rejected() {
        // Migration story: old shape `output:{dir,filename_from,...}`
        // fails parse with a clear unknown-field error pointing at the
        // new top-level keys.
        let s = r#"{
            "provider":"google","model":"m",
            "prompts":{"dir":"/","name":"n","version":"v"},
            "output":{"dir":"/o","filename_from":"v.path","target_ext":".md"}
        }"#;
        let err = serde_json::from_str::<Settings>(s).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown field `dir`"), "{}", msg);
    }

    fn with(extra: &str) -> Result<Settings, String> {
        let s = format!(r#"{{
            "provider": "anthropic", "model": "m",
            "prompts": {{"dir":"/p","name":"convert","version":"v1"}},
            "output": {{"file":{{"dir":"/o","name":"v.path"}}}}{extra}
        }}"#);
        let st: Settings = serde_json::from_str(&s).map_err(|e| e.to_string())?;
        st.validate_model()?;
        Ok(st)
    }

    #[test]
    fn temperature_absent_zero_null_none() {
        assert_eq!(with("").unwrap().temperature, Some(0.0));
        assert_eq!(with(r#", "temperature": null"#).unwrap().temperature, None);
        assert_eq!(with(r#", "temperature": 0.3"#).unwrap().temperature, Some(0.3));
    }

    #[test]
    fn thinking_adaptive_and_effort_parse() {
        let s = with(r#", "thinking": "adaptive", "effort": "xhigh""#).unwrap();
        assert_eq!(s.thinking, Some(ThinkingMode::Adaptive));
        assert_eq!(s.effort.map(|e| e.as_str()), Some("xhigh"));
        assert!(with(r#", "effort": "extreme""#).is_err());
    }

    #[test]
    fn adaptive_with_budget_is_rejected() {
        let e = with(r#", "thinking": "adaptive", "thinking_budget": 2000"#).unwrap_err();
        assert!(e.contains("mutually exclusive"), "{e}");
    }

    #[test]
    fn thinking_and_effort_are_anthropic_only() {
        let s = r#"{
            "provider": "google", "model": "g", "effort": "low",
            "prompts": {"dir":"/p","name":"convert","version":"v1"},
            "output": {"file":{"dir":"/o","name":"v.path"}}
        }"#;
        let st: Settings = serde_json::from_str(s).unwrap();
        assert!(st.validate_model().unwrap_err().contains("Anthropic-only"));
    }

    #[test]
    fn adaptive_with_zero_budget_is_accepted() {
        assert!(with(r#", "thinking": "adaptive", "thinking_budget": 0"#).is_ok());
        assert!(with(r#", "thinking": null, "effort": null"#).is_ok());
    }
}
