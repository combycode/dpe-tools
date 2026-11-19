#!/usr/bin/env bash
# Fetch the pinned pdfium shared library for doc-converter.
#
# The binaries are not stored in git. This script downloads the release named in
# vendor/pdfium/VERSION.txt from https://github.com/bblanchon/pdfium-binaries, checks
# each archive and library against vendor/pdfium/SHA256SUMS, and extracts:
#
#   vendor/pdfium/<platform>/libpdfium.so | libpdfium.dylib | pdfium.dll
#
# build.rs copies the library next to the doc-converter binary.
#
# Usage:
#   bash tools/doc-converter/scripts/fetch-pdfium.sh            # this machine's platform
#   bash tools/doc-converter/scripts/fetch-pdfium.sh linux-arm64 win-x64
#   bash tools/doc-converter/scripts/fetch-pdfium.sh --all
#
# Platforms: linux-x64 linux-arm64 mac-x64 mac-arm64 win-x64.
# A library that is already present with the pinned checksum is left alone.
# Requires: bash, curl, tar, sha256sum or shasum.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"   # tools/doc-converter
VENDOR_DIR="$ROOT/vendor/pdfium"
SUMS="$VENDOR_DIR/SHA256SUMS"
TAG=$(head -1 "$VENDOR_DIR/VERSION.txt")
ALL=(linux-x64 linux-arm64 mac-x64 mac-arm64 win-x64)

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
    else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

expected() {   # expected <name as listed in SHA256SUMS>
    local s; s=$(awk -v n="$1" '$2 == n { print $1 }' "$SUMS")
    [ -n "$s" ] || { echo "[pdfium] no checksum for $1 in $SUMS" >&2; exit 1; }
    echo "$s"
}

host_platform() {
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64)               echo linux-x64 ;;
        Linux-aarch64|Linux-arm64)  echo linux-arm64 ;;
        Darwin-arm64)               echo mac-arm64 ;;
        Darwin-x86_64)              echo mac-x64 ;;
        MINGW*|MSYS*|CYGWIN*)       echo win-x64 ;;
        *) echo "[pdfium] unsupported host $(uname -s)-$(uname -m)" >&2; exit 1 ;;
    esac
}

lib_path() {   # lib_path <platform> -> <path inside archive> <file name>
    case "$1" in
        linux-*) echo "lib/libpdfium.so libpdfium.so" ;;
        mac-*)   echo "lib/libpdfium.dylib libpdfium.dylib" ;;
        win-x64) echo "bin/pdfium.dll pdfium.dll" ;;
        *) echo "[pdfium] unknown platform $1" >&2; exit 1 ;;
    esac
}

case "${1:-}" in
    "")    PLATFORMS=("$(host_platform)") ;;
    --all) PLATFORMS=("${ALL[@]}") ;;
    *)     PLATFORMS=("$@") ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

for plat in "${PLATFORMS[@]}"; do
    read -r inner name <<<"$(lib_path "$plat")"
    dest="$VENDOR_DIR/$plat/$name"
    want_lib=$(expected "$plat/$name")
    if [ -f "$dest" ] && [ "$(sha256 "$dest")" = "$want_lib" ]; then
        echo "[pdfium] $plat: $name present ($TAG)"
        continue
    fi
    archive="pdfium-$plat.tgz"
    url="https://github.com/bblanchon/pdfium-binaries/releases/download/$TAG/$archive"
    echo "[pdfium] $plat: downloading $url"
    curl -fsSL "$url" -o "$TMP/$archive"
    [ "$(sha256 "$TMP/$archive")" = "$(expected "$archive")" ] \
        || { echo "[pdfium] $plat: archive checksum mismatch" >&2; exit 1; }
    mkdir -p "$TMP/$plat"
    tar -xzf "$TMP/$archive" -C "$TMP/$plat"
    [ "$(sha256 "$TMP/$plat/$inner")" = "$want_lib" ] \
        || { echo "[pdfium] $plat: library checksum mismatch" >&2; exit 1; }
    mkdir -p "$VENDOR_DIR/$plat"
    cp "$TMP/$plat/$inner" "$dest"
    chmod 0644 "$dest"
    echo "[pdfium] $plat: wrote $dest"
done
