//! Settings for mongo-find.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSettings {
    pub uri: String,
    pub db: String,
    pub collection: String,

    /// Query mode:
    ///   "once"       — run `filter` once at startup, emit all matches, stop.
    ///                  Input envelopes are ignored (they still flush through).
    ///   "per_input"  — run a find per input envelope, using `filter_from`
    ///                  (each field's value is a path resolved on the envelope).
    #[serde(default = "default_mode")]
    pub mode: Mode,

    /// Literal filter document (JSON). Used in mode=once. May be empty for "find all".
    #[serde(default = "default_empty_object")]
    pub filter: serde_json::Value,

    /// Filter template for per_input mode: each key's value is a path expr
    /// resolved per envelope. e.g. `{order_id: "v.order_id"}`.
    #[serde(default)]
    pub filter_from: std::collections::BTreeMap<String, String>,

    /// Optional sort document (JSON): `{field: 1}` or `{field: -1}`.
    #[serde(default)]
    pub sort: Option<serde_json::Value>,

    /// Optional projection document (JSON).
    #[serde(default)]
    pub projection: Option<serde_json::Value>,

    #[serde(default)]
    pub limit: Option<i64>,

    /// When true, the tool emits input envelopes as-is first, then the found
    /// documents. When false (default), only matching documents are emitted.
    #[serde(default)]
    pub passthrough_input: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Once,
    PerInput,
}

fn default_mode() -> Mode { Mode::Once }
fn default_empty_object() -> serde_json::Value { serde_json::json!({}) }

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test] fn parse_mode_once_minimal() {
        let s: ToolSettings = serde_json::from_value(json!({
            "uri": "mongodb://h", "db": "d", "collection": "c"
        })).unwrap();
        assert_eq!(s.mode, Mode::Once);
        assert_eq!(s.filter, json!({}));
        assert!(s.limit.is_none());
        assert!(!s.passthrough_input);
    }

    #[test] fn parse_mode_per_input_full() {
        let s: ToolSettings = serde_json::from_value(json!({
            "uri": "h", "db": "d", "collection": "c",
            "mode": "per_input",
            "filter_from": {"day": "v.day"},
            "sort": {"day": 1},
            "projection": {"_id": 0},
            "limit": 100,
            "passthrough_input": true
        })).unwrap();
        assert_eq!(s.mode, Mode::PerInput);
        assert_eq!(s.filter_from.len(), 1);
        assert!(s.sort.is_some());
        assert_eq!(s.limit, Some(100));
        assert!(s.passthrough_input);
    }

    #[test] fn parse_with_literal_filter() {
        let s: ToolSettings = serde_json::from_value(json!({
            "uri": "h", "db": "d", "collection": "c",
            "filter": {"done": true}
        })).unwrap();
        assert_eq!(s.filter["done"], true);
    }

    #[test] fn reject_unknown() {
        let r: Result<ToolSettings, _> = serde_json::from_value(json!({
            "uri": "h", "db": "d", "collection": "c", "bogus": 1
        }));
        assert!(r.is_err());
    }
}
