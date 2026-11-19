//! mongo-find — reads documents from a MongoDB collection and emits them
//! as envelopes.

use combycode_dpe::prelude::*;
use combycode_dpe::dpe_run;
use mongodb::bson::Document;
use mongodb::options::{ClientOptions, FindOptions};
use mongodb::{Client, Collection};
use futures::stream::TryStreamExt;
use tokio::runtime::Runtime;

use combycode_dpe_tool_mongo_find::query::{bson_to_json, filter_once, filter_per_input, projection_doc, sort_doc};
use combycode_dpe_tool_mongo_find::settings::{Mode, ToolSettings};

struct State {
    rt: Runtime,
    collection: Collection<Document>,
    cfg: ToolSettings,
    ran_once: std::sync::atomic::AtomicBool,
}

static STATE: std::sync::OnceLock<Option<State>> = std::sync::OnceLock::new();

fn init_state(settings: &Value) -> Option<&State> {
    STATE.get_or_init(|| {
        let cfg: ToolSettings = match serde_json::from_value(settings.clone()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{{\"type\":\"error\",\"error\":\"invalid settings: {}\"}}", e);
                return None;
            }
        };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
        let client: Client = match rt.block_on(async {
            let opts = ClientOptions::parse(&cfg.uri).await?;
            Client::with_options(opts)
        }) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{{\"type\":\"error\",\"error\":\"mongo connect: {}\"}}", e);
                return None;
            }
        };
        let coll = client.database(&cfg.db).collection::<Document>(&cfg.collection);
        Some(State {
            rt, collection: coll, cfg,
            ran_once: std::sync::atomic::AtomicBool::new(false),
        })
    }).as_ref()
}

fn emit_documents(docs: Vec<Document>, ctx: &mut Context<'_>) {
    for d in docs {
        let v = bson_to_json(d);
        ctx.output(v, None, None);
    }
}

fn process_input(v: Value, settings: &Value, ctx: &mut Context<'_>) {
    let state = match init_state(settings) {
        Some(s) => s,
        None => {
            ctx.error(&v, "mongo-find not initialised");
            return;
        }
    };

    match state.cfg.mode {
        Mode::Once => {
            if state.cfg.passthrough_input {
                ctx.output(v.clone(), None, None);
            }
            // Run the query exactly once across all inputs.
            if !state.ran_once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let filter = filter_once(&state.cfg);
                let docs = run_find(state, filter);
                match docs {
                    Ok(items) => emit_documents(items, ctx),
                    Err(e) => ctx.error(&v, &format!("find: {}", e)),
                }
            }
        }
        Mode::PerInput => {
            if state.cfg.passthrough_input {
                ctx.output(v.clone(), None, None);
            }
            let env = json!({"v": v, "t": "d"});
            let filter = filter_per_input(&env, &state.cfg);
            match run_find(state, filter) {
                Ok(items) => emit_documents(items, ctx),
                Err(e) => ctx.error(&v, &format!("find: {}", e)),
            }
        }
    }
}

fn run_find(state: &State, filter: Document) -> Result<Vec<Document>, String> {
    let opts = FindOptions::builder()
        .sort(sort_doc(&state.cfg))
        .projection(projection_doc(&state.cfg))
        .limit(state.cfg.limit)
        .build();
    state.rt.block_on(async {
        let cursor = state.collection
            .find(filter)
            .with_options(opts)
            .await
            .map_err(|e| format!("find call: {}", e))?;
        cursor.try_collect::<Vec<Document>>().await
            .map_err(|e| format!("cursor collect: {}", e))
    })
}

fn main() {
    dpe_run! {
        input: process_input,
    };
    // On EOF (once-mode without any input), make sure we did the query.
    if let Some(Some(state)) = STATE.get() {
        if state.cfg.mode == Mode::Once
           && !state.ran_once.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let filter = filter_once(&state.cfg);
            if let Ok(docs) = run_find(state, filter) {
                for d in docs {
                    let v = bson_to_json(d);
                    let line = serde_json::to_string(&json!({
                        "t":"d", "id": "", "src":"mongo-find", "v": v
                    })).unwrap();
                    println!("{}", line);
                }
            }
        }
    }
}
