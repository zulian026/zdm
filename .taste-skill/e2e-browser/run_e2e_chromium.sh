#!/usr/bin/env bash
# Chromium intercept e2e: page auto-download -> extension cancels ->
# native host -> queue.json -> /usr/bin/zdm downloads the file.
# Settings are backed up and restored byte-for-byte afterwards.
set -u
W=/tmp/zdm-e2e-browser
SETTINGS=~/.config/zdm/settings.json
LOG=$W/e2e.log
: > $LOG
say() { echo "$(date +%H:%M:%S) $*" | tee -a $LOG; }
fail() { say "FAIL: $*"; exit 1; }

# 1. stop any ZDM I started earlier (drops the memory-only PENDING test URL)
pkill -f "^/usr/bin/zdm" 2>/dev/null; sleep 2; pkill -9 -f "^/usr/bin/zdm" 2>/dev/null; sleep 1
[ "$(pgrep -c zdm || true)" = "0" ] || fail "zdm still running"
# 2. backup settings, set ask OFF + clamav OFF
cp "$SETTINGS" $W/settings.json.bak
sha256sum "$SETTINGS" | tee -a $LOG
python3 - "$SETTINGS" <<'PYEOF'
import json, sys
p = sys.argv[1]
s = json.load(open(p))
s["ask_before_download"] = False
s["clamav_scan"] = False
json.dump(s, open(p, "w"), indent=2)
print("ask_before_download:", s["ask_before_download"], "clamav_scan:", s["clamav_scan"])
PYEOF
# 3. test file + page
mkdir -p $W/www
head -c 5242880 /dev/urandom > $W/www/test-5mib.bin
sha256sum $W/www/test-5mib.bin | tee -a $LOG
cat > $W/www/index.html <<'HTMLEOF'
<html><body><a id="dl" href="/test-5mib.bin" download>dl</a>
<script>window.addEventListener("load", () => setTimeout(() => document.getElementById("dl").click(), 500));</script>
</body></html>
HTMLEOF
(cd $W/www && nohup python3 -m http.server 18765 --bind 127.0.0.1 > $W/http.log 2>&1 &)
sleep 1; curl -s -o /dev/null -w "http server: %{http_code}\n" http://127.0.0.1:18765/test-5mib.bin | tee -a $LOG
rm -f ~/Downloads/ZDM/test-5mib.bin ~/Downloads/ZDM/queue.json
# 4. start /usr/bin/zdm
DISPLAY=:2 nohup /usr/bin/zdm > $W/zdm.log 2>&1 &
sleep 6; [ "$(pgrep -c zdm)" = "1" ] || fail "zdm not running (count $(pgrep -c zdm))"
say "zdm running: $(pgrep -c zdm) proc"
# 5. headless Chromium with the extension loads the page (auto-download)
rm -rf /tmp/chr-e2e
# NOTE: Chrome looks for the native-host manifest under the --user-data-dir's
# NativeMessagingHosts/, NOT ~/.config/chromium/ — so copy it into the temp
# profile. In real use (default profile) install.sh already registered it there.
mkdir -p /tmp/chr-e2e/NativeMessagingHosts
cp ~/.config/chromium/NativeMessagingHosts/com.zdm.native.json /tmp/chr-e2e/NativeMessagingHosts/
chromium --headless=new --no-sandbox --disable-gpu --user-data-dir=/tmp/chr-e2e --load-extension=/home/zyan/workspace/zdm-tauri/browser/chromium --enable-logging=stderr --v=0 --virtual-time-budget=15000 --dump-dom http://127.0.0.1:18765/index.html > $W/chr-dom.html 2> $W/chr-log.txt; say "chromium exit=$?"
say "queue.json after intercept: $(cat ~/Downloads/ZDM/queue.json 2>/dev/null || echo MISSING)"
# 6. wait for ZDM to finish the download (max 60 s)
for i in $(seq 1 30); do
  [ -f ~/Downloads/ZDM/test-5mib.bin ] && [ "$(stat -c%s ~/Downloads/ZDM/test-5mib.bin 2>/dev/null)" = "5242880" ] && break
  sleep 2
done
say "zdm procs at end: $(pgrep -c zdm)"
say "downloaded file:"; ls -la ~/Downloads/ZDM/test-5mib.bin 2>&1 | tee -a $LOG
say "sha:"; sha256sum ~/Downloads/ZDM/test-5mib.bin 2>&1 | tee -a $LOG
say "browser-downloaded stray file in ~/Downloads (must NOT exist):"; ls -la ~/Downloads/test-5mib.bin 2>&1 | tee -a $LOG
# 7. cleanup: stop ZDM + server, restore settings, remove test file
pkill -f "^/usr/bin/zdm" 2>/dev/null; sleep 2; pkill -9 -f "^/usr/bin/zdm" 2>/dev/null; sleep 1
pkill -f "http.server 18765" 2>/dev/null
cp $W/settings.json.bak "$SETTINGS"
say "settings restored:"; sha256sum "$SETTINGS" | tee -a $LOG
rm -f ~/Downloads/ZDM/test-5mib.bin
say "E2E DONE"
