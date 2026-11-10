#!/usr/bin/env bash
# Laptop E2E for "Auto-resume saat dibuka" (ZDM Tauri).
#
# Proves, against the real app on the laptop:
#   Phase 1 — an interrupted download (ZDM killed -9 mid-download)
#              auto-resumes from its saved offset on reopen and
#              finishes with a matching SHA-256. The range server's
#              log is the proof: no full GET after the restart, only
#              Range requests starting above zero.
#   Phase 2 — a download in the exact persisted state a manual pause
#              writes (DB status=paused, paused_by_schedule=0)
#              stays paused after a restart, then resumes when its
#              state is flipped back to interrupted.
#   Phase 3 — with resume_on_startup=false nothing auto-resumes;
#              flipping the toggle back ON resumes it.
#
# Isolation: everything runs in $TESTDIR with its own download_dir,
# its own downloads.db and its own queue.json. The user's real
# ~/Downloads/ZDM/ is never touched. settings.json is backed up
# byte-for-byte and restored on exit (even on failure), and the
# user's ZDM is relaunched at the end if it was running.
#
# Honest limit: the manual pause in phase 2 is produced by writing
# the exact row state pause_task persists (status=paused,
# paused_by_schedule=0 in downloads.db + sidecar) — there is no
# headless way to click Pause in the GUI. What is observed live is
# the app's decision on that state after a restart.
#
# Usage (on the laptop, from ~/workspace/zdm-tauri):
#   bash .taste-skill/e2e-autoresume/run_e2e.sh
set -u
cd ~/workspace/zdm-tauri || exit 1

TESTDIR="$HOME/zdm-e2e-autoresume"
DL="$TESTDIR/dl"
SRVDIR="$TESTDIR/server"
SETTINGS="$HOME/.config/zdm/settings.json"
BACKUP="$TESTDIR/settings.json.bak"
SERVER_LOG="$TESTDIR/server.log"
SUMMARY="$TESTDIR/summary.txt"
ZDM_RUNNING_AT_START=0
PORT=8765

log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$SUMMARY"; }
fail() { log "FAIL: $*"; exit 1; }

# --- helpers ---------------------------------------------------------------
db_row() { # db_row <filename> -> status|downloaded|total|paused_by_schedule
    python3 - "$DL/downloads.db" "$1" <<'EOF'
import sqlite3, sys
db, name = sys.argv[1], sys.argv[2]
try:
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=5)
    row = con.execute(
        "SELECT status, downloaded, total, paused_by_schedule FROM downloads WHERE filename=?",
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
row = con.execute("SELECT dest_path FROM downloads WHERE filename=?", (name,)).fetchone()
print(row[0] if row else "")
EOF
}

wait_downloaded() { # wait_downloaded <file> <bytes> <timeout_s>
    local end=$(( $(date +%s) + $3 ))
    while [ "$(date +%s)" -lt "$end" ]; do
        local row; row=$(db_row "$1")
        local d; d=$(echo "$row" | cut -d'|' -f2)
        case "$d" in ''|*[!0-9]*) ;; *) [ "$d" -ge "$2" ] && { echo "$d"; return 0; } ;; esac
        sleep 0.2
    done
    echo "TIMEOUT row=$(db_row "$1")"; return 1
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

# check_server_resume <file> <cutoff_iso> — every GET for <file> after
# <cutoff_iso> must carry Range starting > 0; at least one must exist.
check_server_resume() {
    python3 - "$SERVER_LOG" "$1" "$2" <<'EOF'
import sys
log, name, cutoff = sys.argv[1], sys.argv[2], sys.argv[3]
after = [l for l in open(log) if l.split(" ", 1)[0] > cutoff and f"GET /{name} " in l]
if not after:
    print(f"no GET for {name} after restart"); sys.exit(1)
bad = []
for l in after:
    if "Range=bytes=" not in l:
        bad.append(l)  # full GET — a from-zero restart
        continue
    start = l.split("Range=bytes=", 1)[1].split("-")[0]
    if not start.isdigit() or int(start) == 0:
        bad.append(l)
if bad:
    print(f"full/zero-offset GET after restart: {bad[0].strip()}"); sys.exit(1)
first = after[0].split("Range=")[1].split(" ")[0]
print(f"{len(after)} ranged GETs after restart, first {first}")
EOF
}

patch_settings() { # patch_settings <resume true|false>
    python3 - "$SETTINGS" "$1" "$DL" <<'EOF'
import json, sys
path, resume, dl = sys.argv[1], sys.argv[2] == "true", sys.argv[3]
try:
    s = json.load(open(path))
except (FileNotFoundError, json.JSONDecodeError):
    s = {}
s["download_dir"] = dl
s["ask_before_download"] = False
s["resume_on_startup"] = resume
s["desktop_notifications"] = False
s["speed_limit_kbps"] = 2000.0
s.setdefault("schedule", {})["enabled"] = False
json.dump(s, open(path, "w"), indent=2)
EOF
}

set_db_status() { # set_db_status <file> <status> <paused_by_schedule 0|1>
    python3 - "$DL/downloads.db" "$1" "$2" "$3" <<'EOF'
import sqlite3, sys
db, name, status, flag = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
con = sqlite3.connect(db, timeout=5)
con.execute("UPDATE downloads SET status=?, paused_by_schedule=? WHERE filename=?",
            (status, flag, name))
con.commit()
EOF
    # Keep the sidecar consistent: restore reads the DB status, but a
    # real pause writes the same status into the sidecar too.
    python3 - "$DL/$1.download.json" "$2" <<'EOF'
import json, sys
path, status = sys.argv[1], sys.argv[2]
try:
    s = json.load(open(path))
except (FileNotFoundError, json.JSONDecodeError):
    sys.exit(0)
s["status"] = status
json.dump(s, open(path, "w"))
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

cleanup() {
    pkill -9 -x zdm 2>/dev/null || true
    [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null
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
# Drop any state from a previous run so every phase starts from a fresh DB.
rm -rf "$TESTDIR"
mkdir -p "$TESTDIR" "$DL" "$SRVDIR"
: > "$SUMMARY"
log "E2E auto-resume — $(date)"
if pgrep -x zdm >/dev/null; then ZDM_RUNNING_AT_START=1; log "user ZDM is running (will relaunch at end)"; fi
# Abort if the user has any live download in the real dir.
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

# Test files: deterministic content, known hashes.
python3 -c "
import random
for name, size in [('big.bin', 30*1024*1024), ('big2.bin', 10*1024*1024), ('big3.bin', 10*1024*1024)]:
    r = random.Random(name)
    with open('$SRVDIR/' + name, 'wb') as f:
        left = size
        while left:
            chunk = bytes(r.getrandbits(8) for _ in range(min(left, 1 << 20)))
            f.write(chunk); left -= len(chunk)
"
for f in big.bin big2.bin big3.bin; do
    log "$f sha256=$(sha256sum "$SRVDIR/$f" | cut -d' ' -f1)"
done

cp "$SETTINGS" "$BACKUP" && log "settings.json backed up"
zdm_kill9

python3 .taste-skill/e2e-autoresume/range_server.py --dir "$SRVDIR" --port $PORT --log "$SERVER_LOG" &
SERVER_PID=$!
sleep 1
curl -s -o /dev/null "http://127.0.0.1:$PORT/big.bin" -r 0-99 || fail "range server not responding"

# --- Phase 1: interrupted -> auto-resume ----------------------------------
log "== Phase 1: kill mid-download, reopen, must resume from offset =="
patch_settings true
zdm_start
echo "[\"http://127.0.0.1:$PORT/big.bin\"]" > "$DL/queue.json"
FROZEN=$(wait_downloaded big.bin $((10*1024*1024)) 90) || fail "big.bin never reached 10 MB: $FROZEN"
log "big.bin at $FROZEN bytes, killing ZDM -9"
zdm_kill9
ROW=$(db_row big.bin)
log "after kill: $ROW (want status=downloading)"
[ "$(echo "$ROW" | cut -d'|' -f1)" = "downloading" ] || fail "DB row did not stay 'downloading' after kill: $ROW"
CUTOFF=$(date +%Y-%m-%dT%H:%M:%S)
zdm_start
wait_status big.bin completed 120 || fail "big.bin did not complete after reopen: $(db_row big.bin)"
GOT=$(sha256sum "$(db_dest big.bin)" | cut -d' ' -f1)
WANT=$(sha256sum "$SRVDIR/big.bin" | cut -d' ' -f1)
[ "$GOT" = "$WANT" ] || fail "big.bin hash mismatch after resume"
log "big.bin completed, sha256 match"
check_server_resume big.bin "$CUTOFF" | tee -a "$SUMMARY" || fail "server log shows a from-zero restart"
log "Phase 1 PASS: resumed from ~$FROZEN B, completed, hash match"

# --- Phase 2: manual pause stays paused -----------------------------------
log "== Phase 2: manual-pause state must stay paused after restart =="
echo "[\"http://127.0.0.1:$PORT/big2.bin\"]" > "$DL/queue.json"
FROZEN2=$(wait_downloaded big2.bin $((4*1024*1024)) 60) || fail "big2.bin never reached 4 MB"
log "big2.bin at $FROZEN2 bytes, killing ZDM -9"
zdm_kill9
set_db_status big2.bin paused 0
log "big2.bin seeded to manual-pause state: $(db_row big2.bin)"
CUTOFF2=$(date +%Y-%m-%dT%H:%M:%S)
zdm_start
sleep 10
ROW2=$(db_row big2.bin)
[ "$(echo "$ROW2" | cut -d'|' -f1)" = "paused" ] || fail "manual-paused big2.bin did not stay paused: $ROW2"
if python3 -c "
import sys
cutoff = '$CUTOFF2'
hits = [l for l in open('$SERVER_LOG') if l.split(' ', 1)[0] > cutoff and ' GET /big2.bin ' in l]
sys.exit(0 if hits else 1)
"; then fail "server saw GETs for paused big2.bin after restart"; fi
log "big2.bin stayed paused for 10 s, no server requests — Phase 2a PASS"
# Flip it back to interrupted: it must resume and finish.
zdm_kill9
set_db_status big2.bin downloading 0
zdm_start
wait_status big2.bin completed 60 || fail "big2.bin did not complete after flip-back: $(db_row big2.bin)"
[ "$(sha256sum "$(db_dest big2.bin)" | cut -d' ' -f1)" = "$(sha256sum "$SRVDIR/big2.bin" | cut -d' ' -f1)" ] \
    || fail "big2.bin hash mismatch"
log "Phase 2b PASS: flipped back to interrupted, resumed, hash match"

# --- Phase 3: toggle OFF disables auto-resume ----------------------------
log "== Phase 3: resume_on_startup=false must disable auto-resume =="
echo "[\"http://127.0.0.1:$PORT/big3.bin\"]" > "$DL/queue.json"
FROZEN3=$(wait_downloaded big3.bin $((4*1024*1024)) 60) || fail "big3.bin never reached 4 MB"
log "big3.bin at $FROZEN3 bytes, killing ZDM -9"
zdm_kill9
patch_settings false
CUTOFF3=$(date +%Y-%m-%dT%H:%M:%S)
zdm_start
sleep 10
# The DB row stays "downloading" by design: restore converts it to
# paused in memory only, so the row keeps signalling "interrupted"
# across restarts (flipping the toggle back ON later still resumes
# it). The true observables are frozen bytes + no server requests.
AFTER3=$(db_row big3.bin | cut -d'|' -f2)
[ "$AFTER3" = "$FROZEN3" ] || fail "big3.bin progressed with toggle OFF: $FROZEN3 -> $AFTER3"
if python3 -c "
import sys
cutoff = '$CUTOFF3'
hits = [l for l in open('$SERVER_LOG') if l.split(' ', 1)[0] > cutoff and ' GET /big3.bin ' in l]
sys.exit(0 if hits else 1)
"; then fail "server saw GETs for big3.bin with toggle OFF"; fi
log "big3.bin frozen at $FROZEN3 B with toggle OFF, no server requests — Phase 3a PASS"
zdm_kill9
patch_settings true
zdm_start
wait_status big3.bin completed 60 || fail "big3.bin did not complete after toggle back ON: $(db_row big3.bin)"
[ "$(sha256sum "$(db_dest big3.bin)" | cut -d' ' -f1)" = "$(sha256sum "$SRVDIR/big3.bin" | cut -d' ' -f1)" ] \
    || fail "big3.bin hash mismatch"
log "Phase 3b PASS: toggle back ON, resumed, hash match"

log "ALL PHASES PASS"
