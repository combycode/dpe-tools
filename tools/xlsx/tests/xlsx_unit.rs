//! Unit tests for the testable bits of the xlsx tool.
//!
//! All inputs synthetic (no fixture files, no NDA-bound data). Covers:
//!   - parse_cell_ref: A1 / Z9 / AA1 / column wraparound
//!   - excel_serial_to_date: known anchors, leap years, out-of-range
//!   - cell_to_json: scalars, formulas, date-styled cells
//!   - SheetData::from_cells: dimension detection, formula classification

use combycode_dpe_tool_xlsx::xlsx::{
    cell_to_json, excel_serial_to_date, parse_cell_ref,
    CellData, LossyUtf8, RawMerge, SheetData, StyleInfo,
};
use std::io::Read;
use serde_json::json;

// ── parse_cell_ref ─────────────────────────────────────────────────────

#[test]
fn parse_cell_ref_single_letter_columns() {
    assert_eq!(parse_cell_ref("A1"),  (0, 0));
    assert_eq!(parse_cell_ref("B1"),  (0, 1));
    assert_eq!(parse_cell_ref("Z1"),  (0, 25));
    assert_eq!(parse_cell_ref("A10"), (9, 0));
}

#[test]
fn parse_cell_ref_double_letter_columns() {
    // AA = 26, AZ = 51, BA = 52, ZZ = 701, AAA = 702.
    assert_eq!(parse_cell_ref("AA1"),  (0, 26));
    assert_eq!(parse_cell_ref("AZ1"),  (0, 51));
    assert_eq!(parse_cell_ref("BA1"),  (0, 52));
    assert_eq!(parse_cell_ref("ZZ1"),  (0, 701));
    assert_eq!(parse_cell_ref("AAA1"), (0, 702));
}

#[test]
fn parse_cell_ref_lowercase_normalised() {
    // Excel cell refs are usually upper-case; the parser tolerates lower.
    assert_eq!(parse_cell_ref("a1"),  (0, 0));
    assert_eq!(parse_cell_ref("aa10"),(9, 26));
}

#[test]
fn parse_cell_ref_large_row() {
    assert_eq!(parse_cell_ref("A1000000"), (999_999, 0));
}

// ── excel_serial_to_date ───────────────────────────────────────────────

#[test]
fn excel_serial_known_anchors() {
    // The Excel epoch is conventionally 1900-01-00 (a non-existent date) due
    // to the leap-year bug; the parser follows the standard convention where
    // serial 25569 maps to 1970-01-01 (the Unix epoch).
    assert_eq!(excel_serial_to_date(25_569.0).as_deref(), Some("1970-01-01"));
}

#[test]
fn excel_serial_post_1970() {
    // 2024-01-01 = serial 45292 in the Excel 1900 system.
    assert_eq!(excel_serial_to_date(45_292.0).as_deref(), Some("2024-01-01"));
}

#[test]
fn excel_serial_leap_year_february_29() {
    // 2024-02-29 = serial 45351 (2024 is a leap year).
    assert_eq!(excel_serial_to_date(45_351.0).as_deref(), Some("2024-02-29"));
}

#[test]
fn excel_serial_out_of_range_returns_none() {
    assert_eq!(excel_serial_to_date(0.0),               None);
    assert_eq!(excel_serial_to_date(-100.0),            None);
    assert_eq!(excel_serial_to_date(2_958_466.0),       None); // > Excel max
    assert_eq!(excel_serial_to_date(f64::INFINITY),     None);
}

// ── cell_to_json ───────────────────────────────────────────────────────

fn no_date_styles() -> StyleInfo {
    StyleInfo { is_date: vec![false; 16] }
}

fn date_at(idx: usize) -> StyleInfo {
    let mut v = vec![false; 16];
    v[idx] = true;
    StyleInfo { is_date: v }
}

fn cell(row: usize, col: usize, value: Option<&str>, formula: Option<&str>, style_idx: usize) -> CellData {
    CellData {
        row,
        col,
        value: value.map(String::from),
        formula: formula.map(String::from),
        cell_type: "n".into(),
        style_idx,
    }
}

#[test]
fn cell_to_json_string_value() {
    let c = cell(0, 0, Some("hello"), None, 0);
    assert_eq!(cell_to_json(&c, &no_date_styles()), json!("hello"));
}

#[test]
fn cell_to_json_integer() {
    let c = cell(0, 0, Some("42"), None, 0);
    assert_eq!(cell_to_json(&c, &no_date_styles()), json!(42));
}

#[test]
fn cell_to_json_float() {
    let c = cell(0, 0, Some("3.5"), None, 0);
    assert_eq!(cell_to_json(&c, &no_date_styles()), json!(3.5));
}

#[test]
fn cell_to_json_null_when_value_missing() {
    let c = cell(0, 0, None, None, 0);
    assert_eq!(cell_to_json(&c, &no_date_styles()), json!(null));
}

#[test]
fn cell_to_json_date_styled_serial_becomes_iso_string() {
    let c = cell(0, 0, Some("25569"), None, 5);
    assert_eq!(cell_to_json(&c, &date_at(5)), json!("1970-01-01"));
}

#[test]
fn cell_to_json_non_date_style_serial_stays_numeric() {
    let c = cell(0, 0, Some("25569"), None, 0);
    assert_eq!(cell_to_json(&c, &no_date_styles()), json!(25569));
}

#[test]
fn cell_to_json_formula_yields_pair() {
    let c = cell(0, 0, Some("100"), Some("SUM(A1:A10)"), 0);
    let v = cell_to_json(&c, &no_date_styles());
    assert_eq!(v, json!([100, "=SUM(A1:A10)"]));
}

#[test]
fn cell_to_json_formula_already_prefixed_with_equals() {
    let c = cell(0, 0, Some("5"), Some("=A1*2"), 0);
    assert_eq!(cell_to_json(&c, &no_date_styles()), json!([5, "=A1*2"]));
}

// ── SheetData::from_cells ──────────────────────────────────────────────

#[test]
fn sheet_data_dimensions_match_max_cell() {
    let cells = vec![
        cell(0, 0, Some("a"), None, 0),
        cell(2, 4, Some("b"), None, 0), // row=3, col=5 in 1-based
    ];
    let sd = SheetData::from_cells(cells, vec![]);
    assert_eq!(sd.max_row, 3);
    assert_eq!(sd.max_col, 5);
}

#[test]
fn sheet_data_get_value_roundtrip() {
    let cells = vec![
        cell(0, 0, Some("alpha"), None, 0),
        cell(1, 1, Some("beta"),  None, 0),
    ];
    let sd = SheetData::from_cells(cells, vec![]);
    assert_eq!(sd.get_value(0, 0), Some("alpha"));
    assert_eq!(sd.get_value(1, 1), Some("beta"));
    assert_eq!(sd.get_value(2, 2), None);
    assert!(sd.has_data(0, 0));
    assert!(!sd.has_data(2, 2));
}

#[test]
fn sheet_data_classifies_sum_rows() {
    let cells = vec![
        cell(0, 0, Some("100"), None, 0),
        cell(1, 0, Some("200"), None, 0),
        cell(2, 0, Some("300"), Some("SUM(A1:A2)"), 0),
    ];
    let sd = SheetData::from_cells(cells, vec![]);
    assert!(!sd.rows_with_sum.contains(&0));
    assert!(!sd.rows_with_sum.contains(&1));
    assert!(sd.rows_with_sum.contains(&2));
}

#[test]
fn sheet_data_classifies_subtotal_rows() {
    // SUBTOTAL is also recognised (Excel uses it for collapsible groups).
    let cells = vec![
        cell(0, 0, Some("0"), Some("SUBTOTAL(9,A2:A10)"), 0),
    ];
    let sd = SheetData::from_cells(cells, vec![]);
    assert!(sd.rows_with_sum.contains(&0));
}

#[test]
fn sheet_data_carries_merges() {
    let merges = vec![RawMerge { min_row: 0, max_row: 0, min_col: 0, max_col: 4 }];
    let sd = SheetData::from_cells(vec![cell(0, 0, Some("title"), None, 0)], merges);
    assert_eq!(sd.merges.len(), 1);
    assert_eq!(sd.merges[0].max_col - sd.merges[0].min_col + 1, 5);
}

// ── XML text and entity references (quick-xml >= 0.38 emits them as separate events) ──

fn archive_with(files: &[(&str, &str)]) -> zip::ZipArchive<std::io::Cursor<Vec<u8>>> {
    use std::io::Write;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);
    for (name, body) in files {
        w.start_file(*name, opts).unwrap();
        w.write_all(body.as_bytes()).unwrap();
    }
    let cur = w.finish().unwrap();
    zip::ZipArchive::new(std::io::Cursor::new(cur.into_inner())).unwrap()
}

#[test]
fn shared_strings_resolve_entities_and_char_refs() {
    use combycode_dpe_tool_xlsx::xlsx::read_shared_strings;
    let mut a = archive_with(&[(
        "xl/sharedStrings.xml",
        r#"<sst><si><t>A &amp; B &lt;c&gt; &quot;q&quot; &apos;s&apos;</t></si><si><r><t>x</t></r><r><t>&#x41;&#66;</t></r></si><si><t>plain</t></si></sst>"#,
    )]);
    assert_eq!(read_shared_strings(&mut a), vec![
        "A & B <c> \"q\" 's'".to_string(), "xAB".to_string(), "plain".to_string(),
    ]);
}

#[test]
fn sheet_values_formulas_and_inline_strings_resolve_entities() {
    use combycode_dpe_tool_xlsx::xlsx::parse_sheet;
    let mut a = archive_with(&[(
        "xl/worksheets/sheet1.xml",
        r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>R&amp;D</t></is></c><c r="B1"><f>IF(A1&lt;&gt;"",1,0)</f><v>1</v></c><c r="C1" t="str"><v>a&amp;b</v></c></row></sheetData><mergeCells><mergeCell ref="A2:B3"/></mergeCells></worksheet>"#,
    )]);
    let s = parse_sheet(&mut a, 0, &[]);
    assert_eq!(s.get_value(0, 0), Some("R&D"));
    assert_eq!(s.get_cell(0, 1).and_then(|c| c.formula.as_deref()), Some("IF(A1<>\"\",1,0)"));
    assert_eq!(s.get_value(0, 2), Some("a&b"));
    assert_eq!(s.merges.len(), 1);
    assert_eq!((s.merges[0].min_row, s.merges[0].max_col), (1, 1));
}

#[test]
fn sheet_names_and_date_styles_read_attributes() {
    use combycode_dpe_tool_xlsx::xlsx::{read_sheet_names, read_styles};
    let mut a = archive_with(&[
        ("xl/workbook.xml", r#"<workbook><sheets><sheet name="One" sheetId="1"/><sheet name="Two" sheetId="2"/></sheets></workbook>"#),
        ("xl/styles.xml", r#"<styleSheet><numFmts count="1"><numFmt numFmtId="164" formatCode="dd/mm/yyyy"/></numFmts><cellXfs count="3"><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="14"/></cellXfs></styleSheet>"#),
    ]);
    assert_eq!(read_sheet_names(&mut a), vec!["One".to_string(), "Two".to_string()]);
    let st = read_styles(&mut a);
    assert_eq!((st.is_date_style(0), st.is_date_style(1), st.is_date_style(2)), (false, true, true));
}

// ── LossyUtf8 ──────────────────────────────────────────────────────────

fn lossy(input: &[u8], chunk: usize) -> String {
    let mut out = String::new();
    LossyUtf8::with_chunk(input, chunk).read_to_string(&mut out).unwrap();
    out
}

#[test]
fn lossy_utf8_passes_valid_text_through_at_any_chunk_size() {
    let text = "plain, кириллица, 漢字, 😀 & more";
    for chunk in [1, 2, 3, 5, 64 * 1024] {
        assert_eq!(lossy(text.as_bytes(), chunk), text, "chunk {chunk}");
    }
}

#[test]
fn lossy_utf8_replaces_invalid_bytes_and_keeps_the_rest() {
    let input = b"<t>tw\xe9o</t><t>three \xff\xfe end</t>";
    for chunk in [1, 2, 4, 1024] {
        assert_eq!(lossy(input, chunk), "<t>tw\u{FFFD}o</t><t>three \u{FFFD}\u{FFFD} end</t>", "chunk {chunk}");
    }
}

#[test]
fn lossy_utf8_truncated_sequence_at_eof_becomes_replacement() {
    assert_eq!(lossy(b"ok \xd0", 1024), "ok \u{FFFD}");
    assert_eq!(lossy(b"ok \xf0\x9f\x98", 2), "ok \u{FFFD}");
}

/// Yields its chunks in order, failing once with `Interrupted` before the second one.
struct InterruptOnce { parts: Vec<&'static [u8]>, next: usize, interrupted: bool }

impl Read for InterruptOnce {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.next == 1 && !self.interrupted {
            self.interrupted = true;
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        let Some(part) = self.parts.get(self.next) else { return Ok(0) };
        let n = part.len().min(buf.len());
        buf[..n].copy_from_slice(&part[..n]);
        self.next += 1;
        Ok(n)
    }
}

#[test]
fn lossy_utf8_keeps_a_split_character_across_an_interrupted_read() {
    // "ж" is D0 B6, split across the interrupted read.
    let src = InterruptOnce { parts: vec![b"a\xd0", b"\xb6b"], next: 0, interrupted: false };
    let mut out = String::new();
    LossyUtf8::new(src).read_to_string(&mut out).unwrap();
    assert_eq!(out, "aжb");
}
