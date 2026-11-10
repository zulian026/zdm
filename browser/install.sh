#!/usr/bin/env bash
# ZDM browser integration installer — run on the target machine.
# Registers browser/zdm-native-host.py as the "com.zdm.native" host
# for Chromium and Firefox. No sudo needed.
#
# Usage:
#   cd ~/workspace/zdm-tauri && ./browser/install.sh
#   CHROMIUM_EXTENSION_ID=<id from chrome://extensions> ./browser/install.sh
#       (only needed if chrome://extensions ever shows an ID other
#       than the pinned one below; the manifest's "key" pins it, so
#       this should not be necessary)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOST="$ROOT/browser/zdm-native-host.py"
# Pinned by the "key" field in browser/chromium/manifest.json, so the
# extension ID is stable for every install from this folder.
CHROMIUM_EXT_ID="${CHROMIUM_EXTENSION_ID:-mojdpgikbnejopblfjfphlkdnogkkimh}"

command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 1; }

chmod +x "$HOST"
mkdir -p "$HOME/.config/chromium/NativeMessagingHosts" \
         "$HOME/.mozilla/native-messaging-hosts"

# Chromium: allowed_origins must contain the extension's ID.
sed -e "s|__ZDM_HOST_PATH__|$HOST|g" \
    -e "s|mojdpgikbnejopblfjfphlkdnogkkimh|$CHROMIUM_EXT_ID|g" \
    "$ROOT/browser/chromium/com.zdm.native.json" \
    > "$HOME/.config/chromium/NativeMessagingHosts/com.zdm.native.json"

# Firefox: allowed_extensions must contain the gecko id from
# browser/firefox/manifest.json (zdm@example.local).
sed -e "s|__ZDM_HOST_PATH__|$HOST|g" \
    "$ROOT/browser/firefox/com.zdm.native.json" \
    > "$HOME/.mozilla/native-messaging-hosts/com.zdm.native.json"

python3 -m json.tool \
    "$HOME/.config/chromium/NativeMessagingHosts/com.zdm.native.json" > /dev/null
python3 -m json.tool \
    "$HOME/.mozilla/native-messaging-hosts/com.zdm.native.json" > /dev/null
python3 -m py_compile "$HOST"

echo "ZDM native host registered:"
echo "  host:     $HOST"
echo "  chromium: $HOME/.config/chromium/NativeMessagingHosts/com.zdm.native.json"
echo "  firefox:  $HOME/.mozilla/native-messaging-hosts/com.zdm.native.json"
echo "  chromium extension id: $CHROMIUM_EXT_ID"
echo
echo "Still manual — browsers require a click in the UI:"
echo "  Chromium: chrome://extensions -> Developer mode ON -> Load unpacked"
echo "            -> $ROOT/browser/chromium"
echo "  Firefox:  about:debugging#/runtime/this-firefox -> Load Temporary Add-on"
echo "            -> $ROOT/browser/firefox/manifest.json"
echo "            (a temporary add-on disappears when Firefox restarts;"
echo "             load it again after a restart)"
echo
echo "Test: download any http(s) file in the browser while the ZDM"
echo "toolbar badge says ON — the browser download is cancelled and"
echo "ZDM takes over."
