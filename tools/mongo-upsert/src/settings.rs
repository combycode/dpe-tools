//! Settings for mongo-upsert, passed as argv[1] JSON.

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSettings {
    /// Mongo connection URI, e.g. "mongodb://localhost:27017".
    pub uri: String,
    pub db: String,
    pub collection: String,

    /// Filter document. Each value is a dotted path expression resolved per
    /// envelope (e.g. `"v.order_id"`). If the value is a literal non-string or
    /// a string that doesn't look like a dotted path, it's treated as a
    /// constant. Example:
    ///   key: { order_id: "v.order_id", source: "feed" }
    pub key: BTreeMap<String, String>,

    /// Update operations, applied together in one updateOne call.
    pub operations: Vec<Operation>,

    #[serde(default = "default_true")]
    pub upsert: bool,

    /// Batch N envelopes into one flush of concurrent update_one calls. 1 = no batching (one
    /// round-trip per row). Default 500.
    #[serde(default = "default_bulk_size")]
    pub bulk_size: usize,

    /// Error policy when an update fails. Default: trace (log + continue).
    #[serde(default = "default_on_error")]
    pub on_error: OnError,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    /// Mongo operator: "$set", "$addToSet", "$push", "$inc", "$unset".
    pub op: OperatorKind,
    /// Path inside the document, using dot notation (e.g. "stats.daily" or "tags").
    pub path: String,
    /// Path expression to the value (resolved per envelope). Use "v.xxx" or
    /// a literal — for $unset the value is ignored.
    #[serde(default)]
    pub value: Option<String>,
    /// Literal value override. If both `value` and `literal` are set,
    /// `literal` wins. Useful for fixed timestamps or counters ($inc: 1).
    #[serde(default)]
    pub literal: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum OperatorKind {
    #[serde(rename = "$set")]     Set,
    #[serde(rename = "$addToSet")]AddToSet,
    #[serde(rename = "$push")]    Push,
    #[serde(rename = "$inc")]     Inc,
    #[serde(rename = "$unset")]   Unset,
}

impl OperatorKind {
    pub fn as_mongo_key(&self) -> &'static str {
        match self {
            OperatorKind::Set      => "$set",
            OperatorKind::AddToSet => "$addToSet",
            OperatorKind::Push     => "$push",
            OperatorKind::Inc      => "$inc",
            OperatorKind::Unset    => "$unset",
        }
    }
    /// Whether the operator is replay-safe (idempotent) on repeated writes
    /// of the same envelope. $push and $inc are NOT.
    pub fn is_idempotent(&self) -> bool {
        matches!(self, OperatorKind::Set | OperatorKind::AddToSet | OperatorKind::Unset)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// Log trace event + continue to next envelope (default).
    Trace,
    /// Drop envelope silently.
    Drop,
    /// Terminate the tool with error exit.
    Fail,
}

fn default_true() -> bool { true }
fn default_bulk_size() -> usize { 500 }
fn default_on_error() -> OnError { OnError::Trace }

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test] fn parse_minimal() {
        let s: ToolSettings = serde_json::from_value(json!({
            "uri": "mongodb://localhost:27017",
            "db":  "test",
            "collection": "recon",
            "key": {"order_id": "v.order_id"},
            "operations": [
                {"op": "$set", "path": "updated_at", "value": "v.now"}
            ]
        })).unwrap();
        assert_eq!(s.uri, "mongodb://localhost:27017");
        assert_eq!(s.bulk_size, 500);
        assert!(s.upsert);
        assert_eq!(s.on_error, OnError::Trace);
        assert_eq!(s.operations.len(), 1);
        assert_eq!(s.operations[0].op, OperatorKind::Set);
    }

    #[test] fn parse_full() {
        let s: ToolSettings = serde_json::from_value(json!({
            "uri": "mongodb://h/", "db": "d", "collection": "c",
            "key": {"a": "v.a", "b": "v.b"},
            "operations": [
                {"op": "$set",      "path": "k",     "value": "v.x"},
                {"op": "$addToSet", "path": "tags",  "value": "v.tag"},
                {"op": "$push",     "path": "log",   "value": "v.event"},
                {"op": "$inc",      "path": "count", "literal": 1},
                {"op": "$unset",    "path": "stale"}
            ],
            "upsert": false,
            "bulk_size": 1000,
            "on_error": "fail"
        })).unwrap();
        assert!(!s.upsert);
        assert_eq!(s.bulk_size, 1000);
        assert_eq!(s.on_error, OnError::Fail);
        assert_eq!(s.operations.len(), 5);
    }

    #[test] fn rejects_unknown_field() {
        let r: Result<ToolSettings, _> = serde_json::from_value(json!({
            "uri": "x", "db": "d", "collection": "c", "key": {},
            "operations": [], "bogus": true
        }));
        assert!(r.is_err());
    }

    #[test] fn rejects_unknown_operator() {
        let r: Result<Operation, _> = serde_json::from_value(json!({
            "op": "$bogus", "path": "x"
        }));
        assert!(r.is_err());
    }

    #[test] fn operator_keys_map_to_mongo() {
        assert_eq!(OperatorKind::Set.as_mongo_key(), "$set");
        assert_eq!(OperatorKind::AddToSet.as_mongo_key(), "$addToSet");
        assert_eq!(OperatorKind::Push.as_mongo_key(), "$push");
        assert_eq!(OperatorKind::Inc.as_mongo_key(), "$inc");
        assert_eq!(OperatorKind::Unset.as_mongo_key(), "$unset");
    }

    #[test] fn idempotency_classification() {
        assert!(OperatorKind::Set.is_idempotent());
        assert!(OperatorKind::AddToSet.is_idempotent());
        assert!(OperatorKind::Unset.is_idempotent());
        assert!(!OperatorKind::Push.is_idempotent());
        assert!(!OperatorKind::Inc.is_idempotent());
    }

    #[test] fn operation_literal_precedence() {
        // When both value and literal set, literal wins (documented behaviour).
        let op: Operation = serde_json::from_value(json!({
            "op": "$set", "path": "x", "value": "v.maybe", "literal": 42
        })).unwrap();
        assert_eq!(op.literal, Some(json!(42)));
    }
}
