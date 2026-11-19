use combycode_dpe::prelude::*;
use combycode_dpe::dpe_run;
use combycode_dpe::paths::EnvPaths;
use std::sync::OnceLock;

use combycode_dpe_tool_read_tables::extract::{
    self, build_extract_path, source_stem_from_path, Format,
};
use combycode_dpe_tool_read_tables::matcher;
use combycode_dpe_tool_read_tables::matcher::{extract_data_values, FoundTable};
use combycode_dpe_tool_read_tables::matcher_v3::find_tables_v3;
use combycode_dpe_tool_read_tables::template::Template;

/// Header-band-first matcher (v3). The earlier anchor-walk (v1) and
/// data-rectangle (v2) implementations have been removed; v3 covers
/// every case they did plus side-by-side / 2D-merge / sparse-section
/// layouts they missed.
fn find_tables_dispatch(sheet: &[Value], templates: &[Template]) -> Vec<FoundTable> {
    find_tables_v3(sheet, templates)
}

/// Templates loaded once at startup, stored in global.
static TEMPLATES: OnceLock<Vec<Template>> = OnceLock::new();

fn load_templates(settings: &Value) -> &Vec<Template> {
    TEMPLATES.get_or_init(|| {
        let formats_dir = settings.get("formats")
            .and_then(|v| v.as_str())
            .unwrap_or("formats");

        let path = std::path::Path::new(formats_dir);
        match Template::load_all(path) {
            Ok(t) => {
                eprintln!("{{\"type\":\"log\",\"level\":\"info\",\"msg\":\"Loaded {} templates from {}\"}}", t.len(), formats_dir);
                t
            }
            Err(e) => {
                eprintln!("{{\"type\":\"error\",\"error\":\"Cannot load templates: {}\"}}", e);
                vec![]
            }
        }
    })
}

/// Parse mode: find tables, emit data rows.
/// Input: {path: "sheet.json", sheet: "SheetName"}
/// Output: {sheet, table, format, title, columns, row: [...values...]}
fn process_input(v: Value, settings: &Value, ctx: &mut Context<'_>) {
    let v = EnvPaths::from_env().resolve_value(v);

    let templates = load_templates(settings);
    let mode = settings.get("mode").and_then(|m| m.as_str()).unwrap_or("parse");
    let passthrough_input = settings.get("passthrough_input")
        .and_then(|x| x.as_bool()).unwrap_or(false);

    let path = match v.get("path").and_then(|p| p.as_str()) {
        Some(p) => p.to_string(),
        None => {
            ctx.error(&v, "No path in input");
            return;
        }
    };

    // Load sheet JSON. Accept both forms:
    //   new: { source_file, sheet_name, extracted_at, rows: [...] }
    //   old: [ row, row, ... ]  (bare array)
    let raw: Value = match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(data) => data,
            Err(e) => {
                ctx.error(&v, &format!("Invalid JSON in {}: {}", path, e));
                return;
            }
        },
        Err(e) => {
            ctx.error(&v, &format!("Cannot read {}: {}", path, e));
            return;
        }
    };

    struct Embedded {
        rows: Vec<Value>,
        source_file: Option<String>,
        source_path: Option<String>,
        source_relative: Option<String>,
        sheet_name: Option<String>,
    }
    let embedded = match raw {
        Value::Array(arr) => Embedded {
            rows: arr, source_file: None, source_path: None,
            source_relative: None, sheet_name: None,
        },
        Value::Object(mut m) => {
            let rows = match m.remove("rows") {
                Some(Value::Array(a)) => a,
                Some(_) | None => {
                    ctx.error(&v, &format!("sheet JSON {} has no 'rows' array", path));
                    return;
                }
            };
            let take_str = |m: &mut serde_json::Map<String, Value>, k: &str| -> Option<String> {
                m.remove(k).and_then(|v| match v { Value::String(s) => Some(s), _ => None })
            };
            let src       = take_str(&mut m, "source_file");
            let src_path  = take_str(&mut m, "source_path");
            let src_rel   = take_str(&mut m, "source_relative");
            let sn        = take_str(&mut m, "sheet_name");
            Embedded {
                rows, source_file: src, source_path: src_path,
                source_relative: src_rel, sheet_name: sn,
            }
        }
        _ => {
            ctx.error(&v, &format!("sheet JSON {} has unexpected shape", path));
            return;
        }
    };
    let sheet_data = embedded.rows;

    // Resolution order for sheet / source_*: input envelope → embedded in
    // sheet JSON → default fallbacks.
    let sheet_name: String = v.get("sheet").and_then(|s| s.as_str())
        .map(String::from)
        .or(embedded.sheet_name)
        .unwrap_or_else(|| "unknown".to_string());
    let source_file: Option<String> = v.get("source_file").and_then(|s| s.as_str())
        .map(String::from)
        .or(embedded.source_file);
    let source_path: Option<String> = v.get("source_path").and_then(|s| s.as_str())
        .map(String::from)
        .or(embedded.source_path);
    let source_relative: Option<String> = v.get("source_relative").and_then(|s| s.as_str())
        .map(String::from)
        .or(embedded.source_relative);

    // Build the passthrough field set BEFORE we move into mode_parse.
    // Excludes keys read-tables consumes as inputs (path / sheet /
    // source_*) — those are configuration, not tags worth propagating
    // onto every row. Other v fields (e.g. v.label, v.batch_tag set
    // upstream) flow through to every emitted row envelope when the
    // flag is on.
    static RESERVED_INPUT_KEYS: &[&str] = &[
        "path", "sheet", "source_file", "source_path", "source_relative",
    ];
    let passthrough: Vec<(String, Value)> = if passthrough_input {
        v.as_object()
            .map(|m| m.iter()
                .filter(|(k, _)| !RESERVED_INPUT_KEYS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    match mode {
        "parse" => mode_parse(
            &sheet_data, &sheet_name,
            source_file.as_deref(),
            source_path.as_deref(),
            source_relative.as_deref(),
            &passthrough,
            templates, ctx,
        ),
        "detect" => mode_detect(&sheet_data, &sheet_name, &path, settings, ctx),
        "tables" => mode_tables(
            &v, &sheet_data, &sheet_name,
            source_file.as_deref(),
            source_path.as_deref(),
            source_relative.as_deref(),
            &passthrough,
            templates, settings, ctx,
        ),
        _ => ctx.error(&v, &format!("Unknown mode: {}", mode)),
    }
}

/// Per-row `v` object — same shape `mode_parse` emits as an envelope's
/// `v` field, AND what `mode_tables` writes as one line of an
/// `ndjson` (full) extract file. Factored out so the two paths can't
/// drift.
///
/// Insertion order:
///   1. passthrough fields FIRST (so read-tables's own per-row fields
///      overwrite on collision — mirrors mode_parse's behavior);
///   2. source_* identification (when present);
///   3. table identifying metadata (sheet, table, format, title, columns);
///   4. per-row data (row, row_idx);
///   5. range bounds (col_start, col_end, row_start, row_end).
#[allow(clippy::too_many_arguments)]
fn build_row_v_object(
    table: &FoundTable,
    table_idx: usize,
    row_idx: usize,
    row_values: Vec<Value>,
    sheet_name: &str,
    source_file: Option<&str>,
    source_path: Option<&str>,
    source_relative: Option<&str>,
    passthrough: &[(String, Value)],
    columns_json: &[Value],
    row_end_inclusive: usize,
) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    for (k, val) in passthrough {
        out.insert(k.clone(), val.clone());
    }
    if let Some(sf) = source_file {
        out.insert("source_file".into(), Value::String(sf.to_string()));
    }
    if let Some(sp) = source_path {
        out.insert("source_path".into(), Value::String(sp.to_string()));
    }
    if let Some(sr) = source_relative {
        out.insert("source_relative".into(), Value::String(sr.to_string()));
    }
    out.insert("sheet".into(),     Value::String(sheet_name.to_string()));
    out.insert("table".into(),     Value::Number((table_idx as u64).into()));
    out.insert("format".into(),    Value::String(table.format.clone()));
    out.insert("title".into(),     match &table.title {
        Some(t) => Value::String(t.clone()),
        None    => Value::Null,
    });
    out.insert("columns".into(),   Value::Array(columns_json.to_vec()));
    out.insert("row".into(),       Value::Array(row_values));
    out.insert("row_idx".into(),   Value::Number(((row_idx + 1) as u64).into()));
    out.insert("col_start".into(), Value::Number((table.col_start as u64).into()));
    out.insert("col_end".into(),   Value::Number((table.col_end as u64).into()));
    out.insert("row_start".into(), Value::Number(((table.header_start + 1) as u64).into()));
    out.insert("row_end".into(),   Value::Number(((row_end_inclusive + 1) as u64).into()));
    out
}

/// Parse mode: apply known templates, emit structured data rows.
///
/// `passthrough` is a list of (key, value) pairs from the input
/// envelope's v that flow onto every emitted row envelope. Inserted
/// FIRST so read-tables's own per-row fields (`row`, `columns`,
/// `sheet`, `source_*`, etc.) overwrite on conflict. Empty when the
/// stage didn't enable `passthrough_input`.
#[allow(clippy::too_many_arguments)]
fn mode_parse(
    sheet: &[Value],
    sheet_name: &str,
    source_file: Option<&str>,
    source_path: Option<&str>,
    source_relative: Option<&str>,
    passthrough: &[(String, Value)],
    templates: &[Template],
    ctx: &mut Context<'_>,
) {
    let tables = find_tables_dispatch(sheet, templates);

    if tables.is_empty() {
        ctx.log(&format!("No tables found in sheet '{}'", sheet_name), "warn");
        return;
    }

    let mut tables_meta = Vec::new();

    for (table_idx, table) in tables.iter().enumerate() {
        let columns_json: Vec<Value> = table.columns.iter()
            .map(|c| Value::String(c.clone()))
            .collect();

        // Row range covered by this table (header_start .. last data row)
        let row_end = table.data_rows.last().copied().unwrap_or(table.data_start);

        for &row_idx in &table.data_rows {
            let values = extract_data_values(&sheet[row_idx], table.col_start, table.col_end);

            let src = match source_file {
                Some(sf) => format!("{}:{}:{}", sf, sheet_name, row_idx + 1),
                None => format!("{}:{}", sheet_name, row_idx + 1),
            };
            let id = ctx.hash(&format!("{}:{}:{}:{}",
                source_file.unwrap_or(""), sheet_name, table_idx, row_idx));

            let out = build_row_v_object(
                table, table_idx, row_idx, values,
                sheet_name, source_file, source_path, source_relative,
                passthrough, &columns_json, row_end,
            );

            ctx.output(Value::Object(out), Some(&id), Some(&src));
        }

        tables_meta.push(json!({
            "format":    table.format,
            "title":     table.title,
            "columns":   table.columns,
            "rows":      table.data_rows.len(),
            "col_start": table.col_start,
            "col_end":   table.col_end,
            "row_start": table.header_start + 1,
            "row_end":   row_end + 1,
        }));
    }

    let mut meta = serde_json::Map::new();
    if let Some(sf) = source_file {
        meta.insert("source_file".into(), Value::String(sf.to_string()));
    }
    if let Some(sp) = source_path {
        meta.insert("source_path".into(), Value::String(sp.to_string()));
    }
    if let Some(sr) = source_relative {
        meta.insert("source_relative".into(), Value::String(sr.to_string()));
    }
    meta.insert("sheet".into(),  Value::String(sheet_name.to_string()));
    meta.insert("tables".into(), Value::Array(tables_meta));
    ctx.meta(Value::Object(meta));
}

/// Tables mode: extract each detected table to a file and emit one
/// envelope per table referencing it.
///
/// Same matcher as `mode_parse`; same per-row v shape (when format
/// is `ndjson` full); same `passthrough_input` rule. The difference
/// is granularity: row-mode emits N envelopes per table; tables-mode
/// emits ONE envelope per table and the actual rows live on disk.
///
/// On any I/O failure (mkdir / open / write / flush) the table is
/// surfaced via `ctx.error(&v, ...)` — half-extracted output is worse
/// than no output. The remaining tables in the same sheet still
/// process (each is independent).
#[allow(clippy::too_many_arguments)]
fn mode_tables(
    input_envelope:  &Value,
    sheet:           &[Value],
    sheet_name:      &str,
    source_file:     Option<&str>,
    source_path:     Option<&str>,
    source_relative: Option<&str>,
    passthrough:     &[(String, Value)],
    templates:       &[Template],
    settings:        &Value,
    ctx:             &mut Context<'_>,
) {
    // Resolve `extract: { dir, format }` settings up front. Both
    // are required for tables mode — fail loudly if missing.
    let extract_block = match settings.get("extract") {
        Some(Value::Object(m)) => m,
        Some(_) | None => {
            ctx.error(input_envelope,
                "tables mode requires `extract: { dir, format }` settings block");
            return;
        }
    };
    let dir = match extract_block.get("dir").and_then(|v| v.as_str()) {
        Some(s) => std::path::PathBuf::from(s),
        None => {
            ctx.error(input_envelope, "tables mode: `extract.dir` is required");
            return;
        }
    };
    let format_str = extract_block.get("format").and_then(|v| v.as_str()).unwrap_or("ndjson");
    let format = match Format::parse(format_str) {
        Some(f) => f,
        None => {
            ctx.error(input_envelope, &format!(
                "tables mode: unknown extract.format '{}' (use csv | ndjson | ndjson-compact)",
                format_str,
            ));
            return;
        }
    };

    let tables = find_tables_dispatch(sheet, templates);
    if tables.is_empty() {
        ctx.log(&format!("No tables found in sheet '{}'", sheet_name), "warn");
        return;
    }

    // Source stem for filename auto-derivation. Prefer source_file
    // (the basename / mid-path the upstream provided) over source_path
    // because source_path may be the absolute resolved-from-disk
    // version with cache-mangling.
    let source_stem: Option<String> = source_file
        .and_then(source_stem_from_path)
        .or_else(|| source_path.and_then(source_stem_from_path));

    for (table_idx, table) in tables.iter().enumerate() {
        let columns_json: Vec<Value> = table.columns.iter()
            .map(|c| Value::String(c.clone()))
            .collect();
        let row_end = table.data_rows.last().copied().unwrap_or(table.data_start);

        // Materialise rows into the same Vec<Value> shape mode_parse
        // would emit per row (so ndjson-full's lines are byte-identical
        // to what mode_parse would have produced).
        let mut row_values_per_data_row: Vec<Vec<Value>> = Vec::with_capacity(table.data_rows.len());
        let mut full_v_objects:          Vec<Value>      = Vec::with_capacity(table.data_rows.len());
        for &row_idx in &table.data_rows {
            let values = extract_data_values(&sheet[row_idx], table.col_start, table.col_end);
            if matches!(format, Format::NdjsonFull) {
                let v_obj = build_row_v_object(
                    table, table_idx, row_idx, values.clone(),
                    sheet_name, source_file, source_path, source_relative,
                    passthrough, &columns_json, row_end,
                );
                full_v_objects.push(Value::Object(v_obj));
            }
            row_values_per_data_row.push(values);
        }

        let target_path = build_extract_path(&dir, source_stem.as_deref(), sheet_name, table_idx, format);

        let write_result = match format {
            Format::Csv =>
                extract::write_csv(&target_path, &table.columns, &row_values_per_data_row),
            Format::NdjsonFull =>
                extract::write_ndjson_full(&target_path, &full_v_objects),
            Format::NdjsonCompact =>
                extract::write_ndjson_compact(&target_path, &table.columns, &row_values_per_data_row),
        };
        if let Err(e) = write_result {
            ctx.error(input_envelope, &format!(
                "extract table {} from sheet '{}' to {}: {}",
                table_idx, sheet_name, target_path.display(), e,
            ));
            continue;
        }

        // Build the per-table envelope. Same identifying fields as
        // the row-mode envelope (sheet, table, format, title, columns,
        // col_start, col_end, row_start, row_end, source_*, passthrough)
        // but `row` / `row_idx` are replaced by `target` (file written)
        // and `range` (A1-style readable range label).
        let src = match source_file {
            Some(sf) => format!("{}:{}:t{}", sf, sheet_name, table_idx),
            None     => format!("{}:t{}", sheet_name, table_idx),
        };
        let id = ctx.hash(&format!("{}:{}:t{}",
            source_file.unwrap_or(""), sheet_name, table_idx));

        let mut out = serde_json::Map::new();
        for (k, val) in passthrough {
            out.insert(k.clone(), val.clone());
        }
        if let Some(sf) = source_file {
            out.insert("source_file".into(), Value::String(sf.to_string()));
        }
        if let Some(sp) = source_path {
            out.insert("source_path".into(), Value::String(sp.to_string()));
        }
        if let Some(sr) = source_relative {
            out.insert("source_relative".into(), Value::String(sr.to_string()));
        }
        out.insert("sheet".into(),     Value::String(sheet_name.to_string()));
        out.insert("table".into(),     Value::Number((table_idx as u64).into()));
        out.insert("format".into(),    Value::String(table.format.clone()));
        out.insert("title".into(),     match &table.title {
            Some(t) => Value::String(t.clone()),
            None    => Value::Null,
        });
        out.insert("columns".into(),   Value::Array(columns_json));
        out.insert("col_start".into(), Value::Number((table.col_start as u64).into()));
        out.insert("col_end".into(),   Value::Number((table.col_end as u64).into()));
        out.insert("row_start".into(), Value::Number(((table.header_start + 1) as u64).into()));
        out.insert("row_end".into(),   Value::Number(((row_end + 1) as u64).into()));
        out.insert("range".into(),     Value::String(extract::a1_range(
            table.col_start, table.col_end,
            table.header_start + 1, row_end + 1,
        )));
        out.insert("target".into(),    Value::String(
            target_path.to_string_lossy().replace('\\', "/")
        ));

        ctx.output(Value::Object(out), Some(&id), Some(&src));
    }
}

/// Detect mode: find table shapes, write new format templates to output dir.
fn mode_detect(sheet: &[Value], sheet_name: &str, _path: &str, settings: &Value, ctx: &mut Context<'_>) {
    let output_dir = settings.get("output").and_then(|v| v.as_str()).unwrap_or(".");

    // Walk rows, detect table-like structures without templates
    let detected = detect_table_shapes(sheet);

    for (idx, shape) in detected.iter().enumerate() {
        // Generate template content
        let tpl_content = shape.to_template_string();
        let tpl_name = format!("{}_{}", safe_name(sheet_name), idx);
        let tpl_path = std::path::Path::new(output_dir).join(format!("{}.tpl", tpl_name));

        if let Ok(mut f) = std::fs::File::create(&tpl_path) {
            use std::io::Write;
            let _ = f.write_all(tpl_content.as_bytes());
        }

        let tpl_full_path = tpl_path.to_string_lossy().replace('\\', "/");
        ctx.meta(json!({
            "sheet": sheet_name,
            "format": tpl_name,
            "path": tpl_full_path,
            "shape": tpl_content,
            "rows": shape.data_count,
        }));
    }
}

/// Detected table shape for detect mode.
struct DetectedShape {
    has_title: bool,
    has_subheader: bool,
    data_count: usize,
    has_totals: bool,
}

impl DetectedShape {
    fn to_template_string(&self) -> String {
        let mut lines = Vec::new();
        if self.has_title {
            lines.push("title     : merge".to_string());
        }
        lines.push("header    : text+".to_string());
        if self.has_subheader {
            lines.push("subheader?: text+".to_string());
        }
        lines.push("data*     : any ...".to_string());
        if self.has_totals {
            lines.push("total~    : period|total|итого|sparse|sum".to_string());
        }
        lines.join("\n") + "\n"
    }
}

/// Detect table shapes without templates — for discover mode.
fn detect_table_shapes(sheet: &[Value]) -> Vec<DetectedShape> {
    let mut shapes = Vec::new();
    let mut row = 0;

    while row < sheet.len() {
        if matcher::is_null_or_empty_pub(&sheet[row]) {
            row += 1;
            continue;
        }

        // Check if this looks like a table start
        if let Some((shape, end_row)) = try_detect_shape(sheet, row) {
            if shape.data_count > 0 {
                shapes.push(shape);
            }
            row = end_row + 1;
        } else {
            row += 1;
        }
    }

    shapes
}

fn try_detect_shape(sheet: &[Value], start: usize) -> Option<(DetectedShape, usize)> {
    let mut cursor = start;

    // Check for title (merge)
    let has_title = sheet.get(cursor)
        .and_then(|r| r.as_array())
        .map(|cells| cells.iter().any(|c| c.is_object() && c.get("span").is_some()))
        .unwrap_or(false);

    if has_title { cursor += 1; }
    if cursor >= sheet.len() { return None; }

    // Check for header (text+)
    let is_header = sheet.get(cursor)
        .and_then(|r| r.as_array())
        .map(|cells| {
            let text_count = cells.iter().filter(|c| c.is_string() || (c.is_object() && c.get("v").is_some())).count();
            text_count >= 2
        })
        .unwrap_or(false);

    if !is_header { return None; }
    cursor += 1;

    // Check for subheader
    let has_subheader = cursor < sheet.len() && sheet.get(cursor)
        .and_then(|r| r.as_array())
        .map(|cells| {
            let text_count = cells.iter().filter(|c| c.is_string()).count();
            text_count >= 2
        })
        .unwrap_or(false);

    if has_subheader { cursor += 1; }

    // Skip empty row
    while cursor < sheet.len() && matcher::is_null_or_empty_pub(&sheet[cursor]) {
        cursor += 1;
    }

    // Count data rows
    let mut data_count = 0;
    let mut has_totals = false;
    let mut empty_streak = 0;

    while cursor < sheet.len() {
        if matcher::is_null_or_empty_pub(&sheet[cursor]) {
            empty_streak += 1;
            if empty_streak >= 5 { break; }
            cursor += 1;
            continue;
        }
        empty_streak = 0;

        // Simple total detection for detect mode
        if let Some(cells) = sheet[cursor].as_array() {
            let first_text = cells.first().and_then(|c| c.as_str()).unwrap_or("");
            let lower = first_text.to_lowercase();
            if lower.contains("total") || lower.contains("итого") || PERIOD_RE.is_match(first_text.trim().trim_end_matches(':')) {
                has_totals = true;
                cursor += 1;
                continue;
            }
            // Check for SUM
            if cells.iter().any(matcher::is_sum_formula_pub) {
                has_totals = true;
                cursor += 1;
                continue;
            }
        }

        data_count += 1;
        cursor += 1;
    }

    if data_count == 0 { return None; }

    Some((DetectedShape {
        has_title,
        has_subheader,
        data_count,
        has_totals,
    }, cursor.saturating_sub(1)))
}

use regex::Regex;
static PERIOD_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"^\d{1,2}\.\d{4}:?$").unwrap());

fn safe_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect::<String>()
        .to_lowercase()
}

fn main() {
    dpe_run! {
        input: process_input,
    };
}
