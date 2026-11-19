//! mongo-upsert tool — streams envelopes to a MongoDB collection.
//!
//! For each envelope:
//!   - build filter document from settings.key (path expressions → values)
//!   - build update document from settings.operations (mix of operators)
//!   - accumulate into a batch; flush at bulk_size (concurrent update_one calls)
//!
//! On shutdown: flush any remaining batch.
//!
//! Emits each input envelope downstream unchanged so the tool can sit in
//! the middle of a pipeline. Use as a terminal if no downstream consumer.

use combycode_dpe::prelude::*;
use combycode_dpe::dpe_run;
use mongodb::bson::Document;
use mongodb::options::{ClientOptions, UpdateOptions};
use mongodb::{Client, Collection};
use std::sync::Mutex as StdMutex;
use tokio::runtime::Runtime;

use combycode_dpe_tool_mongo_upsert::settings::{OnError, ToolSettings};
use combycode_dpe_tool_mongo_upsert::update::{build_filter, build_update};

struct Batch {
    filters: Vec<Document>,
    updates: Vec<Document>,
}

struct State {
    rt: Runtime,
    collection: Collection<Document>,
    batch: StdMutex<Batch>,
    cfg: ToolSettings,
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
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{{\"type\":\"error\",\"error\":\"tokio init: {}\"}}", e);
                return None;
            }
        };
        let client_res: Result<Client, String> = rt.block_on(async {
            let opts = ClientOptions::parse(&cfg.uri).await
                .map_err(|e| format!("parse uri: {}", e))?;
            Client::with_options(opts).map_err(|e| format!("client new: {}", e))
        });
        let client = match client_res {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{{\"type\":\"error\",\"error\":\"mongo connect: {}\"}}", e);
                return None;
            }
        };
        let coll = client.database(&cfg.db).collection::<Document>(&cfg.collection);
        Some(State {
            rt,
            collection: coll,
            batch: StdMutex::new(Batch { filters: Vec::new(), updates: Vec::new() }),
            cfg,
        })
    }).as_ref()
}

fn process_input(v: Value, settings: &Value, ctx: &mut Context<'_>) {
    let state = match init_state(settings) {
        Some(s) => s,
        None => {
            ctx.error(&v, "mongo-upsert not initialised");
            return;
        }
    };

    let env = json!({"v": v, "t": "d"});
    let filter = build_filter(&env, &state.cfg);
    let update = build_update(&env, &state.cfg);

    // Accumulate + flush when full
    let mut should_flush = false;
    {
        let mut b = state.batch.lock().unwrap();
        b.filters.push(filter);
        b.updates.push(update);
        if b.filters.len() >= state.cfg.bulk_size {
            should_flush = true;
        }
    }
    if should_flush {
        if let Err(e) = flush(state) {
            handle_err(&v, &e, state.cfg.on_error, ctx);
            return;
        }
    }

    // Pass envelope through for downstream pipelines.
    ctx.output(v, None, None);
}

fn flush(state: &State) -> Result<(), String> {
    let (filters, updates) = {
        let mut b = state.batch.lock().unwrap();
        let f = std::mem::take(&mut b.filters);
        let u = std::mem::take(&mut b.updates);
        (f, u)
    };
    if filters.is_empty() { return Ok(()); }
    let upsert = state.cfg.upsert;

    // Sequential update_one loop. MongoDB's client-level unified bulk_write
    // requires 8.0+, which we can't assume. For larger batches we issue
    // update_one calls concurrently via join_all for reasonable throughput
    // even on 7.x.
    state.rt.block_on(async {
        let opts = UpdateOptions::builder().upsert(upsert).build();
        let mut futs = Vec::with_capacity(filters.len());
        for (f, u) in filters.into_iter().zip(updates) {
            let coll = state.collection.clone();
            let opts = opts.clone();
            futs.push(async move {
                coll.update_one(f, u).with_options(opts).await
            });
        }
        // Drive concurrently but sequentially ordered results.
        use futures::stream::{self, StreamExt};
        let mut stream = stream::iter(futs).buffer_unordered(16);
        while let Some(res) = stream.next().await {
            if let Err(e) = res {
                return Err(format!("update_one: {}", e));
            }
        }
        Ok(())
    })
}

fn handle_err(v: &Value, e: &str, policy: OnError, ctx: &mut Context<'_>) {
    match policy {
        OnError::Trace => {
            eprintln!("{{\"type\":\"trace\",\"labels\":{{\"mongo_err\":\"{}\"}}}}",
                e.replace('"', "\\\""));
        }
        OnError::Drop  => { /* silent */ }
        OnError::Fail  => {
            ctx.error(v, e);
            std::process::exit(1);
        }
    }
}

fn main() {
    // Register a best-effort final flush on drop. The runtime drops STATE
    // at process exit; OnceLock<Option<State>> gives us access.
    dpe_run! {
        input: process_input,
    };
    // dpe_run! blocks until stdin EOF + queue drained. At that point we
    // should flush any remaining batch.
    if let Some(Some(state)) = STATE.get() {
        if let Err(e) = flush(state) {
            eprintln!("{{\"type\":\"error\",\"error\":\"final flush: {}\"}}", e);
        }
    }
}
