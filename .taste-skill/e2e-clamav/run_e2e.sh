#!/usr/bin/env bash
# Laptop E2E for "ClamAV scan" (ZDM Tauri).
#
# Proves, against the real app on the laptop:
#   Phase 1 — no scanner installed: a clean download completes and
#              stays completed with scan_state "unavailable" (a
#              missing scanner never fails a download).
#   Phase 2 — fake clamscan (ZDM_CLAMSCAN): a clean download scans
#              "clean" after completion.
#   Phase 3 — fake clamscan: the EICAR test file fails with
#              "Virus terdeteksi: Eicar-Test-Signature", the file is
#              kept on disk, and a "Virus terdeteksi" desktop
#              notification is captured on D-Bus when the window is
#              unfocused (best-effort: the notification title is
#              unit-tested; the capture depends on focus state under
#              the test display).
#   Phase 4 — clamav_scan=false with a scanner present: the download
#              completes and no scan runs (scan_state "unavailable").
#   Phase 5 — real clamscan, when one is available (system clamscan,
#              or a user-space install at ~/opt/clamav, see
#              install-clamav-userspace.sh): clean -> clean,
#              EICAR -> found. Skipped honestly when no real scanner
#              exists.
#
# The manual re-scan flip-back (virus-failed -> completed after the
# file on disk is cleaned) is proven in the Rust integration test
# (manager.scan_download); there is no headless way to click Scan in
# the GUI, so the laptop script does not fake it.
#
# Isolation: everything runs in $TESTDIR with its own download_dir,
# its own downloads.db and its own queue.json. The user's real
# ~/Downloads/ZDM/ is never touched. settings.json is backed up
# byte-for-byte and restored on exit (even on failure), and the
# user's ZDM is relaunched at the end if it was running.
#
# Usage (on the laptop, from ~/workspace/zdm-tauri):
#   bash .taste-skill/e2e-clamav/run_e2e.sh
set -u
cd ~/workspace/zdm-tauri || exit 1

TESTDIR="$HOME/zdm-e2e-clamav"
DL="$TESTDIR/dl"
SRVDIR="$TESTDIR/server"
FAKEBIN="$TESTDIR/fakebin"
SETTINGS="$HOME/.config/zdm/settings.json"
BACKUP="$TESTDIR/settings.json.bak"
SERVER_LOG="$TESTDIR/server.log"
DBUS_LOG="$TESTDIR/dbus.log"
SUMMARY="$TESTDIR/summary.txt"
ZDM_RUNNING_AT_START=0
PORT=8766

log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$SUMMARY"; }
fail() { log "FAIL: $*"; exit 1; }

# --- helpers ---------------------------------------------------------------
db_row() { # db_row <filename> -> status|scan_state|threat_name|downloaded|total
    python3 - "$DL/downloads.db" "$1" <<'EOF'
import sqlite3, sys
db, name = sys.argv[1], sys.argv[2]
try:
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=5)
    row = con.execute(
        "SELECT status, scan_state, threat_name, downloaded, total FROM downloads WHERE filename=? ORDER BY id DESC LIMIT 1",
        (name,)).fetchone()
    print("|".join("" if v is None else str(v) for v in row) if row else "MISSING")
except Exception as e:
    print(f"ERR {e}")
EOF
}

db_dest() { # db_dest <filename> -> dest_path from the DB row
    python3 - "$DL/downloads.db" "$1" <<'EOF'
import sqlite3, sys
db, name = sys.argv[1], sys.argv[2]
con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=5)
row = con.execute("SELECT dest_path FROM downloads WHERE filename=? ORDER BY id DESC LIMIT 1", (name,)).fetchone()
print(row[0] if row else "")
EOF
}

wait_status() { # wait_status <file> <status> <timeout_s>
    local end=$(( $(date +%s) + $3 ))
    while [ "$(date +%s)" -lt "$end" ]; do
        local row; row=$(db_row "$1")
        [ "$(echo "$row" | cut -d'|' -f1)" = "$2" ] && return 0
        sleep 0.5
    done
    return 1
}

wait_scan() { # wait_scan <file> <scan_state> <timeout_s>
    local end=$(( $(date +%s) + $3 ))
    while [ "$(date +%s)" -lt "$end" ]; do
        local row; row=$(db_row "$1")
        [ "$(echo "$row" | cut -d'|' -f2)" = "$2" ] && return 0
        sleep 0.5
    done
    return 1
}

patch_settings() { # patch_settings <clamav true|false>
    python3 - "$SETTINGS" "$1" "$DL" <<'EOF'
import json, sys
path, scan, dl = sys.argv[1], sys.argv[2] == "true", sys.argv[3]
try:
    s = json.load(open(path))
except (FileNotFoundError, json.JSONDecodeError):
    s = {}
s["download_dir"] = dl
s["ask_before_download"] = False
s["clamav_scan"] = scan
s["resume_on_startup"] = False
s["desktop_notifications"] = True
s.setdefault("schedule", {})["enabled"] = False
json.dump(s, open(path, "w"), indent=2)
EOF
}

zdm_start() { # zdm_start [extra env assignments as "VAR=value" ...]
    env "$@" DISPLAY=:1 ./release/zdm > "$TESTDIR/zdm-$(date +%H%M%S).log" 2>&1 &
    echo $! > "$TESTDIR/zdm.pid"
    sleep 4
    pgrep -x zdm >/dev/null || fail "zdm did not start"
}

zdm_kill9() {
    pkill -9 -x zdm 2>/dev/null || true
    for _ in $(seq 1 20); do pgrep -x zdm >/dev/null || return 0; sleep 0.5; done
    fail "zdm would not die"
}

cleanup() {
    pkill -9 -x zdm 2>/dev/null || true
    [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null
    [ -n "${DBUS_PID:-}" ] && kill "$DBUS_PID" 2>/dev/null
    if [ -f "$BACKUP" ]; then
        cp "$BACKUP" "$SETTINGS" && log "settings.json restored byte-for-byte"
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
mkdir -p "$TESTDIR" "$DL" "$SRVDIR" "$FAKEBIN"
: > "$SUMMARY"
log "E2E ClamAV scan — $(date)"
if pgrep -x zdm >/dev/null; then ZDM_RUNNING_AT_START=1; log "user ZDM is running (will relaunch at end)"; fi
if ls ~/Downloads/ZDM/*.part >/dev/null 2>&1; then
    fail "user has active .part files in ~/Downloads/ZDM — aborting, nothing touched"
fi
if [ -f ~/Downloads/ZDM/downloads.db ]; then
    LIVE=$(python3 -c "
import sqlite3
con = sqlite3.connect('file:$HOME/Downloads/ZDM/downloads.db?mode=ro', uri=True, timeout=5)
print(con.execute(\"SELECT COUNT(*) FROM downloads WHERE status IN ('downloading','queued')\").fetchone()[0])
" 2>/dev/null || echo "?")
    [ "$LIVE" != "0" ] && [ "$LIVE" != "?" ] && fail "user has $LIVE active/queued downloads — aborting"
fi
log "preconditions ok (no user downloads in flight)"

# Test files: deterministic clean content + the EICAR test string.
python3 -c "
import random
r = random.Random('clean.bin')
with open('$SRVDIR/clean.bin', 'wb') as f:
    left = 5*1024*1024
    while left:
        chunk = bytes(r.getrandbits(8) for _ in range(min(left, 1 << 20)))
        f.write(chunk); left -= len(chunk)
"
printf 'X5O!P%%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*' > "$SRVDIR/eicar.com"
log "clean.bin sha256=$(sha256sum "$SRVDIR/clean.bin" | cut -d' ' -f1)"
log "eicar.com bytes=$(stat -c %s "$SRVDIR/eicar.com") (EICAR is 68)"

# Fake clamscan: exit 1 + FOUND when the file contains the EICAR
# string, exit 0 + OK otherwise; --version answers like the real one.
cat > "$FAKEBIN/clamscan" <<'EOF'
#!/bin/sh
if [ "$1" = "--version" ]; then echo 'ClamAV 9.9.9/1/Fake'; exit 0; fi
file="$2"
if grep -q 'EICAR-STANDARD-ANTIVIRUS-TEST-FILE' "$file" 2>/dev/null; then
echo "$file: Eicar-Test-Signature FOUND"; exit 1; fi
echo "$file: OK"; exit 0
EOF
chmod 755 "$FAKEBIN/clamscan"
"$FAKEBIN/clamscan" --version || fail "fake clamscan does not run"
"$FAKEBIN/clamscan" --no-summary "$SRVDIR/eicar.com"; [ "$?" = 1 ] || fail "fake clamscan did not flag EICAR"

cp "$SETTINGS" "$BACKUP" && log "settings.json backed up"
zdm_kill9

python3 .taste-skill/e2e-autoresume/range_server.py --dir "$SRVDIR" --port $PORT --log "$SERVER_LOG" &
SERVER_PID=$!
sleep 1
curl -s -o /dev/null "http://127.0.0.1:$PORT/clean.bin" -r 0-99 || fail "range server not responding"

# --- Phase 1: no scanner -> unavailable, download stays completed ---------
log "== Phase 1: no scanner installed, clean download must stay completed =="
patch_settings true
zdm_start
echo "[\"http://127.0.0.1:$PORT/clean.bin\"]" > "$DL/queue.json"
wait_status clean.bin completed 60 || fail "clean.bin did not complete: $(db_row clean.bin)"
sleep 3  # a scan, if one ran, settles in well under this
ROW=$(db_row clean.bin)
[ "$(echo "$ROW" | cut -d'|' -f2)" = "unavailable" ] || fail "scan_state is not unavailable with no scanner: $ROW"
log "clean.bin completed + scan unavailable — Phase 1 PASS"
zdm_kill9

# --- Phase 2: fake scanner, clean file -> clean ---------------------------
log "== Phase 2: fake scanner, clean file must scan clean =="
zdm_start "ZDM_CLAMSCAN=$FAKEBIN/clamscan"
echo "[\"http://127.0.0.1:$PORT/clean.bin\"]" > "$DL/queue.json"
wait_status clean.bin completed 60 || fail "clean.bin did not complete (phase 2): $(db_row clean.bin)"
wait_scan clean.bin clean 30 || fail "clean.bin did not scan clean: $(db_row clean.bin)"
log "clean.bin completed + scan clean — Phase 2 PASS"

# --- Phase 3: fake scanner, EICAR -> failed, file kept, notification ------
log "== Phase 3: fake scanner, EICAR must fail with Virus terdeteksi =="
# Capture desktop notifications on the session bus while the window
# is unfocused (the app was started in the background; best-effort —
# see the header note).
rm -f "$DBUS_LOG"
dbus-monitor --session "interface='org.freedesktop.Notifications'" > "$DBUS_LOG" 2>&1 &
DBUS_PID=$!
sleep 1
echo "[\"http://127.0.0.1:$PORT/eicar.com\"]" > "$DL/queue.json"
wait_status eicar.com failed 60 || fail "eicar.com did not fail: $(db_row eicar.com)"
wait_scan eicar.com found 30 || fail "eicar.com scan is not found: $(db_row eicar.com)"
ROW=$(db_row eicar.com)
[ "$(echo "$ROW" | cut -d'|' -f3)" = "Eicar-Test-Signature" ] || fail "threat name wrong: $ROW"
DEST=$(db_dest eicar.com)
[ -f "$DEST" ] || fail "EICAR file was deleted from disk"
[ "$(cat "$DEST")" = "X5O!P%@AP[4\PZX54(P^)7CC)7}\$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!\$H+H*" ] || fail "EICAR file content changed"
log "eicar.com failed + threat Eicar-Test-Signature + file kept — Phase 3a PASS"
sleep 2
kill "$DBUS_PID" 2>/dev/null; DBUS_PID=""
if grep -q "Virus terdeteksi" "$DBUS_LOG"; then
    log "dbus-monitor captured 'Virus terdeteksi' — Phase 3b PASS"
else
    log "WARNING: no 'Virus terdeteksi' in dbus log (focus state under the test display) — Phase 3b NOT OBSERVED, notification title is unit-tested"
fi
zdm_kill9

# --- Phase 4: toggle OFF disables the scan -------------------------------
log "== Phase 4: clamav_scan=false must disable scanning =="
patch_settings false
zdm_start "ZDM_CLAMSCAN=$FAKEBIN/clamscan"
echo "[\"http://127.0.0.1:$PORT/clean.bin\"]" > "$DL/queue.json"
wait_status clean.bin completed 60 || fail "clean.bin did not complete (phase 4): $(db_row clean.bin)"
sleep 3
ROW=$(db_row clean.bin)
[ "$(echo "$ROW" | cut -d'|' -f2)" = "unavailable" ] || fail "scan ran with toggle OFF: $ROW"
log "clean.bin completed + scan unavailable with toggle OFF — Phase 4 PASS"
zdm_kill9

# --- Phase 5: real scanner, when one exists -------------------------------
REAL_SCAN=""
if command -v clamscan >/dev/null 2>&1; then REAL_SCAN="$(command -v clamscan)";
elif [ -x "$HOME/opt/clamav/bin/clamscan" ]; then REAL_SCAN="$HOME/opt/clamav/bin/clamscan"; fi
if [ -n "$REAL_SCAN" ]; then
    log "== Phase 5: real scanner at $REAL_SCAN =="
    "$REAL_SCAN" --version | tee -a "$SUMMARY" || fail "real clamscan --version failed"
    patch_settings true
    zdm_start "ZDM_CLAMSCAN=$REAL_SCAN"
    echo "[\"http://127.0.0.1:$PORT/clean.bin\"]" > "$DL/queue.json"
    wait_status clean.bin completed 60 || fail "clean.bin did not complete (phase 5): $(db_row clean.bin)"
    wait_scan clean.bin clean 60 || fail "clean.bin did not scan clean with real clamscan: $(db_row clean.bin)"
    log "clean.bin completed + real scan clean — Phase 5a PASS"
    echo "[\"http://127.0.0.1:$PORT/eicar.com\"]" > "$DL/queue.json"
    wait_status eicar.com failed 60 || fail "eicar.com did not fail with real clamscan: $(db_row eicar.com)"
    wait_scan eicar.com found 60 || fail "eicar.com scan is not found with real clamscan: $(db_row eicar.com)"
    log "eicar.com failed + real scan found — Phase 5b PASS"
    zdm_kill9
else
    log "Phase 5 SKIPPED: no real clamscan on the laptop (install with: sudo pacman -S clamav && sudo freshclam, or bash .taste-skill/e2e-clamav/install-clamav-userspace.sh)"
fi

log "ALL PHASES PASS (phase 3b/5 as noted above)"
