//! Integration tests against a real MongoDB. Skipped (with a note) when no server
//! answers on localhost; with `DPE_MONGO_URL` set an unreachable server fails the test.

use combycode_dpe_tool_mongo_upsert::settings::{OnError, Operation, OperatorKind, ToolSettings};
use combycode_dpe_tool_mongo_upsert::update::{build_filter, build_update};
use mongodb::bson::{doc, Document};
use mongodb::options::UpdateOptions;
use mongodb::{Client, Collection};
use serde_json::json;
use std::collections::BTreeMap;

async fn try_client() -> Option<Client> {
    let uri = std::env::var("DPE_MONGO_URL")
        .unwrap_or_else(|_| "mongodb://localhost:27017".to_string());
    // Quick smoke ping — if no server, bail gracefully
    let ping = async {
        let opts = mongodb::options::ClientOptions::parse(&uri).await?;
        let client = Client::with_options(opts)?;
        client.database("admin").run_command(doc! { "ping": 1 }).await?;
        Ok::<_, mongodb::error::Error>(client)
    }.await;
    match ping {
        Ok(client) => Some(client),
        // An explicit DPE_MONGO_URL means the caller expects a server: fail, never skip silently.
        Err(e) if std::env::var_os("DPE_MONGO_URL").is_some() => panic!("DPE_MONGO_URL is set but unusable: {e}"),
        Err(_) => {
            eprintln!("skipping: no MongoDB at {uri} (set DPE_MONGO_URL to require one)");
            None
        }
    }
}

fn unique_coll_name(prefix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{}_{}", prefix, t)
}

fn settings_for(coll_name: &str, ops: Vec<Operation>, key: BTreeMap<String, String>) -> ToolSettings {
    ToolSettings {
        uri: std::env::var("DPE_MONGO_URL").unwrap_or_else(|_| "mongodb://localhost:27017".to_string()),
        db: "dpe_test".into(),
        collection: coll_name.to_string(),
        key,
        operations: ops,
        upsert: true,
        bulk_size: 1,
        on_error: OnError::Fail,
    }
}

async fn apply_envelope(coll: &Collection<Document>, env: &serde_json::Value, s: &ToolSettings) {
    let filter = build_filter(env, s);
    let update = build_update(env, s);
    let opts = UpdateOptions::builder().upsert(s.upsert).build();
    coll.update_one(filter, update).with_options(opts).await.unwrap();
}

#[tokio::test]
async fn set_upsert_creates_document() {
    let Some(client) = try_client().await else {
        eprintln!("mongo not available — skipping");
        return;
    };
    let coll_name = unique_coll_name("recon");
    let coll = client.database("dpe_test").collection::<Document>(&coll_name);

    let mut key = BTreeMap::new();
    key.insert("order_id".into(), "v.order_id".into());
    let ops = vec![Operation {
        op: OperatorKind::Set, path: "amount".into(),
        value: Some("v.amount".into()), literal: None,
    }];
    let s = settings_for(&coll_name, ops, key);

    let env = json!({"v": {"order_id": "ORD-1", "amount": 100}});
    apply_envelope(&coll, &env, &s).await;

    let found = coll.find_one(doc! {"order_id": "ORD-1"}).await.unwrap().unwrap();
    assert_eq!(found.get_i64("amount").unwrap(), 100);

    // cleanup
    coll.drop().await.ok();
}

#[tokio::test]
async fn set_is_idempotent_on_replay() {
    let Some(client) = try_client().await else { return; };
    let coll_name = unique_coll_name("idem");
    let coll = client.database("dpe_test").collection::<Document>(&coll_name);

    let mut key = BTreeMap::new();
    key.insert("id".into(), "v.id".into());
    let ops = vec![Operation {
        op: OperatorKind::Set, path: "val".into(),
        value: Some("v.val".into()), literal: None,
    }];
    let s = settings_for(&coll_name, ops, key);
    let env = json!({"v": {"id": "X", "val": 42}});

    apply_envelope(&coll, &env, &s).await;
    apply_envelope(&coll, &env, &s).await; // replay

    let count = coll.count_documents(doc! {"id": "X"}).await.unwrap();
    assert_eq!(count, 1);
    let found = coll.find_one(doc! {"id": "X"}).await.unwrap().unwrap();
    assert_eq!(found.get_i64("val").unwrap(), 42);

    coll.drop().await.ok();
}

#[tokio::test]
async fn addtoset_merges_tags_across_calls() {
    let Some(client) = try_client().await else { return; };
    let coll_name = unique_coll_name("tags");
    let coll = client.database("dpe_test").collection::<Document>(&coll_name);

    let mut key = BTreeMap::new();
    key.insert("id".into(), "v.id".into());
    let ops = vec![Operation {
        op: OperatorKind::AddToSet, path: "tags".into(),
        value: Some("v.tag".into()), literal: None,
    }];
    let s = settings_for(&coll_name, ops, key);

    apply_envelope(&coll, &json!({"v": {"id": "A", "tag": "alpha"}}), &s).await;
    apply_envelope(&coll, &json!({"v": {"id": "A", "tag": "beta"}}), &s).await;
    apply_envelope(&coll, &json!({"v": {"id": "A", "tag": "alpha"}}), &s).await; // dup

    let found = coll.find_one(doc! {"id": "A"}).await.unwrap().unwrap();
    let tags = found.get_array("tags").unwrap();
    assert_eq!(tags.len(), 2);
    let as_strs: Vec<&str> = tags.iter().filter_map(|b| b.as_str()).collect();
    assert!(as_strs.contains(&"alpha") && as_strs.contains(&"beta"));

    coll.drop().await.ok();
}

#[tokio::test]
async fn nested_set_merges_two_sources_into_one_doc() {
    let Some(client) = try_client().await else { return; };
    let coll_name = unique_coll_name("nested");
    let coll = client.database("dpe_test").collection::<Document>(&coll_name);

    let mut key = BTreeMap::new();
    key.insert("day".into(), "v.day".into());

    // First call: set by_source.north
    let ops_north = vec![Operation {
        op: OperatorKind::Set, path: "by_source.north".into(),
        value: Some("v.row".into()), literal: None,
    }];
    let s_north = settings_for(&coll_name, ops_north, key.clone());
    apply_envelope(&coll, &json!({"v": {"day": "2025-01-15", "row": {"amount": 100, "source": "north"}}}), &s_north).await;

    // Second call: set by_source.south
    let ops_south = vec![Operation {
        op: OperatorKind::Set, path: "by_source.south".into(),
        value: Some("v.row".into()), literal: None,
    }];
    let s_south = settings_for(&coll_name, ops_south, key);
    apply_envelope(&coll, &json!({"v": {"day": "2025-01-15", "row": {"amount": 28, "source": "south"}}}), &s_south).await;

    let found = coll.find_one(doc! {"day": "2025-01-15"}).await.unwrap().unwrap();
    let by_source = found.get_document("by_source").unwrap();
    let north_amt = by_source.get_document("north").unwrap().get_i64("amount").unwrap();
    let south_amt = by_source.get_document("south").unwrap().get_i64("amount").unwrap();
    assert_eq!(north_amt, 100);
    assert_eq!(south_amt, 28);

    coll.drop().await.ok();
}

#[tokio::test]
async fn mixed_operators_one_shot() {
    let Some(client) = try_client().await else { return; };
    let coll_name = unique_coll_name("mix");
    let coll = client.database("dpe_test").collection::<Document>(&coll_name);

    let mut key = BTreeMap::new();
    key.insert("id".into(), "v.id".into());
    let ops = vec![
        Operation { op: OperatorKind::Set,      path: "last_tag".into(), value: Some("v.tag".into()), literal: None },
        Operation { op: OperatorKind::AddToSet, path: "tags".into(),     value: Some("v.tag".into()), literal: None },
        Operation { op: OperatorKind::Inc,      path: "count".into(),    value: None,                 literal: Some(json!(1)) },
    ];
    let s = settings_for(&coll_name, ops, key);

    apply_envelope(&coll, &json!({"v": {"id": "X", "tag": "a"}}), &s).await;
    apply_envelope(&coll, &json!({"v": {"id": "X", "tag": "b"}}), &s).await;

    let found = coll.find_one(doc! {"id": "X"}).await.unwrap().unwrap();
    assert_eq!(found.get_str("last_tag").unwrap(), "b");
    assert_eq!(found.get_i64("count").unwrap(), 2);
    assert_eq!(found.get_array("tags").unwrap().len(), 2);

    coll.drop().await.ok();
}

#[tokio::test]
async fn missing_value_path_null() {
    let Some(client) = try_client().await else { return; };
    let coll_name = unique_coll_name("null");
    let coll = client.database("dpe_test").collection::<Document>(&coll_name);

    let mut key = BTreeMap::new();
    key.insert("id".into(), "v.id".into());
    let ops = vec![Operation {
        op: OperatorKind::Set, path: "maybe".into(),
        value: Some("v.nope".into()), literal: None,
    }];
    let s = settings_for(&coll_name, ops, key);

    apply_envelope(&coll, &json!({"v": {"id": "Z"}}), &s).await;
    let found = coll.find_one(doc! {"id": "Z"}).await.unwrap().unwrap();
    assert!(found.get("maybe").unwrap() == &mongodb::bson::Bson::Null);

    coll.drop().await.ok();
}
