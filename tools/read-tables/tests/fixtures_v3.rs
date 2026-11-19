//! Fixture-set integration test for the v3 (header-band) matcher.
//!
//! Runs `find_tables` against every per-sheet fixture under
//! `tests/fixtures/mocks/sheets/<workbook>/<sheet>.json` and compares the
//! produced count and structural shape against the committed
//! `tests/fixtures/V3_BASELINE.json`. Any drift -- new tables detected,
//! columns count changing, title text changing -- triggers a failure
//! with a per-fixture diff so the regression is obvious.
//!
//! Refresh the baseline by running
//!   `cargo test -p combycode-dpe-tool-read-tables --test fixtures_v3 -- --ignored update_baseline`
//! and committing the regenerated `V3_BASELINE.json`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use combycode_dpe_tool_read_tables::matcher::find_tables;
use combycode_dpe_tool_read_tables::template::Template;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TableShape {
    title:    Option<String>,
    format:   String,
    col_start: usize,
    col_end:  usize,
    cols:     usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct FixtureRecord {
    fixture: String,
    tables:  Vec<TableShape>,
}

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn baseline_path() -> PathBuf {
    fixtures_root().join("V3_BASELINE.json")
}

fn load_templates() -> Vec<Template> {
    let flat = "header : text+\ndata*  : date any+\ntotal~ : period|total|итого|sparse|sum\n";
    let titled = "title     : merge\nheader    : text+\nsubheader?: text+\ndata*     : date any+\ntotal~    : period|total|итого|sparse|sum\n";
    vec![
        Template::parse("flat", flat).expect("parse flat template"),
        Template::parse("titled", titled).expect("parse titled template"),
    ]
}

fn collect_fixtures() -> Vec<(String, PathBuf)> {
    let root = fixtures_root();
    let mut out = Vec::new();
    let mocks = root.join("mocks/sheets");
    if mocks.is_dir() {
        for xd in walkdir(&mocks) {
            if !xd.is_dir() { continue; }
            for f in walkdir(&xd) {
                if f.extension().and_then(|e| e.to_str()) != Some("json") { continue; }
                if f.file_name().and_then(|s| s.to_str()) == Some("meta.json") { continue; }
                let rel = f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
                out.push((rel, f.clone()));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn walkdir(p: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(p).map(|r| r.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    entries.sort();
    entries
}

fn run_one(path: &Path, templates: &[Template]) -> Vec<TableShape> {
    let raw = fs::read_to_string(path).expect("read sheet fixture");
    let doc: Value = serde_json::from_str(&raw).expect("parse sheet fixture");
    let rows = doc.get("rows").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let tables = find_tables(&rows, templates);
    tables.into_iter().map(|t| TableShape {
        title:     t.title.clone(),
        format:    t.format.clone(),
        col_start: t.col_start,
        col_end:   t.col_end,
        cols:      t.columns.len(),
    }).collect()
}

#[test]
fn fixtures_v3_matches_baseline() {
    let templates = load_templates();
    let fixtures = collect_fixtures();
    let baseline_raw = fs::read_to_string(baseline_path())
        .expect("V3_BASELINE.json missing -- run with `--ignored update_baseline` to generate");
    let baseline: Vec<FixtureRecord> = serde_json::from_str(&baseline_raw)
        .expect("V3_BASELINE.json parse error");
    let baseline_idx: BTreeMap<String, &FixtureRecord> =
        baseline.iter().map(|r| (r.fixture.clone(), r)).collect();

    let mut drifts: Vec<String> = Vec::new();
    let mut unseen_baseline: Vec<String> = baseline.iter().map(|r| r.fixture.clone()).collect();
    for (fx, path) in &fixtures {
        let actual = run_one(path, &templates);
        let expected = baseline_idx.get(fx);
        unseen_baseline.retain(|s| s != fx);
        match expected {
            Some(rec) => {
                if rec.tables != actual {
                    drifts.push(format!(
                        "DRIFT {fx}\n  expected ({} tables): {:#?}\n  actual   ({} tables): {:#?}",
                        rec.tables.len(), rec.tables,
                        actual.len(), actual,
                    ));
                }
            }
            None => {
                drifts.push(format!("NEW FIXTURE not in baseline: {fx}  (actual: {} tables)", actual.len()));
            }
        }
    }
    for missing in &unseen_baseline {
        drifts.push(format!("BASELINE FIXTURE missing from disk: {missing}"));
    }

    assert!(drifts.is_empty(),
        "v3 fixture regression ({} drifts):\n{}",
        drifts.len(),
        drifts.join("\n\n"),
    );
}

/// Regenerate `V3_BASELINE.json` from the current matcher output.
/// Run with: `cargo test --test fixtures_v3 -- --ignored update_baseline`
#[test]
#[ignore]
fn update_baseline() {
    let templates = load_templates();
    let fixtures = collect_fixtures();
    let mut out: Vec<FixtureRecord> = Vec::with_capacity(fixtures.len());
    for (fx, path) in fixtures {
        let tables = run_one(&path, &templates);
        out.push(FixtureRecord { fixture: fx, tables });
    }
    let json = serde_json::to_string_pretty(&out).expect("serialize baseline");
    fs::write(baseline_path(), json).expect("write baseline");
    eprintln!("wrote {} fixtures to {}", out.len(), baseline_path().display());
}
