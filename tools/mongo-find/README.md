# mongo-find

Streams documents out of a MongoDB collection as DPE envelopes. Two modes: dump everything once at startup, or run a parametrised query per input envelope.

**When to use:**
- You need data from a Mongo collection inside a streaming pipeline.
- You want either a full collection dump or per-envelope lookups (enrichment).
- You need to merge Mongo data with envelopes from another source (use `passthrough_input: true` to fan-out).

**When NOT to use:**
- You need writes -> use [`mongo-upsert`](../mongo-upsert).
- You need aggregation framework results -> not supported here; use a Mongo view or aggregation tool.
- You need real-time change streams -> not supported; this tool runs queries on demand only.

---

## Install

```bash
dpe install mongo-find
```

## How it works

### Mode `once` (default)

1. At startup, parse `settings.filter` into a Mongo BSON filter.
2. Wait for the first input envelope (or EOF).
3. Run **one** `find` with `filter` + optional `sort` / `projection` / `limit`.
4. Emit each matching document as an envelope (`v` = the document, BSON types relaxed-extjson-converted to plain JSON).
5. If `passthrough_input` is true, also emit each input envelope as-is BEFORE the matching docs.
6. Subsequent input envelopes either pass through (if `passthrough_input`) or are silently consumed.

### Mode `per_input`

1. For each input envelope, build a filter document by resolving each value in `settings.filter_from`:
   - If the value starts with `v.` or `env.`, it's a path expression resolved against the current envelope.
   - Otherwise it's a literal string.
2. Run `find(<resolved-filter>)` with the same `sort` / `projection` / `limit`.
3. Emit each matching document.
4. If `passthrough_input` is true, emit the input envelope BEFORE its matches.

```
mode = once:
   first input arrives ──▶ run find ──▶ emit all matches ──▶ pass-through subsequent inputs

mode = per_input:
   input ──▶ build filter from filter_from + envelope ──▶ run find ──▶ emit matches
```

---

## Settings reference

```yaml
# All fields:
uri:         "mongodb://localhost:27017"   # required
db:          "shop"                         # required
collection:  "orders"                       # required

mode:        "once"                         # "once" | "per_input"
filter:      { status: "shipped" }          # used in mode=once (literal Mongo doc)
filter_from:                                # used in mode=per_input
  customer_id: "v.customer_id"              #   each value is a path expr or literal
  region:      "EU"

sort:        { shipped_at: -1 }             # standard Mongo sort doc
projection:  { _id: 0, name: 1, total: 1 }  # standard Mongo projection
limit:       10000                          # per-query cap

passthrough_input: false                    # emit input envelopes BEFORE matches
```

| Field | Type | Default | Description |
|---|---|---|---|
| `uri` | string | — | Mongo connection URI. Use env vars for credentials; never bake them into pipeline YAML. |
| `db` | string | — | Database name. |
| `collection` | string | — | Collection name. |
| `mode` | enum | `"once"` | `"once"` (single query at startup) or `"per_input"` (one query per input envelope). |
| `filter` | object | `{}` | Literal Mongo filter doc (mode=once only). Empty = "find all". |
| `filter_from` | object | `{}` | Per-input filter template (mode=per_input only). Each key is a Mongo field; each value is a path expression (`v.foo` / `env.bar`) or a literal string. |
| `sort` | object? | — | Standard Mongo sort doc, e.g. `{day: 1}` (asc) or `{day: -1}` (desc). |
| `projection` | object? | — | Standard Mongo projection, e.g. `{_id: 0, name: 1}`. |
| `limit` | int? | — | Per-query document limit. |
| `passthrough_input` | bool | `false` | If true, each input envelope is emitted unchanged BEFORE any matched documents. Useful for fan-out enrichment patterns. |

---

## Path expressions in `filter_from`

Each value in `filter_from` is resolved per envelope:

| Form              | Meaning                                              |
|-------------------|------------------------------------------------------|
| `v.foo.bar`       | Resolve `envelope.v.foo.bar`; missing -> `null`     |
| `env.foo`         | Resolve `envelope.foo` (root level — `t`, `id`, `src`, `v`, etc.) |
| `v` or `env`      | Whole sub-tree                                       |
| any other string  | Treated as a LITERAL string                          |

So:
```yaml
filter_from:
  user_id: "v.user_id"      # path: per-envelope value
  region:  "EU"             # literal: same string for every query
  shard:   "env.id"         # path on the envelope's id field
```

If you need a literal that looks like a path, use the literal-detection escape: prepend something Mongo accepts (e.g. wrap in a regex `{$regex: "v\\.literal"}`) — or just don't use such values.

---

## Output envelope shape

Each emitted envelope has:
- `t: "d"` (data)
- `id`: framework-derived
- `src`: chained from the input envelope (or seeded if mode=once)
- `v`: the matched document, BSON types converted to relaxed extended JSON

BSON-to-JSON conversions worth knowing:
- `ObjectId(...)` -> `{"$oid": "<hex>"}`
- `ISODate(...)` -> `{"$date": "<iso>"}`
- `Decimal128(...)` -> `{"$numberDecimal": "<digits>"}`
- `Long(...)` -> `<integer>` (when in i64 range) or `{"$numberLong": "<digits>"}`

Filter values go the other way: numbers in `filter` / `filter_from` become: integers that fit a 64-bit signed integer -> `Int64`; larger integers (up to 18446744073709551615) -> an exact `Decimal128`; other numbers -> `Double`. JSON integers beyond that range already arrive as floating point when the envelope is parsed.

If you need flat strings, normalise downstream (e.g. with a `filter`/`map` op extracting `_id.$oid`).

---

## Worked example: dump-then-process

Read a whole collection, then filter and write to disk.

```yaml
stages:
  source:
    tool: mongo-find
    settings:
      uri: "mongodb://${MONGO_USER}:${MONGO_PASS}@mongo:27017"
      db: "shop"
      collection: "orders"
      mode: once
      filter: { status: "shipped" }
      sort:   { shipped_at: -1 }
      limit:  10000
    input: $input

  pick:
    tool: filter
    settings:
      ops:
        - filter: "item.total > 100"
        - map:    "({id: item._id.$oid, total: item.total, when: item.shipped_at.$date})"
      target: ""
    input: source

  sink:
    tool: write-file-stream
    settings: { default_file: "$output/big-orders.ndjson", format: ndjson }
    input: pick
```

## Worked example: per-input enrichment

For each event in a stream, look up the matching record and emit the joined pair.

```yaml
stages:
  events:
    tool: read-file-stream
    settings: { format: ndjson }
    input: $input

  enrich:
    tool: mongo-find
    settings:
      uri: "mongodb://${MONGO_USER}:${MONGO_PASS}@mongo:27017"
      db: "shop"
      collection: "users"
      mode: per_input
      filter_from:
        _id: "v.user_id"
      projection:
        _id: 0
        email: 1
        plan: 1
      passthrough_input: true        # emit the event first, then its match
    input: events
```

Output: each input event followed immediately by zero-or-one user document. Downstream stages can pair them by `src` chain.

---

## More

See [`docs/tools/mongo-find.md`](../../docs/tools/mongo-find.md) for advanced patterns: cursor batching tuning, read-preference for replica sets, change-stream alternatives.

## License

[AGPL-3.0-or-later](../../LICENSE).
