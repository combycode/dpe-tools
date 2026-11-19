//! Integration tests for mongo-find against a real MongoDB. Skipped (with a note) when no
//! server answers on localhost; with `DPE_MONGO_URL` set an unreachable server fails the test.

use combycode_dpe_tool_mongo_find::query::{filter_once, filter_per_input, sort_doc, projection_doc};
use combycode_dpe_tool_mongo_find::settings::{Mode, ToolSettings};
use mongodb::bson::{doc, Document};
use mongodb::options::FindOptions;
use mongodb::{Client, Collection};
use futures::stream::TryStreamExt;
use serde_json::json;
use std::collections::BTreeMap;

async fn try_client() -> Option<Client> {
    let uri = std::env::var("DPE_MONGO_URL")
        .unwrap_or_else(|_| "mongodb://localhost:27017".to_string());
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

async fn populate(coll: &Collection<Document>, docs: Vec<Document>) {
    if !docs.is_empty() {
        coll.insert_many(docs).await.unwrap();
    }
}

fn base_settings(coll: &str) -> ToolSettings {
    ToolSettings {
        uri: std::env::var("DPE_MONGO_URL").unwrap_or_else(|_| "mongodb://localhost:27017".to_string()),
        db: "dpe_test".into(),
        collection: coll.to_string(),
        mode: Mode::Once,
        filter: json!({}),
        filter_from: BTreeMap::new(),
        sort: None, projection: None, limit: None,
        passthrough_input: false,
    }
}

async fn run_find_with_settings(coll: &Collection<Document>, s: &ToolSettings) -> Vec<Document> {
    let opts = FindOptions::builder()
        .sort(sort_doc(s))
        .projection(projection_doc(s))
        .limit(s.limit)
        .build();
    let filter = if s.mode == Mode::Once { filter_once(s) } else { Document::new() };
    coll.find(filter).with_options(opts).await.unwrap()
        .try_collect::<Vec<Document>>().await.unwrap()
}

#[tokio::test]
async fn find_all_empty_filter() {
    let Some(client) = try_client().await else { return; };
    let name = unique_coll_name("find_all");
    let coll = client.database("dpe_test").collection::<Document>(&name);
    populate(&coll, vec![doc!{"n": 1}, doc!{"n": 2}, doc!{"n": 3}]).await;
    let s = base_settings(&name);
    let docs = run_find_with_settings(&coll, &s).await;
    assert_eq!(docs.len(), 3);
    coll.drop().await.ok();
}

#[tokio::test]
async fn find_with_literal_filter() {
    let Some(client) = try_client().await else { return; };
    let name = unique_coll_name("find_lit");
    let coll = client.database("dpe_test").collection::<Document>(&name);
    populate(&coll, vec![
        doc!{"day": "2025-01-15", "kind": "a"},
        doc!{"day": "2025-01-16", "kind": "a"},
        doc!{"day": "2025-01-15", "kind": "b"},
    ]).await;
    let mut s = base_settings(&name);
    s.filter = json!({"kind": "a"});
    let docs = run_find_with_settings(&coll, &s).await;
    assert_eq!(docs.len(), 2);
    coll.drop().await.ok();
}

#[tokio::test]
async fn find_with_sort_limit() {
    let Some(client) = try_client().await else { return; };
    let name = unique_coll_name("find_sort");
    let coll = client.database("dpe_test").collection::<Document>(&name);
    populate(&coll, vec![
        doc!{"n": 3}, doc!{"n": 1}, doc!{"n": 2}, doc!{"n": 4},
    ]).await;
    let mut s = base_settings(&name);
    s.sort = Some(json!({"n": 1}));
    s.limit = Some(2);
    let docs = run_find_with_settings(&coll, &s).await;
    assert_eq!(docs.len(), 2);
    assert_eq!(docs[0].get_i32("n").unwrap(), 1);
    assert_eq!(docs[1].get_i32("n").unwrap(), 2);
    coll.drop().await.ok();
}

#[tokio::test]
async fn find_with_projection() {
    let Some(client) = try_client().await else { return; };
    let name = unique_coll_name("find_proj");
    let coll = client.database("dpe_test").collection::<Document>(&name);
    populate(&coll, vec![doc!{"a": 1, "b": 2, "c": 3}]).await;
    let mut s = base_settings(&name);
    s.projection = Some(json!({"_id": 0, "b": 1}));
    let docs = run_find_with_settings(&coll, &s).await;
    assert_eq!(docs.len(), 1);
    assert!(!docs[0].contains_key("a"));
    assert!(!docs[0].contains_key("c"));
    assert_eq!(docs[0].get_i32("b").unwrap(), 2);
    coll.drop().await.ok();
}

#[tokio::test]
async fn per_input_resolves_filter() {
    let Some(client) = try_client().await else { return; };
    let name = unique_coll_name("per_inp");
    let coll = client.database("dpe_test").collection::<Document>(&name);
    populate(&coll, vec![
        doc!{"day": "A", "val": 1},
        doc!{"day": "B", "val": 2},
    ]).await;
    let mut s = base_settings(&name);
    s.mode = Mode::PerInput;
    s.filter_from.insert("day".into(), "v.day".into());
    let env = json!({"v": {"day": "A"}});
    let filter = filter_per_input(&env, &s);
    let docs = coll.find(filter).await.unwrap()
        .try_collect::<Vec<Document>>().await.unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].get_i32("val").unwrap(), 1);
    coll.drop().await.ok();
}

#[tokio::test]
async fn per_input_missing_path_matches_null() {
    let Some(client) = try_client().await else { return; };
    let name = unique_coll_name("per_null");
    let coll = client.database("dpe_test").collection::<Document>(&name);
    populate(&coll, vec![doc!{"day": "A"}, doc!{"day": mongodb::bson::Bson::Null}]).await;
    let mut s = base_settings(&name);
    s.mode = Mode::PerInput;
    s.filter_from.insert("day".into(), "v.missing".into());
    let env = json!({"v": {}});
    let filter = filter_per_input(&env, &s);
    let docs = coll.find(filter).await.unwrap()
        .try_collect::<Vec<Document>>().await.unwrap();
    // Both the explicit-null doc and the missing-field doc match {day: null}
    assert!(!docs.is_empty());
    coll.drop().await.ok();
}

#[tokio::test]
async fn find_complex_filter_with_operators() {
    let Some(client) = try_client().await else { return; };
    let name = unique_coll_name("ops");
    let coll = client.database("dpe_test").collection::<Document>(&name);
    populate(&coll, vec![
        doc!{"n": 1}, doc!{"n": 5}, doc!{"n": 10},
    ]).await;
    let mut s = base_settings(&name);
    s.filter = json!({"n": {"$gte": 5}});
    let docs = run_find_with_settings(&coll, &s).await;
    assert_eq!(docs.len(), 2);
    coll.drop().await.ok();
}

#[tokio::test]
async fn find_returns_nothing_when_empty() {
    let Some(client) = try_client().await else { return; };
    let name = unique_coll_name("empty");
    let coll = client.database("dpe_test").collection::<Document>(&name);
    // No inserts
    let s = base_settings(&name);
    let docs = run_find_with_settings(&coll, &s).await;
    assert!(docs.is_empty());
    coll.drop().await.ok();
}
