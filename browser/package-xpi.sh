#!/usr/bin/env bash
# Build the Firefox .xpi package for AMO submission.
#
# Zips only the extension files (manifest.json, background.js,
# content.js) from browser/firefox/ — the native-host template
# (com.zdm.native.json) is NOT part of the extension and is left out.
# The .xpi is UNSIGNED: Firefox Release/Beta will not install it.
# It exists so it can be submitted to addons.mozilla.org for signing
# (see browser/README.md, "Firefox, permanent install").
#
# Output: browser/firefox/zdm-firefox-<version>.xpi
#   (version read from browser/firefox/manifest.json)
#
# Usage:
#   ./browser/package-xpi.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FF="$ROOT/browser/firefox"

command -v zip >/dev/null || { echo "zip is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 1; }

# The per-browser copies must match the shared source of truth
# (browser/background.js, browser/content.js).
diff -q "$ROOT/browser/background.js" "$FF/background.js" >/dev/null \
    || { echo "background.js differs from browser/background.js — sync first" >&2; exit 1; }
diff -q "$ROOT/browser/content.js" "$FF/content.js" >/dev/null \
    || { echo "content.js differs from browser/content.js — sync first" >&2; exit 1; }

VERSION="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["version"])' "$FF/manifest.json")"
OUT="$FF/zdm-firefox-$VERSION.xpi"

python3 -m json.tool "$FF/manifest.json" > /dev/null

cd "$FF"
rm -f "$OUT"
zip -q -X "$OUT" manifest.json background.js content.js

echo "Built: $OUT"
unzip -l "$OUT"
