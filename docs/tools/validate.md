# validate

> Extended documentation for the [`validate`](../../tools/validate) tool. See the tool README for the canonical settings reference; this page covers worked examples, edge cases, and rationale.

## What it does

Validates a value addressable on the envelope's `v` (the whole `v` object, or a dotted-path field like `v.response`) against a JSON Schema. Operates in one of two modes:

- **`mark`** — adds `_valid: bool` + `_validation_error: string|null` to `v`; emits every envelope on `data` regardless of result. Downstream stages route on `v._valid`.
- **`filter`** — emits valid envelopes verbatim; invalid ones go to `ctx.error()` and never reach `data`. Use when invalid items should silently flow to the errors log.

## Why a dedicated tool

`normalize` enforces field-level transforms (rename, dict replace, parse, template, compute). It doesn't model whole-envelope shape. `validate` fills that gap with JSON Schema — the standard contract language for declaring what a structured value should look like.

In LLM pipelines especially, the post-LLM response is a string that's *supposed* to be valid JSON of a specific shape. Today the pattern is "parse manually inside a normalize rule and hope". `validate` makes that contract explicit and gives downstream a `_valid` boolean to route on.

## Settings

See [`tools/validate/README.md`](../../tools/validate/README.md) for the full reference. Key knobs:

- `source` — dotted path. `v` = whole object. `v.response` = read string field and parse as JSON before validating.
- `schema` — JSON or YAML schema file path; resolved via `$prefix` substitution by the runner.
- `additional_properties: true` (default) — strips root-level `additionalProperties: false` from the loaded schema BEFORE compilation. Lets the schema target only the fields it cares about without breaking on upstream-added envelope fields. Nested `additionalProperties` declarations are not touched.
- `mode` — `mark` (default) or `filter`.
- `mark_field` / `error_field` — override default field names (`_valid` / `_validation_error`).
- `format` — how to parse `source` when it resolves to a string. Currently `json` only.

## Error format

`_validation_error` follows:

```
<json-pointer>: <keyword> — <human detail>
```

The pointer is RFC-6901 (`/foo/bar`) and `/` for the root. The keyword is the lowercased JSON-Schema keyword name (`enum`, `required`, `type`, `pattern`, ...). The detail is the underlying validator's message.

Examples:

| Failure | Error |
|---|---|
| Missing required field | `"/: required — \"document_type\" is a required property"` |
| Enum violation | `"/document_type: enum — \"UNKNOWN\" is not one of \"report\" or \"memo\""` |
| Type mismatch | `"/amount: type — 123 is not of type \"string\""` |
| Source not JSON | `"parse error: expected value at line 1 column 5"` |
| Path missing on v | `"source path 'v.response' not found"` |

In `filter` mode the same string is what `ctx.error(v, reason)` carries; you'll see it on stderr as the `error` field of the structured error envelope.

## Worked example — frontmatter validation after conversion

`sanitize-fm` produces an envelope whose `v` has frontmatter fields promoted (e.g. `document_type`, `document_id`, `revision`). The schema lives next to the variant config:

```yaml
# variants/convert.yaml
validate-fm:
  tool: validate
  settings:
    source: v
    schema: "$configs/convert/schemas/frontmatter.schema.json"
    mode:   mark
  input: sanitize-fm

route-fm:
  tool: route
  routes:
    valid:   "v._valid == true"
    invalid: "v._valid == false"
  input: validate-fm
```

```json
# configs/convert/schemas/frontmatter.schema.json
{
  "type": "object",
  "required": ["document_type", "document_id"],
  "properties": {
    "document_type": { "enum": ["report", "memo"] },
    "document_id":   { "type": "string", "minLength": 1 },
    "revision":      { "type": "string" }
  }
}
```

Valid envelope ⇒ `v._valid: true`, `v._validation_error: null`, routed to the `valid` channel. Invalid ⇒ `v._valid: false`, `v._validation_error: "/document_type: enum — ..."`, routed to `invalid`. The `invalid` channel typically feeds a writer that keeps the rejected envelopes, so a cleanup pass can clear their cached conversions and re-run (see [doc-converter](doc-converter.md#re-converting-documents-a-later-stage-rejected)).

## Worked example — LLM response shape check

After an LLM stage that puts its raw response in `v.response`:

```yaml
validate-summary:
  tool: validate
  settings:
    source: v.response          # string field — parsed as JSON first
    schema: "$configs/convert/schemas/summary.schema.json"
    mode:   mark
  input: llm-summary
```

If `v.response` isn't valid JSON, `_valid: false` and `_validation_error: "parse error: ..."`. If it parses but fails schema, the validator's first violation surfaces with pointer + keyword.

## `additional_properties` mechanics

The flag exists because upstream stages routinely add fields the schema doesn't declare (`cache_hit`, `path`, `hash`, internal markers). A schema that says `additionalProperties: false` at the root would reject every envelope just because the upstream tooling adds tags.

When `additional_properties: true` (default):
- Tool reads the schema text.
- If root is an object schema with `additionalProperties` set, that key is REMOVED from the root.
- Nested subschemas keep their own `additionalProperties` declarations untouched.

When `additional_properties: false`:
- Schema is compiled verbatim. If the author wrote `additionalProperties: false` at the root, unknown fields will fail validation.

Useful pattern: keep `additional_properties: true` in normal pipelines; flip to `false` when running a strict validation pre-commit hook on hand-authored fixtures.

## JSON Schema dialect

Detected from the schema's `$schema` keyword; without one, Draft 2020-12 applies (`jsonschema` crate 0.61). Covers `enum`, `const`, `required`, `pattern`, `type`, `minLength` / `maxLength` / `minimum` / `maximum`, `oneOf` / `anyOf` / `allOf` / `not`, `if/then/else`, `contains`, `propertyNames`, etc. `format` is only an annotation under Draft 2019-09 / 2020-12 (a value that does not match is accepted); declare Draft 4, 6 or 7 in `$schema` to have `format` (e.g. `email`, `date-time`) enforced.

## Schema-file caching

The compiled validator is cached in a `Mutex<Option<Cached>>` static keyed on `(path, additional_properties)`. The cache invalidates only when one of those two changes mid-run — which shouldn't happen, since a single stage instance has fixed settings.

YAML schema files are accepted alongside JSON; extension `.yaml` / `.yml` triggers YAML parsing via `serde-saphyr` (workspace policy). Anything else parses as JSON.

## Performance notes

- Schema compile cost is one-shot per stage run.
- Per-envelope validation is the cost of walking the schema. For large schemas (hundreds of properties), this dominates; for typical 5-20-field schemas it's microseconds.
- `iter_errors` is stopped on first violation (`next()`) so cost is bounded by the validator's traversal to the first failing keyword.

## License

Part of [`dpe-tools`](https://github.com/combycode/dpe-tools). AGPL-3.0-or-later.
