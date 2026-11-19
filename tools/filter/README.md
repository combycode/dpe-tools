# filter

Filter / map / transform an NDJSON stream via JS expressions. A more flexible companion to the runner's built-in `filter` (which uses the typed accumulator DSL).

**When to use:**
- The built-in `filter` DSL is too restrictive for your predicate (e.g. you need regex, JSON parsing, dot-paths into deeply-nested structures).
- You want to pipeline several filter + map operations through one tool, in declared order.
- You need to drop or pass through metadata envelopes selectively.

**When NOT to use:**
- Your predicate fits the built-in DSL — the built-in is in-process (zero subprocess spawn, no JS overhead).
- You need to call out to external services or accumulate state across envelopes — those need a real tool, not a filter.

---

## Install

```bash
dpe install filter
```

## How it works

For each envelope:

1. **Check `apply_to`.** If the envelope's type isn't allowed (e.g. `apply_to: "data"` and the envelope is meta), the envelope is **dropped** — NOT passed through. To pass meta envelopes through unchanged, use `apply_to: "both"` and add no ops that target meta.
2. **Pick the item:** `item = v[settings.target]` (default `target: "row"`). If `target` is empty/omitted, `item = v` (the whole v).
3. **Run ops top-to-bottom:**
   - `filter`: evaluate expression against `(item, env, hash, now)`. If falsy, **drop the envelope**. If truthy, continue.
   - `map`: evaluate expression. If returns `null` or `undefined`, **drop**. Otherwise the returned value REPLACES `item`.
4. **Survivors emit** with `item` written back to `v[target]` (or to `v` if target is empty).

```
   envelope
     │
     ▼
┌──────────────────────┐
│ apply_to allows?     │── no ──▶ drop
└──────────┬───────────┘
           │ yes
           ▼
   item = v[target]
           │
           ▼
   ┌─── ops ───┐
   │ filter ?  │── falsy ──▶ drop
   │ map ?     │── null  ──▶ drop
   │ filter ?  │            (chain top-to-bottom)
   │ ...       │
   └─────┬─────┘
         ▼
   v[target] = item
         ▼
       emit
```

---

## Settings reference

```yaml
ops:                                # required, >=1 entry
  - filter: "item.amount >= 0"      # drop envelopes failing the predicate
  - map:    "({...item, ratio: item.amount / item.total})"
                                    # transform; null/undefined => drop
target:    "row"                    # which v field to pass as `item`. "" = whole v
apply_to:  "data"                   # "data" | "meta" | "both"
```

| Field      | Type   | Default  | Description |
|------------|--------|----------|-------------|
| `ops`      | array  | —        | **Required.** Ordered list of `{filter: "<expr>"}` or `{map: "<expr>"}` entries. Each entry must have exactly one of `filter` / `map`. |
| `target`   | string | `"row"`  | Field of `v` that ops operate on. Empty string = whole `v`. After all ops, the result is written back to `v[target]` (or replaces `v` if empty). |
| `apply_to` | enum   | `"data"` | Which envelope types reach the ops. `"data"` = only data envelopes; meta envelopes are DROPPED. `"meta"` = only meta. `"both"` = data and meta. |

---

## Expression form

Each `filter` / `map` value is a JS expression string. Two forms:

| Form                                    | Wrapping |
|-----------------------------------------|----------|
| Simple expression (no `return`)         | Wrapped as `function(item, env, hash, now) { return (EXPR); }` |
| Body containing `return`                | Used verbatim as the function body |

Example simple form: `"item.amount >= 0"`.

Example body form (multi-line):
```yaml
ops:
  - map: |
      const a = item.amount || 0;
      const b = item.total  || 1;
      return { ...item, ratio: a / b };
```

---

## Bindings

| Binding   | What it is |
|---|---|
| `item`    | The target of operations (`v[target]` or `v` if target is empty). |
| `env`     | The full envelope (`{t, id, src, v}`). Escape hatch when you need cross-field state. |
| `hash(s)` | Blake2b-64 hex hash. Matches the framework's `hash` helper so two stages computing the same id agree. |
| `now()`   | `Date.now()` ms. |

Plus standard JS globals: `JSON`, `Math`, `Date`, `Object`, `Array`, `String`, `Number`, `RegExp`, `parseInt`, `parseFloat`.

**Important:** the expression is sandboxed only by JavaScript scoping — `Function` constructor, but no Node `process`/`fs` access. Don't feed untrusted settings to this tool; they're effectively code execution.

---

## `apply_to` matrix

| `apply_to` | Data envelope arrives | Meta envelope arrives |
|------------|-----------------------|------------------------|
| `"data"` (default) | runs through ops | **dropped** |
| `"meta"`           | **dropped**       | runs through ops |
| `"both"`           | runs through ops | runs through ops |

If you want meta envelopes to pass through unchanged regardless of ops, set `apply_to: "data"` and place a separate pass-through stage in your pipeline for meta — or use the runner's built-in route instead.

---

## Examples

### Drop rows with negative amounts
```yaml
target: "row"
ops:
  - filter: "item.amount >= 0"
```

### Chain filter + map
```yaml
target: "row"
ops:
  - filter: "item.status === 'active'"
  - map:    "({...item, score: item.value * item.weight})"
```

### Drop everything but a known set of statuses
```yaml
target: "row"
ops:
  - filter: |
      const ok = ["active", "pending", "done"];
      return ok.includes(item.status);
```

### Operate on the whole v (no target)
```yaml
target: ""
ops:
  - filter: "env.t === 'd' && Object.keys(item).length > 0"
```

### Transform numeric strings to numbers
```yaml
ops:
  - map: |
      const parsed = parseFloat(item.value);
      if (Number.isNaN(parsed)) return null;        // drop
      return { ...item, value: parsed };
```

### Pass meta envelopes through with their counters scrubbed
```yaml
apply_to: "meta"
target: ""
ops:
  - map: |
      const { secrets, ...clean } = item;
      return clean;
```

---

## Performance notes

- Each envelope is a JS function call — much faster than a subprocess spawn but slower than the runner's built-in `filter`.
- The expression is compiled ONCE at startup; `Function` cost amortises over the run.
- Avoid heavy work inside the expression (large regexes, deep object copies). If you need that, use [`classify`](../classify) or a custom tool.

---

## More

See [`docs/tools/filter.md`](../../docs/tools/filter.md) for additional patterns: stateful aggregators (with caveats), feeding the `hash` binding into mongo-upsert keys, debugging dropped envelopes.

## License

[AGPL-3.0-or-later](../../LICENSE).
