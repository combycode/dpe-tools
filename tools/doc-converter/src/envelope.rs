//! Output-envelope construction helpers shared by the success + failure paths.
//!
//! Both paths start the output map by spreading the incoming `v` so that
//! upstream-set fields (notably `v.hash` — the source file's blake2b
//! content hash from scan-fs) flow through to downstream stages without
//! requiring them to be re-promoted via the explicit `source_*` mapping.
//!
//! The tool's existing doc-converter-set fields (`path` or the configured
//! field-mode field, `failed`, `cache_hit`, frontmatter promotions,
//! `error_class`, `error_msg`, `error_attempt`) are inserted AFTER the
//! spread, so they override their input-side counterparts.

use serde_json::{Map, Value};

/// Start the output envelope map by cloning every field from the incoming
/// `v` (when v is an object). Non-object v values fall back to an empty
/// map — matches the prior behaviour for any oddly-shaped input.
pub fn envelope_base(v: &Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_v_is_cloned_field_by_field() {
        let v = json!({"hash": "deadbeefdeadbeef", "size": 12345, "extra": "x"});
        let base = envelope_base(&v);
        assert_eq!(base.get("hash"), Some(&json!("deadbeefdeadbeef")));
        assert_eq!(base.get("size"), Some(&json!(12345)));
        assert_eq!(base.get("extra"), Some(&json!("x")));
    }

    #[test]
    fn non_object_v_yields_empty_map() {
        for not_object in [json!(null), json!("string"), json!(42), json!([1, 2])] {
            assert!(envelope_base(&not_object).is_empty());
        }
    }

    #[test]
    fn doc_converter_additions_override_v_keys_after_base() {
        // Simulates the success-envelope pattern: spread v, then override
        // path with the MD output path. Replicates the call site in main.rs.
        let v = json!({"hash": "cafebabe", "path": "/in/source.pdf"});
        let mut out = envelope_base(&v);
        out.insert("path".into(), json!("/tmp/source.md"));
        out.insert("failed".into(), json!(false));
        out.insert("cache_hit".into(), json!(true));
        // v.hash preserved (the key bugfix this module documents).
        assert_eq!(out.get("hash"), Some(&json!("cafebabe")));
        // path overridden to the MD output.
        assert_eq!(out.get("path"), Some(&json!("/tmp/source.md")));
        // failure-envelope additions stamped over.
        assert_eq!(out.get("failed"), Some(&json!(false)));
        assert_eq!(out.get("cache_hit"), Some(&json!(true)));
    }

    #[test]
    fn failure_envelope_keeps_v_hash() {
        // Simulates the failure-envelope pattern: spread v, then stamp
        // failed=true + error_class + error_msg. Replicates emit_failure.
        let v = json!({"hash": "f00f00f00f00", "path": "/in/missing.pdf"});
        let mut out = envelope_base(&v);
        out.insert("failed".into(), json!(true));
        out.insert("error_class".into(), json!("io"));
        out.insert("error_msg".into(), json!("file not found"));
        out.insert("error_attempt".into(), json!(1));
        // v.hash preserved on the failure side too (the spec's other half).
        assert_eq!(out.get("hash"), Some(&json!("f00f00f00f00")));
        assert_eq!(out.get("failed"), Some(&json!(true)));
        assert_eq!(out.get("error_class"), Some(&json!("io")));
    }
}
