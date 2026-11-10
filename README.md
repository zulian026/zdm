# ZDM — ZDM Download Manager (Tauri v2)

A modern, clean-room Linux download manager in **Tauri v2 + Rust**,
a port of the Python ZDM (`../download-manager-linux/`), which itself
is inspired by the *observed behaviour* of Internet Download Manager.
No IDM code, binary, string, or trademarked asset is used or copied.
"IDM-inspired" means: it behaves like the class of tool IDM belongs
to — segmented, resumable, queued downloads — re-implemented from the
public HTTP Range specification (RFC 9110).

Design: true-black-flat (taste-skill direction seed `470404968`,
redrawn 2026-11-09 from the original seed `1660322983`, same style —
`.taste-skill/direction.json` is the design authority) — ground `#000`,
surface `#111`, lime `#c6f24e` for data only, Inter (bundled
`@fontsource-variable/inter`, really loaded, not a system fallback), 224px
sidebar + 64px top bar, per-download segment strip visualising dynamic
segmentation. Face: the ZDM icon (lime segmented download arrow) + "ZDM"
wordmark at the top of the sidebar, vertically centred on the 64px top
bar; signature element: the per-download segment strip (lime done,
bright-lime active, red failed, `#2a2a2a` pending) above a thin lime
progress line. Sidebar (2026-11-09): Downloads and Completed at the top,
Settings pinned to the bottom behind a 1px hairline; active item is
brightness alone (#b4b4b4 → #fff, no weight change); no count badge on nav
(the stats object exposes no action-needing queue count, and the counts
that exist already live in the vitals strip). Icons (2026-11-09, `src/icons.ts`
is the one source): a single outline set, 24x24 viewBox, `currentColor`,
1.5px stroke — 16px in nav (label column at x 48) and toolbar, 14px on
row actions, 32px muted on the empty state; icon + text everywhere, never
icon-only; no icon on KPIs, cards, or status pills; Delete takes the
negative colour on hover only. Live speed graph (2026-11-09): one lime
2px SVG line of aggregate total speed over the last 5 minutes (1 s
samples, in-memory ring buffer), two-thirds of the row beside a
Now/Peak/Average block from the same buffer — no legend, no fill,
straight segments, round y ticks, flat x labels. Timeline dashboard
(2026-11-09, recomposition of the bento after the RON Design cardiology
reference — composition only, still dark true-black-flat): header row
with the title and category pills from the data (All first) beside the
white Add URL pill; a vitals strip of live figures as type (Queue status
+ next schedule line, Now as the 24px lead, Active, Queued, Downloaded
today); status pills (All / Downloading / Completed / Failed); the speed
card full-width with Now/Peak/Average plus the delta against the 5-minute
average (faster is lime, slower is red, named baseline); the queue as a
timeline grouped per day (Today / Yesterday / weekday + date), each group
with a lime node on a 1px line and its downloads as cards hanging off a
vertical rail, one dot per card — every card carries the segment strip,
row actions and verify line, nothing removed; a scrubber strip under the
timeline (one button per day: dot + count) scrolls to its group on
Downloads and filters to that day on Completed; Completed uses the same
card language. Premium pass (2026-11-09): on desktop the
sidebar is sticky at full viewport height and only the dashboard
scrolls (at ≤900px the sidebar stays a Menu sheet and the page scrolls
normally); one type scale 12/14/16/20/24 with line-heights 20/24/16/28/32px,
weights 400/500/600, tabular figures on every number, exactly two greys
(#b4b4b4 / #949494), panel padding unified at 20px, and 120ms colour-only
press/hover feedback (disabled under prefers-reduced-motion). History filters
(2026-11-09): Completed view filters by category (only categories
present in the data) and date (Today / This week / All time), combined
with the text search, plus the scrubber's one-day filter; the count reads "N completed". Gate (2026-11-09):
lint `--app` 0 FAIL, 2 WARN (padStart ETA clock + padStart day key — both
dates, legitimate); inspect `--app` 0 FAIL, 0 WARN on the empty app and
the Completed timeline, 0 FAIL, 1 WARN on the populated Downloads timeline
(below-fold: a queue is a scrolling list — first screen holds the title,
pills, vitals, chart card, toolbar and the first cards) at 1440/768/390. Known inspector limit: with the
confirmation modal open it flags the 24px KPI values under the dialog's
scrim as covered text — any dialog's backdrop does that by design; the
gate runs on the default states with the modal closed.

## Run

```bash
cd zdm-tauri
npm install          # frontend deps (vite, @tauri-apps/cli, @tauri-apps/api)
npm run tauri dev    # dev: Vite on :1420 + Tauri window
npm run tauri build  # release: src-tauri/target/release/zdm (+ .deb/.AppImage in bundle/)
```

Verified build (2026-11-09, this VM): `npm run tauri build` exits 0
and produces `src-tauri/target/release/zdm` (13 MB),
`bundle/deb/ZDM_0.1.0_amd64.deb` (4.77 MiB),
`bundle/rpm/ZDM-0.1.0-1.x86_64.rpm` (4.77 MiB),
`bundle/appimage/ZDM_0.1.0_amd64.AppImage` (79.80 MiB).
The binary was smoke-tested under Xvfb (20 s, no crash; only benign
libEGL DRI3 warnings, no GPU in the VM, and libayatana-appindicator
warnings about the missing session bus — with no tray host present
the tray simply does not appear and the app keeps running). One
build warning remains:
the identifier `com.zdm.app` ends with `.app`, which Tauri flags as
conflicting with the macOS app-bundle extension — harmless on Linux,
kept as specified.

Rust: the VM used a standalone Rust 1.99.1 install at
`/home/hatch/rust/bin` (rustup extraction fails in this sandbox with
`Operation not permitted`; on a normal machine `rustup` works).
Tauri Linux system deps (Ubuntu 24.04):

```bash
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
  libgtk-3-dev libsoup-3.0-dev patchelf
```

CachyOS (pacman) equivalents: `webkit2gtk-4.1 gtk3 libayatana-appindicator
librsvg base-devel patchelf libsoup3`.

One VM quirk: the sandbox's `libzstd1` is a custom `+hatch1` build,
which blocks `libzstd-dev` (required by webkit2gtk dev headers). The
fix used here was a downgrade to the distro version, which removed
nothing:
`apt-get install -y --allow-downgrades libzstd1=1.5.5+dfsg2-2build1.1`.
On a normal Ubuntu/CachyOS install this step is not needed.

## Features

- **Dynamic segmentation** — HEAD (fallback: Range GET) reads
  `Content-Length` + `Accept-Ranges`; the file is split into N equal
  segments (default 8, configurable 1–32). Each segment is fetched
  with its own `Range: bytes=a-b` connection and written to a
  preallocated `.part` file at the correct offset. When a worker
  finishes, it splits the *largest remaining* segment in half and
  takes the second half, so fast connections steal work from slow
  ones. A server that answers a Range request with `200` instead of
  `206` is handled by discarding the already-known prefix.
- **Resume** — segment offsets persist to a JSON sidecar
  (`<file>.download.json`, same shape as the Python ZDM) plus
  `downloads.db` (rusqlite; url, filename, total, downloaded, status,
  category, created). Restarting rebuilds queued/paused tasks from the
  DB and resumes from the saved offsets. Pause / Resume / Cancel /
  Delete per download.
- **Auto-resume on startup** — Settings → "Resume downloads on
  startup" (default ON). When ZDM opens, downloads that were active
  when it last stopped — queued, or killed mid-download (a DB row
  left as `downloading` with no live worker is stale, so it counts
  as interrupted) — continue immediately from their saved offsets,
  without waiting for the ~30 s scheduler tick. A manually paused
  download stays paused; a download the daily scheduler paused only
  resumes inside its window (outside it, it waits for the window);
  a completed file is never re-downloaded. When OFF, nothing
  auto-resumes at startup — manual resume and the daily schedule
  still work as before (the toggle governs only the immediate
  startup resume). The scheduler-pause flag is persisted in
  `downloads.db` (`paused_by_schedule`), so a restart still knows
  the pause was the scheduler's, not the user's. The interrupted
  signal is the DB row itself staying `downloading`: restore
  converts it to paused in memory only, so flipping the toggle
  back ON after an OFF restart still resumes it.
- **Queue + scheduler** — max concurrent downloads (default 3),
  Start/Stop Queue, schedule a queue start time (`YYYY-MM-DD HH:MM`).
- **Daily schedule** — Settings → Daily schedule: a recurring
  window (start/stop `HH:MM`, weekdays Mon–Sun, default 23:00–06:00
  every day, disabled by default). While ZDM is running, the queue
  starts when the window opens and stops — pausing running
  downloads, never cancelling them — when it closes; a download
  paused by the schedule resumes automatically at the next window,
  while a manually paused download stays paused. Windows that cross
  midnight work (the after-midnight part belongs to the day the
  window started on). The next event is shown in Settings and in
  the Downloads toolbar ("Queue starts today 23:00"). Precedence: a
  pending one-shot schedule and a manual Start/Stop hold the daily
  window until the next window boundary, then the daily window
  takes over again. The schedule only runs while ZDM is running —
  turn on "Start ZDM when I log in" below and ZDM is alive from
  login, so the window can fire overnight.
- **Autostart + app menu** — Settings → "Start ZDM when I log in"
  (default OFF). When ON, the backend installs
  `~/.local/share/applications/zdm.desktop` (app-menu entry) and
  `~/.config/autostart/zdm.desktop` (autostart entry whose Exec
  carries `--minimized`, so ZDM starts hidden in the system tray
  with no window popping up at login); when OFF, the autostart
  entry is removed and the menu entry stays. The files are
  re-synced on every startup and every settings save, so they
  always match the toggle even if deleted by hand. The Exec and
  Icon paths come from the running binary (`current_exe`, repo
  icon next to it, else the cached `~/.cache/zdm/icon.png`). This
  is what makes the daily schedule run overnight. Honest limit:
  autostart at the *next* login cannot be observed without a
  logout/login — after logging back in, check that the ZDM tray
  icon is already there before opening any window.
- **Categories** — auto-categorised by extension, falling back to MIME
  type: Documents, Music, Video, Pictures, Programs, Compressed,
  General, each in its own folder under `~/Downloads/ZDM/`.
- **Speed limit** — global token-bucket limiter (KB/s, 0 = unlimited),
  shared by all workers; retry with exponential backoff (up to 5
  retries); proxy from env vars (`http_proxy`/`https_proxy`) or
  settings; Basic auth embedded in the URL is sent as an
  `Authorization` header.
- **UI** — Downloads as a bento grid: row 1 is the speed card (8/12,
  lime line + Now/Peak/Average + delta vs the 5-min average) beside the
  Today panel (4/12, Downloaded today as the lead figure, Active /
  Queued, Queue + schedule); row 2 is the timeline panel (12/12, the
  biggest block) with the status pills and queue toolbar in its header,
  the queue grouped per day with a lime node + rail, and the day
  scrubber as its footer strip. Header category pills, search, sort,
  count, per-download segment strip (lime = done, bright lime = active,
  grey = pending, red = failed), progress bar, speed + ETA,
  pause/resume/cancel per row; Completed in the same card language
  with category/date filters and a one-day scrubber filter;
  Settings
  (connections, concurrent, download dir, speed limit, proxy,
  schedule); Add URL + Batch + Site Grabber (fetch a page, extract
  `<video>`/`<audio>`/`<source>` media and file links, each labelled
  video / audio / HLS stream / DASH stream / link with a HEAD-probed
  size; blob: sources are skipped).
- **HLS/DASH merging** — a `.m3u8` (HLS) or `.mpd` (DASH) URL is not
  saved as a playlist: ZDM parses it, downloads every segment in
  parallel (with resume — finished segments are never re-downloaded,
  partial ones continue with a Range request), and merges them into
  one playable video (`.mp4` when ffmpeg is available, `.ts` for HLS
  without ffmpeg). HLS picks the highest-bandwidth variant; DASH
  picks the best video + audio adaptation sets. AES-128-encrypted HLS
  is supported through ffmpeg; live streams are rejected honestly
  (VOD only). DASH needs ffmpeg: without it the playlist downloads
  as an ordinary file, with a note saying ffmpeg is missing. The
  merged file goes through the ClamAV scan as usual; checksum
  verification is unavailable for merged files (no expected hash).
  Download rows show the stream kind and segment progress
  (`HLS · 12/48 segmen`, `HLS · video digabung` when done).
- **Live progress** — backend emits `download-progress` (Tauri event)
  at most ~4×/sec per download; the UI also polls `list_downloads`
  every 2 s.
- **Download confirmation popup** — like IDM's "Download File
  Info": a browser-intercepted URL becomes PENDING and the ZDM
  window comes to the front with a dialog showing the filename
  (editable, prefilled from the URL), the URL, the file size (from
  a background HEAD probe when available, else "Unknown"), the
  Save-to folder (editable, default category folder), Category and
  Connections, with [Start Download] [Add to Queue] [Cancel].
  Several pending URLs queue up: the dialog shows the first with
  "N more pending" and advances on confirm/cancel. Pending items
  live only in memory — if the app closes with items still pending,
  they are dropped. Setting "Ask before downloading" (Settings,
  default ON) controls this; with it OFF, intercepted downloads
  auto-start as before. Manual Add URL is unaffected and always
  starts immediately. Backend: `list_pending` / `confirm_pending`
  / `dismiss_pending` commands plus a `pending-download` event;
  see `browser/README.md` for the Cancel caveat (the browser
  download is already gone by then).
- **Desktop notifications** — via the official `tauri-plugin-notification`
  v2 (notify-rust over D-Bus on Linux), icon is the ZDM icon
  (`icons/icon.png`): the bundle resource when present (AppImage/
  .deb), else a cached copy of the icon embedded in the binary
  (`~/.cache/zdm/icon.png`, written once), sent as a `file://` URI.
  One notification per download: "Download selesai: \<file\>
  (\<size\>)" on completion, "Download gagal: \<file\> — \<reason\>" on
  failure; when the queue drains with 2+ completions, one extra
  summary "Semua download selesai" (a single file needs no summary).
  Notifications are sent ONLY when the main window is not focused,
  is minimised, or is hidden to the tray — if you are watching ZDM,
  the list itself is the notification (a transition seen while
  focused is recorded as handled and never fires later). Clicking a
  notification shows and focuses the main window. Setting "Desktop
  notifications" (Settings, default ON) turns them off entirely.
  A missing notification daemon is a logged error, never a crash.
  Verify it yourself: start a download, minimise ZDM (or click
  another window), wait for it to finish — the notification should
  appear. Internals: `src-tauri/src/notify.rs` — the queue loop
  (existing 500 ms tick, no new timer) snapshots task statuses and
  the pure `Notifier::decide` picks what to send; unit-tested
  (dedupe, summary rule, focus gating, restored downloads).
- **Checksum verification** — after a download finishes, ZDM streams
  the file (64 KiB chunks, never whole in memory) and hashes it with
  SHA-256 (`sha2` crate; MD5 with the `md5` crate when the source
  provides MD5). Expected hash, in priority order: (a) a hash you
  type into the optional "Expected SHA-256" field in the Add URL
  modal or the download confirmation popup; (b) the server's `Digest`
  / `Content-Digest` header (RFC 3230/9530, `SHA-256=<base64>` or
  `MD5=`); (c) the `Content-MD5` header; (d) a sidecar file
  `<url>.sha256` / `<url>.md5` fetched by plain GET (bare hash or
  `<hash>  <filename>` format, filename match wins). A match shows
  "Verified · \<first 12 hex\>" (lime check, full hash in the
  tooltip) under the status pill; a mismatch marks the download
  failed with "checksum mismatch", keeps the file on disk, and sends
  a "Checksum tidak cocok" desktop notification. With no hash from
  any source the download stays completed and shows a grey dash —
  a download is never failed just for lacking a hash. The `Verify`
  row action (completed rows with a stored hash, or failed rows
  after a mismatch) re-hashes the file on disk; if a repaired file
  now matches, the download flips back to completed. The same
  command is exposed as `verify_checksum(id)`. State and hashes
  persist in `downloads.db` (`expected_hash`, `computed_hash`,
  `verify_state` columns, added by migration — old DBs keep
  working) and in the JSON sidecar, so a resumed download keeps
  its verification record. Internals: `src-tauri/src/checksum.rs`
  (parsing, streaming hash, sidecar fetch) + the finalise step in
  `engine.rs` (hashing runs on a blocking thread, task lock
  released).
- **ClamAV scan** — after a download finishes (and its checksum
  passes, when one applies), ZDM scans the file with ClamAV in the
  background (`clamscan --no-summary <file>`, on a blocking thread,
  never holding the queue): exit 0 / "OK" shows "Clean" (lime
  check) under the status pill; exit 1 / "FOUND" marks the download
  failed with "Virus terdeteksi: \<threat name\>", keeps the file
  on disk, and sends a "Virus terdeteksi" desktop notification.
  Retry re-downloads and re-scans. With no `clamscan` on PATH the
  download stays completed and shows a grey dash — a download is
  never failed just for lacking a scanner — and Settings says
  "ClamAV tidak terpasang" (or the installed version when found).
  Any other scan failure (stale signatures, `clamscan` error) is
  also "unavailable", never failed. Settings toggle
  "Scan downloads with ClamAV" (default ON, old settings files
  load ON) gates only the automatic scan; the `Scan` row action
  (completed rows, or failed rows after a virus) and the
  `scan_download(id)` command always work, and a clean re-scan of
  a repaired file flips a virus-failed download back to completed.
  State and threat name persist in `downloads.db` (`scan_state`,
  `threat_name` columns, added by migration) and in the JSON
  sidecar. Internals: `src-tauri/src/scan.rs` (output parsing,
  scanner probe) + the finalise step in `engine.rs`. A real scan
  takes ~25 s while ClamAV loads its signature database, so the
  row shows "Scanning…" meanwhile. Install on Arch with
  `sudo pacman -S clamav && sudo freshclam` (or see
  `.taste-skill/e2e-clamav/install-clamav-userspace.sh` for a
  no-sudo install under `~/opt/clamav`).
- **Update checker** — ZDM checks a configurable remote manifest
  JSON for a newer version and offers it — never a fake update.
  Settings → "Update manifest URL" (empty by default, which means
  no source is configured and the checker honestly says so);
  "Check for updates" runs a manual check, and ZDM also checks
  once a day at startup. The manifest is
  `{"version": "0.2.0", "download_url": "https://…/ZDM_0.2.0_amd64.AppImage", "sha256": "<64 hex>", "changelog": "…"}` —
  see `updates/version.json` (an example, not a real release) and
  `updates/make-manifest.py` (generates a manifest from a build,
  computing the real SHA-256 of the asset). A newer version shows
  a banner above the Downloads grid (version, changelog,
  Download + Dismiss); Dismiss hides that version until a newer
  one appears. Download queues the asset through the normal engine,
  so it gets checksum verification (against the manifest SHA-256)
  + ClamAV + notifications like any download — a mismatched SHA
  fails it like a checksum mismatch, file kept, never installed.
  After the download finishes ZDM shows where the file is and the
  manual replacement steps: **ZDM never replaces its own running
  binary** (that is the later .deb-install round). A broken
  manifest, a same/older version, or an unreachable source never
  produces an offer — Settings shows the last check and its
  result as-is. Check state (last checked, dismissed version,
  last result) lives in `~/.config/zdm/update_state.json`,
  separate from settings.json so saving Settings can never wipe
  it. Publishing a manifest later is the user's call: host the
  manifest + asset anywhere over HTTPS — a GitHub release needs
  their login, any static file host works — then paste the
  manifest URL into Settings. There is no built-in release
  channel and no repo or account was created for this.
- **System tray** — core Tauri v2 `TrayIconBuilder` (Cargo features
  `tray-icon` + `image-png`, no plugin), icon from
  `src-tauri/icons/icon.png`. Left-click toggles the window;
  menu: Show / Hide ZDM, Add URL..., Start/Stop Queue (label
  follows the queue state), Pause All, Quit. Tooltip shows live
  state, refreshed every ~2 s ("ZDM — 2 downloading, 12.4 MB/s"
  or "ZDM — idle"). Settings "Minimize to tray" and "Close to
  tray" (both default ON): closing the window hides it to the
  tray instead of quitting — Quit only from the tray menu (or
  from the window close when Close-to-tray is OFF). If no tray
  host is available the tray is skipped with a warning on stderr
  and the app keeps running. Linux needs libayatana-appindicator
  at runtime (present on the laptop; `libayatana-appindicator3-dev`
  on this VM for the build).
- **Browser integration** — extensions for Chromium and Firefox
  (`browser/`) intercept http(s) downloads automatically: the
  browser download is cancelled and the URL is sent via the native
  host (`browser/zdm-native-host.py`, Python stdlib) to
  `~/Downloads/ZDM/queue.json`, which the app polls every ~2 s,
  dedupes by URL, and — with "Ask before downloading" ON — holds
  as PENDING for the confirmation popup instead of auto-starting
  (a shared lock file prevents a browser download landing
  mid-drain from being lost). Toolbar click toggles auto-intercept
  ON/OFF; right-click a link -> "Download with ZDM" works even
  when it is OFF. Install on the target machine with
  `./browser/install.sh`, then load the extension once per
  browser (Chromium: `chrome://extensions` -> Load unpacked;
  Firefox: `about:debugging` -> Load Temporary Add-on — see
  `browser/README.md` for the exact steps and troubleshooting).

## Layout

```
index.html                Vite entry (project root, Tauri template layout)
src/main.ts               UI: views, list rendering, segment strip, modals
src/styles.css            true-black-flat tokens + T-frame shell
src-tauri/src/lib.rs      Tauri commands + queue loop + queue-file poller
src-tauri/src/engine.rs   probe, RateLimiter, segments, task runner, Manager
src-tauri/src/db.rs       rusqlite registry (same schema as Python ZDM)
src-tauri/src/settings.rs settings.json (~/.config/zdm/settings.json)
src-tauri/src/grabber.rs  Site Grabber (media + link extraction, kinds, HEAD size probe)
src-tauri/src/stream.rs   HLS/DASH parsing + segment plan + ffmpeg merge (VOD only)
src-tauri/src/update.rs   update checker: manifest fetch, semver compare, state file
src-tauri/tests/download_integration.rs  local Range server, 5 MiB file + ffmpeg stream fixtures
browser/                  Chromium/Firefox extension + native host + install.sh
```

Tauri commands: `add_download`, `pause_download`, `resume_download`,
`cancel_download`, `delete_download`, `list_downloads`, `get_stats`,
`get_settings`, `save_settings`, `add_batch`, `grab_links`,
`start_queue`, `stop_queue`, `schedule_queue`, `get_ffmpeg_status`.

## Architecture mapping: Python → Rust

| Python (`download-manager-linux/`) | Rust (this app) |
|---|---|
| `engine.get_file_info()` (HEAD + Range GET fallback) | `engine::probe()` (reqwest/rustls) |
| `RateLimiter` (token bucket, threads) | `engine::RateLimiter` (`try_take` pure core + async `acquire`) |
| `DownloadTask._claim_segment()` (split largest, 128 KiB min) | `engine::claim_or_split()` (pure fn, unit tested) |
| `threading.Thread` workers + file lock | tokio tasks + `Arc<Mutex<std::fs::File>>` offset writes |
| JSON sidecar + `sqlite3` registry | same sidecar shape + `rusqlite` (bundled) |
| `DownloadManager` queue loop + `schedule()` | `Manager::queue_tick()` (500 ms) + `scheduled_at` |
| `categorize()` | `engine::categorize()` (same extension map) |
| `grabber.py` | `grabber::extract_links()` |
| Tkinter GUI | Tauri v2 + vanilla TypeScript frontend |

## Limitations vs the Python version

- No CLI yet (Tauri commands cover the same operations; a CLI can
  call the same engine later). The browser native-host stub now
  exists (`browser/`), unlike the earlier build.
- No clipboard monitor (Tauri window focus model; Add URL covers it).
- No FTP/MMS, no proprietary video-stream decryption (same as
  Python); HTTP/HTTPS only, Basic auth + proxies only (no
  NTLM/Kerberos).
- No per-connection speed estimation (splitting uses remaining size
  only, as in Python).

## Limitations vs IDM

Same as the Python ZDM: no Windows browser hooking / Advanced
Browser Integration, no licence/activation, no antivirus scan on
completion, no MMS/FTP, no NTLM/Negotiate/Kerberos.

## Tests

```bash
cd src-tauri
cargo test
```

48 tests, all passing (verified 2026-11-09) — now 122 tests
(95 unit + 27 integration), all passing (verified 2026-11-10,
update-checker round):

```
79 unit tests (cargo test --lib): segment splitting (largest split,
free-segment preference, no-split when too small), categorise(),
resume sidecar round-trip, speed-limiter math, auth/filename parsing,
settings normalisation, grabber extraction, SQLite round-trip,
queue-file parsing/dedupe + drain_queue_file add/dedupe (both ask
modes), pending add/dedupe, pending confirm (preset filename /
category / dest / connections honoured), pending dismiss, pending
probe (size filled, edited name kept), settings defaults for the
new toggles (ask_before_download, minimize_to_tray, close_to_tray)
and old settings files without them, checksum: Digest / Content-MD5
/ sidecar parsing (bare + `<hash>  <filename>`), streaming hash vs
known SHA-256/MD5 vectors, checksum-mismatch notification title,
startup-resume decision matrix (interrupted -> resume, manual
pause -> stays paused, schedule pause inside/outside window,
toggle OFF -> nothing, completed -> untouched, queued -> resume),
restore marks a stale `downloading` row as interrupted and reads
back the persisted scheduler-pause flag, scan: clamscan output
parsing (OK / FOUND + threat name / exit codes), scan-state
round-trip, fake-binary scan + scanner status (incl. the
transient-spawn retry), virus notification title
16 integration tests (local Range server, 5 MiB file):
  multi_segment_download_matches_hash   8 segments, SHA-256 matches
  pause_resume_download_matches_hash    pause, resume, SHA-256 matches
  no_range_server_falls_back_to_single_connection
                                        no hash source -> completed +
                                        verify_state unavailable
  unknown_size_chunked_download_completes
  pending_confirm_downloads_with_user_choices
                                        pending -> confirm with a
                                        renamed file + Programs
                                        folder -> SHA-256 matches
  digest_header_verifies_on_completion  Digest header -> verified;
                                        corrupt + repair on disk ->
                                        mismatch/failed, then
                                        verified/completed again
  digest_mismatch_fails_but_keeps_file  wrong Digest -> failed +
                                        file kept; retry fails again
  sidecar_hash_verifies_on_completion   <url>.sha256 -> verified
  user_supplied_hash_verifies           user SHA-256, no server
                                        header -> verified
  scheduled_pause_resumes_when_window_opens
                                        schedule pause -> resumes in
                                        window; manual pause stays
  startup_resume_continues_interrupted_download
                                        runner aborted mid-download
                                        (kill simulation) -> fresh
                                        Manager resumes from the
                                        saved offset, SHA-256 matches
  startup_resume_leaves_manual_pause_and_toggle_off_alone
                                        manual pause stays paused,
                                        toggle OFF resumes nothing
  clean_download_scans_clean            completed + scan_state
                                        clean, SHA-256 matches
  eicar_download_fails_but_keeps_file_and_rescan_flips_back
                                        EICAR -> failed + threat
                                        name + file kept; clean
                                        file + manual scan ->
                                        completed again
  scan_disabled_by_toggle_but_manual_scan_still_works
                                        toggle OFF -> no auto scan;
                                        manual scan still runs
```

Bugs found and fixed during testing:

- `save_settings` command collided with the imported
  `settings::save_settings` (E0255/E0277) — import renamed.
- `app.emit` needs `tauri::Emitter` in scope (E0599).
- Raw-string test HTML contained `"#`, ending the string early —
  switched to `r##"..."##`.
- Generated `icon.png` must be RGBA for `generate_context!`.
- Test HTTP server read each request once and could miss the
  `Range` header; it now reads until `\r\n\r\n`, and the engine
  discards the prefix when a server answers a Range request with
  `200` instead of `206` (this was corrupting resumed downloads).
- `cargo create-tauri-app` needs a real terminal in this sandbox and
  hung on its prompts; the scaffold was written manually from the
  official vanilla-ts template layout instead (same files, same
  versions) and `npm run build` passes.
- AppImage bundling aborted with `couldn't find a square icon to use
  as AppImage icon`: the default `bundle.icon` list was not picked
  up. Fixed by listing the icons explicitly in `tauri.conf.json`
  (`bundle.icon`: 32x32, 128x128, 128x128@2x, icon.png) — the next
  build produced all three bundles.

## Design lint

```bash
node ~/workspace/custom-skills/taste-skill/scripts/lint.mjs --app src
```

Result: **0 FAIL, 1 WARN** — the WARN is `padStart` in `main.ts`
(ETA clock formatting `mm:ss`, legitimate use).

## Hallmark redesign (2026-11-09)

The whole UI was redesigned with the Hallmark skill (`~/workspace/custom-skills/hallmark/`, v2.0.0), full redesign, app-screen scope. The composition (bento + timeline, sticky 224px sidebar, segment strip) is unchanged; the presentation is new.

- **Theme dealt:** Console 1/3 — dark, graphite ground in three lightness steps, Host Grotesk 500 for reading, Atkinson Hyperlegible Mono for measured figures. Brand lime #c6f24e kept as the one accent (same hue family as Console's amber; brand value read from the project's files wins). Console's cool blue is not used as a second signal.
- **Files:** `src/tokens.css` + `src/base.css` (scaffolded from the theme) hold all tokens; `src/styles.css` rewritten in the new system (same selectors, no data or behaviour removed); `src/main.ts` only changed 3 SVG chart stroke literals to token vars; `index.html` gained a meta description, curly quotes, and `rows="6"` on the batch textarea. Backend untouched — cargo test stays 82/82.
- **Sweep:** `node <skill>/scripts/sloplint.mjs index.html src/styles.css src/tokens.css src/base.css --genre modern-minimal --scope app --render --url http://localhost:4173/ --kept` → **0 FAIL, 0 REFLEX, 2 WARN** (F4 scrub-dot, F7 shared panel recipe — both dismissed with reasons in `.hallmark/log.json`). Before: 31 FAIL, 33 REFLEX, 10 WARN. Kept: fields 29/29, headings 4/4, figures 2/2. Full record in `.hallmark/log.json`, `design.md`, `.hallmark/after.txt`, `.hallmark/compare.html`; before/after screenshots in `.hallmark/before/` and `.hallmark/after/`.
- **Finish review (fresh context), verdict: fix** — 6 findings, all fixed in one batch and re-verified in the re-captured screenshots: (1) phone verify line now wraps inside the 96px status column instead of painting over the size line's 100%; (2) active filter pills/tabs/scrub days use the lime-tinted selection (accent-soft + accent text) so Add URL is the only ink-filled button; (3) numeric Settings panels hug their field (`.setting-panel--short`, 160px); (4) "Downloaded today" is one figure (4.2 GB) in every preview; (5) modal/grabber previews rebuilt from the current `preview.html` (current bento background, no phantom sidebar footer, SHA-256 section present); (6) batch textarea `rows="6"` so its placeholder is never cut mid-glyph on phone.
- **Build:** binary `868e80243b0573b4678235a695cd67a7fd5ced67e203b9fe746e092dc523d0f6`, AppImage `22c5fe4ad6eab234ac514cd68dd80894a45299cc092de5b75b31d914076eb259` — identical on the laptop, ran 15 s on `DISPLAY=:1` without crash.
- **Not yet observed:** the redesigned UI with live downloads in the real Tauri window (verified with sample rows in headless Chrome previews, not in the GUI).
