# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

First public release of the curated DPE tool set.

### Added

- Tools:
  - **classify** (TypeScript) — rule-based classifier: ordered JS-expression rules over configurable dictionaries attach `{className, confidence, reason, data}` to each envelope.
  - **doc-converter** (Rust) — converts PDF / DOCX / image documents through LLM vision (Anthropic or Google), page by page with layered prompts.
  - **filter** (TypeScript) — filters, maps and transforms an NDJSON stream with JS expressions.
  - **llm** (TypeScript) — a text LLM call (Anthropic or Google) with layered prompts and text or JSON output.
  - **mongo-find** (Rust) — reads MongoDB documents once at startup or per input envelope.
  - **mongo-upsert** (Rust) — streaming MongoDB upserts with a mix of `$set`, `$addToSet`, `$push`, `$inc`, `$unset` per envelope, flushed in batches of `bulk_size` concurrent `updateOne` calls (MongoDB 7 compatible).
  - **read-tables** (Rust) — finds tables in sheet JSON (header bands bounded by merge coverage, title rows, stacked headers) and extracts them: `parse` (one envelope per row), `tables` (one file per table) and `detect` (format discovery) modes.
  - **validate** (Rust) — JSON Schema validation (draft detected from `$schema`, Draft 2020-12 by default; under 2020-12 `format` is an annotation, under Draft 4/6/7 it is enforced) in `mark` or `filter` mode.
  - **xlsx** (Rust) — extracts every sheet of a workbook to JSON (schema 4): formulas with cached values, horizontal / vertical / 2D merges as `{span, rspan, v}` anchors, empty styled merge anchors kept; entity references resolved; invalid UTF-8 bytes from third-party writers become U+FFFD instead of dropping text.
- **doc-converter**:
  - `request_timeout_ms` and a `retry:` block (exponential backoff on rate limits, server errors, timeouts and connection errors) for every dispatch mode, including batch submit / poll / results.
  - two output modes: `output.file` (`dir`, `name` as `v.<field>` or a template with `safe`, `short[:N]`, `nodot`, `lower`, `upper` filters, `target_ext`, `invalidation: strict | sidecar | none`) and `output.field`. A resolved name that is absolute, has a drive, or contains an empty, `.` or `..` segment is rejected.
  - `cache_hit` and the source-identity fields (`source_path`, `source_hash`, `source_size`, `source_filename`, `source_ext`, `source_file`) on every output envelope; in file mode the written file is the cache entry, so deleting it forces a fresh conversion.
  - release archives include the pdfium library for their platform (chromium/7811). It is not stored in git: `tools/doc-converter/scripts/fetch-pdfium.sh` downloads the pinned release and checks it against `vendor/pdfium/SHA256SUMS`.
- **doc-converter**, **llm**: `on_failure: stderr | envelope | both`. In `envelope` mode a failure is emitted on the data channel with `failed`, `error_class`, `error_msg`, `error_attempt` and the source-identity fields, so a pipeline can route failures to a dead-letter sink or feed them back for another attempt; `error_attempt` counts the attempts across such loops.
- **llm**: JSON output keeps the input's source-identity fields when the model does not return them.
- Repository: `LICENSE` (AGPL-3.0), `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, issue and PR templates, CI on Linux / Windows / macOS.
