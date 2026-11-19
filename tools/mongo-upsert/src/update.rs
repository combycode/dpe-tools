//! Build an updateOne filter + update document from settings + envelope.

use bson::{Bson, Decimal128, Document};
use serde_json::Value;

use crate::path::resolve_owned;
use crate::settings::{Operation, OperatorKind, ToolSettings};

/// Resolve the filter document: each key's string value is either a path
/// expression (starts with "v." / "env.") or a literal string.
pub fn build_filter(env: &Value, settings: &ToolSettings) -> Document {
    let mut doc = Document::new();
    for (k, v_expr) in &settings.key {
        let val = if v_expr.starts_with("v.") || v_expr.starts_with("env.")
                     || v_expr == "v" || v_expr == "env" {
            resolve_owned(env, v_expr)
        } else {
            Value::String(v_expr.clone())
        };
        doc.insert(k, to_bson(&val));
    }
    doc
}

/// Merge operations of the same operator into a single sub-document. Mongo
/// rejects duplicate operator keys at the top level — e.g. `{$set: {..}, $set: {..}}`.
pub fn build_update(env: &Value, settings: &ToolSettings) -> Document {
    let mut out = Document::new();
    for op in &settings.operations {
        let op_key = op.op.as_mongo_key();
        let sub = out.entry(op_key.to_string()).or_insert(Bson::Document(Document::new()));
        let Bson::Document(ref mut inner) = sub else { continue };
        let value = resolve_operation_value(env, op);
        inner.insert(&op.path, to_bson(&value));
    }
    out
}

fn resolve_operation_value(env: &Value, op: &Operation) -> Value {
    if let Some(lit) = &op.literal { return lit.clone(); }
    match (&op.op, &op.value) {
        // $unset: value convention → true/1 (Mongo accepts any; true is idiomatic).
        (OperatorKind::Unset, _) => Value::Bool(true),
        (_, Some(v_expr)) if v_expr.starts_with("v.") || v_expr.starts_with("env.")
                              || v_expr == "v" || v_expr == "env" => {
            resolve_owned(env, v_expr)
        }
        (_, Some(literal_str)) => Value::String(literal_str.clone()),
        (_, None) => Value::Null,
    }
}

/// An integer above i64::MAX (JSON numbers up to u64::MAX) as an exact Decimal128.
fn u64_to_bson(u: u64) -> Bson {
    match u.to_string().parse::<Decimal128>() {
        Ok(d) => Bson::Decimal128(d),
        Err(_) => Bson::Double(u as f64), // unreachable for a u64 digit string
    }
}

pub fn to_bson(v: &Value) -> Bson {
    match v {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() { Bson::Int64(i) }
            // Above i64::MAX: exact Decimal128, never a wrapped negative Int64.
            else if let Some(u) = n.as_u64() { u64_to_bson(u) }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Operation, OperatorKind, ToolSettings};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn settings_with(ops: Vec<Operation>, key: BTreeMap<String, String>) -> ToolSettings {
        ToolSettings {
            uri: String::new(),
            db: String::new(),
            collection: String::new(),
            key,
            operations: ops,
            upsert: true,
            bulk_size: 1,
            on_error: crate::settings::OnError::Fail,
        }
    }

    #[test] fn filter_with_path_values() {
        let mut k = BTreeMap::new();
        k.insert("order_id".into(), "v.order_id".into());
        k.insert("src".into(), "feed".into());  // literal string
        let s = settings_with(vec![], k);
        let env = json!({"v": {"order_id": "ORD-42"}});
        let f = build_filter(&env, &s);
        assert_eq!(f.get_str("order_id").unwrap(), "ORD-42");
        assert_eq!(f.get_str("src").unwrap(), "feed");
    }

    #[test] fn update_single_set() {
        let ops = vec![Operation {
            op: OperatorKind::Set, path: "x".into(),
            value: Some("v.x".into()), literal: None,
        }];
        let s = settings_with(ops, BTreeMap::new());
        let env = json!({"v": {"x": 42}});
        let u = build_update(&env, &s);
        assert_eq!(u.get_document("$set").unwrap().get_i64("x").unwrap(), 42);
    }

    #[test] fn update_merges_same_operator() {
        let ops = vec![
            Operation { op: OperatorKind::Set, path: "a".into(), value: Some("v.a".into()), literal: None },
            Operation { op: OperatorKind::Set, path: "b".into(), value: Some("v.b".into()), literal: None },
        ];
        let s = settings_with(ops, BTreeMap::new());
        let env = json!({"v": {"a": 1, "b": 2}});
        let u = build_update(&env, &s);
        let set = u.get_document("$set").unwrap();
        assert_eq!(set.get_i64("a").unwrap(), 1);
        assert_eq!(set.get_i64("b").unwrap(), 2);
    }

    #[test] fn update_mixed_operators() {
        let ops = vec![
            Operation { op: OperatorKind::Set,      path: "k".into(),    value: Some("v.k".into()),    literal: None },
            Operation { op: OperatorKind::AddToSet, path: "tags".into(), value: Some("v.tag".into()),  literal: None },
            Operation { op: OperatorKind::Push,     path: "log".into(),  value: None,                   literal: Some(json!({"e": 1})) },
            Operation { op: OperatorKind::Inc,      path: "count".into(),value: None,                   literal: Some(json!(1)) },
            Operation { op: OperatorKind::Unset,    path: "stale".into(),value: None,                   literal: None },
        ];
        let s = settings_with(ops, BTreeMap::new());
        let env = json!({"v": {"k": "key1", "tag": "T", "event": "E"}});
        let u = build_update(&env, &s);
        assert!(u.contains_key("$set"));
        assert!(u.contains_key("$addToSet"));
        assert!(u.contains_key("$push"));
        assert!(u.contains_key("$inc"));
        assert!(u.contains_key("$unset"));
        assert_eq!(u.get_document("$inc").unwrap().get_i64("count").unwrap(), 1);
        assert!(u.get_document("$unset").unwrap().get_bool("stale").unwrap());
    }

    #[test] fn literal_wins_over_value() {
        let ops = vec![Operation {
            op: OperatorKind::Set, path: "x".into(),
            value: Some("v.maybe".into()),
            literal: Some(json!("fixed")),
        }];
        let s = settings_with(ops, BTreeMap::new());
        let env = json!({"v": {"maybe": "A"}});
        let u = build_update(&env, &s);
        assert_eq!(u.get_document("$set").unwrap().get_str("x").unwrap(), "fixed");
    }

    #[test] fn missing_path_becomes_null() {
        let ops = vec![Operation {
            op: OperatorKind::Set, path: "x".into(),
            value: Some("v.nope".into()), literal: None,
        }];
        let s = settings_with(ops, BTreeMap::new());
        let env = json!({"v": {"a": 1}});
        let u = build_update(&env, &s);
        let bson_val = u.get_document("$set").unwrap().get("x").unwrap();
        assert!(matches!(bson_val, Bson::Null));
    }

    #[test] fn nested_path_in_mongo() {
        // Dot in path is intentional — mongo treats as nested field.
        let ops = vec![Operation {
            op: OperatorKind::Set, path: "stats.daily".into(),
            value: Some("v.eur".into()), literal: None,
        }];
        let s = settings_with(ops, BTreeMap::new());
        let env = json!({"v": {"eur": {"amount": 28}}});
        let u = build_update(&env, &s);
        let set = u.get_document("$set").unwrap();
        let nested = set.get_document("stats.daily").unwrap();
        assert_eq!(nested.get_i64("amount").unwrap(), 28);
    }

    #[test] fn to_bson_handles_arrays() {
        let b = to_bson(&json!([1, "x", true]));
        if let Bson::Array(a) = b {
            assert_eq!(a.len(), 3);
        } else { panic!(); }
    }

    #[test] fn to_bson_integer_boundaries() {
        use bson::Decimal128;
        let max = i64::MAX;
        assert_eq!(to_bson(&serde_json::json!(max)), Bson::Int64(max));
        assert_eq!(to_bson(&serde_json::json!(i64::MIN)), Bson::Int64(i64::MIN));
        // Above i64::MAX: exact Decimal128, not a wrapped negative Int64.
        let above = (i64::MAX as u64) + 1;
        assert_eq!(to_bson(&serde_json::json!(above)),
                   Bson::Decimal128("9223372036854775808".parse::<Decimal128>().unwrap()));
        assert_eq!(to_bson(&serde_json::json!(u64::MAX)),
                   Bson::Decimal128("18446744073709551615".parse::<Decimal128>().unwrap()));
        assert_eq!(to_bson(&serde_json::json!(1.5)), Bson::Double(1.5));
    }
}
