//! Tables-mode helpers: A1 range labels, filename builder, on-disk
//! writers for the three configured output formats.
//!
//! See `mode_tables` in `main.rs` for the orchestration. The shapes
//! here are deliberately small + pure so they can be unit-tested
//! without an LLM, file-system fixture, or matcher dependency.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Output file format selected via `extract.format` in settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// RFC-4180 CSV with header row.
    Csv,
    /// One JSON object per line. Each object mirrors the shape that
    /// `mode_parse` emits as the envelope's `v` — all metadata
    /// duplicated per row. "100% same data, just another form factor."
    NdjsonFull,
    /// First line is the header array; subsequent lines are value
    /// arrays. Compact when the consumer doesn't need per-row
    /// metadata.
    NdjsonCompact,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "csv"             => Some(Format::Csv),
            "ndjson"          => Some(Format::NdjsonFull),
            "ndjson-compact"  => Some(Format::NdjsonCompact),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::NdjsonFull | Format::NdjsonCompact => "ndjson",
        }
    }
}

/// Encode a table's cell range as an A1-style label, e.g.
/// `(col=0,row_start=1,col=2,row_end=3) → "A1:C3"`.
///
/// Inputs use the same conventions the existing parse-mode envelopes do:
///   - `col_start` / `col_end`: 0-based column indices, inclusive.
///   - `row_start` / `row_end`: 1-based row numbers, inclusive (NB: the
///     `header_start + 1` / `last_data_row + 1` shape from `mode_parse`).
pub fn a1_range(col_start: usize, col_end: usize, row_start: usize, row_end: usize) -> String {
    format!(
        "{}{}:{}{}",
        col_to_letters(col_start),
        row_start,
        col_to_letters(col_end),
        row_end,
    )
}

/// 0-based column index → spreadsheet column label. Tests:
/// `0 → "A", 25 → "Z", 26 → "AA", 27 → "AB", 51 → "AZ", 52 → "BA",
/// 701 → "ZZ", 702 → "AAA"`.
pub fn col_to_letters(mut col: usize) -> String {
    let mut buf = Vec::with_capacity(3);
    loop {
        buf.push((b'A' + (col % 26) as u8) as char);
        if col < 26 { break; }
        col = col / 26 - 1;
    }
    buf.iter().rev().collect()
}

/// Replace anything that's unsafe in a cross-platform filename with
/// `_`. Specifically: path separators `/ \`, the Windows-reserved
/// chars `: * ? " < > |`, and any control chars (0x00-0x1F, 0x7F).
/// Truncates the result at `max_len` bytes (UTF-8-safe — never cuts
/// mid-codepoint).
pub fn sanitize_filename_component(s: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let unsafe_ = matches!(c,
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
        ) || (c as u32) < 0x20 || c == '\u{7F}';
        out.push(if unsafe_ { '_' } else { c });
    }
    // Trim trailing dots / spaces — Windows refuses files that end
    // with these. Leading dots are fine (hidden-file convention).
    while matches!(out.chars().last(), Some('.') | Some(' ')) {
        out.pop();
    }
    if out.is_empty() {
        return "_".to_string();
    }
    if out.len() <= max_len {
        return out;
    }
    // Truncate to max_len bytes without cutting a UTF-8 codepoint.
    let mut end = max_len;
    while end > 0 && !out.is_char_boundary(end) {
        end -= 1;
    }
    out[..end].to_string()
}

/// Compose the on-disk path for an extracted table:
/// `<dir>/<source_stem>__<sanitized_sheet>__t<table_idx>.<ext>`.
///
/// `source_stem` is the basename without extension of the input file.
/// `None` (no source path known) → `"unknown"`. Each component is
/// sanitized + length-capped to keep the final filename comfortably
/// under typical FS limits (255 bytes on most systems).
pub fn build_extract_path(
    dir: &Path,
    source_stem: Option<&str>,
    sheet: &str,
    table_idx: usize,
    format: Format,
) -> PathBuf {
    let stem  = sanitize_filename_component(source_stem.unwrap_or("unknown"), 80);
    let sheet = sanitize_filename_component(sheet, 80);
    let name  = format!("{}__{}__t{}.{}", stem, sheet, table_idx, format.extension());
    dir.join(name)
}

/// Pull the basename stem from an input path. `"/in/foo.xlsx" → "foo"`,
/// `"/in/foo" → "foo"`, `"foo.tar.gz" → "foo.tar"` (drops only the
/// last extension — matches `Path::file_stem`'s convention).
pub fn source_stem_from_path(path: &str) -> Option<String> {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

// ─── Writers ─────────────────────────────────────────────────────────

/// Write CSV (RFC 4180): header row, then one row per data line.
/// Numeric and boolean values land as their literal JSON
/// representation (`100`, `true`); strings are quoted only when they
/// contain `,`, `"`, or a newline (then `"` doubles to `""`); null
/// becomes the empty cell. Object/array cells are JSON-stringified
/// then quoted as if they were strings — preserves merged-cell shapes
/// without losing data.
pub fn write_csv(path: &Path, columns: &[String], rows: &[Vec<Value>]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let f = std::fs::File::create(path)?;
    let mut w = std::io::BufWriter::new(f);
    write_csv_row(&mut w, columns.iter().map(|s| Value::String(s.clone())))?;
    for row in rows {
        write_csv_row(&mut w, row.iter().cloned())?;
    }
    w.flush()
}

fn write_csv_row<W: Write, I: IntoIterator<Item = Value>>(w: &mut W, cells: I) -> std::io::Result<()> {
    let mut first = true;
    for cell in cells {
        if !first { w.write_all(b",")?; }
        first = false;
        let s = csv_field_string(&cell);
        if needs_csv_quoting(&s) {
            w.write_all(b"\"")?;
            for ch in s.chars() {
                if ch == '"' {
                    w.write_all(b"\"\"")?;
                } else {
                    write!(w, "{}", ch)?;
                }
            }
            w.write_all(b"\"")?;
        } else {
            w.write_all(s.as_bytes())?;
        }
    }
    w.write_all(b"\r\n")?;       // RFC 4180 mandates CRLF
    Ok(())
}

fn csv_field_string(v: &Value) -> String {
    match v {
        Value::Null              => String::new(),
        Value::Bool(b)           => b.to_string(),
        Value::Number(n)         => n.to_string(),
        Value::String(s)         => s.clone(),
        Value::Array(_) | Value::Object(_) => v.to_string(),  // JSON-stringify
    }
}

fn needs_csv_quoting(s: &str) -> bool {
    s.chars().any(|c| c == ',' || c == '"' || c == '\n' || c == '\r')
}

/// NDJSON-full: each line is one row of the table as a JSON object,
/// shaped identically to the `v` field that `mode_parse` emits as a
/// per-row envelope. Builder for the shared shape lives in main.rs
/// (`build_row_v_object`); we accept the pre-built objects here so
/// the writer stays I/O-only and serialisable.
pub fn write_ndjson_full(path: &Path, rows: &[Value]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let f = std::fs::File::create(path)?;
    let mut w = std::io::BufWriter::new(f);
    for row in rows {
        serde_json::to_writer(&mut w, row)?;
        w.write_all(b"\n")?;
    }
    w.flush()
}

/// NDJSON-compact: first line is the column-name array; subsequent
/// lines are value arrays in column order.
pub fn write_ndjson_compact(
    path: &Path,
    columns: &[String],
    rows: &[Vec<Value>],
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let f = std::fs::File::create(path)?;
    let mut w = std::io::BufWriter::new(f);
    let header_arr: Vec<Value> = columns.iter().map(|c| Value::String(c.clone())).collect();
    serde_json::to_writer(&mut w, &Value::Array(header_arr))?;
    w.write_all(b"\n")?;
    for row in rows {
        serde_json::to_writer(&mut w, &Value::Array(row.clone()))?;
        w.write_all(b"\n")?;
    }
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    // ─── A1 range ─────────────────────────────────────────────────────

    #[test]
    fn col_to_letters_basics() {
        assert_eq!(col_to_letters(0),  "A");
        assert_eq!(col_to_letters(25), "Z");
        assert_eq!(col_to_letters(26), "AA");
        assert_eq!(col_to_letters(27), "AB");
        assert_eq!(col_to_letters(51), "AZ");
        assert_eq!(col_to_letters(52), "BA");
        assert_eq!(col_to_letters(701), "ZZ");
        assert_eq!(col_to_letters(702), "AAA");
    }

    #[test]
    fn a1_range_basic() {
        assert_eq!(a1_range(0, 2, 1, 3),   "A1:C3");
        assert_eq!(a1_range(0, 0, 5, 5),   "A5:A5");
        assert_eq!(a1_range(25, 27, 1, 1), "Z1:AB1");
    }

    // ─── Filename sanitization ────────────────────────────────────────

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize_filename_component("Q1/Q2", 80),       "Q1_Q2");
        assert_eq!(sanitize_filename_component("a:b*c?d", 80),     "a_b_c_d");
        assert_eq!(sanitize_filename_component("safe-name_42", 80), "safe-name_42");
    }

    #[test]
    fn sanitize_strips_trailing_dots_and_spaces() {
        // Windows refuses files ending in `.` or ` `.
        assert_eq!(sanitize_filename_component("foo. ", 80), "foo");
        assert_eq!(sanitize_filename_component("foo.", 80),  "foo");
        assert_eq!(sanitize_filename_component("foo ", 80),  "foo");
    }

    #[test]
    fn sanitize_handles_empty_after_strip() {
        assert_eq!(sanitize_filename_component("", 80),    "_");
        assert_eq!(sanitize_filename_component("...", 80), "_");
    }

    #[test]
    fn sanitize_truncates_at_max_len() {
        let s = "a".repeat(200);
        let out = sanitize_filename_component(&s, 80);
        assert_eq!(out.len(), 80);
    }

    #[test]
    fn sanitize_truncate_respects_utf8_boundaries() {
        // Each emoji is 4 bytes; truncating to 5 must NOT split a
        // codepoint.
        let s = "😀😀😀";
        let out = sanitize_filename_component(s, 5);
        // Should land at 4 bytes (one emoji), not 5 (mid-emoji).
        assert!(out.len() <= 5);
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn sanitize_replaces_control_chars() {
        let s = "a\x01b\x7Fc";
        assert_eq!(sanitize_filename_component(s, 80), "a_b_c");
    }

    // ─── build_extract_path ───────────────────────────────────────────

    #[test]
    fn build_path_assembles_components() {
        let p = build_extract_path(
            Path::new("/out"), Some("data"), "Sheet1", 0, Format::NdjsonFull,
        );
        assert!(p.ends_with("data__Sheet1__t0.ndjson"));
    }

    #[test]
    fn build_path_csv_extension() {
        let p = build_extract_path(
            Path::new("/out"), Some("data"), "Sheet1", 0, Format::Csv,
        );
        // Path::ends_with matches whole components, not suffixes —
        // assert against the stringified form.
        assert!(p.to_string_lossy().ends_with(".csv"),
            "expected .csv suffix on {:?}", p);
    }

    #[test]
    fn build_path_no_source_stem_falls_back_to_unknown() {
        let p = build_extract_path(
            Path::new("/out"), None, "Sheet1", 0, Format::NdjsonFull,
        );
        assert!(p.to_string_lossy().contains("unknown__Sheet1__t0"));
    }

    #[test]
    fn build_path_sanitizes_sheet_name() {
        // Sheet name with `/` must NOT escape the dir.
        let p = build_extract_path(
            Path::new("/out"), Some("data"), "Q1/Q2*?", 0, Format::Csv,
        );
        let s = p.to_string_lossy().replace('\\', "/");
        assert!(s.contains("Q1_Q2__"));   // /, *, ? all replaced
        assert!(!s.contains("Q1/Q2"));    // raw / does not survive
    }

    #[test]
    fn source_stem_extracts_basename_without_extension() {
        assert_eq!(source_stem_from_path("/in/foo.xlsx").as_deref(), Some("foo"));
        assert_eq!(source_stem_from_path("foo.xlsx").as_deref(),     Some("foo"));
        assert_eq!(source_stem_from_path("/in/foo").as_deref(),       Some("foo"));
        assert_eq!(source_stem_from_path("/in/foo.tar.gz").as_deref(), Some("foo.tar"));
        assert_eq!(source_stem_from_path("").as_deref(),               None);
    }

    // ─── Format parse ─────────────────────────────────────────────────

    #[test]
    fn format_parse_known_values() {
        assert_eq!(Format::parse("csv"),              Some(Format::Csv));
        assert_eq!(Format::parse("ndjson"),           Some(Format::NdjsonFull));
        assert_eq!(Format::parse("ndjson-compact"),   Some(Format::NdjsonCompact));
        assert_eq!(Format::parse("CSV"),              None);     // case-sensitive
        assert_eq!(Format::parse("parquet"),          None);
    }

    // ─── CSV writer ───────────────────────────────────────────────────

    #[test]
    fn csv_basic_round_trip() {
        let d = tempdir().unwrap();
        let p = d.path().join("t.csv");
        let cols = vec!["A".into(), "B".into(), "C".into()];
        let rows = vec![
            vec![json!("x"),   json!(1),  json!(true)],
            vec![json!("y,z"), json!(2),  Value::Null],
        ];
        write_csv(&p, &cols, &rows).unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        // RFC 4180: CRLF terminators.
        assert_eq!(body, "A,B,C\r\nx,1,true\r\n\"y,z\",2,\r\n");
    }

    #[test]
    fn csv_quotes_double_inside_strings() {
        let d = tempdir().unwrap();
        let p = d.path().join("t.csv");
        let rows = vec![vec![json!("she said \"hi\""), json!("ok")]];
        write_csv(&p, &["a".into(), "b".into()], &rows).unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("\"she said \"\"hi\"\"\","));
    }

    #[test]
    fn csv_quotes_strings_with_newlines() {
        let d = tempdir().unwrap();
        let p = d.path().join("t.csv");
        let rows = vec![vec![json!("line1\nline2")]];
        write_csv(&p, &["a".into()], &rows).unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("\"line1\nline2\""));
    }

    #[test]
    fn csv_handles_object_cells_via_json_stringify() {
        let d = tempdir().unwrap();
        let p = d.path().join("t.csv");
        let cells = vec![json!({"v": 1, "span": [1, 2]})];
        let rows = vec![cells];
        write_csv(&p, &["meta".into()], &rows).unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        // Object stringified, then CSV-quoted because it contains `,`.
        assert!(body.contains("\"{"));
    }

    #[test]
    fn csv_creates_parent_dirs() {
        let d = tempdir().unwrap();
        let p = d.path().join("nested/deeper/t.csv");
        write_csv(&p, &["a".into()], &[vec![json!(1)]]).unwrap();
        assert!(p.is_file());
    }

    // ─── NDJSON writers ───────────────────────────────────────────────

    #[test]
    fn ndjson_full_one_object_per_line() {
        let d = tempdir().unwrap();
        let p = d.path().join("t.ndjson");
        let rows = vec![
            json!({"sheet":"S","row":[1,2,3]}),
            json!({"sheet":"S","row":[4,5,6]}),
        ];
        write_ndjson_full(&p, &rows).unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<_> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        // serde_json without `preserve_order` emits maps alphabetically;
        // assert structurally (parse + compare values) rather than
        // matching literal byte-order.
        let l0: Value = serde_json::from_str(lines[0]).unwrap();
        let l1: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(l0, json!({"sheet":"S","row":[1,2,3]}));
        assert_eq!(l1, json!({"sheet":"S","row":[4,5,6]}));
    }

    #[test]
    fn ndjson_compact_header_then_value_arrays() {
        let d = tempdir().unwrap();
        let p = d.path().join("t.ndjson");
        let cols = vec!["A".into(), "B".into()];
        let rows = vec![
            vec![json!("x"),   json!(1)],
            vec![json!("y,z"), json!(true)],
        ];
        write_ndjson_compact(&p, &cols, &rows).unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<_> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], r#"["A","B"]"#);
        assert_eq!(lines[1], r#"["x",1]"#);
        assert_eq!(lines[2], r#"["y,z",true]"#);
    }

    #[test]
    fn writers_create_parent_dirs() {
        let d = tempdir().unwrap();
        let p1 = d.path().join("a/b/full.ndjson");
        write_ndjson_full(&p1, &[json!({"x":1})]).unwrap();
        assert!(p1.is_file());
        let p2 = d.path().join("c/d/compact.ndjson");
        write_ndjson_compact(&p2, &["a".into()], &[vec![json!(1)]]).unwrap();
        assert!(p2.is_file());
    }
}
