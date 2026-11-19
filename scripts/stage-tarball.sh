#!/usr/bin/env bash
# Pack one tool into the release tarball that `dpe install` unpacks.
#
# Usage: scripts/stage-tarball.sh <tool-dir> <platform> <out-dir> [<release-dir>]
#   tool-dir     tools/<name>
#   platform     linux-amd64 | linux-arm64 | darwin-amd64 | darwin-arm64 | windows-amd64
#                (the names `dpe install` uses to pick a tarball and its sha256)
#   out-dir      where <name>-<platform>.tar.gz is written
#   release-dir  Rust tools only: the cargo release dir holding the binary
#                (doc-converter also needs the pdfium library build.rs staged there)
#
# `dpe install <name>` extracts the tarball into <tools>/<name>/, so the archive root is the
# tool folder itself, laid out like a source checkout so meta.json `entry` resolves:
#   Rust: meta.json spec.yaml README.md target/release/<name>[.exe] [+ pdfium library]
#   TS:   meta.json spec.yaml README.md package.json [bun.lock] src/
# The archive is byte-for-byte reproducible (GNU tar; gtar on macOS).
set -euo pipefail

[ $# -ge 3 ] || { sed -n '2,15p' "$0"; exit 2; }
tool_dir="${1%/}"; platform="$2"; out_dir="$3"; release_dir="${4:-}"
tool=$(basename "$tool_dir")

case "$platform" in
    linux-amd64|linux-arm64|darwin-amd64|darwin-arm64|windows-amd64) ;;
    *) echo "unknown platform: $platform" >&2; exit 2 ;;
esac

TAR=tar
if [ "$(uname -s)" = Darwin ]; then
    command -v gtar >/dev/null || brew install gnu-tar
    TAR=gtar
fi
TARDET=(--sort=name --mtime='@1700000000' --owner=0 --group=0 --numeric-owner
        --use-compress-program='gzip -n')

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for f in meta.json spec.yaml README.md; do cp "$tool_dir/$f" "$work/$f"; done

if [ -f "$tool_dir/Cargo.toml" ]; then
    [ -n "$release_dir" ] || { echo "$tool: Rust tool needs <release-dir>" >&2; exit 2; }
    exe=""; [ "${platform%%-*}" = windows ] && exe=.exe
    bin="$release_dir/$tool$exe"
    [ -f "$bin" ] || { echo "missing binary: $bin" >&2; exit 1; }
    mkdir -p "$work/target/release"
    cp "$bin" "$work/target/release/$tool$exe"
    chmod +x "$work/target/release/$tool$exe"
    if [ "$tool" = doc-converter ]; then
        lib=""
        for l in pdfium.dll libpdfium.so libpdfium.dylib; do
            [ -f "$release_dir/$l" ] && lib=$l
        done
        [ -n "$lib" ] || { echo "doc-converter: no pdfium in $release_dir" >&2; exit 1; }
        size=$(wc -c < "$release_dir/$lib")
        [ "$size" -gt 1000000 ] || { echo "doc-converter: $lib is only $size bytes" >&2; exit 1; }
        cp "$release_dir/$lib" "$work/target/release/$lib"
    fi
elif [ -f "$tool_dir/package.json" ]; then
    cp -r "$tool_dir/src" "$work/src"
    cp "$tool_dir/package.json" "$work/package.json"
    [ -f "$tool_dir/bun.lock" ] && cp "$tool_dir/bun.lock" "$work/bun.lock"
else
    echo "$tool: neither Cargo.toml nor package.json" >&2; exit 2
fi

mkdir -p "$out_dir"
"$TAR" "${TARDET[@]}" -cf "$out_dir/$tool-$platform.tar.gz" -C "$work" .
echo "staged $out_dir/$tool-$platform.tar.gz"
