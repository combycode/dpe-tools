//! Shared cell / row utility helpers (cached_value, is_text_cached,
//! is_total_row_structural, looks_like_date, etc.) and the public
//! `find_tables` entry that delegates to v3. The original v1
//! anchor-walk algorithm was removed in 2026-05-13; some helpers
//! below remain only because the unit-test suite in
//! `matcher_tests.rs` exercises them directly. Allow dead_code here
//! so unused helpers don't fail the lib build.

#![allow(dead_code)]

use std::collections::HashSet;
use serde_json::Value;
use crate::template::{CellPattern, Template};

#[cfg(test)]
#[path = "matcher_tests.rs"]
mod tests;

/// A found table.
#[derive(Debug)]
pub struct FoundTable {
    pub format: String,
    pub title: Option<String>,
    pub columns: Vec<String>,
    pub header_start: usize,
    pub data_start: usize,
    pub data_rows: Vec<usize>,
    pub col_start: usize,
    pub col_end: usize,
}

/// A Block = maximal non-empty row range. Within this range tables live.
/// Shared between v1 and v2 matchers (see matcher_v2.rs for the redesign).
#[derive(Debug, Clone)]
pub(crate) struct Block {
    pub(crate) row_start: usize,
    pub(crate) row_end: usize,
}

/// Public API: find all tables in a sheet. Thin wrapper that delegates
/// to the v3 (header-band, bound-and-carve) matcher. The earlier
/// anchor-walk algorithm was removed in 2026-05-13; this name is kept
/// for any external callers that imported it directly.
pub fn find_tables(sheet: &[Value], templates: &[Template]) -> Vec<FoundTable> {
    crate::matcher_v3::find_tables_v3(sheet, templates)
}

// ─── Phase 1: Find Blocks ───────────────────────────────────────────

pub(crate) fn find_blocks(sheet: &[Value]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < sheet.len() {
        // Skip empty rows
        while i < sheet.len() && is_null_or_empty(&sheet[i]) { i += 1; }
        if i >= sheet.len() { break; }

        let start = i;
        let mut empty_streak = 0;
        let mut last_non_empty = i;

        while i < sheet.len() {
            if is_null_or_empty(&sheet[i]) {
                empty_streak += 1;
                if empty_streak >= 5 { break; }
            } else {
                last_non_empty = i;
                empty_streak = 0;
            }
            i += 1;
        }
        blocks.push(Block { row_start: start, row_end: last_non_empty });
    }
    blocks
}
// ─── Helpers ────────────────────────────────────────────────────────

/// Extend col_end through contiguous non-null cells (no gaps) in the given row.
fn extend_col_end_contiguous(row: &Value, current_end: usize) -> usize {
    let Some(cells) = row.as_array() else { return current_end };
    let mut end = current_end;
    for (offset, cell) in cells.iter().enumerate().skip(current_end + 1) {
        if cell.is_null() { break; }
        end = offset;
    }
    end
}

/// Extend col_start LEFT through contiguous non-null cells (no gaps) in
/// the given row. Mirror of [`extend_col_end_contiguous`]. Used so a
/// title-merge that begins at col >= 1 doesn't strand legitimate
/// leading-column header cells (e.g. `Date` at col 0 with the title
/// merge starting at col 1).
fn extend_col_start_contiguous(row: &Value, current_start: usize) -> usize {
    let Some(cells) = row.as_array() else { return current_start };
    if current_start == 0 { return 0; }
    let mut start = current_start;
    let mut i = current_start - 1;
    loop {
        match cells.get(i) {
            Some(c) if !c.is_null() => start = i,
            _                       => break,
        }
        if i == 0 { break; }
        i -= 1;
    }
    start
}

pub fn is_null_or_empty(row: &Value) -> bool {
    match row {
        Value::Null => true,
        Value::Array(arr) => arr.is_empty() || arr.iter().all(|c| c.is_null()),
        _ => false,
    }
}

fn find_next_non_empty(sheet: &[Value], from: usize, to: usize) -> Option<usize> {
    (from..=to).find(|&r| !is_null_or_empty(&sheet[r]))
}

/// Get cached value from a cell (works for formulas [value, formula] too).
pub(crate) fn cached_value(cell: &Value) -> Option<&Value> {
    match cell {
        Value::Null => None,
        Value::Array(arr) if arr.len() == 2 => arr.first(),
        Value::Object(m) => m.get("v"),
        _ => Some(cell),
    }
}

pub(crate) fn is_text_cached(cell: &Value) -> bool {
    match cached_value(cell) {
        Some(Value::String(s)) => !looks_like_date(s),
        _ => false,
    }
}

pub(crate) fn is_number_or_date_cached(cell: &Value) -> bool {
    match cached_value(cell) {
        Some(Value::Number(_)) => true,
        Some(Value::String(s)) => {
            // ISO date
            s.len() >= 10 && s.chars().nth(4) == Some('-') && s.chars().nth(7) == Some('-')
        }
        _ => false,
    }
}

pub(crate) fn count_text_in_range(row: &Value, col_start: usize, col_end: usize) -> usize {
    let cells = match row.as_array() { Some(c) => c, None => return 0 };
    (col_start..=col_end.min(cells.len().saturating_sub(1)))
        .filter(|&i| cells.get(i).map(is_text_cached).unwrap_or(false))
        .count()
}

pub(crate) fn count_cells_in_range(row: &Value, col_start: usize, col_end: usize) -> usize {
    let cells = match row.as_array() { Some(c) => c, None => return 0 };
    (col_start..=col_end.min(cells.len().saturating_sub(1)))
        .filter(|&i| cells.get(i).map(|c| !c.is_null()).unwrap_or(false))
        .count()
}

fn is_all_text_in_range(row: &Value, col_start: usize, col_end: usize) -> bool {
    let cells = match row.as_array() { Some(c) => c, None => return false };
    let range_end = col_end.min(cells.len().saturating_sub(1));
    let mut text_count = 0;
    let mut non_text = 0;
    for i in col_start..=range_end {
        if let Some(cell) = cells.get(i) {
            if cell.is_null() { continue; }
            if is_text_cached(cell) { text_count += 1; }
            else { non_text += 1; }
        }
    }
    text_count >= 2 && non_text == 0
}

fn is_data_row(row: &Value, col_start: usize, col_end: usize) -> bool {
    let cells = match row.as_array() { Some(c) => c, None => return false };
    let range_end = col_end.min(cells.len().saturating_sub(1));
    for i in col_start..=range_end {
        if let Some(cell) = cells.get(i) {
            if is_number_or_date_cached(cell) { return true; }
            // Formula cells with non-text cached values
            if let Value::Array(arr) = cell {
                if arr.len() == 2 {
                    if let Some(Value::Number(_)) = arr.first() { return true }
                }
            }
        }
    }
    false
}

pub(crate) fn is_anchor_only_row(row: &Value, col_start: usize, col_end: usize) -> bool {
    let cells = match row.as_array() { Some(c) => c, None => return false };
    let range_end = col_end.min(cells.len().saturating_sub(1));
    let mut anchor_filled = false;
    let mut others_filled = 0;
    for i in col_start..=range_end {
        if let Some(cell) = cells.get(i) {
            if cell.is_null() { continue; }
            if i == col_start { anchor_filled = true; }
            else { others_filled += 1; }
        }
    }
    anchor_filled && others_filled == 0
}

/// True when `row` looks like an aggregate / total row, by STRUCTURAL
/// signals (label, aggregate formula, orphan-anchor pattern). Replaces
/// the old fill-density heuristic — that one inflated `typical` from
/// rate-sub-header / mixed-format rows that landed in the data walk
/// and silently dropped sparse daily rows (regression).
///
/// Three independent triggers, ANY of which classifies the row as a
/// total:
///
/// 1. **Label-anchor**: `cells[col_start]` is a string matching a
///    total-label pattern (`total:`, `subtotal`, `итого`, period
///    `MM.YYYY`, bare 4-digit year `2025`, …). See [`is_total_label`].
/// 2. **Aggregate formula**: any cell in the range has
///    `[value, "=…formula"]` shape AND the formula contains an
///    aggregate-function keyword (`SUM(`/`SUMIF(`/`SUBTOTAL(`/
///    `AVERAGE(` …). Manual sums of cell refs (`=B1+B2+B3`,
///    `='Sheet'!B1+'Sheet'!B2+'Sheet'!B3`) are explicitly NOT
///    classified — those appear in legitimate daily data rows when a
///    peripheral column manually sums sibling cells (regression). Per-row date formulas like `=A4+90` (single ref)
///    are also NOT classified.
/// 3. **Orphan-anchor**: `cells[col_start]` is null/blank AND ≥3 cells
///    past it are filled with numeric values. Catches month-subtotal
///    rows where the date column is blank because the row is a
///    roll-up.
///
/// Density is intentionally NOT used — sparse daily rows (just date +
/// one value) are structurally indistinguishable from sparse totals
/// once `typical` has been polluted by an outlier.
pub(crate) fn is_total_row_structural(row: &Value, col_start: usize, col_end: usize) -> bool {
    let cells = match row.as_array() { Some(c) => c, None => return false };
    let range_end = col_end.min(cells.len().saturating_sub(1));

    // (1) Label-anchor. Route through `cached_value` so a vertically
    //     merged total label -- emitted by xlsx schema=4 as
    //     `{rspan:N, v:"Итого"}` -- is detected just like a bare string.
    if let Some(s) = cells.get(col_start)
        .and_then(cached_value)
        .and_then(|v| v.as_str())
    {
        if is_total_label(s) { return true; }
    }

    // (2) Aggregate formula in any cell of the range.
    for i in col_start..=range_end {
        if let Some(cell) = cells.get(i) {
            if cell_has_aggregate_formula(cell) { return true; }
        }
    }

    // (3) Orphan-anchor: anchor cell is null/blank AND ≥3 numeric
    //     others. Use cached value so `[number, "=formula"]` shapes
    //     count as numeric. Same `cached_value` unwrap so an
    //     rspan-wrapped empty / null v isn't treated as "filled".
    let anchor_blank = cells.get(col_start).map(|c| {
        match cached_value(c) {
            None => true,
            Some(v) => v.is_null() || v.as_str().map(|s| s.trim().is_empty()).unwrap_or(false),
        }
    }).unwrap_or(true);
    if anchor_blank {
        let mut numeric_others = 0;
        for i in (col_start + 1)..=range_end {
            if let Some(cell) = cells.get(i) {
                if matches!(cached_value(cell), Some(Value::Number(_))) {
                    numeric_others += 1;
                    if numeric_others >= 3 { return true; }
                }
            }
        }
    }

    false
}

/// Total-label patterns: keywords (`total`, `subtotal`, `итого`,
/// `всего`, `sum`, `month`, `year`, …), period `MM.YYYY` /
/// `M.YYYY`, bare 4-digit year (`2025`, `2026`), or any short string
/// with a trailing colon. Trimming + Unicode-aware lowercasing.
///
/// Carefully NOT matched (would cause real data drops):
/// - ISO dates `2025-01-01` (has dashes, no all-digits-or-dot match).
/// - Pure numeric IDs like `1234567` (length != 4).
/// - Plain text like `Notes`, `Header`, etc. (no keyword, no colon).
fn is_total_label(s: &str) -> bool {
    let trimmed_with_colon = s.trim();
    if trimmed_with_colon.is_empty() { return false; }
    let trimmed = trimmed_with_colon.trim_end_matches(':').trim();
    if trimmed.is_empty() { return false; }

    let lower = trimmed.to_lowercase();

    // Known keywords. Exact match OR substring (catches "Grand total:",
    // "Year sum:" etc.).
    static KEYWORDS: &[&str] = &[
        "total", "totals", "subtotal", "sub-total", "grand total", "sum",
        "summary", "month", "year", "quarter", "period",
        // Cyrillic
        "итого", "всего", "сумма", "итог", "месяц", "год",
    ];
    for k in KEYWORDS {
        if lower == *k || lower.contains(k) { return true; }
    }

    // Period MM.YYYY / M.YYYY  +  bare 4-digit year.
    let bytes = trimmed.as_bytes();
    let all_digits_or_dot = !bytes.is_empty() && bytes.iter().all(|b| b.is_ascii_digit() || *b == b'.');
    if all_digits_or_dot {
        if trimmed.len() == 4 && bytes.iter().all(|b| b.is_ascii_digit()) {
            // bare year: 2025, 2026
            return true;
        }
        if trimmed.contains('.') {
            let parts: Vec<&str> = trimmed.split('.').collect();
            if parts.len() == 2
                && !parts[0].is_empty()
                && parts[0].chars().all(|c| c.is_ascii_digit())
                && parts[1].len() == 4
                && parts[1].chars().all(|c| c.is_ascii_digit())
            {
                return true; // 04.2025, 4.2025
            }
        }
    }

    // Short string with trailing colon — strong signal in tables. Not
    // matched: long sentences ending in ':' or anything > 15 chars.
    trimmed_with_colon.ends_with(':') && trimmed.chars().count() <= 15
}

/// True when `cell` has shape `[value, "=formula…"]` AND the formula
/// invokes a SUM-family aggregate function. Manual sums of cell
/// references (`=B1+B2+B3`, `='Sheet'!B1+B2+B3`) are intentionally
/// NOT classified — they appear in legitimate daily data rows when
/// a peripheral column manually sums sibling cells (regression). Real total rows are caught by the label-anchor
/// (check 1) or orphan-anchor (check 3) signals on the
/// `is_total_row_structural` decision.
fn cell_has_aggregate_formula(cell: &Value) -> bool {
    let arr = match cell.as_array() { Some(a) => a, None => return false };
    if arr.len() != 2 { return false; }
    let formula = match arr.get(1).and_then(|v| v.as_str()) {
        Some(s) if s.starts_with('=') => s,
        _ => return false,
    };

    // SUM-family keywords (case-insensitive). `=SUM(`, `=SUBTOTAL(`,
    // `=AVERAGE(` etc. are unambiguous aggregate signals — when they
    // appear in a row, that row is a roll-up.
    let upper = formula.to_ascii_uppercase();
    for kw in &["SUM(", "SUMIF(", "SUMIFS(", "SUMPRODUCT(",
                "SUBTOTAL(", "AGGREGATE(", "AVERAGE(", "AVERAGEIF("] {
        if upper.contains(kw) { return true; }
    }
    false
}

fn has_title_merge_in_range(row: &Value, col_start: usize, col_end: usize) -> bool {
    let cells = match row.as_array() { Some(c) => c, None => return false };
    let range_end = col_end.min(cells.len().saturating_sub(1));
    for i in col_start..=range_end {
        if let Some(cell) = cells.get(i) {
            if let Some(obj) = cell.as_object() {
                if let Some(span) = obj.get("span").and_then(|s| s.as_u64()) {
                    if span >= 3 { return true; }
                }
            }
        }
    }
    false
}

fn is_row_claimed_in_range(claimed: &HashSet<(usize, usize)>, row: usize, col_start: usize, col_end: usize) -> bool {
    let total = col_end - col_start + 1;
    let count = (col_start..=col_end).filter(|&c| claimed.contains(&(row, c))).count();
    count * 2 > total
}

fn find_text_groups(row: &Value) -> Vec<(usize, usize)> {
    let cells = match row.as_array() { Some(c) => c, None => return vec![] };
    let mut groups = Vec::new();
    let mut start: Option<usize> = None;
    let mut last = 0;
    for (i, cell) in cells.iter().enumerate() {
        if is_text_cached(cell) {
            if start.is_none() { start = Some(i); }
            last = i;
        } else {
            // Any non-text cell (null, number, etc.) ends the text group
            if let Some(s) = start {
                if last > s { groups.push((s, last)); }
                start = None;
            }
        }
    }
    if let Some(s) = start {
        if last > s { groups.push((s, last)); }
    }
    groups
}

// ─── Column Building ────────────────────────────────────────────────

fn build_columns(sheet: &[Value], header_rows: &[usize], col_start: usize, col_end: usize) -> Vec<String> {
    if header_rows.len() == 1 {
        return extract_row_texts(&sheet[header_rows[0]], col_start, col_end);
    }
    let parent = extract_row_texts_with_spans(&sheet[header_rows[0]], col_start, col_end);
    let sub = extract_row_texts(&sheet[header_rows[1]], col_start, col_end);
    let width = col_end - col_start + 1;
    (0..width).map(|i| {
        let p = parent.get(i).cloned().unwrap_or_default();
        let s = sub.get(i).cloned().unwrap_or_default();
        if s.is_empty() || s == p { p }
        else if p.is_empty() { s }
        else { format!("{} | {}", p, s) }
    }).collect()
}

pub(crate) fn extract_row_texts(row: &Value, col_start: usize, col_end: usize) -> Vec<String> {
    let width = col_end - col_start + 1;
    let mut result = vec![String::new(); width];
    let cells = match row.as_array() { Some(c) => c, None => return result };
    for i in col_start..=col_end {
        let out_i = i - col_start;
        if let Some(cell) = cells.get(i) {
            result[out_i] = match cached_value(cell) {
                Some(Value::String(s)) => s.trim().to_string(),
                _ => String::new(),
            };
        }
    }
    result
}

pub(crate) fn extract_row_texts_with_spans(row: &Value, col_start: usize, col_end: usize) -> Vec<String> {
    let width = col_end - col_start + 1;
    let mut result = vec![String::new(); width];
    let cells = match row.as_array() { Some(c) => c, None => return result };

    let mut col = 0;
    let mut skip_nulls = 0;

    for cell in cells {
        if col > col_end { break; }

        // Null placeholders under a prior merge: consume without advancing col
        // (the merge already advanced col past its full span).
        if cell.is_null() && skip_nulls > 0 { skip_nulls -= 1; continue; }
        skip_nulls = 0;

        if let Some(obj) = cell.as_object() {
            if let (Some(span), Some(v)) = (obj.get("span"), obj.get("v")) {
                let span_n = span.as_u64().unwrap_or(1) as usize;
                let text = v.as_str().unwrap_or("").trim().to_string();
                for i in 0..span_n {
                    let c = col + i;
                    if c >= col_start && c <= col_end {
                        result[c - col_start] = text.clone();
                    }
                }
                col += span_n;
                skip_nulls = span_n.saturating_sub(1);
                continue;
            }
        }

        if cell.is_null() { col += 1; continue; }

        if col >= col_start && col <= col_end {
            result[col - col_start] = match cell {
                Value::String(s) => s.trim().to_string(),
                Value::Array(arr) if arr.len() == 2 => {
                    match arr.first() {
                        Some(Value::String(s)) => s.trim().to_string(),
                        _ => String::new(),
                    }
                }
                _ => String::new(),
            };
        }
        col += 1;
    }
    result
}

pub fn extract_data_values(row: &Value, col_start: usize, col_end: usize) -> Vec<Value> {
    match row.as_array() {
        Some(cells) => (col_start..=col_end)
            .map(|i| cells.get(i).cloned().unwrap_or(Value::Null))
            .collect(),
        None => vec![Value::Null; col_end - col_start + 1],
    }
}

// ─── Data-pattern slide-matching (template-driven anchor) ──────────
//
// The matching template's `data*` row pattern provides a positional
// prefix that the table's leftmost data columns must satisfy. We
// take the leading run of strict (non-Any) cell types and slide the
// candidate `col_start` right within the column range until every
// strict type matches at its positional offset across a majority of
// sample data rows.
//
// `Empty` is treated as "this column must be null on most data rows"
// — it's still strict, but accepts only blanks. `Any` and `...` end
// the strict prefix; anything after them is unconstrained.

fn anchor_col_start_by_pattern(
    sheet:      &[Value],
    block:      &Block,
    template:   &Template,
    data_start: usize,
    col_start:  usize,
    col_end:    usize,
) -> Option<usize> {
    // Template with no `data*` row → no enforcement. Matches the
    // backwards-compat path for templates that only declare structure
    // (title / header) without a positional data pattern.
    let data = match template.data_pattern() {
        Some(d) => d,
        None    => return Some(col_start),
    };

    // Build the strict positional prefix: cells up to (but not
    // including) the first `Any`. `Empty` is kept — it's a strict
    // "must be null" predicate.
    let prefix: Vec<&CellPattern> = data.cells.iter()
        .take_while(|p| !matches!(p, CellPattern::Any))
        .collect();
    if prefix.is_empty() {
        // `data*: any ...` or empty pattern — no anchor enforcement.
        return Some(col_start);
    }

    // Sample data rows: walk forward from data_start, collect up to N
    // non-empty rows. We need ENOUGH non-empty rows for a meaningful
    // vote; sparse tables (very few non-null rows total) still get a
    // verdict from whatever's available.
    let sample = collect_sample_rows(sheet, data_start, block.row_end, col_start, col_end, 8);
    if sample.is_empty() {
        // No data rows in range — can't validate. Reject so we don't
        // produce a bogus header-only table.
        return None;
    }

    // Threshold: majority of sample rows must match each strict
    // position. Ties (sample.len() == 2 → threshold 1) are permissive
    // intentionally — sparse tables shouldn't be rejected on a quirk
    // row.
    let threshold = sample.len().div_ceil(2);

    for try_start in col_start..=col_end {
        // Does this candidate fit the strict prefix?
        let mut ok = true;
        for (offset, pat) in prefix.iter().enumerate() {
            let col = match try_start.checked_add(offset) {
                Some(c) if c <= col_end => c,
                _                       => { ok = false; break; }
            };
            let match_count = sample.iter().filter(|&&r| {
                let cell = sheet.get(r)
                    .and_then(|row| row.as_array())
                    .and_then(|cs| cs.get(col))
                    .unwrap_or(&Value::Null);
                cell_matches_pattern(cell, pat)
            }).count();
            if match_count < threshold { ok = false; break; }
        }
        if ok { return Some(try_start); }
    }
    None
}

/// Walk forward from `from` collecting up to `cap` row indices whose
/// content in [col_start, col_end] is non-empty. Used by the
/// pattern-anchor matcher to pick "real" data rows for type votes
/// (skip blank rows, rate-rows, etc.).
pub(crate) fn collect_sample_rows(
    sheet: &[Value],
    from: usize,
    to:   usize,
    col_start: usize,
    col_end:   usize,
    cap: usize,
) -> Vec<usize> {
    let mut out = Vec::new();
    // `r` indexes multiple structures (sheet, the count_cells helper);
    // iter-and-enumerate rewrite would obscure the intent.
    #[allow(clippy::needless_range_loop)]
    for r in from..=to {
        if out.len() >= cap { break; }
        if !is_null_or_empty(&sheet[r])
            && count_cells_in_range(&sheet[r], col_start, col_end) > 0
        {
            out.push(r);
        }
    }
    out
}

/// True when `cell` matches the given template cell pattern. Mirrors
/// the template grammar from `template.rs`. `Any` returns true for
/// everything (used when traversing past the strict prefix).
pub fn cell_matches_pattern(cell: &Value, pattern: &CellPattern) -> bool {
    match pattern {
        CellPattern::Any     => true,
        CellPattern::Empty   => is_null_or_blank(cell),
        CellPattern::Date    => is_date_cell(cell),
        CellPattern::Number  => is_number_cell(cell),
        CellPattern::Text    => is_text_cell(cell),
        CellPattern::Formula => is_formula_cell(cell),
        CellPattern::Merge   => is_merge_cell(cell),
    }
}

fn is_null_or_blank(cell: &Value) -> bool {
    match cached_value(cell) {
        None                   => true,
        Some(Value::Null)      => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        _                      => false,
    }
}

/// True when the cell's cached value parses as a date string. Recognised
/// formats (covering everything seen in the MOCKS corpus):
///   - ISO date `YYYY-MM-DD` (with optional time suffix `T…` or ` …`)
///   - ISO with slashes `YYYY/MM/DD`
///   - European `DD.MM.YYYY` (with optional time suffix)
///   - European with slashes `DD/MM/YYYY`
///
/// The check is structural (digit positions + separators); we don't
/// validate calendar validity (`13.13.2025` matches) — that's a job for
/// the data layer, not the structural anchor.
pub(crate) fn is_date_cell(cell: &Value) -> bool {
    match cached_value(cell) {
        Some(Value::String(s)) => looks_like_date(s),
        _ => false,
    }
}

pub(crate) fn looks_like_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 10 { return false; }
    let digits = |range: std::ops::Range<usize>|
        b.get(range).map(|sub| sub.iter().all(|c| c.is_ascii_digit())).unwrap_or(false);

    // YYYY-MM-DD or YYYY/MM/DD (4-2-2 with `-` / `/` separators)
    if (b[4] == b'-' || b[4] == b'/') && (b[7] == b'-' || b[7] == b'/')
        && digits(0..4) && digits(5..7) && digits(8..10)
    {
        return true;
    }
    // DD.MM.YYYY, DD/MM/YYYY, DD-MM-YYYY, or DD,MM,YYYY
    // (the comma form is a Russian-spreadsheet typo we see in real
    // sources, e.g. `16,12,2024`).
    if (b[2] == b'.' || b[2] == b'/' || b[2] == b'-' || b[2] == b',')
        && (b[5] == b'.' || b[5] == b'/' || b[5] == b'-' || b[5] == b',')
        && digits(0..2) && digits(3..5) && digits(6..10)
    {
        return true;
    }
    false
}

fn is_number_cell(cell: &Value) -> bool {
    matches!(cached_value(cell), Some(Value::Number(_)))
}

fn is_text_cell(cell: &Value) -> bool {
    match cached_value(cell) {
        Some(Value::String(s)) => {
            if s.trim().is_empty() { return false; }
            // Date strings (any supported format) are `Date`, not `Text`.
            !looks_like_date(s)
        }
        _ => false,
    }
}

fn is_formula_cell(cell: &Value) -> bool {
    match cell {
        Value::Array(a) if a.len() == 2 => {
            matches!(a.get(1), Some(Value::String(s)) if s.starts_with('='))
        }
        _ => false,
    }
}

fn is_merge_cell(cell: &Value) -> bool {
    match cell {
        Value::Object(m) => m.get("span").and_then(|s| s.as_u64()).map(|n| n >= 2).unwrap_or(false),
        _ => false,
    }
}

// Public wrappers for main.rs (detect mode still uses legacy SUM detection)
pub fn is_null_or_empty_pub(row: &Value) -> bool { is_null_or_empty(row) }
pub fn is_sum_formula_pub(v: &Value) -> bool {
    if let Value::Array(arr) = v {
        if arr.len() == 2 {
            if let Some(f) = arr.get(1).and_then(|f| f.as_str()) {
                let upper = f.to_uppercase();
                return upper.contains("SUM(") || upper.contains("SUBTOTAL(");
            }
        }
    }
    false
}
