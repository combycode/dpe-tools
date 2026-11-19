# read-tables

Reads tables out of sheet JSON (typically produced by [`xlsx`](../xlsx)). Finds table rectangles by matching `.tpl` format templates loaded once at startup. Three modes: emit one envelope per data row (`parse`), extract each table to a file and emit one envelope per table (`tables`), or discover candidate template shapes (`detect`).

**When to use:**
- You have heterogeneous spreadsheets where each table layout follows one of a known set of shapes.
- You need to extract row-by-row data from inside those tables without writing a one-off parser per layout.
- You want to add support for a new layout by dropping a `.tpl` file, not by editing tool code.

**When NOT to use:**
- You have a single, uniform CSV-shaped sheet -> stream it through a CSV reader directly.
- Layouts are too varied for templates -> use [`llm`](../llm) on rendered images via [`doc-converter`](../doc-converter).

---

## Install

```bash
dpe install read-tables
```

## How it works

### `parse` mode (default)

1. **Load templates once** at startup from `settings.formats` (every `*.tpl` file in that dir).
2. **For each input envelope**, read the sheet JSON file at `v.path`. Both wire formats are accepted:
   - New: `{source_file, sheet_name, extracted_at, rows: [...]}`.
   - Legacy: bare array of rows.
3. **Find all tables in the sheet** via header-band detection:
   - Build a coverage map from horizontal (`span`) and vertical (`rspan`) merge anchors so a cell that was "swallowed" by a merge anchor is still considered touched.
   - Bound block(s) by contiguous rows of touched cells.
   - For each row in a block, find HEADER RUNS — maximal contiguous ranges where every cell is a header label (text, or a merge anchor with text content) and date cells break the run.
   - Group adjacent rows whose runs overlap into a vertical header BAND.
   - Detect TITLE row at the top of the band: the largest horizontal merge (`cspan >= 2`); 2D merges (`span + rspan`) and pure rspan anchors alongside are allowed; bare text rejects a title.
   - For each template, find the leftmost column in the segment where the template's strict data prefix (`date`) matches — that's the table's anchor column.
   - Data range runs from band end + 1 down to the next overlapping band, or the last sheet row if none — so titled tables separated from data by long null gaps are matched correctly.
   - Record the matched table rectangle (col range, row range, columns, title, format-name).
4. **Emit one envelope per matched data row** with shape:
   ```json
   {
     "sheet":     "<sheet name>",
     "table":     <0-based table index in the sheet>,
     "format":    "<template name>",
     "title":     "<merged-cell title or null>",
     "columns":   ["Col A", "Col B", "Col C"],
     "row":       [<value-A>, <value-B>, <value-C>],
     "row_idx":   <1-based row number in the source sheet>,
     "col_start": <0-based>,
     "col_end":   <0-based, inclusive>,
     "row_start": <1-based row of the table's first header>,
     "row_end":   <1-based row of the table's last data row>
   }
   ```
5. **Emit a meta envelope** per sheet listing all tables found (format, title, rows, range).

### `tables` mode

Same matcher as `parse`, different granularity. For each detected table:

1. Extract the data rows to a file under `extract.dir` in the format chosen by `extract.format` (`csv` / `ndjson` / `ndjson-compact`).
2. Emit ONE envelope per table referencing the file.

No per-row envelopes flow downstream. The file IS the row data; the envelope IS the table-level metadata.

Filename is auto-derived: `<source-stem>__<sanitized-sheet>__t<table-idx>.<ext>`. Sheet names are sanitized (path separators, control chars, Windows-reserved chars become `_`). Same input → same output (deterministic, overwrites prior runs).

```json
{
  "sheet":           "Q1 Rates",
  "table":           0,
  "format":          "<template name>",
  "title":           "<merged-cell title or null>",
  "columns":         ["Region", "Revenue", "Cost"],
  "col_start":       0,
  "col_end":         2,
  "row_start":       1,
  "row_end":         4,
  "range":           "A1:C4",                                 ← NEW: A1-style readable label
  "target":          "/data/out/extracted/rates__Q1 Rates__t0.csv"   ← NEW: file written
}
```

Use `tables` when:

- Downstream consumers need the data as a file (Excel reader, other tooling, debugging).
- You want one envelope per table for routing / per-table aggregation.
- Per-row envelope-fanout is too chatty for the pipeline shape.

I/O failures (mkdir / open / write / flush) for one table do NOT abort the rest — that table surfaces via `ctx.error(&v, ...)` (lands in `<session>/logs/<stage>_errors.log` with the original input attached) and the next table still processes.

#### Output file formats

For a 3-column table with columns `["Region","Revenue","Cost"]` and 2 rows `["EMEA",100,50]`, `["APAC",80,40]`:

**`csv`** — RFC-4180. Header row, then one row per data row. Quotes wrap fields containing `,` `"` or newline; embedded `"` doubles to `""`. Numbers and booleans serialise as their JSON literal; null becomes the empty cell; object/array cells JSON-stringify (preserves merged-cell shapes losslessly).

```csv
Region,Revenue,Cost
EMEA,100,50
APAC,80,40
```

**`ndjson` (full)** — each line is a JSON object with the **exact same shape** as a row-mode envelope's `v` field. All metadata duplicated per row; same data as `parse` mode just written to disk instead of streamed downstream.

```ndjson
{"col_end":2,"col_start":0,"columns":["Region","Revenue","Cost"],"format":"flat","row":["EMEA",100,50],"row_end":3,"row_idx":2,"row_start":1,"sheet":"Q1 Rates","source_file":"rates.xlsx","table":0,"title":null}
{"col_end":2,"col_start":0,"columns":["Region","Revenue","Cost"],"format":"flat","row":["APAC",80,40],"row_end":3,"row_idx":3,"row_start":1,"sheet":"Q1 Rates","source_file":"rates.xlsx","table":0,"title":null}
```

**`ndjson-compact`** — first line is the column-name array, subsequent lines are value arrays in column order. Smallest format.

```ndjson
["Region","Revenue","Cost"]
["EMEA",100,50]
["APAC",80,40]
```

### `detect` mode

1. Walk the sheet and identify table-shaped blocks WITHOUT consulting templates.
2. For each detected shape, write a `<sheet>_<idx>.tpl` file into `settings.output` containing the inferred template skeleton.
3. Emit meta records pointing at the produced `.tpl` files.

Use `detect` when onboarding a new vendor's spreadsheet: run it once, inspect the produced templates, hand-edit them, then point `parse` mode at the directory.

---

## Settings reference

| Field                    | Type    | Default                | Description |
|--------------------------|---------|------------------------|-------------|
| `formats`                | string  | —                      | **Required.** Path to a directory of `.tpl` template files. All `.tpl` files in that dir are loaded. |
| `mode`                   | enum    | `parse`                | `parse` (emit one envelope per data row) / `tables` (extract each table to a file, emit one envelope per table) / `detect` (write template candidates). |
| `output`                 | string  | `.`                    | `detect` mode only — directory for generated `.tpl` files. Ignored in parse / tables modes. |
| `extract.dir`            | string  | —                      | **Required when `mode = tables`.** Folder where extracted table files are written. |
| `extract.format`         | enum    | `ndjson`               | `csv` / `ndjson` / `ndjson-compact`. See [`tables` mode](#tables-mode) above. |
| `passthrough_input`      | bool    | `false`                | Copy every input-envelope `v` field (except reserved keys `path` / `sheet` / `source_*`) onto every emitted envelope. Applies to both parse and tables modes. |

---

## Input envelope

```json
{ "path": "/data/extracted/q4/Sheet1.json", "sheet": "Sheet1", "source_file": "vendor/q4.xlsx" }
```

| Field         | Required | Description |
|---------------|----------|-------------|
| `path`        | yes      | Absolute path to a sheet JSON file. |
| `sheet`       | no       | Sheet name; used for provenance in output. Falls back to the JSON's embedded `sheet_name`, then `"unknown"`. |
| `source_file` | no       | Provenance label; used in output. Falls back to the JSON's embedded `source_file`. |

---

## Template grammar

Each `.tpl` file is plain text. Lines are `<row-name><modifier>: <cell-pattern>...`.

### Row modifiers

| Modifier | Meaning                                             |
|----------|-----------------------------------------------------|
| (none)   | required, exactly once                              |
| `?`      | optional — match 0 or 1 row                         |
| `*`      | repeat — match 1 or more rows                       |
| `~`      | skip — match the row but don't emit (e.g. totals)   |

### Cell patterns

| Pattern   | Matches                                              |
|-----------|------------------------------------------------------|
| `text`    | Non-empty string cell                                |
| `number`  | Numeric cell or numeric string                       |
| `date`    | ISO date string or numeric Excel serial             |
| `formula` | Formula cell (encoded as `[value, "=..."]`)         |
| `merge`   | Horizontal merge anchor (encoded as `{span, v}`)    |
| `any`     | Any non-null cell                                    |
| `empty`   | Null / empty cell                                    |

### Specials

| Token       | Meaning |
|-------------|---------|
| `...`       | At end of line: accept any remaining cells in the row. |
| `+` suffix  | One or more of the preceding pattern. e.g. `text+` = one or more text cells. |
| `\|`        | Alternatives within one cell. e.g. `text\|empty` = text or empty. |

### Total-row signals (used after `~`)

For total rows, the `total~` pattern can declare which signals indicate "this is a totals row":

| Signal   | Meaning |
|----------|---------|
| `period` | Cell text matches `\d{1,2}\.\d{4}:?` (period-marker pattern). |
| `total`  | Cell text contains the literal `total` (case-insensitive). |
| `итого`  | Cell text contains the literal `итого` (Russian "total"). |
| `sparse` | Significantly fewer filled cells than data rows. |
| `sum`    | Cell formula contains `SUM(` or `SUBTOTAL(`. |

These compose with `|`. Example: `period|total|итого|sum`.

### Comments

Lines beginning with `#` are ignored.

### Two canonical templates

**`titled.tpl`:**
```
title     : merge
header    : text+
subheader?: text+
data*     : any ...
total~    : period|total|итого|sparse|sum
```

Matches: a horizontally-merged title cell, then one header row of text cells, optionally a subheader row, then 1+ data rows with any cell types, optionally a totals row that gets matched-but-not-emitted.

**`flat.tpl`:**
```
header : text+
data*  : any ...
total~ : period|total|итого|sparse|sum
```

Matches: header + data rows + optional totals row, no title.

---

## Output (parse mode)

Per matched data row, one envelope:

```json
{
  "sheet":     "Sheet1",
  "table":     0,
  "format":    "titled",
  "title":     "Quarterly Report",
  "columns":   ["Period", "Volume", "Total"],
  "row":       ["Q4 2024", 1234, 5678],
  "row_idx":   12,
  "col_start": 1,
  "col_end":   3,
  "row_start": 5,
  "row_end":   20
}
```

Plus one meta envelope per sheet:

```json
{
  "sheet": "Sheet1",
  "tables": [
    { "format": "titled", "title": "Q1", "columns": ["...", "..."], "rows": 8, "col_start": 1, "col_end": 3, "row_start": 5, "row_end": 12 },
    { "format": "flat",   "title": null, "columns": ["...", "..."], "rows": 12, "col_start": 1, "col_end": 4, "row_start": 14, "row_end": 25 }
  ]
}
```

---

## Output (tables mode)

Per matched table, ONE envelope. Row data lives in `target` (the file written under `extract.dir`), not on the envelope.

```json
{
  "sheet":           "Sheet1",
  "table":           0,
  "format":          "titled",
  "title":           "Quarterly Report",
  "columns":         ["Period", "Volume", "Total"],
  "col_start":       1,
  "col_end":         3,
  "row_start":       5,
  "row_end":         20,
  "range":           "B5:D20",
  "target":          "/data/out/extracted/q4__Sheet1__t0.csv"
}
```

Drops the per-row `row` and `row_idx` (those live on disk now); adds `range` (A1-style readable label) and `target` (file path). Every other field — `sheet`, `table`, `format`, `title`, `columns`, `col_start/end`, `row_start/end`, `source_file`, `source_path`, `source_relative`, plus any `passthrough_input` fields — is identical to parse mode. No meta-envelope summary in this mode (each table envelope already carries the per-table fields inline).

See [`tables` mode](#tables-mode) above for the three on-disk file formats (`csv` / `ndjson` / `ndjson-compact`).

---

## Worked example: extract two layouts from one workbook

```yaml
stages:
  list:
    tool: scan-fs
    settings: { include: ["*.xlsx"], hash: none }
    input: $input

  unpack:
    tool: xlsx
    input: list

  read:
    tool: read-tables
    settings:
      formats: "./formats"      # ./formats/titled.tpl + ./formats/flat.tpl
      mode:    parse
    input: unpack

  sink:
    tool: write-file-stream
    settings: { default_file: "$output/rows.ndjson", format: ndjson }
    input: read
```

Each row from any matched table — across any sheet of any workbook — lands as one NDJSON line in `rows.ndjson`. Downstream stages (filter, classify, mongo-upsert) can route by `format`, `sheet`, or column values.

---

## Worked example: extract each table to a CSV file

Same matcher, different shape: one envelope per TABLE (not per row), and the row data lands as a CSV file on disk. Useful when downstream consumers want files, or when per-row envelope-fanout is too chatty for the pipeline shape.

```yaml
stages:
  list:
    tool: scan-fs
    settings: { include: ["*.xlsx"], hash: none }
    input: $input

  unpack:
    tool: xlsx
    input: list

  read:
    tool: read-tables
    settings:
      formats: "./formats"
      mode:    tables                              # ← extract instead of stream rows
      extract:
        dir:    "$temp/extracted-tables"            # one file per detected table
        format: csv                                  # csv | ndjson | ndjson-compact
    input: unpack

  index:
    tool: write-file-stream
    settings:                                        # one ndjson line per table extracted
      default_file: "$output/tables-index.ndjson"
      format:       ndjson
    input: read
```

Result: one `*.csv` per matched table under `$temp/extracted-tables/` (auto-named `<source-stem>__<sanitized-sheet>__t<idx>.csv`), and a single `tables-index.ndjson` listing every extracted table with its source / sheet / range / columns / target path. Pipelines that need per-row processing of one specific table can load it directly from the path in the index.

Use `format: ndjson` if downstream tooling prefers JSON; use `format: ndjson-compact` for the smallest representation (one header line + one value-array per row).

---

## Worked example: discover templates for a new vendor

When a new vendor's xlsx layout doesn't match any existing template:

```yaml
stages:
  unpack:
    tool: xlsx
    input: $input

  detect:
    tool: read-tables
    settings:
      formats: "./formats"     # existing templates (consulted but not used in detect mode)
      mode:    detect
      output:  "./formats/_candidates"
    input: unpack
```

Run once. Inspect `./formats/_candidates/<sheet>_<n>.tpl`. Edit each candidate (rename, tweak cell patterns, add modifiers). Move the kept ones to `./formats/`. Re-run with `mode: parse`.

---

## More

See [`docs/tools/read-tables.md`](../../docs/tools/read-tables.md) for advanced patterns: side-by-side tables, scattered tables (multiple tables in one sheet at non-zero column offsets), debugging template-match failures.

## License

[AGPL-3.0-or-later](../../LICENSE).
