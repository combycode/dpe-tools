# dpe-tools

Curated **custom tools** for the [DPE](https://github.com/combycode/dpe) streaming runner. Distributed as a single `catalog.json` file plus per-platform tarballs hosted on GitHub Releases - no extra package registries, no auto-update daemon.

The 7 standard tools (`scan-fs`, `read-file-stream`, `write-file-stream`, `write-file-stream-hashed`, `normalize`, `gate`, `checkpoint`) ship inside the `dpe` runner itself. **This repository ships everything else.**

## Tools in this catalog

| Tool          | Runtime    | What it does                                                                |
|---------------|------------|-----------------------------------------------------------------------------|
| `classify`    | TypeScript | Heuristic table classification for heterogeneous spreadsheets               |
| `doc-converter` | Rust     | Convert PDF / DOCX / RTF to text or structured rows                         |
| `filter`      | TypeScript | Pass-through filter with predicate DSL (companion to runner's built-in)     |
| `llm`         | TypeScript | LLM-backed row processor with provider abstraction                          |
| `mongo-find`  | Rust       | Streaming MongoDB reader                                                    |
| `mongo-upsert`| Rust       | Streaming MongoDB writer with idempotent upserts                            |
| `read-tables` | Rust       | Tabular file reader (CSV / TSV / Parquet) with schema inference             |
| `validate`    | Rust       | JSON Schema validator: tag (mark) or drop (filter) by schema validity       |
| `xlsx`        | Rust       | Full-fidelity XLSX extractor with merge handling and formula preservation   |

Per-tool docs: [`docs/tools/<name>.md`](./docs/tools).

---

## Using these tools (end-user instructions)

The `dpe` runner reads tool registries listed in `config.toml::tools_registries`. Each registry is a local `catalog.json` file that maps tool names to download URLs and per-platform sha256 hashes. To use the dpe-tools catalog:

### 1. Download the catalog

Pick one of:

- **Latest stable release** (recommended):
  ```
  https://github.com/combycode/dpe-tools/releases/latest/download/catalog.json
  ```
- **A specific tagged release** (pin to an exact set of tool versions):
  ```
  https://github.com/combycode/dpe-tools/releases/download/v<X.Y.Z>/catalog.json
  ```

Save the file anywhere on disk. We recommend `~/.dpe/registries/` so it sits next to your other DPE state:

```bash
# Linux / macOS
mkdir -p ~/.dpe/registries
curl -L https://github.com/combycode/dpe-tools/releases/latest/download/catalog.json \
     -o ~/.dpe/registries/dpe-tools.json
```

```powershell
# Windows
New-Item -ItemType Directory -Force -Path "$HOME\.dpe\registries" | Out-Null
Invoke-WebRequest `
  -Uri  https://github.com/combycode/dpe-tools/releases/latest/download/catalog.json `
  -OutFile "$HOME\.dpe\registries\dpe-tools.json"
```

### 2. Add the catalog to `config.toml`

Edit `~/.dpe/config.toml` (the file `dpe` reads at startup; see the [dpe configuration docs](https://github.com/combycode/dpe/blob/main/docs/configuration.md)) and add the file path to `tools_registries`. The list is searched in declaration order with **first-match-wins** semantics on tool name - so a private registry listed first shadows a public one.

```toml
tools_registries = [
    "~/.dpe/registries/dpe-tools.json",
    # any other registries you have, e.g.:
    # "~/.dpe/registries/internal.json",
]
```

If `tools_registries` is empty, `dpe` falls back to a `catalog.json` next to its binary. Once you add a path, that fallback is bypassed - so include the runner's own catalog explicitly if you still want the standard-tool entries:

```toml
tools_registries = [
    "~/.dpe/registries/dpe-tools.json",
    "~/.dpe/catalog.json",   # standard-tool entries that ship next to dpe (if any)
]
```

### 3. Install a tool

```bash
dpe install <tool-name>
```

Example:
```bash
dpe install xlsx
```

This:
1. Resolves `xlsx` against the merged catalog from your `tools_registries`.
2. Downloads the platform-specific tarball from the URL in the catalog entry's `binary.url_template` (substituting `{os}` / `{arch}` / `{version}`).
3. Verifies the sha256 against `binary.sha256[<platform>]`.
4. Extracts to `~/.dpe/tools/<tool-name>/` (containing `meta.json` + the binary + `spec.yaml` + `README.md`).

After install, the tool is usable from any pipeline by name:
```yaml
stages:
  - name: extract
    tool: xlsx
    settings:
      input_path: "/data/in.xlsx"
```

### 4. Update the catalog

The catalog is just a file - re-download it whenever you want the latest entries:

```bash
curl -L https://github.com/combycode/dpe-tools/releases/latest/download/catalog.json \
     -o ~/.dpe/registries/dpe-tools.json
```

Already-installed tools are NOT auto-upgraded. Re-run `dpe install <name>` to pick up a newer version.

### 5. Listing what's available

```bash
dpe tools list                # all entries from all configured registries
dpe tools info <tool-name>    # show the catalog entry, source registry, and any local install state
```

---

## How the catalog model works

A catalog is a JSON file shaped like:

```json
{
  "version": "0.1.0",
  "updated_at": "2026-04-28T00:00:00Z",
  "tools": {
    "xlsx": {
      "description": "XLSX extractor",
      "runtime": "rust",
      "version": "0.1.0",
      "binary": {
        "name": "xlsx",
        "url_template": "https://github.com/combycode/dpe-tools/releases/download/v{version}/xlsx-{os}-{arch}.tar.gz",
        "sha256": {
          "linux-amd64":   "...",
          "linux-arm64":   "...",
          "darwin-amd64":  "...",
          "darwin-arm64":  "...",
          "windows-amd64": "..."
        }
      }
    }
  }
}
```

The runner ships HTTP support for `https://` + `file://` URLs, sha256 verification, and tarball extraction. All `dpe install` does is consume this format - so any third party can publish their own `catalog.json` and it will work.

You can mix multiple catalogs:
- `~/.dpe/registries/dpe-tools.json` - this repo's curated set
- `~/.dpe/registries/<your-org>.json` - your private internal tools
- `file:///path/to/local-dev.json` - tools you're developing locally (use `file://` URLs in `url_template`)

First-match-wins on tool name lets local entries shadow upstream entries.

---

## Building from source

You don't need to do this to USE the tools - the GitHub Releases artifacts are pre-built. Build from source only if you want to develop, modify, or run on a platform we don't pre-build for.

```bash
git clone https://github.com/combycode/dpe-tools
cd dpe-tools

# Rust tools
cargo build --workspace --release
# binaries land at target/release/<tool-name>

# TypeScript tools
for d in tools/*/package.json; do
  (cd "$(dirname "$d")" && bun install && bun run build)
done
```

To use a locally-built tool with `dpe install`, generate a local `catalog.json` whose `url_template` uses `file://` URLs pointing at your built artifacts. See [`scripts/build-catalog.sh`](./scripts/build-catalog.sh).

---

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md). New tool proposals are welcome, but please open a discussion first to confirm fit (we don't want to fork the standard-tool surface, and tools that should be upstream belong in [combycode/dpe](https://github.com/combycode/dpe)).

## Security

See [SECURITY.md](./SECURITY.md). Do **not** file public issues for vulnerabilities.

## License

[AGPL-3.0-or-later](./LICENSE). Each tool may pull in its own dependencies under their respective licenses; per-tool license inventories live in each tool's `README.md`.

For commercial licensing terms, contact [combycode.com/contact](https://combycode.com/contact).

## Related projects

- [combycode/dpe](https://github.com/combycode/dpe) - the runner, frameworks, and 7 standard tools
- [combycode/dpe configuration docs](https://github.com/combycode/dpe/blob/main/docs/configuration.md) - full `config.toml` reference
