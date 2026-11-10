# ZDM Browser Integration (Tauri)

Automatic download interception for Chromium-family browsers
(Chromium, Chrome, Edge, Brave, Opera, Vivaldi) and Firefox,
IDM-style:
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
  -> launches ZDM if not running: /usr/bin/zdm first (system
     install from the .deb), then
     ~/workspace/zdm-tauri/release/zdm (standalone binary: no
     AppImage GStreamer appsink warning), then the AppImage
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
                           (video/audio context menus + floating
                           button listener); the per-browser copies
                           are byte-identical — package-xpi.sh
                           enforces this for Firefox at build time
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
    zdm-firefox-1.2.0.xpi  built by package-xpi.sh (unsigned, for
                           AMO submission — see below; git-ignored)
  package-xpi.sh           builds the Firefox .xpi (manifest.json +
                           background.js + content.js only)
  zdm-native-host.py       native host (stdlib only, executable)
  install.sh               registers the host for every supported
                           browser (all Chromium-family browsers
                           with a binary or config dir, plus Firefox)
  README.md                this file
```

## Install

On the machine that runs the browsers (the laptop):

```bash
cd ~/workspace/zdm-tauri
./browser/install.sh
```

That registers the native host for every supported browser it finds
— Chromium (`~/.config/chromium/NativeMessagingHosts/`), Chrome
(`~/.config/google-chrome/NativeMessagingHosts/`), Edge
(`~/.config/microsoft-edge/NativeMessagingHosts/`), Brave
(`~/.config/BraveSoftware/Brave-Browser/NativeMessagingHosts/`),
Opera (`~/.config/opera/NativeMessagingHosts/`), Vivaldi
(`~/.config/vivaldi/NativeMessagingHosts/`), and Firefox
(`~/.mozilla/native-messaging-hosts/`). A browser is registered when
its binary is on PATH or its config directory exists; the script
prints which ones it registered and which it skipped. No sudo needed.

One pinned extension ID (`mojdpgikbnejopblfjfphlkdnogkkimh`) is used
for all Chromium-family browsers, so the same unpacked extension
works in each of them.

## Load the extension (manual — browsers require a click in the UI)

There is no supported headless way to do this step; it is one click
per browser, once (Chromium-family browsers keep unpacked extensions
across restarts; Firefox temporary add-ons do not — see below).

**Chromium-family** — Chromium, Chrome, Edge, Brave, Opera, Vivaldi
(all use the same steps and the same pinned extension ID):

1. Open `chrome://extensions` (Edge: `edge://extensions`,
   Brave: `brave://extensions`, Opera: `opera://extensions`,
   Vivaldi: `vivaldi://extensions`).
2. Turn **Developer mode** ON (top right).
3. **Load unpacked** -> select `~/workspace/zdm-tauri/browser/chromium`.
4. The extension ID shown must be `mojdpgikbnejopblfjfphlkdnogkkimh`
   (pinned by the manifest `key`). If it ever differs, re-run:
   `CHROMIUM_EXTENSION_ID=<shown id> ./browser/install.sh`.

**Firefox** (`about:debugging#/runtime/this-firefox`):

1. **Load Temporary Add-on…** -> select
   `~/workspace/zdm-tauri/browser/firefox/manifest.json`.
2. Temporary add-ons disappear when Firefox restarts — load it again
   after a restart. For a permanent install, see below.

### Firefox, permanent install

Firefox Release and Beta only install extensions signed by Mozilla.
An unsigned `.xpi` cannot be installed permanently on them — and an
enterprise policy (`policies.json`, `Extensions.Install`) does **not**
exempt it from signing either: the policy only controls where the
extension is installed from, the signature check still applies. The
only ways to run this extension unsigned are Firefox ESR (with
signing enforcement disabled by an administrator) or Developer
Edition / Nightly (with `xpinstall.signatures.required` set to
`false`) — none of which is the stock Firefox Release this project
targets. So a permanent install on stock Firefox means getting the
extension signed through addons.mozilla.org (AMO). **No signed
`.xpi` is shipped with this project, and the extension is not
published on AMO** — signing is a manual step for the maintainer:

1. Build the package (unsigned):
   ```bash
   ./browser/package-xpi.sh
   # -> browser/firefox/zdm-firefox-1.2.0.xpi
   ```
   The manifest already carries what AMO requires for a new
   submission: the gecko id (`zdm@example.local`) and a data
   collection declaration (`data_collection_permissions.required:
   ["none"]` — the extension sends URLs only to the local native
   host on the same machine, never to a server).
2. Create an account at
   [addons.mozilla.org](https://addons.mozilla.org/) and open the
   Developer Hub → **Submit a New Add-on**.
3. Choose the distribution: **"On your own" (unlisted)** to keep the
   extension self-distributed (you host the signed `.xpi` yourself),
   or **"On this site" (listed)** for a public AMO listing.
4. Upload `zdm-firefox-1.2.0.xpi`, fill in the listing details, and
   submit. Mozilla reviews the submission (automated validation
   first; a manual review can follow at any time) and signs it.
   (Equivalent command-line route: `web-ext sign --channel=unlisted`
   with AMO API keys from the Developer Hub.)
5. Download the **signed** `.xpi` AMO returns and install it in
   Firefox (open the file, or drag it onto the browser window).
   A signed unlisted `.xpi` installs permanently and survives
   restarts; Firefox updates it when a newer signed version is
   published at the same update URL.

Alternative without AMO: run **Firefox ESR** and disable signing
enforcement (`about:config` → `xpinstall.signatures.required` →
`false`), then install the unsigned `.xpi` from step 1. That is the
only supported unsigned route Mozilla documents for an ESR
deployment, and it only applies to ESR — not to stock Firefox
Release.

## Safari

Not supported. A Safari extension needs macOS, Xcode, and an Apple
Developer account, none of which this Linux-only project has. The
extension is Chromium-family + Firefox only.

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
  - A `.m3u8` / `.mpd` URL goes through ZDM's stream merging: HLS
    downloads the best variant's segments and remuxes them to
    `.mp4` (AES-128 HLS supported); DASH muxes the best video +
    audio to `.mp4` via ffmpeg. Limits: live streams are rejected
    (VOD only), and DASH without ffmpeg falls back to the playlist
    file with an honest note on the row.
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
- **ZDM does not start:** the host launches `/usr/bin/zdm` first,
  then `<project>/release/zdm`, then the AppImage, when no `zdm`
  process runs. If no binary exists, rebuild (`npm run tauri build`)
  and re-sync (or install the `.deb`).
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
  temporary add-on — load it again via `about:debugging`, or do the
  permanent AMO install above.
- **Chromium shows a different extension ID:** re-run install.sh with
  `CHROMIUM_EXTENSION_ID=<id>` (see Install).
- **Two ZDM windows:** the host only launches ZDM when no `zdm`
  process is found (`/proc` scan — it matches `/usr/bin/zdm`,
  `release/zdm`, and the AppImage). If you run the AppImage and the
  binary at the same time, close one — the queue file is shared, so
  both would drain it, but only one UI is needed.
