#!/usr/bin/env bash
# ZDM browser integration installer — run on the target machine.
# Registers browser/zdm-native-host.py as the "com.zdm.native" host
# for every Chromium-family browser (Chromium, Chrome, Edge, Brave,
# Opera, Vivaldi) and for Firefox. No sudo needed.
#
# A browser is registered when its binary is on PATH or its config
# directory exists. The pinned Chromium extension ID is the same for
# every Chromium-family browser (one ID, one allowed_origins), so the
# same unpacked extension works in all of them.
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

REGISTERED=()
SKIPPED=()

# register_chromium <label> <config-dir> <binary>...
# Writes the native host manifest under <config-dir>/NativeMessagingHosts
# when the browser binary is on PATH or the config dir exists.
register_chromium() {
    local label="$1" config_dir="$2"; shift 2
    local found=0
    local b
    for b in "$@"; do
        if command -v "$b" >/dev/null 2>&1; then found=1; break; fi
    done
    if [ "$found" -eq 0 ] && [ ! -d "$config_dir" ]; then
        SKIPPED+=("$label (not installed)")
        return 0
    fi
    local dest="$config_dir/NativeMessagingHosts"
    mkdir -p "$dest"
    sed -e "s|__ZDM_HOST_PATH__|$HOST|g" \
        -e "s|mojdpgikbnejopblfjfphlkdnogkkimh|$CHROMIUM_EXT_ID|g" \
        "$ROOT/browser/chromium/com.zdm.native.json" \
        > "$dest/com.zdm.native.json"
    python3 -m json.tool "$dest/com.zdm.native.json" > /dev/null
    REGISTERED+=("$label ($dest/com.zdm.native.json)")
}

register_chromium "Chromium" "$HOME/.config/chromium" chromium chromium-browser
register_chromium "Chrome" "$HOME/.config/google-chrome" google-chrome google-chrome-stable
register_chromium "Edge" "$HOME/.config/microsoft-edge" microsoft-edge microsoft-edge-stable
register_chromium "Brave" "$HOME/.config/BraveSoftware/Brave-Browser" brave-browser brave
register_chromium "Opera" "$HOME/.config/opera" opera
register_chromium "Vivaldi" "$HOME/.config/vivaldi" vivaldi vivaldi-stable

# Firefox: allowed_extensions must contain the gecko id from
# browser/firefox/manifest.json (zdm@example.local).
if command -v firefox >/dev/null 2>&1 || [ -d "$HOME/.mozilla" ]; then
    mkdir -p "$HOME/.mozilla/native-messaging-hosts"
    sed -e "s|__ZDM_HOST_PATH__|$HOST|g" \
        "$ROOT/browser/firefox/com.zdm.native.json" \
        > "$HOME/.mozilla/native-messaging-hosts/com.zdm.native.json"
    python3 -m json.tool \
        "$HOME/.mozilla/native-messaging-hosts/com.zdm.native.json" > /dev/null
    REGISTERED+=("Firefox ($HOME/.mozilla/native-messaging-hosts/com.zdm.native.json)")
else
    SKIPPED+=("Firefox (not installed)")
fi

python3 -m py_compile "$HOST"

echo "ZDM native host registered:"
echo "  host:     $HOST"
for r in "${REGISTERED[@]}"; do echo "  + $r"; done
for s in "${SKIPPED[@]}"; do echo "  - $s"; done
echo "  chromium extension id: $CHROMIUM_EXT_ID"
echo
echo "Still manual — browsers require a click in the UI:"
echo "  Chromium-family (Chromium, Chrome, Edge, Brave, Opera, Vivaldi):"
echo "    chrome://extensions -> Developer mode ON -> Load unpacked"
echo "    -> $ROOT/browser/chromium"
echo "    (same pinned extension id in every Chromium-family browser)"
echo "  Firefox:  about:debugging#/runtime/this-firefox -> Load Temporary Add-on"
echo "            -> $ROOT/browser/firefox/manifest.json"
echo "            (a temporary add-on disappears when Firefox restarts;"
echo "             for a permanent install, see browser/README.md:"
echo "             Firefox needs a signed .xpi from addons.mozilla.org)"
echo
echo "Test: download any http(s) file in the browser while the ZDM"
echo "toolbar badge says ON — the browser download is cancelled and"
echo "ZDM takes over."
