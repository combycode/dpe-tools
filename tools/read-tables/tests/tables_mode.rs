//! End-to-end tests for `mode: "tables"`. Spawns the built binary,
//! feeds one envelope, asserts the per-table envelope shape AND the
//! on-disk file content for each `extract.format`.

use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

fn bin() -> &'static str { env!("CARGO_BIN_EXE_read-tables") }

struct Fixture {
    sheet_path:  String,
    formats_dir: String,
    extract_dir: std::path::PathBuf,
    _tmp:        tempfile::TempDir,
}

fn make_fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();

    // Templates dir with one minimal `flat` template — header row +
    // data rows.
    let formats_dir = tmp.path().join("formats");
    std::fs::create_dir_all(&formats_dir).unwrap();
    std::fs::write(
        formats_dir.join("flat.tpl"),
        "header : text+\ndata*  : any ...\n",
    ).unwrap();

    // Self-describing sheet with a single 3-column / 3-row data table.
    let sheet = json!({
        "source_file":     "rates.xlsx",
        "source_path":     "/data/in/rates.xlsx",
        "source_relative": "rates.xlsx",
        "sheet_name":      "Q1 Rates",                  // includes a space → exercises sanitization
        "extracted_at":    "2026-05-09T00:00:00Z",
        "rows": [
            ["Region", "Revenue", "Cost"],
            ["EMEA",   100,        50],
            ["APAC",   200,        90],
            ["AMER",   300,        120],
        ],
    });
    let sheet_path = tmp.path().join("sheet.json");
    std::fs::write(&sheet_path, serde_json::to_vec(&sheet).unwrap()).unwrap();

    let extract_dir = tmp.path().join("extracted");

    Fixture {
        sheet_path:  sheet_path.to_string_lossy().replace('\\', "/"),
        formats_dir: formats_dir.to_string_lossy().replace('\\', "/"),
        extract_dir,
        _tmp:        tmp,
    }
}

fn run(settings: Value, envelope: Value) -> (Vec<Value>, String, i32) {
    let stdin_txt = serde_json::to_string(&envelope).unwrap();
    let mut child = Command::new(bin())
        .arg(settings.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn().unwrap();
    child.stdin.as_mut().unwrap().write_all(stdin_txt.as_bytes()).unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let envelopes: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("t").and_then(|t| t.as_str()) == Some("d"))
        .collect();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let code = out.status.code().unwrap_or(-1);
    (envelopes, stderr, code)
}

// ─── csv format ─────────────────────────────────────────────────────

#[test]
fn tables_mode_csv_emits_one_envelope_and_writes_csv_file() {
    let f = make_fixture();
    let settings = json!({
        "formats": f.formats_dir,
        "mode":    "tables",
        "extract": {
            "dir":    f.extract_dir.to_string_lossy().replace('\\', "/"),
            "format": "csv",
        },
    });
    let envelope = json!({
        "t":"d", "id":"i", "src":"s",
        "v": { "path": f.sheet_path },
    });
    let (envs, stderr, code) = run(settings, envelope);
    assert_eq!(code, 0, "rc=0 expected; stderr: {stderr}");
    assert_eq!(envs.len(), 1, "exactly one table envelope; got: {envs:?}");

    let v = envs[0].get("v").unwrap();
    assert_eq!(v.get("sheet").and_then(|s| s.as_str()), Some("Q1 Rates"));
    assert_eq!(v.get("table").and_then(|t| t.as_u64()), Some(0));
    assert_eq!(v.get("range").and_then(|r| r.as_str()), Some("A1:C4"));
    assert_eq!(
        v.get("columns").unwrap(),
        &json!(["Region", "Revenue", "Cost"]),
    );
    // No row / row_idx in tables-mode envelopes.
    assert!(v.get("row").is_none(),     "row field must be absent in tables mode");
    assert!(v.get("row_idx").is_none(), "row_idx must be absent in tables mode");

    // Target file written, content matches RFC-4180 + sanitized sheet name.
    let target = v.get("target").and_then(|t| t.as_str()).unwrap();
    assert!(target.ends_with(".csv"), "target ends in .csv: {target}");
    assert!(target.contains("rates__Q1 Rates__t0.csv"),
        "filename pattern: {target}");
    let body = std::fs::read_to_string(target).unwrap();
    assert_eq!(
        body,
        "Region,Revenue,Cost\r\n\
         EMEA,100,50\r\n\
         APAC,200,90\r\n\
         AMER,300,120\r\n",
    );
}

// ─── ndjson (full) ──────────────────────────────────────────────────

#[test]
fn tables_mode_ndjson_full_lines_match_row_mode_v_shape() {
    let f = make_fixture();
    let settings = json!({
        "formats": f.formats_dir,
        "mode":    "tables",
        "extract": {
            "dir":    f.extract_dir.to_string_lossy().replace('\\', "/"),
            "format": "ndjson",
        },
    });
    let envelope = json!({
        "t":"d", "id":"i", "src":"s",
        "v": { "path": f.sheet_path },
    });
    let (envs, stderr, code) = run(settings, envelope);
    assert_eq!(code, 0, "rc=0 expected; stderr: {stderr}");
    assert_eq!(envs.len(), 1);

    let v = envs[0].get("v").unwrap();
    let target = v.get("target").and_then(|t| t.as_str()).unwrap();
    assert!(target.ends_with(".ndjson"), "ndjson extension: {target}");

    // Each line is a JSON object — same shape as row-mode envelope's `v`.
    let body = std::fs::read_to_string(target).unwrap();
    let lines: Vec<Value> = body.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3, "3 data rows → 3 lines");
    for (i, line) in lines.iter().enumerate() {
        assert_eq!(line.get("sheet").and_then(|s| s.as_str()), Some("Q1 Rates"));
        assert_eq!(line.get("table").and_then(|t| t.as_u64()), Some(0));
        assert_eq!(line.get("format").and_then(|f| f.as_str()), Some("flat"));
        assert_eq!(line.get("columns").unwrap(),
            &json!(["Region", "Revenue", "Cost"]));
        // row_idx is 1-based row number in source sheet — first data
        // row sits at sheet index 1 → row_idx = 2 (header is row 1).
        assert_eq!(
            line.get("row_idx").and_then(|i| i.as_u64()),
            Some((i + 2) as u64),
            "line {i}: row_idx should match source position",
        );
        // Per-line row array has 3 cells.
        let row = line.get("row").and_then(|r| r.as_array()).unwrap();
        assert_eq!(row.len(), 3);
    }
}

// ─── ndjson-compact ─────────────────────────────────────────────────

#[test]
fn tables_mode_ndjson_compact_first_line_is_headers_then_value_arrays() {
    let f = make_fixture();
    let settings = json!({
        "formats": f.formats_dir,
        "mode":    "tables",
        "extract": {
            "dir":    f.extract_dir.to_string_lossy().replace('\\', "/"),
            "format": "ndjson-compact",
        },
    });
    let envelope = json!({
        "t":"d", "id":"i", "src":"s",
        "v": { "path": f.sheet_path },
    });
    let (envs, stderr, code) = run(settings, envelope);
    assert_eq!(code, 0, "rc=0 expected; stderr: {stderr}");
    assert_eq!(envs.len(), 1);

    let target = envs[0].get("v").unwrap()
        .get("target").and_then(|t| t.as_str()).unwrap();
    let body = std::fs::read_to_string(target).unwrap();
    let lines: Vec<Value> = body.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 4, "1 header line + 3 data lines");
    assert_eq!(lines[0], json!(["Region", "Revenue", "Cost"]));
    assert_eq!(lines[1], json!(["EMEA", 100, 50]));
    assert_eq!(lines[2], json!(["APAC", 200, 90]));
    assert_eq!(lines[3], json!(["AMER", 300, 120]));
}

// ─── envelope completeness ──────────────────────────────────────────

#[test]
fn tables_mode_envelope_carries_all_row_mode_fields_plus_target_and_range() {
    let f = make_fixture();
    let settings = json!({
        "formats": f.formats_dir,
        "mode":    "tables",
        "extract": {
            "dir":    f.extract_dir.to_string_lossy().replace('\\', "/"),
            "format": "csv",
        },
    });
    let envelope = json!({
        "t":"d", "id":"i", "src":"s",
        "v": { "path": f.sheet_path },
    });
    let (envs, _stderr, _code) = run(settings, envelope);
    let v = envs[0].get("v").unwrap();

    // Identifying fields preserved 1:1 from row mode:
    for k in ["source_file", "source_path", "source_relative",
              "sheet", "table", "format", "title", "columns",
              "col_start", "col_end", "row_start", "row_end"] {
        assert!(v.get(k).is_some(), "tables-mode envelope MUST carry '{k}'");
    }
    // New fields:
    assert!(v.get("target").is_some(), "target file path");
    assert!(v.get("range").is_some(),  "A1-style range label");
    // Removed fields:
    assert!(v.get("row").is_none(),     "row removed (lives in target file)");
    assert!(v.get("row_idx").is_none(), "row_idx removed (one envelope per table now)");
}

// ─── passthrough_input ──────────────────────────────────────────────

#[test]
fn tables_mode_passthrough_propagates_to_table_envelope() {
    let f = make_fixture();
    let settings = json!({
        "formats":           f.formats_dir,
        "mode":              "tables",
        "passthrough_input": true,
        "extract": {
            "dir":    f.extract_dir.to_string_lossy().replace('\\', "/"),
            "format": "ndjson-compact",
        },
    });
    let envelope = json!({
        "t":"d", "id":"i", "src":"s",
        "v": {
            "path":     f.sheet_path,
            "label":    "ALPHA",
            "batch":    10,
        },
    });
    let (envs, _stderr, _code) = run(settings, envelope);
    assert_eq!(envs.len(), 1);
    let v = envs[0].get("v").unwrap();
    assert_eq!(v.get("label").and_then(|s| s.as_str()), Some("ALPHA"));
    assert_eq!(v.get("batch").and_then(|n| n.as_u64()),    Some(10));
    // Read-tables's own fields still authoritative.
    assert_eq!(v.get("sheet").and_then(|s| s.as_str()),    Some("Q1 Rates"));
}

// ─── error / edge cases ─────────────────────────────────────────────

#[test]
fn tables_mode_missing_extract_block_emits_error_envelope() {
    let f = make_fixture();
    let settings = json!({
        "formats": f.formats_dir,
        "mode":    "tables",
        // no extract: { ... }
    });
    let envelope = json!({
        "t":"d", "id":"i", "src":"s",
        "v": { "path": f.sheet_path },
    });
    let (envs, stderr, _code) = run(settings, envelope);
    // No data envelopes — the error went via ctx.error to stderr.
    assert!(envs.is_empty(),
        "no data envelopes expected on missing extract block; got: {envs:?}");
    assert!(stderr.contains("extract"),
        "stderr should mention the missing extract block: {stderr}");
}

#[test]
fn tables_mode_unknown_format_emits_error_envelope() {
    let f = make_fixture();
    let settings = json!({
        "formats": f.formats_dir,
        "mode":    "tables",
        "extract": {
            "dir":    f.extract_dir.to_string_lossy().replace('\\', "/"),
            "format": "parquet",                     // not supported
        },
    });
    let envelope = json!({
        "t":"d", "id":"i", "src":"s",
        "v": { "path": f.sheet_path },
    });
    let (envs, stderr, _code) = run(settings, envelope);
    assert!(envs.is_empty(),
        "no data envelopes for unknown format; got: {envs:?}");
    assert!(stderr.contains("parquet") || stderr.contains("format"),
        "stderr should explain the bad format: {stderr}");
}
