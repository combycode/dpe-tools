//! doc-converter binary entry.
//!
//! Per envelope:
//!   1. Take `v.path` (pdf/docx/image).
//!   2. DOCX → PDF via soffice; image → single "page 1"; pdf → as-is.
//!   3. Split into pages via pdfium; filter by `settings.page_filter` ranges.
//!   4. For each page (sequential; batch-API cross-document batching is
//!      deferred -- this is the MVP): compose the system prompt (base +
//!      tag overlays + page-range overlays), call the provider with the
//!      rendered JPEG inline, collect the markdown response.
//!   5. Merge per-page frontmatters reverse-priority; strip per-page
//!      frontmatter blocks; combine bodies with `\n\n`.
//!   6. Write `<output.dir>/<filename_stem><target_ext>`.
//!   7. Emit `{v: {path, ...merged_frontmatter_fields}}`.
//!
//! Note: batch mode (settings.batch.enabled) is reserved for future work.
//! Current impl always processes pages sequentially.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use combycode_dpe::prelude::*;
use combycode_dpe::dpe_run;
use combycode_dpe_tool_doc_converter::{
    batch_state, docx, envelope::envelope_base, filename, frontmatter, page_range,
    pdf::{PdfRenderer},
    prompt_loader,
    provider::{self, BatchRequest, BatchResult, BatchStatus, ModelCfg, Provider, UserContent},
    settings::{
        FileOutputCfg, InvalidationMode, OnFailureMode, PerPageMode, ProviderKind, Settings,
    },
};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

// ─── per-process state (lazy-init on first envelope) ────────────────

struct ToolState {
    settings: Settings,
    provider: Arc<dyn Provider>,
    renderer: Arc<PdfRenderer>,
    rt:       Runtime,
    temp_dir: PathBuf,
}

fn build_state(settings: Settings) -> anyhow::Result<ToolState> {
    let timeout = std::time::Duration::from_millis(settings.request_timeout_ms);
    let retry = settings.retry.clone();
    let provider: Arc<dyn Provider> = match settings.provider {
        ProviderKind::Anthropic => {
            let key = std::env::var("ANTHROPIC_API_KEY")
                .map_err(|_| anyhow::anyhow!("ANTHROPIC_API_KEY not set"))?;
            Arc::new(provider::anthropic::AnthropicProvider::new(key, timeout, retry))
        }
        ProviderKind::Google => {
            let key = std::env::var("GEMINI_API_KEY")
                .or_else(|_| std::env::var("GOOGLE_API_KEY"))
                .map_err(|_| anyhow::anyhow!("GEMINI_API_KEY / GOOGLE_API_KEY not set"))?;
            Arc::new(provider::google::GoogleProvider::new(key, timeout, retry))
        }
    };
    let renderer = Arc::new(
        PdfRenderer::new().map_err(|e| anyhow::anyhow!("pdf init: {}", e))?
            .with_dpi(settings.render.dpi)
            .with_quality(settings.render.quality),
    );
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let temp_dir = settings.docx.temp_dir.clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("dpe-doc-converter"));
    std::fs::create_dir_all(&temp_dir)?;
    Ok(ToolState { settings, provider, renderer, rt, temp_dir })
}

// ─── entry ───────────────────────────────────────────────────────────

static STATE: std::sync::OnceLock<std::sync::Mutex<ToolState>> = std::sync::OnceLock::new();

fn init_state_from_settings(raw: &Value) -> Result<(), String> {
    let settings: Settings = serde_json::from_value(raw.clone())
        .map_err(|e| format!("bad settings: {}", e))?;
    settings.output.validate().map_err(|e| format!("settings: {}", e))?;
    let state = build_state(settings).map_err(|e| format!("init: {}", e))?;
    STATE.set(std::sync::Mutex::new(state))
         .map_err(|_| "tool state already initialised".to_string())
}

/// Resolved per-envelope output destination. Built from
/// `state.settings.output.{file, field}` plus the envelope-derived
/// filename (Mode A only). One of two shapes.
#[derive(Debug)]
enum OutputTarget {
    /// Mode A — write the merged body to `path`. Sidecar at
    /// `<path>.dpe.json` is written only when `invalidation == Sidecar`.
    File {
        path: PathBuf,
        invalidation: InvalidationMode,
    },
    /// Mode B — emit the merged body on `v.<field_name>`. Cache lives at
    /// the framework's default location.
    Field {
        field_name: String,
    },
}

impl OutputTarget {
    fn sidecar_path(file_path: &Path) -> PathBuf {
        // <file>.dpe.json — adjacent to the output file. Per-file.
        let mut s = file_path.as_os_str().to_owned();
        s.push(".dpe.json");
        PathBuf::from(s)
    }
}

/// Resolve the envelope-derived output target from settings + envelope.
/// Returns `Err` if the filename template fails to resolve against the
/// envelope (missing field, empty value, etc.); the caller treats that
/// as a per-envelope failure (class `input`).
fn resolve_output_target(settings: &Settings, v: &Value) -> Result<OutputTarget, String> {
    if let Some(fcfg) = &settings.output.file {
        return resolve_file_target(fcfg, v).map(|p| OutputTarget::File {
            path: p,
            invalidation: fcfg.invalidation,
        });
    }
    if let Some(field) = &settings.output.field {
        return Ok(OutputTarget::Field { field_name: field.name.clone() });
    }
    // OutputCfg::validate at startup rejects this branch; unreachable.
    Err("output: neither file nor field configured".into())
}

fn resolve_file_target(cfg: &FileOutputCfg, v: &Value) -> Result<PathBuf, String> {
    let tmpl = filename::NameTemplate::parse(&cfg.name)
        .map_err(|e| format!("output.file.name: {}", e))?;
    let mut resolved = tmpl.resolve(v).map_err(|e| format!("output.file.name: {}", e))?;
    // Append target_ext only when the resolved name's last path segment
    // has no dot — keeps the simplest case (`name: "v.hash"` + default
    // `.md`) working without producing `<hash>.md.md` when name already
    // includes an extension.
    if !filename::last_segment_has_dot(&resolved) {
        resolved.push_str(&cfg.target_ext);
    }
    // A raw field such as `v.path` can hold an absolute path; joining it
    // would replace `dir` and could overwrite the source document.
    filename::check_relative(&resolved).map_err(|e| format!("output.file.name: {}", e))?;
    Ok(PathBuf::from(&cfg.dir).join(resolved))
}

fn process_input(v: Value, settings: &Value, ctx: &mut Context<'_>) {
    // Lazy-init on first envelope using the runtime-provided settings.
    // Settings parse failure pre-dates state init, so we don't have an
    // on_failure mode to dispatch on — fall back to stderr-only.
    if STATE.get().is_none() {
        if let Err(e) = init_state_from_settings(settings) {
            ctx.error(&v, &e);
            return;
        }
    }
    let state = match STATE.get() {
        Some(s) => s.lock().unwrap(),
        None    => { ctx.error(&v, "state not initialised"); return; }
    };
    let on_failure = state.settings.on_failure;
    let overlays   = state.settings.overlays.clone();
    // Two-layer retry: read `v.error_attempt` so the emitted fail-envelope
    // can increment it. Pipeline can then gate retries by attempt count
    // (e.g. `route: failed = "v.failed && v.error_attempt < 5"`).
    let prev_attempt = v.get("error_attempt")
        .and_then(|n| n.as_u64()).unwrap_or(0);

    let source_path = match extract_path(&v) {
        Ok(p)  => p,
        Err(e) => {
            emit_failure(ctx, &v, "input", &e, on_failure, &overlays, prev_attempt);
            return;
        }
    };

    // Resolve the envelope-derived output destination up front. Template
    // failures (missing field, empty value) surface BEFORE the LLM call.
    let target = match resolve_output_target(&state.settings, &v) {
        Ok(t)  => t,
        Err(e) => {
            emit_failure(ctx, &v, "output", &e, on_failure, &overlays, prev_attempt);
            return;
        }
    };

    // Mode A `strict` fast-path: file exists → cache HIT. No LLM call,
    // no PDF parse, no cache_key derivation. Read the body, parse FM
    // for envelope promotion, emit, return. (Sidecar and None modes
    // fall through to convert_document, which handles their gating
    // internally.)
    if let OutputTarget::File { path, invalidation: InvalidationMode::Strict } = &target {
        if path.is_file() {
            let doc = match read_file_as_cached_doc(path) {
                Ok(d)  => d,
                Err(e) => {
                    emit_failure(ctx, &v, "io", &e, on_failure, &overlays, prev_attempt);
                    return;
                }
            };
            emit_success_envelope(&state, ctx, &v, &target, &doc, true, &overlays);
            return;
        }
    }

    // Batch helpers emit ctx.meta() / ctx.log() inline via the &mut
    // ctx threaded through convert_document — no manual meta plumbing.
    // Outcome variants:
    //   - Document(doc) → write file (Mode A) + emit data envelope here
    //   - Pending       → no data envelope; the helper already emitted
    //                     ctx.meta + ctx.log to record the in-flight state
    //   - Failed(reason) → emit_failure with original input attached
    let result = state.rt.block_on(
        convert_document(&state, &source_path, &v, ctx, &target)
    );
    let (doc, cache_hit, cache_key_hash) = match result {
        Ok(ConvertOutcome::Document { doc, cache_hit, cache_key_hash }) =>
            (doc, cache_hit, cache_key_hash),
        Ok(ConvertOutcome::Pending)       => return,
        Ok(ConvertOutcome::Failed(reason)) => {
            emit_failure(ctx, &v, "convert", &format!("convert: {}", reason), on_failure, &overlays, prev_attempt);
            return;
        }
        Err(e) => {
            emit_failure(ctx, &v, "convert", &format!("convert: {}", e), on_failure, &overlays, prev_attempt);
            return;
        }
    };

    // Mode A: persist the body to disk (and the sidecar when configured)
    // unless convert_document already determined this was a cache hit
    // and returned the body it read from the existing file.
    if let OutputTarget::File { path, invalidation } = &target {
        if !cache_hit {
            if let Some(parent) = path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    let msg = format!("mkdir {}: {}", parent.display(), e);
                    emit_failure(ctx, &v, "io", &msg, on_failure, &overlays, prev_attempt);
                    return;
                }
            }
            if let Err(e) = std::fs::write(path, &doc.final_text) {
                let msg = format!("write {}: {}", path.display(), e);
                emit_failure(ctx, &v, "io", &msg, on_failure, &overlays, prev_attempt);
                return;
            }
            if *invalidation == InvalidationMode::Sidecar {
                let sc = OutputTarget::sidecar_path(path);
                let hash = cache_key_hash.as_deref().unwrap_or("");
                if let Err(e) = write_sidecar(&sc, hash) {
                    // Sidecar write failure is non-fatal — the .md file
                    // is on disk, the next run will treat the missing
                    // sidecar as a cache miss and regenerate.
                    ctx.log(&format!("sidecar write {}: {} (non-fatal)", sc.display(), e), "warn");
                }
            }
        }
    }

    // Success on a retry input — log it. The output envelope itself is
    // built fresh below (no copy of v.failed / error_* fields), so the
    // retry envelope is "closed" by virtue of the success envelope NOT
    // carrying failure markers. `route: failed = v.failed == true` no
    // longer matches; the envelope flows through the OK branch.
    if prev_attempt > 0 {
        ctx.log(&format!(
            "convert: succeeded for {} after {} external retr{}",
            source_path.display(),
            prev_attempt,
            if prev_attempt == 1 { "y" } else { "ies" },
        ), "info");
    }

    emit_success_envelope(&state, ctx, &v, &target, &doc, cache_hit, &overlays);
}

/// Read a previously-written .md file as a `CachedDoc`. The body keeps
/// the leading `---` frontmatter block intact (the redesign no longer
/// strips it on write); frontmatter is parsed back into a map for
/// envelope promotion downstream.
fn read_file_as_cached_doc(path: &Path) -> Result<CachedDoc, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read {}: {}", path.display(), e))?;
    let (fm, _body) = frontmatter::extract(&text);
    Ok(CachedDoc { final_text: text, frontmatter: fm })
}

/// Write the sidecar JSON next to a Mode A output file. Used to
/// validate cache hits on subsequent runs when `invalidation: sidecar`.
fn write_sidecar(path: &Path, cache_key_hash: &str) -> std::io::Result<()> {
    let payload = serde_json::json!({
        "cache_key_hash": cache_key_hash,
        "wrote_at":       chrono_like_now_iso(),
        "tool_version":   env!("CARGO_PKG_VERSION"),
    });
    let s = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    std::fs::write(path, s)
}

/// Read a sidecar JSON and return its `cache_key_hash`. Used to gate
/// cache hits in `invalidation: sidecar` mode.
fn read_sidecar_hash(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("cache_key_hash").and_then(|x| x.as_str()).map(String::from)
}

/// RFC3339-ish current timestamp without pulling chrono. Used only for
/// the sidecar's `wrote_at` field which is informational (operators
/// reading the file to debug); precision down to seconds is fine.
fn chrono_like_now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days-from-epoch math: enough precision for our needs.
    let days = secs / 86400;
    let rem  = secs % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    // Approximate Y/M/D — good enough for an audit field; exact value
    // recoverable from `wrote_at` epoch seconds embedded below.
    format!("{}Z (epoch {}d {}:{:02}:{:02})", "iso", days, h, m, s)
}

/// Emit the success envelope for either output mode. Mode A inserts
/// `v.path`; Mode B inserts `v.<configured_field>` with the merged
/// markdown body. Both modes promote frontmatter fields and overlay
/// tags onto the envelope.
fn emit_success_envelope(
    state:     &ToolState,
    ctx:       &mut Context<'_>,
    v:         &Value,
    target:    &OutputTarget,
    doc:       &CachedDoc,
    cache_hit: bool,
    overlays:  &[String],
) {
    // Spread incoming v as the base so v.hash (and any other upstream-set
    // fields — overlays, source_*, custom upstream additions) flow through
    // to downstream stages. doc-converter's own fields (path / configured
    // field, failed, cache_hit, frontmatter, overlays) are inserted below
    // and override where keys collide.
    let mut out_v = envelope_base(v);
    match target {
        OutputTarget::File { path, .. } => {
            out_v.insert("path".into(), Value::String(path.to_string_lossy().into()));
        }
        OutputTarget::Field { field_name } => {
            out_v.insert(field_name.clone(), Value::String(doc.final_text.clone()));
        }
    }
    if matches!(state.settings.on_failure, OnFailureMode::Envelope | OnFailureMode::Both) {
        out_v.insert("failed".into(), Value::Bool(false));
    }
    out_v.insert("cache_hit".into(), Value::Bool(cache_hit));
    // Frontmatter promotion — only when extract_frontmatter is on
    // (default true). The body retains the `---` block regardless;
    // this flag just controls whether the keys become envelope fields.
    if state.settings.output.extract_frontmatter {
        for (k, val) in &doc.frontmatter {
            if !out_v.contains_key(k) {
                out_v.insert(k.clone(), Value::String(val.clone()));
            }
        }
    }
    // Preserve every overlay-name field from input v when frontmatter
    // didn't already promote it.
    for tag_name in overlays {
        if !out_v.contains_key(tag_name) {
            if let Some(val) = v.get(tag_name).and_then(|x| x.as_str()) {
                if !val.is_empty() {
                    out_v.insert(tag_name.clone(), Value::String(val.to_string()));
                }
            }
        }
    }
    // Source identity — additive only. Never overrides v.path (set
    // above to the .md output in Mode A) or any frontmatter / overlay
    // field. Promotes raw scan-fs fields to `source_*` and passes
    // through pre-existing source_* fields from upstream tools.
    for (in_field, out_field) in &[
        ("path",            "source_path"),
        ("hash",            "source_hash"),
        ("size",            "source_size"),
        ("filename",        "source_filename"),
        ("ext",             "source_ext"),
        ("source_path",     "source_path"),
        ("source_hash",     "source_hash"),
        ("source_size",     "source_size"),
        ("source_filename", "source_filename"),
        ("source_ext",      "source_ext"),
        ("source_file",     "source_file"),
    ] {
        if let Some(val) = v.get(*in_field) {
            if !out_v.contains_key(*out_field) && !val.is_null() {
                out_v.insert((*out_field).to_string(), val.clone());
            }
        }
    }
    ctx.output(Value::Object(out_v), None, None);
}

/// Outcome of `convert_document`.
///
/// Helpers emit `ctx.log()` / `ctx.meta()` / `ctx.error()` directly via
/// the threaded `&mut Context`; this enum just carries the structural
/// result back up to `process_input`:
///
///   Document — write file + emit data envelope.
///   Pending  — batch in flight; no data this run, helpers already
///              recorded the state via ctx.meta + ctx.log.
///   Failed   — terminal; `process_input` calls `ctx.error(&v, reason)`
///              so the framework's structured error envelope carries
///              the original input.
enum ConvertOutcome {
    Document {
        doc:            CachedDoc,
        /// True when the result was served from a cache (Mode A: file
        /// on disk; Mode B: framework cache JSON) without dispatching
        /// an LLM call. Surfaced as `v.cache_hit` for cost attribution.
        cache_hit:      bool,
        /// Blake2b-32-hex of the canonical `doc_cache_key_v2` JSON. Set
        /// for ALL successful conversions (cache hit and cold alike)
        /// so the caller can stamp a Mode A `sidecar` invalidation
        /// record. `None` when the strategy doesn't compute a cache
        /// key (currently unreachable — every strategy computes one).
        cache_key_hash: Option<String>,
    },
    Pending,
    Failed(String),
}

/// Whether `cached_impl` should be used to wrap an LLM-call producer.
/// Mode A (file output) deliberately bypasses the framework cache —
/// the `.md` file on disk serves as the persistent cache, so writing
/// a second copy as JSON in `$DPE_STORAGE/doc-converter/` is wasteful.
/// Mode B (envelope-field output) keeps the framework cache because
/// the body lives only in the envelope; without the cache JSON, every
/// re-run would call the LLM.
fn should_use_framework_cache(target: &OutputTarget) -> bool {
    matches!(target, OutputTarget::Field { .. })
}

/// Compute the blake2b-32-hex hash of the canonical-JSON form of a
/// cache key. Used to stamp `cache_key_hash` on every ConvertOutcome
/// so the Mode A sidecar write can record it.
fn cache_key_hash_of(key: &Value) -> String {
    blake2b_hex32(&canonical_json_string(key))
}

/// Whether the framework cache (`cached_impl`) is about to HIT for
/// this key. Mirrors the `will_read` predicate inside cached_impl:
/// `DPE_CACHE_MODE` is `use` (NOT `refresh`/`bypass`/`off`) AND the
/// cache file is on disk. Captured BEFORE the cached_impl call so
/// the result is the actual hit/miss for THIS dispatch. Only used
/// in Mode B (Mode A's cache hit is determined by .md file existence).
fn framework_cache_will_hit(cache_key: &Value) -> bool {
    let mode = combycode_dpe::cache::read_cache_mode();
    if mode != "use" { return false; }
    combycode_dpe::cache::cache_path(CACHE_NAMESPACE, cache_key)
        .map(|p| p.is_file())
        .unwrap_or(false)
}

/// Mode A sidecar invalidation: try to satisfy the conversion from
/// the .md file alone, comparing the sidecar's recorded hash against
/// the current settings' hash. Returns `Some((doc, hash))` on hit.
fn try_sidecar_cache_hit(
    file_path:      &Path,
    cache_key_hash: &str,
) -> Option<CachedDoc> {
    if !file_path.is_file() { return None; }
    let sc_path = OutputTarget::sidecar_path(file_path);
    if !sc_path.is_file() { return None; }
    let recorded = read_sidecar_hash(&sc_path)?;
    if recorded != cache_key_hash { return None; }
    read_file_as_cached_doc(file_path).ok()
}

fn main() {
    // Debug entry: `--dump-prompt <SETTINGS_JSON>` skips LLM/render/cache
    // entirely and just composes the prompt for each input envelope, so a
    // pipeline author can verify their settings (overlays, page filter,
    // prompt files) produce the expected text without burning API tokens.
    // Per-envelope output: one envelope per page, v = {page, total, prompt}.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--dump-prompt") {
        if let Err(e) = run_dump_prompt(&args) {
            eprintln!("dump-prompt: {e}");
            std::process::exit(1);
        }
        return;
    }
    dpe_run! { input: process_input };
}

// ─── --dump-prompt mode ────────────────────────────────────────────────
//
// Stand-alone pre-flight checker. The combine of (settings.prompts +
// settings.overlays + settings.overlay_defaults + settings.category +
// settings.page_filter + envelope.v.<overlay-name>) decides what system
// prompt the LLM sees per page; that combine is non-trivial and
// fork-prone (overlay precedence rules), so a "show me what you'd send"
// command without the cost or side-effects of a real run is high-value.
//
// Per-envelope contract (input → output):
//   in:  any data envelope with v = { ...overlay tag values..., page?, total? }
//   out: one envelope per page covered by page_filter, with
//        v = { page, total, prompt }
//
// Rules:
//   - page  defaults to 1 when v.page is missing.
//   - total defaults to v.page (or 1 if neither given) — page_filter still
//     applies, so callers can say `total=10, page_filter=["1-3","0-2"]` to
//     enumerate the four pages a real run would actually prompt on.
//   - All other settings are honoured exactly as a real run.

fn run_dump_prompt(args: &[String]) -> Result<(), String> {
    let settings_json = args
        .iter()
        .skip(1)
        .find(|a| a.as_str() != "--dump-prompt" && !a.starts_with("--"))
        .ok_or_else(|| "missing settings JSON arg (usage: doc-converter --dump-prompt '<JSON>')".to_string())?;
    let raw: Value = serde_json::from_str(settings_json)
        .map_err(|e| format!("settings JSON parse: {e}"))?;
    let settings: Settings = serde_json::from_value(raw)
        .map_err(|e| format!("settings shape: {e}"))?;

    use std::io::{self, BufRead, Write};
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("stdin read: {e}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        let envelope: Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("envelope parse: {e}"))?;
        // Only data records get a prompt; meta envelopes pass through silently.
        if envelope.get("t").and_then(|t| t.as_str()) != Some("d") { continue; }
        for emit in compose_dump_envelopes(&settings, &envelope)? {
            writeln!(out, "{}", serde_json::to_string(&emit).unwrap())
                .map_err(|e| format!("stdout write: {e}"))?;
        }
    }
    Ok(())
}

/// Pure helper — given an input data envelope, produce one output
/// envelope per page covered by `settings.page_filter` (or every page
/// if no filter). Extracted so it's testable without stdin/stdout.
fn compose_dump_envelopes(settings: &Settings, envelope: &Value) -> Result<Vec<Value>, String> {
    let v = envelope.get("v").cloned().unwrap_or(json!({}));
    let v_page = v.get("page").and_then(|p| p.as_u64()).map(|n| n as u32);
    let v_total = v.get("total").and_then(|p| p.as_u64()).map(|n| n as u32);
    let total = v_total.or(v_page).unwrap_or(1).max(1);
    let pages = select_pages(&settings.page_filter, total)?;
    // If page_filter narrowed to nothing (caller's filter excludes every
    // page), still emit one for the explicitly-given page so the caller
    // sees something — easier to debug than silent drop.
    let pages_to_emit: Vec<u32> = if pages.is_empty() { vec![v_page.unwrap_or(1)] } else { pages };

    let tags = build_tag_pairs(settings, &v);
    let id = envelope.get("id").cloned().unwrap_or(json!(""));
    let src = envelope.get("src").cloned().unwrap_or(json!(""));

    let mut out = Vec::with_capacity(pages_to_emit.len());
    for page in pages_to_emit {
        let prompt = prompt_loader::compose(&prompt_loader::LoadInput {
            dir:     Path::new(&settings.prompts.dir),
            name:    &settings.prompts.name,
            version: &settings.prompts.version,
            tags:    &tags,
            page, total,
        }).map_err(|e| format!("compose prompt page {page}: {e}"))?;
        out.push(json!({
            "t":   "d",
            "id":  id,
            "src": src,
            "v":   { "page": page, "total": total, "prompt": prompt }
        }));
    }
    Ok(out)
}

// ─── core conversion flow ───────────────────────────────────────────
//
// Cache strategy (Phase 1): the WHOLE document is cached as one entry,
// not per-page. Cache key derives from (file_hash, selected pages,
// every per-page prompt, model + provider knobs, output post-processing
// rules). The cached value is the merged final markdown plus the
// frontmatter for envelope promotion. Caller writes the file + emits
// the envelope; cache stores only the in-memory result, so changing
// `output.dir` between runs hits the cache and writes to a new place.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedDoc {
    final_text: String,
    frontmatter: BTreeMap<String, String>,
}

fn extract_path(v: &Value) -> Result<PathBuf, String> {
    // Normal input: v.path points at the source file.
    // Retry input (envelope coming back through the pipeline retry loop):
    // v.path is absent but v.source_path carries the original file path.
    // Falling back here keeps the same tool serving both cases — no
    // intermediate `normalize` stage needed. See README's
    // "Failure handling & retry loop" section.
    let s = v.get("path").and_then(|p| p.as_str())
        .or_else(|| v.get("source_path").and_then(|p| p.as_str()))
        .ok_or_else(|| "missing v.path (and no v.source_path fallback)".to_string())?;
    Ok(PathBuf::from(s))
}

/// Heuristic classification of a failure message into a canonical class
/// for routing. Looks for substrings matching the four retry-trigger
/// classes; falls back to the caller-supplied `fallback` otherwise.
fn classify_error(msg: &str, fallback: &'static str) -> &'static str {
    let m = msg.to_ascii_lowercase();
    if m.contains(" 429 ") || m.contains("rate limit") || m.contains("rate-limit") || m.contains("rate_limit") {
        return "rate_limit";
    }
    if m.contains(" 500") || m.contains(" 502") || m.contains(" 503") || m.contains(" 504") || m.contains(" 5xx") {
        return "server_error";
    }
    if m.contains("timeout") || m.contains("timed out") {
        return "timeout";
    }
    if m.contains("connection") || m.contains("dns") || m.contains(" tls ") || m.contains("connect refused") || m.contains("tcp reset") {
        return "connection_error";
    }
    fallback
}

/// Surface a terminal failure per `settings.on_failure`. Always called
/// instead of bare `ctx.error(&v, msg)` for any path that drops the
/// envelope from the data stream.
///
/// `fallback_class` is the class name to use when `classify_error` can't
/// match a retry trigger from the message — typically "input", "io",
/// "convert", "parse", or "other".
///
/// `prev_attempt` is the value of `v.error_attempt` read from the input
/// envelope (0 if not set or non-numeric). The emitted envelope carries
/// `error_attempt: prev_attempt + 1` so the pipeline retry loop can
/// gate by attempt count.
fn emit_failure(
    ctx:            &mut Context<'_>,
    v:              &Value,
    fallback_class: &'static str,
    error_msg:      &str,
    on_failure:     OnFailureMode,
    overlays:       &[String],
    prev_attempt:   u64,
) {
    use OnFailureMode::*;
    let log_stderr  = matches!(on_failure, Stderr | Both);
    let emit_env    = matches!(on_failure, Envelope | Both);
    let error_class = classify_error(error_msg, fallback_class);

    if emit_env {
        // Same spread-v-first pattern as the success envelope (see
        // emit_success_envelope): preserves v.hash + any other upstream
        // fields on the failure side too. The error_* fields below
        // override any same-named v fields.
        let mut out = envelope_base(v);
        out.insert("failed".into(),        Value::Bool(true));
        out.insert("error_class".into(),   Value::String(error_class.into()));
        out.insert("error_msg".into(),     Value::String(error_msg.into()));
        out.insert("error_attempt".into(), Value::Number((prev_attempt + 1).into()));
        // Source identity — same mapping as the success envelope. Lets
        // the retry loop (or a dead-letter sink) trace the failure back
        // to its source file without parsing the error message.
        for (in_field, out_field) in &[
            ("path",            "source_path"),
            ("hash",            "source_hash"),
            ("size",            "source_size"),
            ("filename",        "source_filename"),
            ("ext",             "source_ext"),
            ("source_path",     "source_path"),
            ("source_hash",     "source_hash"),
            ("source_size",     "source_size"),
            ("source_filename", "source_filename"),
            ("source_ext",      "source_ext"),
            ("source_file",     "source_file"),
        ] {
            if let Some(val) = v.get(*in_field) {
                if !out.contains_key(*out_field) && !val.is_null() {
                    out.insert((*out_field).into(), val.clone());
                }
            }
        }
        // Carry overlay tag values from input so `route` expressions can
        // still see (category, kind, etc.) on the failure envelope.
        for tag_name in overlays {
            if !out.contains_key(tag_name) {
                if let Some(val) = v.get(tag_name).and_then(|x| x.as_str()) {
                    if !val.is_empty() {
                        out.insert(tag_name.clone(), Value::String(val.to_string()));
                    }
                }
            }
        }
        ctx.output(Value::Object(out), None, None);
    }

    if log_stderr {
        ctx.error(v, error_msg);
    }
}

fn file_kind(path: &Path) -> FileKind {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "pdf"  => FileKind::Pdf,
        "docx" => FileKind::Docx,
        "png" | "jpg" | "jpeg" | "webp" | "gif" => FileKind::Image(ext),
        _      => FileKind::Unknown,
    }
}

enum FileKind { Pdf, Docx, Image(String), Unknown }

/// What kind of LLM dispatch the routing chose for this conversion.
/// Surfaced in the cache key so cache slots are mode-distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrategyKind {
    /// Whole document sent as one raw-PDF attachment in a single LLM
    /// call. Page filter is ignored; selected_pages is empty in the
    /// cache key. The dedicated `<name>_<version>_whole.txt` overlay
    /// (if present) is added to the prompt.
    WholeDoc,
    /// Each selected page rendered to JPEG and dispatched in parallel
    /// (Phase 2 / current default for non-whole-doc).
    PerPage,
    /// Per-page dispatch via batch API (Phase 4). Output should be
    /// byte-identical to PerPage when the LLM is deterministic; we
    /// distinguish the cache slots so a "this came from batch" cache
    /// hit is self-describing in debug.
    PerPageBatch,
}

impl StrategyKind {
    fn as_str(self) -> &'static str {
        match self {
            StrategyKind::WholeDoc     => "whole_doc",
            StrategyKind::PerPage      => "per_page",
            StrategyKind::PerPageBatch => "per_page_batch",
        }
    }
}

async fn convert_document(
    state: &ToolState,
    source_path: &Path,
    envelope: &Value,
    ctx: &mut Context<'_>,
    target: &OutputTarget,
) -> Result<ConvertOutcome, String> {
    // 1. Normalize to PDF (or keep image as-is). The PDF path here is
    //    what gets hashed for the cache key; the original docx (if any)
    //    is one step removed but its converted PDF is content-stable.
    let kind = file_kind(source_path);
    let (work_path, total): (PathBuf, u32) = match &kind {
        FileKind::Pdf => {
            let total = state.renderer.page_count(source_path)
                .map_err(|e| format!("page_count: {}", e))?;
            (source_path.to_path_buf(), total)
        }
        FileKind::Docx => {
            let pdf_path = docx::convert_to_pdf(
                &state.settings.docx.soffice_bin,
                source_path,
                &state.temp_dir,
            ).map_err(|e| format!("docx: {}", e))?;
            let total = state.renderer.page_count(&pdf_path)
                .map_err(|e| format!("page_count: {}", e))?;
            (pdf_path, total)
        }
        FileKind::Image(_) => {
            // 1-page virtual document; renderer is unused for images.
            (source_path.to_path_buf(), 1)
        }
        FileKind::Unknown => {
            return Err(format!("unsupported extension: {}", source_path.display()));
        }
    };

    let selected = select_pages(&state.settings.page_filter, total)?;
    if selected.is_empty() {
        // Page filter excludes every page of this doc. Caller surfaces
        // as an error envelope so the operator sees the bad filter
        // rather than a silently empty .md file.
        return Err(format!(
            "page_filter excludes every page of this document (total={})",
            total
        ));
    }
    let tags = build_tag_pairs(&state.settings, envelope);
    let file_hash = combycode_dpe::envelope::hash_file(
        &work_path.to_string_lossy(), 65536,
    ).unwrap_or_default();

    // 2. Decide strategy. Whole-doc mode requires:
    //      - kind is PDF or DOCX (raw bytes attachable as application/pdf)
    //      - total ≤ strategy.whole_doc_threshold
    //      - total ≤ strategy.whole_doc_max_pages
    //      - file size ≤ strategy.whole_doc_max_bytes
    //    Otherwise per-page (Phase 2 parallel) takes over.
    //    Image inputs always use per-page (n=1) — there's no PDF to
    //    attach as a document; the existing image branch handles them.
    let strategy = decide_strategy(state, &kind, &work_path, total)?;

    match strategy {
        StrategyKind::WholeDoc => {
            // Whole-doc prompt: base + tag overlays + optional whole.txt.
            // Page-range overlays explicitly skipped (per A+D design).
            let prompt = prompt_loader::compose_whole_doc(&prompt_loader::WholeDocInput {
                dir:     Path::new(&state.settings.prompts.dir),
                name:    &state.settings.prompts.name,
                version: &state.settings.prompts.version,
                tags:    &tags,
            }).map_err(|e| format!("whole-doc prompt: {}", e))?;

            // Cache key for whole-doc mode: selected_pages = [] (page
            // filter is ignored in this mode), prompts is the single
            // whole-doc prompt.
            let cache_key = doc_cache_key_v2(
                &state.settings, &file_hash, &[], std::slice::from_ref(&prompt),
                StrategyKind::WholeDoc, total,
            );
            let key_hash = cache_key_hash_of(&cache_key);

            // Mode A sidecar fast-path — gated here (vs strict which
            // process_input handles upfront). Validate the sidecar's
            // recorded key hash against the current settings' hash.
            if let OutputTarget::File { path, invalidation: InvalidationMode::Sidecar } = target {
                if let Some(doc) = try_sidecar_cache_hit(path, &key_hash) {
                    return Ok(ConvertOutcome::Document {
                        doc, cache_hit: true, cache_key_hash: Some(key_hash),
                    });
                }
            }

            let work_path_owned = work_path.clone();
            let cache_key_for_check = cache_key.clone();
            // Run with framework cache only for Mode B (envelope-field
            // output). Mode A persists via the .md file itself; piping
            // through cached_impl would write the body twice.
            let (doc, cache_hit) = if should_use_framework_cache(target) {
                let cache_hit = framework_cache_will_hit(&cache_key_for_check);
                let cached: std::io::Result<CachedDoc> = tokio::task::block_in_place(|| {
                    combycode_dpe::cache::cached_impl::<CachedDoc, _>(
                        CACHE_NAMESPACE,
                        &cache_key,
                        move || {
                            tokio::runtime::Handle::current().block_on(run_whole_doc(
                                state, &work_path_owned, &prompt,
                            ))
                        },
                    )
                });
                (cached.map_err(|e| format!("{}", e))?, cache_hit)
            } else {
                let doc = run_whole_doc(state, &work_path_owned, &prompt).await
                    .map_err(|e| format!("{}", e))?;
                (doc, false)
            };
            Ok(ConvertOutcome::Document {
                doc, cache_hit, cache_key_hash: Some(key_hash),
            })
        }
        StrategyKind::PerPage => {
            // Pre-compose every per-page prompt before the cache lookup.
            let mut prompts_per_page: Vec<String> = Vec::with_capacity(selected.len());
            for &page in &selected {
                let prompt = prompt_loader::compose(&prompt_loader::LoadInput {
                    dir:     Path::new(&state.settings.prompts.dir),
                    name:    &state.settings.prompts.name,
                    version: &state.settings.prompts.version,
                    tags:    &tags,
                    page, total,
                }).map_err(|e| format!("prompt page {}: {}", page, e))?;
                prompts_per_page.push(prompt);
            }

            let cache_key = doc_cache_key_v2(
                &state.settings, &file_hash, &selected, &prompts_per_page,
                StrategyKind::PerPage, total,
            );
            let key_hash = cache_key_hash_of(&cache_key);

            // Mode A sidecar fast-path (see WholeDoc branch).
            if let OutputTarget::File { path, invalidation: InvalidationMode::Sidecar } = target {
                if let Some(doc) = try_sidecar_cache_hit(path, &key_hash) {
                    return Ok(ConvertOutcome::Document {
                        doc, cache_hit: true, cache_key_hash: Some(key_hash),
                    });
                }
            }

            let work_path_owned = work_path.clone();
            let kind_owned      = kind;
            let selected_owned  = selected.clone();
            let prompts_owned   = prompts_per_page;

            let (doc, cache_hit) = if should_use_framework_cache(target) {
                let cache_hit = framework_cache_will_hit(&cache_key);
                let cached: std::io::Result<CachedDoc> = tokio::task::block_in_place(|| {
                    combycode_dpe::cache::cached_impl::<CachedDoc, _>(
                        CACHE_NAMESPACE,
                        &cache_key,
                        move || {
                            tokio::runtime::Handle::current().block_on(run_pages_and_merge(
                                state, &work_path_owned, &kind_owned,
                                &selected_owned, &prompts_owned,
                            ))
                        },
                    )
                });
                (cached.map_err(|e| format!("{}", e))?, cache_hit)
            } else {
                let doc = run_pages_and_merge(
                    state, &work_path_owned, &kind_owned,
                    &selected_owned, &prompts_owned,
                ).await.map_err(|e| format!("{}", e))?;
                (doc, false)
            };
            Ok(ConvertOutcome::Document {
                doc, cache_hit, cache_key_hash: Some(key_hash),
            })
        }
        StrategyKind::PerPageBatch => {
            // Per-page prompts (same shape as PerPage; the difference
            // is dispatch — batch API instead of inline parallel calls).
            let mut prompts_per_page: Vec<String> = Vec::with_capacity(selected.len());
            for &page in &selected {
                let prompt = prompt_loader::compose(&prompt_loader::LoadInput {
                    dir:     Path::new(&state.settings.prompts.dir),
                    name:    &state.settings.prompts.name,
                    version: &state.settings.prompts.version,
                    tags:    &tags,
                    page, total,
                }).map_err(|e| format!("prompt page {}: {}", page, e))?;
                prompts_per_page.push(prompt);
            }
            let cache_key = doc_cache_key_v2(
                &state.settings, &file_hash, &selected, &prompts_per_page,
                StrategyKind::PerPageBatch, total,
            );
            let key_hash = cache_key_hash_of(&cache_key);

            // Mode A sidecar fast-path. Batch's own short-circuit also
            // checks the framework cache; in Mode A we want the .md
            // file to win when its sidecar matches the current settings.
            if let OutputTarget::File { path, invalidation: InvalidationMode::Sidecar } = target {
                if let Some(doc) = try_sidecar_cache_hit(path, &key_hash) {
                    return Ok(ConvertOutcome::Document {
                        doc, cache_hit: true, cache_key_hash: Some(key_hash),
                    });
                }
            }

            run_per_page_batch(
                state, &work_path, &kind, &selected, &prompts_per_page,
                &cache_key, &key_hash, target, source_path, ctx,
            ).await
        }
    }
}

/// Routing decision: WholeDoc when all whole-doc gates pass and
/// the input is a PDF; otherwise the per-page dispatch chosen by
/// `settings.strategy.per_page_mode` (PerPage / PerPageBatch).
/// Image inputs always go to PerPage (no PDF to attach as document
/// AND no benefit from batching one image).
fn decide_strategy(
    state: &ToolState,
    kind: &FileKind,
    work_path: &Path,
    total: u32,
) -> Result<StrategyKind, String> {
    let per_page_mode = match state.settings.strategy.per_page_mode {
        PerPageMode::Parallel => StrategyKind::PerPage,
        PerPageMode::Batch    => StrategyKind::PerPageBatch,
    };
    // Image: no PDF to attach. Always direct per-page (batch a single
    // image isn't useful — submission overhead > savings).
    if matches!(kind, FileKind::Image(_)) {
        return Ok(StrategyKind::PerPage);
    }
    let s = &state.settings.strategy;
    if total > s.whole_doc_threshold {
        return Ok(per_page_mode);
    }
    if total > s.whole_doc_max_pages {
        return Ok(per_page_mode);
    }
    let file_size = std::fs::metadata(work_path)
        .map_err(|e| format!("stat {}: {}", work_path.display(), e))?
        .len();
    if file_size > s.whole_doc_max_bytes {
        return Ok(per_page_mode);
    }
    Ok(StrategyKind::WholeDoc)
}

// ─── Per-page batch orchestrator ─────────────────────────────────────
//
// Three states: cache hit → return now; pending state file → poll;
// neither → submit + persist + poll-or-fire-and-forget.
//
// All status/progress goes through ctx.log; the single meta envelope
// per file (`{source, batch_id}`) goes through ctx.meta; terminal
// failures propagate as ConvertOutcome::Failed and surface via
// ctx.error in process_input (with the original input attached).
#[allow(clippy::too_many_arguments)]
async fn run_per_page_batch(
    state: &ToolState,
    work_path: &Path,
    kind: &FileKind,
    selected: &[u32],
    prompts: &[String],
    cache_key: &Value,
    cache_key_hash: &str,
    target: &OutputTarget,
    source_path: &Path,
    ctx: &mut Context<'_>,
) -> Result<ConvertOutcome, String> {
    // 1. Framework cache hit short-circuit — Mode B only. In Mode A
    //    the file-as-cache check happened at the convert_document
    //    layer (sidecar) or in process_input (strict). We never want
    //    Mode A to read the framework cache JSON because writing it
    //    would be the double-write the redesign eliminates.
    if should_use_framework_cache(target) {
        if let Some(p) = combycode_dpe::cache::cache_path(CACHE_NAMESPACE, cache_key) {
            let mode = combycode_dpe::cache::read_cache_mode();
            if (mode == "use") && p.is_file() {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    if let Ok(doc) = serde_json::from_str::<CachedDoc>(&text) {
                        return Ok(ConvertOutcome::Document {
                            doc, cache_hit: true,
                            cache_key_hash: Some(cache_key_hash.to_string()),
                        });
                    }
                }
            }
        }
    }

    // 2. State-file path. Resolve the configured state_dir (which
    //    typically uses $storage; the runner has already resolved
    //    that prefix when it built our env, so the value we get is
    //    already absolute).
    let state_dir = std::path::PathBuf::from(&state.settings.batch.state_dir);
    let cache_key_hash = cache_key_hash.to_string();

    let now_ms = now_unix_ms();

    // 3. Resume? An existing state file means a batch is already in
    //    flight (or was, before its TTL).
    let pending = batch_state::read_pending(&state_dir, &cache_key_hash, now_ms)
        .map_err(|e| format!("read state file: {}", e))?;

    if let Some(p) = pending {
        // Sanity gate: model + provider must match the new run's
        // settings. If the user changed the model between runs, the
        // cache key would have changed too, so we shouldn't even be
        // here — but as a belt-and-suspenders check, refuse.
        let cur_provider = provider_kind_str(&state.settings.provider);
        if p.provider != cur_provider {
            return Err(format!(
                "state file's provider '{}' doesn't match current settings '{}'; \
                 delete {} or fix settings",
                p.provider, cur_provider,
                batch_state::state_path(&state_dir, &cache_key_hash).display(),
            ));
        }
        return resume_pending_batch(state, work_path, kind, selected, prompts,
                                     &p, &state_dir, &cache_key_hash, cache_key, target,
                                     source_path, ctx).await;
    }

    // 4. Fresh submit. Build BatchRequests — one per selected page.
    //    For images this branch should be unreachable (decide_strategy
    //    returns PerPage for FileKind::Image), but cover defensively.
    if matches!(kind, FileKind::Image(_)) {
        return Err("batch mode does not support image inputs".to_string());
    }
    let mut requests: Vec<BatchRequest> = Vec::with_capacity(selected.len());
    let mut custom_ids: Vec<String> = Vec::with_capacity(selected.len());
    for (idx, &page) in selected.iter().enumerate() {
        let custom_id = format!("page-{}", page);
        let jpeg = state.renderer.render_page_jpeg(work_path, page)
            .map_err(|e| format!("render page {page}: {e}"))?;
        let user = UserContent::InlineImage {
            bytes: jpeg, mime: "image/jpeg".to_string(), text: None,
        };
        requests.push(BatchRequest {
            custom_id: custom_id.clone(),
            system:    prompts[idx].clone(),
            user,
        });
        custom_ids.push(custom_id);
    }

    let cfg = ModelCfg {
        model: state.settings.model.clone(),
        temperature: state.settings.temperature,
        max_tokens: state.settings.max_tokens,
        thinking_budget: state.settings.thinking_budget,
    };

    ctx.log(&format!("batch: submitting {} pages", requests.len()), "info");
    let batch_id = state.provider.submit_batch(requests, &cfg).await
        .map_err(|e| format!("submit_batch: {}", e))?;
    ctx.log(&format!("batch: submitted, id={batch_id}"), "info");

    let provider_str = provider_kind_str(&state.settings.provider);
    let ttl_ms = match state.settings.provider {
        ProviderKind::Anthropic => 24 * 60 * 60 * 1000,    // 24h
        ProviderKind::Google    => 48 * 60 * 60 * 1000,    // 48h
    };

    let pending = batch_state::PendingBatch {
        doc_cache_key_hash: cache_key_hash.clone(),
        batch_id: batch_id.clone(),
        provider: provider_str.to_string(),
        source_path: source_path.to_string_lossy().to_string(),
        page_count: selected.len() as u32,
        submitted_at_ms: now_ms,
        expires_at_ms:  now_ms + ttl_ms,
        model: state.settings.model.clone(),
        custom_ids,
    };
    batch_state::persist_pending(&state_dir, &pending)
        .map_err(|e| format!("persist state file: {}", e))?;

    // Single meta envelope per file: just source + batch_id. Downstream
    // stages use these to correlate this file with the batch_id (for
    // external tracking, manual cancellation, etc.). Status/progress
    // go through ctx.log — the meta channel stays clean.
    ctx.meta(json!({
        "source":   pending.source_path,
        "batch_id": pending.batch_id,
    }));

    // 5. Either fire-and-forget (poll = None) or poll loop until
    //    completion / deadline.
    if state.settings.batch.poll.is_none() {
        return Ok(ConvertOutcome::Pending);
    }

    poll_loop_and_finalize(
        state, &pending, &state_dir, &cache_key_hash, cache_key, target,
        selected, prompts, work_path, kind, source_path, ctx,
    ).await
}

/// Resume path — saved state file exists. Poll once or per the
/// configured cadence, then either return Document (success) /
/// Pending (deadline) / Failed (terminal error).
#[allow(clippy::too_many_arguments)]
async fn resume_pending_batch(
    state: &ToolState,
    work_path: &Path,
    kind: &FileKind,
    selected: &[u32],
    prompts: &[String],
    pending: &batch_state::PendingBatch,
    state_dir: &Path,
    cache_key_hash: &str,
    cache_key: &Value,
    target: &OutputTarget,
    source_path: &Path,
    ctx: &mut Context<'_>,
) -> Result<ConvertOutcome, String> {
    ctx.log(
        &format!("batch: resuming id={} (page_count={})",
            pending.batch_id, pending.page_count),
        "info",
    );
    // Single meta per file (same shape as the fresh-submit path): one
    // record per run with the source + batch_id.
    ctx.meta(json!({
        "source":   pending.source_path,
        "batch_id": pending.batch_id,
    }));

    if state.settings.batch.poll.is_none() {
        // Fire-and-forget mode also applies on resume — try ONE poll;
        // if not done, leave the state file and exit.
        let outcome = state.provider.poll_batch(&pending.batch_id).await
            .map_err(|e| format!("poll_batch: {}", e))?;
        return finalize_outcome(
            state, outcome, pending, state_dir, cache_key_hash, cache_key, target,
            selected, prompts, work_path, kind, source_path, ctx,
        ).await;
    }

    poll_loop_and_finalize(
        state, pending, state_dir, cache_key_hash, cache_key, target,
        selected, prompts, work_path, kind, source_path, ctx,
    ).await
}

/// Poll loop — sleep `start_after_ms`, then poll every `interval_ms`
/// until terminal status or `deadline_ms` wall-clock from NOW.
#[allow(clippy::too_many_arguments)]
async fn poll_loop_and_finalize(
    state: &ToolState,
    pending: &batch_state::PendingBatch,
    state_dir: &Path,
    cache_key_hash: &str,
    cache_key: &Value,
    target: &OutputTarget,
    selected: &[u32],
    prompts: &[String],
    work_path: &Path,
    kind: &FileKind,
    source_path: &Path,
    ctx: &mut Context<'_>,
) -> Result<ConvertOutcome, String> {
    // Callers guard `poll.is_none()` before invoking this function (see
    // the two call sites that gate on `state.settings.batch.poll`); the
    // explicit expect documents that invariant so a third caller can't
    // silently introduce a panic.
    let poll_cfg = state.settings.batch.poll.as_ref()
        .expect("poll_loop_and_finalize requires settings.batch.poll; guard at every call site");
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(poll_cfg.deadline_ms);

    if poll_cfg.start_after_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(poll_cfg.start_after_ms)).await;
    }

    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            // Deadline hit. Leave the state file in place; future
            // run resumes. No data envelope. NOT an error — the
            // input is fine, just not done yet — so it's a warn log,
            // not ctx.error.
            ctx.log(
                &format!("batch: deadline reached for id={} — leaving state file for resume",
                    pending.batch_id),
                "warn",
            );
            return Ok(ConvertOutcome::Pending);
        }
        let outcome = state.provider.poll_batch(&pending.batch_id).await
            .map_err(|e| format!("poll_batch: {}", e))?;
        match outcome {
            BatchStatus::InProgress { processed, total } => {
                ctx.log(
                    &format!("batch: in_progress {processed}/{total}"),
                    "info",
                );
                tokio::time::sleep(
                    std::time::Duration::from_millis(poll_cfg.interval_ms)
                ).await;
                continue;
            }
            terminal => {
                return finalize_outcome(
                    state, terminal, pending, state_dir, cache_key_hash, cache_key, target,
                    selected, prompts, work_path, kind, source_path, ctx,
                ).await;
            }
        }
    }
}

/// Reached a terminal `BatchStatus`. Translate it: success → assemble
/// the document, write to cache, clear state file; failure / expired
/// → clear state file, surface error; in-progress → emit pending meta.
#[allow(clippy::too_many_arguments)]
async fn finalize_outcome(
    state: &ToolState,
    outcome: BatchStatus,
    pending: &batch_state::PendingBatch,
    state_dir: &Path,
    cache_key_hash: &str,
    cache_key: &Value,
    target: &OutputTarget,
    selected: &[u32],
    _prompts: &[String],
    _work_path: &Path,
    _kind: &FileKind,
    _source_path: &Path,
    ctx: &mut Context<'_>,
) -> Result<ConvertOutcome, String> {
    // The per-file meta envelope (`{source, batch_id}`) was emitted
    // at submit/resume time. Status updates from here on:
    //   - "info" / "warn" via ctx.log for non-failure progress
    //   - terminal failures (Failed / Expired / mapping mismatch)
    //     come back as ConvertOutcome::Failed; process_input then
    //     calls ctx.error(&v, &reason) so the framework's
    //     structured error envelope carries the original input.
    match outcome {
        BatchStatus::InProgress { .. } => {
            ctx.log(
                &format!("batch: still in_progress on single-shot poll for id={} — state file kept for resume",
                    pending.batch_id),
                "warn",
            );
            Ok(ConvertOutcome::Pending)
        }
        BatchStatus::Completed { results } => {
            // Map results back to original page order via the
            // submission-order custom_ids list. Any mismatch ↔
            // missing page → terminal failure.
            let bodies = match assemble_batch_bodies(&results, &pending.custom_ids) {
                Ok(b) => b,
                Err(e) => {
                    let _ = batch_state::clear_pending(state_dir, cache_key_hash);
                    return Ok(ConvertOutcome::Failed(
                        format!("batch result mapping (id={}): {e}", pending.batch_id),
                    ));
                }
            };

            // Apply the same merge / frontmatter pipeline as PerPage.
            let mut fms = Vec::with_capacity(bodies.len());
            let mut clean_bodies = Vec::with_capacity(bodies.len());
            for body in &bodies {
                let stripped = strip_fences(body);
                let (fm, md) = frontmatter::extract(&stripped);
                fms.push(fm);
                clean_bodies.push(md);
            }
            let merged_fm = frontmatter::merge_reverse_priority(&fms);
            let body = merge_pages_with_separator(
                &clean_bodies, selected, &state.settings.output.page_separator,
            );
            // Always rebuild the leading frontmatter block when one was
            // parsed — Mode A writes this to disk verbatim, Mode B
            // ships it on the envelope. Body retains its `---` block
            // unconditionally per the redesign (the body shape is the
            // SAME regardless of `extract_frontmatter`; that setting
            // only controls whether parsed keys also become envelope
            // fields downstream).
            let final_text = if !merged_fm.is_empty() {
                format!("{}\n{}", frontmatter::render_block(&merged_fm), body)
            } else {
                body
            };
            let doc = CachedDoc { final_text, frontmatter: merged_fm };

            // Persist to the framework cache only in Mode B. Mode A
            // writes the body to its `.md` file (in process_input);
            // duplicating to `$DPE_STORAGE/doc-converter/<hash>.json`
            // is the double-write the redesign eliminates.
            if should_use_framework_cache(target) {
                if let Some(p) = combycode_dpe::cache::cache_path(CACHE_NAMESPACE, cache_key) {
                    let mode = combycode_dpe::cache::read_cache_mode();
                    if mode != "bypass" && mode != "off" {
                        if let Some(parent) = p.parent() { let _ = std::fs::create_dir_all(parent); }
                        if let Ok(json) = serde_json::to_vec(&doc) {
                            let _ = std::fs::write(&p, json);
                        }
                    }
                }
            }

            // Clear state file — batch is harvested.
            let _ = batch_state::clear_pending(state_dir, cache_key_hash);

            ctx.log(
                &format!("batch: completed id={} ({} pages)",
                    pending.batch_id, pending.page_count),
                "info",
            );
            // Data envelope IS the success signal — no extra meta,
            // no log at "error" level. This branch is the batch-result
            // harvest path: we just dispatched (or polled) the batch
            // API and got back per-page results — never a cache hit.
            Ok(ConvertOutcome::Document {
                doc,
                cache_hit: false,
                cache_key_hash: Some(cache_key_hash.to_string()),
            })
        }
        BatchStatus::Failed { reason } => {
            // Provider returned a terminal failure for this batch.
            // Clear state, return Failed → process_input emits a
            // proper structured error envelope (ctx.error) with the
            // original input attached and routes it to the per-stage
            // error log.
            let _ = batch_state::clear_pending(state_dir, cache_key_hash);
            Ok(ConvertOutcome::Failed(
                format!("batch failed (id={}): {reason}", pending.batch_id),
            ))
        }
        BatchStatus::Expired => {
            // Provider TTL hit before the batch completed. Same path
            // as Failed — terminal, surface as a structured error.
            let _ = batch_state::clear_pending(state_dir, cache_key_hash);
            Ok(ConvertOutcome::Failed(
                format!("batch expired (provider TTL, id={})", pending.batch_id),
            ))
        }
    }
}

/// Map a flat list of `BatchResult` back to one body per selected
/// page. Honours the order in `expected_custom_ids` — providers that
/// echo `custom_id` (Anthropic) match by it; providers that fall back
/// to position-based keys (Gemini inline `key_<i>`) effectively match
/// by index since the position == submission index. Per-item errors
/// abort the whole assembly.
fn assemble_batch_bodies(
    results: &[BatchResult],
    expected_custom_ids: &[String],
) -> Result<Vec<String>, String> {
    if results.len() != expected_custom_ids.len() {
        return Err(format!(
            "result count {} != expected {} pages",
            results.len(), expected_custom_ids.len()
        ));
    }
    let mut bodies: Vec<String> = vec![String::new(); expected_custom_ids.len()];
    for (idx, exp) in expected_custom_ids.iter().enumerate() {
        // Match by custom_id first. If the result doesn't carry the
        // expected id (Gemini inline fallback), fall back to position.
        let pick = results.iter().find(|r| r.custom_id == *exp)
            .or_else(|| results.iter().find(|r| r.custom_id == format!("key_{idx}")))
            .or_else(|| results.get(idx));
        let r = pick.ok_or_else(|| format!(
            "missing batch result for custom_id={}", exp
        ))?;
        match &r.outcome {
            Ok(text) => bodies[idx] = text.clone(),
            Err(e)   => return Err(format!("page {} (custom_id={}): {}", idx + 1, exp, e)),
        }
    }
    Ok(bodies)
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Hash a JSON value to a 32-hex digest. Mirrors what
/// `combycode_dpe::cache::cache_path` does internally so our state
/// file's hash key matches the cache file's hash key (lets a future
/// `dpe doc-converter cleanup-batches` cross-reference them).
fn blake2b_hex32(s: &str) -> String {
    combycode_dpe::envelope::hash_string(s)
}

/// Canonical JSON string of a value (sorted keys recursively). Same
/// shape the framework's cache_path hashes.
fn canonical_json_string(v: &Value) -> String {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Array(a) => Value::Array(a.iter().map(sort).collect()),
            Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                let mut m = serde_json::Map::new();
                for k in keys { m.insert(k.clone(), sort(&o[k])); }
                Value::Object(m)
            }
            other => other.clone(),
        }
    }
    serde_json::to_string(&sort(v)).unwrap_or_default()
}

/// Whole-doc inner. Reads the PDF as bytes, sends one LLM call with
/// it as an `InlineDocument` attachment, packages the response as
/// `CachedDoc`. Frontmatter is extracted from the response (top of the
/// markdown blob) when `output.extract_frontmatter` is on.
async fn run_whole_doc(
    state: &ToolState,
    pdf_path: &Path,
    prompt: &str,
) -> std::io::Result<CachedDoc> {
    let bytes = std::fs::read(pdf_path)
        .map_err(|e| std::io::Error::other(format!("read PDF for whole-doc: {e}")))?;

    let response = call_provider_document(state, prompt, bytes).await
        .map_err(std::io::Error::other)?;

    let (fm, body) = frontmatter::extract(&response);
    // Always reconstruct the leading `---` block when one was parsed.
    // The body retains it unconditionally per the redesign — that's
    // the contract Mode A writes to disk and Mode B ships on the
    // envelope. `extract_frontmatter` controls envelope-side promotion
    // only; it doesn't change the body shape.
    let final_text = if !fm.is_empty() {
        format!("{}\n{}", frontmatter::render_block(&fm), body)
    } else {
        body
    };
    Ok(CachedDoc { final_text, frontmatter: fm })
}

/// Provider call carrying a whole-document attachment. Mirrors
/// `call_provider` (which uses InlineImage) but with InlineDocument.
async fn call_provider_document(
    state: &ToolState,
    system: &str,
    pdf_bytes: Vec<u8>,
) -> Result<String, String> {
    let cfg = ModelCfg {
        model: state.settings.model.clone(),
        temperature: state.settings.temperature,
        max_tokens: state.settings.max_tokens,
        thinking_budget: state.settings.thinking_budget,
    };
    let user = UserContent::InlineDocument {
        bytes: pdf_bytes,
        mime: "application/pdf".to_string(),
        text: None,
    };
    state.provider.message_single(system, user, &cfg).await
        .map_err(|e| format!("{}", e))
        .map(|resp| strip_fences(&resp))
}

/// The cacheable inner. Runs LLM calls per page (parallel-dispatched,
/// capped at `settings.parallel.limit`), extracts frontmatter, merges
/// page bodies with the configured separator, prepends the merged
/// frontmatter block per `extract_frontmatter`. Returns the in-memory
/// artefact; caller writes to disk.
///
/// Concurrency: `futures::stream::buffer_unordered(limit)`. Output
/// preserves the input page order — each future yields its `(idx,
/// body)` pair and we re-sort into a `Vec` indexed by `idx` after
/// collection.
async fn run_pages_and_merge(
    state:    &ToolState,
    work_path: &Path,
    kind:     &FileKind,
    selected: &[u32],
    prompts:  &[String],
) -> std::io::Result<CachedDoc> {
    use futures::stream::{self, StreamExt, TryStreamExt};

    // Validate at runtime — settings parses any u32 via #[serde(default)].
    let limit = state.settings.parallel.limit.max(1) as usize;

    // Pair each page with its prompt + index so we can re-order results.
    let work: Vec<(usize, u32, &str)> = selected.iter().enumerate()
        .map(|(idx, &page)| (idx, page, prompts[idx].as_str()))
        .collect();

    // Stream of (idx, body) results. `buffer_unordered` polls up to
    // `limit` futures concurrently; results land in completion order,
    // not submission order — that's why we carry the idx through.
    let mut results: Vec<(usize, String)> = stream::iter(work.into_iter().map(|(idx, page, prompt)| {
        let prompt = prompt.to_string();
        async move {
            let body = match kind {
                FileKind::Image(ext) => {
                    let bytes = std::fs::read(work_path)
                        .map_err(|e| std::io::Error::other(format!("read image: {e}")))?;
                    let mime = mime_for_image(ext);
                    call_provider(state, &prompt, bytes, mime).await
                        .map_err(std::io::Error::other)?
                }
                _ => {
                    let jpeg = state.renderer.render_page_jpeg(work_path, page)
                        .map_err(|e| std::io::Error::other(format!("render page {page}: {e}")))?;
                    call_provider(state, &prompt, jpeg, "image/jpeg".to_string()).await
                        .map_err(std::io::Error::other)?
                }
            };
            Ok::<(usize, String), std::io::Error>((idx, body))
        }
    }))
    .buffer_unordered(limit)
    .try_collect()
    .await?;

    // Re-order by idx → byte-identical to the legacy sequential path.
    results.sort_by_key(|(idx, _)| *idx);

    // Split bodies + extract frontmatter per page.
    let mut bodies = Vec::with_capacity(results.len());
    let mut fms    = Vec::with_capacity(results.len());
    for (_idx, body) in results {
        let (fm, md) = frontmatter::extract(&body);
        bodies.push(md);
        fms.push(fm);
    }

    let merged_fm = frontmatter::merge_reverse_priority(&fms);
    let body = merge_pages_with_separator(
        &bodies, selected, &state.settings.output.page_separator,
    );
    // Same rule as run_whole_doc / finalize_outcome: rebuild the
    // leading `---` block unconditionally whenever frontmatter was
    // parsed. `extract_frontmatter` no longer gates the body shape.
    let final_text = if !merged_fm.is_empty() {
        format!("{}\n{}", frontmatter::render_block(&merged_fm), body)
    } else {
        body
    };

    Ok(CachedDoc { final_text, frontmatter: merged_fm })
}

/// Combine per-page markdown bodies into a single document. For each
/// page the `template` is applied (with `{n}` → page number) and
/// inserted BEFORE the body, yielding:
///
/// ```text
/// <!-- PAGE: 1 -->
///
/// <body of page 1>
///
/// <!-- PAGE: 2 -->
///
/// <body of page 2>
/// ```
///
/// An empty `template` disables markers entirely and reduces to the
/// legacy `bodies.join("\n\n")` shape — useful for tests and for
/// pipelines that post-process the output and don't want comment noise.
fn merge_pages_with_separator(bodies: &[String], pages: &[u32], template: &str) -> String {
    if template.is_empty() {
        return bodies.iter().map(|b| b.trim()).collect::<Vec<_>>().join("\n\n");
    }
    let mut parts: Vec<String> = Vec::with_capacity(bodies.len() * 2);
    for (idx, body) in bodies.iter().enumerate() {
        let marker = template.replace("{n}", &pages[idx].to_string());
        parts.push(marker);
        parts.push(body.trim().to_string());
    }
    parts.join("\n\n")
}

fn select_pages(ranges: &[String], total: u32) -> Result<Vec<u32>, String> {
    if ranges.is_empty() {
        return Ok((1..=total).collect());
    }
    let parsed = page_range::parse_many(ranges)
        .map_err(|e| format!("page_filter: {}", e))?;
    Ok(page_range::union_pages(&parsed, total))
}

fn build_tag_pairs(settings: &Settings, envelope: &Value) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for name in &settings.overlays {
        // Resolution order:
        //   1. `category` overlay only: settings.category (pipeline-level override)
        //   2. envelope.v[name] (any string field on the envelope)
        //   3. settings.overlay_defaults[name]
        //   4. empty -> overlay skipped
        let from_pipeline = if name == "category" {
            settings.category.clone().unwrap_or_default()
        } else {
            String::new()
        };
        let from_env = envelope.get(name)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let from_default = settings.overlay_defaults.get(name).cloned().unwrap_or_default();
        let value = if !from_pipeline.is_empty() { from_pipeline }
                    else if !from_env.is_empty() { from_env }
                    else { from_default };
        pairs.push((name.clone(), value));
    }
    pairs
}

async fn call_provider(
    state: &ToolState,
    system: &str,
    image_bytes: Vec<u8>,
    mime: String,
) -> Result<String, String> {
    let cfg = ModelCfg {
        model: state.settings.model.clone(),
        temperature: state.settings.temperature,
        max_tokens: state.settings.max_tokens,
        thinking_budget: state.settings.thinking_budget,
    };
    let user = UserContent::InlineImage { bytes: image_bytes, mime, text: None };
    state.provider.message_single(system, user, &cfg).await
        .map_err(|e| format!("{}", e))
        .map(|resp| strip_fences(&resp))
}

// ─── caching ─────────────────────────────────────────────────────────
//
// Doc-level cache (Phase 1 — flipped from per-page in earlier versions).
// One cache entry per document: input file × selected pages × every
// per-page prompt × model knobs × output post-processing rules. The
// cached value is the in-memory `CachedDoc` (final merged text +
// extracted frontmatter); writing the .md file happens AFTER cache
// lookup so changing `output.dir` doesn't bust the cache.
//
// Cache layer is the framework helper: honors DPE_CACHE_MODE (use /
// refresh / bypass / off) and writes under
// $DPE_STORAGE/doc-converter/<hash>.json.

const CACHE_NAMESPACE: &str = "doc-converter";

fn provider_kind_str(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Google    => "google",
    }
}

/// Cache key for one whole-document conversion (legacy — Phase 1
/// shape). Kept for backward compatibility with the existing tests
/// that pin the per-page-only key shape; new code should use
/// `doc_cache_key_v2` which carries `strategy_kind` and `total_pages`.
#[cfg(test)]
fn doc_cache_key(
    settings: &Settings,
    file_hash: &str,
    selected_pages: &[u32],
    prompts_per_page: &[String],
) -> Value {
    doc_cache_key_v2(
        settings, file_hash, selected_pages, prompts_per_page,
        StrategyKind::PerPage,
        // total_pages unknown in this legacy path; tests only compare
        // keys against each other, not against runtime values.
        selected_pages.last().copied().unwrap_or(0),
    )
}

/// Cache key (Phase 3 shape, post-redesign). Includes every input that
/// influences the merged output body text. Output-shape settings that
/// don't affect the body — `output.file.dir`, `output.file.name`,
/// `output.file.target_ext`, `output.field.name`, and now
/// `output.extract_frontmatter` (which only gates envelope promotion;
/// the body shape is identical regardless) — are deliberately
/// excluded so the cache stays warm across runs that move output
/// destinations around or flip frontmatter-promotion.
fn doc_cache_key_v2(
    settings: &Settings,
    file_hash: &str,
    selected_pages: &[u32],
    prompts: &[String],
    strategy: StrategyKind,
    total_pages: u32,
) -> Value {
    // Hash all prompts as one — for per-page mode they're in 1:1
    // order with `selected_pages`; for whole-doc there's exactly one.
    let prompts_concat = prompts.join("\n\u{1F}---\u{1F}\n");
    let prompts_hash   = combycode_dpe::envelope::hash_string(&prompts_concat);

    json!({
        "file_hash":       file_hash,
        "total_pages":     total_pages,
        "selected_pages":  selected_pages,
        "prompts_hash":    prompts_hash,
        // strategy_kind ∈ {"per_page", "whole_doc", "per_page_batch"}.
        // Promoting between strategies invalidates the cache: per-page
        // sees one image at a time; whole-doc sees the entire PDF
        // natively. Different call shapes → different output character.
        "strategy_kind":   strategy.as_str(),
        "model":           settings.model,
        "provider":        provider_kind_str(&settings.provider),
        "temperature":     settings.temperature,
        "max_tokens":      settings.max_tokens,
        "thinking_budget": settings.thinking_budget,
        "page_separator":  settings.output.page_separator,
    })
}

/// Known language tags the LLM may emit as an orphan first line
/// after a bare ``` opening fence. Allow-list (not "any lowercase
/// word") to avoid eating real first-line content like `"hi"` or
/// `"body"`. Curated from real-world LLM behavior — extend if you
/// observe new ones in the wild. Match is exact (no whitespace,
/// no digits, no punctuation).
const ORPHAN_LANGUAGE_TAGS: &[&str] = &[
    "markdown", "md", "yaml", "yml", "json", "text", "txt", "plain",
    "xml", "html", "htm", "toml", "ini", "csv", "tsv",
    "sh", "bash", "shell", "zsh", "fish",
    "python", "py", "rust", "rs", "go", "java", "kotlin", "kt",
    "javascript", "js", "typescript", "ts", "tsx", "jsx",
    "c", "cpp", "cxx", "h", "hpp", "rb", "ruby", "php", "swift",
    "sql",
];

fn strip_fences(s: &str) -> String {
    let t = s.trim();

    // Strip the opening ``` (with or without a language tag). Then,
    // independently, peek at the FIRST line of what's left — if it's
    // a known language tag (yaml / markdown / md / json / etc.), the
    // LLM stuck the language label on its own line BELOW the bare
    // ``` opener instead of inline. Strip that line too. Without this
    // second pass the resulting `.md` starts with a stray `yaml` line
    // above the frontmatter.
    //
    // Allow-list match (NOT "any lowercase word") so we don't eat
    // real first-line content that happens to be a single lowercase
    // English word like "hi" or "body".
    let (t, was_fenced) = if let Some(rest) = t.strip_prefix("```markdown") {
        (rest.trim_start_matches('\n'), true)
    } else if let Some(rest) = t.strip_prefix("```json") {
        (rest.trim_start_matches('\n'), true)
    } else if let Some(rest) = t.strip_prefix("```") {
        (rest.trim_start_matches('\n'), true)
    } else {
        (t, false)
    };

    let t = if was_fenced {
        if let Some(idx) = t.find('\n') {
            let first_line = &t[..idx];
            if ORPHAN_LANGUAGE_TAGS.contains(&first_line) {
                t[idx + 1..].trim_start_matches('\n')
            } else {
                t
            }
        } else {
            t
        }
    } else {
        t
    };

    // Trailing fence — strip ``` + trailing newlines. We deliberately
    // do NOT do the same orphan-language-tag scan on the trailing
    // side: the LLM almost never emits a trailing language tag, and
    // the cost of accidentally eating the last line of content
    // (e.g. a markdown doc that ends with a single-word paragraph) is
    // worse than the rare extra trailing token.
    let t = if let Some(rest) = t.strip_suffix("```") {
        rest.trim_end_matches('\n')
    } else {
        t
    };

    t.trim().to_string()
}

fn mime_for_image(ext: &str) -> String {
    match ext {
        "png"          => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp"         => "image/webp",
        "gif"          => "image/gif",
        _              => "application/octet-stream",
    }.to_string()
}

// `build_output_path` removed — replaced by `resolve_output_target`
// + `filename::NameTemplate`. The new resolver supports both the
// legacy `v.<field>` shorthand and the new `{name:filter}` template
// DSL. See `tools/doc-converter/src/filename.rs`.

// ─── tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_kind_dispatch() {
        assert!(matches!(file_kind(Path::new("a.pdf")),  FileKind::Pdf));
        assert!(matches!(file_kind(Path::new("a.PDF")),  FileKind::Pdf));
        assert!(matches!(file_kind(Path::new("a.docx")), FileKind::Docx));
        assert!(matches!(file_kind(Path::new("a.png")),  FileKind::Image(_)));
        assert!(matches!(file_kind(Path::new("a.xyz")),  FileKind::Unknown));
    }

    #[test]
    fn select_pages_default_all() {
        assert_eq!(select_pages(&[], 5).unwrap(), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn select_pages_union() {
        let r: Vec<String> = vec!["1-3".into(), "0-3".into()];
        assert_eq!(select_pages(&r, 10).unwrap(), vec![1, 2, 3, 8, 9, 10]);
    }

    #[test]
    fn select_pages_bad_range() {
        let r: Vec<String> = vec!["bad".into()];
        assert!(select_pages(&r, 10).is_err());
    }

    #[test]
    fn build_tag_pairs_uses_pipeline_category_and_envelope_tag() {
        let settings = Settings {
            provider: ProviderKind::Anthropic,
            model: "m".into(), temperature: 0.0, max_tokens: 1,
            thinking_budget: None,
            prompts: combycode_dpe_tool_doc_converter::settings::PromptsCfg {
                dir: "/".into(), name: "n".into(), version: "v".into()
            },
            category: Some("alpha".into()),
            overlays: vec!["category".into(), "kind".into()],
            overlay_defaults: Default::default(),
            page_filter: vec![],
            render: Default::default(),
            output: combycode_dpe_tool_doc_converter::settings::OutputCfg {
                file: Some(combycode_dpe_tool_doc_converter::settings::FileOutputCfg {
                    dir: "/".into(),
                    name: "v.path".into(),
                    target_ext: ".md".into(),
                    invalidation: Default::default(),
                }),
                field: None,
                extract_frontmatter: true,
                page_separator: String::new(),
            },
            docx: Default::default(),
            parallel: Default::default(),
            strategy: Default::default(),
            batch: Default::default(),
            request_timeout_ms: 180_000,
            retry: Default::default(),

            on_failure: Default::default(),
        };
        let envelope = json!({"kind": "bravo"});
        let pairs = build_tag_pairs(&settings, &envelope);
        assert_eq!(pairs, vec![
            ("category".into(), "alpha".into()),
            ("kind".into(),     "bravo".into()),
        ]);
    }

    #[test]
    fn build_tag_pairs_falls_back_to_defaults() {
        let mut defaults = std::collections::BTreeMap::new();
        defaults.insert("category".into(), "fallback".into());
        let settings = Settings {
            provider: ProviderKind::Anthropic,
            model: "m".into(), temperature: 0.0, max_tokens: 1,
            thinking_budget: None,
            prompts: combycode_dpe_tool_doc_converter::settings::PromptsCfg {
                dir: "/".into(), name: "n".into(), version: "v".into()
            },
            category: None,
            overlays: vec!["category".into()],
            overlay_defaults: defaults,
            page_filter: vec![],
            render: Default::default(),
            output: combycode_dpe_tool_doc_converter::settings::OutputCfg {
                file: Some(combycode_dpe_tool_doc_converter::settings::FileOutputCfg {
                    dir: "/".into(),
                    name: "v.path".into(),
                    target_ext: ".md".into(),
                    invalidation: Default::default(),
                }),
                field: None,
                extract_frontmatter: true,
                page_separator: String::new(),
            },
            docx: Default::default(),
            parallel: Default::default(),
            strategy: Default::default(),
            batch: Default::default(),
            request_timeout_ms: 180_000,
            retry: Default::default(),

            on_failure: Default::default(),
        };
        let envelope = json!({});
        let pairs = build_tag_pairs(&settings, &envelope);
        assert_eq!(pairs, vec![("category".into(), "fallback".into())]);
    }

    fn output_settings(name: &str) -> Settings {
        Settings {
            provider: ProviderKind::Google,
            model: "m".into(), temperature: 0.0, max_tokens: 1,
            thinking_budget: None,
            prompts: combycode_dpe_tool_doc_converter::settings::PromptsCfg {
                dir: "/".into(), name: "n".into(), version: "v".into()
            },
            category: None, overlays: vec![], overlay_defaults: Default::default(),
            page_filter: vec![],
            render: Default::default(),
            output: combycode_dpe_tool_doc_converter::settings::OutputCfg {
                file: Some(combycode_dpe_tool_doc_converter::settings::FileOutputCfg {
                    dir: "/out".into(),
                    name: name.into(),
                    target_ext: ".md".into(),
                    invalidation: Default::default(),
                }),
                field: None,
                extract_frontmatter: true,
                page_separator: String::new(),
            },
            docx: Default::default(),
            parallel: Default::default(),
            strategy: Default::default(),
            batch: Default::default(),
            request_timeout_ms: 180_000,
            retry: Default::default(),
            on_failure: Default::default(),
        }
    }

    // ─── resolve_output_target: integration tests ────────────────────
    //
    // Filename template parsing + resolution is exhaustively tested in
    // the `filename` module (33 unit tests). These integration tests
    // cover the wiring: target_ext auto-append, File vs Field branch
    // selection, and the legacy `v.<field>` shorthand still resolving
    // through the new path. Older `build_output_path`-targeted tests
    // were removed when that function was deleted; their coverage is
    // strictly subsumed by `filename::tests::resolve_*`.

    #[test]
    fn resolve_output_target_file_mode_shorthand() {
        let settings = output_settings("v.hash");
        let env = json!({"path": "/in/Agreement.pdf", "hash": "abc123"});
        let t = resolve_output_target(&settings, &env).unwrap();
        match t {
            OutputTarget::File { path, .. } => {
                assert_eq!(path, PathBuf::from("/out/abc123.md"));
            }
            _ => panic!("expected File target"),
        }
    }

    #[test]
    fn resolve_output_target_file_mode_template_with_extension_no_double_ext() {
        // `name` already includes `.md` → target_ext NOT appended.
        let settings = output_settings("{hash}.md");
        let env = json!({"hash": "abc"});
        let t = resolve_output_target(&settings, &env).unwrap();
        if let OutputTarget::File { path, .. } = t {
            assert_eq!(path, PathBuf::from("/out/abc.md"));
        } else {
            panic!("expected File target");
        }
    }

    #[test]
    fn resolve_output_target_file_mode_missing_field_errors() {
        let settings = output_settings("v.hash");
        let env = json!({"path": "/x.pdf"});  // no hash
        let err = resolve_output_target(&settings, &env).unwrap_err();
        assert!(err.contains("hash"), "{}", err);
    }

    #[test]
    fn resolve_output_target_rejects_absolute_source_path() {
        // `name: v.path` with an absolute source path must not resolve to the
        // source file itself (join would drop `dir`).
        let settings = output_settings("v.path");
        for p in ["/data/in/foo.pdf", "C:/in/foo.pdf", "../foo.pdf"] {
            let err = resolve_output_target(&settings, &json!({"path": p})).unwrap_err();
            assert!(err.contains("output.file.name"), "{}: {}", p, err);
        }
    }

    #[test]
    fn resolve_output_target_field_mode() {
        let mut settings = output_settings("v.hash");
        settings.output.file = None;
        settings.output.field = Some(
            combycode_dpe_tool_doc_converter::settings::FieldOutputCfg {
                name: "content".into(),
            }
        );
        let env = json!({"hash": "abc"});
        let t = resolve_output_target(&settings, &env).unwrap();
        match t {
            OutputTarget::Field { field_name } => assert_eq!(field_name, "content"),
            _ => panic!("expected Field target"),
        }
    }

    #[test]
    fn strip_fences_cases() {
        assert_eq!(strip_fences("hello"), "hello");
        assert_eq!(strip_fences("```\nhi\n```"), "hi");
        assert_eq!(strip_fences("```markdown\nhi\n```"), "hi");
        assert_eq!(strip_fences("```json\n{\"x\":1}\n```"), "{\"x\":1}");
        assert_eq!(strip_fences("  ```\nhi\n```\n"), "hi");
    }

    // ─── orphan language label after bare ``` (regression) ─
    //
    // The LLM occasionally emits the opening fence as bare ``` then
    // sticks the language tag (yaml, markdown, md, json, ...) on its
    // own line. Without the language-label-line strip, the output
    // starts with a stray `yaml` line above the actual content.

    #[test]
    fn strip_fences_eats_orphan_yaml_label() {
        let raw = "```\nyaml\n---\ndocument_type: alpha\n---\n\n# Heading\n```";
        let out = strip_fences(raw);
        assert!(out.starts_with("---"),
            "should start with frontmatter, not 'yaml': {:?}", out);
        assert!(!out.starts_with("yaml"),
            "the orphan 'yaml' label must be eaten: {:?}", out);
        assert!(out.contains("# Heading"));
    }

    #[test]
    fn strip_fences_eats_orphan_md_label() {
        let raw = "```\nmd\n# Heading\n```";
        assert_eq!(strip_fences(raw), "# Heading");
    }

    #[test]
    fn strip_fences_eats_orphan_markdown_label() {
        let raw = "```\nmarkdown\n# Heading\n\nbody\n```";
        let out = strip_fences(raw);
        assert!(out.starts_with("# Heading"), "got: {:?}", out);
    }

    #[test]
    fn strip_fences_does_not_eat_real_first_line_with_punctuation() {
        // First line "Hello." has a capital + punctuation → not a
        // language tag. Must NOT be stripped, even though it's a
        // single word followed by a newline.
        let raw = "```\nHello.\nWorld\n```";
        assert_eq!(strip_fences(raw), "Hello.\nWorld");
    }

    #[test]
    fn strip_fences_does_not_eat_real_first_line_with_spaces() {
        // First line "hello world" contains a space → not a
        // language tag. Stays as content.
        let raw = "```\nhello world\nbody\n```";
        assert_eq!(strip_fences(raw), "hello world\nbody");
    }

    #[test]
    fn strip_fences_does_not_eat_first_line_with_digits() {
        // Frontmatter-ish start: real content, single word but contains a digit.
        let raw = "```\nv2\nbody\n```";
        assert_eq!(strip_fences(raw), "v2\nbody");
    }

    #[test]
    fn strip_fences_inline_language_still_works() {
        // The original ``` markdown / ``` json paths must still cover
        // the case where the LLM correctly inlines the tag.
        assert_eq!(strip_fences("```markdown\n# H\n```"), "# H");
        assert_eq!(strip_fences("```yaml\nfoo: 1\n```"), "foo: 1",
            "```yaml fence should leave just the content");
    }

    #[test]
    fn strip_fences_no_fence_passes_through_untouched() {
        // The orphan-label strip is gated on `was_fenced`. A plain
        // markdown doc starting with a single lowercase word stays
        // intact when there's no fence at all.
        assert_eq!(strip_fences("yaml\nbody"), "yaml\nbody");
    }

    #[test]
    fn mime_for_image_basic() {
        assert_eq!(mime_for_image("png"),  "image/png");
        assert_eq!(mime_for_image("jpg"),  "image/jpeg");
        assert_eq!(mime_for_image("jpeg"), "image/jpeg");
        assert_eq!(mime_for_image("webp"), "image/webp");
        assert_eq!(mime_for_image("gif"),  "image/gif");
    }

    // ─── doc cache key derivation (Phase 1: per-doc, not per-page) ───
    //
    // The cache key is what determines whether two whole-document
    // conversions hit the same `$DPE_STORAGE/doc-converter/<hash>.json`
    // entry. These tests pin the key composition: same inputs → same
    // key, and any field that affects the merged output MUST diverge
    // the key. Things that DON'T affect the merged output (e.g.
    // output.dir) MUST NOT be in the key — that's how `dpe run` with
    // a different output dir still hits the cache.

    fn mk_settings(provider: ProviderKind, model: &str, temperature: f32) -> Settings {
        Settings {
            provider,
            model: model.into(),
            temperature,
            max_tokens: 1024,
            thinking_budget: None,
            prompts: combycode_dpe_tool_doc_converter::settings::PromptsCfg {
                dir: "/p".into(), name: "n".into(), version: "v".into(),
            },
            category: None,
            overlays: vec![],
            overlay_defaults: Default::default(),
            page_filter: vec![],
            render: Default::default(),
            output: combycode_dpe_tool_doc_converter::settings::OutputCfg {
                file: Some(combycode_dpe_tool_doc_converter::settings::FileOutputCfg {
                    dir: "/o".into(),
                    name: "v.path".into(),
                    target_ext: ".md".into(),
                    invalidation: Default::default(),
                }),
                field: None,
                extract_frontmatter: true,
                page_separator: String::new(),
            },
            docx: Default::default(),
            parallel: Default::default(),
            strategy: Default::default(),
            batch: Default::default(),

            request_timeout_ms: 180_000,

            retry: Default::default(),


            on_failure: Default::default(),
        }
    }

    #[test]
    fn doc_cache_key_stable_for_identical_inputs() {
        let s = mk_settings(ProviderKind::Anthropic, "claude-x", 0.0);
        let a = doc_cache_key(&s, "abc123", &[1, 2], &["p1".into(), "p2".into()]);
        let b = doc_cache_key(&s, "abc123", &[1, 2], &["p1".into(), "p2".into()]);
        assert_eq!(a, b);
    }

    #[test]
    fn doc_cache_key_diverges_on_file_hash() {
        let s = mk_settings(ProviderKind::Anthropic, "claude-x", 0.0);
        let a = doc_cache_key(&s, "abc123", &[1], &["p".into()]);
        let b = doc_cache_key(&s, "def456", &[1], &["p".into()]);
        assert_ne!(a, b);
    }

    #[test]
    fn doc_cache_key_diverges_on_selected_pages() {
        let s = mk_settings(ProviderKind::Anthropic, "claude-x", 0.0);
        let a = doc_cache_key(&s, "abc", &[1, 2, 3], &vec!["p".to_string(); 3]);
        let b = doc_cache_key(&s, "abc", &[1, 2],    &vec!["p".to_string(); 2]);
        assert_ne!(a, b);
    }

    #[test]
    fn doc_cache_key_diverges_on_any_prompt() {
        let s = mk_settings(ProviderKind::Anthropic, "claude-x", 0.0);
        let a = doc_cache_key(&s, "abc", &[1, 2], &["p1".into(), "p2".into()]);
        let b = doc_cache_key(&s, "abc", &[1, 2], &["p1".into(), "p2-DIFF".into()]);
        assert_ne!(a, b, "changing the second page's prompt must invalidate the doc-level key");
    }

    #[test]
    fn doc_cache_key_diverges_on_model() {
        let a = doc_cache_key(&mk_settings(ProviderKind::Anthropic, "claude-x", 0.0),
                              "abc", &[1], &["p".into()]);
        let b = doc_cache_key(&mk_settings(ProviderKind::Anthropic, "claude-y", 0.0),
                              "abc", &[1], &["p".into()]);
        assert_ne!(a, b);
    }

    #[test]
    fn doc_cache_key_diverges_on_provider() {
        let a = doc_cache_key(&mk_settings(ProviderKind::Anthropic, "m", 0.0),
                              "abc", &[1], &["p".into()]);
        let b = doc_cache_key(&mk_settings(ProviderKind::Google, "m", 0.0),
                              "abc", &[1], &["p".into()]);
        assert_ne!(a, b);
    }

    #[test]
    fn doc_cache_key_stable_when_extract_frontmatter_changes() {
        // Post-redesign: `extract_frontmatter` only controls envelope
        // promotion. The body shape is identical regardless (the `---`
        // block is always kept in the body). So the cache key MUST be
        // stable across `extract_frontmatter` flips — flipping the
        // setting should NOT invalidate the LLM-call cache.
        let mut a = mk_settings(ProviderKind::Anthropic, "m", 0.0);
        let mut b = a.clone();
        a.output.extract_frontmatter = true;
        b.output.extract_frontmatter = false;
        assert_eq!(
            doc_cache_key(&a, "abc", &[1], &["p".into()]),
            doc_cache_key(&b, "abc", &[1], &["p".into()]),
        );
    }

    #[test]
    fn doc_cache_key_stable_when_target_ext_changes() {
        // Post-redesign: `target_ext` is filesystem packaging only. The
        // body the LLM produces is identical whether we package it as
        // `.md` or `.txt` — no point invalidating the cache.
        let mut a = mk_settings(ProviderKind::Anthropic, "m", 0.0);
        let mut b = a.clone();
        if let Some(f) = a.output.file.as_mut() { f.target_ext = ".md".into(); }
        if let Some(f) = b.output.file.as_mut() { f.target_ext = ".txt".into(); }
        assert_eq!(
            doc_cache_key(&a, "abc", &[1], &["p".into()]),
            doc_cache_key(&b, "abc", &[1], &["p".into()]),
        );
    }

    #[test]
    fn doc_cache_key_diverges_on_page_separator() {
        let mut a = mk_settings(ProviderKind::Anthropic, "m", 0.0);
        let mut b = a.clone();
        a.output.page_separator = "<!-- PAGE: {n} -->".into();
        b.output.page_separator = "## Page {n}".into();
        assert_ne!(
            doc_cache_key(&a, "abc", &[1, 2], &vec!["p".to_string(); 2]),
            doc_cache_key(&b, "abc", &[1, 2], &vec!["p".to_string(); 2]),
        );
    }

    #[test]
    fn doc_cache_key_stable_when_output_dir_changes() {
        // The output directory is NOT in the cache key by design — the
        // cache holds the in-memory CachedDoc, and the caller writes to
        // disk under whichever dir is configured for the current run.
        let mut a = mk_settings(ProviderKind::Anthropic, "m", 0.0);
        let mut b = a.clone();
        if let Some(f) = a.output.file.as_mut() { f.dir = "/runs/2026-05-09".into(); }
        if let Some(f) = b.output.file.as_mut() { f.dir = "/runs/2026-05-10".into(); }
        assert_eq!(
            doc_cache_key(&a, "abc", &[1], &["p".into()]),
            doc_cache_key(&b, "abc", &[1], &["p".into()]),
        );
    }

    #[test]
    fn doc_cache_key_includes_strategy_kind() {
        // Phase 1 hardcodes "per_page". Sanity-pin the literal so the
        // value is stable for downstream test fixtures and so a future
        // refactor flagging strategy_kind has to update this test too.
        let s = mk_settings(ProviderKind::Anthropic, "m", 0.0);
        let key = doc_cache_key(&s, "abc", &[1], &["p".into()]);
        assert_eq!(key.get("strategy_kind").and_then(|v| v.as_str()),
                   Some("per_page"));
    }

    // ─── page_separator template ─────────────────────────────────────

    #[test]
    fn merge_pages_empty_template_is_join_with_blank_line() {
        // Backward-compat shape: `page_separator: ""` reproduces the
        // legacy `bodies.join("\n\n")` behaviour exactly.
        let bodies = vec!["page one body".to_string(), "page two body".to_string()];
        let pages  = vec![1, 2];
        assert_eq!(merge_pages_with_separator(&bodies, &pages, ""),
                   "page one body\n\npage two body");
    }

    #[test]
    fn merge_pages_default_html_comment_template() {
        let bodies = vec!["page one".to_string(), "page two".to_string()];
        let pages  = vec![1, 2];
        let merged = merge_pages_with_separator(&bodies, &pages, "<!-- PAGE: {n} -->");
        assert_eq!(merged,
            "<!-- PAGE: 1 -->\n\npage one\n\n<!-- PAGE: 2 -->\n\npage two");
    }

    #[test]
    fn merge_pages_substitutes_actual_page_numbers_not_indices() {
        // `pages` carries the original PDF page numbers (which may be
        // discontinuous after page_filter — e.g. "first 2 + last 2"
        // gives [1, 2, 9, 10]). The template's {n} substitution must
        // honour the original page number, not the position in the
        // merged output.
        let bodies = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let pages  = vec![1, 9, 10];
        let merged = merge_pages_with_separator(&bodies, &pages, "<!-- PAGE: {n} -->");
        assert!(merged.contains("<!-- PAGE: 1 -->"));
        assert!(merged.contains("<!-- PAGE: 9 -->"));
        assert!(merged.contains("<!-- PAGE: 10 -->"));
        assert!(!merged.contains("<!-- PAGE: 2 -->"),
            "filtered-out page 2 must NOT appear: got {:?}", merged);
    }

    #[test]
    fn merge_pages_visible_heading_template_works() {
        let bodies = vec!["body".to_string()];
        let pages  = vec![5];
        assert_eq!(
            merge_pages_with_separator(&bodies, &pages, "## Page {n}"),
            "## Page 5\n\nbody",
        );
    }

    #[test]
    fn merge_pages_trims_per_page_whitespace_when_template_set() {
        // Per-page LLM responses sometimes have leading/trailing
        // newlines that would inflate the gap to 4+ blank lines.
        // Trim each body so the doc has predictable spacing.
        let bodies = vec!["\n\npage one\n\n".to_string(), "page two".to_string()];
        let pages  = vec![1, 2];
        let merged = merge_pages_with_separator(&bodies, &pages, "<!-- PAGE: {n} -->");
        // Exactly one blank line between sections:
        assert_eq!(merged,
            "<!-- PAGE: 1 -->\n\npage one\n\n<!-- PAGE: 2 -->\n\npage two");
    }

    // ─── page filter clamp (regression: handle "first 2 + last 3" on a 1-page doc) ─

    #[test]
    fn select_pages_clamps_to_short_doc() {
        // Filter says "first 2 + last 2" (the user's "first N + last M
        // on a short doc" concern). Forward range "1-2" = pages 1..=2;
        // backward range "0-2" = the LAST 2 pages of the doc.
        // page_range::union_pages clamps both to the doc's actual size.
        let r: Vec<String> = vec!["1-2".into(), "0-2".into()];

        // 1-page doc → both ranges collapse to page 1.
        assert_eq!(select_pages(&r, 1).unwrap(), vec![1]);
        // 2-page doc → first-2 + last-2 = full doc, deduped.
        assert_eq!(select_pages(&r, 2).unwrap(), vec![1, 2]);
        // 3-page doc → first-2 = [1,2]; last-2 = [2,3]; union = [1,2,3].
        assert_eq!(select_pages(&r, 3).unwrap(), vec![1, 2, 3]);
        // 5-page doc → first-2 = [1,2]; last-2 = [4,5]; page 3 (middle)
        // is NOT in either range and stays excluded.
        assert_eq!(select_pages(&r, 5).unwrap(), vec![1, 2, 4, 5]);
    }

    #[test]
    fn select_pages_filter_excludes_everything_returns_empty() {
        // "5-10" on a 1-page doc: clamps to nothing. No error from
        // select_pages; convert_document is responsible for surfacing
        // the empty selection as a clear error envelope to the caller
        // (verified separately — convert_document's path in this case
        // returns Err("page_filter excludes every page...")).
        let r: Vec<String> = vec!["5-10".into()];
        assert_eq!(select_pages(&r, 1).unwrap(), Vec::<u32>::new());
    }

    // ─── Phase 2 — buffer_unordered + resort preserves order ─────────
    //
    // The parallel-dispatch pattern in run_pages_and_merge polls up to
    // `parallel.limit` futures concurrently. Results land in COMPLETION
    // order (not submission order), so the submission `idx` is carried
    // through the future and we sort by idx after collection. This test
    // pins the algorithm: future 2 finishes first, then 1, then 0; the
    // sorted output must still be [0, 1, 2]. Without the resort step
    // the merged document's pages would scramble whenever the LLM
    // happens to respond out of order.

    #[tokio::test]
    async fn buffer_unordered_resort_preserves_submission_order() {
        use futures::stream::{self, StreamExt, TryStreamExt};
        use std::time::Duration;

        // (idx, delay_ms) — page 0 is slowest, page 2 is fastest.
        // Without the resort step, completion order = [2, 1, 0].
        let work = vec![(0usize, 90u64), (1, 50), (2, 10)];

        let mut results: Vec<(usize, String)> = stream::iter(
            work.into_iter().map(|(idx, delay)| async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                Ok::<_, std::io::Error>((idx, format!("body-{idx}")))
            })
        )
        .buffer_unordered(3)
        .try_collect()
        .await
        .unwrap();

        results.sort_by_key(|(idx, _)| *idx);

        let bodies: Vec<String> = results.into_iter().map(|(_, b)| b).collect();
        assert_eq!(bodies, vec!["body-0", "body-1", "body-2"],
            "after resort, order must match submission, not completion");
    }

    // ─── Phase 3 — strategy routing + whole-doc cache key ───────────

    fn mk_settings_with_strategy(
        threshold: u32, max_pages: u32, max_bytes: u64,
    ) -> Settings {
        let mut s = mk_settings(ProviderKind::Anthropic, "m", 0.0);
        s.strategy = combycode_dpe_tool_doc_converter::settings::StrategyCfg {
            whole_doc_threshold: threshold,
            whole_doc_max_pages: max_pages,
            whole_doc_max_bytes: max_bytes,
            per_page_mode: combycode_dpe_tool_doc_converter::settings::PerPageMode::Parallel,
        };
        s
    }

    #[test]
    fn doc_cache_key_v2_diverges_on_strategy_kind() {
        // Same inputs, different strategy → different cache slot.
        // This is the property that lets the cache distinguish
        // "I processed this PDF page-by-page" from "I sent the whole
        // PDF as one attachment" — different LLM call shapes produce
        // different output character.
        let s = mk_settings(ProviderKind::Anthropic, "m", 0.0);
        let a = doc_cache_key_v2(&s, "abc", &[1, 2, 3], &vec!["p".into(); 3],
                                  StrategyKind::PerPage,  3);
        let b = doc_cache_key_v2(&s, "abc", &[],          &["whole".to_string()],
                                  StrategyKind::WholeDoc, 3);
        assert_ne!(a, b);
        assert_eq!(a.get("strategy_kind").and_then(|v| v.as_str()), Some("per_page"));
        assert_eq!(b.get("strategy_kind").and_then(|v| v.as_str()), Some("whole_doc"));
    }

    #[test]
    fn doc_cache_key_v2_includes_total_pages() {
        // Same source, same model, same strategy — but total_pages
        // differs because the doc filter changed underneath us. Still
        // an output-affecting input → must be in the key.
        let s = mk_settings(ProviderKind::Anthropic, "m", 0.0);
        let a = doc_cache_key_v2(&s, "abc", &[],  &["p".to_string()], StrategyKind::WholeDoc, 5);
        let b = doc_cache_key_v2(&s, "abc", &[],  &["p".to_string()], StrategyKind::WholeDoc, 7);
        assert_ne!(a, b);
        assert_eq!(a.get("total_pages").and_then(|v| v.as_u64()), Some(5));
        assert_eq!(b.get("total_pages").and_then(|v| v.as_u64()), Some(7));
    }

    #[test]
    fn doc_cache_key_v2_whole_doc_has_empty_selected_pages() {
        // Whole-doc mode ignores the page filter — the LLM gets the
        // whole PDF. The cache key reflects that with selected_pages = [].
        let s = mk_settings(ProviderKind::Anthropic, "m", 0.0);
        let key = doc_cache_key_v2(&s, "abc", &[], &["p".to_string()],
                                    StrategyKind::WholeDoc, 5);
        assert_eq!(key.get("selected_pages").and_then(|v| v.as_array()).map(|a| a.len()),
                   Some(0));
    }

    // Strategy routing — the gating logic in `decide_strategy`. We
    // can't easily build a real ToolState here (Provider, PdfRenderer,
    // tokio runtime), so these tests verify the GATE conditions
    // directly via a minimal helper that mirrors decide_strategy's
    // arithmetic without the I/O.
    fn would_pick_whole_doc(
        kind: &FileKind,
        total: u32,
        file_size: u64,
        s: &combycode_dpe_tool_doc_converter::settings::StrategyCfg,
    ) -> bool {
        if matches!(kind, FileKind::Image(_)) { return false; }
        if total > s.whole_doc_threshold { return false; }
        if total > s.whole_doc_max_pages { return false; }
        if file_size > s.whole_doc_max_bytes { return false; }
        true
    }

    #[test]
    fn strategy_picks_whole_doc_for_short_pdf_under_caps() {
        let s = combycode_dpe_tool_doc_converter::settings::StrategyCfg {
            whole_doc_threshold: 5,
            whole_doc_max_pages: 100,
            whole_doc_max_bytes: 20 * 1024 * 1024,
            per_page_mode: combycode_dpe_tool_doc_converter::settings::PerPageMode::Parallel,
        };
        // 3-page PDF, 1 MB → whole-doc.
        assert!(would_pick_whole_doc(&FileKind::Pdf, 3, 1_000_000, &s));
    }

    #[test]
    fn strategy_falls_back_to_per_page_when_too_many_pages() {
        let s = combycode_dpe_tool_doc_converter::settings::StrategyCfg {
            whole_doc_threshold: 5,
            whole_doc_max_pages: 100,
            whole_doc_max_bytes: 20 * 1024 * 1024,
            per_page_mode: combycode_dpe_tool_doc_converter::settings::PerPageMode::Parallel,
        };
        // 6 pages > threshold(5) → per-page even though under max_pages.
        assert!(!would_pick_whole_doc(&FileKind::Pdf, 6, 1_000, &s));
        // 200 pages > max_pages(100) → per-page even at low threshold.
        let mut s = s.clone();
        s.whole_doc_threshold = 1000;     // wide-open threshold
        assert!(!would_pick_whole_doc(&FileKind::Pdf, 200, 1_000, &s));
    }

    #[test]
    fn strategy_falls_back_to_per_page_when_file_too_large() {
        let s = combycode_dpe_tool_doc_converter::settings::StrategyCfg {
            whole_doc_threshold: 5,
            whole_doc_max_pages: 100,
            whole_doc_max_bytes: 1_000_000,         // 1 MB cap for this test
            per_page_mode: combycode_dpe_tool_doc_converter::settings::PerPageMode::Parallel,
        };
        // 3 pages but 2 MB file → per-page (size cap takes over).
        assert!(!would_pick_whole_doc(&FileKind::Pdf, 3, 2_000_000, &s));
    }

    #[test]
    fn strategy_image_input_always_per_page() {
        let s = combycode_dpe_tool_doc_converter::settings::StrategyCfg {
            whole_doc_threshold: 5,
            whole_doc_max_pages: 100,
            whole_doc_max_bytes: 20 * 1024 * 1024,
            per_page_mode: combycode_dpe_tool_doc_converter::settings::PerPageMode::Parallel,
        };
        // Even though every numeric gate passes, image inputs go
        // per-page (no PDF to attach as a document). Routing must
        // refuse whole-doc for FileKind::Image.
        let img = FileKind::Image("png".into());
        assert!(!would_pick_whole_doc(&img, 1, 100, &s));
    }

    #[test]
    fn strategy_threshold_zero_disables_whole_doc_entirely() {
        let s = combycode_dpe_tool_doc_converter::settings::StrategyCfg {
            whole_doc_threshold: 0,                  // disable
            whole_doc_max_pages: 100,
            whole_doc_max_bytes: 20 * 1024 * 1024,
            per_page_mode: combycode_dpe_tool_doc_converter::settings::PerPageMode::Parallel,
        };
        // 1-page tiny PDF → still per-page because threshold=0.
        assert!(!would_pick_whole_doc(&FileKind::Pdf, 1, 1_000, &s));
    }

    #[test]
    fn strategy_kind_as_str_round_trips() {
        assert_eq!(StrategyKind::PerPage.as_str(),  "per_page");
        assert_eq!(StrategyKind::WholeDoc.as_str(), "whole_doc");
    }

    // (StrategyCfg defaults are pinned in settings tests.)
    #[allow(dead_code)]
    fn _unused_silence(_s: &Settings) {
        // Suppresses the warning about mk_settings_with_strategy being
        // unused as long as the field-divergence assertion above still
        // compiles. If we add tests that use it, drop this stub.
        let _ = mk_settings_with_strategy(5, 100, 20 * 1024 * 1024);
    }

    #[tokio::test]
    async fn buffer_unordered_concurrency_cap_honoured() {
        // Pin the bounded-concurrency invariant: with limit=2 and 5
        // tasks each sleeping 20ms, the runtime never has more than
        // 2 tasks in flight simultaneously, AND parallelism is
        // actually engaged (peak > 1).
        use futures::stream::{self, StreamExt, TryStreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        let in_flight     = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));

        let limit = 2usize;
        let n = 5usize;

        let _: Vec<()> = stream::iter((0..n).map(|_| {
            let in_f = in_flight.clone();
            let max_f = max_in_flight.clone();
            async move {
                let now = in_f.fetch_add(1, Ordering::SeqCst) + 1;
                let mut peak = max_f.load(Ordering::SeqCst);
                while peak < now {
                    match max_f.compare_exchange(peak, now,
                        Ordering::SeqCst, Ordering::SeqCst) {
                        Ok(_)  => break,
                        Err(c) => peak = c,
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                in_f.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, std::io::Error>(())
            }
        }))
        .buffer_unordered(limit)
        .try_collect()
        .await
        .unwrap();

        let observed_max = max_in_flight.load(Ordering::SeqCst);
        assert!(observed_max <= limit,
            "observed max in-flight = {observed_max}, must be ≤ {limit}");
        assert!(observed_max > 1,
            "observed max in-flight = {observed_max}, parallelism not actually engaged");
    }

    #[test]
    fn doc_cache_key_diverges_on_temperature() {
        let a = doc_cache_key(&mk_settings(ProviderKind::Anthropic, "m", 0.0),
                              "abc", &[1], &["p".into()]);
        let b = doc_cache_key(&mk_settings(ProviderKind::Anthropic, "m", 0.7),
                              "abc", &[1], &["p".into()]);
        assert_ne!(a, b);
    }

    #[test]
    fn doc_cache_key_includes_prompt_hash_not_full_text() {
        let s = mk_settings(ProviderKind::Anthropic, "m", 0.0);
        let key = doc_cache_key(&s, "abc", &[1], &["long system prompt here".into()]);
        // Key carries the hash of all per-page prompts concatenated,
        // not the raw text — keeps key payload small and stable.
        assert!(key.get("prompts_hash").and_then(|v| v.as_str()).is_some());
        assert_eq!(key.get("prompts_hash").unwrap().as_str().unwrap().len(), 16);
        assert!(key.get("prompts").is_none(),
            "raw prompts must NOT leak into the key");
    }

    // ─── --dump-prompt: compose_dump_envelopes ───────────────────────
    //
    // The CLI's debug mode writes prompts derived ENTIRELY from settings
    // + the envelope's overlay tag values + page/total — no LLM, no file
    // I/O, no caching. These tests pin the envelope shape and the
    // settings-→-prompt composition that callers will be diff'ing
    // against in their pre-flight scripts.

    fn dump_settings(prompts_dir: &str, overlays: &[&str], page_filter: &[&str]) -> Settings {
        Settings {
            provider: ProviderKind::Anthropic,
            model: "m".into(),
            temperature: 0.0,
            max_tokens: 1024,
            thinking_budget: None,
            prompts: combycode_dpe_tool_doc_converter::settings::PromptsCfg {
                dir: prompts_dir.into(),
                name: "convert".into(),
                version: "v1".into(),
            },
            category: None,
            overlays: overlays.iter().map(|s| s.to_string()).collect(),
            overlay_defaults: Default::default(),
            page_filter: page_filter.iter().map(|s| s.to_string()).collect(),
            render: Default::default(),
            output: combycode_dpe_tool_doc_converter::settings::OutputCfg {
                file: Some(combycode_dpe_tool_doc_converter::settings::FileOutputCfg {
                    dir: "/o".into(),
                    name: "v.path".into(),
                    target_ext: ".md".into(),
                    invalidation: Default::default(),
                }),
                field: None,
                extract_frontmatter: true,
                page_separator: String::new(),
            },
            docx: Default::default(),
            parallel: Default::default(),
            strategy: Default::default(),
            batch: Default::default(),

            request_timeout_ms: 180_000,

            retry: Default::default(),


            on_failure: Default::default(),
        }
    }

    fn write_prompt(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn dump_envelope_uses_base_only_when_no_overlays_match() {
        let d = tempfile::tempdir().unwrap();
        write_prompt(d.path(), "convert_v1.txt", "BASE");
        let settings = dump_settings(d.path().to_str().unwrap(), &[], &[]);
        let env = json!({"t":"d","id":"x","src":"y","v":{}});
        let out = compose_dump_envelopes(&settings, &env).unwrap();
        assert_eq!(out.len(), 1);
        let v = &out[0]["v"];
        assert_eq!(v["page"], 1);
        assert_eq!(v["total"], 1);
        assert_eq!(v["prompt"], "BASE");
        // id/src round-tripped from input.
        assert_eq!(out[0]["id"], "x");
        assert_eq!(out[0]["src"], "y");
    }

    #[test]
    fn dump_envelope_resolves_category_from_settings_then_envelope() {
        let d = tempfile::tempdir().unwrap();
        write_prompt(d.path(), "convert_v1.txt", "BASE");
        write_prompt(d.path(), "convert_v1_alpha.txt", "ALPHA");
        write_prompt(d.path(), "convert_v1_bravo.txt", "BRAVO");
        // category overlay → settings wins over envelope.
        let mut s_alpha = dump_settings(d.path().to_str().unwrap(), &["category"], &[]);
        s_alpha.category = Some("alpha".into());
        let env = json!({"t":"d","v":{"category":"bravo"}});
        let out = compose_dump_envelopes(&s_alpha, &env).unwrap();
        assert_eq!(out[0]["v"]["prompt"], "BASE\n\n---\n\nALPHA",
            "settings.category=alpha must win over v.category=bravo");
    }

    #[test]
    fn dump_envelope_uses_v_page_and_total_when_provided() {
        let d = tempfile::tempdir().unwrap();
        write_prompt(d.path(), "convert_v1.txt", "BASE");
        // Page-3-of-10 overlay covers page 3 only.
        write_prompt(d.path(), "convert_v1_p3-1.txt", "PAGE3");
        let settings = dump_settings(d.path().to_str().unwrap(), &[], &[]);
        let env = json!({"t":"d","v":{"page":3,"total":10}});
        let out = compose_dump_envelopes(&settings, &env).unwrap();
        // No page_filter → emit for every page 1..=10. Page 3 picks up the overlay.
        assert_eq!(out.len(), 10);
        for env in &out {
            let p = env["v"]["page"].as_u64().unwrap();
            let prompt = env["v"]["prompt"].as_str().unwrap();
            if p == 3 {
                assert!(prompt.contains("PAGE3"), "page 3 should include overlay; got {prompt:?}");
            } else {
                assert_eq!(prompt, "BASE", "page {p} should be base only; got {prompt:?}");
            }
        }
    }

    #[test]
    fn dump_envelope_respects_page_filter() {
        let d = tempfile::tempdir().unwrap();
        write_prompt(d.path(), "convert_v1.txt", "BASE");
        // page_filter narrows the enumeration; total still drives overlay coverage.
        let settings = dump_settings(d.path().to_str().unwrap(), &[], &["1-2", "0-1"]);
        let env = json!({"t":"d","v":{"total":10}});
        let out = compose_dump_envelopes(&settings, &env).unwrap();
        let pages: Vec<u64> = out.iter().map(|e| e["v"]["page"].as_u64().unwrap()).collect();
        assert_eq!(pages, vec![1, 2, 10], "page_filter 1-2 + 0-1 on total=10 → pages 1,2,10");
    }

    #[test]
    fn dump_envelope_skips_meta_envelopes_in_run_dump_prompt() {
        // compose_dump_envelopes itself doesn't filter on `t` — that's the
        // caller's job (run_dump_prompt does it). Here we just confirm a
        // missing `t` is fine: helper is permissive.
        let d = tempfile::tempdir().unwrap();
        write_prompt(d.path(), "convert_v1.txt", "BASE");
        let settings = dump_settings(d.path().to_str().unwrap(), &[], &[]);
        let env = json!({"v":{}});
        assert_eq!(compose_dump_envelopes(&settings, &env).unwrap().len(), 1);
    }
}
