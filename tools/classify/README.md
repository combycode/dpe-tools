# classify

Universal rule-based classifier. Applies an ordered list of JS-expression rules against named dictionaries to attach `{className, category, confidence, reason, data}` to each envelope. First match wins.

The rule body is plain JavaScript run inside a sandboxed function; bindings expose dictionary lookups, hashing, string normalisation, and a configurable item path. No magic — your rules can be as complex as raw JS.

**When to use:**
- You have a stream of items (paths, names, free text) that need to be tagged with a class name.
- Your decision logic combines fuzzy string matching, dictionary lookups, regex, and dotted-path access.
- You want to keep the logic OUTSIDE Rust/TS code so non-engineers can edit it.

**When NOT to use:**
- You need ML-style classification (fine-tuned models). Use [`llm`](../llm) instead.
- Your rules are a one-liner regex -> use the runner's built-in `match` DSL.

---

## Install

```bash
dpe install classify
```

## How it works

For each envelope:

1. **Resolve the item.** If `settings.source` is set (e.g. `"v.path"`), the item is `envelope.v.path`. Otherwise the item is the whole `v`.
2. **Build bindings:** the item, the full envelope, the loaded dictionaries, the helpers (`normalize`, `alias`, `match`, `closest`, etc.).
3. **Run rules in order.** Each rule is a compiled function that returns one of:
   - A class name string -> classified.
   - `{className, category?, confidence?, reason?, data?}` -> classified with metadata.
   - `null` / `undefined` -> rule didn't match; try the next.
4. **First match wins.** Execution stops at the first rule that returns non-null.
5. **Write the result** to `envelope.v[settings.target]` (default `"class"`).
6. **Emit** the envelope downstream.

The result object always has shape:
```json
{
  "className":  "<class>" | null,
  "category":   "FOUND" | "NEW" | "UNKNOWN",
  "confidence": <0..100>,
  "reason":     "<rule-name or hint>",
  "data":       <any>
}
```

`category` is auto-derived:
- `"FOUND"` if `className` is in the loaded `classes` dictionary.
- `"NEW"` if the rule used `NEW(...)` or returned a string not in `classes`.
- `"UNKNOWN"` if the rule returned `UNKNOWN(...)`, the bare string `"UNKNOWN"`, or no rule matched.

---

## Settings reference

```yaml
# All fields:
classes:  "./dicts/classes.txt"          # optional; if set, used to derive `category`
dicts:                                    # named dictionaries available in rules
  aliases: "./dicts/aliases.tsv"          #   path string -> auto-detect format
  groups:                                 #   or { path, key } object for jsonl
    path: "./dicts/groups.jsonl"
    key:  "name"
rules:                                    # ordered list; first match wins
  - "./rules/exact-match.js"              # path to .js file (export default [{...}, ...])
  - "./rules/aliases.json"                # or .json file containing rule specs
  - name: "fuzzy-fallback"                # or inline {name, confidence, expr}
    confidence: 70
    expr: |
      const hit = closest(normalize(item), classes, 1);
      return hit ? { className: hit, reason: "fuzzy-1" } : null;
source: "v.path"                          # dotted path to the item; default = whole v
target: "class"                           # dotted path to write result; default "class"
```

| Field | Type | Default | Description |
|---|---|---|---|
| `classes` | string? | — | Path to a dictionary of known class names. Loaded as a flat dict; the first column of each row is the class name. Also exposed in rules as `dicts.classes`. |
| `dicts` | map? | `{}` | Named dictionaries. Each entry is either a path string (format auto-detected by extension) or an object `{path, key?}` for `.jsonl` files where `key` selects the primary-key field (default `"id"`). |
| `rules` | array | — | **Required.** Ordered list of rule specs. Each entry is either a path to a `.js`/`.json` file containing rule specs, or an inline `{name, confidence, expr}` object. File-based specs are flattened in declaration order. |
| `source` | string? | — | Dotted path on the envelope picking the item passed to rules. Default: whole `v`. |
| `target` | string | `"class"` | Dotted path inside `v` where the classification result is written. |

---

## Dictionary formats

`loadDict()` auto-detects by extension:

| Extension       | Type | Format |
|-----------------|------|--------|
| `.txt`          | flat | One row per non-empty line. Tab or ` : ` splits the row into columns. |
| `.tsv` / `.csv` | flat | Tab-split rows. (`.csv` uses tab too — historical, despite the name.) |
| `.json`         | obj  | Top-level object: `{ <primary-key>: <record>, ... }`. |
| `.jsonl` / `.ndjson` | obj | One JSON record per line. Primary key extracted from `record[<settings.dicts.<name>.key>]` (default `"id"`). |

**Flat row syntax** (auto-detected per line):
- Contains a tab -> tab-split.
- Contains ` : ` (space-colon-space) -> split on that.
- Else -> single-column row.

Blank lines and `#`-comment lines are ignored.

---

## Rule expression bindings

Rules run as JavaScript functions with these bindings:

### Item + envelope
| Binding | What it is |
|---|---|
| `item`  | The classified value (resolved from `source`). String, object, or whatever your envelope holds. |
| `env`   | The full envelope (`{t, id, src, v}`). Escape hatch for cross-field logic. |
| `classes` | `string[]` — primary keys of `dicts.classes` (empty if no `classes` setting). |
| `dicts` | `Record<string, Dict>` — every loaded dictionary keyed by its name. |

### String / path helpers
| Helper | Purpose |
|---|---|
| `normalize(s)` | Lowercase + collapse whitespace + strip punctuation + strip company suffixes (LTD / Limited / LLC / s.r.o. / etc.). Preserves Unicode letters (Cyrillic, etc.). |
| `lower(s)` | Just lowercases. |
| `strip(s, tokens)` | Remove each token (string or regex), case-insensitive. |
| `stripParens(s)` | Remove `(...)` groups. |
| `stripWords(s, dictName)` | Remove any of the dict's primary keys as whole words. |
| `replace(s, from, to)` | One-shot replace (string or regex). |
| `words(s, dictName?)` | Tokenise on whitespace + punctuation; if `dictName` given, dict entries are kept as single tokens. |
| `split(s, seps)` | Split on any char in `seps`. |
| `segment(s, idx, sep="/")` | Return the `idx`-th segment (negative = from end). |
| `fileName(s)` / `fileStem(s)` / `fileExt(s)` | Path basename / stem / extension. |
| `get(obj, path)` | Dot-path read on any object. |

### Matching helpers
| Helper | Purpose |
|---|---|
| `match(s, patterns, fuzzy=0)` | Return the first matching pattern (string or regex), or null. `fuzzy` allows N edit-distance for string patterns. |
| `closest(s, list, maxDist=2)` | Return the closest entry within `maxDist` edits, or null. |
| `include(s, list, method="strict")` | `{found, where}`-style result; methods: `"strict"`, `"fuzzy"`, `"fuzzy:N"`. |
| `contains(s, words)` | True if all words appear somewhere in `s`. |
| `hasPrefix(s, list)` / `hasSuffix(s, list)` | Self-explanatory. |
| `editDistance(a, b)` | Levenshtein. |

### Dictionary helpers
| Helper | Purpose |
|---|---|
| `keys(name)` | All primary keys of dict `name`. |
| `values(name, ...)` | Various forms; see `helpers.ts::valuesImpl`. |
| `entry(name, key)` | The full record for `key` (or null). |
| `members(name, field, value)` | All records whose `field` equals `value`. |
| `pairs(name, key, field?)` | Tuples for FlatDict introspection. |
| `alias(name, value)` | For a 2-column FlatDict, return col-1 where col-0 == value. |
| `aliasReverse(name, value)` | Inverse: col-0 where col-1 == value. |
| `lookup(name, key, def?)` | Like `entry` with default. |
| `dict(name)` | Raw `Dict` object (for advanced use). |

### Predicates / numeric
| Helper | Purpose |
|---|---|
| `inRange(v, from, to)` | Numeric range check. |
| `parseNumber(v)` | Returns `number | null`. |
| `empty(v)` | True if v is null / undefined / "" / [] / {}. |
| `has(obj, path)` | Like `get` but returns boolean. |
| `uniq(list)` | Dedup. |
| `count(list, pred?)` | Count predicate matches. |
| `hash(s)` | Blake2b-64 hex (matches the framework's hash). |
| `now()` | `Date.now()` ms. |

### Result builders
| Helper | Returns |
|---|---|
| `CLASS(name, data?)` | `{class: name, data}` — passed through to result. |
| `NEW(name, reason?, data?)` | `{new: name, reason, data}` — explicitly mark a not-yet-known class. |
| `UNKNOWN(reason?)` | `{unknown: true, reason}` — explicitly say "doesn't match anything". |

Plus standard JS globals: `JSON`, `Math`, `Date`, `Object`, `Array`, `String`, `Number`, `RegExp`, `parseInt`, `parseFloat`.

---

## Rule shape

Each rule is `{name, confidence, expr}`:

| Field | Type | Description |
|---|---|---|
| `name` | string | Identifier; appears in `result.reason` when this rule matches. |
| `confidence` | int 0-100 | Best-confidence-wins below 100; at 100 the rule short-circuits and immediately wins. |
| `expr` | string | JS body. If contains a `return` keyword, used verbatim. Otherwise wrapped as `function(...) { return (EXPR); }`. |

### Confidence semantics

- A rule returning at confidence **100** short-circuits the run — no later rules execute.
- A rule returning at confidence **< 100** is recorded but execution continues. Later rules with HIGHER confidence override; ties keep the first.
- A rule returning `null`/`undefined` is "no match"; next rule tried.
- After all rules: the highest-confidence match wins. If nothing matched, result is `{className: null, category: "UNKNOWN", reason: "no-rule-matched"}`.

---

## Worked example

Tag file paths as one of three classes by structural rules + fuzzy fallback.

**Layout:**
```
./classifier/
├── classes.txt                       # Alpha\nBravo\nCharlie
├── dicts/
│   ├── aliases.tsv                   # alf<TAB>Alpha\nbrv<TAB>Bravo
│   └── stop_words.txt                # demo, test, draft
└── rules/
    ├── 01-exact-name.js
    ├── 02-alias.js
    └── 03-fuzzy.js
```

**`./classifier/rules/01-exact-name.js`:**
```javascript
export default [{
  name: "exact-name",
  confidence: 100,
  expr: `
    const stem = fileStem(item);
    const norm = normalize(stem);
    if (classes.includes(norm)) return CLASS(norm, { source: "stem" });
    return null;
  `,
}];
```

**`./classifier/rules/02-alias.js`:**
```javascript
export default [{
  name: "alias-hit",
  confidence: 95,
  expr: `
    const stem = normalize(fileStem(item));
    const hit = alias("aliases", stem);
    return hit ? CLASS(hit, { source: "alias", from: stem }) : null;
  `,
}];
```

**`./classifier/rules/03-fuzzy.js`:**
```javascript
export default [{
  name: "fuzzy-class",
  confidence: 70,
  expr: `
    const norm = normalize(fileStem(item));
    const hit = closest(norm, classes, 1);
    return hit ? CLASS(hit, { source: "fuzzy", from: norm, distance: 1 }) : null;
  `,
}];
```

**Pipeline:**
```yaml
stages:
  scan:
    tool: scan-fs
    settings: { include: ["*.pdf"], hash: none }
    input: $input

  classify:
    tool: classify
    settings:
      classes:  "./classifier/classes.txt"
      dicts:
        aliases: "./classifier/dicts/aliases.tsv"
      rules:
        - "./classifier/rules/01-exact-name.js"
        - "./classifier/rules/02-alias.js"
        - "./classifier/rules/03-fuzzy.js"
      source: "v.path"
      target: "class"
    input: scan
```

For an envelope `{path: "/data/Alpha-2024.pdf"}`:
1. Rule 01 normalises stem `"Alpha-2024"` to `"alpha 2024"`. Not in classes -> null.
2. Rule 02 looks up alias "alpha 2024" -> not found -> null.
3. Rule 03 closest match within edit-distance 1 of "alpha 2024" against `["Alpha","Bravo","Charlie"]` -> "Alpha" (distance 0 after normalisation case-folds). Returns `CLASS("Alpha", { source: "fuzzy", ... })`.

Output envelope:
```json
{
  "path": "/data/Alpha-2024.pdf",
  "class": {
    "className":  "Alpha",
    "category":   "FOUND",
    "confidence": 70,
    "reason":     "fuzzy-class",
    "data":       { "source": "fuzzy", "from": "alpha 2024", "distance": 1 }
  }
}
```

---

## More

See [`docs/tools/classify.md`](../../docs/tools/classify.md) for advanced patterns: composing rules across multiple files, performance tuning for large dictionaries, debugging rule misses.

## License

[AGPL-3.0-or-later](../../LICENSE).
