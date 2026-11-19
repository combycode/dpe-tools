# llm

One LLM call per envelope. Layered system prompts. Output as text (preserves other v fields) or JSON (replaces v with the parsed object).

**When to use:**
- You have one envelope = one logical "ask the model" call.
- The system prompt varies based on tags carried on the envelope (e.g. category/kind/language).
- You want the result either merged into the envelope as `response` (text mode) or used as the new envelope payload (json mode).

**When NOT to use:**
- You need vision/image input -> use [`doc-converter`](../doc-converter) which renders pages to JPEGs and feeds an LLM.
- You need streaming token output -> not supported here; this tool is single-shot.
- You need to merge multiple inputs into one prompt -> wire an aggregator upstream first.

---

## Install

```bash
dpe install llm
```

## API keys (env, NOT settings)

| Provider    | Env vars                              |
|-------------|---------------------------------------|
| `anthropic` | `ANTHROPIC_API_KEY`                   |
| `google`    | `GEMINI_API_KEY` or `GOOGLE_API_KEY`  |

Keys are NEVER passed via settings — that would put them in pipeline YAML and journals.

---

## How it works

For each input envelope:

1. **Resolve the user content** from `settings.input_path`. The path is a dotted expression rooted at `v` (`"v"` = the whole `v` field; `"v.markdown"` = `v.markdown`; `"v.a.b.c"` = nested). Strings pass through; non-strings are JSON-stringified.
2. **Compose the system prompt** (see [Prompts](#prompts) below): `base + tag overlays`, joined with `\n\n---\n\n`. There are no page-range overlays here (that's a `doc-converter` feature; llm is single-page).
3. **Call the provider** (`provider.messageSingle(system, userText, modelCfg)`). The provider impl handles authentication, retries on transient errors, and returns the raw text response.
4. **Strip code fences** from the response: a leading ` ```markdown ` / ` ```json ` and trailing ``` ``` are removed. Whitespace trimmed.
5. **Emit per `output` mode:**
   - **`text`**: wrap as `{response: <stripped-text>}` and merge with input v's other fields. The original input field at `input_path` is removed (otherwise the envelope grows unboundedly through a long pipeline).
   - **`json`**: parse the stripped text as JSON. Replace v entirely with the parsed value. (Useful when the LLM is the sole structured-output source.)

```
   v
   │
   ▼
┌──────────────────────┐
│ resolve input_path   │  -> userText (string or stringified)
└──────────┬───────────┘
           ▼
┌──────────────────────┐
│ compose system prompt│  base + tag overlay 1 + tag overlay 2 + ...
└──────────┬───────────┘
           ▼
┌──────────────────────┐
│ provider.messageSingle│  retries on transient errors (per provider)
└──────────┬───────────┘
           ▼
       strip fences
           ▼
   ┌───────┴────────┐
   │                │
text mode        json mode
   │                │
{response, ...}   parsed JSON replaces v
```

---

## Settings reference

```yaml
# All fields:
provider:        "anthropic" | "google"   # required
model:           string                    # required, e.g. "claude-haiku-4-5-20251001"
temperature:     0.0                       # provider-dependent; see notes below
max_tokens:      64000
thinking_budget: null                      # int, provider-specific reasoning tokens

prompts:
  dir:      "./prompts"                    # required
  name:     "summarise"                    # required, base file name
  version:  "v1"                           # required
  overlays: ["category", "kind"]           # optional; ordered list of overlay tag names

overlay_defaults: { kind: "default" }      # fallback values per name when envelope lacks the field

input_path:     "v.markdown"               # required; dotted path rooted at "v"
output:         "text"                     # required; "text" | "json"

retry:
  max_retries: 3
  delays_ms:   [500, 2000, 5000]

on_failure:      "stderr"                  # "stderr" (default) | "envelope" | "both"
                                           # See Failure handling & retry loop below.
```

| Field | Type | Default | Description |
|---|---|---|---|
| `provider` | enum | — | `anthropic` or `google`. Selects the API client; the key is read from env. |
| `model` | string | — | Provider-specific model identifier. |
| `temperature` | float | `0.0` | Sampling temperature. Provider-dependent range — see [Temperature ranges per provider](#temperature-ranges-per-provider). |
| `max_tokens` | int | `64000` | Max response tokens (provider-clamped if higher than the model allows). |
| `thinking_budget` | int? | `null` | Reasoning-token budget for providers that support extended thinking. **Anthropic constraint:** if set on Anthropic, `temperature` is forced to `1.0` by the provider impl (the API rejects other values when thinking is enabled). |
| `prompts.dir` | string | — | Directory holding the prompt files. Absolute or relative to runner cwd. |
| `prompts.name` | string | — | Base prompt name. The base file is `<name>_<version>.txt`. |
| `prompts.version` | string | — | Version tag, used in every filename. |
| `prompts.overlays` | string[] | `[]` | Ordered list of overlay tag NAMES. For each name, the per-envelope value is resolved as: (1) `envelope.v[name]`, else (2) `overlay_defaults[name]`. There are no special hard-coded names. |
| `overlay_defaults` | map | `{}` | Fallback values per overlay name when the envelope doesn't carry the field. Envelope values override these. |
| `input_path` | string | — | Dotted path on the envelope picking the user content. `"v"` = the whole `v` field; `"v.markdown"` = `v.markdown`; `"v.a.b"` = nested. |
| `output` | enum | — | `"text"` or `"json"` — see [How it works](#how-it-works). |
| `retry.max_retries` | int? | provider-default | Retry budget for transient errors (5xx, rate limits). |
| `retry.delays_ms` | int[]? | provider-default | Per-attempt back-off delays in ms. |
| `on_failure` | enum | `"stderr"` | How terminal failures surface. `"stderr"`: legacy — `ctx.error` only. `"envelope"`: fail-envelope on data channel for pipeline-side routing. `"both"`: both. See [Failure handling & retry loop](#failure-handling--retry-loop). |

### Temperature ranges per provider

| Provider | Documented range | Notes |
|---|---|---|
| `anthropic` | `0.0` to `1.0` (inclusive) | Hard limit per [Messages API docs](https://docs.anthropic.com/en/api/messages). Values outside the range are rejected. |
| `google` | not explicitly documented | Default `1.0`. Gemini 3 docs recommend keeping the default; values below `1.0` "may lead to unexpected behavior, such as looping or degraded performance, particularly in complex mathematical or reasoning tasks." |

**Anthropic + extended thinking gotcha:** when `thinking_budget` is set on `provider: anthropic`, the request body is sent with `temperature = 1.0` regardless of the configured `temperature` value. The Anthropic API rejects any other temperature when thinking is enabled. The provider impl in this tool clamps for you (no error).

---

## Prompts

The system prompt is composed per envelope from up to two file classes, both living **flat** in `prompts.dir`:

### 1. Base prompt

```
prompts.dir/<name>_<version>.txt
```

Mandatory. Missing-base is a hard error. Contains the global instructions: output format, required JSON schema (in json mode), behavioural guardrails.

### 2. Tag overlays

For each name in `settings.prompts.overlays`, the tool resolves a value to a single string. The overlay file is then:

```
prompts.dir/<name>_<version>_<resolved-value>.txt
```

**Value resolution per overlay name** (first non-empty wins):

| Step | Source                                  | Notes                                  |
|------|-----------------------------------------|----------------------------------------|
| 1    | `envelope.v[name]`                      | Any string field on the input envelope. |
| 2    | `settings.overlay_defaults[name]`       | Fallback configured per pipeline.      |
| 3    | (empty -> overlay skipped)              | No file is loaded; not an error.       |

If the resolved-value file does not exist on disk, the overlay is also silently skipped — so an envelope with `kind: "alpha"` but no `summarise_v1_alpha.txt` still works (only the base + other overlays apply).

**Example A.** Settings:
```yaml
prompts:
  dir: ./prompts
  name: summarise
  version: v1
  overlays: ["category", "kind"]
overlay_defaults: { kind: "default" }
```
Envelope: `{markdown: "...", category: "alpha", kind: "bravo"}`. Loaded files:

```
prompts.dir/summarise_v1.txt              <- base
prompts.dir/summarise_v1_alpha.txt        <- category (envelope.v.category)
prompts.dir/summarise_v1_bravo.txt        <- kind (envelope.v.kind)
```

**Example B.** Same settings, envelope without `kind`:

```
prompts.dir/summarise_v1.txt
prompts.dir/summarise_v1_alpha.txt
prompts.dir/summarise_v1_default.txt      <- kind fallback from overlay_defaults
```

### Final composition

```
<base content>

---

<tag overlay 1 content>

---

<tag overlay 2 content>
```

Sections joined with `\n\n---\n\n`. Models see a clearly-delimited stack.

> **Difference from `doc-converter`:** llm has no page-range overlays. doc-converter has them because it processes one document = N pages. llm processes one envelope = one prompt.

---

## Output modes

### `output: text`

Use when the LLM result is a single text blob and you want to keep upstream envelope fields alongside it.

- The response (after fence stripping) is wrapped as `{response: "<text>"}`.
- The original `input_path` field is REMOVED from the envelope (otherwise long pipelines accumulate the source content forever).
- All other v fields pass through unchanged.

Example. Input:
```json
{ "markdown": "<long source>", "category": "alpha", "doc_id": "DOC-001" }
```
Settings: `input_path: "v.markdown"`, `output: "text"`. Output:
```json
{ "response": "<llm summary>", "category": "alpha", "doc_id": "DOC-001" }
```

### `output: json`

Use when the LLM is the SOLE source of structured output for this stage.

- The response is fence-stripped, then `JSON.parse(...)`-ed.
- The parsed value REPLACES `v` entirely. Upstream fields are NOT preserved (they're considered no longer relevant once the structured payload exists).
- If the response isn't valid JSON, the envelope errors out via `ctx.error`.

Example. Input:
```json
{ "text": "Q3 revenue was 1.2M, growth 14%" }
```
Settings: `input_path: "v.text"`, `output: "json"`. LLM returns:
```
```json
{ "metric": "revenue", "value": 1200000, "growth_pct": 14 }
```
```
Output envelope's v becomes:
```json
{ "metric": "revenue", "value": 1200000, "growth_pct": 14 }
```

If you need both the parsed JSON AND upstream fields, use `output: text` and have the prompt return JSON inside `response` — then a downstream `map` op re-parses it.

---

## Retry behaviour

The provider impls retry on transient failures (HTTP 5xx, 429 rate limits, network resets). Settings:

- `retry.max_retries`: how many additional attempts after the first failure. Default depends on provider impl.
- `retry.delays_ms`: per-attempt back-off in ms. e.g. `[500, 2000, 5000]` means wait 500 ms before retry 1, 2 s before retry 2, 5 s before retry 3. If `max_retries` exceeds the array length, the last delay repeats.

Non-transient errors (4xx auth, malformed request) fail immediately — no retries waste your quota.

---

## Failure handling & retry loop

Same shape as doc-converter — internal retry (above) handles transient blips; **external** retry, managed by the pipeline graph, handles longer outages by routing failed envelopes back into `llm` after a cooldown.

`settings.on_failure` controls what happens when internal retry is exhausted:

| `on_failure` | Behaviour |
|---|---|
| `"stderr"` (default) | `ctx.error(v, msg)` only. Envelope dropped from data stream. Legacy. |
| `"envelope"` | Emit a **fail-envelope** on data channel with `v.failed=true`, `v.error_class`, `v.error_msg`, `v.error_attempt`. No stderr. |
| `"both"` | Emit fail-envelope AND call `ctx.error`. |

### Fail-envelope shape

```jsonc
{
  "v": {
    "failed":          true,
    "error_class":     "rate_limit",        // "rate_limit"|"server_error"|"timeout"|"connection_error"|"input"|"convert"|"parse"|"other"
    "error_msg":       "provider call failed: anthropic 429: ...",
    "error_attempt":   1,                    // count of external retries (1 = first failure)
    "source_path":     "/data/in/foo.pdf",   // preserved from input (or v.path mapped)
    "source_hash":     "1a2db56c...",
    "source_filename": "foo.pdf"
    // ...other source_* fields preserved from input...
  }
}
```

### Retry loop in pipeline YAML

```yaml
stages:
  llm-extract:
    tool: llm
    input: [from-doc-converter, llm-retry-cooldown]    # fan-in: fresh + retries
    settings:
      provider:    anthropic
      model:       claude-haiku-4-5-20251001
      input_path:  "v.content"
      output:      "json"
      on_failure:  "envelope"
      retry:
        max_retries: 3
        delays_ms:   [1000, 3000, 10000]
      # ... prompts, overlays, etc ...

  route-llm-result:
    tool: route
    input: llm-extract
    routes:
      retry:  "v.failed == true && v.error_attempt < 5"
      dead:   "v.failed == true && v.error_attempt >= 5"
      ok:     "true"

  llm-retry-cooldown:
    tool: <delay-or-equivalent>
    settings:
      delay_ms: 30000
    input: route-llm-result:retry

  sink-llm-failed:
    tool: write-file-stream
    settings:
      default_file: "$output/failed-extractions.ndjson"
      format: ndjson
    input: route-llm-result:dead

  ok-downstream:
    input: route-llm-result:ok
    # ... etc ...
```

On a successful re-run, the success envelope is built fresh — `v.failed` / `v.error_*` fields are NOT propagated (even in `output: text` mode, which usually preserves input fields, explicitly skips these). The loop closes itself: `route: retry` no longer matches, the envelope flows through `ok`. `ctx.log(info)` records the resolution.

---

## Worked example: two-stage pipeline (llm text -> llm json)

Pull a free-text summary from each input doc, then turn it into structured JSON.

```yaml
stages:
  read:
    tool: read-file-stream
    settings: { format: lines }
    input: $input

  summarise:
    tool: llm
    settings:
      provider: anthropic
      model: claude-haiku-4-5-20251001
      temperature: 0
      max_tokens: 8000
      prompts:
        dir: "$configs/prompts"
        name: summarise
        version: v1
        overlays: [category]
      overlay_defaults: { category: "default" }
      input_path: v.line
      output: text
    input: read

  extract:
    tool: llm
    settings:
      provider: anthropic
      model: claude-haiku-4-5-20251001
      temperature: 0
      max_tokens: 4000
      prompts:
        dir: "$configs/prompts"
        name: extract
        version: v1
      input_path: v.response       # consume the previous stage's `response`
      output: json
    input: summarise

  sink:
    tool: write-file-stream
    settings: { default_file: "$output/extracted.ndjson", format: ndjson }
    input: extract
```

After `summarise`, an envelope looks like:
```json
{ "response": "Doc-001 covers Q3 2024 revenue ...", "category": "alpha" }
```

After `extract`, the same envelope's v is REPLACED with the parsed JSON:
```json
{ "doc_id": "DOC-001", "period": "Q3 2024", "revenue": 1200000 }
```

---

## More

See [`docs/tools/llm.md`](../../docs/tools/llm.md) for additional cookbook examples (mocking the provider in tests, retry tuning, prompt versioning conventions).

## License

[AGPL-3.0-or-later](../../LICENSE).
