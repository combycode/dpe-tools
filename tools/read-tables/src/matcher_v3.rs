//! Table matcher v3 -- header-band first, template-validated.
//!
//! Rules:
//!
//! 1. A table is one HEADER BAND with no internal column gap. A gap is
//!    a column where every header-band row has no touched cell.
//! 2. A header band is a contiguous run of HEADER ROWS. A header row
//!    has touched cells where the majority are text labels (counting
//!    text strings, rspan/cspan-anchor text v, and rspan-coverage from
//!    above where the anchor's v is text).
//! 3. Within a header segment (between column gaps), the table's
//!    anchor column is the leftmost segment column at which the
//!    template's data prefix (e.g. `date` for `data*: date any+`)
//!    matches at least one row in the data range below the band.
//!    Segments with no anchor are skipped (no table follows the
//!    template).
//! 4. The data range extends from band_end+1 until the next header
//!    band in the same column range OR the end of the block.
//! 5. The title row is the row directly above the band when that row
//!    contains a horizontal span covering the segment column range
//!    and no bare values outside it.

use std::collections::HashMap;

use serde_json::Value;

use crate::matcher::{
    Block, FoundTable, cached_value,
    cell_matches_pattern,
    count_cells_in_range,
    find_blocks,
    is_anchor_only_row,
    is_total_row_structural,
};
use crate::template::{CellPattern, Template};

/// `(covered_row, covered_col) -> (anchor_row, anchor_col)`. Covers
/// both horizontal and vertical merge rectangles; anchor cells themselves
/// are NOT in the map (they carry the value).
pub(crate) type MergeCov = HashMap<(usize, usize), (usize, usize)>;

/// Public entry.
pub fn find_tables_v3(sheet: &[Value], templates: &[Template]) -> Vec<FoundTable> {
    let merged = build_merge_coverage(sheet);
    let blocks = find_blocks(sheet);
    let debug = std::env::var("READ_TABLES_V3_DEBUG").ok().as_deref() == Some("1");

    // 1. Pre-pass: find header bands in every block, accumulate them
    //    GLOBALLY. Then data ranges can span across the empty-row
    //    gaps that `find_blocks` inserts (e.g. a sheet that
    //    has its header at rows 0-2 and a 125-row gap before data
    //    starts at row 128 -- V1 handles this with
    //    `try_extend_into_next_block`; we just treat the sheet as
    //    one virtual block for data extent).
    let mut all_bands: Vec<(usize, usize, usize, usize)> = Vec::new();
    let sheet_last = sheet.len().saturating_sub(1);
    for block in &blocks {
        let max_col = match max_col_in_block(sheet, &merged, block) {
            Some(m) => m,
            None    => continue,
        };
        let row_runs: Vec<Vec<(usize, usize)>> = (block.row_start..=block.row_end)
            .map(|r| header_runs_in_row(sheet, &merged, r, max_col))
            .collect();
        let bands = group_runs_into_bands(&row_runs, block.row_start);
        if debug {
            eprintln!("{{\"type\":\"log\",\"level\":\"debug\",\"msg\":\"v3 block rows={}-{} bands={}\"}}",
                block.row_start, block.row_end, bands.len());
            for b in &bands {
                eprintln!("{{\"type\":\"log\",\"level\":\"debug\",\"msg\":\"  band rows={}-{} cols={}-{}\"}}", b.0, b.1, b.2, b.3);
            }
        }
        all_bands.extend(bands);
    }
    all_bands.sort_by_key(|b| (b.0, b.2));

    // 2. For each band, data range goes from band_bot+1 to the next
    //    OVERLAPPING band's row_start - 1, or the LAST sheet row.
    //    Crossing block boundaries is fine -- empty rows in the
    //    middle get skipped by the data-row walk's "no content"
    //    check inside build_table_for_segment.
    let mut tables = Vec::new();
    for (i, &(rt, rb, cs, ce)) in all_bands.iter().enumerate() {
        let mut data_end = sheet_last;
        for &(rt2, _, cs2, ce2) in &all_bands[i + 1..] {
            let overlaps = cs <= ce2 && cs2 <= ce;
            if overlaps && rt2 > rb {
                data_end = rt2 - 1;
                break;
            }
        }
        if let Some(t) = build_table_for_segment(
            sheet, &merged, rt, rb, cs, ce, rb + 1, data_end, templates,
        ) {
            tables.push(t);
        }
    }
    tables.sort_by_key(|t| (t.header_start, t.col_start));
    tables
}

// ─── Coverage map ────────────────────────────────────────────────────

fn build_merge_coverage(sheet: &[Value]) -> MergeCov {
    let mut cov = MergeCov::new();
    for (r, row) in sheet.iter().enumerate() {
        let Some(cells) = row.as_array() else { continue };
        for (c, cell) in cells.iter().enumerate() {
            let Some(obj) = cell.as_object() else { continue };
            let cspan = obj.get("span").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            let rspan = obj.get("rspan").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            if cspan <= 1 && rspan <= 1 { continue; }
            for rr in r..(r + rspan) {
                for cc in c..(c + cspan) {
                    if rr == r && cc == c { continue; }
                    cov.entry((rr, cc)).or_insert((r, c));
                }
            }
        }
    }
    cov
}

fn is_touched(sheet: &[Value], merged: &MergeCov, r: usize, c: usize) -> bool {
    if merged.contains_key(&(r, c)) { return true; }
    let cell = sheet.get(r).and_then(|row| row.as_array()).and_then(|cs| cs.get(c));
    matches!(cell, Some(v) if !v.is_null())
}

fn max_col_in_block(sheet: &[Value], merged: &MergeCov, block: &Block) -> Option<usize> {
    let mut max_c: Option<usize> = None;
    for r in block.row_start..=block.row_end {
        if let Some(cells) = sheet.get(r).and_then(|row| row.as_array()) {
            for (c, cell) in cells.iter().enumerate() {
                if !cell.is_null() {
                    max_c = Some(max_c.map(|m| m.max(c)).unwrap_or(c));
                }
            }
        }
    }
    for &(r, c) in merged.keys() {
        if r >= block.row_start && r <= block.row_end {
            max_c = Some(max_c.map(|m| m.max(c)).unwrap_or(c));
        }
    }
    max_c
}

// ─── Header runs ─────────────────────────────────────────────────────

/// Find header runs in row `r` within columns `[0, max_col]`.
/// Strategy:
///   1. Skip the row entirely if it's a total/period marker row.
///   2. Walk the row, partition into TOUCHED SEGMENTS (contiguous runs
///      of touched cells separated by null gaps with no coverage).
///   3. For each segment, check:
///      - has any date cell? → DATA segment, no run emitted
///      - has any header label cell? → emit (seg_start, seg_end)
///      - else → skip
///
/// Per-segment date check is critical for side-by-side tables sharing
/// a row (e.g. a sheet whose row 4 has dates in cols 0-5 AND table-2
/// header content in cols 8-13).
fn header_runs_in_row(sheet: &[Value], merged: &MergeCov, r: usize, max_col: usize) -> Vec<(usize, usize)> {
    let Some(cells) = sheet.get(r).and_then(|row| row.as_array()) else { return vec![] };
    // Iterate up to `max_col`, NOT `cells.len()-1`. A row containing
    // only a wide span anchor (e.g. cells=[{span:7,
    // v:"Section..."}]) has cells.len()=1 but the span covers cols 0-6.
    // We need to read coverage at cols 1-6 to form the proper run.
    let top = max_col;

    // Fix (4): if this row contains at least one cspan>=2 anchor (i.e.
    // could be a title row), bare own cells (cspan=1, rspan=1) on it
    // are noise -- treat them as non-touched so they don't extend a
    // run or contribute to label_count. Anchor cells and their
    // coverage form runs as usual.
    let row_has_title_anchor = cells.iter().any(|cell| {
        cell.as_object()
            .and_then(|o| o.get("span"))
            .and_then(|s| s.as_u64())
            .map(|cs| cs >= 2)
            .unwrap_or(false)
    });

    // Pre-pass: is the WHOLE row a header? i.e. every non-null own
    // cell across [0, max_col] is a header label AND no date anywhere.
    // Rows that pass this test admit bare-string segments as runs;
    // rows that fail it require span/rspan anchors INSIDE the segment.
    let row_is_full_header = is_all_header_row(cells, top);

    let mut out = Vec::new();
    let mut seg_start: Option<usize> = None;
    let mut seg_end: usize = 0;
    let mut seg_has_date = false;
    let mut seg_label_count: usize = 0;
    let mut seg_has_span_anchor = false;

    // Fix (5): emit() now also checks is_total_row_structural restricted
    // to the segment's own col range. A row whose segment looks like a
    // totals/period-marker row WITHIN the segment is rejected. This is
    // tighter than the previous full-row pre-check (which used
    // `[0, max_col]`) and lets a row with header labels in one col
    // range and unrelated rate-constants outside still emit its header
    // run -- e.g. a row with sub-headers in cols 6-11 + numeric
    // constants 0.025/0.015/0.04 at cols 19-21; the constants are
    // not in the segment so the segment's structural check returns
    // false and the sub-header run is correctly emitted.
    let emit = |start_opt: Option<usize>, end: usize, has_date: bool, label_count: usize, has_span: bool, row_is_full: bool, sheet: &[Value], r: usize, out: &mut Vec<(usize, usize)>| {
        if let Some(s) = start_opt {
            if end > s && label_count >= 2 && !has_date && (row_is_full || has_span) {
                if let Some(row) = sheet.get(r) {
                    if is_total_row_structural(row, s, end) { return; }
                }
                out.push((s, end));
            }
        }
    };

    for c in 0..=top {
        let cell = cells.get(c).unwrap_or(&Value::Null);
        let (mut touched, label_here, date_here) = cell_kind(cell, merged, sheet, r, c);
        let span_anchor_here = is_span_or_rspan_anchor_with_text(cell);
        // Fix (4): bare own cells on a title-candidate row are noise.
        // Own non-null + not an object (string / number / bool) AND
        // this row has at least one cspan>=2 anchor → drop it from
        // segment formation.
        if touched && row_has_title_anchor
            && !cell.is_null() && cell.as_object().is_none()
        {
            touched = false;
        }
        if touched {
            if seg_start.is_none() {
                seg_start = Some(c);
                seg_has_date = false;
                seg_label_count = 0;
                seg_has_span_anchor = false;
            }
            seg_end = c;
            if label_here       { seg_label_count    += 1;    }
            if date_here        { seg_has_date        = true; }
            if span_anchor_here { seg_has_span_anchor = true; }
        } else {
            emit(seg_start.take(), seg_end, seg_has_date, seg_label_count, seg_has_span_anchor, row_is_full_header, sheet, r, &mut out);
            seg_has_date = false;
            seg_label_count = 0;
            seg_has_span_anchor = false;
        }
    }
    emit(seg_start, seg_end, seg_has_date, seg_label_count, seg_has_span_anchor, row_is_full_header, sheet, r, &mut out);
    out
}

/// True iff the row is "header-dominant" across cols [0, top]: no
/// date cells anywhere, and header-label own cells outnumber any
/// non-label non-null own cells. A row with 7 text headers and 1
/// numeric ID (e.g. an ID like 12345 in col 1 alongside text
/// labels) is admitted as a header row; a row
/// with mixed text + date + number (e.g. a data row with description
/// columns) is not.
fn is_all_header_row(cells: &[Value], top: usize) -> bool {
    let mut labels = 0usize;
    let mut non_label = 0usize;
    // Iterate up to `top` regardless of row array length. Out-of-range
    // is `Value::Null` and just skipped.
    for c in 0..=top {
        let cell = cells.get(c).unwrap_or(&Value::Null);
        if cell.is_null() { continue; }
        if let Some(s) = cached_value(cell).and_then(|v| v.as_str()) {
            if crate::matcher::looks_like_date(s) { return false; }
        }
        if is_header_label_cell(cell) { labels += 1; }
        else                          { non_label += 1; }
    }
    labels >= 2 && labels > non_label
}

/// True iff the cell is a span- or rspan-anchor whose `v` is a string.
fn is_span_or_rspan_anchor_with_text(cell: &Value) -> bool {
    let Some(obj) = cell.as_object() else { return false };
    let cspan = obj.get("span").and_then(|s| s.as_u64()).unwrap_or(1);
    let rspan = obj.get("rspan").and_then(|s| s.as_u64()).unwrap_or(1);
    if cspan < 2 && rspan < 2 { return false; }
    obj.get("v").and_then(|v| v.as_str())
        .map(|s| !crate::matcher::looks_like_date(s))
        .unwrap_or(false)
}

/// Classify a single cell for the purposes of header-run scanning.
/// Returns `(touched, is_header_label, is_date)`.
fn cell_kind(cell: &Value, merged: &MergeCov, sheet: &[Value], r: usize, c: usize) -> (bool, bool, bool) {
    if !cell.is_null() {
        let label = is_header_label_cell(cell);
        let date = cached_value(cell)
            .and_then(|v| v.as_str())
            .map(crate::matcher::looks_like_date)
            .unwrap_or(false);
        return (true, label, date);
    }
    if let Some(&(ar, ac)) = merged.get(&(r, c)) {
        let anchor = sheet.get(ar).and_then(|row| row.as_array())
            .and_then(|cs| cs.get(ac)).unwrap_or(&Value::Null);
        let label = is_header_label_cell(anchor);
        // Coverage doesn't introduce a date even if the anchor's
        // cached value happens to be one (very rare in practice).
        return (true, label, false);
    }
    (false, false, false)
}

/// Group rows with header runs into vertical bands. Two adjacent
/// rows belong to the same band when at least one of their runs
/// overlaps with at least one of the other's runs; the band's col
/// range is the UNION of those overlapping runs -- per the rule
/// "gap = empty in ALL header rows simultaneously", a column that
/// appears in any participating row's header run belongs to the band.
fn group_runs_into_bands(row_runs: &[Vec<(usize, usize)>], block_start: usize) -> Vec<(usize, usize, usize, usize)> {
    let mut bands: Vec<(usize, usize, usize, usize)> = Vec::new();
    for (i, runs) in row_runs.iter().enumerate() {
        let r = block_start + i;
        for &(rs, re) in runs {
            let mut extended = false;
            for b in bands.iter_mut() {
                if b.1 + 1 == r && b.2 <= re && rs <= b.3 {
                    b.1 = r;
                    b.2 = b.2.min(rs);
                    b.3 = b.3.max(re);
                    extended = true;
                    break;
                }
            }
            if !extended {
                bands.push((r, r, rs, re));
            }
        }
    }
    bands
}

/// True iff a cell's shape says "header label": any non-date string
/// reachable via `cached_value`. Covers bare strings, span/rspan
/// anchors with text `v`, AND formula tuples whose cached value is
/// text (e.g. `['File Date', '=IFERROR(...)']` -- a header label
/// imported via a formula). Data-row error cells like `["#REF!",
/// "=..."]` also LOOK like text labels by this check, but rows
/// containing them have date cells too, which `has_date` filters out
/// at the row level.
fn is_header_label_cell(cell: &Value) -> bool {
    cached_value(cell)
        .and_then(|v| v.as_str())
        .map(|s| !crate::matcher::looks_like_date(s))
        .unwrap_or(false)
}


// ─── Build one table from a header segment ───────────────────────────

#[allow(clippy::too_many_arguments)]
fn build_table_for_segment(
    sheet:     &[Value],
    merged:    &MergeCov,
    band_top:  usize,
    band_bot:  usize,
    col_start: usize,
    col_end:   usize,
    data_top:  usize,
    data_bot:  usize,
    templates: &[Template],
) -> Option<FoundTable> {
    // Find anchor column: leftmost col in [col_start, col_end] where
    // the template's data prefix matches >= 1 row in [data_top, data_bot].
    let (anchor_col, used_template) = find_anchor_col_and_template(
        sheet, col_start, col_end, data_top, data_bot, templates,
    )?;
    let _ = used_template;

    // Title detection: either ABOVE the band (the classic title
    // pattern -- title row sits above the header text rows) or AT
    // the TOP of the band when the band's top row is itself a
    // title-only row (e.g. row 3 IS the title for the
    // table whose headers run at rows 4-5).
    let (title_row, header_rows): (Option<usize>, Vec<usize>) = {
        if let Some(tr) = title_row_above(sheet, band_top, col_start, col_end) {
            (Some(tr), (band_top..=band_bot).collect())
        } else if is_title_only_in_segment(sheet, merged, band_top, col_start, col_end) {
            // Title at band top. If band has additional rows they become
            // the header band; if it's a single-row band (title alone,
            // followed directly by data with no per-column headers,
            // e.g. `ABC 1234 100 K`), the header
            // band is empty and columns will be blank labels.
            let hdr: Vec<usize> = if band_bot > band_top {
                ((band_top + 1)..=band_bot).collect()
            } else {
                Vec::new()
            };
            (Some(band_top), hdr)
        } else {
            (None, (band_top..=band_bot).collect())
        }
    };
    let title = title_row.and_then(|tr| extract_title_text(sheet, tr, col_start, col_end));

    // Walk data rows: collect non-empty non-total non-anchor-only
    // rows in [data_top, data_bot] that have some content in the
    // segment column range.
    let mut data_rows: Vec<usize> = Vec::new();
    for r in data_top..=data_bot {
        let row = sheet.get(r).unwrap_or(&Value::Null);
        if matches!(row, Value::Null) { continue; }
        if !row_has_content_in_range(sheet, merged, r, col_start, col_end) { continue; }
        if is_total_row_structural(row, anchor_col, col_end) { continue; }
        if is_anchor_only_row(row, anchor_col, col_end) { continue; }
        if count_cells_in_range(row, anchor_col, col_end) == 0 { continue; }
        data_rows.push(r);
    }
    if data_rows.is_empty() { return None; }

    let columns = build_columns_for_band(sheet, merged, &header_rows, col_start, col_end, title_row);

    let format = if title_row.is_some() { "titled".to_string() } else { "flat".to_string() };

    Some(FoundTable {
        format,
        title,
        columns,
        header_start: title_row.unwrap_or(band_top),
        data_start: *data_rows.first().unwrap(),
        data_rows,
        col_start: anchor_col,
        col_end,
    })
}

fn row_has_content_in_range(
    sheet: &[Value], merged: &MergeCov, r: usize, col_start: usize, col_end: usize,
) -> bool {
    for c in col_start..=col_end {
        if is_touched(sheet, merged, r, c) { return true; }
    }
    false
}

fn find_anchor_col_and_template<'a>(
    sheet:     &[Value],
    col_start: usize,
    col_end:   usize,
    data_top:  usize,
    data_bot:  usize,
    templates: &'a [Template],
) -> Option<(usize, &'a Template)> {
    for tpl in templates {
        let data_pattern = match tpl.data_pattern() {
            Some(p) => p,
            None    => continue,
        };
        let prefix: Vec<&CellPattern> = data_pattern.cells.iter()
            .take_while(|p| !matches!(p, CellPattern::Any))
            .collect();
        if prefix.is_empty() {
            // Template with no strict prefix (e.g. `data*: any ...`)
            // — accept any row in the data range as data. Anchor =
            // segment's leftmost column.
            return Some((col_start, tpl));
        }
        for c in col_start..=col_end {
            for r in data_top..=data_bot {
                if row_matches_data_prefix(sheet, r, c, &prefix, col_end) {
                    return Some((c, tpl));
                }
            }
        }
    }
    None
}

fn row_matches_data_prefix(
    sheet:   &[Value],
    r:       usize,
    col:     usize,
    prefix:  &[&CellPattern],
    col_end: usize,
) -> bool {
    let Some(cells) = sheet.get(r).and_then(|row| row.as_array()) else { return false };
    for (offset, pat) in prefix.iter().enumerate() {
        let c = match col.checked_add(offset) {
            Some(cc) if cc <= col_end => cc,
            _ => return false,
        };
        let cell = cells.get(c).unwrap_or(&Value::Null);
        if !cell_matches_pattern(cell, pat) { return false; }
    }
    true
}

// ─── Title row above the header band ────────────────────────────────

/// A title row exists at `band_top - 1` iff that row contains:
///   - EXACTLY ONE horizontal span (cspan >= 2, rspan == 1) overlapping
///     the segment (multi-span rows are multi-section header rows,
///     not title rows)
///   - all other non-null cells in segment range are rspan anchors
///     (cspan == 1, rspan >= 2) -- they're column-header anchors that
///     extend down into the band
///   - no bare text, no bare numbers, no 2D merges
fn title_row_above(sheet: &[Value], band_top: usize, col_start: usize, col_end: usize) -> Option<usize> {
    if band_top == 0 { return None; }
    let candidate = band_top - 1;
    let row = sheet.get(candidate)?.as_array()?;
    let top = col_end.min(row.len().saturating_sub(1));
    let mut h_span_count = 0usize;
    let mut span_covers_segment = false;
    for c in col_start..=top {
        let cell = row.get(c).unwrap_or(&Value::Null);
        if cell.is_null() { continue; }
        // Bare cells (own string / number / bool) on the title row are
        // noise -- skip them, same rule as is_title_only_in_segment.
        let Some(obj) = cell.as_object() else { continue };
        let cspan = obj.get("span").and_then(|s| s.as_u64()).unwrap_or(1) as usize;
        let rspan = obj.get("rspan").and_then(|s| s.as_u64()).unwrap_or(1) as usize;
        if cspan >= 2 {
            h_span_count += 1;
            if c <= col_end && c + cspan > col_start {
                span_covers_segment = true;
            }
            continue;
        }
        if cspan == 1 && rspan >= 2 { continue; }
        return None;
    }
    if h_span_count >= 1 && span_covers_segment { Some(candidate) } else { None }
}

/// True iff row `row_idx` is a "title" row within the column range
/// `[col_start, col_end]`: it contains EXACTLY ONE horizontal span
/// anchor (cspan >= 2, rspan == 1), and every other own non-null
/// cell in the segment is an rspan anchor (cspan == 1, rspan >= 2).
/// Bare-text and bare-number cells disqualify -- those make the row
/// a header/data row, not a title. Multi-span rows (multiple
/// horizontal spans like `Group A`+`Group B` group headers) are NOT
/// titles either; they're multi-section header rows.
///
/// rspan companions (with any v type, text or numeric) are tolerated
/// because they represent column-header anchors that just happen to
/// have their anchor in the title row -- they extend downward into
/// the header band.
fn is_title_only_in_segment(
    sheet:     &[Value],
    _merged:   &MergeCov,
    row_idx:   usize,
    col_start: usize,
    col_end:   usize,
) -> bool {
    let Some(row) = sheet.get(row_idx).and_then(|r| r.as_array()) else { return false };
    let top = col_end.min(row.len().saturating_sub(1));
    let mut h_span_count = 0usize;
    for c in col_start..=top {
        let cell = row.get(c).unwrap_or(&Value::Null);
        if cell.is_null() { continue; }
        // Bare cells (cspan=1, rspan=1: own string / number / bool) in
        // the title row are noise -- skip them so the title still
        // qualifies. They're discarded entirely by build_columns_for_band
        // too (a bare cell on the title row contributes nothing).
        let Some(obj) = cell.as_object() else { continue };
        let cspan = obj.get("span").and_then(|s| s.as_u64()).unwrap_or(1) as usize;
        let rspan = obj.get("rspan").and_then(|s| s.as_u64()).unwrap_or(1) as usize;
        // Title anchor: cspan >= 2 (covers multiple cols). rspan may
        // be any value: rspan == 1 is the classic title row; rspan
        // >= 2 is a 2D-merge "group title" (e.g.
        // `{span:4, rspan:2, v:'Group (A)'}`).
        if cspan >= 2 { h_span_count += 1; continue; }
        if cspan == 1 && rspan >= 2 { continue; }   // rspan companion -- column header anchor
        return false;
    }
    // Allow >= 1 horizontal spans. Multi-span title rows like
    // a row 0 like (`'12345 SECTION'` cspan=15 + `'ABC'`
    // cspan=4 contiguously) are treated as ONE table with the
    // largest span as the title; the secondary spans are skipped
    // (safer than merging into headers
    // because the merged labels would break downstream template
    // normalisation).
    h_span_count >= 1
}

fn extract_title_text(sheet: &[Value], title_row: usize, col_start: usize, col_end: usize) -> Option<String> {
    let row = sheet.get(title_row)?.as_array()?;
    let top = col_end.min(row.len().saturating_sub(1));
    let mut best: Option<(usize, &Value)> = None;
    for c in col_start..=top {
        let cell = row.get(c)?;
        let Some(obj) = cell.as_object() else { continue };
        let cspan = obj.get("span").and_then(|s| s.as_u64()).unwrap_or(1) as usize;
        if cspan < 2 { continue; }      // any rspan tolerated (2D-merge titles OK)
        let Some(v) = obj.get("v") else { continue };
        if best.map(|(b, _)| cspan > b).unwrap_or(true) {
            best = Some((cspan, v));
        }
    }
    let v = best?.1;
    // Accept string OR numeric titles (some sheets title tables with
    // bare numeric IDs such as `1234` or `56789`). Empty
    // strings collapse to None so the caller knows the title row
    // exists but carries no readable text.
    let s = match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if s.is_empty() { None } else { Some(s) }
}

// ─── Column label composition ────────────────────────────────────────

fn build_columns_for_band(
    sheet:       &[Value],
    merged:      &MergeCov,
    header_rows: &[usize],
    col_start:   usize,
    col_end:     usize,
    title_row:   Option<usize>,
) -> Vec<String> {
    let width = col_end - col_start + 1;
    let mut out = vec![String::new(); width];
    for c in col_start..=col_end {
        let mut parts: Vec<String> = Vec::new();
        for &hr in header_rows {
            let cell = sheet.get(hr).and_then(|r| r.as_array())
                .and_then(|cs| cs.get(c)).unwrap_or(&Value::Null);
            let text = if !cell.is_null() {
                // Own cell. If this row is the title row AND the cell
                // is the title's cspan>=2 anchor itself, skip it -- the
                // title text doesn't belong in the column label. rspan
                // companions in the title row (cspan=1, rspan>=2) ARE
                // column-header anchors and DO contribute their text.
                // Bare cells (cspan=1, rspan=1: own string/number) on
                // the title row are noise -- skip them too.
                let is_title_row = Some(hr) == title_row;
                let cspan = cell.as_object()
                    .and_then(|o| o.get("span"))
                    .and_then(|s| s.as_u64())
                    .unwrap_or(1);
                let is_bare_own = cell.as_object().is_none();
                if is_title_row && (cspan >= 2 || is_bare_own) {
                    String::new()
                } else {
                    cached_value(cell).and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string()).unwrap_or_default()
                }
            } else if let Some(&(ar, ac)) = merged.get(&(hr, c)) {
                // Coverage. Skip ONLY if the anchor is the title's
                // cspan>=2 in the title row. rspan companions in the
                // title row (cspan=1, rspan>=2) extend down into the
                // header band and their text IS a column label
                // (e.g. `{rspan:3, v:"TOTAL"}` at col 12).
                let anchor = sheet.get(ar).and_then(|r| r.as_array())
                    .and_then(|cs| cs.get(ac)).unwrap_or(&Value::Null);
                let anchor_cspan = anchor.as_object()
                    .and_then(|o| o.get("span"))
                    .and_then(|s| s.as_u64())
                    .unwrap_or(1);
                let is_title_cspan = Some(ar) == title_row && anchor_cspan >= 2;
                if is_title_cspan {
                    String::new()
                } else {
                    cached_value(anchor).and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string()).unwrap_or_default()
                }
            } else {
                String::new()
            };
            if !text.is_empty() && !parts.iter().any(|p| p == &text) {
                parts.push(text);
            }
        }
        out[c - col_start] = parts.join(" | ");
    }
    out
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn coverage_horizontal_anchor_covers_right_neighbours() {
        let sheet = vec![json!([{"span": 3, "v": "X"}, null, null, "after"])];
        let cov = build_merge_coverage(&sheet);
        assert!(cov.contains_key(&(0, 1)));
        assert!(cov.contains_key(&(0, 2)));
        assert!(!cov.contains_key(&(0, 0)));
        assert!(!cov.contains_key(&(0, 3)));
    }

    #[test]
    fn coverage_vertical_anchor_covers_below() {
        let sheet = vec![
            json!([{"rspan": 3, "v": "Date"}]),
            json!([null]),
            json!([null]),
        ];
        let cov = build_merge_coverage(&sheet);
        assert_eq!(cov.get(&(1, 0)), Some(&(0, 0)));
        assert_eq!(cov.get(&(2, 0)), Some(&(0, 0)));
    }

    #[test]
    fn header_runs_split_on_gap() {
        // Row 1: text cells in cols 0-1, gap at col 2, text in cols 3-4.
        // Run-finding requires a header-dominant row (no dates, all
        // own cells header labels). All 4 own cells are bare text, so
        // is_all_header_row passes; the gap at col 2 splits into two
        // runs (0-1) and (3-4).
        let sheet = vec![json!(["A", "B", null, "C", "D"])];
        let merged = build_merge_coverage(&sheet);
        let runs = header_runs_in_row(&sheet, &merged, 0, 4);
        assert_eq!(runs, vec![(0, 1), (3, 4)]);
    }

    #[test]
    fn header_runs_no_gap_one_run() {
        let sheet = vec![json!(["A", "B", "C"])];
        let merged = build_merge_coverage(&sheet);
        let runs = header_runs_in_row(&sheet, &merged, 0, 2);
        assert_eq!(runs, vec![(0, 2)]);
    }

    #[test]
    fn header_runs_data_row_with_date_emits_nothing() {
        // A row with a date cell at col 0 is a data row -- its
        // trailing text cells must NOT form a header run (the date
        // disqualifies the segment containing it).
        let sheet = vec![
            json!(["Date", "Amount", "Notes"]),
            json!(["2024-01-01", 100, "trailing text"]),
        ];
        let merged = build_merge_coverage(&sheet);
        let header_runs = header_runs_in_row(&sheet, &merged, 0, 2);
        let data_runs = header_runs_in_row(&sheet, &merged, 1, 2);
        assert_eq!(header_runs, vec![(0, 2)]);
        assert_eq!(data_runs, Vec::<(usize, usize)>::new(),
            "data row segment containing a date must not produce a header run");
    }
}
