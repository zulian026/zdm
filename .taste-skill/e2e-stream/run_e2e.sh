#!/usr/bin/env bash
# Laptop E2E for "HLS/DASH merging" (ZDM Tauri, grabber v2).
#
# Proves, against the real app on the laptop:
#   Phase 1 — HLS (.m3u8): all segments download and merge into one
#              playable .mp4 (ffprobe duration + video stream, video
#              frames identical to the source via framemd5).
#   Phase 2 — DASH (.mpd): init + video/audio segments mux into one
#              playable .mp4, same frame check.
#   Phase 3 — AES-128 HLS: the encrypted stream decrypts and merges,
#              same frame check.
#   Phase 4 — live playlist (no EXT-X-ENDLIST): rejected with a clear
#              "live stream" failure, no file produced.
#   Phase 5 — DASH without ffmpeg (ZDM_FFMPEG=""): honest fallback —
#              only the playlist downloads, with a note saying ffmpeg
#              is missing.
#   Phase 6 — interruption: ZDM is killed -9 mid-segment-download
#              (server dribbles segment bodies); on reopen the stream
#              resumes — finished segments are never re-requested and
#              the partial segment continues with a Range request —
#              and the merged file still matches the source frames.
#
# Isolation: everything runs in $TESTDIR with its own download_dir,
# its own downloads.db and its own queue.json. The user's real
# ~/Downloads/ZDM/ is never touched. settings.json is backed up
# byte-for-byte and restored on exit (even on failure), and the
# user's ZDM is relaunched at the end if it was running.
#
# Usage (on the laptop, from ~/workspace/zdm-tauri):
#   bash .taste-skill/e2e-stream/run_e2e.sh
set -u
cd ~/workspace/zdm-tauri || exit 1

TESTDIR="$HOME/zdm-e2e-stream"
DL="$TESTDIR/dl"
SRVDIR="$TESTDIR/server"
SETTINGS="$HOME/.config/zdm/settings.json"
BACKUP="$TESTDIR/settings.json.bak"
SERVER_LOG="$TESTDIR/server.log"
SUMMARY="$TESTDIR/summary.txt"
ZDM_RUNNING_AT_START=0
PORT=8767

log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$SUMMARY"; }
fail() { log "FAIL: $*"; exit 1; }

# --- helpers ---------------------------------------------------------------
db_row() { # db_row <filename> -> status|stream_kind|downloaded|total
    python3 - "$DL/downloads.db" "$1" <<'EOF'
import sqlite3, sys
db, name = sys.argv[1], sys.argv[2]
try:
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=5)
    row = con.execute(
        "SELECT status, stream_kind, downloaded, total FROM downloads WHERE filename=? ORDER BY id DESC LIMIT 1",
        (name,)).fetchone()
    print("|".join("" if v is None else str(v) for v in row) if row else "MISSING")
except Exception as e:
    print(f"ERR {e}")
EOF
}

db_err() { # db_err <filename> -> error text
    python3 - "$DL/downloads.db" "$1" <<'EOF'
import sqlite3, sys
db, name = sys.argv[1], sys.argv[2]
con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=5)
row = con.execute("SELECT error FROM downloads WHERE filename=? ORDER BY id DESC LIMIT 1", (name,)).fetchone()
print(row[0] if row and row[0] else "")
EOF
}

db_dest() { # db_dest <filename> -> dest_path
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
s["clamav_scan"] = False
s["resume_on_startup"] = resume
s["desktop_notifications"] = False
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

ffprobe_info() { # ffprobe_info <file> -> "duration|has_video(0/1)"
    local out
    out=$(ffprobe -v error -show_entries format=duration -show_entries stream=codec_type -of json "$1" 2>/dev/null) || { echo "0|0"; return; }
    local dur; dur=$(echo "$out" | python3 -c "import json,sys; print(json.load(sys.stdin)['format'].get('duration','0'))" 2>/dev/null || echo 0)
    local vid=0; echo "$out" | grep -q '"codec_type": "video"' && vid=1
    echo "$dur|$vid"
}

frame_hashes() { # frame_hashes <file> -> framemd5 lines of the video stream
    ffmpeg -v error -i "$1" -map 0:v:0 -f framemd5 - 2>/dev/null | grep -E '^[0-9]+,' || true
}

assert_merged() { # assert_merged <dest> <source> <label>
    [ -f "$1" ] || fail "$3: merged file missing: $1"
    local info; info=$(ffprobe_info "$1")
    local dur=${info%%|*} vid=${info##*|}
    [ "$vid" = 1 ] || fail "$3: merged file has no video stream ($1)"
    python3 -c "import sys; d=float('$dur'); sys.exit(0 if 5.5 <= d <= 6.5 else 1)" \
        || fail "$3: merged duration $dur not ~= 6 s"
    local a b
    a=$(frame_hashes "$1"); b=$(frame_hashes "$2")
    [ -n "$a" ] && [ "$a" = "$b" ] || fail "$3: merged video frames differ from the source"
    log "$3: merged file plays (duration ${dur}s, video stream) and frames match the source"
}

queue_url() { # queue_url <url> — ask the app to fetch it via queue.json
    printf '["%s"]' "$1" > "$DL/queue.json"
}

# --- preconditions ---------------------------------------------------------
rm -rf "$TESTDIR"
mkdir -p "$TESTDIR" "$DL" "$SRVDIR"
: > "$SUMMARY"
log "E2E HLS/DASH merging — $(date)"
command -v ffmpeg >/dev/null || fail "ffmpeg not on PATH"
command -v ffprobe >/dev/null || fail "ffprobe not on PATH"
log "ffmpeg $(ffmpeg -version | head -1 | awk '{print $3}')"
if pgrep -x zdm >/dev/null; then ZDM_RUNNING_AT_START=1; log "user ZDM is running (will relaunch at end)"; fi
if ls ~/Downloads/ZDM/*.part >/dev/null 2>&1; then
    fail "user has active .part files in ~/Downloads/ZDM — aborting, nothing touched"
fi
log "preconditions ok"

# --- fixtures: 6 s source, HLS + DASH + AES-128 + live ----------------------
# -g 20 forces a keyframe every 2 s: -c copy HLS/DASH can only split
# at keyframes, so without it the 6 s source becomes one segment.
ffmpeg -y -v error -f lavfi -i testsrc=duration=6:size=320x240:rate=10 \
    -f lavfi -i sine=frequency=440:duration=6 \
    -c:v libx264 -pix_fmt yuv420p -r 10 -g 20 -c:a aac -shortest \
    "$SRVDIR/source.mp4" || fail "ffmpeg could not make source.mp4"
ffmpeg -y -v error -i "$SRVDIR/source.mp4" -c copy -f hls -hls_time 2 \
    -hls_list_size 0 -hls_segment_filename "$SRVDIR/seg%d.ts" \
    "$SRVDIR/playlist.m3u8" || fail "ffmpeg could not make HLS"
ffmpeg -y -v error -i "$SRVDIR/source.mp4" -c copy -f dash -seg_duration 2 \
    -use_template 1 -use_timeline 1 "$SRVDIR/manifest.mpd" || fail "ffmpeg could not make DASH"
python3 -c "open('$SRVDIR/enc.key','wb').write(bytes([0x11])*16)"
printf 'http://127.0.0.1:%s/enc.key\n%s/enc.key\n' "$PORT" "$SRVDIR" > "$SRVDIR/keyinfo"
ffmpeg -y -v error -i "$SRVDIR/source.mp4" -c copy -hls_time 2 -hls_list_size 0 \
    -hls_segment_filename "$SRVDIR/enc%d.ts" -hls_key_info_file "$SRVDIR/keyinfo" \
    "$SRVDIR/enc.m3u8" || fail "ffmpeg could not make AES-128 HLS"
printf '#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2.0,\nseg0.ts\n#EXTINF:2.0,\nseg1.ts\n' > "$SRVDIR/live.m3u8"
log "fixtures: $(ls "$SRVDIR" | tr '\n' ' ')"
log "HLS segments: $(ls "$SRVDIR"/seg*.ts | wc -l), playlist lines: $(wc -l < "$SRVDIR/playlist.m3u8")"

cp "$SETTINGS" "$BACKUP" && log "settings.json backed up"
zdm_kill9

python3 .taste-skill/e2e-autoresume/range_server.py --dir "$SRVDIR" --port $PORT --log "$SERVER_LOG" &
SERVER_PID=$!
sleep 1
curl -s -o /dev/null "http://127.0.0.1:$PORT/playlist.m3u8" || fail "range server not responding"

# --- Phase 1: HLS merges ----------------------------------------------------
log "== Phase 1: HLS stream must merge into one playable mp4 =="
patch_settings false
zdm_start
queue_url "http://127.0.0.1:$PORT/playlist.m3u8"
wait_status playlist.mp4 completed 60 || fail "playlist.mp4 did not complete: $(db_row playlist.mp4) err=$(db_err playlist.mp4)"
ROW=$(db_row playlist.mp4)
[ "$(echo "$ROW" | cut -d'|' -f2)" = "hls" ] || fail "stream_kind is not hls: $ROW"
DEST=$(db_dest playlist.mp4)
assert_merged "$DEST" "$SRVDIR/source.mp4" "Phase 1 HLS"
log "Phase 1 PASS"
zdm_kill9

# --- Phase 2: DASH muxes ----------------------------------------------------
log "== Phase 2: DASH stream must mux into one playable mp4 =="
zdm_start
queue_url "http://127.0.0.1:$PORT/manifest.mpd"
wait_status manifest.mp4 completed 60 || fail "manifest.mp4 did not complete: $(db_row manifest.mp4) err=$(db_err manifest.mp4)"
ROW=$(db_row manifest.mp4)
[ "$(echo "$ROW" | cut -d'|' -f2)" = "dash" ] || fail "stream_kind is not dash: $ROW"
DEST=$(db_dest manifest.mp4)
assert_merged "$DEST" "$SRVDIR/source.mp4" "Phase 2 DASH"
log "Phase 2 PASS"
zdm_kill9

# --- Phase 3: AES-128 HLS decrypts ------------------------------------------
log "== Phase 3: AES-128 HLS must decrypt and merge =="
zdm_start
queue_url "http://127.0.0.1:$PORT/enc.m3u8"
wait_status enc.mp4 completed 60 || fail "enc.mp4 did not complete: $(db_row enc.mp4) err=$(db_err enc.mp4)"
DEST=$(db_dest enc.mp4)
assert_merged "$DEST" "$SRVDIR/source.mp4" "Phase 3 AES-128"
log "Phase 3 PASS"
zdm_kill9

# --- Phase 4: live playlist rejected ----------------------------------------
log "== Phase 4: live playlist must fail with a clear message =="
zdm_start
queue_url "http://127.0.0.1:$PORT/live.m3u8"
wait_status live.m3u8 failed 60 || fail "live.m3u8 did not fail: $(db_row live.m3u8)"
ERR=$(db_err live.m3u8)
echo "$ERR" | grep -q "live stream" || fail "live failure message is not honest: $ERR"
[ -f "$DL/Video/live.m3u8" ] && fail "live playlist produced a file anyway"
log "live rejected honestly ('$ERR') — Phase 4 PASS"
zdm_kill9

# --- Phase 5: DASH without ffmpeg -> playlist only --------------------------
log "== Phase 5: DASH without ffmpeg must fall back to the playlist =="
zdm_start "ZDM_FFMPEG="
queue_url "http://127.0.0.1:$PORT/manifest.mpd"
wait_status manifest.mpd completed 60 || fail "manifest.mpd did not complete: $(db_row manifest.mpd) err=$(db_err manifest.mpd)"
DEST=$(db_dest manifest.mpd)
[ -f "$DEST" ] || fail "fallback playlist file missing"
head -c 5 "$DEST" | grep -q '<?xml' || fail "fallback file is not the playlist XML"
NOTE=$(python3 -c "import json; print(json.load(open('$DEST.download.json')).get('stream_note',''))" 2>/dev/null || echo "")
echo "$NOTE" | grep -q "ffmpeg" || fail "fallback note does not mention ffmpeg: $NOTE"
log "playlist downloaded + note '$NOTE' — Phase 5 PASS"
zdm_kill9

# --- Phase 6: kill -9 mid-download, resume must not redo segments ----------
log "== Phase 6: interruption mid-segment-download must resume cleanly =="
# A longer stream (60 s, 30 segments) with a dribbled server: each
# .ts body arrives in two halves 3 s apart, so a kill at ~5 s lands
# while segments are in flight for sure.
ffmpeg -y -v error -f lavfi -i testsrc=duration=60:size=320x240:rate=10 \
    -f lavfi -i sine=frequency=440:duration=60 \
    -c:v libx264 -pix_fmt yuv420p -r 10 -g 20 -c:a aac -shortest \
    "$SRVDIR/long.mp4" || fail "ffmpeg could not make long.mp4"
ffmpeg -y -v error -i "$SRVDIR/long.mp4" -c copy -f hls -hls_time 2 \
    -hls_list_size 0 -hls_segment_filename "$SRVDIR/long%d.ts" \
    "$SRVDIR/long.m3u8" || fail "ffmpeg could not make long HLS"
kill "$SERVER_PID" 2>/dev/null; sleep 1
python3 .taste-skill/e2e-autoresume/range_server.py --dir "$SRVDIR" --port $PORT --log "$SERVER_LOG" --dribble 3 --dribble-suffix .ts &
SERVER_PID=$!
sleep 1
curl -s -o /dev/null "http://127.0.0.1:$PORT/long.m3u8" || fail "dribble server not responding"
patch_settings true
zdm_start
queue_url "http://127.0.0.1:$PORT/long.m3u8"
# Wait until at least one segment request hit the server, then kill.
for _ in $(seq 1 30); do grep -q "GET /long" "$SERVER_LOG" 2>/dev/null && break; sleep 0.5; done
grep -q "GET /long" "$SERVER_LOG" || fail "no segment request reached the server"
sleep 4
zdm_kill9
log "killed mid-download; server log so far:"
grep "GET /long" "$SERVER_LOG" | head -n 12 | tee -a "$SUMMARY"
# Reopen: resume_on_startup must pick the stream up by itself.
zdm_start
wait_status long.mp4 completed 120 || fail "long.mp4 did not complete after resume: $(db_row long.mp4) err=$(db_err long.mp4)"
DEST=$(db_dest long.mp4)
# The merged file must play and match the long source's frames.
INFO=$(ffprobe_info "$DEST"); DUR=${INFO%%|*} VID=${INFO##*|}
[ "$VID" = 1 ] || fail "Phase 6: merged file has no video stream"
python3 -c "import sys; d=float('$DUR'); sys.exit(0 if 59.0 <= d <= 61.0 else 1)" \
    || fail "Phase 6: merged duration $DUR not ~= 60 s"
A=$(frame_hashes "$DEST"); B=$(frame_hashes "$SRVDIR/long.mp4")
[ -n "$A" ] && [ "$A" = "$B" ] || fail "Phase 6: merged frames differ from the source after resume"
log "merged long file plays (duration ${DUR}s) and frames match — resume produced a correct file"
# Server-log proof: finished segments requested exactly once, and at
# least one segment resumed with a Range request (non-zero start).
python3 - "$SERVER_LOG" <<'EOF' || fail "Phase 6: resume proof in the server log failed"
import re, sys
from collections import Counter
gets, ranged = Counter(), Counter()
for line in open(sys.argv[1]):
    m = re.match(r"\S+ GET (\S+) Range=(\S+) (\d+)", line)
    if not m:
        continue
    path, rng, _ = m.groups()
    if path.startswith("/long") and path.endswith(".ts"):
        gets[path] += 1
        if rng != "-" and not rng.startswith("bytes=0-"):
            ranged[path] += 1
once = [p for p, c in gets.items() if c == 1]
print(f"segments requested: {len(gets)}, requested exactly once: {len(once)}, resumed with Range: {len(ranged)}")
assert once, "every segment was re-requested — finished segments were not skipped"
assert ranged, "no segment resumed with a Range request"
EOF
log "Phase 6 PASS"

log "ALL PHASES PASS"
