//! Raw xlsx extractor. Writes sheet files + meta to output directory.
//!
//! Input: {path: "file.xlsx", output: "output/dir/", source_file?: "rel/path.xlsx", root?: "$input/"}
//!   - `source_file`: provenance label. If set, used verbatim. Else the
//!     basename of `path`.
//!   - `root`: absolute path prefix (e.g. `$input/`). When present, xlsx
//!     records the input file's path relative to root as `source_relative`
//!     (a directory-tree-preserving label that survives across machines).
//!     When absent, `source_relative` is omitted.
//!
//! Creates: output/dir/filename/
//!   - SheetName.json per sheet — wrapped as
//!     {source_file, source_path, source_relative?, sheet_name, extracted_at, rows}
//!     so each file is self-describing.
//!   - meta.json with source_file + source_path + source_relative? +
//!     extracted_at + sheets.
//!
//! Output to stdout: one record per generated sheet containing the sheet
//! JSON path, source_file, source_path, source_relative (when known), and
//! sheet name (enough for downstream routing decisions without re-opening
//! meta.json).
//!
//! Caching: parse + write is wrapped in `ctx.cached("xlsx", ...)`. On a
//! cache hit, parsing + file writes are skipped — we re-emit the same
//! envelopes from the cached manifest. Output files written on the
//! previous miss MUST still be on disk for downstream tools to read
//! them; clearing the output dir without clearing the cache will leave
//! dangling paths in emitted envelopes.

use combycode_dpe::prelude::*;
use combycode_dpe::dpe_run;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

use combycode_dpe_tool_xlsx::xlsx;

const CACHE_NAMESPACE: &str = "xlsx";

/// Cache value: just enough to reconstruct the emitted envelopes.
/// Heavy data (full row arrays) is NOT stored here — it lives in the
/// per-sheet JSON files written during the miss path.
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    sheets: Vec<ManifestSheet>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestSheet {
    /// Original sheet name as it appears in the workbook.
    name: String,
    /// Filesystem-safe variant used for the per-sheet `<name>.json` file.
    safe_name: String,
    rows: usize,
    cols: usize,
}

/// Cache key — every field that, if changed, must produce a fresh run.
/// `schema` lets us invalidate all entries by bumping the constant
/// when the manifest layout changes.
///
/// `source_path` and `source_relative` are part of the key because they
/// are persisted into the per-sheet JSON files; if either changes between
/// runs the existing on-disk JSON would carry stale provenance, so the
/// extraction must be re-done.
fn build_cache_key(
    file_hash: &str,
    output_dir: &str,
    source_file: &str,
    source_path: &str,
    source_relative: Option<&str>,
    filename: &str,
) -> Value {
    json!({
        "file_hash":       file_hash,
        "output_dir":      output_dir,
        "source_file":     source_file,
        "source_path":     source_path,
        "source_relative": source_relative,
        "filename":        filename,
        "schema":          4,
    })
}

/// Compute `source_relative` (path relative to `root`) when both are
/// present and `path` actually starts with `root`. Otherwise returns
/// None. All paths are normalised to forward slashes for comparison.
fn compute_source_relative(path: &str, root: Option<&str>) -> Option<String> {
    let root = root?;
    let path_norm = path.replace('\\', "/");
    let mut root_norm = root.replace('\\', "/");
    if !root_norm.ends_with('/') {
        root_norm.push('/');
    }
    path_norm.strip_prefix(&root_norm).map(|s| s.to_string())
}

/// Build the per-anchor merge map: `(row, col) -> (colspan, rowspan)`.
/// Both `colspan` and `rowspan` are inclusive cell counts (a non-merged
/// cell would be `(1, 1)`, which we deliberately skip). Covered cells
/// of a merge (any position other than the anchor `(min_row, min_col)`)
/// carry no entry here and emit as `null` in the row loop above.
fn build_merge_info(
    merges: &[combycode_dpe_tool_xlsx::xlsx::RawMerge],
) -> std::collections::HashMap<(usize, usize), (usize, usize)> {
    let mut info = std::collections::HashMap::new();
    for m in merges {
        let colspan = m.max_col.saturating_sub(m.min_col) + 1;
        let rowspan = m.max_row.saturating_sub(m.min_row) + 1;
        if colspan > 1 || rowspan > 1 {
            info.insert((m.min_row, m.min_col), (colspan, rowspan));
        }
    }
    info
}

/// Wrap a cell value into `{span?, rspan?, v}` when its anchor is merged,
/// else pass the value through unwrapped. Omits each span field when its
/// dimension is 1 so backward compatibility holds: pure horizontal merges
/// still emit `{"span": N, "v": ...}` exactly as before this change.
fn wrap_cell_with_merge(cell_val: Value, merge: Option<(usize, usize)>) -> Value {
    match merge {
        Some((cspan, rspan)) => {
            let mut obj = serde_json::Map::new();
            if cspan > 1 {
                obj.insert("span".into(), Value::Number((cspan as u64).into()));
            }
            if rspan > 1 {
                obj.insert("rspan".into(), Value::Number((rspan as u64).into()));
            }
            obj.insert("v".into(), cell_val);
            Value::Object(obj)
        }
        None => cell_val,
    }
}

fn process_input(v: Value, _settings: &Value, ctx: &mut Context<'_>) {
    let path = match v.get("path").or(v.get("file")).and_then(|p| p.as_str()) {
        Some(p) => p.to_string(),
        None => {
            ctx.error(&v, "No path/file in input");
            return;
        }
    };

    let output_dir = match v.get("output").and_then(|p| p.as_str()) {
        Some(p) => p.to_string(),
        None => {
            ctx.error(&v, "No output directory in input");
            return;
        }
    };

    // Provenance label: prefer explicit v.source_file, else basename of v.path.
    let source_file = v.get("source_file").and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            std::path::Path::new(&path).file_name()
                .unwrap_or_default().to_string_lossy().to_string()
        });

    // Absolute source path, forward-slash normalised. Always present.
    let source_path = path.replace('\\', "/");
    // Root-relative path, when v.root is present and path starts with it.
    let root = v.get("root").and_then(|s| s.as_str());
    let source_relative = compute_source_relative(&path, root);

    let filename = match safe_dir_stem(&path) {
        Ok(s) => s,
        Err(e) => {
            ctx.error(&v, &format!("xlsx: {}: {}", e, path));
            return;
        }
    };

    let file_dir = std::path::Path::new(&output_dir).join(&filename);

    // Hash file content — this is the dominant cache-key dimension. If
    // the file isn't readable the parse step would fail anyway, so
    // surface that as an envelope error before touching the cache.
    let file_hash = match ctx.hash_file(&path) {
        Some(h) => h,
        None => {
            ctx.error(&v, &format!("Cannot hash file: {}", path));
            return;
        }
    };

    let cache_key = build_cache_key(
        &file_hash, &output_dir, &source_file,
        &source_path, source_relative.as_deref(), &filename,
    );

    // Move owned values into the closure: cached_impl is FnOnce, so this
    // is fine even though we read them again outside for emission.
    let path_cl              = path.clone();
    let source_file_cl       = source_file.clone();
    let source_path_cl       = source_path.clone();
    let source_relative_cl   = source_relative.clone();
    let file_dir_cl          = file_dir.clone();
    let extracted_at         = chrono::Utc::now().to_rfc3339();
    let extracted_at_cl      = extracted_at.clone();

    let manifest_result: std::io::Result<Manifest> = combycode_dpe::cache::cached_impl(
        CACHE_NAMESPACE,
        &cache_key,
        move || {
            parse_and_write(
                &path_cl,
                &source_file_cl,
                &source_path_cl,
                source_relative_cl.as_deref(),
                &file_dir_cl,
                &extracted_at_cl,
            ).map_err(std::io::Error::other)
        },
    );

    let manifest = match manifest_result {
        Ok(m) => m,
        Err(e) => {
            ctx.error(&v, &format!("xlsx process failed: {}", e));
            return;
        }
    };

    // Emit one envelope per sheet (cache-hit and cache-miss paths
    // produce identical emissions for identical inputs).
    for sheet in &manifest.sheets {
        let sheet_full_path = file_dir
            .join(format!("{}.json", sheet.safe_name))
            .to_string_lossy()
            .replace('\\', "/");
        let id = ctx.hash(&format!("{}:{}", path, sheet.name));
        let mut out = serde_json::Map::new();
        out.insert("path".into(),        Value::String(sheet_full_path));
        out.insert("sheet".into(),       Value::String(sheet.name.clone()));
        out.insert("source_file".into(), Value::String(source_file.clone()));
        out.insert("source_path".into(), Value::String(source_path.clone()));
        if let Some(rel) = &source_relative {
            out.insert("source_relative".into(), Value::String(rel.clone()));
        }
        out.insert("rows".into(),        Value::Number((sheet.rows as u64).into()));
        out.insert("cols".into(),        Value::Number((sheet.cols as u64).into()));
        ctx.output(Value::Object(out), Some(&id), Some(&format!("{}:{}", filename, sheet.name)));
    }

    let mut meta = serde_json::Map::new();
    meta.insert("source_file".into(), Value::String(source_file.clone()));
    meta.insert("source_path".into(), Value::String(source_path.clone()));
    if let Some(rel) = &source_relative {
        meta.insert("source_relative".into(), Value::String(rel.clone()));
    }
    meta.insert("sheets".into(), Value::Number((manifest.sheets.len() as u64).into()));
    ctx.meta(Value::Object(meta));
}

/// Parse the workbook, write per-sheet JSON files + meta.json. Returns
/// a small manifest used to emit envelopes.
///
/// Errors: file read, ZIP open, output-dir creation. Per-sheet write
/// failures are logged to stderr and skipped (matches the pre-cache
/// behavior of `continue` past one bad sheet).
fn parse_and_write(
    path: &str,
    source_file: &str,
    source_path: &str,
    source_relative: Option<&str>,
    file_dir: &Path,
    extracted_at: &str,
) -> Result<Manifest, String> {
    let file_data = std::fs::read(path)
        .map_err(|e| format!("Cannot read file: {}", e))?;
    let cursor = std::io::Cursor::new(file_data);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| format!("Invalid xlsx (ZIP error): {}", e))?;

    let shared_strings = xlsx::read_shared_strings(&mut archive);
    let sheet_names = xlsx::read_sheet_names(&mut archive);
    let styles = xlsx::read_styles(&mut archive);

    std::fs::create_dir_all(file_dir)
        .map_err(|e| format!("Cannot create output dir: {}", e))?;

    let mut sheets_meta_files: Vec<Value> = Vec::new();
    let mut manifest_sheets: Vec<ManifestSheet> = Vec::new();

    for (idx, sheet_name) in sheet_names.iter().enumerate() {
        let sheet = xlsx::parse_sheet(&mut archive, idx, &shared_strings);
        if sheet.max_row == 0 {
            continue;
        }

        // Anchor (row, col) -> (colspan, rowspan). Covered (non-anchor)
        // cells of a merge carry no CellData in XLSX and emit as `null`
        // below; downstream consumers that care about continuation must
        // walk rows in order and remember which columns are still under
        // an open vertical merge.
        let merge_info = build_merge_info(&sheet.merges);

        // Build all rows for this sheet
        let mut rows: Vec<Value> = Vec::new();

        for row in 0..sheet.max_row {
            // Find last filled column. Includes cells with data AND
            // anchor cells of empty-but-merged regions (Excel
            // <mergeCell ref="A1:R1"/> with no value -- still a merge
            // the matcher needs to see).
            let mut last_col: Option<usize> = None;
            for col in 0..sheet.max_col {
                if sheet.has_data(row, col) || merge_info.contains_key(&(row, col)) {
                    last_col = Some(col);
                }
            }

            let last_col = match last_col {
                Some(c) => c,
                None => {
                    rows.push(Value::Null); // empty row marker
                    continue;
                }
            };

            // Always start from column 0 to preserve absolute positions
            let mut cells: Vec<Value> = Vec::new();
            for col in 0..=last_col {
                if let Some(cd) = sheet.get_cell(row, col) {
                    let cell_val = xlsx::cell_to_json(cd, &styles);
                    cells.push(wrap_cell_with_merge(cell_val, merge_info.get(&(row, col)).copied()));
                } else if let Some(&merge) = merge_info.get(&(row, col)) {
                    // Empty cell that IS a merge anchor: preserve the
                    // anchor with v=null so the matcher can see the merge.
                    cells.push(wrap_cell_with_merge(Value::Null, Some(merge)));
                } else {
                    cells.push(Value::Null);
                }
            }

            rows.push(Value::Array(cells));
        }

        // Write sheet file — self-describing envelope wrapping the row array
        let safe_name = safe_filename(sheet_name);
        let sheet_path: PathBuf = file_dir.join(format!("{}.json", safe_name));
        let mut sheet_doc = serde_json::Map::new();
        sheet_doc.insert("source_file".into(),  Value::String(source_file.into()));
        sheet_doc.insert("source_path".into(),  Value::String(source_path.into()));
        if let Some(rel) = source_relative {
            sheet_doc.insert("source_relative".into(), Value::String(rel.into()));
        }
        sheet_doc.insert("sheet_name".into(),   Value::String(sheet_name.into()));
        sheet_doc.insert("extracted_at".into(), Value::String(extracted_at.into()));
        sheet_doc.insert("rows".into(),         Value::Array(rows));
        let sheet_doc = Value::Object(sheet_doc);
        // Write the per-sheet JSON. Failures HERE used to be eprintln+
        // continue, which caused silent data loss (regression:
        // Windows trailing-space dir-name fallout produced 0
        // sheets with no error event). Now any write failure aborts
        // parse_and_write so ctx.error fires upstream and the pipeline
        // surfaces the loss.
        let mut f = std::fs::File::create(&sheet_path)
            .map_err(|e| format!("create sheet file {}: {}", sheet_path.display(), e))?;
        let json_bytes = serde_json::to_vec(&sheet_doc).unwrap_or_default();
        f.write_all(&json_bytes)
            .map_err(|e| format!("write sheet {}: {}", sheet_path.display(), e))?;

        manifest_sheets.push(ManifestSheet {
            name: sheet_name.clone(),
            safe_name: safe_name.clone(),
            rows: sheet.max_row,
            cols: sheet.max_col,
        });
        sheets_meta_files.push(json!({
            "name": sheet_name,
            "file": format!("{}.json", safe_name),
            "rows": sheet.max_row,
            "cols": sheet.max_col,
        }));
    }

    // Write meta.json with full provenance
    let mut meta_obj = serde_json::Map::new();
    meta_obj.insert("source_file".into(),  Value::String(source_file.into()));
    meta_obj.insert("source_path".into(),  Value::String(source_path.into()));
    if let Some(rel) = source_relative {
        meta_obj.insert("source_relative".into(), Value::String(rel.into()));
    }
    meta_obj.insert("extracted_at".into(), Value::String(extracted_at.into()));
    meta_obj.insert("sheets".into(),       Value::Array(sheets_meta_files));
    let meta = Value::Object(meta_obj);
    let meta_path = file_dir.join("meta.json");
    if let Ok(mut f) = std::fs::File::create(&meta_path) {
        let json_bytes = serde_json::to_string_pretty(&meta).unwrap_or_default();
        let _ = f.write_all(json_bytes.as_bytes());
    }

    Ok(Manifest { sheets: manifest_sheets })
}

/// Stem of `path` for use as a per-file output directory name.
///
/// Trims surrounding whitespace from the stem because Windows refuses
/// to create directories with trailing spaces — `create_dir_all` silently
/// strips the trailing space, then later `File::create` against the
/// trailing-space path fails with ERROR_PATH_NOT_FOUND. Without this
/// trim, the bug reproduced silently: every per-sheet write failed
/// and was swallowed, the meta envelope reported `{sheets: 0}`, and the
/// pipeline lost the entire workbook with no error event.
///
/// Empty stem (e.g. `.xlsx` with no name, or whitespace-only) is a hard
/// error — there's no safe directory we could derive.
fn safe_dir_stem(path: &str) -> Result<String, &'static str> {
    let stem = std::path::Path::new(path)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .trim()
        .to_string();
    if stem.is_empty() {
        return Err("file_stem empty after trim");
    }
    Ok(stem)
}

/// Sanitize sheet name for use as filename. Whitespace and special chars → underscore, collapsed.
fn safe_filename(name: &str) -> String {
    let mut result = String::new();
    let mut prev_underscore = false;
    for c in name.chars() {
        let is_bad = matches!(c, ' ' | '\t' | '\n' | '\r' | '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*');
        if is_bad {
            if !prev_underscore {
                result.push('_');
                prev_underscore = true;
            }
        } else {
            result.push(c);
            prev_underscore = false;
        }
    }
    result.trim_matches('_').to_string()
}

fn main() {
    dpe_run! {
        input: process_input,
    };
}

// ─── tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_filename_basic_replacements() {
        assert_eq!(safe_filename("Sheet 1"), "Sheet_1");
        assert_eq!(safe_filename("a/b\\c"), "a_b_c");
        assert_eq!(safe_filename("  spaced  "), "spaced");
        assert_eq!(safe_filename("plain"), "plain");
    }

    // ─── safe_dir_stem (regression) ─────────────────────
    //
    // file_stem("Report .xlsx") returns "Report " (note trailing
    // space). On Windows, create_dir_all silently strips that trailing
    // space, then File::create against the trailing-space path fails
    // with ERROR_PATH_NOT_FOUND. We trim BEFORE the create_dir_all so
    // the dir name and per-sheet write paths agree.

    #[test]
    fn safe_dir_stem_trims_trailing_space() {
        // The exact shape that reproduced the bug.
        assert_eq!(safe_dir_stem("Report .xlsx").unwrap(), "Report");
    }

    #[test]
    fn safe_dir_stem_trims_leading_and_trailing() {
        assert_eq!(safe_dir_stem(" data .xlsx").unwrap(), "data");
    }

    #[test]
    fn safe_dir_stem_preserves_internal_spaces() {
        assert_eq!(
            safe_dir_stem("Q2 12 and 34 ACME .xlsx").unwrap(),
            "Q2 12 and 34 ACME",
        );
    }

    #[test]
    fn safe_dir_stem_plain_basename() {
        assert_eq!(safe_dir_stem("MyData.xlsx").unwrap(), "MyData");
    }

    #[test]
    fn safe_dir_stem_with_directory() {
        assert_eq!(
            safe_dir_stem("D:/some/dir/Report .xlsx").unwrap(),
            "Report",
        );
    }

    #[test]
    fn safe_dir_stem_empty_after_trim_errors() {
        assert!(safe_dir_stem("   .xlsx").is_err());
    }

    #[test]
    fn safe_dir_stem_no_stem_errors() {
        assert!(safe_dir_stem("").is_err());
    }

    // ─── cache key composition ───────────────────────────────────────
    //
    // Pin the key shape: identical inputs MUST produce identical keys,
    // and any field change MUST diverge them — otherwise stale runs
    // would be served when the user changes the destination or
    // provenance label.

    fn ck(file_hash: &str, output_dir: &str, source_file: &str, source_path: &str,
          source_relative: Option<&str>, filename: &str) -> Value {
        build_cache_key(file_hash, output_dir, source_file, source_path, source_relative, filename)
    }

    #[test]
    fn cache_key_stable_for_identical_inputs() {
        let a = ck("hash", "/out", "src.xlsx", "/abs/src.xlsx", Some("rel/src.xlsx"), "src");
        let b = ck("hash", "/out", "src.xlsx", "/abs/src.xlsx", Some("rel/src.xlsx"), "src");
        assert_eq!(a, b);
    }

    #[test]
    fn cache_key_diverges_on_file_hash() {
        let a = ck("hashA", "/out", "src.xlsx", "/abs/src.xlsx", None, "src");
        let b = ck("hashB", "/out", "src.xlsx", "/abs/src.xlsx", None, "src");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_diverges_on_output_dir() {
        let a = ck("h", "/out-a", "src.xlsx", "/abs/src.xlsx", None, "src");
        let b = ck("h", "/out-b", "src.xlsx", "/abs/src.xlsx", None, "src");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_diverges_on_source_file() {
        let a = ck("h", "/out", "alpha.xlsx", "/abs/x.xlsx", None, "src");
        let b = ck("h", "/out", "bravo.xlsx", "/abs/x.xlsx", None, "src");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_diverges_on_source_path() {
        let a = ck("h", "/out", "src.xlsx", "/abs/a/src.xlsx", None, "src");
        let b = ck("h", "/out", "src.xlsx", "/abs/b/src.xlsx", None, "src");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_diverges_on_source_relative() {
        let a = ck("h", "/out", "src.xlsx", "/abs/x.xlsx", Some("a/src.xlsx"), "src");
        let b = ck("h", "/out", "src.xlsx", "/abs/x.xlsx", Some("b/src.xlsx"), "src");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_diverges_on_source_relative_presence() {
        let a = ck("h", "/out", "src.xlsx", "/abs/x.xlsx", Some("a/src.xlsx"), "src");
        let b = ck("h", "/out", "src.xlsx", "/abs/x.xlsx", None,               "src");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_diverges_on_filename() {
        let a = ck("h", "/out", "src.xlsx", "/abs/x.xlsx", None, "alpha");
        let b = ck("h", "/out", "src.xlsx", "/abs/x.xlsx", None, "bravo");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_schema_marker_bumped_to_4() {
        // Schema bumps invalidate pre-existing cached entries so reruns
        // pick up output-shape changes. History:
        //   schema=3 — sheet-JSON header + Windows trailing-whitespace
        //              stem fix
        //   schema=4 — anchor cells of vertical / 2D merges now wrap
        //              into `{rspan?, span?, v}` instead of emitting
        //              the bare value
        // If you change cell emission again, bump and update both the
        // constant and this assertion.
        let key = ck("h", "/out", "src.xlsx", "/abs/x.xlsx", None, "src");
        assert_eq!(key.get("schema").and_then(|v| v.as_u64()), Some(4));
    }

    // ─── source_relative computation (regression) ───────
    //
    // Path tree carries semantic info (category/period/label or similar
    // domain labels). source_relative preserves it; absolute source_path
    // leaks machine-specific prefixes.

    #[test]
    fn source_relative_strips_root_with_trailing_slash() {
        let got = compute_source_relative(
            "D:/data/input/m/file.xlsx",
            Some("D:/data/input/"),
        );
        assert_eq!(got.as_deref(), Some("m/file.xlsx"));
    }

    #[test]
    fn source_relative_strips_root_without_trailing_slash() {
        // Root may arrive without trailing slash; we must add one before
        // comparing or we'd accidentally also match `.../inputXXX/...`.
        let got = compute_source_relative(
            "D:/data/input/m/file.xlsx",
            Some("D:/data/input"),
        );
        assert_eq!(got.as_deref(), Some("m/file.xlsx"));
    }

    #[test]
    fn source_relative_normalises_backslashes() {
        let got = compute_source_relative(
            "D:\\data\\input\\m\\file.xlsx",
            Some("D:/data/input/"),
        );
        assert_eq!(got.as_deref(), Some("m/file.xlsx"));
    }

    #[test]
    fn source_relative_none_when_root_absent() {
        let got = compute_source_relative("D:/data/input/m/file.xlsx", None);
        assert_eq!(got, None);
    }

    #[test]
    fn source_relative_none_when_path_outside_root() {
        let got = compute_source_relative(
            "C:/elsewhere/file.xlsx",
            Some("D:/data/input/"),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn source_relative_does_not_match_root_prefix_alone() {
        // "D:/data/inputs/foo" must NOT match root "D:/data/input/" —
        // we only treat exact directory prefixes (slash boundary) as
        // matches. The trailing-slash normalisation handles this.
        let got = compute_source_relative(
            "D:/data/inputs/foo.xlsx",
            Some("D:/data/input"),
        );
        assert_eq!(got, None);
    }

    // ─── merge map / cell wrapping (rspan emission) ─────────────────
    //
    // The XLSX parser captures full RawMerge rectangles. The emission
    // layer used to drop everything except same-row colspans. After
    // schema=4 we now publish both `span` (colspan) and `rspan` on
    // anchor cells; covered (non-anchor) cells remain `null` and
    // consumers must reconstruct continuation by walking rows in order.

    use combycode_dpe_tool_xlsx::xlsx::RawMerge;

    fn rm(min_row: usize, min_col: usize, max_row: usize, max_col: usize) -> RawMerge {
        RawMerge { min_row, min_col, max_row, max_col }
    }

    #[test]
    fn merge_info_horizontal_only() {
        let info = build_merge_info(&[rm(0, 0, 0, 2)]);
        assert_eq!(info.len(), 1);
        assert_eq!(info.get(&(0, 0)), Some(&(3, 1)));
    }

    #[test]
    fn merge_info_vertical_only() {
        let info = build_merge_info(&[rm(2, 4, 5, 4)]);
        assert_eq!(info.len(), 1);
        assert_eq!(info.get(&(2, 4)), Some(&(1, 4)));
    }

    #[test]
    fn merge_info_two_dimensional() {
        let info = build_merge_info(&[rm(1, 1, 3, 4)]);
        assert_eq!(info.get(&(1, 1)), Some(&(4, 3)));
    }

    #[test]
    fn merge_info_skips_single_cell_noop_merge() {
        // A degenerate <mergeCell ref="A1:A1"/> exists in some workbooks;
        // it's not a real merge so we drop it rather than emit a wrapper
        // that says span:1, rspan:1.
        let info = build_merge_info(&[rm(0, 0, 0, 0)]);
        assert!(info.is_empty());
    }

    #[test]
    fn merge_info_handles_multiple_anchors() {
        let info = build_merge_info(&[
            rm(0, 0, 0, 1),
            rm(1, 0, 3, 0),
            rm(5, 5, 5, 5), // skipped
        ]);
        assert_eq!(info.len(), 2);
        assert_eq!(info.get(&(0, 0)), Some(&(2, 1)));
        assert_eq!(info.get(&(1, 0)), Some(&(1, 3)));
    }

    #[test]
    fn wrap_cell_passthrough_when_not_merged() {
        let v = wrap_cell_with_merge(json!("hello"), None);
        assert_eq!(v, json!("hello"));
    }

    #[test]
    fn wrap_cell_horizontal_merge_emits_span_only_backward_compatible() {
        // This is the legacy shape — vital that schema=3 consumers don't
        // see an unexpected rspan: it's omitted when rowspan == 1.
        let v = wrap_cell_with_merge(json!("Title"), Some((3, 1)));
        assert_eq!(v, json!({"span": 3, "v": "Title"}));
        // And explicitly no rspan key.
        assert!(v.get("rspan").is_none());
    }

    #[test]
    fn wrap_cell_vertical_merge_emits_rspan_only() {
        // Cells previously emitted as a bare value when they had a
        // pure rowspan now wrap into `{rspan, v}`. This is the only
        // backward-incompatible facet of schema=4.
        let v = wrap_cell_with_merge(json!("Group"), Some((1, 4)));
        assert_eq!(v, json!({"rspan": 4, "v": "Group"}));
        assert!(v.get("span").is_none());
    }

    #[test]
    fn wrap_cell_two_dimensional_merge_emits_both() {
        let v = wrap_cell_with_merge(json!(42), Some((2, 3)));
        assert_eq!(v, json!({"span": 2, "rspan": 3, "v": 42}));
    }

    #[test]
    fn wrap_cell_preserves_formula_tuple_inside_v() {
        // cell_to_json emits `[value, "=expr"]` for formula cells; the
        // wrapper must not destructure that — `.v` carries it verbatim.
        let v = wrap_cell_with_merge(json!([100, "=SUM(A1:A2)"]), Some((1, 2)));
        assert_eq!(v, json!({"rspan": 2, "v": [100, "=SUM(A1:A2)"]}));
    }

    #[test]
    fn wrap_cell_preserves_null_value_inside_v() {
        // Edge case: anchor cell with no value (e.g. a merge anchor that
        // happens to be empty) — `.v` must still be present and null.
        let v = wrap_cell_with_merge(json!(null), Some((2, 1)));
        assert_eq!(v, json!({"span": 2, "v": null}));
    }

    #[test]
    fn manifest_round_trip_serde() {
        let m = Manifest {
            sheets: vec![ManifestSheet {
                name: "Sheet 1".into(),
                safe_name: "Sheet_1".into(),
                rows: 42,
                cols: 7,
            }],
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sheets.len(), 1);
        assert_eq!(back.sheets[0].name, "Sheet 1");
        assert_eq!(back.sheets[0].safe_name, "Sheet_1");
        assert_eq!(back.sheets[0].rows, 42);
        assert_eq!(back.sheets[0].cols, 7);
    }
}
