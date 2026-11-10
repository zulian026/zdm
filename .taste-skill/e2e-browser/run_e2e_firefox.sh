#!/usr/bin/env bash
# Firefox intercept e2e (via Selenium + temporary add-on):
# page auto-download -> extension cancels -> native host ->
# queue.json -> /usr/bin/zdm downloads the file.
# Settings are backed up and restored byte-for-byte afterwards.
set -u
W=/tmp/zdm-e2e-browser
SETTINGS=~/.config/zdm/settings.json
LOG=$W/e2e-firefox.log
: > $LOG
say() { echo "$(date +%H:%M:%S) $*" | tee -a $LOG; }
fail() { say "FAIL: $*"; exit 1; }

pkill -f "^/usr/bin/zdm" 2>/dev/null; sleep 2; pkill -9 -f "^/usr/bin/zdm" 2>/dev/null; sleep 1
[ "$(pgrep -c zdm || true)" = "0" ] || fail "zdm still running"
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
mkdir -p $W/www-ff
head -c 5242880 /dev/urandom > $W/www-ff/test-5mib-ff.bin
sha256sum $W/www-ff/test-5mib-ff.bin | tee -a $LOG
cat > $W/www-ff/index.html <<'HTMLEOF'
<html><body><a id="dl" href="/test-5mib-ff.bin" download>dl</a>
<script>window.addEventListener("load", () => setTimeout(() => document.getElementById("dl").click(), 500));</script>
</body></html>
HTMLEOF
(cd $W/www-ff && nohup python3 -m http.server 18766 --bind 127.0.0.1 > $W/http-ff.log 2>&1 &)
sleep 1; curl -s -o /dev/null -w "http server: %{http_code}\n" http://127.0.0.1:18766/test-5mib-ff.bin | tee -a $LOG
rm -f ~/Downloads/ZDM/Programs/test-5mib-ff.bin ~/Downloads/ZDM/queue.json
DISPLAY=:2 nohup /usr/bin/zdm > $W/zdm-ff.log 2>&1 &
sleep 6; [ "$(pgrep -c zdm)" = "1" ] || fail "zdm not running (count $(pgrep -c zdm))"
say "zdm running: $(pgrep -c zdm) proc"

cat > $W/ff_drive.py <<'PYEOF'
import time, os
# NOTE: Selenium 4.52 opts.headless=True did NOT propagate to Firefox here
# (geckodriver log: "Error: no DISPLAY environment variable specified").
# MOZ_HEADLESS=1 in the environment is what makes manual headless work.
os.environ["MOZ_HEADLESS"] = "1"
from selenium import webdriver
from selenium.webdriver.firefox.options import Options
from selenium.webdriver.firefox.firefox_profile import FirefoxProfile
from selenium.webdriver.firefox.service import Service
# NOTE: Firefox 159 headless fails with "Could not find profile folder"
# unless the profile dir pre-exists; pass an explicit pre-created profile.
prof_dir = "/tmp/zdm-e2e-browser/ff-profile"
os.makedirs(prof_dir, exist_ok=True)
opts = Options()
opts.headless = True
opts.add_argument("-headless")
opts.profile = FirefoxProfile(prof_dir)
opts.set_preference("browser.download.dir", "/tmp/zdm-e2e-browser/ff-downloads")
opts.set_preference("browser.download.folderList", 2)
svc = Service(log_output="/tmp/zdm-e2e-browser/geckodriver.log")
driver = webdriver.Firefox(options=opts, service=svc)
try:
    addon_id = driver.install_addon("/home/zyan/workspace/zdm-tauri/browser/firefox", temporary=True)
    print("ADDON INSTALLED:", addon_id)
    driver.get("http://127.0.0.1:18766/index.html")
    print("PAGE TITLE:", driver.title)
    time.sleep(15)
    print("PAGE DONE, queue check happens outside")
finally:
    driver.quit()
    print("DRIVER QUIT")
PYEOF
$W/venv/bin/python $W/ff_drive.py 2>&1 | tee -a $LOG
say "queue.json after intercept: $(cat ~/Downloads/ZDM/queue.json 2>/dev/null || echo MISSING)"
for i in $(seq 1 30); do
  [ -f ~/Downloads/ZDM/Programs/test-5mib-ff.bin ] && [ "$(stat -c%s ~/Downloads/ZDM/Programs/test-5mib-ff.bin 2>/dev/null)" = "5242880" ] && break
  sleep 2
done
say "zdm procs at end: $(pgrep -c zdm)"
say "downloaded file:"; ls -la ~/Downloads/ZDM/Programs/test-5mib-ff.bin 2>&1 | tee -a $LOG
say "sha:"; sha256sum ~/Downloads/ZDM/Programs/test-5mib-ff.bin 2>&1 | tee -a $LOG
say "db row:"; python3 -c "
import sqlite3
c = sqlite3.connect('/home/zyan/Downloads/ZDM/downloads.db')
for r in c.execute(\"SELECT id, filename, status, downloaded, total FROM downloads WHERE url LIKE '%test-5mib-ff%'\"):
    print(r)
" 2>&1 | tee -a $LOG
say "browser-downloaded stray file (must NOT exist):"; ls -la ~/Downloads/test-5mib-ff.bin $W/ff-downloads/ 2>&1 | tee -a $LOG
pkill -f "^/usr/bin/zdm" 2>/dev/null; sleep 2; pkill -9 -f "^/usr/bin/zdm" 2>/dev/null; sleep 1
pkill -f "http.server 18766" 2>/dev/null
cp $W/settings.json.bak "$SETTINGS"
say "settings restored:"; sha256sum "$SETTINGS" | tee -a $LOG
rm -f ~/Downloads/ZDM/Programs/test-5mib-ff.bin
say "E2E FIREFOX DONE"
