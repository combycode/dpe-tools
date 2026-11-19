# Contributing to dpe-tools

Thanks for your interest in contributing custom DPE tools. This document is the canonical guide for setting up a dev environment, the conventions we follow, and the PR review process.

## Scope of this repository

`dpe-tools` ships **custom tools** distributed as a curated `catalog.json` for use with the [dpe runner](https://github.com/combycode/dpe). It does NOT ship:
- The `dpe` runner or `dpe-dev` CLI
- The Rust / TypeScript / Python frameworks
- The 7 standard tools (`scan-fs`, `read-file-stream`, `write-file-stream`, `write-file-stream-hashed`, `normalize`, `gate`, `checkpoint`)

Issues or PRs that touch any of the above belong in the upstream [combycode/dpe](https://github.com/combycode/dpe) repo, not here.

## License

By submitting a contribution, you agree your work is licensed under the project's [AGPL-3.0-or-later](./LICENSE). Inbound = outbound; no separate CLA required.

## Local development

### Prerequisites

- A working [`dpe`](https://github.com/combycode/dpe) install for end-to-end tool testing.
- Rust **1.91+** (`rustup`) — for the Rust tools.
- Bun **1.2+** (`https://bun.sh`) — for the TypeScript tools.
- Tool-specific runtime deps (a MongoDB server for the `mongo-*` integration tests, LibreOffice `soffice` for DOCX input in `doc-converter`, etc.) — see each tool's README.
- `doc-converter` needs the pdfium library, which is not stored in git: run `bash tools/doc-converter/scripts/fetch-pdfium.sh` once before building or testing it.

### Build a single tool

Each tool lives at `tools/<name>/` and builds independently.

**Rust tools:**
```bash
cd tools/<name>
cargo build --release
```

**TypeScript tools:**
```bash
cd tools/<name>
bun install
bun build src/index.ts --outfile dist/<name>
```

### Build the whole workspace

```bash
bash tools/doc-converter/scripts/fetch-pdfium.sh   # pdfium for doc-converter
cargo build --workspace --release      # all Rust tools
for d in tools/*/package.json; do      # all TS tools
  (cd "$(dirname "$d")" && bun install && bun run build)
done
```

### Test

- Rust: `cargo test --workspace`
- TypeScript: `cd tools/<name> && bun test`

### Lint

```bash
bash scripts/lint.sh
```

Mirrors the upstream `dpe` lint suite: `cargo clippy -D warnings` -> `biome check` -> `tsc --noEmit` -> `actionlint` (with `shellcheck`). `actionlint` and `shellcheck` self-install to `scripts/.bin/` on first run if not on `PATH`.

### Pre-commit gate

**Every commit must pass `bash scripts/lint.sh` AND the relevant test command before being committed.** No exceptions, no bypassing via `--no-verify`. CI enforces the same gate.

## Adding a new tool

1. Create `tools/<name>/` with: `meta.json`, `spec.yaml`, `README.md`, `tests/`, and language-specific build files (`Cargo.toml` / `package.json`).
2. The cargo `[[bin]] name` (Rust) or built artifact name (TS) MUST match `<name>` exactly. `meta.json::entry` must point to the produced binary path.
3. Add an entry to `Cargo.toml` workspace members (Rust tools only).
4. Add a `[Unreleased]` line to `CHANGELOG.md`.
5. Add user-facing docs at `docs/tools/<name>.md`.
6. Confirm `bash scripts/lint.sh` passes, then open a PR.

## Commit message style

- Title is one short line (<= 72 chars), imperative mood
- Use a `type(scope): subject` prefix from this set: `feat`, `fix`, `chore`, `ci`, `docs`, `refactor`, `test`
- **Body only when the *why* is non-obvious.** No plan checklists, validation reports, or step lists in the body - those belong in CHANGELOG / release notes / PR description
- Mark breaking changes with `BREAKING CHANGE:` in the body, never silently

Good examples:
```
fix(xlsx): handle merged cells spanning > 1024 rows
feat(llm): add streaming output mode
chore: bump tokio 1.40 -> 1.41
```

## Pull requests

- Keep PRs focused - one tool or one logical concern per PR
- Reference the issue being solved (e.g. `Fixes #42`)
- Update `CHANGELOG.md` under `[Unreleased]`
- Update `docs/tools/<name>.md` when behaviour changes
- Add tests for new code paths
- CI must be green before merge

## Reporting bugs / suggesting features

Use the GitHub issue templates:
- [Bug report](https://github.com/combycode/dpe-tools/issues/new?template=bug_report.yml)
- [Feature request](https://github.com/combycode/dpe-tools/issues/new?template=feature_request.yml)
- Questions go to [GitHub Discussions](https://github.com/combycode/dpe-tools/discussions), not Issues.

For security-sensitive reports, see [SECURITY.md](./SECURITY.md) - please do not file public issues for vulnerabilities.

## Project layout

```
dpe-tools/
├── tools/<name>/      # one directory per tool
│   ├── meta.json      # runner-readable metadata (entry, spec, version)
│   ├── spec.yaml      # JSON-Schema-shape settings (used by UI editors)
│   ├── Cargo.toml     # Rust tools
│   ├── package.json   # TypeScript tools
│   ├── src/           # implementation
│   ├── tests/         # tool-local tests
│   └── README.md      # user-facing tool docs
├── docs/tools/<name>.md   # extended docs / examples / cookbook
├── scripts/
│   ├── build-catalog.sh   # generates catalog.json from tools/*/meta.json + release assets
│   └── lint.sh
├── catalog.json       # generated; do not hand-edit
└── .github/workflows/release.yml   # tag -> build per-platform tarballs + publish catalog.json
```

## Code of conduct

Participation in this project is governed by [CODE_OF_CONDUCT.md](./CODE_OF_CONDUCT.md). Be kind, be specific, assume good intent.
