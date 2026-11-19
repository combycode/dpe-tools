# Security Policy

## Reporting a vulnerability

**Do not open public GitHub issues for security vulnerabilities.**

Email **security@combycode.com** with:
- A description of the vulnerability and its impact
- Reproduction steps (proof-of-concept welcome)
- Affected tool name and version (`dpe-tools` release tag, e.g. `v0.1.0`)
- Optional: your suggested fix, or any constraints on disclosure

We acknowledge reports within **3 business days** and aim to provide a fix or mitigation within **14 business days** for confirmed high-severity issues.

You may also use [GitHub's private security advisory](https://github.com/combycode/dpe-tools/security/advisories/new) flow for the same effect.

## Scope

This repository ships **custom DPE tools** distributed via a curated `catalog.json`. In-scope vulnerabilities include:

- Code execution, sandbox escape, or privilege escalation in any tool binary
- Memory-safety bugs in the Rust tools
- SSRF / SQLi / NoSQLi in tools that talk to external services (`mongo-find`, `mongo-upsert`, `llm`)
- Catalog tampering or signature-bypass scenarios
- Path traversal in tools that read or write files (`xlsx`, `read-tables`, `doc-converter`)

Out of scope here, but in scope for the upstream `dpe` repository:
- Vulnerabilities in the `dpe` runner, frameworks, or the 7 standard tools (scan-fs, read-file-stream, write-file-stream, write-file-stream-hashed, normalize, gate, checkpoint) — report at [combycode/dpe security](https://github.com/combycode/dpe/security/advisories/new).

## Supported versions

Only the latest minor release line of the latest major version receives security updates. Check the [latest release](https://github.com/combycode/dpe-tools/releases/latest).

## Disclosure

Once a fix is available we publish:
1. A patched release.
2. A regenerated `catalog.json` with updated sha256s.
3. A GitHub Security Advisory describing the issue, affected versions, and the fix.

We coordinate disclosure timing with reporters where reasonable.
