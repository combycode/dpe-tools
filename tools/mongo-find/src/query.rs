//! Build filter / sort / projection BSON documents from settings + envelope.

use bson::{Bson, Document};
use serde_json::Value;

use crate::path::resolve_owned;
use crate::settings::ToolSettings;

pub fn to_bson(v: &Value) -> Bson {
    match v {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() { Bson::Int64(i) }
            else if let Some(u) = n.as_u64() { Bson::Int64(u as i64) }
            else if let Some(f) = n.as_f64() { Bson::Double(f) }
            else { Bson::Null }
        }
        Value::String(s) => Bson::String(s.clone()),
        Value::Array(items) => Bson::Array(items.iter().map(to_bson).collect()),
        Value::Object(obj) => {
            let mut d = Document::new();
            for (k, v) in obj { d.insert(k, to_bson(v)); }
            Bson::Document(d)
        }
    }
}

pub fn value_to_doc(v: &Value) -> Document {
    if !v.is_object() { return Document::new(); }
    let mut out = Document::new();
    for (k, sub) in v.as_object().unwrap() {
        out.insert(k, to_bson(sub));
    }
    out
}

/// In "once" mode: use literal settings.filter directly.
pub fn filter_once(s: &ToolSettings) -> Document {
    value_to_doc(&s.filter)
}

/// In "per_input" mode: resolve each value in filter_from against the envelope.
pub fn filter_per_input(env: &Value, s: &ToolSettings) -> Document {
    let mut doc = Document::new();
    for (field, expr) in &s.filter_from {
        let val = if expr.starts_with("v.") || expr.starts_with("env.")
                     || expr == "v" || expr == "env" {
            resolve_owned(env, expr)
        } else {
            Value::String(expr.clone())
        };
        doc.insert(field, to_bson(&val));
    }
    doc
}

pub fn sort_doc(s: &ToolSettings) -> Option<Document> {
    s.sort.as_ref().map(value_to_doc)
}

pub fn projection_doc(s: &ToolSettings) -> Option<Document> {
    s.projection.as_ref().map(value_to_doc)
}

/// Convert a BSON document → serde_json::Value for envelope emission.
pub fn bson_to_json(doc: Document) -> Value {
    // Use canonical relaxed JSON so types roundtrip sensibly.
    let bson_val = Bson::Document(doc);
    
    // RelaxedExtJson emits things like {"$oid": "..."} for ObjectId. That's
    // fine — callers can flatten with normalize if they need plain strings.
    bson_val.into_relaxed_extjson()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Mode, ToolSettings};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn base_settings() -> ToolSettings {
        ToolSettings {
            uri: String::new(), db: String::new(), collection: String::new(),
            mode: Mode::Once,
            filter: json!({}),
            filter_from: BTreeMap::new(),
            sort: None, projection: None, limit: None,
            passthrough_input: false,
        }
    }

    #[test] fn filter_once_plain_doc() {
        let mut s = base_settings();
        s.filter = json!({"done": true, "kind": "recon"});
        let d = filter_once(&s);
        assert!(d.get_bool("done").unwrap());
        assert_eq!(d.get_str("kind").unwrap(), "recon");
    }

    #[test] fn filter_once_empty() {
        let s = base_settings();
        let d = filter_once(&s);
        assert!(d.is_empty());
    }

    #[test] fn filter_per_input_resolves_paths() {
        let mut s = base_settings();
        s.filter_from.insert("day".into(), "v.day".into());
        s.filter_from.insert("src".into(), "literal".into());
        let env = json!({"v": {"day": "2025-01-15"}});
        let d = filter_per_input(&env, &s);
        assert_eq!(d.get_str("day").unwrap(), "2025-01-15");
        assert_eq!(d.get_str("src").unwrap(), "literal");
    }

    #[test] fn filter_per_input_missing_path_null() {
        let mut s = base_settings();
        s.filter_from.insert("day".into(), "v.nope".into());
        let env = json!({"v": {}});
        let d = filter_per_input(&env, &s);
        assert!(d.get("day").unwrap() == &Bson::Null);
    }

    #[test] fn sort_and_projection() {
        let mut s = base_settings();
        s.sort = Some(json!({"day": 1}));
        s.projection = Some(json!({"_id": 0, "day": 1}));
        assert!(sort_doc(&s).is_some());
        assert_eq!(sort_doc(&s).unwrap().get_i64("day").unwrap(), 1);
        assert!(projection_doc(&s).is_some());
    }

    #[test] fn sort_absent_none() {
        let s = base_settings();
        assert!(sort_doc(&s).is_none());
        assert!(projection_doc(&s).is_none());
    }

    #[test] fn bson_to_json_roundtrip() {
        let mut d = Document::new();
        d.insert("a", 42i64);
        d.insert("s", "hi");
        let j = bson_to_json(d);
        assert_eq!(j["a"], 42);
        assert_eq!(j["s"], "hi");
    }

    #[test] fn bson_to_json_keeps_field_order() {
        // Emitted envelopes must keep the stored field order (serde_json preserve_order).
        let mut d = Document::new();
        d.insert("zeta", 1i64);
        d.insert("alpha", 2i64);
        d.insert("mid", 3i64);
        let j = bson_to_json(d);
        assert_eq!(serde_json::to_string(&j).unwrap(), r#"{"zeta":1,"alpha":2,"mid":3}"#);
    }
}
