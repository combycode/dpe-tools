# mongo-upsert

Streaming MongoDB upsert. Each envelope produces one `updateOne` with a configurable mix of update operators applied atomically. Bulk-batched by default, pass-through downstream so the tool can sit mid-pipeline.

**When to use:**
- You have a stream of envelopes that must land in a MongoDB collection.
- The same key may appear multiple times in the stream (you want upsert, not append).
- You need to set, push, increment, and unset different fields on the same document in a single call.

**When NOT to use:**
- You need pure inserts (no merging) -> use the Mongo driver directly or write a thin tool.
- You need transactions across documents -> each update is a separate `updateOne`; use a transaction-aware writer instead.

---

## Install

```bash
dpe install mongo-upsert
```

## How it works

Per envelope:

1. **Build the filter document** from `settings.key`. Each `key[<field>]` value is either a path expression (starts with `v.` / `env.`) resolved against the envelope or a literal string. Result: `{ field1: <resolved>, field2: <resolved>, ... }`.
2. **Build the update document** from `settings.operations`. Operations are merged so multiple ops on the same operator collapse into one sub-document (Mongo rejects `{$set: {a:1}, $set: {b:2}}` — they must be `{$set: {a:1, b:2}}`).
3. **Resolve each operation's value:** `op.literal` wins over `op.value`; `op.value` is treated as a path expression if it starts with `v.` or `env.`, else as a literal string. `$unset` always uses `true`.
4. **Enqueue** filter + update into an in-memory bulk batch.
5. **Flush** the batch (concurrent `updateOne` calls; the unified client `bulkWrite` needs MongoDB 8) when:
   - Batch size reaches `bulk_size`, OR
   - The tool receives EOF on stdin.
6. **Emit input envelopes downstream UNCHANGED** as they're enqueued. Downstream sees envelopes BEFORE the batch has actually committed — useful for chaining, but **not a write-confirmation signal**. If you need confirmed writes, sink the output of this tool to a terminal stage with `bulk_size: 1`.

```
   envelope (one of N)
     │
     ▼
┌──────────────────────────┐
│ build filter from key    │   each value: path (v.x) or literal
│ build update from ops    │   merged per operator ($set, $addToSet, ...)
└──────────┬───────────────┘
           ▼
   ┌─── bulk batch ───┐
   │  (capacity:      │
   │   bulk_size)     │
   └────────┬─────────┘
            │  full or EOF
            ▼
       Mongo updateOne x N
            │
            ▼  (write errors handled per `on_error`)
       (envelope already emitted downstream when enqueued)
```

---

## Settings reference

```yaml
# All fields:
uri:         "mongodb://localhost:27017"   # required
db:          "shop"                         # required
collection:  "orders"                       # required

key:                                        # required, >=1 entry
  order_id:  "v.order_id"                   # path expression
  region:    "EU"                           # literal string

operations:                                 # required, >=1 entry
  - op:    "$set"
    path:  "updated_at"
    value: "v.now"
  - op:    "$addToSet"
    path:  "tags"
    value: "v.tag"
  - op:    "$inc"
    path:  "view_count"
    literal: 1
  - op:    "$set"
    path:  "status"
    literal: "active"

upsert:    true                             # default true
bulk_size: 500                              # batch size; 1 disables batching
on_error:  "trace"                          # "trace" | "drop" | "fail"
```

| Field | Type | Default | Description |
|---|---|---|---|
| `uri` | string | — | MongoDB connection URI. **Use env vars for credentials** (e.g. `mongodb://${MONGO_USER}:${MONGO_PASS}@host`); never bake secrets into pipeline YAML. |
| `db` | string | — | Database name. |
| `collection` | string | — | Collection name. |
| `key` | object | — | Filter doc. Each value is either a path expression (starts with `v.` or `env.`) resolved per envelope, or a literal string. The filter must uniquely identify the target document for `upsert: true` to be meaningful. |
| `operations` | array | — | List of update ops. Multiple ops on the same operator are merged. See [Operators](#operators) below. |
| `upsert` | bool | `true` | Standard Mongo upsert flag. `false` means non-existing keys are silently skipped. |
| `bulk_size` | int | `500` | Number of envelopes per batch flush. `1` disables batching (one round-trip per envelope). Larger = fewer round-trips, more memory. |
| `on_error` | enum | `"trace"` | `"trace"` (log + continue), `"drop"` (silently skip the failing op), `"fail"` (terminate the tool). Applies to write-side errors only — connection / auth failures always terminate. |

### Value types

JSON numbers become BSON as: integers that fit a 64-bit signed integer -> `Int64`; larger integers (up to 18446744073709551615) -> an exact `Decimal128`; other numbers -> `Double`. JSON integers beyond that range already arrive as floating point when the envelope is parsed. The same applies to `key` values, so an upsert matches the document with exactly that key.

---

## Operators

Supported: `$set`, `$addToSet`, `$push`, `$inc`, `$unset`.

| Operator     | Semantics                                       | Idempotent? |
|--------------|-------------------------------------------------|-------------|
| `$set`       | Replace the field with the given value          | yes         |
| `$addToSet`  | Append to an array IFF value not already there  | yes         |
| `$unset`     | Remove the field                                | yes         |
| `$push`      | Append unconditionally (even on duplicate runs) | **NO**      |
| `$inc`       | Increment by N                                  | **NO**      |

Each operation has these fields:

| Field     | Required | Description |
|---|---|---|
| `op`      | yes      | One of the operators above. |
| `path`    | yes      | Dot-path inside the document, e.g. `tags`, `metadata.last_seen`, `counts.alpha`. |
| `value`   | no       | Path expression (resolved per envelope) OR literal string. Ignored for `$unset`. |
| `literal` | no       | Literal value override (any JSON type). If both `value` and `literal` are set, `literal` wins. Useful for fixed timestamps, counters (`literal: 1`), enum values. |

**Multiple ops same operator:**
```yaml
operations:
  - op: "$set"
    path: "updated_at"
    value: "v.now"
  - op: "$set"
    path: "status"
    literal: "active"
```
becomes a single Mongo update: `{$set: {updated_at: <now>, status: "active"}}`.

**Idempotency rule:** prefer `$set` + `$addToSet` + `$unset` for any pipeline that may replay (recovery, retries, reprocessing the same input). Re-running the same envelope through an `$inc: 1` will double-count; through `$push` will duplicate the array entry. If you must use `$push` / `$inc`, dedup upstream (e.g. via `gate`/`checkpoint` or a `$set`-only "seen" marker on a separate field).

---

## Path expressions

Values in `key.<field>` and `operations[].value` follow these rules:

| Form              | Meaning                                              |
|-------------------|------------------------------------------------------|
| `v.foo.bar`       | Resolve `envelope.v.foo.bar`, default `null` if missing |
| `env.foo`         | Resolve `envelope.foo` (root level — `t`, `id`, `src`, `v`, etc.) |
| `v` or `env`      | Whole sub-tree                                       |
| any other string  | Treated as a LITERAL string value                    |

This means:
```yaml
key:
  order_id: "v.order_id"     # path: looks up envelope.v.order_id
  region:   "EU"             # literal: stored as the string "EU"
```

If you need a literal that LOOKS like a path (e.g. you really want the string `"v.foo"` stored), use the operation's `literal:` field instead of `value:`.

---

## Error handling (`on_error`)

| Mode      | Behaviour |
|-----------|-----------|
| `trace` (default) | Log the failing envelope's id + the error to stderr, continue with the next envelope. The bulk batch flushes the rest. |
| `drop`    | Same as `trace` but no log line — the envelope is silently dropped from the bulk write. |
| `fail`    | Tool exits with non-zero status. The runner sees the failure and stops the pipeline. |

Connection / auth failures (handshake, missing collection on a sharded cluster) always terminate regardless of `on_error` — they're not per-document errors and retrying isn't going to help.

---

## Worked example: idempotent replay

Pipe normalised input rows into Mongo with replay-safe semantics. Each input row carries `id`, `amount`, `seen_at`, and a free-form `tag`.

```yaml
stages:
  read:
    tool: read-file-stream
    settings: { format: ndjson }
    input: $input

  upsert:
    tool: mongo-upsert
    settings:
      uri: "mongodb://${MONGO_USER}:${MONGO_PASS}@mongo:27017"
      db: "metrics"
      collection: "rows"
      key:
        id: "v.id"                 # uniquely identifies the document
      operations:
        - op:    "$set"            # last-seen wins
          path:  "amount"
          value: "v.amount"
        - op:    "$set"            # last-seen wins
          path:  "updated_at"
          value: "v.seen_at"
        - op:    "$addToSet"       # accumulate tags without duplication
          path:  "tags"
          value: "v.tag"
        - op:    "$set"            # mark the row as processed (idempotent)
          path:  "processed"
          literal: true
      bulk_size: 200
      on_error: "trace"
    input: read
```

Re-running this pipeline with the same input is safe: all ops are idempotent. The `tags` array won't duplicate entries; `amount` and `updated_at` simply get re-written; `processed` stays `true`.

If you need an actual counter (e.g. "how many times has this id been seen"), DON'T use `$inc` here — it'll double-count on replay. Either dedup upstream or use a `$set`-with-max pattern.

---

## More

See [`docs/tools/mongo-upsert.md`](../../docs/tools/mongo-upsert.md) for advanced patterns: shard-key-aware filtering, custom write concerns, transaction wrappers.

## License

[AGPL-3.0-or-later](../../LICENSE).
