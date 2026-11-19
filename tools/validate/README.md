# validate

DPE tool: JSON Schema validator. Tags every envelope with `_valid` / `_validation_error` (mark mode), or drops invalid envelopes to the errors log (filter mode).

## Why

Polymorphic LLM pipelines need a way to declare the expected shape of a normalised envelope (or a JSON-string field on it) and route subsequent stages by validity. `normalize` enforces field-level transforms but doesn't model whole-document structure; this tool fills that gap.

## Quick example

```yaml
category-validate-fm:
  tool: validate
  settings:
    source: v
    schema: "$configs/convert/schemas/frontmatter.schema.json"
    mode:   mark
  input: category-sanitize-fm

category-route-fm:
  tool: route
  routes:
    valid:   "v._valid == true"
    invalid: "v._valid == false"
  input: category-validate-fm
```

## Settings

```yaml
tool: validate
settings:
  # Required.
  source: v               # dotted path; "v" = whole v object,
                          # "v.response" = read string-then-parse, etc.
  schema: "$configs/x.json"   # JSON or YAML schema file (resolved by runner)

  # Optional.
  format: "json"          # how to parse when `source` is a string (default json)
  mode:   "mark"          # mark | filter (default: mark)
  mark_field:   "_valid"            # field name written on v in mark mode
  error_field:  "_validation_error" # error string field (null on success)
  additional_properties: true       # strip root additionalProperties:false from schema
```

### Modes

| Mode | Behaviour |
|---|---|
| `mark` | adds `v._valid: bool` + `v._validation_error: string\|null` to every envelope; emits all on `data` |
| `filter` | valid → emitted unchanged on `data`; invalid → `ctx.error(v, reason)`, NOT emitted |

Use `mark` when you need downstream routing on validity. Use `filter` when invalid items should fail out silently to the errors log.

### `additional_properties`

When `true` (default), the validator strips any root-level `additionalProperties` keyword from the schema before compilation, so envelopes with extra upstream-added fields (`cache_hit`, `path`, hashes, etc.) don't fail validation. Set `false` to enforce the schema verbatim — useful for strict shape validation where any unknown field is a bug.

Nested `additionalProperties` declarations are NOT touched — strict nested objects under a permissive root work as expected.

## Schema files

- JSON or YAML — extension `.yaml` / `.yml` triggers YAML parsing (via `serde-saphyr`, workspace policy). Anything else is parsed as JSON.
- Path is resolved by the runner's `$prefix` substitution (`$configs`, `$pipeline`, etc.) before the tool starts.
- Loaded ONCE per stage run and cached in memory. The cache invalidates if the schema path or `additional_properties` flag changes mid-run (which shouldn't happen — a single stage uses a fixed schema).

JSON Schema draft taken from the schema's `$schema`; without one, Draft 2020-12 (covers `if/then/else`, `contains`, `not`, `const`, `enum`). Under 2020-12 `format` is an annotation only; declare Draft 4, 6 or 7 to enforce it.

## Error format

`_validation_error` follows the shape:

```
<json-pointer>: <keyword> — <human detail>
```

Examples:
- `/document_type: enum — "value 'UNKNOWN' is not one of \"alpha\", \"beta\""`
- `/: required — \"plan_id\" is a required property`
- `parse error: expected value at line 1 column 5` (when `source` is a string and `format: json` parsing fails)

## License

Part of [`dpe-tools`](https://github.com/combycode/dpe-tools). AGPL-3.0-or-later.
