#!/usr/bin/env bash
# Fetch the public EXIF fixture corpus (docs/exif-fixture-coverage.md) into
# $PHOTOSTACKER_FIXTURES, or the folder given as the first argument.
#
# Usage (bash, with curl, sha256sum and python3):
#   scripts/fetch-raw-samples.sh [DEST] [PATH-REGEX]
#
# - The corpus is scripts/fixtures.tsv: path, sha256, URL, zip entry, licence.
# - A file already present with the right sha256 is skipped; nothing is
#   re-downloaded. Every download is verified before it replaces anything.
# - Empa (Zenodo) files are single entries of large zips: they are extracted
#   with HTTP Range requests by scripts/zip-range-extract.py, which checks
#   their CRC32.
# - Wikimedia Commons asks for a descriptive User-Agent and returns HTTP 429
#   quickly: requests to it are spaced out.
# - PATH-REGEX (grep -E) limits the run to the matching paths.
#
# DEST must be outside the repository: the fixtures are never committed.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LIST="$SCRIPT_DIR/fixtures.tsv"
EXTRACT="$SCRIPT_DIR/zip-range-extract.py"
UA="photostacker-fixtures/0.1 (+https://github.com/riccardogabellone/photostacker; test fixtures)"
WIKIMEDIA_DELAY_S=3

DEST="${1:-${PHOTOSTACKER_FIXTURES:-}}"
FILTER="${2:-}"
if [[ -z "$DEST" ]]; then
    echo "usage: $0 [DEST] [PATH-REGEX]   (or set PHOTOSTACKER_FIXTURES)" >&2
    exit 2
fi
DEST="$(realpath -m -- "$DEST")"
case "$DEST/" in
    "$REPO_ROOT"/*)
        echo "refusing to download into the repository ($DEST): fixtures are never committed" >&2
        exit 2
        ;;
esac
mkdir -p "$DEST"
for tool in curl sha256sum python3; do
    command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 2; }
done

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

sha_of() { sha256sum "$1" | cut -d' ' -f1; }

present=0 fetched=0 failed=0
while IFS=$'\t' read -r path sha url entry licence; do
    [[ -z "$path" || "$path" == \#* ]] && continue
    if [[ -n "$FILTER" ]] && ! grep -qE -- "$FILTER" <<<"$path"; then
        continue
    fi
    target="$DEST/$path"
    if [[ -f "$target" && "$(sha_of "$target")" == "$sha" ]]; then
        present=$((present + 1))
        continue
    fi
    echo "fetch $path ($licence)"
    part="$TMP/download"
    rm -f "$part"
    ok=1
    if [[ "$entry" == "-" ]]; then
        if [[ "$url" == https://upload.wikimedia.org/* ]]; then
            sleep "$WIKIMEDIA_DELAY_S"
        fi
        curl --fail --location --silent --show-error --retry 5 --retry-delay 10 \
            --user-agent "$UA" --output "$part" "$url" || ok=0
    else
        python3 -I "$EXTRACT" "$url" "$entry" "$part" "$UA" || ok=0
    fi
    if [[ "$ok" == 1 && "$(sha_of "$part")" != "$sha" ]]; then
        echo "  sha256 mismatch: got $(sha_of "$part"), want $sha" >&2
        ok=0
    fi
    if [[ "$ok" == 1 ]]; then
        mkdir -p "$(dirname "$target")"
        mv -f "$part" "$target"
        fetched=$((fetched + 1))
    else
        echo "  FAILED $path" >&2
        failed=$((failed + 1))
    fi
done <"$LIST"

echo "$present already present, $fetched fetched, $failed failed -> $DEST"
[[ "$failed" == 0 ]]
