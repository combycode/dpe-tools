//! Unit tests for matcher.rs
//!
//! Organised by subject:
//!   - cell_types    — cached value extraction, text/number/date classification
//!   - row_shape     — per-row predicates (header/data/total/anchor-only)
//!   - counts        — text/cell counts, anchor-only edges
//!   - ranges        — title merges, text groups, contiguous extension
//!   - blocks        — block detection
//!   - anchors       — title anchor and header-row anchor detection
//!   - merge_titles  — adjacent-title merging
//!   - output        — text extraction with merge spans
//!   - integration   — end-to-end find_tables on synthetic sheets
//!
//! Each logic path has positive, negative, and false-positive examples drawn
//! from common spreadsheet layouts (daily log, stacked headers, sparse rows,
//! empty gaps).

use super::*;
use serde_json::json;

// ═══════════════════════════════════════════════════════════════════════════
// cell_types
// ═══════════════════════════════════════════════════════════════════════════
mod cell_types {
    use super::*;

    #[test] fn cached_plain_string() {
        assert_eq!(cached_value(&json!("hi")), Some(&json!("hi")));
    }
    #[test] fn cached_plain_number() {
        assert_eq!(cached_value(&json!(42)), Some(&json!(42)));
    }
    #[test] fn cached_null_is_none() {
        assert_eq!(cached_value(&json!(null)), None);
    }
    #[test] fn cached_formula_pair() {
        // Day Log style: [value, formula]
        assert_eq!(cached_value(&json!([42, "=A1"])), Some(&json!(42)));
    }
    #[test] fn cached_formula_null_value() {
        assert_eq!(cached_value(&json!([null, "=SUM(A1:A5)"])), Some(&json!(null)));
    }
    #[test] fn cached_merge_object() {
        assert_eq!(cached_value(&json!({"span": 3, "v": "Title"})), Some(&json!("Title")));
    }
    #[test] fn cached_array_single_passes_through() {
        // Only arrays of length 2 are treated as [value, formula].
        // Other arrays fall through to `_ => Some(cell)` — i.e. the array itself.
        let a = json!([1]);
        assert_eq!(cached_value(&a), Some(&a));
    }
    #[test] fn cached_array_three_elements_passes_through() {
        let a = json!([1, 2, 3]);
        assert_eq!(cached_value(&a), Some(&a));
    }

    #[test] fn text_plain_string() {
        assert!(is_text_cached(&json!("hello")));
    }
    #[test] fn text_empty_string_is_text() {
        // Empty string is still a string → header text validation uses trim elsewhere
        assert!(is_text_cached(&json!("")));
    }
    #[test] fn text_iso_date_is_not_text() {
        assert!(!is_text_cached(&json!("2024-01-01")));
    }
    #[test] fn text_iso_datetime_is_not_text() {
        assert!(!is_text_cached(&json!("2024-01-01 10:30:00")));
    }
    #[test] fn text_short_date_like_still_text() {
        // "2024-01" fails len>=10 check; treated as text (edge case, rarely encountered)
        assert!(is_text_cached(&json!("2024-01")));
    }
    #[test] fn text_number_not_text() {
        assert!(!is_text_cached(&json!(42)));
    }
    #[test] fn text_null_not_text() {
        assert!(!is_text_cached(&json!(null)));
    }
    #[test] fn text_formula_with_text_cached() {
        // Day Log header: ["Date", "=IFERROR(__xludf.DUMMYFUNCTION(...))"]
        assert!(is_text_cached(&json!(["Date", "=IFERROR(...)"])));
    }
    #[test] fn text_formula_with_number_cached() {
        assert!(!is_text_cached(&json!([100, "=B1*0.1"])));
    }
    #[test] fn text_formula_with_null_cached() {
        assert!(!is_text_cached(&json!([null, "=SUM(A1:A10)"])));
    }
    #[test] fn text_merge_with_text_value() {
        assert!(is_text_cached(&json!({"span": 15, "v": "Title"})));
    }

    #[test] fn num_plain_integer() {
        assert!(is_number_or_date_cached(&json!(100)));
    }
    #[test] fn num_plain_float() {
        assert!(is_number_or_date_cached(&json!(3.5)));
    }
    #[test] fn num_zero_is_number() {
        // Case 4: "all zeros row" should still count as data row
        assert!(is_number_or_date_cached(&json!(0)));
    }
    #[test] fn num_negative_is_number() {
        assert!(is_number_or_date_cached(&json!(-123.45)));
    }
    #[test] fn num_iso_date_is_date() {
        assert!(is_number_or_date_cached(&json!("2024-12-31")));
    }
    #[test] fn num_text_not_number() {
        assert!(!is_number_or_date_cached(&json!("hello")));
    }
    #[test] fn num_null_not_number() {
        assert!(!is_number_or_date_cached(&json!(null)));
    }
    #[test] fn num_formula_with_number_cached() {
        assert!(is_number_or_date_cached(&json!([1234.56, "=B2*1.2"])));
    }
    #[test] fn num_formula_with_date_cached() {
        // Day Log: ["2025-09-16", "=IFERROR(..., 45916.0)"]
        assert!(is_number_or_date_cached(&json!(["2025-09-16", "=IFERROR(...)"])));
    }
    #[test] fn num_formula_with_text_cached_not_number() {
        assert!(!is_number_or_date_cached(&json!(["Qty", "=IFERROR(...)"])));
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// row_shape
// ═══════════════════════════════════════════════════════════════════════════
mod row_shape {
    use super::*;

    // ─── is_null_or_empty ─────────────────────────────────────────────
    #[test] fn empty_null_value() {
        assert!(is_null_or_empty(&json!(null)));
    }
    #[test] fn empty_empty_array() {
        assert!(is_null_or_empty(&json!([])));
    }
    #[test] fn empty_all_null_array() {
        assert!(is_null_or_empty(&json!([null, null, null])));
    }
    #[test] fn empty_array_with_one_value() {
        assert!(!is_null_or_empty(&json!([null, 1, null])));
    }
    #[test] fn empty_non_array_non_null() {
        // Defensive: a raw string/number isn't a row shape, treat as non-empty
        assert!(!is_null_or_empty(&json!("scalar")));
        assert!(!is_null_or_empty(&json!(0)));
    }

    // (is_header_row tests removed when V1 algorithm was retired -- V3
    //  classifies header rows via `header_runs_in_row` in matcher_v3.rs.)

    // ─── is_all_text_in_range ─────────────────────────────────────────
    #[test] fn all_text_in_range_plain() {
        assert!(is_all_text_in_range(&json!(["a", "b", "c"]), 0, 2));
    }
    #[test] fn all_text_range_ignores_outside() {
        // Out-of-range numbers don't affect verdict
        assert!(is_all_text_in_range(&json!([100, "a", "b", 999]), 1, 2));
    }
    #[test] fn all_text_range_mixed_fails() {
        assert!(!is_all_text_in_range(&json!([100, "a", "b"]), 0, 2));
    }
    #[test] fn all_text_range_single_cell_fails() {
        // Needs ≥2 text cells
        assert!(!is_all_text_in_range(&json!(["a"]), 0, 0));
    }
    #[test] fn all_text_range_with_nulls_ok() {
        assert!(is_all_text_in_range(&json!(["a", null, "b"]), 0, 2));
    }

    // ─── is_data_row ──────────────────────────────────────────────────
    #[test] fn data_with_number() {
        assert!(is_data_row(&json!([100, 200]), 0, 1));
    }
    #[test] fn data_with_iso_date() {
        assert!(is_data_row(&json!(["2024-01-01", null]), 0, 1));
    }
    #[test] fn data_with_formula_number() {
        assert!(is_data_row(&json!([[100, "=B2"]]), 0, 0));
    }
    #[test] fn data_with_zero_values() {
        // User case 4: all-zero data row still counts as data
        assert!(is_data_row(&json!([0, [0, "=A1*0"]]), 0, 1));
    }
    #[test] fn data_text_only_fails() {
        assert!(!is_data_row(&json!(["hello", "world"]), 0, 1));
    }
    #[test] fn data_all_null_fails() {
        assert!(!is_data_row(&json!([null, null]), 0, 1));
    }
    #[test] fn data_out_of_range_ignored() {
        assert!(!is_data_row(&json!([100, "text"]), 1, 1));
    }
    #[test] fn data_formula_with_text_cached_fails() {
        // False-positive guard: Day Log header formula
        assert!(!is_data_row(&json!([["Date", "=IFERROR(...)"]]), 0, 0));
    }

    // ─── is_anchor_only_row ───────────────────────────────────────────
    #[test] fn anchor_only_date_in_col_start() {
        // User case 5: Sparse tail rows (only date col filled)
        assert!(is_anchor_only_row(&json!(["2026-01-06", null, null]), 0, 2));
    }
    #[test] fn anchor_only_text_in_col_start() {
        assert!(is_anchor_only_row(&json!(["label", null, null, null]), 0, 3));
    }
    #[test] fn anchor_only_with_zero_elsewhere_fails() {
        // User case 4: zero values in other cols → NOT anchor-only, still data
        assert!(!is_anchor_only_row(&json!(["2026-01-06", 0, 0]), 0, 2));
    }
    #[test] fn anchor_only_anchor_empty_fails() {
        assert!(!is_anchor_only_row(&json!([null, 1, null]), 0, 2));
    }
    #[test] fn anchor_only_all_null_fails() {
        assert!(!is_anchor_only_row(&json!([null, null, null]), 0, 2));
    }
    #[test] fn anchor_only_offset_col_start() {
        // col_start=2, only col 2 filled
        assert!(is_anchor_only_row(&json!([null, null, "x", null, null]), 2, 4));
    }

    // ─── is_total_row_structural (regression) ────────────
    //
    // The old fill-density heuristic flagged rows whose fill count was
    // <½ of the running mean. That collapsed once a rate-sub-header /
    // mixed-format row landed in the data walk and inflated `typical`.
    // The new detector uses structural signals: label-anchor, aggregate
    // formula, orphan-anchor. Density doesn't enter the decision.

    #[test] fn structural_empty_row_returns_false() {
        assert!(!is_total_row_structural(&json!([null, null, null]), 0, 2));
    }
    #[test] fn structural_grand_total_label_match() {
        // 'Total' / 'TOTAL' / 'итого' all match.
        assert!(is_total_row_structural(
            &json!(["Total", 100, null, null, null, null, null, null]), 0, 7));
        assert!(is_total_row_structural(
            &json!(["TOTAL", 100, 200, 300, null, null, null, null]), 0, 7));
        assert!(is_total_row_structural(
            &json!(["Итог:", 100, 200, null, null, null, null, null]), 0, 7));
    }
    #[test] fn structural_year_label_match() {
        // Bare 4-digit year and trailing-colon variants — the sparse-totals regression
        // headline shape.
        assert!(is_total_row_structural(
            &json!(["2026", 100, 200, 300, null, null]), 0, 5));
        assert!(is_total_row_structural(
            &json!(["2026:", 12_345_678.91, null, null]), 0, 3));
        assert!(is_total_row_structural(
            &json!(["total:", 23_456_789.12, null, null]), 0, 3));
    }
    #[test] fn structural_period_label_match() {
        // Monthly subtotal label '04.2025:'.
        assert!(is_total_row_structural(
            &json!(["04.2025:", 23, 0.58, null, null]), 0, 4));
        assert!(is_total_row_structural(
            &json!(["12.2025:", 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]), 0, 13));
        assert!(is_total_row_structural(
            &json!(["4.2025", 23, 0.58]), 0, 2));
    }
    #[test] fn structural_full_data_row_with_label_is_still_total() {
        // CHANGE FROM OLD BEHAVIOUR: a "wide" totals row (`Итог:` with
        // every cell filled) is now classified as a total by its label
        // alone. The old density heuristic let it through; downstream
        // had to re-classify. The sparse-totals regression demands this:
        // a '2026:' year row with fully-filled cells IS a total.
        let row = json!(["Итог:", 1, 2, 3, 4, 5, 6, 7]);
        assert!(is_total_row_structural(&row, 0, 7));
    }
    #[test] fn structural_iso_date_anchor_is_data_not_total() {
        // Real daily row — date in col[0], some sparse other cells.
        // Pre-fix density heuristic could mark this as total once
        // `typical` was inflated. Structural says: not a label, not
        // an aggregate formula, anchor not empty → DATA.
        let row = json!(["2024-08-01", 100, null, "note", null, null, null, null]);
        assert!(!is_total_row_structural(&row, 0, 7));
    }
    #[test] fn structural_sparse_daily_row_with_date_formula_is_data() {
        // Sparse daily shape: date + one value
        // computed via per-row date formula `=A4+$F$3`. Should pass.
        let row = json!([
            "2025-01-01", null, null, null, null,
            ["2025-06-30", "=A4+$F$3"], null, null
        ]);
        assert!(!is_total_row_structural(&row, 0, 7));
    }
    #[test] fn structural_orphan_anchor_three_numerics_is_total() {
        // Anchor empty + ≥3 numerics → orphan total.
        let row = json!([null, 1, 2, 3, null, null]);
        assert!(is_total_row_structural(&row, 0, 5));
    }
    #[test] fn structural_orphan_anchor_two_numerics_is_data() {
        // Anchor empty + only 2 numerics — under threshold; let it
        // through (downstream date-shape filter handles).
        let row = json!([null, 1, 2, null, null, null]);
        assert!(!is_total_row_structural(&row, 0, 5));
    }
    #[test] fn structural_aggregate_formula_sum_keyword() {
        let row = json!([null, [12_345_678.91, "=SUM(B5:B30)"], null]);
        assert!(is_total_row_structural(&row, 0, 2));
    }
    #[test] fn structural_aggregate_formula_multi_row_refs() {
        // Year subtotal computed via an explicit B-row sum. Since the
        // manual-sum regression fix, the manual-row-ref
        // heuristic was dropped from the formula classifier — this
        // row still classifies as a total via the LABEL-anchor
        // check (`"2026:"` is a bare year + colon, caught by
        // is_total_label).
        let row = json!([
            "2026:",
            [12_345_678.91, "=B413+B442+B474+B500+B530+B561+B591"],
            null
        ]);
        assert!(is_total_row_structural(&row, 0, 2));
    }
    #[test] fn structural_daily_row_with_peripheral_manual_sum_is_data() {
        // Regression: A daily data row whose anchor is an
        // ISO date and primary value cell is a raw number, but
        // whose peripheral "Side" column has a manual cross-sheet
        // sum formula `='Sheet'!B156+'Sheet'!B157+'Sheet'!B158`. The
        // old multi-row-ref heuristic flagged this as a total and
        // silently dropped the row from output. Must be DATA.
        let row = json!([
            "2025-01-03",                // anchor: ISO date
            123_456.78,                  // value: raw number
            null, null, null, null,
            [234_567.89, "='Sheet B'!B156+'Sheet B'!B157+'Sheet B'!B158"],
            null
        ]);
        assert!(!is_total_row_structural(&row, 0, 7));
    }
    #[test] fn structural_per_row_date_formula_is_not_aggregate() {
        // Per-row date offset: `=A4+90` (1 ref) and `=A4+$F$3` (2
        // refs across different cols, same OR adjacent rows). Must
        // NOT be classified — these are the per-day target-date
        // computations that show up in EVERY sparse daily row.
        let r1 = json!(["2025-01-01", null, null, null, null,
                        ["2025-06-30", "=A4+90"], null]);
        let r2 = json!(["2025-01-01", null, null, null, null,
                        ["2025-06-30", "=A4+$F$3"], null]);
        assert!(!is_total_row_structural(&r1, 0, 6));
        assert!(!is_total_row_structural(&r2, 0, 6));
    }

    // ─── is_total_label direct unit tests ────────────────────────────
    #[test] fn label_keywords_match() {
        assert!(is_total_label("total"));
        assert!(is_total_label("TOTAL"));
        assert!(is_total_label("Total:"));
        assert!(is_total_label("subtotal"));
        assert!(is_total_label("Grand Total"));
        assert!(is_total_label("итого"));
        assert!(is_total_label("Итог:"));
        assert!(is_total_label("ВСЕГО:"));
        assert!(is_total_label("сумма"));
    }
    #[test] fn label_period_match() {
        assert!(is_total_label("04.2025"));
        assert!(is_total_label("4.2025"));
        assert!(is_total_label("12.2025:"));
        assert!(is_total_label("2025"));
        assert!(is_total_label("2026:"));
    }
    #[test] fn label_short_with_colon_match() {
        // Short label with trailing colon — strong signal even if
        // we don't recognize the keyword.
        assert!(is_total_label("special:"));
    }
    #[test] fn label_iso_date_no_match() {
        // ISO date — has dashes, not all-digits-or-dot.
        assert!(!is_total_label("2025-01-01"));
        assert!(!is_total_label("2025-12-31"));
    }
    #[test] fn label_pure_numeric_id_no_match() {
        // 7-digit account ID — not a year, not a period.
        assert!(!is_total_label("1234567"));
        assert!(!is_total_label("00001"));
    }
    #[test] fn label_plain_text_no_match() {
        assert!(!is_total_label("Notes"));
        assert!(!is_total_label("Header"));
        assert!(!is_total_label(""));
        assert!(!is_total_label("   "));
        assert!(!is_total_label("  :  "));
    }
    #[test] fn label_long_with_colon_no_match() {
        // Long string ending in colon — likely a sentence, not a label.
        assert!(!is_total_label("This is a long description ending in a colon:"));
    }

    // ─── cell_has_aggregate_formula direct unit tests ────────────────
    #[test] fn formula_sum_family_keywords() {
        assert!(cell_has_aggregate_formula(&json!([100, "=SUM(B1:B30)"])));
        assert!(cell_has_aggregate_formula(&json!([10, "=SUMIF(A:A,\"x\",B:B)"])));
        assert!(cell_has_aggregate_formula(&json!([10, "=SUMIFS(B:B,A:A,\"x\")"])));
        assert!(cell_has_aggregate_formula(&json!([10, "=SUBTOTAL(9,B1:B30)"])));
        assert!(cell_has_aggregate_formula(&json!([10, "=AVERAGE(B1:B30)"])));
        assert!(cell_has_aggregate_formula(&json!([10, "=AGGREGATE(9,5,B:B)"])));
    }
    #[test] fn formula_manual_sum_no_match() {
        // Regression: Manual sums of cell references are
        // NOT aggregate — they show up on legitimate daily data rows
        // when a peripheral column manually adds sibling cells. The
        // old multi-row-ref heuristic flagged these and silently dropped
        // the rows. Real totals still classified via label / orphan
        // anchor checks at the row level.
        assert!(!cell_has_aggregate_formula(&json!([300, "=B413+B442+B474"])));
        assert!(!cell_has_aggregate_formula(&json!([300, "=B1+B2+B3+B4"])));
        // The original reproducer shape — cross-sheet manual sum.
        assert!(!cell_has_aggregate_formula(&json!([
            234_567.89,
            "='Sheet B'!B156+'Sheet B'!B157+'Sheet B'!B158"
        ])));
        assert!(!cell_has_aggregate_formula(&json!([
            345_678.91,
            "='Sheet B'!B159+'Sheet B'!B160+'Sheet B'!B161"
        ])));
    }
    #[test] fn formula_two_refs_no_match() {
        // Per-row date offsets (typical sparse-daily-row formula).
        assert!(!cell_has_aggregate_formula(&json!([100, "=A4+$F$3"])));
        assert!(!cell_has_aggregate_formula(&json!([100, "=B1+B2"])));
    }
    #[test] fn formula_single_ref_no_match() {
        assert!(!cell_has_aggregate_formula(&json!([100, "=A4+90"])));
        assert!(!cell_has_aggregate_formula(&json!([100, "=A4*1.05"])));
        // Single cross-sheet ref (the rows in the manual-sum regression that were NOT
        // dropped — single `='Sheet'!B162` reference).
        assert!(!cell_has_aggregate_formula(&json!([
            456_789.12, "='Sheet B'!B162"
        ])));
    }
    #[test] fn formula_non_array_no_match() {
        assert!(!cell_has_aggregate_formula(&json!(100)));
        assert!(!cell_has_aggregate_formula(&json!("text")));
        assert!(!cell_has_aggregate_formula(&json!(null)));
    }
    #[test] fn formula_array_without_formula_string_no_match() {
        // Two-element array but second element isn't a formula string.
        assert!(!cell_has_aggregate_formula(&json!([100, 200])));
        assert!(!cell_has_aggregate_formula(&json!([100, "no equals sign"])));
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// counts
// ═══════════════════════════════════════════════════════════════════════════
mod counts {
    use super::*;

    #[test] fn count_text_basic() {
        assert_eq!(count_text_in_range(&json!(["a", "b", 1, "c"]), 0, 3), 3);
    }
    #[test] fn count_text_with_nulls() {
        assert_eq!(count_text_in_range(&json!(["a", null, "b", null]), 0, 3), 2);
    }
    #[test] fn count_text_range_limited() {
        assert_eq!(count_text_in_range(&json!(["a", "b", "c"]), 0, 1), 2);
    }
    #[test] fn count_text_iso_date_not_text() {
        assert_eq!(count_text_in_range(&json!(["2024-01-01", "ok"]), 0, 1), 1);
    }

    #[test] fn count_cells_basic() {
        assert_eq!(count_cells_in_range(&json!([1, null, 2, 3]), 0, 3), 3);
    }
    #[test] fn count_cells_all_null() {
        assert_eq!(count_cells_in_range(&json!([null, null, null]), 0, 2), 0);
    }
    #[test] fn count_cells_range_past_end() {
        // col_end beyond row length is clamped
        assert_eq!(count_cells_in_range(&json!([1, 2]), 0, 5), 2);
    }
    #[test] fn count_cells_partial_range() {
        assert_eq!(count_cells_in_range(&json!([1, 2, 3, 4]), 1, 2), 2);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// ranges
// ═══════════════════════════════════════════════════════════════════════════
mod ranges {
    use super::*;

    #[test] fn title_merge_span3_detected() {
        assert!(has_title_merge_in_range(&json!([
            {"span": 3, "v": "Group B"}, null, null
        ]), 0, 2));
    }
    #[test] fn title_merge_span2_not_detected() {
        // has_title_merge_in_range requires span>=3
        assert!(!has_title_merge_in_range(&json!([
            {"span": 2, "v": "EUR"}, null
        ]), 0, 1));
    }
    #[test] fn title_merge_span1_not_merge() {
        assert!(!has_title_merge_in_range(&json!([
            {"span": 1, "v": "x"}
        ]), 0, 0));
    }
    #[test] fn title_merge_out_of_range() {
        assert!(!has_title_merge_in_range(&json!([
            "skip",
            {"span": 3, "v": "Group A"}, null, null
        ]), 0, 0));
    }
    #[test] fn title_merge_plain_text_not_merge() {
        assert!(!has_title_merge_in_range(&json!(["a", "b", "c"]), 0, 2));
    }

    // ─── find_text_groups ─────────────────────────────────────────────
    #[test] fn text_groups_contiguous() {
        assert_eq!(find_text_groups(&json!(["a", "b", "c"])), vec![(0, 2)]);
    }
    #[test] fn text_groups_singletons_dropped() {
        // Single text cells (last == start) are NOT reported as groups
        assert_eq!(find_text_groups(&json!(["a"])), vec![]);
    }
    #[test] fn text_groups_split_by_number() {
        // [text, text, number, text, text] → two groups
        assert_eq!(find_text_groups(&json!(["a", "b", 1, "c", "d"])),
                   vec![(0, 1), (3, 4)]);
    }
    #[test] fn text_groups_split_by_null() {
        assert_eq!(find_text_groups(&json!(["a", "b", null, "c", "d"])),
                   vec![(0, 1), (3, 4)]);
    }
    #[test] fn text_groups_isolated_drops() {
        // Isolated "a" drops (start==last), second group (2,3) stays
        assert_eq!(find_text_groups(&json!(["a", 1, "c", "d"])), vec![(2, 3)]);
    }
    #[test] fn text_groups_all_empty() {
        assert_eq!(find_text_groups(&json!([null, null])), vec![]);
    }
    #[test] fn text_groups_iso_dates_not_text() {
        // ISO dates break groups
        assert_eq!(find_text_groups(&json!(["a", "b", "2024-01-01", "c", "d"])),
                   vec![(0, 1), (3, 4)]);
    }

    // ─── extend_col_end_contiguous ────────────────────────────────────
    #[test] fn extend_through_non_nulls() {
        // current_end=2; cells 3,4 non-null → extend to 4; 5 null → stop
        let row = json!([1, 2, 3, 4, 5, null, 6]);
        assert_eq!(extend_col_end_contiguous(&row, 2), 4);
    }
    #[test] fn extend_stops_at_first_null() {
        let row = json!([1, 2, 3, null, 5, 6]);
        assert_eq!(extend_col_end_contiguous(&row, 2), 2);
    }
    #[test] fn extend_through_end() {
        let row = json!([1, 2, 3, 4]);
        assert_eq!(extend_col_end_contiguous(&row, 2), 3);
    }
    #[test] fn extend_already_at_end() {
        let row = json!([1, 2]);
        assert_eq!(extend_col_end_contiguous(&row, 1), 1);
    }

    // ─── extend_col_start_contiguous (regression) ─────────
    #[test] fn extend_left_through_non_nulls() {
        // current_start=3; cells 1,2 non-null → extend to 1; 0 null → stop.
        let row = json!([null, 1, 2, 3, 4]);
        assert_eq!(extend_col_start_contiguous(&row, 3), 1);
    }
    #[test] fn extend_left_stops_at_first_null() {
        // current_start=4; cell 3 null → stay at 4.
        let row = json!([1, 2, 3, null, 5]);
        assert_eq!(extend_col_start_contiguous(&row, 4), 4);
    }
    #[test] fn extend_left_through_to_zero() {
        // All cells non-null up to current_start → land at 0.
        let row = json!([1, 2, 3, 4]);
        assert_eq!(extend_col_start_contiguous(&row, 3), 0);
    }
    #[test] fn extend_left_already_at_zero() {
        let row = json!([1, 2]);
        assert_eq!(extend_col_start_contiguous(&row, 0), 0);
    }
    #[test] fn extend_left_non_array_returns_input() {
        assert_eq!(extend_col_start_contiguous(&json!(null), 5), 5);
    }

    // ─── find_next_non_empty ──────────────────────────────────────────
    #[test] fn next_non_empty_finds_first() {
        let s = vec![json!(null), json!([1, 2]), json!(null)];
        assert_eq!(find_next_non_empty(&s, 0, 2), Some(1));
    }
    #[test] fn next_non_empty_none_found() {
        let s = vec![json!(null), json!(null)];
        assert_eq!(find_next_non_empty(&s, 0, 1), None);
    }
    #[test] fn next_non_empty_starts_from_offset() {
        let s = vec![json!([1]), json!(null), json!([2])];
        assert_eq!(find_next_non_empty(&s, 1, 2), Some(2));
    }

    // ─── is_row_claimed_in_range ──────────────────────────────────────
    #[test] fn claimed_majority_true() {
        let mut claimed = HashSet::new();
        for c in 0..5 { claimed.insert((3, c)); } // 5 of 5 cells claimed
        assert!(is_row_claimed_in_range(&claimed, 3, 0, 4));
    }
    #[test] fn claimed_exactly_half_false() {
        let mut claimed = HashSet::new();
        for c in 0..2 { claimed.insert((3, c)); } // 2 of 4 claimed
        assert!(!is_row_claimed_in_range(&claimed, 3, 0, 3));
    }
    #[test] fn claimed_none_false() {
        let claimed = HashSet::new();
        assert!(!is_row_claimed_in_range(&claimed, 3, 0, 4));
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// blocks
// ═══════════════════════════════════════════════════════════════════════════
mod blocks {
    use super::*;

    #[test] fn single_contiguous_block() {
        let s = vec![json!([1]), json!([2]), json!([3])];
        let b = find_blocks(&s);
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].row_start, b[0].row_end), (0, 2));
    }

    #[test] fn leading_empty_skipped() {
        let s = vec![json!(null), json!(null), json!([1]), json!([2])];
        let b = find_blocks(&s);
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].row_start, b[0].row_end), (2, 3));
    }

    #[test] fn trailing_empty_trimmed() {
        // last_non_empty pointer means trailing empties drop off
        let s = vec![json!([1]), json!([2]), json!(null), json!(null)];
        let b = find_blocks(&s);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].row_end, 1);
    }

    #[test] fn small_gap_keeps_one_block() {
        // 4-row empty streak doesn't split (threshold is 5)
        let s = vec![
            json!([1]), json!([2]),
            json!(null), json!(null), json!(null), json!(null),
            json!([3]), json!([4]),
        ];
        let b = find_blocks(&s);
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].row_start, b[0].row_end), (0, 7));
    }

    #[test] fn five_empty_rows_splits_blocks() {
        let s = vec![
            json!([1]), json!([2]),
            json!(null), json!(null), json!(null), json!(null), json!(null),
            json!([3]), json!([4]),
        ];
        let b = find_blocks(&s);
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].row_start, b[0].row_end), (0, 1));
        assert_eq!((b[1].row_start, b[1].row_end), (7, 8));
    }

    #[test] fn all_empty_no_blocks() {
        let s = vec![json!(null); 10];
        assert_eq!(find_blocks(&s).len(), 0);
    }
}

