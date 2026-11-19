# doc-converter

Convert PDF / DOCX / image documents to markdown (or any text format) via LLM vision, page-by-page, with layered prompts and per-page-range prompt overrides.

The pipeline is: **render document pages to JPEG, send each page to an LLM with a composed system prompt, collect markdown, merge per-page frontmatter, write a single output file.**

This tool is the right shape when:
- You have heterogeneous documents (mixed PDF, DOCX, scans, images) but want one output format.
- The structure varies per page (e.g. cover page, then schedules, then signatures).
- The LLM does the structural extraction; we just orchestrate.

---

## Install

```bash
dpe install doc-converter
```

### Optional: LibreOffice (only needed for DOCX inputs)

doc-converter handles three input types:

| Input  | Requires LibreOffice? |
|--------|-----------------------|
| `.pdf` | no                    |
| image  | no                    |
| `.docx`| **yes** — `soffice --headless` is invoked to convert DOCX -> PDF before page rendering |

If you never feed DOCX, skip this section.

For DOCX inputs the tool needs the LibreOffice CLI binary (`soffice`) on PATH. We do NOT vendor LibreOffice (250-500 MB application suite — wrong shape for redistribution). Install it once at the OS level. There is no quality-equivalent alternative for full OOXML rendering; LibreOffice and Microsoft Word are the only options that implement the format correctly, and fidelity matters because the rendered pages feed an LLM.

```bash
# Linux (Debian / Ubuntu) — minimal subset is enough
sudo apt-get install -y --no-install-recommends \
    libreoffice-core libreoffice-writer

# Linux (Fedora / RHEL)
sudo dnf install -y libreoffice-core libreoffice-writer

# macOS (Homebrew)
brew install --cask libreoffice
# soffice ends up at /Applications/LibreOffice.app/Contents/MacOS/soffice

# Windows (winget)
winget install --id TheDocumentFoundation.LibreOffice --silent
# soffice.exe at: C:\Program Files\LibreOffice\program\soffice.exe
```

If `soffice` is not on PATH, point at it explicitly per pipeline:

```yaml
tool: doc-converter
settings:
  docx:
    soffice_bin: "/Applications/LibreOffice.app/Contents/MacOS/soffice"
```

The tool fails only when a DOCX envelope arrives without a working `soffice`. PDF and image pipelines run fine without LibreOffice installed.

---

## API keys (env, NOT settings)

| Provider    | Env vars                              |
|-------------|---------------------------------------|
| `anthropic` | `ANTHROPIC_API_KEY`                   |
| `google`    | `GEMINI_API_KEY` or `GOOGLE_API_KEY`  |

Keys are NEVER passed via settings — that would put them in pipeline YAML and journals.

---

## How it works

For each input envelope, the tool routes through one of three dispatch strategies, then merges and writes a single output file. The strategy decision is automatic, gated on document size and the configured `per_page_mode`.

### High-level flow

1. **Detect file kind** from `v.path`'s extension: PDF, DOCX, or image (`png`/`jpg`/`jpeg`/`webp`/`gif`). Any other extension errors.
2. **Normalise to PDF** (or keep as image):
   - PDF: passed through.
   - DOCX: converted to PDF by spawning `soffice --headless --convert-to pdf`. Result lands in `docx.temp_dir` (or OS tempdir).
   - Image: skipped — treated as a single-page document.
3. **Count pages** via pdfium (image inputs are always page-count = 1).
4. **Decide strategy** (see [Dispatch strategy](#dispatch-strategy) below):
   - **Whole-doc** — small enough PDF (`≤ strategy.whole_doc_threshold` pages, `≤ whole_doc_max_pages`, `≤ whole_doc_max_bytes`). Sends the whole PDF as ONE document attachment.
   - **Per-page parallel** (`strategy.per_page_mode: parallel`, default) — split, render each selected page, fire `parallel.limit` concurrent direct LLM calls.
   - **Per-page batch** (`strategy.per_page_mode: batch`) — submit all selected pages as one batch-API request, poll to completion.
5. **Compose prompts** ([Prompts](#prompts)):
   - Per-page modes: `base + tag overlays + page-range overlays` per page.
   - Whole-doc mode: `base + tag overlays + (optional) <name>_<version>_whole.txt` — page-range overlays are NOT loaded.
6. **Call the LLM** for the selected dispatch strategy. Per-page calls return one markdown blob per page; whole-doc returns a single blob for the entire document.
7. **Extract + merge frontmatter** with reverse priority across pages (earlier pages win on collisions). Whole-doc has just the one block.
8. **Combine page bodies** with the `output.page_separator` template injected before each page. Default template: `<!-- PAGE: {n} -->`. Whole-doc skips the merger entirely.
9. **Prepend the merged frontmatter block** when one was parsed. The body retains the `---` block in both output modes regardless of `output.extract_frontmatter` — that setting only controls envelope-side promotion.
10. **Cache** the result. In Mode A (file output), the `.md` file IS the persistent cache — no separate cache JSON. In Mode B (envelope-field output), the framework cache at `$DPE_STORAGE/doc-converter/<hash>.json` stores `{final_text, frontmatter}`. See [Caching](#caching).
11. **Write or emit:**
    - Mode A: write `<output.file.dir>/<resolved-name>` per the filename template. Output destination is **NOT** in the cache key — moving `output.file.dir` between runs still reuses cached LLM work.
    - Mode B: no file write; merged body ships on the envelope.
12. **Emit envelope**:
    - Mode A: `{path: <output-file>, cache_hit, ...merged_frontmatter_keys, ...source_*, ...overlays}`.
    - Mode B: `{<configured_field>: <merged-body-text>, cache_hit, ...merged_frontmatter_keys, ...source_*, ...overlays}` (no `path`).

```
   v.path
     │
     ▼
┌─────────────────┐  DOCX                ┌──────────────┐
│ detect filetype │──────────────────────▶│ soffice → PDF│
└─────────────────┘                      └──────┬───────┘
   │ PDF / image                                │
   ▼                                            ▼
┌─────────────────┐
│ decide strategy │  pages × bytes vs. thresholds + per_page_mode
└──────┬──────────┘
       │
       ├─── whole_doc ────────┐    ├─── per_page_parallel ─────┐    ├─── per_page_batch ─┐
       │                      │    │                            │    │                    │
       ▼                      │    ▼                            │    ▼                    │
  send raw PDF as 1 doc       │  render each page → JPEG        │  submit batch API ──┐   │
  → 1 LLM call                │  → bounded-concurrency LLM      │  emit meta {source, │   │
  → 1 markdown blob           │     calls (parallel.limit)      │    batch_id}        │   │
                              │  → N markdown blobs              │  poll until done    │   │
                              │                                  │  → N markdown blobs │   │
       ▼                      ▼                            ▼     ▼                          ▼
       └────┬─────────────────┴─────────────────┬──────────┴──────────────────────────────────┘
            │                                   │
            ▼                                   ▼
                extract frontmatter · merge per-page (whole-doc skips merge)
                                       │
                                       ▼
            cache { final_text, frontmatter }   ← keyed on doc + prompts + model + post-process
                                       │
                                       ▼
                              write <out_dir>/<stem><ext>
                                       │
                                       ▼
                       emit { path, ...merged_frontmatter_fields }
```

### Dispatch strategy

The router picks **whole-doc** when ALL of the following hold (PDF / DOCX inputs only — image inputs always go per-page):

- `pages ≤ strategy.whole_doc_threshold` (default 5)
- `pages ≤ strategy.whole_doc_max_pages` (default 100; provider hard cap — Anthropic 100, Gemini 1000)
- `file_size ≤ strategy.whole_doc_max_bytes` (default 20 MiB; smaller of Anthropic 32 MB and Gemini 20 MB inline)

Otherwise it falls back to per-page dispatch using `strategy.per_page_mode`:

| Mode | When to use | Cost / latency |
|---|---|---|
| `parallel` (default) | Medium-N pages, latency matters | Direct calls, full price, fastest wall time. Capped at `parallel.limit` concurrent in-flight calls. |
| `batch` | High-N pages, cost matters more than latency | Single batch submission per file. **50% cost discount** on both providers. Provider returns results within 24 h (Anthropic) / 48 h (Gemini), typically within minutes. Full async flow with state-file resume — see [Batch mode](#batch-mode). |

Setting `strategy.whole_doc_threshold: 0` disables whole-doc mode entirely; everything goes per-page.

### Whole-doc mode — what's different

- The whole PDF is sent as ONE inline document attachment (Anthropic `document` content block; Gemini `inlineData` with `application/pdf` mime). The model handles page extraction internally — no client-side rendering.
- `page_filter` is **ignored**. The provider sees every page; the LLM decides what to extract.
- Page-range prompt overlays (`<name>_<version>_p<N>-<M>.txt`) are **never loaded** in whole-doc mode. Use a dedicated `<name>_<version>_whole.txt` overlay (auto-loaded if present) for whole-doc-specific instructions.
- The output has NO page-separator markers (there's no merge step — the LLM emits one continuous response).
- One LLM call instead of N — typically 5-10× faster wall-clock than per-page for short documents, AND cheaper.

### Concurrency semantics

Three independent dimensions, sometimes confused:

| Dimension | Where | Default | Bound |
|---|---|---|---|
| **Per-document dispatch** (whole-doc vs per-page) | `strategy.whole_doc_threshold` + `whole_doc_max_pages` + `whole_doc_max_bytes` | 5 pages / 100 pages / 20 MiB | Decision is **deterministic per document** — `per_page_mode` does NOT override. A document ≤ threshold ALWAYS gets exactly 1 LLM call. |
| **Per-page concurrency WITHIN a document** | `parallel.limit` | 4 | Applies only when the router chose per-page-parallel for THIS document. Bounds in-flight LLM calls for one document's pages. Does NOT affect concurrent envelopes. |
| **Concurrent envelopes** (multiple files in flight) | n/a — framework-controlled | n/a | **The tool processes envelopes strictly serially** — one input at a time. The framework's `dpe_run!` loop reads one envelope, runs `process_input`, then reads the next. No cross-envelope tokio::spawn. To get parallelism across documents, run multiple replicas of the tool stage. |

So under default settings (`whole_doc_threshold: 5`) a 4-page document makes **1 LLM call**, while a 12-page document goes per-page and makes 12. A 50-page document with `per_page_mode: parallel` and `parallel.limit: 4` makes 50 LLM calls but only 4 in flight at once. Independent of either, two envelopes arriving back-to-back are processed sequentially.

---

## Settings reference

```yaml
# All fields:
provider:        "anthropic" | "google"        # required
model:           string                         # required, e.g. "claude-sonnet-4-6"
temperature:     0.0                            # provider-dependent; see notes below
max_tokens:      65536
thinking_budget: null                           # int, provider-specific reasoning tokens
thinking:        null                           # "adaptive" (Anthropic)
effort:          null                           # low|medium|high|xhigh|max (Anthropic)

prompts:
  dir:     "./prompts"                          # required
  name:    "convert"                            # required, base name
  version: "v1"                                 # required

category:         "alpha"                       # pipeline-level value for the `category` overlay
overlays:         ["category", "kind"]          # ordered list of overlay tag names
overlay_defaults: { kind: "default" }           # per-name fallback when envelope is missing the field

page_filter:      ["1-1", "0-3"]                # ranges to convert; empty = all pages
                                                # IGNORED in whole-doc mode (LLM gets the whole PDF)

render:
  dpi:      150                                 # 72-600
  quality:  85                                  # 1-100

output:                                         # exactly ONE of `file` / `field` required
  # Mode A — write merged markdown to disk. The .md file IS the persistent cache.
  file:
    dir:           "/data/converted"            # required; output directory
    name:          "v.hash"                     # required; field-pointer shorthand OR template
    # name:        "{hash:short:8}-{filename:safe}"   # alternative: composed template
    target_ext:    ".md"                        # default ".md"; ignored when `name` already has extension
    invalidation:  "strict"                     # "strict" (default) | "sidecar" | "none"
  # Mode B — emit merged markdown body on envelope field. (mutually exclusive with `file`)
  # field:
  #   name:        "content"                    # envelope key that carries the body
  extract_frontmatter:  true                    # promote parsed frontmatter to envelope fields
                                                # (body always retains the `---` block in both modes)
  page_separator:       "<!-- PAGE: {n} -->"    # template; {n} = 1-based page number; "" disables.
                                                # Per-page mode only — whole-doc has no merge step.

docx:
  soffice_bin: "soffice"                        # PATH lookup by default
  temp_dir:    null                             # override soffice's scratch dir

# Per-page parallel dispatch (when strategy.per_page_mode = parallel).
parallel:
  limit:    4                                   # max concurrent in-flight LLM calls; 1 = serial

# Routing decision: whole-doc vs. per-page dispatch.
strategy:
  whole_doc_threshold:  5                       # pages; 0 disables whole-doc entirely
  whole_doc_max_pages:  100                     # provider cap (Anthropic 100, Gemini 1000)
  whole_doc_max_bytes:  20971520                # 20 MiB; min(Anthropic 32 MB, Gemini inline 20 MB)
  per_page_mode:        "parallel"              # "parallel" | "batch"

# Batch-API dispatch (active only when strategy.per_page_mode = batch).
batch:
  state_dir:           "$storage/doc-converter/batches"
  poll:                                         # null = fire-and-forget (harvest on next run)
    start_after_ms:    30000                    # delay before FIRST poll
    interval_ms:       10000                    # between subsequent polls (>= 1000)
    deadline_ms:       600000                   # end-to-end wall-clock cutoff
  request_timeout_ms:  120000                   # per-HTTP-request timeout (orthogonal to deadline)

# HTTP request timeout + retry policy. Applied uniformly to whole-doc,
# per-page parallel, AND batch internals.
request_timeout_ms:   180000                    # 180s; aborts stalled calls (counts as retryable Timeout)
retry:
  max_attempts:       3                         # 1 = no retry; 3 = 1 initial + up to 2 retries
  initial_backoff_ms: 1000                      # doubles each retry, capped at max_backoff_ms
  max_backoff_ms:     30000
  retry_on:                                     # only listed classes are retried; everything else surfaces immediately
    - rate_limit                                # HTTP 429
    - server_error                              # HTTP 5xx
    - timeout                                   # request_timeout_ms exceeded
    - connection_error                          # TCP reset / DNS / TLS handshake
```

| Field | Type | Default | Description |
|---|---|---|---|
| `provider` | enum | — | `anthropic` or `google`. Selects the API client; the key is read from env. |
| `model` | string | — | Provider-specific model identifier. |
| `temperature` | float | `0.0` | LLM sampling temperature. Provider-dependent range — see [Temperature ranges per provider](#temperature-ranges-per-provider) below. `null`: not sent (current Claude models reject it; see [Current Claude models](#current-claude-models-temperature-thinking-effort)). |
| `max_tokens` | int | `65536` | Max response tokens (provider-clamped if higher than the model allows). |
| `thinking_budget` | int? | `null` | Reasoning-token budget for providers that support extended thinking. **Anthropic constraint:** if set on Anthropic, `temperature` must be exactly `1.0` (the API rejects other values when thinking is enabled). |
| `thinking` | `"adaptive"`? | — | Anthropic only. Adaptive thinking for models that reject `thinking_budget`; mutually exclusive with it. |
| `effort` | enum? | — | Anthropic only. `low` \| `medium` \| `high` \| `xhigh` \| `max` (`output_config.effort`). |
| `prompts.dir` | string | — | Directory holding the prompt files. Absolute or relative to runner cwd. |
| `prompts.name` | string | — | Base prompt name. The base file is `<name>_<version>.txt`. |
| `prompts.version` | string | — | Version tag, used in every filename. |
| `category` | string? | — | Pipeline-level value for the `category` overlay (only when `"category"` is in `overlays`). Wins over envelope and `overlay_defaults`. Use it when one overlay value applies to every envelope in the run. |
| `overlays` | string[] | `[]` | Ordered list of overlay tag NAMES. For each name, the per-envelope value is resolved as: (1) `settings.category` if name is `category`, else (2) `envelope.v[name]`, else (3) `overlay_defaults[name]`. There are no other hard-coded names. |
| `overlay_defaults` | map | `{}` | Fallback values per overlay name, used when the envelope doesn't carry the field. Envelope values override these. |
| `page_filter` | string[] | `[]` (= all) | List of page ranges to convert. Grammar below. |
| `render.dpi` | int | `150` | JPEG render DPI. Higher = sharper text but bigger images and slower LLM calls. |
| `render.quality` | int | `85` | JPEG quality 1-100. |
| `output.file` / `output.field` | object | — | EXACTLY ONE required. `file` writes the markdown to disk (Mode A). `field` emits it on an envelope key (Mode B). Both / neither errors at startup. |
| `output.file.dir` | string | — | Mode A only. Output directory; created if missing. Path-prefix tokens (`$temp`, `$session`) resolved by the runner. |
| `output.file.name` | string | — | Mode A only. Filename template. Either a shorthand `"v.<field>"` (e.g. `"v.hash"`) — value of that envelope field — OR a template with `{...}` placeholders (e.g. `"{hash:short:8}-{filename:safe}"`). See [Filename template](#filename-template) below. |
| `output.file.target_ext` | string | `".md"` | Mode A only. Extension appended only when `name`'s last path segment has no dot. If `name` already includes an extension (`"{hash}.md"`), `target_ext` is ignored — no double-extension. |
| `output.file.invalidation` | enum | `"strict"` | Mode A only. How to detect cache hits when the output file already exists: `strict` — file exists → cache HIT, no validation; `sidecar` — file + matching `<file>.dpe.json` → cache HIT; `none` — always regenerate. |
| `output.field.name` | string | — | Mode B only. Envelope key (top-level, no dots) that carries the full merged markdown body. |
| `output.extract_frontmatter` | bool | `true` | Whether parsed frontmatter fields get promoted onto the envelope as separate keys (for routing / filtering). The body ALWAYS retains its `---` block regardless of this setting — extraction is for envelope promotion only. |
| `output.page_separator` | string | `"<!-- PAGE: {n} -->"` | Template injected before every per-page body during merge. `{n}` substitutes the 1-based page number. Empty string disables markers. Whole-doc mode never injects this. |
| `docx.soffice_bin` | string | `"soffice"` | LibreOffice CLI path. PATH lookup by default. |
| `docx.temp_dir` | string? | OS temp | Scratch dir for the DOCX -> PDF intermediate file. |
| `parallel.limit` | int | `4` | Max concurrent LLM calls when `strategy.per_page_mode = parallel`. `1` = serial (legacy behaviour). Bump only after measuring provider rate-limit headroom. |
| `strategy.whole_doc_threshold` | int | `5` | Page count at or below which the document is sent as ONE raw-PDF attachment. `0` = always per-page. |
| `strategy.whole_doc_max_pages` | int | `100` | Provider cap for whole-doc — falls back to per-page above this. Anthropic 100, Gemini 1000. |
| `strategy.whole_doc_max_bytes` | int | `20 * 1024 * 1024` | Provider cap for whole-doc — falls back to per-page above this. Min(Anthropic 32 MB PDF, Gemini 20 MB inline). |
| `strategy.per_page_mode` | enum | `"parallel"` | `parallel` (direct N-concurrent calls) or `batch` (single batch-API submission). |
| `batch.state_dir` | string | `"$storage/doc-converter/batches"` | Directory for atomic JSON state files. One file per pending batch; cleared on completion / terminal failure. |
| `batch.poll` | object? | shown below | Polling cadence. `null` = fire-and-forget. |
| `batch.poll.start_after_ms` | int | `30000` | Delay before the FIRST status poll. |
| `batch.poll.interval_ms` | int | `10000` | Between subsequent polls. Min `1000` (1 s). |
| `batch.poll.deadline_ms` | int | `600000` | End-to-end wall-clock cutoff. After this, emits a `warn` log, leaves the state file, returns without a data envelope. Next run picks up. |
| `batch.request_timeout_ms` | int | `120000` | Per-HTTP-request timeout (submit / status / fetch results). Orthogonal to `poll.deadline_ms`. |
| `request_timeout_ms` | int | `180000` | Top-level per-HTTP-request timeout (ms). Applied to every provider call: whole-doc, per-page parallel, batch internals. If the provider stalls past this, the call aborts with a timeout error, which the retry wrapper treats as transient (when `timeout` is in `retry.retry_on`). |
| `retry.max_attempts` | int | `3` | Total attempts including the first. `1` disables retry. |
| `retry.initial_backoff_ms` | int | `1000` | Backoff before the first retry. Doubles each subsequent retry, capped at `retry.max_backoff_ms`. |
| `retry.max_backoff_ms` | int | `30000` | Per-retry sleep cap. |
| `retry.retry_on` | string[] | all four | Retry trigger classes: `rate_limit` (HTTP 429), `server_error` (HTTP 5xx), `timeout` (request_timeout_ms exceeded), `connection_error` (TCP reset / DNS / TLS). Other failures (4xx other than 429, parse errors, validation refusals) are NEVER retried. |
| `on_failure` | enum | `"stderr"` | How terminal failures surface. `"stderr"`: `ctx.error` only (envelope dropped). `"envelope"`: fail-envelope on data channel for pipeline-side routing. `"both"`: both. See [Failure handling & retry loop](#failure-handling--retry-loop). |

### Temperature ranges per provider

| Provider | Documented range | Notes |
|---|---|---|
| `anthropic` | `0.0` to `1.0` (inclusive) | Hard limit per [Messages API docs](https://docs.anthropic.com/en/api/messages). Values outside the range are rejected. Current Claude models accept no temperature at all — see below. |
| `google` | not explicitly documented | Default `1.0`. Gemini 3 docs recommend keeping the default; values below `1.0` "may lead to unexpected behavior, such as looping or degraded performance, particularly in complex mathematical or reasoning tasks." |

### Current Claude models: `temperature`, `thinking`, `effort`

Current Claude models restrict sampling parameters: some reject `temperature` outright, others accept only its default value. The newest ones (e.g. Opus 5.5, Sonnet 5.5, Haiku 5.5) also reject `thinking_budget`. Check the model's documentation; the tool handles the common cases:

- **`temperature`** — when the API answers a request whose `temperature` is within `0.0`-`1.0` with a 400 about `temperature`, the tool sends the request again without it and leaves it out for that model for the rest of the run (one `warn` log line). Set `temperature: null` to never send it. Older models keep receiving the configured value (default `0.0`), as before; an out-of-range value is still reported as an error. `temperature: null` also applies with `thinking_budget` (which otherwise sends `1.0`). In batch mode the rejection arrives per page in the results, with a hint to set `temperature: null`.
- **`thinking: adaptive`** — sends `thinking: {type: "adaptive"}`: the model decides how much to reason. Use it instead of `thinking_budget` on models that reject budgets; the two are mutually exclusive. No temperature is sent with it.
- **`effort`** — `low` | `medium` | `high` | `xhigh` | `max`, sent as `output_config.effort`: how much the model reasons and writes. Leave it unset to use the model's default; older models (e.g. Haiku 4.5) reject it.

`thinking` and `effort` are Anthropic-only; setting them with `provider: google` is a startup error.

```yaml
provider: anthropic
model:    claude-haiku-5-5
temperature: null        # optional: skips the first rejected request
thinking: adaptive
effort:   low
```

**Practical advice:** for structural document extraction the model should be as deterministic as possible, so `0.0` (Anthropic) or default `1.0` (Gemini) is what you want. Don't push temperature high for this tool — you'll get hallucinated frontmatter values.

---

## Prompts

The system prompt sent to the LLM is composed per page from up to three classes of files, all living **flat** in `prompts.dir`:

### 1. Base prompt

```
prompts.dir/<name>_<version>.txt
```

Mandatory. Missing-base is a hard error. Contains the global instructions (output schema, frontmatter required keys, formatting rules).

### 2. Tag overlays

For each name in `settings.overlays`, the tool resolves a value to a single string. The overlay file is then:

```
prompts.dir/<name>_<version>_<resolved-value>.txt
```

**Value resolution order per overlay name** (first non-empty wins):

| Step | Source                                     | Notes                                  |
|------|--------------------------------------------|----------------------------------------|
| 1    | `settings.category` *(only if name is `category`)* | Pipeline-level override.       |
| 2    | `envelope.v[name]`                         | Any string field on the input envelope. |
| 3    | `settings.overlay_defaults[name]`          | Fallback configured per pipeline.      |
| 4    | (empty -> overlay skipped)                 | No file is loaded; not an error.       |

If the resolved-value file does not exist on disk, the overlay is also silently skipped — so an envelope carrying `kind: "alpha"` but no `convert_v1_alpha.txt` still works (only the base + other overlays apply).

**Example A.** Settings:
```yaml
overlays:         ["category", "kind"]
category:         "alpha"
overlay_defaults: { kind: "default" }
```
Input envelope: `{path: ..., kind: "bravo"}`. Files loaded for each page:

```
prompts.dir/convert_v1.txt              <- base
prompts.dir/convert_v1_alpha.txt        <- category (settings.category wins)
prompts.dir/convert_v1_bravo.txt        <- kind (envelope.v.kind wins over default)
```

**Example B.** Same settings, but the envelope is missing `kind`:

```
prompts.dir/convert_v1.txt
prompts.dir/convert_v1_alpha.txt
prompts.dir/convert_v1_default.txt      <- kind fallback from overlay_defaults
```

**Example C.** An overlay name that has no value anywhere:
```yaml
overlays:         ["category", "language"]
category:         "alpha"
overlay_defaults: {}
```
Input envelope without `language`: only base + category overlay are loaded; the `language` overlay is silently skipped.

### 3. Page-range overlays

> **Note:** page-range overlays apply ONLY in per-page modes (parallel + batch). Whole-doc mode skips them entirely; use a `_whole.txt` overlay (next subsection) for whole-doc-specific rules.

Drop files named:

```
prompts.dir/<name>_<version>_p<N>-<M>.txt
```

The tool auto-discovers them by filename pattern. For each rendered page, every overlay whose range covers that page is appended to the prompt **in filename-sort order** (so `p1-1` comes before `p2-99`).

Range grammar inside the filename: `pN-M`
- `N` = 1-based start page (or `0` for "from end")
- `M` = number of pages (>= 1)

Examples for a 10-page document:

| File                          | Pages it applies to |
|-------------------------------|---------------------|
| `convert_v1_p1-1.txt`         | 1                   |
| `convert_v1_p1-3.txt`         | 1, 2, 3             |
| `convert_v1_p5-1.txt`         | 5                   |
| `convert_v1_p0-1.txt`         | 10                  |
| `convert_v1_p0-3.txt`         | 8, 9, 10            |
| `convert_v1_p3-99.txt`        | 3 through 10 (clamped) |

Multiple page-range overlays can apply to the same page; they all append. Common patterns:

- **Cover-page-only instructions:** `convert_v1_p1-1.txt` containing "this is a cover page; emit `company_name`, `doc_date`, `provider_name` in frontmatter; body is the document title only."
- **Signature pages:** `convert_v1_p0-2.txt` (last 2 pages) containing "extract `signed_by`, `signature_date`."
- **Schedules/appendices:** `convert_v1_p3-99.txt` for everything past the cover.

### 4. Whole-doc overlay (whole-doc mode only)

When the router picks whole-doc dispatch, the prompt composer:

1. Loads `<name>_<version>.txt` (base) — same as per-page.
2. Loads tag overlays in `overlays:` order — same as per-page.
3. Optionally loads `<name>_<version>_whole.txt` if present.
4. **Skips every page-range overlay** (`p<N>-<M>.txt`).

`_whole.txt` is the explicit place to put whole-doc-specific instructions ("scan the entire document and produce a unified summary; emit one frontmatter block at the top covering the whole agreement"). Missing → silently skipped (whole-doc mode works without one).

```
prompts.dir/extract_v4.txt           <- base
prompts.dir/extract_v4_sends.txt     <- tag overlay (category=sends)
prompts.dir/extract_v4_whole.txt     <- whole-doc overlay (auto-loaded; per-page modes ignore it)
prompts.dir/extract_v4_p1-1.txt      <- page-range overlay (per-page modes only)
```

### Final composition

Per-page mode (parallel or batch):
```
<base content>

---

<tag overlay 1 content>

---

<tag overlay 2 content>

---

<page-range overlay 1 content>

---

<page-range overlay 2 content>
```

Whole-doc mode:
```
<base content>

---

<tag overlay 1 content>

---

<tag overlay 2 content>

---

<whole-doc overlay content>     # only if <name>_<version>_whole.txt exists
```

Sections joined with `\n\n---\n\n`. This is also a valid horizontal-rule sequence in markdown so the model sees a clearly-delimited stack.

---

## Page filtering

`settings.page_filter` is a list of range strings using the same `pN-M` grammar as page-range overlays (you can omit the leading `p` since it's not in a filename). All ranges are unioned and deduplicated, yielding sorted unique 1-based page numbers.

Examples for a 10-page document:

| `page_filter` value         | Pages converted |
|-----------------------------|-----------------|
| `[]` or omitted             | 1-10 (all)      |
| `["1-1"]`                   | 1               |
| `["1-3"]`                   | 1, 2, 3         |
| `["1-1", "0-1"]`            | 1, 10 (first + last) |
| `["1-1", "0-2"]`            | 1, 9, 10        |
| `["3-99"]`                  | 3-10 (clamped)  |
| `["1-1", "5-1", "0-1"]`     | 1, 5, 10        |

> **Note on grammar:** ranges are `start-count`, not `start-end`. `1-3` means "starting at page 1, three pages" = pages 1-3. `5-1` means "page 5 only". Use `0-N` for "the last N pages" — handy for "always include signature pages."

Pages outside the document are silently clamped (no error). An empty filter is the same as omitting the field — all pages are processed.

---

## Output

doc-converter has two output modes, picked by which sub-block of `output` is configured. Exactly one of `output.file` or `output.field` must be present.

### Mode A — write to file (`output.file`)

The merged markdown body is written to `<dir>/<resolved-name><target_ext?>`. The file **IS** the persistent cache — re-runs read it back per the configured `invalidation` strategy without dispatching the LLM.

```yaml
output:
  file:
    dir:           "$temp/documents"
    name:          "v.hash"                 # OR a template like "{hash:short:8}-{filename:safe}"
    target_ext:    ".md"                    # default; appended only when name has no extension
    invalidation:  "strict"                 # strict | sidecar | none
```

#### Filename template

`output.file.name` accepts two syntaxes:

- **Shorthand** — `v.<field>`, e.g. `"v.hash"`. Resolves to the input envelope's `v.<field>` value, stringified, with no filters applied.
- **Template** — string with `{...}` placeholders. Inside `{...}`: `{<field>}` reads `v.<field>`. Filters apply left-to-right via `:`. Five filters at v1:

| Filter | Example | Effect |
|---|---|---|
| `safe` | `{filename:safe}` | replace `[^A-Za-z0-9._-]` with `_`, lowercase |
| `short` | `{hash:short}` | first 8 chars |
| `short:N` | `{hash:short:4}` | first N chars (N must be a positive integer immediately following `short`) |
| `nodot` | `{filename:nodot}` | strip everything after the last `.` (drop extension) |
| `lower` / `upper` | `{name:upper}` | case conversion |

Filters can be chained: `{path:safe:short:12}` = sanitize then first-12-chars. Examples:

| `name` template                                | Source `v` fields                              | Result                              |
|------------------------------------------------|------------------------------------------------|-------------------------------------|
| `"v.hash"`                                     | `hash="a1b2c3d4"`                              | `a1b2c3d4.md`                       |
| `"{hash}.md"`                                  | `hash="a1b2c3d4"`                              | `a1b2c3d4.md`                       |
| `"{hash:short}-{filename:safe}.md"`            | `hash="a1b2c3..."`, `filename="Q4 Report.pdf"` | `a1b2c3de-q4_report.pdf.md`         |
| `"{directory:safe}/{filename:safe}.md"`        | `directory="/in/2024"`, `filename="Foo.pdf"`   | `_in_2024/foo.pdf.md`               |

The resolved name must stay inside `dir`: an absolute path, a drive (`C:`), an empty segment, or a `.` / `..` segment fails that envelope with an `output.file.name` error, as does a placeholder that is empty after its filters. So pass raw path fields through `safe` (or use `{filename}`), never `v.path` directly.

Different inputs can resolve to the same name — `safe` lowercases and replaces every non-ASCII character, and a template with no placeholder is one fixed name. Under `invalidation: strict` the second input is then a cache hit that returns the first one's markdown. Include `{hash}` (or `{hash:short}`) when names may collide.

#### `target_ext` auto-append rule

`target_ext` (default `.md`) is appended to the resolved name **only when the name's last path segment has no dot.** So:

| `name`                       | `target_ext` | Resulting filename            |
|------------------------------|--------------|-------------------------------|
| `"v.hash"`                   | `.md`        | `<hash>.md` (ext added)       |
| `"{hash}.md"`                | `.md`        | `<hash>.md` (no double-ext)   |
| `"{filename:safe}"` (e.g. `"q4_report.pdf"`) | `.md` | `q4_report.pdf` (kept) |
| `"{filename:safe:nodot}"`    | `.md`        | `q4_report.md`                |

#### Cache invalidation (`output.file.invalidation`)

| Value | Cache lookup | Cache write | Settings drift detection |
|---|---|---|---|
| `"strict"` (default) | File exists → cache HIT, no validation. | Write `.md` only. | Manual — include a version in `name` (e.g. `"{hash}-v3.md"`) or clear the output dir. |
| `"sidecar"` | File exists AND `<file>.dpe.json` matches the current settings hash → cache HIT. | Write `.md` and a small sidecar JSON. | Automatic via sidecar. |
| `"none"` | Never reuse — always regenerate. | Write `.md` only. | N/A — always fresh. |

The sidecar JSON shape (only present when `invalidation: "sidecar"`):
```json
{ "cache_key_hash": "<32-hex>", "wrote_at": "...", "tool_version": "0.1.0" }
```

### Mode B — emit on envelope field (`output.field`)

The merged markdown body is shipped on the envelope under the configured key. No file is written. The framework cache at `$DPE_STORAGE/doc-converter/<hash>.json` persists the result for re-runs.

```yaml
output:
  field:
    name: "content"
```

### Output envelope shape (success)

**Mode A:**

```jsonc
{
  "v": {
    "path":         "/data/out/foo.md",         // file path
    "cache_hit":    true,                        // true when no LLM call dispatched
    "source_path":  "/data/in/foo.pdf",          // preserved from input
    "source_hash":  "1a2db56c...",
    "source_size":  245678,
    "source_filename": "foo.pdf",
    "source_ext":   "pdf",
    // ...frontmatter keys promoted (when extract_frontmatter: true)...
    // ...overlay tag values passed through...
  }
}
```

**Mode B:**

```jsonc
{
  "v": {
    "content":      "---\n<frontmatter>\n---\n\n<body>",   // FULL markdown body, FM block included
    "cache_hit":    true,
    "source_path":  "/data/in/foo.pdf",
    "source_hash":  "1a2db56c...",
    // ...source_*, frontmatter promotions, overlays...
  }
}
```

The output `v` starts as a copy of the input `v`, so Mode B also carries the input's `v.path` (the source path) and every other upstream field; the tool's own fields override on collision.

NB: on a successful retry, the `v.error_class` / `v.error_msg` / `v.error_attempt` fields of the input retry envelope are carried through unchanged. With `on_failure: envelope` or `both` the tool sets `v.failed=false`; with `on_failure: stderr` it does not touch `v.failed`. Route on `v.failed`, not on the presence of `error_*` fields.

**`cache_hit`** — boolean, always present. `true` when no LLM call was dispatched for this envelope (the cached result was reused). Useful for cost attribution and verifying re-run replays.

**`source_*`** — additive identity fields, all optional. Tool fills them in from the input envelope so downstream stages can trace from the converted output back to the original document without re-hashing. The mapping:

| Input `v.*` field | Output `v.*` field |
|---|---|
| `path`            | `source_path`     |
| `hash`            | `source_hash`     |
| `size`            | `source_size`     |
| `filename`        | `source_filename` |
| `ext`             | `source_ext`      |
| `source_path` / `source_hash` / `source_size` / `source_filename` / `source_ext` / `source_file` | passed through verbatim |

`source_*` fields NEVER overwrite frontmatter or overlay-tag values — the LLM's emitted frontmatter and any pipeline-set overlays always win. The tool emits a source field only if the corresponding key isn't already in the output envelope. To rename or strip them downstream, add a `normalize` stage.

### Failure handling & retry loop

There are two layers of retry. **Internal** retry (`settings.retry`) handles transient HTTP failures within a single dispatch — bounded to a few seconds. **External** retry, managed by the pipeline graph, handles longer outages by routing failed envelopes back into the converter after a cooldown — bounded to minutes/hours.

The internal retry is invisible to the pipeline. The external retry is what `settings.on_failure` opens the door to:

| `on_failure` | Behaviour on terminal failure (after `retry` exhausted) |
|---|---|
| `"stderr"` (default) | `ctx.error(&v, msg)` only. Envelope is dropped from the data stream. Legacy behaviour. |
| `"envelope"` | Emit a **fail-envelope** on the data channel with `v.failed=true`, `v.error_class`, `v.error_msg`, `v.error_attempt`, and source-identity fields. No stderr error. |
| `"both"` | Emit fail-envelope AND call `ctx.error`. Useful while migrating an existing pipeline. |

#### Fail-envelope shape

```jsonc
{
  "v": {
    "failed":          true,
    "error_class":     "rate_limit",      // "rate_limit" | "server_error" | "timeout" | "connection_error" | "input" | "io" | "convert" | "output" | "other"
    "error_msg":       "convert: anthropic 429: rate limited (after 3 attempts)",
    "error_attempt":   1,                  // count of EXTERNAL retries: 1 on first failure, 2 on second, etc.
    // ── Same source-identity fields as the success envelope ──
    "source_path":     "/data/in/foo.pdf",
    "source_hash":     "1a2db56c...",
    "source_size":     245678,
    "source_filename": "foo.pdf",
    "source_ext":      "pdf",
    // ── Overlay tag values from input — so route expressions still work ──
    "category":        "rates"
  }
}
```

Note: the fail-envelope also starts as a copy of the input `v`, so `v.path` there is the input's path (the source document), not an MD output. Downstream `route` / `filter` stages distinguish success vs failure by `v.failed == true`.

#### `error_class` taxonomy

The tool first classifies by call site (`input`, `io`, `convert`, `output`, `other`), then refines on the underlying error message: if the message matches a retry-trigger substring (`429`, `5xx`, `timeout`, `connection`), the class is upgraded to the corresponding retry class (`rate_limit`, `server_error`, `timeout`, `connection_error`). Lets pipeline `route` expressions gate retries on the cause:

```yaml
routes:
  retry_rate_limit:  "v.failed && v.error_class == 'rate_limit' && v.error_attempt < 10"  # patient retry
  retry_transient:   "v.failed && (v.error_class == 'server_error' || v.error_class == 'timeout') && v.error_attempt < 3"
  dead:              "v.failed"  # everything else after retries exhausted
  ok:                "true"
```

#### Retry-loop pipeline pattern

```yaml
stages:
  scan-pdfs:
    tool: scan-fs
    input: $input
    settings:
      include: ["*.pdf"]

  convert-pdf:
    tool: doc-converter
    # Fan-in: fresh scan output AND any envelopes returning through the
    # retry-cooldown stage. doc-converter handles both shapes:
    #   - fresh: {v.path: <pdf>, v.hash: ..., overlays...}
    #   - retry: {v.source_path: <pdf>, v.failed: true, v.error_attempt: 1, ...}
    # A retry input still carries the source v.path (fail-envelopes copy the
    # input v); v.source_path is the fallback. Either way the source resolves.
    input: [scan-pdfs, retry-cooldown]
    settings:
      provider:    anthropic
      model:       claude-haiku-4-5-20251001
      on_failure:  envelope          # emit fail-envelope on data channel
      retry:                         # internal HTTP-level retry
        max_attempts:       3
        initial_backoff_ms: 1000
        max_backoff_ms:     30000
      # ... rest of settings ...

  route-result:
    tool: route
    input: convert-pdf
    routes:
      retry:  "v.failed == true && v.error_attempt < 5"   # try again, up to 5×
      dead:   "v.failed == true && v.error_attempt >= 5"  # give up
      ok:     "true"                                       # successful conversion

  retry-cooldown:
    # Hold each failed envelope for a cooldown period before re-feeding.
    # While held, the rest of the pipeline keeps flowing — doc-converter
    # processes other PDFs from the fresh queue in the meantime. (Choose
    # a tool that holds envelopes for a configurable delay; e.g. a custom
    # `delay` stage, or a `checkpoint` + scheduled re-emit.)
    tool: <delay-or-equivalent>
    settings:
      delay_ms: 60000                # 1 minute cooldown
    input: route-result:retry

  sink-dead-letter:
    tool: write-file-stream
    settings:
      default_file: "$output/failed-conversions.ndjson"
      format: ndjson
    input: route-result:dead

  # ── Normal downstream processing — success envelopes only ──
  classify-recon:
    tool: classify
    input: route-result:ok
    # ... etc ...
```

When a failed conversion eventually succeeds (via cache hit, transient outage resolution, etc.), the success envelope is built fresh — `v.failed` is absent, so `route-result:retry` no longer matches and the envelope flows through `ok` to downstream stages. The retry loop closes itself.

`ctx.log(info)` records "convert: succeeded for foo.pdf after N external retries" in the journal so operators can see retry-loop resolutions without diffing envelopes.

### Frontmatter handling

Each page's LLM response can contain a leading frontmatter block:
```markdown
---
company_name: Acme Corp
doc_date: 2024-07-01
---

# Body of the page
...
```

The tool:
1. **Extracts** the YAML-flat frontmatter into a `key -> value` map per page.
2. **Strips** that block from the page's body.
3. **Merges** all page maps with reverse priority: keys are sorted alphabetically; for duplicates the FIRST page that set the key wins.
4. **Renders** the merged map as a single `---\n<key>: <value>\n...\n---\n` block at the top of the final file (only if `output.extract_frontmatter=true` and the merged map is non-empty).
5. **Promotes** every merged frontmatter key onto the output envelope's `v`. The downstream tool sees `{path, company_name, doc_date, ...}` without re-parsing the file.

---

## Render settings

| Field            | Default | Range  | Notes                                 |
|------------------|---------|--------|---------------------------------------|
| `render.dpi`     | 150     | 72-600 | 150 is good for most text; bump to 200-250 for fine print or scanned documents. |
| `render.quality` | 85      | 1-100  | JPEG quality. 85 is the sweet spot; 95+ rarely improves LLM accuracy. |

Higher DPI = sharper text but linear cost: a 200-DPI page is ~78% bigger than a 150-DPI page. Cost scales with image size for vision-capable LLMs.

---

## Batch mode

Active when `strategy.per_page_mode = batch` AND the router falls back to per-page (i.e., the document doesn't qualify for whole-doc).

### Lifecycle

1. **Render selected pages** to JPEG, build one `BatchRequest` per page (each carries a `custom_id` like `page-N`).
2. **Submit** to the provider's batch endpoint (Anthropic Messages Batches; Gemini `:batchGenerateContent` inline mode). Provider returns a `batch_id`.
3. **Persist a state file** at `<batch.state_dir>/<doc_cache_key_hash>.json` containing `{batch_id, provider, source_path, page_count, submitted_at_ms, expires_at_ms, model, custom_ids}`. Atomic write (tmp + rename).
4. **Emit ONE meta envelope** to stdout: `{"t":"m","v":{"source": "<input>", "batch_id": "<id>"}}`. Downstream stages can use this to track the batch externally (logging, manual cancel UI, alerting).
5. **Poll** per `batch.poll`:
   - Sleep `start_after_ms`, then poll every `interval_ms` until terminal status or `deadline_ms` wall-clock.
   - Each `info` log: `{"type":"log","level":"info","msg":"batch: in_progress N/M"}`.
   - On `Completed` → fetch results, map back to pages by `custom_id`, run the standard frontmatter merge + page-separator pipeline, **persist to the doc cache** (so re-runs hit cache instead of re-submitting), clear the state file, emit data envelope.
   - On `Failed` / `Expired` → clear state, surface as `ctx.error(&v, reason)` (lands in `<stage>_errors.log`).
   - On deadline reached → leave state file in place, emit a `warn` log, return without a data envelope. The next run that sees the same input picks up the saved state file and resumes polling.

### Resume on re-run

A re-run with the same input + settings:

1. Computes the doc cache key.
2. Tries the doc cache directly — cache hit → emit data envelope immediately, done.
3. Tries `<state_dir>/<doc_cache_key_hash>.json` — exists and not expired (TTL: Anthropic 24 h, Gemini 48 h) → re-emit the per-file meta envelope, poll the saved `batch_id` per `batch.poll`, finalize as above.
4. Neither hit → fresh submit (back to step 1 of the lifecycle).

The state file's `provider` field is a sanity gate: if a re-run's settings switched providers, the tool refuses to poll an Anthropic `batch_id` against a Google-configured client. Either fix settings or delete the state file by hand.

### Fire-and-forget mode

Set `batch.poll: null` to submit, emit the per-file meta envelope, and return immediately. No data envelope this run. The state file persists; the next run resumes (with `poll: null` it does ONE single-shot poll on resume). This is the right shape when an operator wants to launch many batches up-front, then run a follow-up pipeline pass that harvests them.

### Cost / latency vs. parallel

| Mode | Cost | Latency | When to use |
|---|---|---|---|
| `parallel` | full price | seconds-to-minute (4-8× speedup over serial at default `limit: 4`) | latency matters; provider quota is fine |
| `batch` | **50% off** (both providers) | 1-30 minutes typical, hard cutoff 24 h / 48 h | bulk processing, cost matters more than latency |

### Provider differences

| | Anthropic Messages Batches | Gemini Batch API (inline mode) |
|---|---|---|
| Submit | `POST /v1/messages/batches` | `POST /v1beta/models/{model}:batchGenerateContent` |
| Status | `GET /v1/messages/batches/{id}` | `GET /v1beta/{batch_name}` |
| Results | `GET /v1/messages/batches/{id}/results` (NDJSON stream) | nested `dest.inlined_responses[]` in status response |
| Auth | `x-api-key`, `anthropic-version: 2023-06-01` | `?key=<KEY>` query param |
| TTL | 24 h | 48 h |
| Status enum | `in_progress` / `canceling` / `ended` (with per-item `succeeded` / `errored` / `canceled` / `expired`) | `JOB_STATE_PENDING` / `RUNNING` / `SUCCEEDED` / `FAILED` / `CANCELLED` / `EXPIRED` |
| Custom-id field | `custom_id` (echoed in each result) | `metadata.key` (echoed when present; tool falls back to position-based mapping) |
| Inline-mode size cap | (single shape) | 20 MB total request payload |

The trait abstracts these — both implementations land at the same `BatchStatus::{InProgress, Completed, Failed, Expired}` outcome. Gemini File-mode upload (for batches > 20 MB inline) is not yet implemented.

---

## Caching

Caching is **doc-level, all-or-nothing**. The cache value is the in-memory result of the conversion (`{final_text, frontmatter}`); writing the .md file happens AFTER the cache lookup, so changing `output.dir` between runs still hits cache.

### Cache key composition

The key includes every input that affects the merged output text, and ONLY those:

```
{
  "namespace":           "doc-converter",
  "file_hash":           "<blake2b of source bytes>",
  "total_pages":         <int>,
  "selected_pages":      [<int>, ...],          // empty for whole-doc mode
  "prompts_hash":        "<hash of every per-page prompt OR the whole-doc prompt>",
  "strategy_kind":       "whole_doc" | "per_page" | "per_page_batch",
  "model":               "<model id>",
  "provider":            "anthropic" | "google",
  "temperature":         <float>,
  "max_tokens":          <int>,
  "thinking_budget":     <int|null>,
  "extract_frontmatter": <bool>,
  "target_ext":          "<ext>",
  "page_separator":      "<template>"
}
```

What's deliberately **excluded**: `output.dir`, envelope tag values that aren't reflected in tag-overlay prompts, host config, cwd. Re-running with a different output directory hits cache.

`strategy_kind` distinguishes whole-doc (cross-page context, one LLM call) from per-page (independent passes). Per-page parallel and per-page batch are also distinguished — output should be byte-identical between them, but separating cache slots makes "this came from batch" self-describing in debug.

### `DPE_CACHE_MODE`

Honored from the runner's environment (set via `dpe run --cache <mode>` / `dpe test --cache <mode>` / `cache:` field in test.yaml):

| Mode | Read | Write |
|---|---|---|
| `use` (default) | hit on match | write on miss |
| `refresh` | ignore (always re-fetch) | write |
| `bypass` | ignore | ignore |
| `off` | ignore | ignore |

Batch mode honors the same conventions for the final cached document. State files (under `batch.state_dir`) are independent of `DPE_CACHE_MODE` — they're not cache, they're in-flight tracking.

---

## Emitted events

### stdout (data + meta channels)

| Channel | When | Shape |
|---|---|---|
| `t:"d"` (data) | On successful conversion (any mode) | `{path, cache_hit, ...merged_frontmatter_fields, ...source_*, ...preserved-overlay-tags}` |
| `t:"m"` (meta) | Once per file in **batch mode only** (at submit OR resume) | `{source: "<input-path>", batch_id: "<provider-id>"}` |

In Mode A `path` is the converted file, which is also its cache entry: to re-convert documents a later stage rejected, delete their files (and `.dpe.json` sidecars) before the next run. In Mode B `path` is still the INPUT document (the envelope is a copy of the input `v`), so never delete it; re-run with `DPE_CACHE_MODE=refresh` instead. See [docs/tools/doc-converter.md](../../docs/tools/doc-converter.md).

```bash
# Mode A (output.file) ONLY - in Mode B this would delete the source documents.
jq -r '.path' invalid_docs.ndjson | xargs -r rm -f
```

Whole-doc and per-page parallel emit ONLY a data envelope — no meta. Batch mode emits one meta then either (a) the data envelope (poll completed) or (b) nothing (poll deadline / fire-and-forget — next run continues).

### stderr (logs + traces + errors, all framework-routed)

The tool uses `ctx.log(msg, level)` and `ctx.error(&v, err)` exclusively for stderr — no direct `eprintln!` in pipeline mode. The runner's stderr classifier routes them by `"type"`:

| Type | Source | Sink |
|---|---|---|
| `log` (`info`) | submit, submitted, in_progress poll, completed, etc. | logger pipe (console) |
| `log` (`warn`) | poll deadline reached (still in flight, will resume), single-shot poll still in_progress | logger pipe |
| `error` | provider Failed / Expired / result mapping mismatch / settings parse / IO | `<session>/logs/<stage>_errors.log` |
| `trace` | one per `ctx.output(v)` call (framework-emitted, automatic) | `<session>/trace/trace.N.ndjson` |

`ctx.error` always carries the original input envelope so downstream debugging has the full context.

---

## Worked example

Goal: convert a folder of mixed PDF / DOCX documents. The first page carries the document's metadata; the last page carries closing fields (signatures, dates).

**Prompt layout:**

```
configs/prompts/
├── extract_v1.txt              # base: required frontmatter keys, formatting
├── extract_v1_alpha.txt        # tag overlay (used when category="alpha")
├── extract_v1_p1-1.txt         # page 1 only: emit doc_id, doc_date, party_name
└── extract_v1_p0-1.txt         # last page: emit signed_by, signature_date
```

**Pipeline:**

```yaml
stages:
  scan:
    tool: scan-fs
    settings: { include: ["*.pdf", "*.docx"], hash: none }
    input: $input

  convert:
    tool: doc-converter
    settings:
      provider: google
      model: gemini-3-flash-preview
      temperature: 0
      max_tokens: 32000
      prompts:
        dir: "$configs/prompts"
        name: extract
        version: v1
      category: alpha               # pipeline-level value for the `category` overlay
      overlays: [category, kind]    # category + a per-envelope tag named `kind`
      overlay_defaults:
        kind: default
      page_filter: ["1-1", "0-1"]   # only page 1 + last page
      render:
        dpi: 180
        quality: 85
      output:
        file:
          dir: "$output"
          name: "{filename:nodot}.md"
        extract_frontmatter: true
    input: scan
```

**Per-page prompt composition** for page 1 of a 12-page document with `kind: "bravo"` in the envelope:

```
extract_v1.txt              <- base
extract_v1_alpha.txt        <- category overlay (settings.category = "alpha")
extract_v1_bravo.txt        <- kind overlay (envelope.v.kind, if file exists)
extract_v1_p1-1.txt         <- page-range overlay (page 1 only)
```

For the last page (page 12):

```
extract_v1.txt
extract_v1_alpha.txt
extract_v1_bravo.txt
extract_v1_p0-1.txt         <- page-range overlay (last page)
```

Pages 2-11 are skipped entirely by `page_filter`.

**Output envelope per converted document:**

```json
{
  "path":           "/data/converted/SampleDoc.md",
  "doc_id":         "DOC-001",
  "doc_date":       "2024-07-01",
  "party_name":     "Acme Corp",
  "signed_by":      "Alice Example",
  "signature_date": "2024-07-03",
  "kind":           "bravo"
}
```

---

## pdfium runtime

doc-converter uses [pdfium](https://github.com/bblanchon/pdfium-binaries) for PDF rendering. Release archives ship the library for their platform next to the `doc-converter` executable, so an installed tool works out of the box.

When building from source, fetch the pinned library first; `build.rs` then copies it next to the compiled executable:

```bash
bash tools/doc-converter/scripts/fetch-pdfium.sh          # this machine's platform
bash tools/doc-converter/scripts/fetch-pdfium.sh --all    # every platform
```

The pinned release is `tools/doc-converter/vendor/pdfium/VERSION.txt`; `SHA256SUMS` next to it holds the checksums of the archives and libraries, and the script refuses anything that does not match. The libraries land in `vendor/pdfium/<platform>/`, which git ignores. At runtime `DPE_PDFIUM_PATH` can point at any other pdfium library.

### Moving to a newer pdfium

```bash
bash tools/doc-converter/scripts/update-pdfium.sh chromium/7811   # or no argument for the latest (needs gh)
```

It rewrites `VERSION.txt` and `SHA256SUMS` and fetches every platform. Commit those two files, check that the `pdfium_<N>` API feature in `Cargo.toml` is not newer than the release, run `cargo test -p combycode-dpe-tool-doc-converter`, and note the new release in the CHANGELOG.

---

## More

See [`docs/tools/doc-converter.md`](../../docs/tools/doc-converter.md) for cookbook-style worked examples beyond the one above.

## License

[AGPL-3.0-or-later](../../LICENSE).
