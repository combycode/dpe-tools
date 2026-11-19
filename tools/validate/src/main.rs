//! validate — JSON Schema validator stage.
//!
//! Two operational modes:
//!
//! - `mark` — write `v._valid` + `v._validation_error`; emit every
//!   envelope on `data` regardless of result.
//! - `filter` — emit valid envelopes verbatim; route invalid to the
//!   stage's errors log via `ctx.error()`. NOT emitted on data;
//!   downstream can't route on filtered-out items.
//!
//! Schema files are loaded once on first envelope (and reloaded if the
//! `schema:` setting changes mid-run, which shouldn't happen but is
//! handled). YAML schema files are accepted alongside JSON — extension
//! `.yaml` / `.yml` triggers YAML parsing via serde-saphyr (workspace
//! YAML policy).
//!
//! `additional_properties: true` (the default) strips any root-level
//! `additionalProperties` keyword from the schema BEFORE compilation.
//! This lets schema authors target the fields they care about without
//! their schemas breaking on envelope fields added upstream
//! (`cache_hit`, `path`, hash, etc.). Set `false` to use the schema
//! verbatim.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use combycode_dpe::prelude::*;
use combycode_dpe::dpe_run;
use jsonschema::Validator;
use serde::Deserialize;
use serde_json::Value;

// ─── Settings ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    source: String,
    schema: String,
    #[serde(default = "default_format")]
    format: String,
    #[serde(default)]
    mode: Mode,
    #[serde(default = "default_mark_field")]
    mark_field: String,
    #[serde(default = "default_error_field")]
    error_field: String,
    #[serde(default = "default_additional_properties")]
    additional_properties: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Mode {
    #[default]
    Mark,
    Filter,
}

fn default_format() -> String { "json".into() }
fn default_mark_field() -> String { "_valid".into() }
fn default_error_field() -> String { "_validation_error".into() }
fn default_additional_properties() -> bool { true }

// ─── Schema cache ────────────────────────────────────────────────────

struct Cached {
    path: PathBuf,
    additional_properties: bool,
    validator: Arc<Validator>,
}

// Re-loading the schema for every envelope would be wasteful when a
// stage processes thousands. Cache the compiled validator keyed on
// (path, additional_properties) — both inputs to compilation. Single-
// threaded in practice (the framework calls process_input serially),
// but Mutex keeps the static safely shareable. Callers get an Arc of the
// validator they asked for, so a concurrent reload for other settings
// can never swap the schema between compile and validate.
static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

fn load_schema_text(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read schema {}: {}", path.display(), e))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if ext == "yaml" || ext == "yml" {
        let opts = serde_saphyr::options!(
            strict_booleans:      true,
            no_schema:            true,
            legacy_octal_numbers: false,
            // keep .inf/.nan/-.inf as strings, as serde-saphyr 0.0.x did
            non_finite_float_policy: serde_saphyr::options::NonFiniteFloatPolicy::AsString,
            // deeper YAML is a clean error; ~250 levels overflow a 1 MB (Windows main-thread) stack
            budget: serde_saphyr::budget!(max_depth: 128),
        );
        serde_saphyr::from_str_with_options::<Value>(&text, opts)
            .map_err(|e| format!("parse schema yaml {}: {}", path.display(), e))
    } else {
        serde_json::from_str::<Value>(&text)
            .map_err(|e| format!("parse schema json {}: {}", path.display(), e))
    }
}

fn compile_schema(raw: Value, additional_properties: bool) -> Result<Validator, String> {
    let prepared = if additional_properties {
        strip_root_additional_properties(raw)
    } else {
        raw
    };
    jsonschema::validator_for(&prepared)
        .map_err(|e| format!("compile schema: {}", e))
}

/// Remove a root-level `additionalProperties` keyword (if present). Only
/// the root object is touched — nested subschemas keep their own
/// declarations because users may legitimately want strict nested
/// objects with a permissive root.
fn strip_root_additional_properties(v: Value) -> Value {
    match v {
        Value::Object(mut m) => {
            m.remove("additionalProperties");
            Value::Object(m)
        }
        other => other,
    }
}

fn ensure_compiled(settings: &Settings) -> Result<Arc<Validator>, String> {
    let mut guard = CACHE.lock().map_err(|e| format!("cache lock poisoned: {}", e))?;
    let path = PathBuf::from(&settings.schema);
    let needs_reload = match guard.as_ref() {
        None => true,
        Some(c) => c.path != path || c.additional_properties != settings.additional_properties,
    };
    if needs_reload {
        let raw = load_schema_text(&path)?;
        let validator = compile_schema(raw, settings.additional_properties)?;
        *guard = Some(Cached {
            path,
            additional_properties: settings.additional_properties,
            validator: Arc::new(validator),
        });
    }
    guard.as_ref()
        .map(|c| Arc::clone(&c.validator))
        .ok_or_else(|| "validator missing after compile".to_string())
}

// ─── Source resolution ───────────────────────────────────────────────

/// Resolve `source` against the envelope's `v`. Special-case `"v"` →
/// whole v object. Otherwise split on `.` and walk: `v.foo.bar` reads
/// `v["foo"]["bar"]`. Returns None when any segment is missing.
fn resolve_source<'a>(v: &'a Value, source: &str) -> Option<&'a Value> {
    let trimmed = source.trim();
    if trimmed == "v" { return Some(v); }
    let rest = trimmed.strip_prefix("v.")?;
    let mut cur = v;
    for seg in rest.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// If `value` is a string, parse it per `format`. Otherwise return it
/// as-is. Currently only `json` is supported (per spec.yaml; future
/// formats added here).
fn parse_string_source(value: &Value, format: &str) -> Result<Value, String> {
    if let Some(s) = value.as_str() {
        match format {
            "json" => serde_json::from_str::<Value>(s)
                .map_err(|e| format!("parse error: {}", e)),
            other  => Err(format!("unsupported format '{}'", other)),
        }
    } else {
        Ok(value.clone())
    }
}

// ─── Validation ──────────────────────────────────────────────────────

/// Run validation. Returns Ok(()) on pass, Err(reason) on first
/// violation. Reason format mirrors the spec:
///   `"<json-pointer>: <keyword> — <human detail>"`
fn validate_value(validator: &Validator, value: &Value) -> Result<(), String> {
    let mut errs = validator.iter_errors(value);
    if let Some(first) = errs.next() {
        let pointer_str = first.instance_path().to_string();
        let pointer = if pointer_str.is_empty() { "/".to_string() } else { pointer_str };
        // `kind()` returns a `&ValidationErrorKind`. Its Debug output
        // is `Variant { fields... }` for struct-like variants. We want
        // only the variant name, lowercased, to match the JSON Schema
        // keyword the user wrote ("enum", "required", "type", ...).
        let debug = format!("{:?}", first.kind());
        let keyword = debug
            .split_whitespace().next().unwrap_or(&debug)
            .trim_end_matches('{').trim()
            .to_lowercase();
        // jsonschema's error Display already produces a readable
        // "value X is not valid under ..." style message; we prepend
        // the pointer + keyword so they're unambiguous.
        Err(format!("{}: {} — {}", pointer, keyword, first))
    } else {
        Ok(())
    }
}

// ─── Processing ──────────────────────────────────────────────────────

fn process_input(v: Value, raw_settings: &Value, ctx: &mut Context<'_>) {
    let settings: Settings = match serde_json::from_value(raw_settings.clone()) {
        Ok(s)  => s,
        Err(e) => { ctx.error(&v, &format!("bad settings: {}", e)); return; }
    };
    let validator = match ensure_compiled(&settings) {
        Ok(val) => val,
        Err(e)  => { ctx.error(&v, &e); return; }
    };

    // Resolve + parse the source value.
    let raw = match resolve_source(&v, &settings.source) {
        Some(r) => r.clone(),
        None    => {
            let reason = format!("source path '{}' not found", settings.source);
            return emit_invalid(v, &settings, &reason, ctx);
        }
    };
    let parsed = match parse_string_source(&raw, &settings.format) {
        Ok(p)  => p,
        Err(e) => return emit_invalid(v, &settings, &e, ctx),
    };

    // Validate.
    let result = validate_value(&validator, &parsed);

    match result {
        Ok(()) => emit_valid(v, &settings, ctx),
        Err(e) => emit_invalid(v, &settings, &e, ctx),
    }
}

fn emit_valid(v: Value, settings: &Settings, ctx: &mut Context<'_>) {
    match settings.mode {
        Mode::Mark => {
            let mut out = v;
            mark_v(&mut out, settings, true, None);
            ctx.output(out, None, None);
        }
        Mode::Filter => {
            ctx.output(v, None, None);
        }
    }
}

fn emit_invalid(v: Value, settings: &Settings, reason: &str, ctx: &mut Context<'_>) {
    match settings.mode {
        Mode::Mark => {
            let mut out = v;
            mark_v(&mut out, settings, false, Some(reason));
            ctx.output(out, None, None);
        }
        Mode::Filter => {
            ctx.error(&v, reason);
        }
    }
}

/// Write `mark_field` (bool) + `error_field` (string|null) onto the
/// v value. The framework strips the outer envelope before dispatch,
/// so the value passed to `process_input` IS v already; the marks
/// land at v's top level. If v isn't an object (rare — most stages
/// emit objects), wrap it in `{ "_value": v, ...marks }` so the
/// invariant "marks appear on v" still holds rather than dropping
/// silently.
fn mark_v(v: &mut Value, settings: &Settings, valid: bool, reason: Option<&str>) {
    let reason_v = match reason {
        Some(s) => Value::String(s.to_string()),
        None    => Value::Null,
    };
    if let Value::Object(m) = v {
        m.insert(settings.mark_field.clone(), Value::Bool(valid));
        m.insert(settings.error_field.clone(), reason_v);
    } else {
        let mut wrapper = serde_json::Map::new();
        wrapper.insert("_value".into(), v.take());
        wrapper.insert(settings.mark_field.clone(), Value::Bool(valid));
        wrapper.insert(settings.error_field.clone(), reason_v);
        *v = Value::Object(wrapper);
    }
}

fn main() {
    dpe_run! {
        input: process_input,
    };
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings_mark(schema_path: &str, source: &str) -> Settings {
        Settings {
            source: source.to_string(),
            schema: schema_path.to_string(),
            format: "json".into(),
            mode: Mode::Mark,
            mark_field: "_valid".into(),
            error_field: "_validation_error".into(),
            additional_properties: true,
        }
    }

    fn write_schema(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("schema.json");
        std::fs::write(&p, body).unwrap();
        p
    }

    fn clear_cache() {
        if let Ok(mut g) = CACHE.lock() { *g = None; }
    }

    #[test]
    fn concurrent_reloads_never_swap_the_validator() {
        // Two settings alternate on the shared cache from two threads; each
        // must always validate with its own schema (regression: tests failed
        // ~2 in 5 parallel runs when another thread recompiled in between).
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        std::fs::write(&a, r#"{"type":"object","required":["a"]}"#).unwrap();
        std::fs::write(&b, r#"{"type":"object","required":["b"]}"#).unwrap();
        let run = |schema: PathBuf, ok: Value, bad: Value| {
            std::thread::spawn(move || {
                let s = settings_mark(schema.to_str().unwrap(), "v");
                for _ in 0..200 {
                    let validator = ensure_compiled(&s).unwrap();
                    assert!(validate_value(&validator, &ok).is_ok());
                    assert!(validate_value(&validator, &bad).is_err());
                }
            })
        };
        let ta = run(a, json!({"a":1}), json!({"b":1}));
        let tb = run(b, json!({"b":1}), json!({"a":1}));
        ta.join().unwrap();
        tb.join().unwrap();
    }

    // ─── settings parsing ───────────────────────────────────────────

    #[test]
    fn settings_defaults_applied_when_minimal() {
        let raw = json!({ "source": "v", "schema": "/tmp/x.json" });
        let s: Settings = serde_json::from_value(raw).unwrap();
        assert_eq!(s.format, "json");
        assert_eq!(s.mode, Mode::Mark);
        assert_eq!(s.mark_field, "_valid");
        assert_eq!(s.error_field, "_validation_error");
        assert!(s.additional_properties);
    }

    #[test]
    fn settings_rejects_unknown_field() {
        let raw = json!({ "source": "v", "schema": "x", "bogus": 1 });
        assert!(serde_json::from_value::<Settings>(raw).is_err());
    }

    // ─── source resolution ──────────────────────────────────────────

    // The framework's `process_input(v, ...)` passes the INNER `v`
    // object — the outer envelope is stripped before dispatch. So
    // `source: v` returns the whole v map; `source: v.response`
    // walks v.response.
    #[test]
    fn source_v_returns_whole_v() {
        let v = json!({"a": 1, "b": 2});
        let got = resolve_source(&v, "v").unwrap();
        assert_eq!(got, &v);
    }

    #[test]
    fn source_dotted_path_walks_v() {
        let v = json!({"response": "{\"x\":1}", "nested": {"y": 2}});
        assert_eq!(resolve_source(&v, "v.response").unwrap(), &json!("{\"x\":1}"));
        assert_eq!(resolve_source(&v, "v.nested.y").unwrap(), &json!(2));
    }

    #[test]
    fn source_missing_path_returns_none() {
        let v = json!({});
        assert!(resolve_source(&v, "v.missing").is_none());
    }

    // ─── string parsing ─────────────────────────────────────────────

    #[test]
    fn string_source_parsed_as_json_when_format_json() {
        let v = json!("{\"x\": 1}");
        let parsed = parse_string_source(&v, "json").unwrap();
        assert_eq!(parsed, json!({"x": 1}));
    }

    #[test]
    fn non_string_source_returned_as_is() {
        let v = json!({"x": 1});
        let out = parse_string_source(&v, "json").unwrap();
        assert_eq!(out, v);
    }

    #[test]
    fn string_source_parse_failure_surfaced() {
        let v = json!("not valid json {");
        let err = parse_string_source(&v, "json").unwrap_err();
        assert!(err.starts_with("parse error:"), "got: {err}");
    }

    // ─── strip-root-additional-properties ───────────────────────────

    #[test]
    fn strip_removes_only_root_additional_properties() {
        let s = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "nested": {
                    "type": "object",
                    "additionalProperties": false
                }
            }
        });
        let stripped = strip_root_additional_properties(s);
        let obj = stripped.as_object().unwrap();
        assert!(obj.get("additionalProperties").is_none());
        // Nested untouched.
        let nested_ap = obj["properties"]["nested"]["additionalProperties"].clone();
        assert_eq!(nested_ap, json!(false));
    }

    // ─── end-to-end mark / filter ───────────────────────────────────

    #[test]
    fn case_valid_object_passes_mark() {
        clear_cache();
        let dir = tempfile::tempdir().unwrap();
        let schema = write_schema(dir.path(), r#"{
            "type":"object",
            "required":["document_type"],
            "properties":{"document_type":{"type":"string","enum":["alpha","beta"]}}
        }"#);
        let s = settings_mark(schema.to_str().unwrap(), "v");
        let validator = ensure_compiled(&s).unwrap();
        let v = json!({"document_type":"alpha"});
        let parsed = parse_string_source(&v, "json").unwrap();
        assert!(validate_value(&validator, &parsed).is_ok());
    }

    #[test]
    fn case_invalid_object_fails_mark() {
        clear_cache();
        let dir = tempfile::tempdir().unwrap();
        let schema = write_schema(dir.path(), r#"{
            "type":"object",
            "required":["document_type"],
            "properties":{"document_type":{"type":"string"}}
        }"#);
        let s = settings_mark(schema.to_str().unwrap(), "v");
        let validator = ensure_compiled(&s).unwrap();
        let v = json!({"other":"x"});
        let err = validate_value(&validator, &v).unwrap_err();
        assert!(err.contains("document_type") || err.contains("required"), "got: {err}");
    }

    #[test]
    fn case_bad_enum_value() {
        clear_cache();
        let dir = tempfile::tempdir().unwrap();
        let schema = write_schema(dir.path(), r#"{
            "type":"object",
            "properties":{"document_type":{"enum":["alpha","beta"]}}
        }"#);
        let s = settings_mark(schema.to_str().unwrap(), "v");
        let validator = ensure_compiled(&s).unwrap();
        let v = json!({"document_type":"UNKNOWN"});
        let err = validate_value(&validator, &v).unwrap_err();
        assert!(err.contains("/document_type"), "got: {err}");
    }

    #[test]
    fn case_string_source_valid_json() {
        clear_cache();
        let dir = tempfile::tempdir().unwrap();
        let schema = write_schema(dir.path(), r#"{
            "type":"object",
            "required":["plan_id"]
        }"#);
        let s = settings_mark(schema.to_str().unwrap(), "v.response");
        let validator = ensure_compiled(&s).unwrap();
        let v = json!({"response":"{\"plan_id\":\"P1\"}"});
        let src = resolve_source(&v, &s.source).unwrap();
        let parsed = parse_string_source(src, &s.format).unwrap();
        assert!(validate_value(&validator, &parsed).is_ok());
    }

    #[test]
    fn case_string_source_parse_failure() {
        let v = json!({"response":"this is prose not json"});
        let src = resolve_source(&v, "v.response").unwrap();
        let err = parse_string_source(src, "json").unwrap_err();
        assert!(err.starts_with("parse error:"), "got: {err}");
    }

    #[test]
    fn case_additional_fields_ignored_by_default() {
        // additional_properties=true → root additionalProperties:false
        // is stripped; envelope can have extra fields the schema
        // didn't declare.
        clear_cache();
        let dir = tempfile::tempdir().unwrap();
        let schema = write_schema(dir.path(), r#"{
            "type":"object",
            "additionalProperties": false,
            "properties":{"a":{"type":"string"}}
        }"#);
        let s = settings_mark(schema.to_str().unwrap(), "v");
        let validator = ensure_compiled(&s).unwrap();
        let v = json!({"a":"hello", "extra":"ignored"});
        assert!(validate_value(&validator, &v).is_ok());
    }

    #[test]
    fn case_additional_properties_false_strict() {
        clear_cache();
        let dir = tempfile::tempdir().unwrap();
        let schema = write_schema(dir.path(), r#"{
            "type":"object",
            "additionalProperties": false,
            "properties":{"a":{"type":"string"}}
        }"#);
        let mut s = settings_mark(schema.to_str().unwrap(), "v");
        s.additional_properties = false;
        let validator = ensure_compiled(&s).unwrap();
        let v = json!({"a":"hello", "extra":"banned"});
        let err = validate_value(&validator, &v).unwrap_err();
        assert!(err.contains("extra") || err.contains("additional"), "got: {err}");
    }

    #[test]
    fn yaml_schema_loaded_via_serde_saphyr() {
        clear_cache();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("schema.yaml");
        std::fs::write(&p, r#"type: object
required: [name]
properties:
  name: { type: string }
"#).unwrap();
        let raw = load_schema_text(&p).unwrap();
        assert_eq!(raw["type"], json!("object"));
        assert_eq!(raw["required"], json!(["name"]));
    }

    // ─── mark_v shape ───────────────────────────────────────────────

    #[test]
    fn mark_v_writes_fields_at_v_root() {
        let mut v = json!({"x":1});
        let s = settings_mark("/tmp/x", "v");
        mark_v(&mut v, &s, true, None);
        assert_eq!(v["_valid"], json!(true));
        assert_eq!(v["_validation_error"], Value::Null);
        // Original v.x preserved.
        assert_eq!(v["x"], json!(1));
    }

    #[test]
    fn mark_v_writes_error_string_when_invalid() {
        let mut v = json!({});
        let s = settings_mark("/tmp/x", "v");
        mark_v(&mut v, &s, false, Some("/document_type: enum — bad"));
        assert_eq!(v["_valid"], json!(false));
        assert_eq!(v["_validation_error"], json!("/document_type: enum — bad"));
    }

    #[test]
    fn mark_v_respects_custom_field_names() {
        let mut v = json!({"name":"a"});
        let mut s = settings_mark("/tmp/x", "v");
        s.mark_field = "ok".into();
        s.error_field = "why".into();
        mark_v(&mut v, &s, true, None);
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["why"], Value::Null);
        assert!(v.get("_valid").is_none());
    }

    #[test]
    fn mark_v_wraps_non_object_input() {
        let mut v = json!("plain string");
        let s = settings_mark("/tmp/x", "v");
        mark_v(&mut v, &s, false, Some("not an object"));
        assert_eq!(v["_value"], json!("plain string"));
        assert_eq!(v["_valid"], json!(false));
        assert_eq!(v["_validation_error"], json!("not an object"));
    }
}
