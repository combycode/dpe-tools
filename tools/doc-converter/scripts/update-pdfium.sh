#!/usr/bin/env bash
# Move doc-converter to another pdfium release.
#
# Downloads every platform archive of the given bblanchon/pdfium-binaries release,
# records the release in vendor/pdfium/VERSION.txt and the archive + library checksums
# in vendor/pdfium/SHA256SUMS (both committed), then fetches the libraries locally.
# The libraries themselves are not committed: fetch-pdfium.sh downloads them.
#
#   bash tools/doc-converter/scripts/update-pdfium.sh               # latest (needs gh)
#   bash tools/doc-converter/scripts/update-pdfium.sh chromium/7811 # pin
#
# Afterwards: check the pdfium-render API feature in Cargo.toml (pdfium_<N> must not be
# newer than the release), run the doc-converter tests and note the new release in
# CHANGELOG.md.
#
# Requires: bash, curl, tar, sha256sum or shasum, gh (only when no tag is given).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"   # tools/doc-converter
VENDOR_DIR="$ROOT/vendor/pdfium"

TAG="${1:-}"
if [ -z "$TAG" ]; then
    command -v gh >/dev/null 2>&1 || { echo "no tag given and 'gh' is not installed; e.g. $0 chromium/7811" >&2; exit 2; }
    TAG=$(gh release view --repo bblanchon/pdfium-binaries --json tagName -q '.tagName')
fi
echo "[pdfium] release: $TAG"

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
    else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# <platform> <path inside archive> <file name>
PLATFORMS=(
    "linux-x64    lib/libpdfium.so      libpdfium.so"
    "linux-arm64  lib/libpdfium.so      libpdfium.so"
    "mac-x64      lib/libpdfium.dylib   libpdfium.dylib"
    "mac-arm64    lib/libpdfium.dylib   libpdfium.dylib"
    "win-x64      bin/pdfium.dll        pdfium.dll"
)

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
archives=""; libs=""
for line in "${PLATFORMS[@]}"; do
    read -r plat inner name <<<"$line"
    archive="pdfium-$plat.tgz"
    curl -fsSL "https://github.com/bblanchon/pdfium-binaries/releases/download/$TAG/$archive" -o "$TMP/$archive"
    mkdir -p "$TMP/$plat"
    tar -xzf "$TMP/$archive" -C "$TMP/$plat"
    [ -f "$TMP/$plat/$inner" ] || { echo "[pdfium] $inner not found in $archive" >&2; exit 1; }
    archives+="$(sha256 "$TMP/$archive")  $archive"$'\n'
    libs+="$(sha256 "$TMP/$plat/$inner")  $plat/$name"$'\n'
    echo "[pdfium] $plat ok"
done

mkdir -p "$VENDOR_DIR"
printf '%s' "$archives$libs" > "$VENDOR_DIR/SHA256SUMS"
{
    echo "$TAG"
    echo "updated_at: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "source: https://github.com/bblanchon/pdfium-binaries/releases/tag/$TAG"
} > "$VENDOR_DIR/VERSION.txt"

bash "$(dirname "$0")/fetch-pdfium.sh" --all
echo "[pdfium] commit vendor/pdfium/VERSION.txt and vendor/pdfium/SHA256SUMS"
