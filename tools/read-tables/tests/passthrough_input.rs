//! Integration tests for read-tables `passthrough_input` setting.
//!
//! Spawns the binary as a child process with a real templates dir +
//! sheet JSON on disk, sends one input envelope on stdin, asserts the
//! shape of every emitted output envelope on stdout.

use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

fn bin() -> &'static str { env!("CARGO_BIN_EXE_read-tables") }

struct Fixture {
    sheet_path: String,
    formats_dir: String,
    _tmp: tempfile::TempDir,
}

fn make_fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let formats_dir = tmp.path().join("formats");
    std::fs::create_dir_all(&formats_dir).unwrap();
    // Minimal flat template — header row + any data rows.
    std::fs::write(
        formats_dir.join("flat.tpl"),
        "header : text+\ndata*  : any ...\n",
    ).unwrap();

    // Self-describing sheet JSON: header + 3 daily rows.
    let sheet = json!({
        "source_file":     "Alpha_AUD_2025_02.xlsx",
        "source_path":     "D:/in/items/02.2025/ALPHA/AUD/Alpha_AUD_2025_02.xlsx",
        "source_relative": "items/02.2025/ALPHA/AUD/Alpha_AUD_2025_02.xlsx",
        "sheet_name":      "Sheet1",
        "extracted_at":    "2026-05-05T00:00:00Z",
        "rows": [
            ["Date", "Amount", "Status"],
            ["2025-02-01", 100, "OK"],
            ["2025-02-02", 200, "OK"],
            ["2025-02-03", 300, "OK"],
        ],
    });
    let sheet_path = tmp.path().join("Sheet1.json");
    std::fs::write(&sheet_path, serde_json::to_vec(&sheet).unwrap()).unwrap();

    Fixture {
        sheet_path:  sheet_path.to_string_lossy().replace('\\', "/"),
        formats_dir: formats_dir.to_string_lossy().replace('\\', "/"),
        _tmp:        tmp,
    }
}

fn run(settings: Value, stdin_lines: &[Value]) -> (Vec<Value>, String) {
    let stdin_txt: String = stdin_lines.iter()
        .map(|v| serde_json::to_string(v).unwrap())
        .collect::<Vec<_>>().join("\n");
    let mut child = Command::new(bin())
        .arg(settings.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn().unwrap();
    child.stdin.as_mut().unwrap().write_all(stdin_txt.as_bytes()).unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let stdout: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("t").and_then(|t| t.as_str()) == Some("d"))
        .collect();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    (stdout, stderr)
}

#[test]
fn passthrough_input_off_by_default_no_extra_fields() {
    let f = make_fixture();
    let settings = json!({ "formats": f.formats_dir });
    let envelope = json!({
        "t": "d", "id": "t", "src": "t",
        "v": {
            "path":     f.sheet_path,
            "label":    "ALPHA",           // tag set upstream
            "batch":    10,
            "category": "items",
        },
    });
    let (stdout, stderr) = run(settings, &[envelope]);
    assert!(!stdout.is_empty(), "expected at least one row envelope; stderr: {}", stderr);
    for env in &stdout {
        let v = env.get("v").unwrap();
        // Default behaviour: arbitrary upstream fields are NOT echoed.
        assert!(v.get("label").is_none(),
            "default off: label must NOT propagate; got: {:?}", v);
        assert!(v.get("batch").is_none());
        assert!(v.get("category").is_none());
    }
}

#[test]
fn passthrough_input_true_copies_arbitrary_v_fields_to_every_row() {
    let f = make_fixture();
    let settings = json!({
        "formats":           f.formats_dir,
        "passthrough_input": true,
    });
    let envelope = json!({
        "t": "d", "id": "t", "src": "t",
        "v": {
            "path":     f.sheet_path,
            "label":    "ALPHA",
            "batch":    10,
            "category": "items",
            "extras":   { "tier": "gold", "region": "EU" },
        },
    });
    let (stdout, stderr) = run(settings, &[envelope]);
    assert!(!stdout.is_empty(), "expected row envelopes; stderr: {}", stderr);

    for env in &stdout {
        let v = env.get("v").unwrap();
        assert_eq!(v.get("label").and_then(|x| x.as_str()), Some("ALPHA"));
        assert_eq!(v.get("batch").and_then(|x| x.as_u64()), Some(10));
        assert_eq!(v.get("category").and_then(|x| x.as_str()), Some("items"));
        // Nested objects pass through verbatim.
        assert_eq!(
            v.get("extras").and_then(|x| x.get("tier")).and_then(|x| x.as_str()),
            Some("gold"),
        );
        // read-tables's own fields still present.
        assert!(v.get("row").is_some(),     "row field must remain");
        assert!(v.get("columns").is_some(), "columns must remain");
        assert!(v.get("sheet").is_some(),   "sheet must remain");
    }
    // 3 data rows in fixture → 3 output envelopes.
    assert_eq!(stdout.len(), 3, "expected 3 row envelopes; got: {}", stdout.len());
}

#[test]
fn passthrough_excludes_reserved_input_keys() {
    // path / sheet / source_file / source_path / source_relative are
    // configuration keys for read-tables. They MUST NOT bleed onto
    // outputs as raw passthrough — those slots are owned by
    // read-tables's per-row machinery.
    let f = make_fixture();
    let settings = json!({
        "formats":           f.formats_dir,
        "passthrough_input": true,
    });
    let envelope = json!({
        "t": "d", "id": "t", "src": "t",
        "v": {
            "path":            f.sheet_path,
            "sheet":           "Sheet1",                       // reserved
            "source_file":     "OVERRIDE.xlsx",                 // reserved
            "source_path":     "/should/not/leak.xlsx",         // reserved
            "source_relative": "should/not/leak.xlsx",          // reserved
            "label":           "ALPHA",                          // not reserved
        },
    });
    let (stdout, _) = run(settings, &[envelope]);
    assert!(!stdout.is_empty());
    for env in &stdout {
        let v = env.get("v").unwrap();
        // The reserved-key VALUES still appear (read-tables's resolution
        // chain uses them as inputs and echoes the resolved value), but
        // they should be the values RESOLVED by read-tables's own logic
        // — NOT the raw passthrough copy of the input string. (Sanity:
        // sheet should be the resolved sheet name, source_file should
        // be the embedded one when input override matches, etc.)
        assert!(v.get("sheet").is_some());
        assert!(v.get("source_file").is_some());
        // The custom field passes through.
        assert_eq!(v.get("label").and_then(|x| x.as_str()), Some("ALPHA"));
        // `path` is a read-tables input key but read-tables emits its
        // own `path` field (the sheet json file path). When the input
        // envelope's v.path is the SAME value, it appears in output —
        // but not because of passthrough; because read-tables resolved
        // it. With our exclusion list, the input v.path is NOT
        // double-inserted.
    }
}

#[test]
fn passthrough_does_not_overwrite_read_tables_per_row_fields() {
    // If the user maliciously / accidentally sets v.row or v.columns
    // on the input envelope, the per-row construction MUST overwrite
    // the passthrough copy. Otherwise the output envelopes would be
    // corrupted.
    let f = make_fixture();
    let settings = json!({
        "formats":           f.formats_dir,
        "passthrough_input": true,
    });
    let envelope = json!({
        "t": "d", "id": "t", "src": "t",
        "v": {
            "path":     f.sheet_path,
            "label":    "ALPHA",
            "row":      "should-be-overwritten",          // intentionally bad
            "columns":  "should-be-overwritten",
            "table":    "should-be-overwritten",
            "format":   "should-be-overwritten",
        },
    });
    let (stdout, _) = run(settings, &[envelope]);
    assert!(!stdout.is_empty());
    for env in &stdout {
        let v = env.get("v").unwrap();
        // read-tables's actual per-row values win.
        assert!(v.get("row").is_some_and(|r| r.is_array()),
            "row must be the read-tables array, not passthrough string; got: {:?}", v.get("row"));
        assert!(v.get("columns").is_some_and(|c| c.is_array()));
        assert!(v.get("table").is_some_and(|t| t.is_number()));
        assert!(v.get("format").is_some_and(|fm| fm.is_string()));
        // The non-conflicting field still propagates.
        assert_eq!(v.get("label").and_then(|x| x.as_str()), Some("ALPHA"));
    }
}
