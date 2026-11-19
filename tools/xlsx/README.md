# xlsx

XLSX file extractor. Reads a `.xlsx` workbook and writes one self-describing JSON file per sheet plus a `meta.json` index. The unit of work is a sheet — the tool emits one envelope per produced sheet file, so downstream stages can route per-sheet decisions.

**When to use:**
- You have `.xlsx` workbooks and want to feed their contents into a streaming pipeline.
- You need to preserve cell-level fidelity (merges, formulas, dates) for downstream tools that interpret structure.

**When NOT to use:**
- You have CSV — read it directly with `read-file-stream` (no need for this tool).
- You only need a flat dump and don't care about merges or formulas — a CSV export would be simpler.

---

## Install

```bash
dpe install xlsx
```

No external runtime dependencies (parser is pure Rust + `quick-xml` + `zip`).

---

## How it works

For each input envelope:

1. Open the `.xlsx` (a ZIP archive of XML parts).
2. Read shared strings, sheet names, styles.
3. **For each sheet** in the workbook:
   - Skip if the sheet's `max_row` is 0 (truly empty).
   - Build a merge map covering BOTH horizontal (`span`) and vertical (`rspan`) merges. The anchor cell (top-left of the merge rectangle) records the merge's `span` (column count) and `rspan` (row count); cells "covered" by the merge remain `null`.
   - For each row from 0 to `max_row - 1`:
     - Find the last filled column.
     - Emit `null` for fully-empty rows (preserves row indexes).
     - Otherwise build an array of cells from column 0 to last_col (preserves absolute positions).
     - Wrap each cell per its kind (see [Cell encoding](#cell-encoding) below).
4. **Write the sheet JSON** to `<output>/<file-stem>/<safe-sheet-name>.json` — a self-describing envelope:
   ```json
   {
     "source_file":  "<provenance label>",
     "sheet_name":   "<original sheet name>",
     "extracted_at": "<RFC3339 UTC timestamp>",
     "rows":         [ <cell-array-or-null>, ... ]
   }
   ```
5. **Emit one envelope per produced sheet file:**
   ```json
   {
     "path":        "/abs/path/to/<file-stem>/<safe-sheet-name>.json",
     "sheet":       "<original sheet name>",
     "source_file": "<provenance label>",
     "rows":        <max_row>,
     "cols":        <max_col>
   }
   ```
6. **Write `<output>/<file-stem>/meta.json`** with the full sheet index:
   ```json
   {
     "source_file":  "<label>",
     "source_path":  "/forward/slashes/path.xlsx",
     "extracted_at": "<RFC3339>",
     "sheets":       [ {"name": "...", "file": "...", "rows": N, "cols": M}, ... ]
   }
   ```
7. **Emit one meta envelope** for the workbook:
   ```json
   { "source_file": "<label>", "sheets": <count> }
   ```

---

## Settings

This tool has **no settings**. All configuration is per-envelope.

---

## Input envelope

```json
{ "path": "/data/in/sample.xlsx", "output": "/data/extracted/", "source_file": "vendor/q4-2024.xlsx" }
```

| Field         | Required | Description |
|---------------|----------|-------------|
| `path`        | yes      | Absolute path to the `.xlsx` file. (Alias: `file` — both accepted.) |
| `output`      | yes      | Output ROOT directory. The tool writes to `<output>/<file-stem>/`. Created if missing. |
| `source_file` | no       | Provenance label baked into every emitted sheet JSON (`source_file` field) and into the per-row envelopes. **Recommended:** pass a scan-root-relative path so downstream tools can group per source document. Defaults to the file's basename. |

---

## Cell encoding

Each cell in the `rows` arrays is one of:

| Cell shape                                  | Meaning |
|---------------------------------------------|---------|
| `null`                                      | Empty cell (or whole-empty row when the row is `null` directly). |
| string                                      | String cell; ISO-8601 (`YYYY-MM-DD`) when the source had a date format. |
| number                                      | Numeric cell. |
| boolean                                     | Boolean cell. |
| `{"span": N, "v": <value>}`                 | Horizontal-merge anchor. The cell at column C spans N columns (C..C+N-1). The merged value is `v`. |
| `{"rspan": M, "v": <value>}`                | Vertical-merge anchor. The cell at row R spans M rows (R..R+M-1) in a single column. The merged value is `v`. Covered cells in rows R+1..R+M-1 stay `null`. |
| `{"span": N, "rspan": M, "v": <value>}`     | 2D-merge anchor. The cell at (R, C) spans N columns and M rows. Both `span` and `rspan` are emitted when the merge rectangle is larger than 1×1 in both dimensions. |
| `[<computed-value>, "=<formula>"]`          | Formula cell. Element 0 is the cached value; element 1 is the formula string starting with `=`. |

### Why this shape

- **Merges as anchor objects** preserve the column where the merge starts AND the span (and row count, for vertical / 2D merges), while leaving the cells "covered" by the merge as plain `null`s. Downstream template-matchers (e.g. [`read-tables`](../read-tables)) can detect merge anchors as titles or category bands and reconstruct stacked headers from `rspan` carry-through.
- **Formulas as `[value, formula]` pairs** let downstream tools either use the cached numeric result OR re-evaluate the formula string. Recognising "this cell is a SUM" is useful for inferring totals rows.
- **Dates as ISO strings** sidesteps the Excel epoch-bug and timezone confusion. The 1900-leap-year quirk is handled internally.
- **Empty rows as `null`** instead of `[]` keeps the JSON small and lets downstream code distinguish "no data" from "all-null cells in a populated row".

---

## Output

### Per-envelope output (one per produced sheet)

```json
{
  "path":        "/data/extracted/q4-2024/Sheet1.json",
  "sheet":       "Sheet1",
  "source_file": "vendor/q4-2024.xlsx",
  "rows":        128,
  "cols":        12
}
```

### Sheet JSON file format

```json
{
  "source_file":  "vendor/q4-2024.xlsx",
  "sheet_name":   "Sheet1",
  "extracted_at": "2026-04-29T08:00:00Z",
  "rows": [
    [ {"span": 6, "v": "Quarterly Report"}, null, null, null, null, null ],
    [ {"rspan": 2, "v": "Period"}, "Volume", "Total" ],
    [ null, 1234, [5678, "=SUM(B3:B5)"] ],
    null,
    [ "Q3 2024", "2024-09-30", 999 ]
  ]
}
```

### Meta JSON file format

```json
{
  "source_file":  "vendor/q4-2024.xlsx",
  "source_path":  "/data/in/sample.xlsx",
  "extracted_at": "2026-04-29T08:00:00Z",
  "sheets": [
    {"name": "Sheet1", "file": "Sheet1.json", "rows": 128, "cols": 12},
    {"name": "Notes",  "file": "Notes.json",  "rows": 8,   "cols": 2}
  ]
}
```

---

## Filename safety

Sheet names go through a small sanitiser before becoming filenames:
- Whitespace, `< > : " / \ | ? *` -> `_`
- Consecutive bad chars collapse to a single `_`

So a sheet named `"Q4 / 2024"` writes to `Q4_2024.json`. Unicode letters (Cyrillic, Chinese, accented latin) are preserved.

---

## Worked example: extract -> classify per sheet

```yaml
stages:
  list:
    tool: scan-fs
    settings: { include: ["*.xlsx"], hash: none }
    input: $input

  prep:
    tool: filter
    settings:
      ops:
        - map: "({path: item.path, output: '/data/extracted/', source_file: item.rel_path})"
      target: "row"
    input: list

  unpack:
    tool: xlsx
    input: prep

  read:
    tool: read-tables
    settings:
      formats: "./formats"
      mode: parse
    input: unpack

  sink:
    tool: write-file-stream
    settings: { default_file: "$output/rows.ndjson", format: ndjson }
    input: read
```

`xlsx` produces N envelopes (one per non-empty sheet across all workbooks); each is a self-describing sheet JSON file that `read-tables` can stream from.

---

## More

See [`docs/tools/xlsx.md`](../../docs/tools/xlsx.md) for advanced topics: very large workbooks (memory profile), date format detection edge cases, formula cells with array results.

## License

[AGPL-3.0-or-later](../../LICENSE).
