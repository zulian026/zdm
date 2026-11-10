#!/usr/bin/env bash
# Laptop E2E for the "Update checker" (ZDM Tauri).
#
# Proves, against the real app on the laptop, using a local manifest
# server (manifest_server.py, stdlib only):
#   Phase 1 — newer version (0.2.0): update_state.json shows
#              update_available with version 0.2.0 + the manifest
#              download_url + sha256. That state is exactly what the
#              offer banner renders (banner pixels were reviewed in
#              headless Chrome at 1440/768/390).
#   Phase 2 — same version (0.1.0): up_to_date, no offer.
#   Phase 3 — broken manifest ("not json{"): invalid_manifest, no offer.
#   Phase 4 — dead source (closed port): unreachable, no offer.
#   Phase 5 — empty manifest URL: no check runs at all (the auto-check
#              returns early; update_state.json is not even written).
#   Phase 6 — the manifest's asset URL downloads through the real
#              engine (queued via queue.json, ask OFF): completed file
#              with the manifest's SHA-256. This proves the URL the
#              offer carries is downloadable by the app.
#
# Honest limit of this script: the Download button itself is a GUI
# click and cannot be driven headlessly on the laptop (Wayland, no
# xdotool/wmctrl). What the click does — add_download(url, sha256)
# — is proven by the integration test
# update_asset_wrong_sha_fails_but_keeps_file (manifest SHA passed as
# the expected hash; a tampered asset fails like a checksum mismatch,
# file kept). The banner's rendering of the offer state is proven by
# the headless-Chrome screenshots reviewed in this round.
#
# Isolation: everything runs in $TESTDIR with its own download_dir,
# its own downloads.db and its own queue.json. The user's real
# ~/Downloads/ZDM/ is never touched. settings.json and
# update_state.json are backed up byte-for-byte and restored on exit
# (even on failure), and the user's ZDM is relaunched at the end if
# it was running.
#
# Usage (on the laptop, from ~/workspace/zdm-tauri):
#   bash .taste-skill/e2e-update/run_e2e.sh
set -u
cd ~/workspace/zdm-tauri || exit 1

TESTDIR="$HOME/zdm-e2e-update"
DL="$TESTDIR/dl"
SETTINGS="$HOME/.config/zdm/settings.json"
USTATE="$HOME/.config/zdm/update_state.json"
BACKUP="$TESTDIR/settings.json.bak"
UBACKUP="$TESTDIR/update_state.json.bak"
SUMMARY="$TESTDIR/summary.txt"
ZDM_RUNNING_AT_START=0
SERVER_PID=""

log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$SUMMARY"; }
fail() { log "FAIL: $*"; exit 1; }

# --- helpers ---------------------------------------------------------------
patch_settings() { # patch_settings <manifest_url> — "" means empty
    python3 - "$SETTINGS" "$1" "$DL" <<'EOF'
import json, sys
path, url, dl = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    s = json.load(open(path))
except (FileNotFoundError, json.JSONDecodeError):
    s = {}
s["download_dir"] = dl
s["ask_before_download"] = False
s["clamav_scan"] = False
s["resume_on_startup"] = False
s["desktop_notifications"] = False
s["update_manifest_url"] = url
s.setdefault("schedule", {})["enabled"] = False
json.dump(s, open(path, "w"), indent=2)
EOF
}

state_json() { # state_json -> raw update_state.json or MISSING
    [ -f "$USTATE" ] && cat "$USTATE" || echo MISSING
}

state_field() { # state_field <field> — top-level field of update_state.json
    python3 - "$USTATE" "$1" <<'EOF'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    print("MISSING"); sys.exit(0)
c = d.get("last_check") or {}
f = sys.argv[2]
if f == "state":
    print(c.get("state", ""))
elif f == "version":
    m = c.get("manifest") or {}
    print(m.get("version", ""))
elif f == "download_url":
    m = c.get("manifest") or {}
    print(m.get("download_url", ""))
elif f == "sha256":
    m = c.get("manifest") or {}
    print(m.get("sha256", ""))
elif f == "source_url":
    print(c.get("source_url", ""))
elif f == "has_manifest":
    print("yes" if c.get("manifest") else "no")
EOF
}

wait_state() { # wait_state <want> <timeout_s> — last_check.state == want
    local end=$(( $(date +%s) + $2 ))
    while [ "$(date +%s)" -lt "$end" ]; do
        [ "$(state_field state)" = "$1" ] && return 0
        sleep 1
    done
    return 1
}

db_row() { # db_row <filename> -> status|verify_state|downloaded
    python3 - "$DL/downloads.db" "$1" <<'EOF'
import sqlite3, sys
db, name = sys.argv[1], sys.argv[2]
try:
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=5)
    row = con.execute(
        "SELECT status, verify_state, downloaded FROM downloads WHERE filename=? ORDER BY id DESC LIMIT 1",
        (name,)).fetchone()
    print("|".join("" if v is None else str(v) for v in row) if row else "MISSING")
except Exception as e:
    print(f"ERR {e}")
EOF
}

wait_db_status() { # wait_db_status <file> <status> <timeout_s>
    local end=$(( $(date +%s) + $3 ))
    while [ "$(date +%s)" -lt "$end" ]; do
        local row; row=$(db_row "$1")
        [ "$(echo "$row" | cut -d'|' -f1)" = "$2" ] && return 0
        sleep 0.5
    done
    return 1
}

db_dest() { # db_dest <filename> -> dest_path (files land in category subfolders)
    python3 - "$DL/downloads.db" "$1" <<'EOF'
import sqlite3, sys
db, name = sys.argv[1], sys.argv[2]
con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=5)
row = con.execute("SELECT dest_path FROM downloads WHERE filename=? ORDER BY id DESC LIMIT 1", (name,)).fetchone()
print(row[0] if row else "")
EOF
}

zdm_start() {
    DISPLAY=:1 ./release/zdm > "$TESTDIR/zdm-$(date +%H%M%S).log" 2>&1 &
    echo $! > "$TESTDIR/zdm.pid"
    sleep 4
    pgrep -x zdm >/dev/null || fail "zdm did not start"
}

zdm_kill9() {
    pkill -9 -x zdm 2>/dev/null || true
    for _ in $(seq 1 20); do pgrep -x zdm >/dev/null || return 0; sleep 0.5; done
    fail "zdm would not die"
}

start_server() { # start_server [manifest_server.py args...] — sets MANIFEST_URL
    python3 .taste-skill/e2e-update/manifest_server.py "$@" > "$TESTDIR/server.log" 2> "$TESTDIR/server.err" &
    SERVER_PID=$!
    local port=""
    for _ in $(seq 1 20); do
        port=$(grep -m1 '^PORT=' "$TESTDIR/server.log" 2>/dev/null | cut -d= -f2)
        [ -n "$port" ] && break
        sleep 0.5
    done
    [ -n "$port" ] || fail "manifest server did not start (see $TESTDIR/server.err)"
    MANIFEST_URL="http://127.0.0.1:$port/version.json"
    log "manifest server on port $port ($*)"
}

stop_server() {
    [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
    SERVER_PID=""
}

cleanup() {
    pkill -9 -x zdm 2>/dev/null || true
    stop_server
    if [ -f "$BACKUP" ]; then
        cp "$BACKUP" "$SETTINGS" && log "settings.json restored byte-for-byte"
    fi
    if [ -f "$UBACKUP" ]; then
        cp "$UBACKUP" "$USTATE" && log "update_state.json restored byte-for-byte"
    else
        rm -f "$USTATE"
        log "update_state.json removed (did not exist before the run)"
    fi
    if [ "$ZDM_RUNNING_AT_START" = 1 ]; then
        DISPLAY=:1 ./release/zdm > "$TESTDIR/zdm-relaunch.log" 2>&1 &
        sleep 3
        pgrep -x zdm >/dev/null && log "user ZDM relaunched with restored settings" \
            || log "WARNING: user ZDM did not relaunch — start it by hand"
    fi
    log "test dir kept at $TESTDIR (remove by hand after reading summary.txt)"
}
trap cleanup EXIT

# --- preconditions ---------------------------------------------------------
rm -rf "$TESTDIR"
mkdir -p "$TESTDIR" "$DL"
: > "$SUMMARY"
log "E2E update checker — $(date)"
[ -f "$SETTINGS" ] && cp "$SETTINGS" "$BACKUP" && log "settings.json backed up"
[ -f "$USTATE" ] && cp "$USTATE" "$UBACKUP" && log "update_state.json backed up"
if pgrep -x zdm >/dev/null; then ZDM_RUNNING_AT_START=1; log "user ZDM is running (will relaunch at end)"; fi
if ls ~/Downloads/ZDM/*.part >/dev/null 2>&1; then
    fail "user has active .part files in ~/Downloads/ZDM — aborting, nothing touched"
fi
log "preconditions ok"

# --- phase 1: newer version offers an update -------------------------------
rm -f "$USTATE"
start_server --version 0.2.0 --sha correct
patch_settings "$MANIFEST_URL"
zdm_start
wait_state update_available 40 || fail "phase 1: no update_available state (got: $(state_json))"
[ "$(state_field version)" = "0.2.0" ] || fail "phase 1: offered version is not 0.2.0 (got: $(state_field version))"
[ "$(state_field source_url)" = "$MANIFEST_URL" ] || fail "phase 1: stored check is not for this manifest URL (got: $(state_field source_url))"
[ "$(state_field download_url)" = "${MANIFEST_URL%/version.json}/asset.bin" ] || fail "phase 1: offered download_url is not this server's asset (got: $(state_field download_url))"
[ -n "$(state_field sha256)" ] || fail "phase 1: offered manifest has no sha256"
log "phase 1 PASS: update_available for 0.2.0, download_url + sha256 present"
zdm_kill9
stop_server

# --- phase 2: same version is up to date -----------------------------------
rm -f "$USTATE"
start_server --version 0.1.0 --sha correct
patch_settings "$MANIFEST_URL"
zdm_start
wait_state up_to_date 40 || fail "phase 2: no up_to_date state (got: $(state_json))"
log "phase 2 PASS: same version (0.1.0) is up_to_date, no offer"
zdm_kill9
stop_server

# --- phase 3: broken manifest is invalid, no offer -------------------------
rm -f "$USTATE"
start_server --manifest-body 'not json{'
patch_settings "$MANIFEST_URL"
zdm_start
wait_state invalid_manifest 40 || fail "phase 3: no invalid_manifest state (got: $(state_json))"
[ "$(state_field has_manifest)" = "no" ] || fail "phase 3: broken manifest produced a manifest"
log "phase 3 PASS: broken manifest is invalid_manifest, no offer"
zdm_kill9
stop_server

# --- phase 4: dead source is unreachable, no offer -------------------------
rm -f "$USTATE"
DEAD_PORT=$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()")
patch_settings "http://127.0.0.1:$DEAD_PORT/version.json"
zdm_start
wait_state unreachable 40 || fail "phase 4: no unreachable state (got: $(state_json))"
log "phase 4 PASS: dead source is unreachable, no offer"
zdm_kill9

# --- phase 5: empty URL means no check runs --------------------------------
rm -f "$USTATE"
patch_settings ""
zdm_start
sleep 15
[ -f "$USTATE" ] && fail "phase 5: update_state.json was written with an empty manifest URL: $(state_json)"
log "phase 5 PASS: empty URL — no check ran, no state written, no offer"
zdm_kill9

# --- phase 6: the manifest's asset downloads through the engine ------------
rm -f "$USTATE"
start_server --version 0.2.0 --sha correct
patch_settings "$MANIFEST_URL"
zdm_start
wait_state update_available 40 || fail "phase 6: no update_available state (got: $(state_json))"
ASSET_SHA=$(state_field sha256)
ASSET_URL=$(state_field download_url)
[ -n "$ASSET_SHA" ] && [ -n "$ASSET_URL" ] || fail "phase 6: offer has no asset URL/sha"
printf '["%s"]' "$ASSET_URL" > "$DL/queue.json"
wait_db_status "asset.bin" completed 60 || fail "phase 6: asset.bin did not complete (row: $(db_row asset.bin))"
DEST=$(db_dest "asset.bin")
[ -f "$DEST" ] || fail "phase 6: completed asset.bin not found at DB dest_path: $DEST"
ACTUAL_SHA=$(sha256sum "$DEST" | cut -d' ' -f1)
[ "$ACTUAL_SHA" = "$ASSET_SHA" ] || fail "phase 6: downloaded asset sha $ACTUAL_SHA != manifest sha $ASSET_SHA"
log "phase 6 PASS: asset.bin downloaded via the engine, sha256 matches the manifest"
zdm_kill9
stop_server

log "ALL PHASES PASS"
