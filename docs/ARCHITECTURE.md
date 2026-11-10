# Architecture

ZDM is a Tauri v2 desktop app: a Rust backend does all downloading, and a Vite + TypeScript frontend renders it. The two sides talk through Tauri commands and events. Linux only; HTTP(S) only; there is no CLI (the only command-line flag is `--minimized`, used by autostart).

## Workspace layout

```
.
├── index.html               # Vite entry
├── package.json             # frontend deps (Vite, @tauri-apps/cli, @tauri-apps/api)
├── src/                     # frontend: main.ts, styles.css, tokens.css, base.css, icons.ts
├── src-tauri/
│   ├── Cargo.toml           # edition 2021, Tauri 2, tokio, reqwest (rustls), rusqlite
│   ├── tauri.conf.json      # app metadata, version, bundle targets (all: AppImage, .deb, .rpm)
│   ├── icons/               # app + tray icons
│   ├── src/                 # Rust backend (modules below)
│   └── tests/               # download_integration.rs — local Range server, no network needed
├── browser/                 # Chromium/Firefox extensions + Python native host + install.sh
├── updates/                 # version.json (release manifest), make-manifest.py
├── docs/                    # this file, screenshots
├── design.md                # locked design system (tokens, shell, signature)
└── zdm.desktop              # app-menu entry
```

`.taste-skill/` and `.hallmark/` hold development-time verification artefacts (lint/inspect reports, before/after screenshots, e2e scripts). They are not runtime code.

## Backend modules (`src-tauri/src/`)

| Module | Responsibility |
| --- | --- |
| `lib.rs` | Tauri commands, app setup, the 500 ms queue loop, the queue-file poller, system tray, pending-download confirmation flow |
| `engine.rs` | Download engine: probing, dynamic segmentation, workers, token-bucket speed limit, retry with backoff, categories, task lifecycle, stream downloads |
| `db.rs` | SQLite registry (`downloads.db`, in the download directory): insert, update, list, migrations |
| `settings.rs` | `settings.json` (`~/.config/zdm/`): fields, defaults, normalisation of old files |
| `grabber.rs` | Site Grabber: extract media (`<video>`, `<audio>`, `<source>`) and links from a page, with kinds and HEAD-probed sizes |
| `stream.rs` | HLS (`.m3u8`) and DASH (`.mpd`) parsing, segment plans, resume of segments, ffmpeg merge |
| `checksum.rs` | SHA-256 / MD5 streaming hash; expected-hash sources (typed hash, `Digest`/`Content-MD5` headers, `<url>.sha256`/`.md5` sidecar) |
| `scan.rs` | ClamAV: probe for `clamscan`, parse its output (`OK` / `FOUND` + threat name) |
| `schedule.rs` | Daily recurring queue window (start/stop, weekdays, overnight windows, next event) |
| `update.rs` | Update checker: manifest fetch and parse, strict semver compare, five states, once-a-day rule, `update_state.json` |
| `notify.rs` | Desktop notifications: per-file complete/failed, queue-drain summary, focus gating |
| `autostart.rs` | Start-on-login: installs/removes `~/.config/autostart/zdm.desktop` and the app-menu entry; `--minimized` detection |
| `main.rs` | Binary entry point |

Key invariants: segment offsets are positions, never accumulators — a re-downloaded byte is never counted twice (`writable_in_segment` clamps each chunk write to the segment's live end, and `downloaded` is clamped to `total` when persisted). A download's `downloaded` count must never exceed its `total`.

## Frontend (`src/`)

Vanilla TypeScript (strict) rendered into `index.html`, built with Vite. Three views — Downloads, Completed, Settings — plus the Add URL dialog (URL, optional expected SHA-256, batch input, Site Grabber) and the download-confirmation popup for browser-intercepted URLs. The UI polls `list_downloads` every 2 s and also listens for `download-progress` events (at most ~4 per second per download).

The design system is locked in [`design.md`](../design.md): dark minimal theme, Host Grotesk for reading, Atkinson Hyperlegible Mono for measured figures (speeds, sizes, ETAs, hashes), lime `#c6f24e` as the single accent, a 224 px sticky sidebar, and a per-download segment strip as the signature element. Tokens live in `src/tokens.css`, which is the source of truth; `src/icons.ts` is the single icon set.

## Download data flow

1. **Add** — a URL arrives from the Add URL dialog, the batch field, the browser extension (`~/Downloads/ZDM/queue.json`, polled every ~2 s and deduped by URL), or the Site Grabber. With "Ask before downloading" on, browser-intercepted URLs wait as PENDING for confirmation.
2. **Probe** — a HEAD (falling back to a Range GET) reads `Content-Length` and `Accept-Ranges` and picks the filename and category.
3. **Segment** — the file is split into N segments (default 8, configurable 1–32, minimum split 128 KiB). Each segment is fetched with its own `Range: bytes=a-b` connection into a preallocated `.part` file at the correct offset. Idle workers split the largest remaining segment. A server that answers a Range request with `200` instead of `206` has its already-known prefix discarded.
4. **Persist** — progress is written to the JSON sidecar (`<file>.download.json`) and to `downloads.db`, so a pause, crash, or kill resumes from the saved offsets. At most 3 downloads run concurrently by default.
5. **Finalise** — on completion the file is hashed (checksum step), then scanned with ClamAV when installed, then marked completed. A checksum mismatch or a virus detection fails the download but keeps the file on disk; a clean re-scan or a repaired file flips it back to completed.

Streams (`.m3u8` / `.mpd`) take a different path through `stream.rs`: the playlist is parsed, the best variant / adaptation sets are chosen, segments are downloaded in parallel with resume, and ffmpeg merges them into one `.mp4` (`.ts` for HLS without ffmpeg). AES-128 HLS works through ffmpeg. Live streams are rejected — video on demand only.

## Update channel

The checker fetches a manifest JSON — `{version, download_url, sha256, changelog}` — from the URL in Settings (default: the latest release of this repository, `https://github.com/zulian026/zdm/releases/latest/download/version.json`). It compares the manifest version against the running version with strict semver (including prereleases) and reports one of five states: `no_source`, `up_to_date`, `update_available`, `unreachable`, `invalid_manifest`. It checks once a day at startup and on demand ("Check for updates"). The fetcher follows redirects (reqwest default, up to 10), which is what makes the `releases/latest` URL work as a stable channel: a new release only needs a new `version.json` asset, no settings change.

An offered update downloads through the normal engine, so it gets checksum verification against the manifest SHA-256 (a mismatch fails like any checksum mismatch, file kept) and the usual notifications. ZDM never replaces its own running binary; after the download it shows the file location and the manual replacement steps. Check state (last checked, dismissed version, last result) persists in `~/.config/zdm/update_state.json`, separate from settings.
