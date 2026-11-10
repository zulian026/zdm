# ZDM Browser Integration (Tauri)

Automatic download interception for Chromium and Firefox, IDM-style:
a download started in the browser is cancelled there and handed to
ZDM instead. No Windows-style OS hooking exists on Linux — this uses
the supported route: a browser extension (`downloads` API) + a native
messaging host + the queue file the Tauri app already polls.

## How it works

```
browser download starts
  -> extension background.js (downloads.onCreated)
  -> only http(s) URLs, only when auto-intercept is ON (default)
  -> browser download cancelled + erased from the shelf
  -> chrome.runtime.sendNativeMessage("com.zdm.native", {url})
  -> browser/zdm-native-host.py (Python 3 stdlib, Native Messaging protocol)
  -> appends the URL to ~/Downloads/ZDM/queue.json
     (JSON array of strings — the same format the Python ZDM uses)
  -> launches ~/workspace/zdm-tauri/release/zdm if not running
     (standalone binary: no AppImage GStreamer appsink warning)
  -> Tauri app polls queue.json every ~2 s (queue_loop in
     src-tauri/src/lib.rs), dedupes by URL, and — with "Ask before
     downloading" ON (Settings, default) — holds the URL as PENDING:
     the ZDM window comes to the front and a "Download File Info"
     confirmation popup appears (filename, URL, size from a
     background HEAD probe, save folder, category, connections).
     [Start Download] starts it at once, [Add to Queue] queues it,
     [Cancel] drops it. With the setting OFF, intercepted URLs
     auto-start the queue as before (max 3 concurrent, dynamic
     segmentation).
```

A shared lock file (`~/Downloads/ZDM/.queue.lock`, exclusive-create,
stale after 10 s) is used by both the native host and the Tauri app,
so a browser download landing mid-drain is never lost. The frontend
already polls `list_downloads` every 2 s, so intercepted downloads
appear in the UI on their own.

## Files

```
browser/
  background.js            shared source of truth for the extension
  content.js               shared source of truth for the page media
                           grabber (content script, see below)
  chromium/
    background.js          identical copy (extensions load only
    content.js             identical copy
    manifest.json          MV3, service_worker. The "key" field pins
                           the extension ID to
                           mojdpgikbnejopblfjfphlkdnogkkimh so the
                           native host manifest can name it in advance.
    com.zdm.native.json    template (__ZDM_HOST_PATH__ placeholder;
                           install.sh writes the real path)
  firefox/
    background.js          identical copy
    content.js             identical copy
    manifest.json          MV3, background scripts, gecko id
                           zdm@example.local (must match
                           allowed_extensions in the native manifest)
    com.zdm.native.json    template (same placeholder)
  zdm-native-host.py       native host (stdlib only, executable)
  install.sh               registers the host for both browsers
  README.md                this file
```

## Install

On the machine that runs the browsers (the laptop):

```bash
cd ~/workspace/zdm-tauri
./browser/install.sh
```

That registers the native host for Chromium
(`~/.config/chromium/NativeMessagingHosts/com.zdm.native.json`) and
Firefox (`~/.mozilla/native-messaging-hosts/com.zdm.native.json`).
No sudo needed.

## Load the extension (manual — browsers require a click in the UI)

There is no supported headless way to do this step; it is one click
per browser, once (Chromium keeps unpacked extensions across
restarts; Firefox temporary add-ons do not — see below).

**Chromium** (`chrome://extensions`):

1. Turn **Developer mode** ON (top right).
2. **Load unpacked** -> select `~/workspace/zdm-tauri/browser/chromium`.
3. The extension ID shown must be `mojdpgikbnejopblfjfphlkdnogkkimh`
   (pinned by the manifest `key`). If it ever differs, re-run:
   `CHROMIUM_EXTENSION_ID=<shown id> ./browser/install.sh`.

**Firefox** (`about:debugging#/runtime/this-firefox`):

1. **Load Temporary Add-on…** -> select
   `~/workspace/zdm-tauri/browser/firefox/manifest.json`.
2. Temporary add-ons disappear when Firefox restarts — load it again
   after a restart. (A permanent install would need a signed `.xpi`
   from addons.mozilla.org, which this project does not ship.)

## Use

- **Auto-intercept (default ON):** any http(s) download started in
  the browser is cancelled there and appears in ZDM as a
  confirmation popup (see below). The toolbar badge shows `ON`.
- **Confirmation popup:** with "Ask before downloading" ON (ZDM
  Settings, default), an intercepted URL waits as PENDING — nothing
  is downloaded yet — until you press **Start Download** (starts
  immediately) or **Add to Queue** (queued, starts when the queue
  runs). Several URLs queue up: the dialog shows the first with
  "N more pending" and advances on each confirm/cancel. The file
  size appears once the background HEAD probe answers; "Unknown"
  means the server did not report one. Pending items live only in
  memory: closing ZDM with items still pending drops them.
  Turn the setting OFF to restore silent auto-start.
- **Cancel means the file is NOT downloaded.** The extension
  cancels the browser download at intercept time, so by the time
  the ZDM dialog appears there is no browser download left to
  resume — pressing Cancel discards the URL entirely. To get the
  file, click the link again. (Pausing the browser download and
  resuming it on Cancel is not feasible over the one-shot
  native-messaging round-trip.)
- **Toggle:** click the ZDM toolbar icon — badge flips to `OFF`,
  downloads go to the browser again. Click again to re-enable.
- **One-off:** right-click a link -> **Download with ZDM**. Works
  even when auto-intercept is OFF.
- **Page media grabber:** the content script (`content.js`, on every
  http(s) page) scans the live DOM for `<video>`/`<audio>` and reads
  `currentSrc`, so it catches sources set by JavaScript that never
  appear in the page HTML. Hover a video and a small **Download with
  ZDM** button appears near its top-right corner (IDM-style); click
  it and the URL goes to ZDM through the same native-host path, so
  the confirmation popup applies as usual. Right-click a video or
  audio element -> **Download video/audio with ZDM** does the same
  without the button. No `webRequest` sniffing: the DOM scan covers
  the common cases and keeps the permission set minimal.
  - `blob:` / `data:` sources get no button — the browser does not
    hand those bytes out to extensions.
  - A `.m3u8` / `.mpd` URL downloads the playlist file, not the
    merged video; ZDM does not yet join HLS/DASH segments.
  - Players that fetch media purely over XHR/fetch (no `<video>`
    source) are not seen by the content script.
- **Not intercepted:** `blob:` / `data:` / `chrome://` / `about:` /
  `moz-extension://` / `file://` URLs, and downloads that already
  finished before the extension could cancel them (tiny files).

## Troubleshooting

- **Download stays in the browser:** check the toolbar badge is `ON`,
  and that the extension is enabled. Open the extension's service
  worker / background page console: a `[ZDM] native host error`
  means the host is not registered — re-run `./browser/install.sh`
  and restart the browser.
- **URL never reaches ZDM:** check `~/Downloads/ZDM/queue.json`
  (should be `[]` after ZDM drains it) and
  `~/Downloads/ZDM/native-host.log` for host errors.
- **ZDM does not start:** the host launches
  `<project>/release/zdm` when no `zdm` process runs. If the binary
  is missing, rebuild (`npm run tauri build`) and re-sync.
- **No confirmation popup:** check Settings -> "Ask before
  downloading" is ON. If it is OFF, intercepted downloads
  auto-start silently by design. If it is ON and still nothing
  appears, check `~/Downloads/ZDM/queue.json` (should be `[]`
  after ZDM drains it) — a stale URL there means ZDM is not
  running or not polling.
- **Popup shows but the size stays "Unknown":** the server did not
  answer the HEAD probe (or did not send Content-Length). The
  download still works — the size is filled in once it starts.
- **Firefox lost the extension after a restart:** expected for a
  temporary add-on — load it again via `about:debugging`.
- **Chromium shows a different extension ID:** re-run install.sh with
  `CHROMIUM_EXTENSION_ID=<id>` (see Install).
- **Two ZDM windows:** the host only launches ZDM when no `zdm`
  process is found (`/proc` scan). If you run the AppImage and the
  binary at the same time, close one — the queue file is shared, so
  both would drain it, but only one UI is needed.
