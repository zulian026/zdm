import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ICONS, type IconName } from "./icons";

/* ---------- Types (backend API contract) ---------- */

type Status =
  | "queued"
  | "downloading"
  | "paused"
  | "completed"
  | "failed"
  | "cancelled";

interface DownloadSegment {
  start: number;
  end: number;
  pos: number;
  done: boolean;
  assigned: boolean;
}

interface DownloadView {
  id: string;
  url: string;
  filename: string;
  dest_path: string;
  total: number | null;
  downloaded: number;
  status: Status;
  category: string;
  created: string | number;
  speed_bps: number;
  eta_secs: number | null;
  error: string | null;
  segments: DownloadSegment[];
  // Checksum verification: "unavailable" | "verified" | "mismatch".
  verify_state: string;
  computed_hash: string | null;
  expected_hash: string | null;
  // ClamAV scan: "unavailable" | "scanning" | "clean" | "found".
  scan_state: string;
  threat_name: string | null;
  // Merged stream: "hls" | "dash" | null, with the segment counter.
  stream_kind: string | null;
  segments_done: number;
  segments_total: number;
  // Honest note when a stream could not be merged (DASH without
  // ffmpeg: only the playlist was downloaded).
  stream_note: string | null;
}

interface Stats {
  active: number;
  total_speed_bps: number;
  downloaded_today: number;
  queued: number;
  completed_total: number;
}

interface DailySchedule {
  enabled: boolean;
  start: string;
  stop: string;
  days: boolean[];
}

interface ScheduleStatus {
  enabled: boolean;
  in_window: boolean;
  next_label: string | null;
}

interface Settings {
  connections: number;
  max_concurrent: number;
  download_dir: string;
  speed_limit_kbps: number;
  proxy: string;
  ask_before_download: boolean;
  minimize_to_tray: boolean;
  close_to_tray: boolean;
  desktop_notifications: boolean;
  autostart: boolean;
  resume_on_startup: boolean;
  clamav_scan: boolean;
  update_manifest_url: string;
  schedule: DailySchedule;
}

interface UpdateManifest {
  version: string;
  download_url: string;
  sha256: string | null;
  changelog: string | null;
}

type UpdateStateName =
  | "no_source"
  | "up_to_date"
  | "update_available"
  | "unreachable"
  | "invalid_manifest";

interface UpdateCheck {
  state: UpdateStateName;
  current_version: string;
  manifest: UpdateManifest | null;
  source_url: string;
  checked_at: string;
  message: string;
}

interface UpdateStatusView {
  check: UpdateCheck | null;
  dismissed_version: string | null;
}

interface PendingDownload {
  id: string;
  url: string;
  filename: string;
  total: number | null;
  mime: string | null;
  category: string;
  dest_dir: string;
  connections: number;
  created: number;
}

interface ConfirmOptions {
  filename: string;
  dest_dir: string;
  category: string;
  connections: number;
  start_now: boolean;
  expected_sha256: string | null;
}

interface GrabItem {
  url: string;
  kind: "video" | "audio" | "manifest" | "link";
  size: number | null;
}

interface ProgressPayload {
  id: string;
  filename: string;
  downloaded: number;
  total: number | null;
  speed_bps: number;
  eta_secs: number | null;
  status: Status;
}

type ViewName = "downloads" | "completed" | "settings";
type TabName = "all" | "downloading" | "completed" | "failed";
type SortName = "newest" | "fastest" | "largest";

/* ---------- State ---------- */

let downloads: DownloadView[] = [];
let stats: Stats | null = null;
let settings: Settings | null = null;
// Browser-intercepted downloads waiting for confirmation. The modal
// shows the first one; the rest wait in this list (in-memory only —
// they are dropped if the app closes).
let pendings: PendingDownload[] = [];
let currentPending: PendingDownload | null = null;
let view: ViewName = "downloads";
let tab: TabName = "all";
let sort: SortName = "newest";
let query = "";
// Header category pills (Downloads view): "all" or one category from
// the data itself. Completed has its own category dropdown instead.
let dlCategory = "all";
let pollTimer: number | undefined;
// Live speed graph: aggregate total speed sampled every second, kept
// in memory only (last 5 minutes, ring buffer, never persisted).
let speedHistory: { t: number; v: number }[] = [];
let speedTimer: number | undefined;
// History filters (Completed view): category from the data itself, plus
// a date window. Both combine with the text search.
let historyCategory = "all";
type HistoryRange = "all" | "today" | "week";
let historyRange: HistoryRange = "all";
// One day picked on the Completed scrubber ("2026-11-09", local date).
// Cleared whenever the category or range filter changes.
let historyDay: string | null = null;

/* ---------- Helpers ---------- */

function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) throw new Error(`Missing element: ${id}`);
  return el;
}

function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function fmtBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return u === 0 ? `${Math.round(v)} B` : `${v.toFixed(1)} ${units[u]}`;
}

function fmtSpeed(bps: number): string {
  if (!Number.isFinite(bps) || bps <= 0) return "0 KB/s";
  if (bps < 1024) return `${Math.round(bps)} B/s`;
  if (bps < 1024 * 1024) return `${(bps / 1024).toFixed(1)} KB/s`;
  return `${(bps / (1024 * 1024)).toFixed(1)} MB/s`;
}

// Split "20.2 MB/s" into ["20.2", "MB/s"] so the unit renders as a small
// grey suffix while the figure stays tabular and right-aligned.
function splitUnit(s: string): [string, string] {
  const i = s.lastIndexOf(" ");
  return i > 0 ? [s.slice(0, i), s.slice(i + 1)] : [s, ""];
}

function fmtEta(secs: number | null): string {
  if (secs === null || !Number.isFinite(secs) || secs < 0) return "-";
  const s = Math.round(secs);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const r = s % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${pad(h)}:${pad(m)}:${pad(r)}` : `${pad(m)}:${pad(r)}`;
}

function progressOf(d: DownloadView): number {
  if (!d.total || d.total <= 0) return 0;
  return Math.min(1, Math.max(0, d.downloaded / d.total));
}

// Shorten a URL for display: drop the scheme, cut long paths with an
// ellipsis. The full URL stays in the title attribute. Editorial shortening
// (app-screens §7.4) also keeps the text node's range inside the row on a
// phone, where an ellipsis-clipped URL still measures past the screen edge.
function shortUrl(url: string): string {
  const bare = url.replace(/^https?:\/\//, "");
  return bare.length > 44 ? `${bare.slice(0, 42)}…` : bare;
}

function statusLabel(s: Status): string {
  switch (s) {
    case "queued":
      return "Queued";
    case "downloading":
      return "Downloading";
    case "paused":
      return "Paused";
    case "completed":
      return "Completed";
    case "failed":
      return "Failed";
    case "cancelled":
      return "Cancelled";
  }
}

function createdKey(d: DownloadView): number {
  const n = Number(d.created);
  if (Number.isFinite(n) && n > 0) return n;
  const t = Date.parse(String(d.created));
  return Number.isFinite(t) ? t : 0;
}

// created as milliseconds since epoch. The backend stores RFC3339 text
// (Date.parse gives ms); a bare unix-seconds number is scaled up. 0
// means unknown — never "today" or "this week".
function createdMs(d: DownloadView): number {
  const k = createdKey(d);
  if (k > 0 && k < 1e12) return k * 1000;
  return k;
}

/* ---------- Day grouping (timeline) ---------- */

// Local "YYYY-MM-DD" for a timestamp; the grouping key for one day.
function dayKey(ms: number): string {
  if (!ms || ms <= 0) return "unknown";
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

// The label on a timeline node: Today / Yesterday, else "Mon 9 Nov".
// 0 (unknown) groups under "Unknown date", last, never as Today.
function dayLabel(ms: number): string {
  if (!ms || ms <= 0) return "Unknown date";
  const d = new Date(ms);
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const day = new Date(d);
  day.setHours(0, 0, 0, 0);
  const diffDays = Math.round(
    (today.getTime() - day.getTime()) / 86400000,
  );
  if (diffDays === 0) return "Today";
  if (diffDays === 1) return "Yesterday";
  const weekday = d.toLocaleDateString("en-US", { weekday: "short" });
  const month = d.toLocaleDateString("en-US", { month: "short" });
  return `${weekday} ${d.getDate()} ${month}`;
}

// "Mon 9 Nov" short form for a scrubber button (no Today/Yesterday:
// the scrubber lists days, the timeline names the recent ones).
function dayShort(ms: number): string {
  if (!ms || ms <= 0) return "Unknown";
  const d = new Date(ms);
  const month = d.toLocaleDateString("en-US", { month: "short" });
  return `${d.getDate()} ${month}`;
}

interface DayGroup {
  key: string;
  label: string;
  items: DownloadView[];
}

// Newest day first; the unknown-date group (if any) goes last.
function groupByDay(items: DownloadView[]): DayGroup[] {
  const map = new Map<string, DownloadView[]>();
  for (const d of items) {
    const key = dayKey(createdMs(d));
    const list = map.get(key);
    if (list) list.push(d);
    else map.set(key, [d]);
  }
  const groups: DayGroup[] = [...map.entries()].map(([key, list]) => ({
    key,
    label: dayLabel(createdMs(list[0])),
    items: list,
  }));
  groups.sort((a, b) => {
    if (a.key === "unknown") return 1;
    if (b.key === "unknown") return -1;
    return b.key.localeCompare(a.key);
  });
  return groups;
}

/* ---------- Backend calls ---------- */

async function listDownloads(): Promise<DownloadView[]> {
  return invoke<DownloadView[]>("list_downloads");
}

async function getStats(): Promise<Stats> {
  return invoke<Stats>("get_stats");
}

async function getSettings(): Promise<Settings> {
  return invoke<Settings>("get_settings");
}

/* ---------- Filtering / sorting ---------- */

function filteredDownloads(onlyCompleted: boolean): DownloadView[] {
  const q = query.trim().toLowerCase();
  let items = downloads.filter((d) => {
    if (onlyCompleted) return d.status === "completed";
    if (tab === "downloading") return d.status === "downloading";
    if (tab === "completed") return d.status === "completed";
    if (tab === "failed") return d.status === "failed";
    return true;
  });
  if (!onlyCompleted && dlCategory !== "all") {
    items = items.filter((d) => d.category === dlCategory);
  }
  if (onlyCompleted) {
    // History filters combine with the text search below.
    if (historyCategory !== "all") {
      items = items.filter((d) => d.category === historyCategory);
    }
    if (historyDay !== null) {
      items = items.filter((d) => dayKey(createdMs(d)) === historyDay);
    }
    if (historyRange !== "all") {
      const startOfToday = new Date();
      startOfToday.setHours(0, 0, 0, 0);
      let from = startOfToday.getTime();
      if (historyRange === "week") {
        // This calendar week, Monday 00:00 local time.
        const monday = new Date(startOfToday);
        monday.setDate(monday.getDate() - ((monday.getDay() + 6) % 7));
        from = monday.getTime();
      }
      items = items.filter((d) => createdMs(d) >= from);
    }
  }
  if (q) {
    items = items.filter(
      (d) =>
        d.filename.toLowerCase().includes(q) ||
        d.url.toLowerCase().includes(q),
    );
  }
  const sorted = [...items];
  if (sort === "newest") sorted.sort((a, b) => createdKey(b) - createdKey(a));
  else if (sort === "fastest")
    sorted.sort((a, b) => b.speed_bps - a.speed_bps);
  else
    sorted.sort(
      (a, b) => (b.total ?? -1) - (a.total ?? -1),
    );
  return sorted;
}

/* ---------- Rendering ---------- */

function segmentStrip(d: DownloadView): string {
  if (!d.segments || d.segments.length === 0) {
    // Streams: one block per segment (capped at 32), filled in
    // download order — the same strip, driven by segment counts.
    if (!d.stream_kind || d.segments_total === 0) return "";
    const total = Math.min(d.segments_total, 32);
    const done = Math.round((d.segments_done / d.segments_total) * total);
    const blocks = Array.from({ length: total }, (_, i) => {
      let cls = "";
      if (i < done) cls = "is-done";
      else if (d.status === "failed") cls = "is-failed";
      return `<span class="seg ${cls}"></span>`;
    });
    return `<div class="dl-segments" aria-hidden="true">${blocks.join("")}</div>`;
  }
  const blocks = d.segments.slice(0, 32).map((s) => {
    let cls = "";
    if (s.done) cls = "is-done";
    else if (d.status === "failed") cls = "is-failed";
    else if (s.assigned) cls = "is-active";
    return `<span class="seg ${cls}"></span>`;
  });
  return `<div class="dl-segments" aria-hidden="true">${blocks.join("")}</div>`;
}

function rowActions(d: DownloadView): string {
  const btns: string[] = [];
  // Icon + text, never icon-only: the label is the action.
  if (d.status === "downloading") {
    btns.push(`<button class="btn btn-text" data-action="pause" data-id="${esc(d.id)}" type="button">${ICONS.pause}Pause</button>`);
  }
  if (d.status === "paused") {
    btns.push(`<button class="btn btn-text" data-action="resume" data-id="${esc(d.id)}" type="button">${ICONS.play}Resume</button>`);
  }
  if (d.status === "downloading" || d.status === "paused" || d.status === "queued") {
    btns.push(`<button class="btn btn-text" data-action="cancel" data-id="${esc(d.id)}" type="button">${ICONS.x}Cancel</button>`);
  }
  if (d.status === "completed" || d.status === "failed" || d.status === "cancelled") {
    btns.push(`<button class="btn btn-text btn-danger" data-action="delete" data-id="${esc(d.id)}" type="button">${ICONS.trash}Delete</button>`);
  }
  // Re-verify a finished file against its stored expected hash:
  // completed rows that carry one, and failed rows whose failure
  // was a checksum mismatch (the file may have been fixed on disk).
  if (
    (d.status === "completed" && d.expected_hash) ||
    (d.status === "failed" && d.verify_state === "mismatch")
  ) {
    btns.push(`<button class="btn btn-text" data-action="verify" data-id="${esc(d.id)}" type="button">${ICONS.checkCircle}Verify</button>`);
  }
  // Re-scan a finished file with ClamAV: completed rows, and failed
  // rows whose failure was a virus (the file may have been cleaned
  // on disk since).
  if (
    d.status === "completed" ||
    (d.status === "failed" && d.scan_state === "found")
  ) {
    btns.push(`<button class="btn btn-text" data-action="scan" data-id="${esc(d.id)}" type="button">${ICONS.search}Scan</button>`);
  }
  return btns.join("");
}

function verifyLine(d: DownloadView): string {
  // One line under the status pill, only when it says something:
  // verified (lime check + first 12 hex, full hash in the tooltip),
  // mismatch (red), and a grey dash for completed downloads with
  // no hash source at all.
  if (d.verify_state === "verified" && d.computed_hash) {
    return `<div class="verify verify-ok" title="SHA-256 ${esc(d.computed_hash)}">${ICONS.checkCircle}Verified · ${esc(d.computed_hash.slice(0, 12))}</div>`;
  }
  if (d.verify_state === "mismatch") {
    return `<div class="verify verify-bad">Checksum mismatch</div>`;
  }
  if (d.status === "completed") {
    return `<div class="verify verify-none">Checksum —</div>`;
  }
  return "";
}

function scanLine(d: DownloadView): string {
  // One line under the verify line, only when it says something:
  // clean (lime check), found (red, threat name), scanning (grey,
  // the detached scan is still running), and a grey dash for
  // completed downloads with no scanner at all.
  if (d.scan_state === "clean") {
    return `<div class="verify verify-ok" title="Scanned with ClamAV">${ICONS.checkCircle}Clean</div>`;
  }
  if (d.scan_state === "found") {
    const full = d.threat_name ? esc(d.threat_name) : "Virus";
    const short = full.length > 24 ? `${full.slice(0, 24)}…` : full;
    return `<div class="verify verify-bad" title="Virus terdeteksi: ${full}">${ICONS.x}Virus: ${short}</div>`;
  }
  if (d.scan_state === "scanning") {
    return `<div class="verify verify-none">Scanning…</div>`;
  }
  if (d.status === "completed") {
    return `<div class="verify verify-none">Scan —</div>`;
  }
  return "";
}

function streamLine(d: DownloadView): string {
  // One line under the scan line, only for streams: the segment
  // counter while it runs, and the merged result when it is done. A
  // failed stream shows its reason in the error line instead; the
  // honest no-merge note has its own full-width footer row (see
  // downloadRow) because the status column is too narrow for it.
  if (d.stream_note) return "";
  if (!d.stream_kind) return "";
  const kind = d.stream_kind.toUpperCase();
  if (d.status === "completed") {
    return `<div class="verify verify-ok" title="Stream ${kind} digabung menjadi satu video">${ICONS.checkCircle}${kind} · video digabung</div>`;
  }
  if (d.status === "failed") return "";
  if (d.segments_total > 0) {
    return `<div class="verify verify-none">${kind} · ${d.segments_done}/${d.segments_total} segmen</div>`;
  }
  return `<div class="verify verify-none">${kind} · stream</div>`;
}

function downloadRow(d: DownloadView): string {
  const pct = progressOf(d);
  // Two measures stack in one cell: downloaded on line 1, total + percent
  // in 12px grey on line 2 — never a wrapped "3.6 GB / 5.7 GB".
  const [dlNum, dlUnit] = splitUnit(fmtBytes(d.downloaded));
  const size = d.total
    ? `${dlNum} <span class="unit">${dlUnit}</span><span class="dl-sub">of ${fmtBytes(d.total)} · ${Math.round(pct * 100)}%</span>`
    : `${dlNum} <span class="unit">${dlUnit}</span>`;
  // Speed is not applicable off-download: a grey dash, not "0 KB/s".
  const [spNum, spUnit] =
    d.status === "downloading" ? splitUnit(fmtSpeed(d.speed_bps)) : ["-", ""];
  const speed =
    d.status === "downloading"
      ? `${spNum} <span class="unit">${spUnit}</span>`
      : "-";
  const eta =
    d.status === "downloading" ? fmtEta(d.eta_secs) : "-";
  const error = d.error
    ? `<div class="dl-error">${esc(d.error)}</div>`
    : "";
  // The phone shows the row's first action under the identity; the desktop
  // action cell is hidden there. Both carry the same data-action.
  const phoneAction = rowActions(d).match(/<button[^>]*>.*?<\/button>/)?.[0] ?? "";
  return `
    <div class="dl-row" data-id="${esc(d.id)}">
      <div class="dl-identity">
        <div class="dl-filename">${esc(d.filename || d.url)}</div>
        <div class="dl-url" title="${esc(d.url)}">${esc(shortUrl(d.url))}</div>
        ${segmentStrip(d)}
        <div class="dl-progress"><div class="dl-progress-fill" style="width: ${(pct * 100).toFixed(1)}%"></div></div>
        ${phoneAction ? `<span class="dl-phone-action">${phoneAction}</span>` : ""}
        ${error}
      </div>
      <div class="dl-num dl-col-size">${size}</div>
      <div class="dl-num dl-col-speed">${speed}</div>
      <div class="dl-num dl-col-eta">${eta}</div>
      <div class="dl-status"><span class="pill pill-${d.status}">${statusLabel(d.status)}</span>${verifyLine(d)}${scanLine(d)}${streamLine(d)}</div>
      <div class="dl-actions dl-col-actions">${rowActions(d)}</div>
      ${d.stream_note ? `<div class="dl-stream-note">${esc(d.stream_note)}</div>` : ""}
    </div>`;
}

function tableHead(): string {
  // Offset by the rail (28px) + card padding (20px) so the columns
  // line up with the columns inside the timeline cards.
  return `
    <div class="dl-table-head timeline-head">
      <div>File</div>
      <div class="num">Size</div>
      <div class="num dl-col-speed">Speed</div>
      <div class="num dl-col-eta">ETA</div>
      <div class="num">Status</div>
      <div></div>
    </div>`;
}

// The queue as a timeline: one group per day, newest first, each with
// a node on the line and its downloads as cards hanging off the rail.
// `prefix` namespaces the group anchors per list ("d" Downloads, "c"
// Completed) so the scrubber can jump to the right one.
function renderList(container: HTMLElement, items: DownloadView[], emptyText: string, prefix: string): void {
  if (items.length === 0) {
    // The empty queue keeps its frame and says what fills it: one sentence
    // plus the three real ways to add a download. `emptyText` is plain text.
    const ways =
      container.id === "download-list"
        ? `<ul class="empty-ways"><li>Add a URL with the Add URL button</li><li>Download a file in your browser, ZDM intercepts it</li><li>Grab links from a page with Site Grabber</li></ul>`
        : "";
    container.innerHTML = `<div class="empty-state"><div class="empty-icon">${ICONS.download}</div><p>${esc(emptyText)}</p>${ways}</div>`;
    return;
  }
  const groups = groupByDay(items);
  container.innerHTML =
    tableHead() +
    groups
      .map(
        (g) => `
    <div class="tl-group" id="tl-${prefix}-${g.key}">
      <div class="tl-head">
        <span class="tl-node" aria-hidden="true"></span>
        <span class="tl-date">${esc(g.label)}</span>
        <span class="tl-count">${g.items.length} download${g.items.length === 1 ? "" : "s"}</span>
        <span class="tl-rule" aria-hidden="true"></span>
      </div>
      ${g.items.map((d) => `<div class="tl-card">${downloadRow(d)}</div>`).join("")}
    </div>`,
      )
      .join("");
}

function kpiValue(id: string, text: string): void {
  const [num, unit] = splitUnit(text);
  $(id).innerHTML = unit
    ? `${esc(num)} <span class="kpi-unit">${esc(unit)}</span>`
    : esc(num);
}

function renderKpis(): void {
  if (!stats) return;
  // Today panel: the live figures as type, from the same stats object
  // as the speed panel. Now lives only in the chart card (one figure,
  // one place); this panel carries Downloaded today, Active, Queued
  // and the queue state.
  ($("vit-queue").textContent =
    stats.active > 0 ? "Downloading" : stats.queued > 0 ? "Waiting" : "Idle");
  ($("vit-active").textContent = String(stats.active));
  ($("vit-queued").textContent = String(stats.queued));
  kpiValue("vit-today", fmtBytes(stats.downloaded_today));
  ($("usage-active").textContent = String(stats.active));
  kpiValue("usage-today", fmtBytes(stats.downloaded_today));
  ($("usage-completed").textContent = String(stats.completed_total));
  const active = stats.active > 0;
  $("topbar-status").textContent = active
    ? `${stats.active} active · ${fmtSpeed(stats.total_speed_bps)}`
    : stats.queued > 0
      ? `${stats.queued} queued`
      : "";
}

/* ---------- Live speed graph ---------- */

const SPEED_WINDOW_MS = 5 * 60 * 1000;
const SPEED_MAX_SAMPLES = 300;

// Round step for the y scale: 1, 2 or 5 times a power of ten, so ticks
// are always round and even (app-screens §6.3).
function niceStep(raw: number): number {
  if (!(raw > 0)) return 1;
  const pow = Math.pow(10, Math.floor(Math.log10(raw)));
  const n = raw / pow;
  if (n <= 1) return pow;
  if (n <= 2) return 2 * pow;
  if (n <= 5) return 5 * pow;
  return 10 * pow;
}

function fmtTick(v: number): string {
  return Number.isInteger(v) ? String(v) : v.toFixed(1);
}

function recordSpeedSample(): void {
  const now = Date.now();
  speedHistory.push({ t: now, v: stats?.total_speed_bps ?? 0 });
  const cutoff = now - SPEED_WINDOW_MS;
  while (speedHistory.length > 0 && speedHistory[0].t < cutoff) {
    speedHistory.shift();
  }
  if (speedHistory.length > SPEED_MAX_SAMPLES) {
    speedHistory.splice(0, speedHistory.length - SPEED_MAX_SAMPLES);
  }
  renderSpeedChart();
}

function renderSpeedChart(): void {
  const host = $("speed-chart");
  const width = host.clientWidth || 640;
  const height = 180;
  const padL = 28;
  const padR = 8;
  const padT = 6;
  const padB = 20;
  const plotW = width - padL - padR;
  const plotH = height - padT - padB;
  const now = Date.now();
  const windowStart = now - SPEED_WINDOW_MS;

  // The unit follows the largest value in the window; the card header
  // names it, so the tick labels stay plain numbers.
  const maxV = speedHistory.reduce((m, s) => Math.max(m, s.v), 0);
  let divisor = 1;
  let unit = "B/s";
  if (maxV >= 1024 * 1024) {
    divisor = 1024 * 1024;
    unit = "MB/s";
  } else if (maxV >= 1024) {
    divisor = 1024;
    unit = "KB/s";
  }
  const maxUnit = maxV / divisor;
  // Flat history (all zero) still gets a scale, 0-4 KB/s, with the line
  // on zero: an honest flat line, never a "No data" picture.
  const step = maxUnit > 0 ? niceStep(maxUnit / 5) : 1;
  const top = maxUnit > 0 ? Math.ceil(maxUnit / step) * step : 4 * step;
  const ticks: number[] = [];
  for (let v = 0; v <= top + 1e-9; v += step) ticks.push(v);

  const y = (v: number) => padT + plotH - (v / top) * plotH;
  const x = (t: number) =>
    padL + ((t - windowStart) / SPEED_WINDOW_MS) * plotW;
  const parts: string[] = [];
  for (const v of ticks) {
    if (v !== 0) {
      parts.push(
        `<line x1="${padL}" x2="${padL + plotW}" y1="${y(v).toFixed(1)}" y2="${y(v).toFixed(1)}" stroke="var(--color-rule)" stroke-width="1"/>`,
      );
    }
    parts.push(
      `<text x="${padL - 8}" y="${(y(v) + 4).toFixed(1)}" text-anchor="end">${fmtTick(v)}</text>`,
    );
  }
  // Baseline: the one heavier line.
  parts.push(
    `<line x1="${padL}" x2="${padL + plotW}" y1="${y(0).toFixed(1)}" y2="${y(0).toFixed(1)}" stroke="var(--color-rule-2)" stroke-width="2"/>`,
  );
  // X labels: -5m … -1m, now. First pulled inside on the left, last on
  // the right, the rest centred — flat, never rotated.
  for (let i = 0; i <= 5; i++) {
    const t = windowStart + i * 60_000;
    const label = i === 5 ? "now" : `-${5 - i}m`;
    const anchor = i === 0 ? "start" : i === 5 ? "end" : "middle";
    const lx = i === 0 ? padL : i === 5 ? padL + plotW : x(t);
    parts.push(
      `<text x="${lx.toFixed(1)}" y="${height - 6}" text-anchor="${anchor}">${label}</text>`,
    );
  }
  // One series, straight segments, no smoothing, no point markers.
  // Fewer than two samples: grid and baseline stand alone — the "Now"
  // figure carries the number until the line has a second point.
  if (speedHistory.length >= 2) {
    const pts = speedHistory
      .map((s) => `${x(s.t).toFixed(1)},${y(s.v / divisor).toFixed(1)}`)
      .join(" ");
    parts.push(
      `<polyline points="${pts}" fill="none" stroke="var(--color-accent)" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>`,
    );
  }
  host.innerHTML = `<svg viewBox="0 0 ${width} ${height}" width="${width}" height="${height}" role="img" aria-label="Total download speed over the last 5 minutes">${parts.join("")}</svg>`;

  // Side block: the chart card's Now is the screen's only copy of
  // total speed (one figure, one place). Delta: Now against the
  // 5-minute average, signed, with its baseline named. Faster than
  // average is good (lime), slower is bad (red); no change is a
  // faint dash.
  $("speed-unit").textContent = unit;
  const nowBps = stats?.total_speed_bps ?? 0;
  kpiValue("speed-now", fmtSpeed(nowBps));
  const peak = speedHistory.reduce((m, s) => Math.max(m, s.v), 0);
  kpiValue("speed-peak", fmtSpeed(peak));
  const avg =
    speedHistory.length > 0
      ? speedHistory.reduce((a, s) => a + s.v, 0) / speedHistory.length
      : 0;
  kpiValue("speed-average", fmtSpeed(avg));
  const deltaEl = $("speed-delta");
  if (speedHistory.length < 2 || (nowBps === 0 && avg === 0)) {
    deltaEl.textContent = "No change vs 5-min average";
    deltaEl.className = "speed-delta";
  } else if (nowBps > avg) {
    deltaEl.textContent = `+${fmtSpeed(nowBps - avg)} vs 5-min average`;
    deltaEl.className = "speed-delta delta-up";
  } else if (nowBps < avg) {
    deltaEl.textContent = `-${fmtSpeed(avg - nowBps)} vs 5-min average`;
    deltaEl.className = "speed-delta delta-down";
  } else {
    deltaEl.textContent = "±0 vs 5-min average";
    deltaEl.className = "speed-delta";
  }
  ($("speed-note") as HTMLElement).hidden = !(
    stats !== null && stats.active === 0
  );
}

function renderDownloadsView(): void {
  rebuildCategoryPills();
  const items = filteredDownloads(false);
  renderList($("download-list"), items, "No downloads yet. Add a URL to get started.", "d");
  $("count").textContent = `${items.length} download${items.length === 1 ? "" : "s"}`;
  renderScrubber($("scrubber"), items, "downloads");
}

// Category pills come from the data itself: only categories that have
// a download are offered. Rebuilt only when the list changes, so an
// open state is never clobbered by the 2 s poll.
let categoryPillOptions = "";
function rebuildCategoryPills(): void {
  const cats = [...new Set(downloads.filter((d) => d.category).map((d) => d.category))].sort((a, b) =>
    a.localeCompare(b),
  );
  const key = cats.join("\n");
  if (key === categoryPillOptions) return;
  categoryPillOptions = key;
  const box = $("cat-pills");
  box.innerHTML =
    `<button class="cat-pill${dlCategory === "all" ? " is-active" : ""}" data-cat="all" type="button">All</button>` +
    cats
      .map(
        (c) =>
          `<button class="cat-pill${dlCategory === c ? " is-active" : ""}" data-cat="${esc(c)}" type="button">${esc(c)}</button>`,
      )
      .join("");
  if (!cats.includes(dlCategory)) dlCategory = "all";
}

// The scrubber: one button per day in the data (dot, day, count).
// On Downloads it scrolls to that day's group; on Completed it filters
// the history to that day. "All" goes back to the top / every day.
function renderScrubber(container: HTMLElement, items: DownloadView[], mode: "downloads" | "completed"): void {
  const groups = groupByDay(items);
  if (groups.length === 0) {
    container.hidden = true;
    container.innerHTML = "";
    return;
  }
  container.hidden = false;
  const activeDay = mode === "completed" ? historyDay : null;
  container.innerHTML =
    `<button class="scrub-btn${activeDay === null && mode === "completed" ? " is-active" : ""}" data-day="all" data-mode="${mode}" type="button">All</button>` +
    groups
      .map((g) => {
        const firstMs = createdMs(g.items[0]);
        return `<button class="scrub-btn${activeDay === g.key ? " is-active" : ""}" data-day="${esc(g.key)}" data-mode="${mode}" type="button"><span class="scrub-dot" aria-hidden="true"></span>${esc(dayShort(firstMs))}<span class="scrub-count">${g.items.length}</span></button>`;
      })
      .join("");
}

// Category options come from the data itself: only categories that have
// a completed download are offered. Rebuilt only when the list changes,
// so an open dropdown is never clobbered by the 2 s poll.
let historyCategoryOptions = "";
function rebuildHistoryCategoryOptions(): void {
  const cats = [
    ...new Set(
      downloads
        .filter((d) => d.status === "completed" && d.category)
        .map((d) => d.category),
    ),
  ].sort((a, b) => a.localeCompare(b));
  const key = cats.join("\n");
  if (key === historyCategoryOptions) return;
  historyCategoryOptions = key;
  const sel = $("history-category") as HTMLSelectElement;
  sel.innerHTML =
    `<option value="all">All categories</option>` +
    cats
      .map((c) => `<option value="${esc(c)}">${esc(c)}</option>`)
      .join("");
  sel.value = cats.includes(historyCategory) ? historyCategory : "all";
  historyCategory = sel.value;
}

function renderCompletedView(): void {
  rebuildHistoryCategoryOptions();
  const items = filteredDownloads(true);
  const filtersActive =
    query.trim() !== "" || historyCategory !== "all" || historyRange !== "all" || historyDay !== null;
  renderList(
    $("completed-list"),
    items,
    filtersActive
      ? "No completed downloads match these filters."
      : "No completed downloads yet.",
    "c",
  );
  $("completed-count").textContent = `${items.length} completed`;
  $("completed-hint").textContent = settings?.download_dir
    ? `Files are saved to ${settings.download_dir}.`
    : "Finished downloads appear here.";
  // Scrubber counts come from the items before the day filter, so
  // every day stays clickable while one day is picked.
  const savedDay = historyDay;
  historyDay = null;
  const base = filteredDownloads(true);
  historyDay = savedDay;
  renderScrubber($("completed-scrubber"), base, "completed");
}

function renderAll(): void {
  renderKpis();
  renderSpeedChart();
  if (view === "downloads") renderDownloadsView();
  else if (view === "completed") renderCompletedView();
}

/* ---------- In-place progress updates ---------- */

function applyProgress(p: ProgressPayload): void {
  const d = downloads.find((x) => x.id === p.id);
  if (d) {
    d.downloaded = p.downloaded;
    d.total = p.total;
    d.speed_bps = p.speed_bps;
    d.eta_secs = p.eta_secs;
    d.status = p.status;
  }
  const row = document.querySelector<HTMLElement>(`.dl-row[data-id="${CSS.escape(p.id)}"]`);
  if (!row) return;
  const pct = p.total && p.total > 0 ? Math.min(1, Math.max(0, p.downloaded / p.total)) : 0;
  const fill = row.querySelector<HTMLElement>(".dl-progress-fill");
  if (fill) fill.style.width = `${(pct * 100).toFixed(1)}%`;
  const sizeCell = row.querySelector<HTMLElement>(".dl-col-size");
  if (sizeCell) {
    const [dlNum, dlUnit] = splitUnit(fmtBytes(p.downloaded));
    sizeCell.innerHTML = p.total
      ? `${dlNum} <span class="unit">${dlUnit}</span><span class="dl-sub">of ${fmtBytes(p.total)} · ${Math.round(pct * 100)}%</span>`
      : `${dlNum} <span class="unit">${dlUnit}</span>`;
  }
  const speedCell = row.querySelector<HTMLElement>(".dl-col-speed");
  if (speedCell) {
    if (p.status === "downloading") {
      const [spNum, spUnit] = splitUnit(fmtSpeed(p.speed_bps));
      speedCell.innerHTML = `${spNum} <span class="unit">${spUnit}</span>`;
    } else {
      speedCell.textContent = "-";
    }
  }
  const etaCell = row.querySelector<HTMLElement>(".dl-col-eta");
  if (etaCell) etaCell.textContent = p.status === "downloading" ? fmtEta(p.eta_secs) : "-";
  const pill = row.querySelector<HTMLElement>(".pill");
  if (pill) {
    pill.className = `pill pill-${p.status}`;
    pill.textContent = statusLabel(p.status);
  }
}

/* ---------- Refresh / polling ---------- */

async function refresh(): Promise<void> {
  try {
    const [list, s] = await Promise.all([listDownloads(), getStats()]);
    downloads = list;
    stats = s;
    ($("list-error") as HTMLElement).hidden = true;
    ($("list-retry") as HTMLElement).hidden = true;
    renderAll();
    void refreshScheduleStatus();
    void refreshUpdateBanner();
    // Pending popups: the 2 s poll is the fallback for the
    // `pending-download` event (e.g. after a missed emit).
    void refreshPendings();
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    const el = $("list-error");
    el.textContent = `Could not load downloads: ${msg}`;
    el.hidden = false;
    ($("list-retry") as HTMLElement).hidden = false;
    // Never leave the loading skeleton on screen after a failure.
    $("download-list").innerHTML =
      `<div class="empty-state"><div class="empty-icon">${ICONS.download}</div><p>Could not load downloads.</p></div>`;
  }
}

function startPolling(): void {
  window.clearInterval(pollTimer);
  pollTimer = window.setInterval(() => {
    void refresh();
  }, 2000);
}

/* ---------- Views / nav ---------- */

const VIEW_TITLES: Record<ViewName, string> = {
  downloads: "Downloads",
  completed: "Completed",
  settings: "Settings",
};

function setView(next: ViewName): void {
  view = next;
  // One h1 for the whole app, renamed per view — the inspector reads the
  // first h1 in the DOM, so three hidden ones reported "no h1" on Settings.
  $("view-title").textContent = VIEW_TITLES[next];
  ($("add-url-btn") as HTMLElement).hidden = next !== "downloads";
  // The category pills filter the Downloads queue only.
  ($("cat-pills") as HTMLElement).hidden = next !== "downloads";
  document.body.classList.remove("nav-open");
  ($("menu-btn") as HTMLElement).setAttribute("aria-expanded", "false");
  for (const section of ["downloads", "completed", "settings"]) {
    ($(`view-${section}`) as HTMLElement).hidden = section !== next;
  }
  document.querySelectorAll<HTMLElement>(".nav-item").forEach((b) => {
    b.classList.toggle("is-active", b.dataset.view === next);
  });
  if (next === "settings") void loadSettings();
  renderAll();
}

function setTab(next: TabName): void {
  tab = next;
  document.querySelectorAll<HTMLElement>(".tab").forEach((b) => {
    b.classList.toggle("is-active", b.dataset.tab === next);
  });
  renderDownloadsView();
}

/* ---------- Modal ---------- */

function openModal(): void {
  ($("modal-root") as HTMLElement).hidden = false;
  ($("pending-modal") as HTMLElement).hidden = true;
  ($("add-modal") as HTMLElement).hidden = false;
  ($("add-url") as HTMLInputElement).focus();
}

function closeModal(): void {
  ($("modal-root") as HTMLElement).hidden = true;
  ($("add-modal") as HTMLElement).hidden = true;
  ($("pending-modal") as HTMLElement).hidden = true;
  currentPending = null;
  ($("add-error") as HTMLElement).hidden = true;
  ($("grab-error") as HTMLElement).hidden = true;
  ($("batch-note") as HTMLElement).hidden = true;
}

/* ---------- Download confirmation popup ---------- */

async function listPending(): Promise<PendingDownload[]> {
  return invoke<PendingDownload[]>("list_pending");
}

function pendingModalOpen(): boolean {
  return !($("pending-modal") as HTMLElement).hidden;
}

function fillPendingForm(p: PendingDownload): void {
  ($("pending-filename") as HTMLInputElement).value = p.filename;
  $("pending-url").textContent = p.url;
  ($("pending-dest") as HTMLInputElement).value = p.dest_dir;
  ($("pending-category") as HTMLSelectElement).value = p.category;
  ($("pending-connections") as HTMLInputElement).value = String(p.connections);
  ($("pending-sha256") as HTMLInputElement).value = "";
  ($("pending-error") as HTMLElement).hidden = true;
}

function updatePendingSize(p: PendingDownload): void {
  $("pending-size").textContent =
    p.total !== null && p.total > 0 ? fmtBytes(p.total) : "Unknown";
  const more = $("pending-more");
  const rest = pendings.length - 1;
  if (rest > 0) {
    more.textContent = `${rest} more pending`;
    more.hidden = false;
  } else {
    more.hidden = true;
  }
}

function showPending(p: PendingDownload): void {
  currentPending = p;
  ($("modal-root") as HTMLElement).hidden = false;
  ($("add-modal") as HTMLElement).hidden = true;
  ($("pending-modal") as HTMLElement).hidden = false;
  fillPendingForm(p);
  updatePendingSize(p);
  ($("pending-filename") as HTMLInputElement).focus();
  ($("pending-filename") as HTMLInputElement).select();
}

/** Re-fetch the pending list and show the next popup, or close the
 *  modal when nothing is left. While the modal is open for a pending
 *  item, only its size line is updated — never clobber user edits. */
async function refreshPendings(): Promise<void> {
  try {
    pendings = await listPending();
  } catch {
    return;
  }
  if (currentPending) {
    const stillThere = pendings.find((p) => p.id === currentPending!.id);
    if (stillThere) {
      updatePendingSize(stillThere);
      return;
    }
    currentPending = null;
  }
  if (pendings.length > 0) {
    showPending(pendings[0]);
  } else if (pendingModalOpen()) {
    ($("pending-modal") as HTMLElement).hidden = true;
    if (($("add-modal") as HTMLElement).hidden) {
      ($("modal-root") as HTMLElement).hidden = true;
    }
  }
}

function readPendingForm(): ConfirmOptions | null {
  if (!currentPending) return null;
  const filename = ($("pending-filename") as HTMLInputElement).value.trim();
  const destDir = ($("pending-dest") as HTMLInputElement).value.trim();
  const connections = Math.min(
    32,
    Math.max(
      1,
      Number(($("pending-connections") as HTMLInputElement).value) ||
        currentPending.connections,
    ),
  );
  return {
    filename: filename || currentPending.filename,
    dest_dir: destDir || currentPending.dest_dir,
    category: ($("pending-category") as HTMLSelectElement).value,
    connections,
    start_now: false,
    expected_sha256:
      ($("pending-sha256") as HTMLInputElement).value.trim() || null,
  };
}

async function confirmCurrent(startNow: boolean): Promise<void> {
  const opts = readPendingForm();
  if (!opts || !currentPending) return;
  opts.start_now = startNow;
  const errEl = $("pending-error");
  try {
    await invoke("confirm_pending", { id: currentPending.id, options: opts });
    errEl.hidden = true;
    currentPending = null;
    await refresh();
    await refreshPendings();
  } catch (err) {
    errEl.textContent = err instanceof Error ? err.message : String(err);
    errEl.hidden = false;
  }
}

async function dismissCurrent(): Promise<void> {
  if (!currentPending) return;
  const id = currentPending.id;
  currentPending = null;
  try {
    await invoke("dismiss_pending", { id });
  } catch {
    // Dismiss is best-effort: the item is gone from the UI either way.
  }
  await refreshPendings();
}

async function addOne(url: string, connections: number | null, expectedSha256: string | null = null): Promise<void> {
  const errEl = $("add-error");
  try {
    await invoke<string>("add_download", { url, connections, expected_sha256: expectedSha256 });
    errEl.hidden = true;
    closeModal();
    await refresh();
  } catch (err) {
    errEl.textContent = err instanceof Error ? err.message : String(err);
    errEl.hidden = false;
  }
}

/* ---------- Settings ---------- */

async function fillSettingsForm(s: Settings): Promise<void> {
  ($("set-connections") as HTMLInputElement).value = String(s.connections);
  ($("set-max-concurrent") as HTMLInputElement).value = String(s.max_concurrent);
  ($("set-download-dir") as HTMLInputElement).value = s.download_dir;
  ($("set-speed-limit") as HTMLInputElement).value = String(s.speed_limit_kbps);
  ($("set-proxy") as HTMLInputElement).value = s.proxy;
  ($("set-ask-before-download") as HTMLInputElement).checked = s.ask_before_download;
  ($("set-minimize-to-tray") as HTMLInputElement).checked = s.minimize_to_tray;
  ($("set-close-to-tray") as HTMLInputElement).checked = s.close_to_tray;
  ($("set-desktop-notifications") as HTMLInputElement).checked = s.desktop_notifications;
  ($("set-autostart") as HTMLInputElement).checked = s.autostart;
  ($("set-resume-on-startup") as HTMLInputElement).checked = s.resume_on_startup;
  ($("set-clamav-scan") as HTMLInputElement).checked = s.clamav_scan;
  ($("set-update-manifest-url") as HTMLInputElement).value = s.update_manifest_url;
  // ClamAV availability is live state, not a setting: ask the
  // backend so the row says honestly whether a scanner exists.
  try {
    const st = (await invoke("get_clamav_status")) as { available: boolean; version: string | null };
    const note = $("clamav-status");
    note.textContent = st.available
      ? `${st.version ?? "ClamAV"} terpasang — download selesai dipindai otomatis.`
      : "ClamAV tidak terpasang — download tidak dipindai. Pasang dengan: sudo pacman -S clamav && sudo freshclam";
  } catch {
    const note = $("clamav-status");
    note.textContent = "Status ClamAV tidak dapat diperiksa.";
  }
  ($("set-schedule-enabled") as HTMLInputElement).checked = s.schedule.enabled;
  ($("set-schedule-start") as HTMLInputElement).value = s.schedule.start;
  ($("set-schedule-stop") as HTMLInputElement).value = s.schedule.stop;
  document.querySelectorAll<HTMLElement>(".day-btn").forEach((b) => {
    const on = s.schedule.days[Number(b.dataset.day ?? 0)] ?? true;
    b.setAttribute("aria-pressed", String(on));
    b.classList.toggle("is-active", on);
  });
}

async function refreshScheduleStatus(): Promise<void> {
  try {
    const st = await invoke<ScheduleStatus>("get_schedule_status");
    const show = st.enabled && st.next_label !== null;
    const next = $("schedule-next");
    next.textContent = show ? st.next_label : "";
    next.hidden = !show;
    // The Today panel's subline always says something: the next event when
    // the schedule is on, otherwise that there is no daily schedule.
    const line = $("schedule-line");
    line.textContent = show ? st.next_label : "No daily schedule";
    line.hidden = false;
  } catch {
    // Informational only — a failed status read never blocks the UI.
  }
}

async function loadSettings(): Promise<void> {
  try {
    settings = await getSettings();
    fillSettingsForm(settings);
    void refreshScheduleStatus();
    void refreshUpdateStatus();
    renderCompletedView();
  } catch {
    // Settings form keeps its defaults until the backend responds.
  }
}

// A time input always yields "HH:MM" or "", so anything else (a
// hand-edited value) falls back instead of reaching the backend.
function validTime(v: string, fallback: string): string {
  return /^\d{2}:\d{2}$/.test(v) ? v : fallback;
}

function readSettingsForm(): Settings {
  const connections = Math.min(
    32,
    Math.max(1, Number(($("set-connections") as HTMLInputElement).value) || 8),
  );
  const maxConcurrent = Math.max(
    1,
    Number(($("set-max-concurrent") as HTMLInputElement).value) || 3,
  );
  return {
    connections,
    max_concurrent: maxConcurrent,
    download_dir: ($("set-download-dir") as HTMLInputElement).value.trim(),
    speed_limit_kbps: Math.max(
      0,
      Number(($("set-speed-limit") as HTMLInputElement).value) || 0,
    ),
    proxy: ($("set-proxy") as HTMLInputElement).value.trim(),
    ask_before_download: ($("set-ask-before-download") as HTMLInputElement).checked,
    minimize_to_tray: ($("set-minimize-to-tray") as HTMLInputElement).checked,
    close_to_tray: ($("set-close-to-tray") as HTMLInputElement).checked,
    desktop_notifications: ($("set-desktop-notifications") as HTMLInputElement).checked,
    autostart: ($("set-autostart") as HTMLInputElement).checked,
    resume_on_startup: ($("set-resume-on-startup") as HTMLInputElement).checked,
    clamav_scan: ($("set-clamav-scan") as HTMLInputElement).checked,
    update_manifest_url: ($("set-update-manifest-url") as HTMLInputElement).value.trim(),
    schedule: {
      enabled: ($("set-schedule-enabled") as HTMLInputElement).checked,
      start: validTime(($("set-schedule-start") as HTMLInputElement).value, "23:00"),
      stop: validTime(($("set-schedule-stop") as HTMLInputElement).value, "06:00"),
      days: Array.from(document.querySelectorAll<HTMLElement>(".day-btn")).map(
        (b) => b.getAttribute("aria-pressed") === "true",
      ),
    },
  };
}

/* ---------- Update checker ---------- */

// The offered manifest (if any), the task id of the update asset
// download once the user clicks Download, and the last rendered
// banner key so the 2 s poll does not rewrite the DOM.
let offeredUpdate: UpdateManifest | null = null;
let updateDownloadId: string | null = null;
let updateBannerKey = "";

function fmtCheckedAt(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString([], {
    day: "numeric",
    month: "short",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/// The Settings status line: the last check's message plus when it
/// ran. Honest for every state — including "no source configured".
async function refreshUpdateStatus(): Promise<void> {
  const el = $("update-status");
  try {
    const view = await invoke<UpdateStatusView>("get_update_status");
    const check = view.check;
    el.textContent = check
      ? `${check.message} Checked ${fmtCheckedAt(check.checked_at)}.`
      : "Update checks have not run yet.";
  } catch {
    el.textContent = "Update status could not be read.";
  }
}

/// The offer banner above the Downloads bento grid. Shown only for
/// an update that is newer than the running version and not
/// dismissed; after the user clicks Download it follows the asset
/// download (queued / downloading / completed / failed) so the
/// manual install steps are shown exactly when the file is ready.
async function refreshUpdateBanner(): Promise<void> {
  let view: UpdateStatusView | null = null;
  try {
    view = await invoke<UpdateStatusView>("get_update_status");
  } catch {
    return; // Informational only — a failed read hides nothing.
  }
  const check = view?.check ?? null;
  const manifest =
    check?.state === "update_available" ? check.manifest : null;
  offeredUpdate = manifest;
  const visible =
    manifest !== null && manifest.version !== view?.dismissed_version;
  const dl =
    updateDownloadId !== null
      ? downloads.find((d) => d.id === updateDownloadId)
      : undefined;
  const key = visible
    ? `${manifest.version}|${dl?.status ?? "offer"}|${dl?.error ?? ""}`
    : "hidden";
  if (key === updateBannerKey) return;
  updateBannerKey = key;
  const banner = $("update-banner");
  if (!visible || manifest === null) {
    banner.hidden = true;
    return;
  }
  const title = $("update-banner-title");
  const notes = $("update-banner-notes");
  const downloadBtn = $("update-download") as HTMLButtonElement;
  const dismissBtn = $("update-dismiss") as HTMLButtonElement;
  const ver = `<span class="update-version">${esc(manifest.version)}</span>`;
  if (dl && dl.status === "completed") {
    // The file is ready. ZDM never replaces its own running binary:
    // the note says where the file is and what to do by hand.
    title.innerHTML = `Update downloaded — ZDM ${ver}`;
    notes.textContent = `Saved to ${dl.dest_path}. ZDM cannot replace its own running binary — quit ZDM, then replace ./release/zdm (or the AppImage) with the downloaded file.`;
    downloadBtn.hidden = true;
    dismissBtn.hidden = false;
  } else if (dl && dl.status === "failed") {
    title.innerHTML = `Update download failed — ZDM ${ver}`;
    notes.textContent = dl.error ?? "The download failed.";
    downloadBtn.hidden = false;
    dismissBtn.hidden = false;
  } else if (dl && dl.status === "queued") {
    title.innerHTML = `Update queued — ZDM ${ver}`;
    notes.textContent = "Start the queue to download the update.";
    downloadBtn.hidden = true;
    dismissBtn.hidden = false;
  } else if (dl && dl.status === "paused") {
    title.innerHTML = `Update download paused — ZDM ${ver}`;
    notes.textContent = "Resume it from the queue.";
    downloadBtn.hidden = true;
    dismissBtn.hidden = false;
  } else if (dl) {
    title.innerHTML = `Downloading update — ZDM ${ver}`;
    notes.textContent =
      "The update downloads like any other file — checksum and virus scan apply.";
    downloadBtn.hidden = true;
    dismissBtn.hidden = false;
  } else {
    title.innerHTML = `Update available — ZDM ${ver}`;
    notes.textContent = manifest.changelog ?? "No changelog provided.";
    downloadBtn.hidden = false;
    dismissBtn.hidden = false;
  }
  banner.hidden = false;
}

/* ---------- Row actions (event delegation) ---------- */

async function handleRowAction(action: string, id: string): Promise<void> {
  if (action === "verify") {
    try {
      await invoke("verify_checksum", { id });
      await refresh();
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      const el = $("list-error");
      el.textContent = `Could not verify download: ${msg}`;
      el.hidden = false;
    }
    return;
  }
  if (action === "scan") {
    try {
      await invoke("scan_download", { id });
      await refresh();
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      const el = $("list-error");
      el.textContent = `Could not scan download: ${msg}`;
      el.hidden = false;
    }
    return;
  }
  const commands: Record<string, string> = {
    pause: "pause_download",
    resume: "resume_download",
    cancel: "cancel_download",
    delete: "delete_download",
  };
  const cmd = commands[action];
  if (!cmd) return;
  try {
    await invoke(cmd, { id });
    await refresh();
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    const el = $("list-error");
    el.textContent = `Could not ${action} download: ${msg}`;
    el.hidden = false;
  }
}

/* ---------- Static icons (injected once at init) ---------- */

// Static HTML stays text-only so there is exactly one icon source
// (src/icons.ts); the icons are prepended here, before first render.
function prependIcon(el: HTMLElement | null, name: IconName): void {
  if (el) el.insertAdjacentHTML("afterbegin", ICONS[name]);
}

function injectStaticIcons(): void {
  const navIcons: Record<string, IconName> = {
    downloads: "download",
    completed: "checkCircle",
    settings: "settings",
  };
  document.querySelectorAll<HTMLElement>(".nav-item").forEach((b) => {
    const name = navIcons[b.dataset.view ?? ""];
    if (name) prependIcon(b, name);
  });
  prependIcon($("add-url-btn"), "plus");
  prependIcon($("queue-start"), "play");
  prependIcon($("queue-stop"), "pause");
  prependIcon($("pending-start"), "download");
  prependIcon($("pending-queue"), "list");
  prependIcon($("pending-cancel"), "x");
  prependIcon($("add-download"), "plus");
  prependIcon($("add-batch"), "list");
  prependIcon($("grab-links"), "globe");
  // Search icon sits inside the field, muted, on the left.
  for (const field of document.querySelectorAll<HTMLElement>(".search-field")) {
    field.insertAdjacentHTML(
      "afterbegin",
      `<span class="search-icon">${ICONS.search}</span>`,
    );
  }
}

/* ---------- Init ---------- */

function bindSearch(inputId: string): void {
  const input = document.getElementById(inputId) as HTMLInputElement | null;
  if (!input) return;
  input.addEventListener("input", () => {
    query = input.value;
    for (const other of ["top-search", "completed-search"]) {
      if (other !== inputId) {
        const el = document.getElementById(other) as HTMLInputElement | null;
        if (el) el.value = input.value;
      }
    }
    renderAll();
  });
}

async function init(): Promise<void> {
  injectStaticIcons();
  document.querySelectorAll<HTMLElement>(".nav-item").forEach((b) => {
    b.addEventListener("click", () => setView(b.dataset.view as ViewName));
  });
  document.querySelectorAll<HTMLElement>(".tab").forEach((b) => {
    b.addEventListener("click", () => setTab(b.dataset.tab as TabName));
  });

  // Phone nav: the sidebar is a sheet opened from the top bar, never a
  // 64px rail with unreadable labels.
  const menuBtn = $("menu-btn") as HTMLButtonElement;
  const closeNav = (): void => {
    document.body.classList.remove("nav-open");
    menuBtn.setAttribute("aria-expanded", "false");
    menuBtn.setAttribute("aria-label", "Open menu");
  };
  menuBtn.addEventListener("click", () => {
    const open = !document.body.classList.contains("nav-open");
    document.body.classList.toggle("nav-open", open);
    menuBtn.setAttribute("aria-expanded", String(open));
    menuBtn.setAttribute("aria-label", open ? "Close menu" : "Open menu");
  });
  $("nav-backdrop").addEventListener("click", closeNav);

  bindSearch("top-search");
  bindSearch("completed-search");

  $("sort").addEventListener("change", (e) => {
    sort = (e.target as HTMLSelectElement).value as SortName;
    renderAll();
  });

  $("history-category").addEventListener("change", (e) => {
    historyCategory = (e.target as HTMLSelectElement).value;
    historyDay = null;
    renderCompletedView();
  });

  $("history-range").addEventListener("change", (e) => {
    historyRange = (e.target as HTMLSelectElement).value as HistoryRange;
    historyDay = null;
    renderCompletedView();
  });

  // Header category pills: one click filters the Downloads queue.
  $("cat-pills").addEventListener("click", (e) => {
    const target = (e.target as HTMLElement).closest<HTMLElement>("[data-cat]");
    if (!target) return;
    dlCategory = target.dataset.cat ?? "all";
    document.querySelectorAll<HTMLElement>(".cat-pill").forEach((b) => {
      b.classList.toggle("is-active", b.dataset.cat === dlCategory);
    });
    renderDownloadsView();
  });

  // Scrubber: Downloads scrolls to the day's group (and marks it);
  // Completed filters the history to that day. "All" goes back.
  document.addEventListener("click", (e) => {
    const target = (e.target as HTMLElement).closest<HTMLElement>(".scrub-btn");
    if (!target) return;
    const day = target.dataset.day ?? "all";
    const mode = target.dataset.mode ?? "downloads";
    if (mode === "completed") {
      historyDay = day === "all" ? null : day;
      renderCompletedView();
      return;
    }
    document.querySelectorAll<HTMLElement>("#scrubber .scrub-btn").forEach((b) => {
      b.classList.toggle("is-active", b === target);
    });
    if (day === "all") {
      $("content").scrollTo({ top: 0, behavior: "smooth" });
      return;
    }
    const group = document.getElementById(`tl-d-${day}`);
    if (!group) return;
    group.scrollIntoView({ behavior: "smooth", block: "start" });
    group.classList.add("tl-flash");
    window.setTimeout(() => group.classList.remove("tl-flash"), 1200);
  });

  $("add-url-btn").addEventListener("click", openModal);
  $("list-retry").addEventListener("click", () => void refresh());
  $("modal-close").addEventListener("click", closeModal);
  // Backdrop click closes the Add URL modal only — never dismiss a
  // pending confirmation by accident.
  $("modal-backdrop").addEventListener("click", () => {
    if (!pendingModalOpen()) closeModal();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    if (document.body.classList.contains("nav-open")) {
      document.body.classList.remove("nav-open");
      const menuBtn = $("menu-btn") as HTMLButtonElement;
      menuBtn.setAttribute("aria-expanded", "false");
      menuBtn.setAttribute("aria-label", "Open menu");
      return;
    }
    // Escape on the confirmation popup dismisses (cancels) it;
    // otherwise it closes the Add URL modal.
    if (pendingModalOpen()) void dismissCurrent();
    else closeModal();
  });

  $("pending-start").addEventListener("click", () => void confirmCurrent(true));
  $("pending-queue").addEventListener("click", () => void confirmCurrent(false));
  $("pending-cancel").addEventListener("click", () => void dismissCurrent());

  $("add-download").addEventListener("click", () => {
    const url = ($("add-url") as HTMLInputElement).value.trim();
    const raw = Number(($("add-connections") as HTMLInputElement).value);
    const connections = Number.isFinite(raw) && raw >= 1 && raw <= 32 ? raw : null;
    const expectedSha256 =
      ($("add-sha256") as HTMLInputElement).value.trim() || null;
    if (!url) {
      const errEl = $("add-error");
      errEl.textContent = "Enter a URL first.";
      errEl.hidden = false;
      return;
    }
    void addOne(url, connections, expectedSha256);
  });
  $("add-url").addEventListener("keydown", (e) => {
    if (e.key === "Enter") ($("add-download") as HTMLButtonElement).click();
  });

  $("add-batch").addEventListener("click", async () => {
    const urls = ($("batch-urls") as HTMLTextAreaElement).value
      .split("\n")
      .map((u) => u.trim())
      .filter(Boolean);
    const note = $("batch-note");
    if (urls.length === 0) return;
    try {
      const ids = await invoke<string[]>("add_batch", { urls });
      note.textContent = `Added ${ids.length} download${ids.length === 1 ? "" : "s"}.`;
      note.hidden = false;
      ($("batch-urls") as HTMLTextAreaElement).value = "";
      await refresh();
    } catch (err) {
      note.textContent = err instanceof Error ? err.message : String(err);
      note.hidden = false;
    }
  });

  $("grab-links").addEventListener("click", async () => {
    const pageUrl = ($("grab-url") as HTMLInputElement).value.trim();
    const errEl = $("grab-error");
    const results = $("grab-results");
    if (!pageUrl) return;
    try {
      const items = await invoke<GrabItem[]>("grab_links", { page_url: pageUrl });
      // ffmpeg decides whether a DASH manifest can be merged at
      // all; the label says so honestly when it is missing.
      let ffmpegAvailable = true;
      try {
        const st = await invoke<{ available: boolean }>("get_ffmpeg_status");
        ffmpegAvailable = st.available;
      } catch {
        // Best effort: assume it is there.
      }
      errEl.hidden = true;
      results.innerHTML =
        items.length === 0
          ? `<li><span class="grab-link">No downloadable links found on this page.</span></li>`
          : items
              .map((item) => {
                let label: string = item.kind;
                if (item.kind === "manifest") {
                  const isDash = item.url.split(/[?#]/)[0].toLowerCase().endsWith(".mpd");
                  label = isDash ? "DASH stream" : "HLS stream";
                  if (isDash && !ffmpegAvailable) label += " · butuh ffmpeg";
                }
                const size = item.size == null ? "—" : fmtBytes(item.size);
                return `<li><span class="grab-kind">${esc(label)}</span><span class="grab-link" title="${esc(item.url)}">${esc(item.url)}</span><span class="grab-size">${esc(size)}</span><button class="btn btn-text" data-grab-add="${esc(item.url)}" type="button">Add</button></li>`;
              })
              .join("");
    } catch (err) {
      errEl.textContent = err instanceof Error ? err.message : String(err);
      errEl.hidden = false;
    }
  });

  $("grab-results").addEventListener("click", (e) => {
    const target = (e.target as HTMLElement).closest<HTMLElement>("[data-grab-add]");
    if (!target) return;
    void addOne(target.dataset.grabAdd ?? "", null);
  });

  $("queue-start").addEventListener("click", async () => {
    try {
      await invoke("start_queue");
      await refresh();
    } catch {
      // Queue control is best-effort until the backend registers it.
    }
  });
  $("queue-stop").addEventListener("click", async () => {
    try {
      await invoke("stop_queue");
      await refresh();
    } catch {
      // Queue control is best-effort until the backend registers it.
    }
  });

  $("save-settings").addEventListener("click", async () => {
    const next = readSettingsForm();
    const savedEl = $("settings-saved");
    const errEl = $("settings-error");
    try {
      await invoke("save_settings", { settings: next });
      settings = next;
      savedEl.hidden = false;
      errEl.hidden = true;
      void refreshScheduleStatus();
      window.setTimeout(() => {
        savedEl.hidden = true;
      }, 3000);
    } catch (err) {
      errEl.textContent = err instanceof Error ? err.message : String(err);
      errEl.hidden = false;
      savedEl.hidden = true;
    }
  });

  // Check for updates: save the form first, so a manifest URL that
  // was just typed in is the one being checked, then run the check
  // and show the result in the status line and the banner.
  $("check-updates").addEventListener("click", async () => {
    const btn = $("check-updates") as HTMLButtonElement;
    const savedEl = $("settings-saved");
    const errEl = $("settings-error");
    btn.disabled = true;
    btn.textContent = "Checking…";
    try {
      const next = readSettingsForm();
      await invoke("save_settings", { settings: next });
      settings = next;
      await invoke("check_for_updates");
      await refreshUpdateStatus();
      await refreshUpdateBanner();
      savedEl.hidden = false;
      errEl.hidden = true;
      window.setTimeout(() => {
        savedEl.hidden = true;
      }, 3000);
    } catch (err) {
      errEl.textContent = err instanceof Error ? err.message : String(err);
      errEl.hidden = false;
      savedEl.hidden = true;
    } finally {
      btn.disabled = false;
      btn.textContent = "Check now";
    }
  });

  // The offer banner: Download queues the asset through the normal
  // engine path (with the manifest sha256 as the expected hash, so a
  // tampered asset fails like a checksum mismatch); Dismiss records
  // the version so the offer stays hidden until a newer one appears.
  $("update-download").addEventListener("click", async () => {
    if (!offeredUpdate) return;
    try {
      const id = await invoke<string>("add_download", {
        url: offeredUpdate.download_url,
        connections: null,
        expected_sha256: offeredUpdate.sha256,
      });
      updateDownloadId = id;
      await refresh();
      await refreshUpdateBanner();
    } catch (err) {
      $("update-banner-notes").textContent = `Could not start the update download: ${
        err instanceof Error ? err.message : String(err)
      }`;
    }
  });

  $("update-dismiss").addEventListener("click", async () => {
    if (!offeredUpdate) return;
    try {
      await invoke("dismiss_update", { version: offeredUpdate.version });
    } catch {
      // Dismiss is best-effort: the banner hides either way.
    }
    offeredUpdate = null;
    updateBannerKey = "";
    await refreshUpdateBanner();
  });

  $("schedule-queue").addEventListener("click", async () => {
    const raw = ($("set-schedule") as HTMLInputElement).value;
    const savedEl = $("settings-saved");
    const errEl = $("settings-error");
    if (!raw) {
      errEl.textContent = "Pick a date and time first.";
      errEl.hidden = false;
      return;
    }
    const startTime = raw.replace("T", " ");
    try {
      await invoke("schedule_queue", { start_time: startTime });
      savedEl.textContent = `Queue scheduled for ${startTime}.`;
      savedEl.hidden = false;
      errEl.hidden = true;
    } catch (err) {
      errEl.textContent = err instanceof Error ? err.message : String(err);
      errEl.hidden = false;
    }
  });

  // Daily-schedule day toggles: a click flips one day in place;
  // the choice is saved with the rest of the form.
  document.querySelectorAll<HTMLElement>(".day-btn").forEach((b) => {
    b.addEventListener("click", () => {
      const on = b.getAttribute("aria-pressed") !== "true";
      b.setAttribute("aria-pressed", String(on));
      b.classList.toggle("is-active", on);
    });
  });

  document.addEventListener("click", (e) => {
    const target = (e.target as HTMLElement).closest<HTMLElement>("[data-action]");
    if (!target) return;
    const action = target.dataset.action ?? "";
    const id = target.dataset.id ?? "";
    void handleRowAction(action, id);
  });

  try {
    await listen<ProgressPayload>("download-progress", (event) => {
      applyProgress(event.payload);
    });
  } catch {
    // Progress events are unavailable outside the Tauri runtime; polling covers it.
  }

  try {
    // A browser-intercepted URL became pending: re-fetch the list
    // (the event payload is the first pending item) and pop the dialog.
    await listen<PendingDownload>("pending-download", () => {
      void refreshPendings();
    });
  } catch {
    // The 2 s poll in refresh() covers missed events.
  }

  try {
    // Tray menu "Add URL..." — show the window and open the modal.
    await listen("open-add-modal", () => {
      openModal();
    });
  } catch {
    // Tray menu is unavailable without the tray; nothing to open.
  }

  await loadSettings();
  await refresh();
  startPolling();
  // Live speed graph: one sample a second from the last known stats;
  // re-render on resize so the plot always fits its card.
  renderSpeedChart();
  speedTimer = window.setInterval(recordSpeedSample, 1000);
  window.addEventListener("resize", () => renderSpeedChart());
}

void init();
