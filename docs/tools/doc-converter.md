# doc-converter

> Extended documentation for the [`doc-converter`](../../tools/doc-converter) tool. See the tool README for the canonical settings reference; this page covers worked examples, edge cases, and rationale.

## Re-converting documents a later stage rejected

A schema-validation stage downstream of doc-converter (e.g. the [`validate`](validate.md) tool) may reject a converted document because the model hallucinated a frontmatter value or the shape drifted. A re-run would normally serve the same result from the cache, so the rejected documents have to be cleared first. How depends on the output mode:

- **`output.file` (Mode A).** The written markdown file IS the cache entry, and `v.path` on the output envelope is that file. Delete it (plus its `<file>.dpe.json` sidecar under `invalidation: sidecar`) and the next run converts the document again.
- **`output.field` (Mode B).** The result is cached by the framework under a key the envelope does not expose, and `v.path` is still the INPUT document, so do not delete it. Re-run those inputs with `DPE_CACHE_MODE=refresh`, which skips the cache read and overwrites the entries.

```bash
# Mode A: the pipeline routes rejected documents to a sink (wiring below);
# after the run, delete their files and re-run.
jq -r '.path' data/output/invalid_documents.ndjson | xargs -r rm -f
```

### Pipeline wiring example

Combined with `validate` for the validate-quarantine-reclaim flow:

```yaml
convert:
  tool: doc-converter
  settings: { ... }
  input: scan

validate-fm:
  tool: validate
  settings:
    source: v
    schema: "$configs/convert/schemas/frontmatter.schema.json"
    mode:   mark            # adds v._valid + v._validation_error
  input: convert

route-fm:
  tool: route
  routes:
    valid:   "v._valid == true"
    invalid: "v._valid == false"
  input: validate-fm

sink-invalid:
  tool: write-file-stream
  settings:
    default_file: "$output/invalid_documents.ndjson"
    format:       ndjson
  input: route-fm.invalid
```

The `sink-invalid` file accumulates the rejected envelopes; in Mode A each one carries the `path` of its converted file.

---

## More

See [`tools/doc-converter/README.md`](../../tools/doc-converter/README.md) for the canonical settings reference, prompt composition rules, dispatch strategy decision table, and batch-mode lifecycle.

## License

Part of [`dpe-tools`](https://github.com/combycode/dpe-tools). AGPL-3.0-or-later.
