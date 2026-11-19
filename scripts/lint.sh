#!/usr/bin/env bash
# Runs every linter across the repo. Exits non-zero on first failure.
# Strict mode: all warnings are errors. No --fix here; use scripts/fmt.sh
# for auto-fixable formatting.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "[lint] no machine paths in tracked files"
# A published repo must not carry home directories or paths that escape the repo.
leaks=$(git ls-files -z | xargs -0 grep -IlE '\.\./\.\./\.\./|[A-Za-z]:[/\]Users[/\]|/home/[a-z]+/|/Users/[A-Za-z]+/' 2>/dev/null || true)
if [ -n "$leaks" ]; then
    echo "[lint] machine-specific paths found in:"; echo "$leaks"; exit 1
fi

echo "[lint] cargo clippy --workspace (all targets, -D warnings)"
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo "[lint] biome check (TS tools)"
# Run only on TS tool dirs (skip per-tool node_modules; biome respects .gitignore).
TS_DIRS=()
for d in tools/*/package.json; do
    TS_DIRS+=("$(dirname "$d")")
done
if [ ${#TS_DIRS[@]} -gt 0 ]; then
    bunx --bun @biomejs/biome@2.4.13 check "${TS_DIRS[@]}"
else
    echo "[lint]   no TS tools - skipping biome"
fi

echo "[lint] tsc --noEmit (each TS tool)"
for d in "${TS_DIRS[@]}"; do
    if [ -f "$d/tsconfig.json" ]; then
        ( cd "$d" && bun x tsc --noEmit )
    fi
done

# ─── actionlint + shellcheck ──────────────────────────────────────────────
# Catches workflow YAML syntax, invalid action references, expression
# mistakes, AND shellcheck warnings inside `run:` bash blocks. shellcheck
# integration is critical: plain actionlint misses real bugs (SC2086 word
# splitting, SC2129 redirect grouping, etc.) that CI would catch otherwise -
# leading to "passed locally, failed in CI."
#
# Both tools self-install to scripts/.bin/ on first run if missing.
BIN_DIR="$ROOT/scripts/.bin"
EXE_SUFFIX=""
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) EXE_SUFFIX=".exe" ;;
esac

# shellcheck must be on PATH (or in BIN_DIR) before actionlint runs;
# otherwise actionlint silently SKIPS the shellcheck step.
SHELLCHECK_VERSION="0.10.0"
if command -v shellcheck >/dev/null 2>&1; then
    : # already on PATH
elif [ -x "$BIN_DIR/shellcheck$EXE_SUFFIX" ]; then
    export PATH="$BIN_DIR:$PATH"
else
    echo "[lint]   first-run: downloading shellcheck v${SHELLCHECK_VERSION}"
    mkdir -p "$BIN_DIR"
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64)         sc_url="https://github.com/koalaman/shellcheck/releases/download/v${SHELLCHECK_VERSION}/shellcheck-v${SHELLCHECK_VERSION}.linux.x86_64.tar.xz" ;;
        Linux-aarch64)        sc_url="https://github.com/koalaman/shellcheck/releases/download/v${SHELLCHECK_VERSION}/shellcheck-v${SHELLCHECK_VERSION}.linux.aarch64.tar.xz" ;;
        Darwin-x86_64|Darwin-arm64) sc_url="https://github.com/koalaman/shellcheck/releases/download/v${SHELLCHECK_VERSION}/shellcheck-v${SHELLCHECK_VERSION}.darwin.aarch64.tar.xz" ;;
        MINGW*|MSYS*|CYGWIN*) sc_url="https://github.com/koalaman/shellcheck/releases/download/v${SHELLCHECK_VERSION}/shellcheck-v${SHELLCHECK_VERSION}.zip" ;;
        *) echo "[lint] shellcheck: unsupported platform $(uname -s)-$(uname -m)" >&2; exit 1 ;;
    esac
    tmp="$(mktemp -d)"
    if [[ "$sc_url" == *.zip ]]; then
        curl -fsSL "$sc_url" -o "$tmp/sc.zip"
        unzip -q "$tmp/sc.zip" -d "$BIN_DIR"
    else
        curl -fsSL "$sc_url" | tar -xJ -C "$tmp"
        cp "$tmp"/shellcheck-*/shellcheck "$BIN_DIR/shellcheck"
        chmod +x "$BIN_DIR/shellcheck"
    fi
    rm -rf "$tmp"
    export PATH="$BIN_DIR:$PATH"
fi

ACTIONLINT_VERSION="1.7.12"
ACTIONLINT_BIN="$BIN_DIR/actionlint$EXE_SUFFIX"
echo "[lint] actionlint (GitHub workflows, with shellcheck)"
if command -v actionlint >/dev/null 2>&1; then
    ACTIONLINT_CMD=actionlint
elif [ -x "$ACTIONLINT_BIN" ]; then
    ACTIONLINT_CMD="$ACTIONLINT_BIN"
else
    echo "[lint]   first-run: downloading actionlint v${ACTIONLINT_VERSION}"
    mkdir -p "$BIN_DIR"
    bash <(curl -fsSL "https://raw.githubusercontent.com/rhysd/actionlint/v${ACTIONLINT_VERSION}/scripts/download-actionlint.bash") \
        "$ACTIONLINT_VERSION" "$BIN_DIR" >/dev/null
    ACTIONLINT_CMD="$ACTIONLINT_BIN"
fi
"$ACTIONLINT_CMD" -no-color -oneline .github/workflows/*.yml

echo "[lint] ALL PASSED"
