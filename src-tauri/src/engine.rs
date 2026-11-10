//! Download engine — clean-room Rust port of the Python ZDM engine
//! (`downloader/engine.py`). Plain HTTP/1.1 Range downloads (RFC 9110):
//! dynamic segmentation, resume via JSON sidecar + SQLite, queue,
//! categories, token-bucket speed limit, exponential-backoff retry.

use crate::checksum::{self, ExpectedHash, VerifyState};
use crate::scan::{self, ScanState};
use crate::stream::{self, StreamPlan};
use crate::db;
use crate::settings::Settings;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

pub const CHUNK: usize = 64 * 1024;
pub const MIN_SPLIT: u64 = 128 * 1024;
pub const MAX_RETRIES: u32 = 5;
pub const DEFAULT_CONNECTIONS: u32 = 8;
pub const MAX_CONNECTIONS: u32 = 32;

// ---------------------------------------------------------------- categories

pub const CATEGORY_EXTENSIONS: &[(&str, &[&str])] = &[
    ("Documents", &[".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".txt", ".md", ".rtf", ".odt", ".ods", ".odp", ".csv", ".epub"]),
    ("Music", &[".mp3", ".flac", ".wav", ".ogg", ".m4a", ".aac", ".opus", ".wma"]),
    ("Video", &[".mp4", ".mkv", ".avi", ".mov", ".webm", ".wmv", ".flv", ".m4v"]),
    ("Pictures", &[".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".svg", ".tiff", ".heic", ".avif"]),
    ("Programs", &[".exe", ".msi", ".dmg", ".deb", ".rpm", ".appimage", ".apk", ".bin", ".run", ".sh"]),
    ("Compressed", &[".zip", ".rar", ".7z", ".tar", ".gz", ".bz2", ".xz", ".zst"]),
];

pub fn categorize(filename: &str, mime: Option<&str>) -> String {
    let lower = filename.to_lowercase();
    let ext = lower.rsplit('.').next().map(|e| format!(".{e}")).unwrap_or_default();
    if lower.contains('.') {
        for (cat, exts) in CATEGORY_EXTENSIONS {
            if exts.contains(&ext.as_str()) {
                return cat.to_string();
            }
        }
    }
    if let Some(m) = mime {
        let m = m.to_lowercase();
        for (prefix, cat) in [
            ("audio/", "Music"),
            ("video/", "Video"),
            ("image/", "Pictures"),
            ("application/pdf", "Documents"),
            ("text/", "Documents"),
            ("application/zip", "Compressed"),
            ("application/x-rar-compressed", "Compressed"),
            ("application/x-7z-compressed", "Compressed"),
            ("application/gzip", "Compressed"),
        ] {
            if m.starts_with(prefix) {
                return cat.to_string();
            }
        }
    }
    "General".to_string()
}

pub fn category_dir(base_dir: &str, category: &str) -> PathBuf {
    let path = PathBuf::from(base_dir).join(category);
    let _ = std::fs::create_dir_all(&path);
    path
}

// ---------------------------------------------------------------- rate limit

/// Token-bucket limiter. `try_take` is the pure, unit-testable core:
/// it refills from elapsed time and returns seconds the caller must wait
/// before `n` bytes may pass (0.0 = go now).
pub struct RateLimiter {
    limit_bps: f64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(limit_kbps: f64) -> Self {
        Self {
            limit_bps: limit_kbps.max(0.0) * 1024.0,
            tokens: 0.0,
            last: Instant::now(),
        }
    }

    /// Token ceiling: one second of tokens, so an idle period never
    /// banks a burst. `acquire` sizes its steps to this ceiling.
    fn cap(&self) -> f64 {
        self.limit_bps
    }

    pub fn set_limit(&mut self, limit_kbps: f64) {
        self.limit_bps = limit_kbps.max(0.0) * 1024.0;
        self.tokens = self.tokens.min(self.cap());
    }

    /// Seconds to wait before `n` bytes may pass (0.0 = go now, and
    /// the `n` bytes are deducted). A non-zero wait deducts NOTHING:
    /// the caller retries after sleeping, and the refill is never
    /// lost. The old version deducted a partial take on every call,
    /// which wedged `acquire` forever whenever a chunk was larger
    /// than the one-second token cap, or another worker took tokens
    /// mid-wait — the wait was recomputed from the full `n` each
    /// time while the tokens it had already taken were gone.
    pub fn try_take(&mut self, n: usize) -> f64 {
        if self.limit_bps <= 0.0 {
            return 0.0;
        }
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.limit_bps).min(self.cap());
        let n = n as f64;
        if n <= self.tokens {
            self.tokens -= n;
            0.0
        } else {
            (n - self.tokens) / self.limit_bps
        }
    }

    pub async fn acquire(limiter: &Arc<AsyncMutex<RateLimiter>>, n: usize) {
        // Split into steps no bigger than the token cap: a single
        // try_take can never satisfy more than the cap, so an unsplit
        // large chunk would wait forever.
        let mut remaining = n;
        while remaining > 0 {
            let step = {
                let g = limiter.lock().await;
                if g.limit_bps <= 0.0 {
                    return; // unlimited
                }
                (remaining as f64).min(g.cap().max(1.0)) as usize
            };
            loop {
                let wait = {
                    let mut g = limiter.lock().await;
                    g.try_take(step)
                };
                if wait <= 0.0 {
                    break;
                }
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            }
            remaining -= step;
        }
    }
}

// ---------------------------------------------------------------- segments

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    pub pos: u64,
    pub done: bool,
    #[serde(default)]
    pub assigned: bool,
}

impl Segment {
    fn remainder(&self) -> u64 {
        if self.done || self.pos > self.end {
            0
        } else {
            self.end - self.pos + 1
        }
    }
}

/// Pick work for an idle worker, splitting the largest remainder.
/// Returns the index of the claimed segment, or `None` when nothing
/// remains or is splittable. Pure function — unit tested.
pub fn claim_or_split(segments: &mut Vec<Segment>) -> Option<usize> {
    // Prefer a free (unassigned) unfinished segment with the largest remainder.
    let mut best: Option<usize> = None;
    for (i, s) in segments.iter().enumerate() {
        if s.done || s.assigned {
            continue;
        }
        let better = match best {
            None => true,
            Some(b) => s.remainder() > segments[b].remainder(),
        };
        if better {
            best = Some(i);
        }
    }
    if let Some(i) = best {
        segments[i].assigned = true;
        return Some(i);
    }
    // All unfinished segments are busy: split the largest remainder.
    let mut best: Option<usize> = None;
    for (i, s) in segments.iter().enumerate() {
        if s.done {
            continue;
        }
        let better = match best {
            None => true,
            Some(b) => s.remainder() > segments[b].remainder(),
        };
        if better {
            best = Some(i);
        }
    }
    let i = best?;
    let remaining = segments[i].remainder();
    if remaining < 2 * MIN_SPLIT {
        return None;
    }
    let mid = segments[i].pos + remaining / 2;
    let old_end = segments[i].end;
    segments[i].end = mid;
    let new_seg = Segment {
        start: mid + 1,
        end: old_end,
        pos: mid + 1,
        done: false,
        assigned: true,
    };
    segments.push(new_seg);
    Some(segments.len() - 1)
}

fn init_segments(total: u64, connections: u32) -> Vec<Segment> {
    let n = (connections as u64).min(total).max(1);
    let size = total / n;
    (0..n)
        .map(|i| {
            let start = i * size;
            let end = if i == n - 1 { total - 1 } else { start + size - 1 };
            Segment {
                start,
                end,
                pos: start,
                done: false,
                assigned: false,
            }
        })
        .collect()
}

// ---------------------------------------------------------------- URL helpers

/// Split (clean_url, Authorization header) from a URL with embedded
/// Basic auth (`https://user:pass@host/...`).
pub fn strip_auth(raw_url: &str) -> (String, Option<String>) {
    let Ok(mut parsed) = url::Url::parse(raw_url) else {
        return (raw_url.to_string(), None);
    };
    if parsed.username().is_empty() {
        return (raw_url.to_string(), None);
    }
    let username = parsed.username().to_string();
    let password = parsed.password().unwrap_or("").to_string();
    let decoded = format!(
        "{}:{}",
        percent_decode(&username),
        percent_decode(&password)
    );
    let auth = format!("Basic {}", base64_encode(decoded.as_bytes()));
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    (parsed.to_string(), Some(auth))
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18 & 63) as usize] as char);
        out.push(TABLE[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}

pub fn filename_from_url(raw_url: &str, content_disposition: Option<&str>) -> String {
    if let Some(cd) = content_disposition {
        // filename*=UTF-8''... or filename="..."
        for part in cd.split(';') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix("filename*=") {
                let v = v.trim_matches('"');
                let name = v.split("''").nth(1).unwrap_or(v);
                let name = percent_decode(name);
                if !name.is_empty() {
                    return sanitize_filename(&name);
                }
            } else if let Some(v) = part.strip_prefix("filename=") {
                let name = v.trim().trim_matches('"');
                if !name.is_empty() {
                    return sanitize_filename(&percent_decode(name));
                }
            }
        }
    }
    let name = url::Url::parse(raw_url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut s| s.next_back().map(|x| x.to_string()))
        })
        .unwrap_or_default();
    let name = percent_decode(name.trim());
    if name.is_empty() {
        "download".to_string()
    } else {
        sanitize_filename(&name)
    }
}

fn sanitize_filename(name: &str) -> String {
    let base = std::path::Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    if base.is_empty() || base == "." || base == ".." {
        "download".to_string()
    } else {
        base
    }
}

pub fn build_client(settings: &Settings) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().user_agent("ZDM/1.0");
    if !settings.proxy.trim().is_empty() {
        let proxy = reqwest::Proxy::all(settings.proxy.trim()).map_err(|e| e.to_string())?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- probe

pub struct FileInfo {
    pub total: Option<u64>,
    pub accepts_ranges: bool,
    pub filename: String,
    pub mime: Option<String>,
    /// Expected hash from `Digest` / `Content-Digest` / `Content-MD5`
    /// (none when the server sends no checksum header).
    pub expected_hash: Option<ExpectedHash>,
}

/// Expected hash from the response headers, if any. `Digest` (RFC
/// 3230) and `Content-Digest` (RFC 9530) win over `Content-MD5`.
fn expected_from_headers(h: &reqwest::header::HeaderMap) -> Option<ExpectedHash> {
    for name in ["digest", "content-digest"] {
        if let Some(v) = h.get(name).and_then(|v| v.to_str().ok()) {
            if let Some(found) = checksum::parse_digest_header(v) {
                return Some(found);
            }
        }
    }
    // The `http` crate has no CONTENT_MD5 constant: use the name.
    if let Some(v) = h
        .get("content-md5")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(found) = checksum::parse_content_md5(v) {
            return Some(found);
        }
    }
    None
}

pub async fn probe(client: &reqwest::Client, url: &str) -> FileInfo {
    let (clean_url, auth) = strip_auth(url);
    let mut head = client.head(&clean_url);
    if let Some(a) = &auth {
        head = head.header("Authorization", a.clone());
    }
    if let Ok(resp) = head.send().await {
        if resp.status().is_success() {
            let h = resp.headers();
            let total = h
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let ranges = h
                .get(reqwest::header::ACCEPT_RANGES)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.eq_ignore_ascii_case("bytes"))
                .unwrap_or(false);
            let cd = h
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let mime = h
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            return FileInfo {
                total,
                accepts_ranges: ranges,
                filename: filename_from_url(url, cd.as_deref()),
                mime,
                expected_hash: expected_from_headers(h),
            };
        }
    }
    // Fallback: 1-byte Range GET — also yields the total via Content-Range.
    let mut get = client.get(&clean_url).header("Range", "bytes=0-0");
    if let Some(a) = &auth {
        get = get.header("Authorization", a.clone());
    }
    if let Ok(resp) = get.send().await {
        let h = resp.headers();
        let mut total = None;
        let mut has_range = false;
        if let Some(cr) = h
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
        {
            has_range = true;
            if let Some(t) = cr.rsplit('/').next() {
                total = t.parse::<u64>().ok();
            }
        }
        if total.is_none() {
            total = h
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
        }
        let cd = h
            .get(reqwest::header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let mime = h
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        return FileInfo {
            total,
            accepts_ranges: has_range,
            filename: filename_from_url(url, cd.as_deref()),
            mime,
            expected_hash: expected_from_headers(h),
        };
    }
    FileInfo {
        total: None,
        accepts_ranges: false,
        filename: filename_from_url(url, None),
        mime: None,
        expected_hash: None,
    }
}

// ---------------------------------------------------------------- task state

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sidecar {
    pub url: String,
    pub filename: String,
    pub total: Option<u64>,
    pub status: String,
    #[serde(default)]
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub downloaded_single: u64,
    /// Expected hash (user / header / sidecar). Old sidecars have
    /// none — `#[serde(default)]` keeps them readable.
    #[serde(default)]
    pub expected_hash: Option<ExpectedHash>,
    #[serde(default)]
    pub computed_hash: Option<String>,
    #[serde(default)]
    pub verify_state: VerifyState,
    /// ClamAV outcome. Old sidecars have none — `#[serde(default)]`
    /// keeps them readable. "scanning" is never written (see
    /// `save_state`): a restart must not restore a scan in progress.
    #[serde(default)]
    pub scan_state: ScanState,
    #[serde(default)]
    pub threat_name: Option<String>,
    /// Stream plan (HLS/DASH). Old sidecars have none. Persisted so
    /// a pause / crash / restart resumes segment-by-segment instead
    /// of re-planning and re-downloading finished segments.
    #[serde(default)]
    pub stream: Option<StreamPlan>,
    /// Honest note for a stream that could not be merged (e.g. DASH
    /// without ffmpeg: only the playlist was downloaded).
    #[serde(default)]
    pub stream_note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SegmentView {
    pub start: u64,
    pub end: u64,
    pub pos: u64,
    pub done: bool,
    pub assigned: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadView {
    pub id: String,
    pub url: String,
    pub filename: String,
    pub dest_path: String,
    pub total: Option<u64>,
    pub downloaded: u64,
    pub status: String,
    pub category: String,
    pub created: String,
    pub speed_bps: f64,
    pub eta_secs: Option<u64>,
    pub error: Option<String>,
    pub segments: Vec<SegmentView>,
    /// "unavailable" | "verified" | "mismatch".
    pub verify_state: String,
    /// Hash computed from the finished file (lowercase hex).
    pub computed_hash: Option<String>,
    /// Expected hash hex, when a source provided one.
    pub expected_hash: Option<String>,
    /// ClamAV outcome: "unavailable" | "scanning" | "clean" | "found".
    pub scan_state: String,
    /// Threat name when a virus was found (`Eicar-Test-Signature`).
    pub threat_name: Option<String>,
    /// Stream kind when this download is a merged stream ("hls" /
    /// "dash"), None for ordinary files.
    pub stream_kind: Option<String>,
    /// Stream segments finished / total (both tracks counted).
    pub segments_done: u64,
    pub segments_total: u64,
    /// Honest note when a stream could not be merged (DASH without
    /// ffmpeg: only the playlist was downloaded).
    pub stream_note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    pub id: String,
    pub filename: String,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub speed_bps: f64,
    pub eta_secs: Option<u64>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyResult {
    pub verified: bool,
    pub computed_hash: String,
    pub expected_hash: String,
    pub verify_state: VerifyState,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub active: u64,
    pub total_speed_bps: f64,
    pub downloaded_today: i64,
    pub queued: u64,
    pub completed_total: i64,
}

/// A browser-intercepted download waiting for the user in the
/// confirmation popup. In-memory only: if the app closes with pending
/// items, they are dropped (the browser download was already
/// cancelled, so the user must click the link again).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDownload {
    pub id: String,
    pub url: String,
    pub filename: String,
    pub total: Option<u64>,
    pub mime: Option<String>,
    pub category: String,
    /// Full destination folder (category folder under the download dir).
    pub dest_dir: String,
    pub connections: u32,
    pub created: String,
    /// True while `filename` is still the one derived from the URL:
    /// the HEAD probe may replace it with the server's filename.
    #[serde(default)]
    pub auto_filename: bool,
}

/// Options the user picks in the confirmation popup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmOptions {
    pub filename: String,
    /// Full destination folder chosen in the popup.
    pub dest_dir: String,
    pub category: String,
    pub connections: u32,
    pub start_now: bool,
    /// Optional user-supplied expected SHA-256 (64 hex chars).
    /// Absent in older saved payloads — `#[serde(default)]`.
    #[serde(default)]
    pub expected_sha256: Option<String>,
}

pub struct TaskInner {
    pub id: String,
    pub url: String,
    pub clean_url: String,
    pub auth: Option<String>,
    pub connections: u32,
    pub filename: String,
    pub category: String,
    pub dest_dir: String,
    pub dest_path: PathBuf,
    pub part_path: PathBuf,
    pub state_path: PathBuf,
    pub total: Option<u64>,
    pub accepts_ranges: bool,
    pub mime: Option<String>,
    pub segments: Vec<Segment>,
    pub downloaded_single: u64,
    pub status: String,
    pub error: Option<String>,
    /// Expected hash, from the user, a response header, or a sidecar
    /// file. Priority: user > Digest/Content-MD5 header > sidecar.
    pub expected_hash: Option<ExpectedHash>,
    /// Hash computed from the finished file (streaming, after rename).
    pub computed_hash: Option<String>,
    pub verify_state: VerifyState,
    /// ClamAV outcome for the finished file. `Scanning` is transient
    /// (never persisted); `Found` carries the threat in `threat_name`.
    pub scan_state: ScanState,
    pub threat_name: Option<String>,
    /// HLS/DASH stream plan (None for ordinary files). Persisted in
    /// the sidecar so a pause / crash / restart resumes
    /// segment-by-segment.
    pub stream: Option<StreamPlan>,
    /// Honest note when a stream could not be merged (DASH without
    /// ffmpeg: only the playlist was downloaded).
    pub stream_note: Option<String>,
    pub speed_bps: f64,
    pub db_id: Option<i64>,
    pub created: String,
    pub bytes_since_tick: u64,
    pub last_tick: Instant,
    pub last_emit: Instant,
    pub last_save: Instant,
    /// True when filename/category/destination were chosen by the user
    /// in the confirmation popup: `prepare()` must keep them instead
    /// of re-deriving them from the probe.
    pub preset: bool,
    /// True when the task was paused by the daily scheduler (window
    /// ended). Only such tasks are auto-resumed when the window opens
    /// again — a user-paused task stays paused. Persisted in
    /// `downloads.db` (`paused_by_schedule`) so a restart still
    /// knows the pause was the scheduler's: `restore_from_db` reads
    /// it back and `update_db` writes it on every save.
    pub paused_by_schedule: bool,
    /// True when the task was actively downloading when the app last
    /// stopped (DB status was "downloading" at restore time — a
    /// crash, a kill, or a power cut mid-download). Runtime-only:
    /// it is only meaningful once, at the next startup, where
    /// `startup_resume` treats such a task as resumable. Cleared by
    /// `start_task` (it is running again) and by `pause_task` (the
    /// user deliberately paused it, so it is a manual pause now).
    pub interrupted: bool,
}

impl TaskInner {
    pub fn downloaded(&self) -> u64 {
        if self.segments.is_empty() {
            self.downloaded_single
        } else {
            self.segments
                .iter()
                .map(|s| s.pos.saturating_sub(s.start))
                .sum()
        }
    }

    pub fn progress(&self) -> f64 {
        match self.total {
            Some(t) if t > 0 => (self.downloaded() as f64 / t as f64).min(1.0),
            _ => 0.0,
        }
    }

    pub fn eta_secs(&self) -> Option<u64> {
        if self.speed_bps <= 0.0 {
            return None;
        }
        self.total
            .map(|t| ((t.saturating_sub(self.downloaded())) as f64 / self.speed_bps) as u64)
    }

    pub fn view(&self) -> DownloadView {
        DownloadView {
            id: self.id.clone(),
            url: self.url.clone(),
            filename: self.filename.clone(),
            dest_path: self.dest_path.to_string_lossy().into_owned(),
            total: self.total,
            downloaded: self.downloaded(),
            status: self.status.clone(),
            category: self.category.clone(),
            created: self.created.clone(),
            speed_bps: self.speed_bps,
            eta_secs: self.eta_secs(),
            error: self.error.clone(),
            segments: self
                .segments
                .iter()
                .map(|s| SegmentView {
                    start: s.start,
                    end: s.end,
                    pos: s.pos,
                    done: s.done,
                    assigned: s.assigned,
                })
                .collect(),
            verify_state: self.verify_state.as_str().to_string(),
            computed_hash: self.computed_hash.clone(),
            expected_hash: self.expected_hash.as_ref().map(|e| e.hex.clone()),
            scan_state: self.scan_state.as_str().to_string(),
            threat_name: self.threat_name.clone(),
            stream_kind: self
                .stream
                .as_ref()
                .map(|s| s.kind.as_str().to_string()),
            segments_done: self
                .stream
                .as_ref()
                .map(|s| s.progress().0)
                .unwrap_or(0),
            segments_total: self
                .stream
                .as_ref()
                .map(|s| s.progress().1)
                .unwrap_or(0),
            stream_note: self.stream_note.clone(),
        }
    }
}

pub struct TaskHandle {
    pub id: String,
    pub inner: Arc<AsyncMutex<TaskInner>>,
    pub stop: Arc<AtomicBool>,
    pub cancel: Arc<AtomicBool>,
    pub join: AsyncMutex<Option<JoinHandle<()>>>,
    pub file: Arc<std::sync::Mutex<Option<std::fs::File>>>,
}

// ---------------------------------------------------------------- manager

pub struct Manager {
    pub settings: Settings,
    pub tasks: HashMap<String, Arc<TaskHandle>>,
    pub order: Vec<String>,
    /// Browser-intercepted downloads awaiting confirmation (FIFO).
    pub pending: VecDeque<PendingDownload>,
    pub queue_running: bool,
    pub scheduled_at: Option<chrono::NaiveDateTime>,
    /// A manual Start/Stop (or a pending one-shot schedule) holds
    /// the daily window until the next window boundary — the
    /// scheduler never fights a deliberate user action mid-window.
    pub schedule_hold: bool,
    /// Last observed in-window state, for detecting boundaries.
    pub schedule_last_in_window: Option<bool>,
    pub limiter: Arc<AsyncMutex<RateLimiter>>,
    pub app: Option<AppHandle>,
    pub id_counter: AtomicU64,
    /// Desktop-notification state (dedupe + batch summary). Lives
    /// here so the queue loop, which already observes every status
    /// transition, can drive it without a second watcher.
    pub notifier: crate::notify::Notifier,
}

impl Manager {
    pub fn new(settings: Settings) -> Self {
        let limiter = Arc::new(AsyncMutex::new(RateLimiter::new(
            settings.speed_limit_kbps,
        )));
        let mut m = Self {
            settings,
            tasks: HashMap::new(),
            order: Vec::new(),
            pending: VecDeque::new(),
            queue_running: false,
            scheduled_at: None,
            schedule_hold: false,
            schedule_last_in_window: None,
            limiter,
            app: None,
            id_counter: AtomicU64::new(0),
            notifier: crate::notify::Notifier::new(),
        };
        m.restore_from_db();
        m
    }

    pub fn db_path(&self) -> PathBuf {
        db::db_path_for(&self.settings.download_dir)
    }

    fn next_id(&self) -> String {
        let n = self.id_counter.fetch_add(1, Ordering::SeqCst);
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("{millis}-{n}")
    }

    pub fn add(&mut self, raw_url: &str, connections: Option<u32>) -> Result<Arc<TaskHandle>, String> {
        let url = raw_url.trim().to_string();
        let parsed = url::Url::parse(&url).map_err(|e| format!("invalid URL: {e}"))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err("only http(s) URLs are supported".to_string());
        }
        let (clean_url, auth) = strip_auth(&url);
        let id = self.next_id();
        let created = chrono::Local::now().to_rfc3339();
        let inner = TaskInner {
            id: id.clone(),
            url: url.clone(),
            clean_url,
            auth,
            connections: connections.unwrap_or(self.settings.connections).clamp(1, MAX_CONNECTIONS),
            filename: String::new(),
            category: "General".to_string(),
            dest_dir: self.settings.download_dir.clone(),
            dest_path: PathBuf::new(),
            part_path: PathBuf::new(),
            state_path: PathBuf::new(),
            total: None,
            accepts_ranges: false,
            mime: None,
            segments: Vec::new(),
            downloaded_single: 0,
            status: "queued".to_string(),
            error: None,
            expected_hash: None,
            computed_hash: None,
            verify_state: VerifyState::Unavailable,
            scan_state: ScanState::Unavailable,
            threat_name: None,
            stream: None,
            stream_note: None,
            speed_bps: 0.0,
            db_id: None,
            created,
            bytes_since_tick: 0,
            last_tick: Instant::now(),
            last_emit: Instant::now(),
            last_save: Instant::now(),
            preset: false,
            paused_by_schedule: false,
            interrupted: false,
        };
        let handle = Arc::new(TaskHandle {
            id: id.clone(),
            inner: Arc::new(AsyncMutex::new(inner)),
            stop: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            join: AsyncMutex::new(None),
            file: Arc::new(std::sync::Mutex::new(None)),
        });
        self.tasks.insert(id.clone(), handle.clone());
        self.order.push(id);
        Ok(handle)
    }

    // ------------------------------------------------------------ pending

    /// Queue a browser-intercepted URL for confirmation. Returns
    /// `Ok(None)` when the URL is already pending or already tracked
    /// as a task (dedupe), `Err` for an invalid URL.
    pub fn add_pending(&mut self, raw_url: &str) -> Result<Option<PendingDownload>, String> {
        let url = raw_url.trim().to_string();
        let parsed = url::Url::parse(&url).map_err(|e| format!("invalid URL: {e}"))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err("only http(s) URLs are supported".to_string());
        }
        if self.pending.iter().any(|p| p.url == url) {
            return Ok(None);
        }
        // Dedupe against tasks happens in the drain path, which holds
        // the task URL list (task URLs live behind async mutexes, so
        // this sync method cannot read them itself).
        let filename = filename_from_url(&url, None);
        let category = categorize(&filename, None);
        let dest_dir = category_dir(&self.settings.download_dir, &category)
            .to_string_lossy()
            .into_owned();
        let pending = PendingDownload {
            id: self.next_id(),
            url,
            filename,
            total: None,
            mime: None,
            category,
            dest_dir,
            connections: self.settings.connections,
            created: chrono::Local::now().to_rfc3339(),
            auto_filename: true,
        };
        self.pending.push_back(pending.clone());
        Ok(Some(pending))
    }

    pub fn list_pending(&self) -> Vec<PendingDownload> {
        self.pending.iter().cloned().collect()
    }

    /// Remove a pending item without starting it. Returns false when
    /// the id is unknown (already confirmed/dismissed).
    pub fn dismiss_pending(&mut self, id: &str) -> bool {
        let before = self.pending.len();
        self.pending.retain(|p| p.id != id);
        self.pending.len() != before
    }

    /// Fill in probe results for a pending item (total, MIME, and the
    /// server filename when the name is still URL-derived). The first
    /// probe claims the filename (`auto_filename` cleared), so a later
    /// probe never renames it again — and a name the user edits in
    /// the popup is only ever read at confirm time.
    pub fn update_pending_probe(&mut self, id: &str, info: &FileInfo) {
        if let Some(p) = self.pending.iter_mut().find(|p| p.id == id) {
            p.total = info.total;
            p.mime = info.mime.clone();
            if p.auto_filename && !info.filename.is_empty() {
                p.auto_filename = false;
                if info.filename != p.filename {
                    p.filename = sanitize_filename(&info.filename);
                    p.category = categorize(&p.filename, p.mime.as_deref());
                    p.dest_dir = category_dir(&self.settings.download_dir, &p.category)
                        .to_string_lossy()
                        .into_owned();
                }
            }
        }
    }

    /// Turn a pending item into a real task with the user's choices.
    /// The task is created in "queued" state; the caller decides
    /// whether to start it now. User choices are marked `preset` so
    /// `prepare()` keeps them instead of re-deriving them.
    pub async fn confirm_pending(
        &mut self,
        id: &str,
        opts: ConfirmOptions,
    ) -> Result<Arc<TaskHandle>, String> {
        let Some(pos) = self.pending.iter().position(|p| p.id == id) else {
            return Err("pending download not found".to_string());
        };
        let pending = self.pending.remove(pos).unwrap();
        let filename = if opts.filename.trim().is_empty() {
            pending.filename.clone()
        } else {
            sanitize_filename(opts.filename.trim())
        };
        let category = if CATEGORY_EXTENSIONS
            .iter()
            .any(|(c, _)| *c == opts.category)
            || opts.category == "General"
        {
            opts.category.clone()
        } else {
            categorize(&filename, None)
        };
        let dest_dir = opts.dest_dir.trim();
        let dest_dir = if dest_dir.is_empty() {
            pending.dest_dir.clone()
        } else {
            dest_dir.to_string()
        };
        let handle = self.add(&pending.url, Some(opts.connections))?;
        {
            let mut g = handle.inner.lock().await;
            g.filename = filename.clone();
            g.category = category;
            g.dest_dir = self.settings.download_dir.clone();
            g.dest_path = PathBuf::from(&dest_dir).join(&filename);
            g.part_path = PathBuf::from(format!("{}.part", g.dest_path.display()));
            g.state_path = PathBuf::from(format!("{}.download.json", g.dest_path.display()));
            // User-supplied expected hash wins over any header or
            // sidecar: an invalid value fails the confirm loudly
            // instead of silently passing every download.
            if let Some(raw) = opts.expected_sha256.as_deref() {
                let raw = raw.trim();
                if !raw.is_empty() {
                    g.expected_hash = Some(ExpectedHash {
                        algo: checksum::HashAlgo::Sha256,
                        hex: checksum::normalize_sha256(raw).ok_or_else(|| {
                            "expected SHA-256 must be 64 hex characters".to_string()
                        })?,
                        source: "user".to_string(),
                    });
                }
            }
            g.preset = true;
        }
        Ok(handle)
    }

    /// Rebuild queued/paused tasks from the SQLite registry on startup.
    fn restore_from_db(&mut self) {
        let db_path = self.db_path();
        let Ok(rows) = db::list_downloads(&db_path) else {
            return;
        };
        for row in rows {
            if !["queued", "paused", "downloading", "failed"].contains(&row.status.as_str()) {
                continue;
            }
            let Some(filename) = row.filename.clone() else {
                continue;
            };
            let (clean_url, auth) = strip_auth(&row.url);
            let id = self.next_id();
            let category = row.category.clone();
            let dest_dir = self.settings.download_dir.clone();
            let folder = category_dir(&dest_dir, &category);
            let dest_path = folder.join(&filename);
            let mut inner = TaskInner {
                id: id.clone(),
                url: row.url.clone(),
                clean_url,
                auth,
                connections: self.settings.connections,
                filename: filename.clone(),
                category,
                dest_dir,
                dest_path: dest_path.clone(),
                part_path: PathBuf::from(format!("{}.part", dest_path.display())),
                state_path: PathBuf::from(format!("{}.download.json", dest_path.display())),
                total: row.total.map(|t| t as u64),
                accepts_ranges: false,
                mime: None,
                segments: Vec::new(),
                downloaded_single: 0,
                // A row left as "downloading" means the app stopped
                // (crash, kill, power cut) while the task was running:
                // no worker is alive for it, so it comes back paused
                // but marked interrupted — `startup_resume` treats it
                // as resumable, never as a deliberate manual pause.
                status: if row.status == "downloading" {
                    "paused".to_string()
                } else {
                    row.status.clone()
                },
                interrupted: row.status == "downloading",
                error: row.error.clone(),
                expected_hash: row.expected_hash.as_deref().and_then(|hex| {
                    let algo = if hex.len() == 64 {
                        checksum::HashAlgo::Sha256
                    } else if hex.len() == 32 {
                        checksum::HashAlgo::Md5
                    } else {
                        return None;
                    };
                    checksum::normalize_hex(hex, algo).map(|hex| ExpectedHash {
                        algo,
                        hex,
                        source: "stored".to_string(),
                    })
                }),
                computed_hash: row.computed_hash.clone(),
                verify_state: row
                    .verify_state
                    .as_deref()
                    .map(VerifyState::from_str)
                    .unwrap_or_default(),
                scan_state: row
                    .scan_state
                    .as_deref()
                    .map(ScanState::from_str)
                    .unwrap_or_default(),
                threat_name: row.threat_name.clone(),
                stream: None,
                stream_note: None,
                speed_bps: 0.0,
                db_id: Some(row.id),
                created: row.created.clone().unwrap_or_default(),
                bytes_since_tick: 0,
                last_tick: Instant::now(),
                last_emit: Instant::now(),
                last_save: Instant::now(),
                preset: false,
                paused_by_schedule: row.paused_by_schedule,
            };
            // Try to resume state from the sidecar now (before
            // wrapping in Arc<Mutex> — no contention on a fresh task).
            if inner.state_path.exists() {
                if let Ok(text) = std::fs::read_to_string(&inner.state_path) {
                    if let Ok(sc) = serde_json::from_str::<Sidecar>(&text) {
                        if sc.url == row.url {
                            // Stream plan first: a stream task may
                            // have no .part file yet (merge not done),
                            // but its segments live in the segments
                            // dir and its plan in the sidecar. The
                            // note is restored even without a plan
                            // (DASH without ffmpeg: playlist only).
                            inner.stream_note = sc.stream_note;
                            if let Some(plan) = sc.stream {
                                inner.stream = Some(plan);
                            }
                            if inner.part_path.exists() {
                                inner.total = sc.total;
                                inner.segments = sc.segments;
                                inner.downloaded_single = sc.downloaded_single;
                                // The sidecar is written on every save, so
                                // it is the freshest verification record.
                                if sc.expected_hash.is_some() {
                                    inner.expected_hash = sc.expected_hash;
                                }
                                inner.computed_hash = sc.computed_hash;
                                inner.verify_state = sc.verify_state;
                                // Same for the scan: the sidecar is the
                                // freshest record, but "scanning" is never
                                // written to it, so a restore never sees a
                                // scan in progress.
                                inner.scan_state = sc.scan_state;
                                inner.threat_name = sc.threat_name;
                                for s in &mut inner.segments {
                                    s.assigned = false;
                                    s.pos = s.pos.clamp(s.start, s.end + 1);
                                    s.done = s.pos > s.end;
                                }
                            }
                        }
                    }
                }
            }
            let handle = Arc::new(TaskHandle {
                id: id.clone(),
                inner: Arc::new(AsyncMutex::new(inner)),
                stop: Arc::new(AtomicBool::new(false)),
                cancel: Arc::new(AtomicBool::new(false)),
                join: AsyncMutex::new(None),
                file: Arc::new(std::sync::Mutex::new(None)),
            });
            self.tasks.insert(id.clone(), handle);
            self.order.push(id);
        }
    }

    pub async fn list_views(&self) -> Vec<DownloadView> {
        let mut out = Vec::new();
        for id in &self.order {
            if let Some(h) = self.tasks.get(id) {
                out.push(h.inner.lock().await.view());
            }
        }
        out
    }

    /// Re-verify a finished download against its stored expected
    /// hash: stream the file on disk, compare, persist the outcome.
    /// A mismatch marks the download failed (file kept on disk),
    /// exactly like the automatic check after a download.
    pub async fn verify_checksum(&self, id: &str) -> Result<VerifyResult, String> {
        let handle = self
            .tasks
            .get(id)
            .cloned()
            .ok_or_else(|| "download not found".to_string())?;
        let (expected, dest_path) = {
            let g = handle.inner.lock().await;
            (g.expected_hash.clone(), g.dest_path.clone())
        };
        let Some(exp) = expected else {
            return Err("no expected hash recorded for this download".to_string());
        };
        if !dest_path.is_file() {
            return Err("downloaded file not found on disk".to_string());
        }
        let algo = exp.algo;
        let hex = tokio::task::spawn_blocking(move || checksum::hash_file(&dest_path, algo))
            .await
            .map_err(|e| format!("hash task failed: {e}"))??;
        let verified = hex == exp.hex;
        let mut g = handle.inner.lock().await;
        g.computed_hash = Some(hex.clone());
        g.verify_state = if verified {
            VerifyState::Verified
        } else {
            VerifyState::Mismatch
        };
        if !verified {
            g.status = "failed".to_string();
            g.error = Some(format!(
                "checksum mismatch (expected {}…, got {}…)",
                checksum::short_hash(&exp.hex),
                checksum::short_hash(&hex)
            ));
        } else if g.status == "failed"
            && g.error
                .as_deref()
                .is_some_and(|e| e.starts_with("checksum mismatch"))
        {
            // The file was fixed on disk and now matches: it is a
            // completed download again.
            g.status = "completed".to_string();
            g.error = None;
        }
        update_db(&g);
        Ok(VerifyResult {
            verified,
            computed_hash: hex,
            expected_hash: exp.hex,
            verify_state: g.verify_state,
        })
    }

    /// Re-scan a finished download with ClamAV: scan the file on
    /// disk, persist the outcome. A virus marks the download failed
    /// (file kept on disk), exactly like the automatic scan after a
    /// download. A clean re-scan flips a virus-failed download back
    /// to completed — the same flip-back the checksum re-verify has.
    pub async fn scan_download(&self, id: &str) -> Result<scan::ScanResult, String> {
        let handle = self
            .tasks
            .get(id)
            .cloned()
            .ok_or_else(|| "download not found".to_string())?;
        let dest_path = {
            let g = handle.inner.lock().await;
            g.dest_path.clone()
        };
        if !dest_path.is_file() {
            return Err("downloaded file not found on disk".to_string());
        }
        let result = tokio::task::spawn_blocking(move || scan::scan_file(&dest_path))
            .await
            .map_err(|e| format!("scan task failed: {e}"))?;
        let mut g = handle.inner.lock().await;
        apply_scan_result(&mut g, result.clone());
        if result.state == ScanState::Clean
            && g.status == "failed"
            && g.error
                .as_deref()
                .is_some_and(|e| e.starts_with("Virus terdeteksi"))
        {
            // The file was cleaned or replaced on disk and now scans
            // clean: it is a completed download again.
            g.status = "completed".to_string();
            g.error = None;
        }
        update_db(&g);
        Ok(result)
    }

    pub async fn stats(&self) -> Stats {
        let mut active = 0u64;
        let mut queued = 0u64;
        let mut total_speed = 0.0f64;
        for h in self.tasks.values() {
            let g = h.inner.lock().await;
            match g.status.as_str() {
                "downloading" => {
                    active += 1;
                    total_speed += g.speed_bps;
                }
                "queued" => queued += 1,
                _ => {}
            }
        }
        let db_path = self.db_path();
        let downloaded_today = db::downloaded_today(&db_path);
        let completed_total = db::list_downloads(&db_path)
            .map(|rows| rows.iter().filter(|r| r.status == "completed").count() as i64)
            .unwrap_or(0);
        Stats {
            active,
            total_speed_bps: total_speed,
            downloaded_today,
            queued,
            completed_total,
        }
    }

    /// Drain the browser queue file (`<download_dir>/queue.json`,
    /// written by `browser/zdm-native-host.py`): consume every URL not
    /// already tracked. Returns `(tasks_started, newly_pending)`.
    ///
    /// When `ask_before_download` is ON (the default), intercepted
    /// URLs become PENDING items for the confirmation popup instead of
    /// tasks — nothing downloads until the user confirms. The queue is
    /// not auto-started in that case.
    ///
    /// When it is OFF, the old behaviour applies: browser arrivals
    /// auto-start the queue — the user explicitly asked for the
    /// download in the browser, so it should start immediately
    /// (subject to `max_concurrent`, enforced by `queue_tick`).
    ///
    /// Invalid URLs are dropped with a stderr note, never retried.
    /// The drain takes the shared queue lock, so a browser download
    /// landing mid-drain is never lost; if the lock is held, this
    /// tick is skipped and retried on the next poll.
    pub async fn drain_queue_file(&mut self) -> (Vec<Arc<TaskHandle>>, Vec<PendingDownload>) {
        let path = crate::queue_file_path(&self.settings);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return (Vec::new(), Vec::new());
        };
        let urls = crate::parse_queue_file(&text);
        if urls.is_empty() {
            // Corrupt or empty file: clear it so we stop re-reading it.
            let _ = std::fs::write(&path, "[]");
            return (Vec::new(), Vec::new());
        }
        let Some(lock) = crate::try_queue_lock(&self.settings) else {
            return (Vec::new(), Vec::new());
        };
        // Re-read under the lock: a URL appended after the first read
        // survives — only the URLs seen here are consumed.
        let urls = std::fs::read_to_string(&path)
            .map(|t| crate::parse_queue_file(&t))
            .unwrap_or_default();
        let mut added: Vec<Arc<TaskHandle>> = Vec::new();
        let mut pendings: Vec<PendingDownload> = Vec::new();
        if !urls.is_empty() {
            let mut existing: Vec<String> = Vec::new();
            for h in self.tasks.values() {
                existing.push(h.inner.lock().await.url.clone());
            }
            for url in &urls {
                if existing.contains(url) {
                    continue;
                }
                if self.settings.ask_before_download {
                    match self.add_pending(url) {
                        Ok(Some(p)) => {
                            existing.push(url.clone());
                            pendings.push(p);
                        }
                        Ok(None) => {
                            // Already pending — deduped silently.
                        }
                        Err(e) => {
                            eprintln!("[zdm] queue file: skipping invalid URL {url}: {e}");
                        }
                    }
                } else {
                    match self.add(url, None) {
                        Ok(h) => {
                            existing.push(url.clone());
                            added.push(h);
                        }
                        Err(e) => {
                            eprintln!("[zdm] queue file: skipping invalid URL {url}: {e}");
                        }
                    }
                }
            }
            // Write back only URLs that arrived after our read.
            let rest: Vec<String> = std::fs::read_to_string(&path)
                .map(|t| crate::parse_queue_file(&t))
                .unwrap_or_default()
                .into_iter()
                .filter(|u| !urls.contains(u))
                .collect();
            let _ = std::fs::write(
                &path,
                serde_json::to_string(&rest).unwrap_or_else(|_| "[]".to_string()),
            );
            if !added.is_empty() {
                self.queue_running = true;
            }
        }
        let _ = std::fs::remove_file(&lock);
        (added, pendings)
    }
}

/// What the daily scheduler decided on this tick. `Stop` means the
/// caller must also pause running downloads (done in `lib.rs`,
/// outside the manager lock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleAction {
    Start,
    Stop,
    None,
}

impl Manager {
    /// One daily-schedule tick (called ~every 30 s from the queue
    /// loop). Pure decision, except for flipping `queue_running`:
    /// inside the window and stopped -> start; outside the window
    /// and running -> stop. Disabled schedule, manual hold, and
    /// no-change all return `None`. A window boundary clears a
    /// manual hold, so a deliberate Start/Stop only holds until the
    /// next transition.
    pub fn schedule_tick(&mut self, now: chrono::DateTime<chrono::Local>) -> ScheduleAction {
        let sched = &self.settings.schedule;
        if !sched.enabled {
            self.schedule_last_in_window = None;
            return ScheduleAction::None;
        }
        let in_window = sched.contains(now);
        match self.schedule_last_in_window {
            Some(prev) if prev != in_window => {
                self.schedule_hold = false;
                self.schedule_last_in_window = Some(in_window);
            }
            None => self.schedule_last_in_window = Some(in_window),
            _ => {}
        }
        if self.schedule_hold {
            return ScheduleAction::None;
        }
        if in_window && !self.queue_running {
            self.queue_running = true;
            ScheduleAction::Start
        } else if !in_window && self.queue_running {
            self.queue_running = false;
            ScheduleAction::Stop
        } else {
            ScheduleAction::None
        }
    }

    /// Ids (queue order) the queue would start right now: queued
    /// tasks, plus paused tasks that were interrupted by a shutdown
    /// or paused by the daily scheduler. A manually paused, failed
    /// or completed task never appears here. Pure decision over
    /// in-memory state — no I/O, no spawning; the concurrent-slot
    /// cap is applied by `queue_tick`, not here. A contended task
    /// lock means the task is mid-transition — it is skipped this
    /// once and the next queue tick picks it up.
    pub fn resume_candidate_ids(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|id| {
                let Some(h) = self.tasks.get(*id) else {
                    return false;
                };
                let Ok(g) = h.inner.try_lock() else {
                    return false;
                };
                g.status == "queued"
                    || (g.status == "paused" && (g.interrupted || g.paused_by_schedule))
            })
            .cloned()
            .collect()
    }

    /// Auto-resume at startup: interrupted and queued downloads
    /// continue from where they left off, immediately — the queue
    /// loop's 500 ms tick starts them within the concurrent slots,
    /// no waiting for the ~30 s scheduler tick. Returns true when
    /// the queue was started. Nothing starts when the setting is
    /// OFF, when there is nothing resumable, or when an enabled
    /// daily schedule is currently outside its window (the
    /// scheduler takes over at the next window open, and
    /// `queue_tick` then resumes the interrupted tasks too).
    /// A completed download is never touched — it is not a
    /// candidate and `restore_from_db` never rebuilds it as one.
    pub fn startup_resume(&mut self, now: chrono::DateTime<chrono::Local>) -> bool {
        if !self.settings.resume_on_startup {
            return false;
        }
        if self.settings.schedule.enabled && !self.settings.schedule.contains(now) {
            return false;
        }
        if self.resume_candidate_ids().is_empty() {
            return false;
        }
        self.queue_running = true;
        true
    }

    pub async fn queue_tick(&mut self) {
        if !self.queue_running {
            return;
        }
        if let Some(when) = self.scheduled_at {
            if chrono::Local::now().naive_local() < when {
                return;
            }
            self.scheduled_at = None;
            // The one-shot fired: the daily window takes over again.
            self.schedule_hold = false;
        }
        let active = self
            .tasks
            .values()
            .filter(|h| {
                // Cheap check without awaiting each lock: use try_lock.
                h.inner
                    .try_lock()
                    .map(|g| g.status == "downloading")
                    .unwrap_or(false)
            })
            .count();
        let mut slots = self.settings.max_concurrent as usize;
        slots = slots.saturating_sub(active);
        if slots == 0 {
            return;
        }
        let mut to_start: Vec<Arc<TaskHandle>> = Vec::new();
        for id in self.order.clone() {
            if to_start.len() >= slots {
                break;
            }
            if let Some(h) = self.tasks.get(&id) {
                let g = h.inner.lock().await;
                // Queued tasks start; tasks the daily scheduler
                // paused resume when the window opens again, as do
                // tasks interrupted by a shutdown (their .part
                // offset is restored, so they continue, not
                // restart). A user-paused task never auto-starts.
                if g.status == "queued"
                    || (g.status == "paused" && (g.paused_by_schedule || g.interrupted))
                {
                    to_start.push(h.clone());
                }
            }
        }
        for h in to_start {
            Self::start_task(
                h,
                self.settings.clone(),
                self.limiter.clone(),
                self.app.clone(),
            )
            .await;
        }
    }

    /// Spawn the runner for a task (no-op if already downloading).
    /// Static on purpose: callers must not hold the manager lock while
    /// a task runs — the runner lives for the whole download.
    pub async fn start_task(
        handle: Arc<TaskHandle>,
        settings: Settings,
        limiter: Arc<AsyncMutex<RateLimiter>>,
        app: Option<AppHandle>,
    ) {
        {
            let g = handle.inner.lock().await;
            if g.status == "downloading" {
                return;
            }
        }
        handle.stop.store(false, Ordering::SeqCst);
        handle.cancel.store(false, Ordering::SeqCst);
        {
            let mut g = handle.inner.lock().await;
            g.status = "downloading".to_string();
            g.error = None;
            g.paused_by_schedule = false;
            g.interrupted = false;
            // Persist the cleared flags now: if the app is killed
            // again before the runner's first save, the DB must not
            // still claim this task was scheduler-paused.
            update_db(&g);
        }
        let h2 = handle.clone();
        let join = tokio::spawn(async move {
            run_task(h2, app, limiter, settings).await;
        });
        *handle.join.lock().await = Some(join);
    }

    /// Pause a task: signal stop, wait for the runner, persist state.
    /// Static — does not need the manager lock.
    pub async fn pause_task(handle: Arc<TaskHandle>) {
        handle.stop.store(true, Ordering::SeqCst);
        if let Some(j) = handle.join.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(30), j).await;
        }
        let mut g = handle.inner.lock().await;
        if !handle.cancel.load(Ordering::SeqCst) && g.status == "downloading" {
            g.status = "paused".to_string();
            // A deliberate pause is a manual pause from now on: even
            // a task that was interrupted by a shutdown must not be
            // auto-started again by the queue once the user paused it.
            g.interrupted = false;
            save_state(&mut g, true);
            update_db(&g);
        }
    }

    /// Pause a task because the daily window ended: like
    /// `pause_task`, but marks it so the queue resumes it when the
    /// window opens again (see `queue_tick`).
    pub async fn pause_task_scheduled(handle: Arc<TaskHandle>) {
        Self::pause_task(handle.clone()).await;
        let mut g = handle.inner.lock().await;
        if g.status == "paused" {
            g.paused_by_schedule = true;
            // `pause_task` already saved with the flag clear; persist
            // the set flag now so a restart still knows this pause
            // was the scheduler's.
            save_state(&mut g, true);
            update_db(&g);
        }
    }

    /// Cancel a task: signal stop+cancel, wait for the runner, delete
    /// the part file and sidecar. The finished file (if any) is kept.
    /// Static — does not need the manager lock.
    pub async fn cancel_task(handle: Arc<TaskHandle>) {
        handle.cancel.store(true, Ordering::SeqCst);
        handle.stop.store(true, Ordering::SeqCst);
        if let Some(j) = handle.join.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(30), j).await;
        }
        let mut g = handle.inner.lock().await;
        g.status = "cancelled".to_string();
        let _ = std::fs::remove_file(&g.part_path);
        let _ = std::fs::remove_file(&g.state_path);
        let _ = std::fs::remove_dir_all(stream::segments_dir(&g.dest_path));
        update_db(&g);
    }
}

// ---------------------------------------------------------------- persistence

/// Apply a settled ClamAV scan outcome to a task (shared by the
/// automatic scan after a download and the manual re-scan). A virus
/// fails a download that is still completed — the file stays on disk,
/// only the status says it cannot be trusted. Any other status is
/// left alone: the user may have retried or deleted the download
/// while the scan was running.
fn apply_scan_result(g: &mut TaskInner, result: scan::ScanResult) {
    g.scan_state = result.state;
    g.threat_name = result.threat.clone();
    if result.state == ScanState::Found && g.status == "completed" {
        g.status = "failed".to_string();
        g.error = Some(format!(
            "Virus terdeteksi: {}",
            result.threat.as_deref().unwrap_or("unknown threat")
        ));
    }
}

fn update_db(g: &TaskInner) {
    let Some(db_id) = g.db_id else {
        return;
    };
    let db_path = db::db_path_for(&g.dest_dir);
    let _ = db::update_download(&db_path, db_id, g.downloaded() as i64, g.total.map(|t| t as i64), &g.status, g.paused_by_schedule);
    let _ = db::update_verification(
        &db_path,
        db_id,
        g.expected_hash.as_ref().map(|e| e.hex.as_str()),
        g.computed_hash.as_deref(),
        g.verify_state.as_str(),
    );
    // Only settled scan states are persisted: a restart must never
    // restore a "scanning" that is no longer running.
    if g.scan_state != ScanState::Scanning {
        let _ = db::update_scan(
            &db_path,
            db_id,
            g.scan_state.as_str(),
            g.threat_name.as_deref(),
        );
    }
    // Stream kind, so the registry (shared with the Python ZDM)
    // knows a row is a merged stream even when the sidecar is gone.
    if let Some(plan) = &g.stream {
        let _ = db::update_stream(&db_path, db_id, plan.kind.as_str());
    }
    // Failure reason, so a failed row still explains itself after
    // a restart (restore_from_db reads it back into the view).
    if let Some(err) = &g.error {
        let _ = db::update_error(&db_path, db_id, err);
    }
}

fn save_state(g: &mut TaskInner, force: bool) {
    // A stream task has no byte-range total until its playlist is
    // parsed, but its plan still needs persisting (segment done
    // flags are the resume record). A stream fallback note is also
    // worth persisting on its own: the playlist-only download has
    // no total and no plan, but its sidecar is the note's durable
    // home (the success path keeps it — see run_task).
    if g.state_path.as_os_str().is_empty()
        || (g.total.is_none() && g.stream.is_none() && g.stream_note.is_none())
    {
        return;
    }
    let now = Instant::now();
    if !force && now.duration_since(g.last_save) < Duration::from_secs(1) {
        return;
    }
    g.last_save = now;
    let sc = Sidecar {
        url: g.url.clone(),
        filename: g.filename.clone(),
        total: g.total,
        status: g.status.clone(),
        segments: g.segments.iter().map(|s| Segment { assigned: false, ..s.clone() }).collect(),
        downloaded_single: g.downloaded_single,
        expected_hash: g.expected_hash.clone(),
        computed_hash: g.computed_hash.clone(),
        verify_state: g.verify_state,
        // "scanning" is transient: persist it as "unavailable" so a
        // restart never restores a scan that is no longer running.
        scan_state: if g.scan_state == ScanState::Scanning {
            ScanState::Unavailable
        } else {
            g.scan_state
        },
        threat_name: g.threat_name.clone(),
        stream: g.stream.clone(),
        stream_note: g.stream_note.clone(),
    };
    if let Ok(text) = serde_json::to_string(&sc) {
        let tmp = g.state_path.with_extension("download.json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &g.state_path);
        }
    }
    update_db(g);
}

fn emit_progress(app: &Option<AppHandle>, g: &TaskInner) {
    let Some(app) = app else {
        return;
    };
    let ev = ProgressEvent {
        id: g.id.clone(),
        filename: g.filename.clone(),
        downloaded: g.downloaded(),
        total: g.total,
        speed_bps: g.speed_bps,
        eta_secs: g.eta_secs(),
        status: g.status.clone(),
    };
    let _ = app.emit("download-progress", ev);
}

fn note_bytes(g: &mut TaskInner, app: &Option<AppHandle>, n: usize) {
    g.bytes_since_tick += n as u64;
    let now = Instant::now();
    if now.duration_since(g.last_tick) >= Duration::from_millis(500) {
        let elapsed = now.duration_since(g.last_tick).as_secs_f64();
        if elapsed > 0.0 {
            g.speed_bps = g.bytes_since_tick as f64 / elapsed;
        }
        g.bytes_since_tick = 0;
        g.last_tick = now;
    }
    if now.duration_since(g.last_emit) >= Duration::from_millis(250) {
        g.last_emit = now;
        emit_progress(app, g);
    }
}

// ---------------------------------------------------------------- streams

/// Fetch + parse a stream manifest into a resolved plan. HLS master
/// playlists resolve to their highest-bandwidth variant first.
async fn build_stream_plan(
    client: &reqwest::Client,
    url: &str,
    auth: &Option<String>,
    kind: stream::StreamKind,
) -> Result<stream::ResolvedPlan, String> {
    let fetch = async |u: &str| -> Result<String, String> {
        let mut req = client.get(u);
        if let Some(a) = auth {
            req = req.header("Authorization", a.clone());
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("gagal mengunduh playlist: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("playlist HTTP {}", resp.status()));
        }
        resp.text()
            .await
            .map_err(|e| format!("gagal membaca playlist: {e}"))
    };
    match kind {
        stream::StreamKind::Hls => {
            let text = fetch(url).await?;
            let (media_url, media) = match stream::parse_hls_master(&text, url)? {
                Some(variant) => {
                    let mtext = fetch(&variant).await?;
                    (
                        variant.clone(),
                        stream::parse_hls_media(&mtext, &variant)?,
                    )
                }
                None => (url.to_string(), stream::parse_hls_media(&text, url)?),
            };
            let mut keys: Vec<(String, Option<String>)> = Vec::new();
            let segments: Vec<stream::ResolvedSegment> = media
                .segments
                .iter()
                .map(|s| {
                    if let Some(k) = &s.key {
                        if !keys.contains(k) {
                            keys.push(k.clone());
                        }
                    }
                    stream::ResolvedSegment {
                        url: s.url.clone(),
                        duration: s.duration,
                        key: s.key.clone(),
                    }
                })
                .collect();
            let total_duration = segments.iter().map(|s| s.duration).sum();
            let is_fmp4 = media.map.is_some();
            Ok(stream::ResolvedPlan {
                kind,
                playlist_url: media_url,
                segments,
                init: media.map,
                audio_segments: Vec::new(),
                audio_init: None,
                keys,
                total_duration,
                is_fmp4,
            })
        }
        stream::StreamKind::Dash => {
            let text = fetch(url).await?;
            let dash = stream::parse_mpd(&text, url)?;
            let to_resolved = |t: &stream::DashTrack| {
                t.segments
                    .iter()
                    .map(|(u, d)| stream::ResolvedSegment {
                        url: u.clone(),
                        duration: *d,
                        key: None,
                    })
                    .collect::<Vec<_>>()
            };
            let video = to_resolved(&dash.video);
            let audio = dash.audio.as_ref().map(to_resolved).unwrap_or_default();
            let total_duration = video
                .iter()
                .map(|s| s.duration)
                .sum::<f64>()
                .max(audio.iter().map(|s| s.duration).sum());
            Ok(stream::ResolvedPlan {
                kind,
                playlist_url: url.to_string(),
                segments: video,
                init: if dash.video.init_url.is_empty() {
                    None
                } else {
                    Some(dash.video.init_url)
                },
                audio_segments: audio,
                audio_init: dash.audio.and_then(|t| {
                    if t.init_url.is_empty() {
                        None
                    } else {
                        Some(t.init_url)
                    }
                }),
                keys: Vec::new(),
                total_duration,
                is_fmp4: true,
            })
        }
    }
}

/// One unit of stream download work.
enum StreamJob {
    Main(usize),
    Audio(usize),
    Init,
    AudioInit,
    Key(usize),
}

/// Learn segment sizes with parallel HEAD requests (best effort —
/// a server that refuses HEAD still serves the segments). When every
/// size is known the task total is set, so the progress bar is
/// meaningful; otherwise only the segment counter drives the UI.
async fn head_stream_sizes(handle: &Arc<TaskHandle>, client: &reqwest::Client) {
    let jobs: Vec<(StreamJob, String)> = {
        let g = handle.inner.lock().await;
        let Some(plan) = g.stream.clone() else {
            return;
        };
        let mut out = Vec::new();
        if let Some(init) = &plan.init {
            if init.size.is_none() {
                out.push((StreamJob::Init, init.url.clone()));
            }
        }
        for (i, s) in plan.segments.iter().enumerate() {
            if s.size.is_none() {
                out.push((StreamJob::Main(i), s.url.clone()));
            }
        }
        if let Some(init) = &plan.audio_init {
            if init.size.is_none() {
                out.push((StreamJob::AudioInit, init.url.clone()));
            }
        }
        for (i, s) in plan.audio_segments.iter().enumerate() {
            if s.size.is_none() {
                out.push((StreamJob::Audio(i), s.url.clone()));
            }
        }
        out
    };
    if jobs.is_empty() {
        return;
    }
    let auth = handle.inner.lock().await.auth.clone();
    let mut set = tokio::task::JoinSet::new();
    for (job, url) in jobs {
        let c = client.clone();
        let a = auth.clone();
        set.spawn(async move {
            let mut req = c.head(&url);
            if let Some(a) = &a {
                req = req.header("Authorization", a.clone());
            }
            let size = tokio::time::timeout(Duration::from_secs(10), req.send())
                .await
                .ok()
                .and_then(|r| r.ok())
                .and_then(|resp| {
                    if resp.status().is_success() {
                        resp.headers()
                            .get(reqwest::header::CONTENT_LENGTH)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<u64>().ok())
                    } else {
                        None
                    }
                });
            (job, size)
        });
    }
    let mut all_known = true;
    while let Some(Ok((job, size))) = set.join_next().await {
        let mut g = handle.inner.lock().await;
        let Some(plan) = g.stream.as_mut() else {
            continue;
        };
        let slot: Option<&mut stream::StreamSegment> = match job {
            StreamJob::Main(i) => plan.segments.get_mut(i),
            StreamJob::Audio(i) => plan.audio_segments.get_mut(i),
            StreamJob::Init => plan.init.as_mut(),
            StreamJob::AudioInit => plan.audio_init.as_mut(),
            StreamJob::Key(_) => None,
        };
        if let Some(seg) = slot {
            if let Some(size) = size {
                seg.size = Some(size);
            } else {
                all_known = false;
            }
        }
    }
    // Keys are tiny; they do not gate the total.
    let mut g = handle.inner.lock().await;
    if all_known {
        if let Some(plan) = &g.stream {
            let known: u64 = plan
                .segments
                .iter()
                .chain(plan.audio_segments.iter())
                .chain(plan.init.iter())
                .chain(plan.audio_init.iter())
                .filter_map(|s| s.size)
                .sum();
            if known > 0 {
                g.total = Some(known);
            }
        }
    }
    save_state(&mut g, true);
    update_db(&g);
}

/// Download one stream file (segment, init, or key) with resume:
/// a file already complete on disk is skipped, a partial file is
/// continued with a Range request (a server that ignores Range
/// restarts the file from zero). Returns Ok when the file on disk is
/// complete; Err after MAX_RETRIES.
#[allow(clippy::too_many_arguments)]
async fn download_stream_file(
    handle: &Arc<TaskHandle>,
    client: &reqwest::Client,
    limiter: &Arc<AsyncMutex<RateLimiter>>,
    app: &Option<AppHandle>,
    url: &str,
    auth: &Option<String>,
    file: &Path,
    expected_size: Option<u64>,
) -> Result<(), String> {
    if let Some(size) = expected_size {
        if let Ok(meta) = std::fs::metadata(file) {
            if meta.len() == size {
                return Ok(());
            }
        }
    }
    let mut retries = 0u32;
    loop {
        if handle.stop.load(Ordering::SeqCst) || handle.cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        let start = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
        let mut req = client.get(url);
        if start > 0 {
            req = req.header("Range", format!("bytes={start}-"));
        }
        if let Some(a) = auth {
            req = req.header("Authorization", a.clone());
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                retries += 1;
                if retries > MAX_RETRIES {
                    return Err(format!("segmen {url} gagal setelah {retries} percobaan: {e}"));
                }
                tokio::time::sleep(Duration::from_secs(1 << retries.min(4))).await;
                continue;
            }
        };
        if !resp.status().is_success() {
            return Err(format!("segmen {url} HTTP {}", resp.status()));
        }
        // A server that ignores Range answers 200 with the body
        // starting at byte 0: truncate and start over instead of
        // appending a full body after a partial prefix.
        let restart = start > 0 && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT;
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(restart)
            .open(file)
            .map_err(|e| format!("gagal membuka {file:?}: {e}"))?;
        let mut write_pos = if restart { 0 } else { start };
        if !restart {
            use std::io::Seek;
            f.seek(SeekFrom::Start(write_pos))
                .map_err(|e| format!("seek gagal: {e}"))?;
        }
        let mut stream = resp.bytes_stream();
        let mut failed = false;
        loop {
            if handle.stop.load(Ordering::SeqCst) || handle.cancel.load(Ordering::SeqCst) {
                return Ok(());
            }
            let chunk = match stream.next().await {
                Some(Ok(c)) => c,
                Some(Err(e)) => {
                    retries += 1;
                    if retries > MAX_RETRIES {
                        return Err(format!(
                            "segmen {url} gagal setelah {retries} percobaan: {e}"
                        ));
                    }
                    tokio::time::sleep(Duration::from_secs(1 << retries.min(4))).await;
                    failed = true;
                    break;
                }
                None => break,
            };
            if chunk.is_empty() {
                continue;
            }
            let n = chunk.len();
            RateLimiter::acquire(limiter, n).await;
            {
                use std::io::Seek;
                if f.seek(SeekFrom::Start(write_pos)).is_err() {
                    return Err("seek gagal pada file segmen".to_string());
                }
                if f.write_all(&chunk).is_err() {
                    return Err("gagal menulis file segmen".to_string());
                }
            }
            write_pos += n as u64;
            {
                let mut g = handle.inner.lock().await;
                g.downloaded_single += n as u64;
                note_bytes(&mut g, app, n);
                save_state(&mut g, false);
            }
            retries = 0;
        }
        if failed {
            continue;
        }
        drop(f);
        if let Some(size) = expected_size {
            let got = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
            if got != size {
                return Err(format!(
                    "segmen {url} ukurannya {got}, seharusnya {size}"
                ));
            }
        }
        return Ok(());
    }
}

/// Download every unfinished stream file (init + keys + segments),
/// up to `connections` in parallel. A segment whose file is already
/// complete on disk is never fetched again — that is what makes a
/// pause / crash resume cheap.
async fn download_stream_segments(
    handle: &Arc<TaskHandle>,
    client: &reqwest::Client,
    limiter: &Arc<AsyncMutex<RateLimiter>>,
    app: &Option<AppHandle>,
    seg_dir: &Path,
) -> Result<(), String> {
    let (jobs, auth, connections) = {
        let g = handle.inner.lock().await;
        let Some(plan) = g.stream.clone() else {
            return Err("stream task without a plan".to_string());
        };
        let mut jobs: Vec<StreamJob> = Vec::new();
        // A segment counts as finished only when its file is really
        // there at the expected size: the `done` flag alone is not
        // proof (a crash between the write and the save, or a
        // truncated file, must re-download, not merge a stub).
        let file_complete = |file: &str, size: Option<u64>| -> bool {
            match std::fs::metadata(seg_dir.join(file)) {
                Ok(m) => match size {
                    Some(s) => m.len() == s,
                    None => m.len() > 0,
                },
                Err(_) => false,
            }
        };
        if let Some(init) = &plan.init {
            if !init.done || !file_complete(&init.file, init.size) {
                jobs.push(StreamJob::Init);
            }
        }
        for (i, s) in plan.segments.iter().enumerate() {
            if !s.done || !file_complete(&s.file, s.size) {
                jobs.push(StreamJob::Main(i));
            }
        }
        if let Some(init) = &plan.audio_init {
            if !init.done || !file_complete(&init.file, init.size) {
                jobs.push(StreamJob::AudioInit);
            }
        }
        for (i, s) in plan.audio_segments.iter().enumerate() {
            if !s.done || !file_complete(&s.file, s.size) {
                jobs.push(StreamJob::Audio(i));
            }
        }
        for (i, k) in plan.keys.iter().enumerate() {
            let kf = seg_dir.join(&k.file);
            if !kf.exists() || std::fs::metadata(&kf).map(|m| m.len()).unwrap_or(0) == 0 {
                jobs.push(StreamJob::Key(i));
            }
        }
        (jobs, g.auth.clone(), g.connections.max(1) as usize)
    };
    if jobs.is_empty() {
        return Ok(());
    }
    let jobs = std::sync::Arc::new(jobs);
    let next = std::sync::Arc::new(AtomicUsize::new(0));
    let abort = std::sync::Arc::new(AtomicBool::new(false));
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..connections {
        let h = handle.clone();
        let c = client.clone();
        let l = limiter.clone();
        let a = app.clone();
        let jobs = jobs.clone();
        let next = next.clone();
        let abort = abort.clone();
        let auth = auth.clone();
        let seg_dir = seg_dir.to_path_buf();
        set.spawn(async move {
            loop {
                if abort.load(Ordering::SeqCst)
                    || h.stop.load(Ordering::SeqCst)
                    || h.cancel.load(Ordering::SeqCst)
                {
                    return Ok::<(), String>(());
                }
                let idx = next.fetch_add(1, Ordering::SeqCst);
                let Some(job) = jobs.get(idx) else {
                    return Ok::<(), String>(());
                };
                // Resolve the job to (url, file, expected size).
                let (url, file, size): (String, PathBuf, Option<u64>) = {
                    let g = h.inner.lock().await;
                    let Some(plan) = &g.stream else {
                        return Err("stream plan hilang".to_string());
                    };
                    match job {
                        StreamJob::Main(i) => {
                            let s = &plan.segments[*i];
                            (s.url.clone(), seg_dir.join(&s.file), s.size)
                        }
                        StreamJob::Audio(i) => {
                            let s = &plan.audio_segments[*i];
                            (s.url.clone(), seg_dir.join(&s.file), s.size)
                        }
                        StreamJob::Init => {
                            let s = plan.init.as_ref().unwrap();
                            (s.url.clone(), seg_dir.join(&s.file), s.size)
                        }
                        StreamJob::AudioInit => {
                            let s = plan.audio_init.as_ref().unwrap();
                            (s.url.clone(), seg_dir.join(&s.file), s.size)
                        }
                        StreamJob::Key(i) => {
                            let k = &plan.keys[*i];
                            (k.url.clone(), seg_dir.join(&k.file), None)
                        }
                    }
                };
                let r = download_stream_file(&h, &c, &l, &a, &url, &auth, &file, size).await;
                if let Err(e) = r {
                    abort.store(true, Ordering::SeqCst);
                    return Err(e);
                }
                // Mark the file done in the plan (keys have no flag:
                // their presence on disk is the record).
                let mut g = h.inner.lock().await;
                if let Some(plan) = g.stream.as_mut() {
                    match job {
                        StreamJob::Main(i) => {
                            if let Some(s) = plan.segments.get_mut(*i) {
                                s.done = true;
                                s.size = Some(std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0));
                            }
                        }
                        StreamJob::Audio(i) => {
                            if let Some(s) = plan.audio_segments.get_mut(*i) {
                                s.done = true;
                                s.size = Some(std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0));
                            }
                        }
                        StreamJob::Init => {
                            if let Some(s) = plan.init.as_mut() {
                                s.done = true;
                                s.size = Some(std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0));
                            }
                        }
                        StreamJob::AudioInit => {
                            if let Some(s) = plan.audio_init.as_mut() {
                                s.done = true;
                                s.size = Some(std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0));
                            }
                        }
                        StreamJob::Key(_) => {}
                    }
                    save_state(&mut g, true);
                    update_db(&g);
                }
            }
        });
    }
    let mut first_err: Option<String> = None;
    while let Some(res) = set.join_next().await {
        if let Ok(Err(e)) = res {
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    Ok(())
}

/// Stream pipeline: fetch the playlist, build the segment plan (or
/// reuse the sidecar's), download segments in parallel with resume,
/// merge into one file at the .part path. Returns Ok on success and
/// on stop/cancel (the finalise in run_task settles the status);
/// Err only for a real failure (live stream, bad playlist, merge
/// failure, …).
async fn run_stream(
    handle: &Arc<TaskHandle>,
    client: &reqwest::Client,
    limiter: &Arc<AsyncMutex<RateLimiter>>,
    app: &Option<AppHandle>,
) -> Result<(), String> {
    let (seg_dir, part_path) = {
        let g = handle.inner.lock().await;
        (stream::segments_dir(&g.dest_path), g.part_path.clone())
    };
    // Already merged (killed between merge and rename)? The merged
    // file is at the .part path — skip straight to the finalise.
    let merged_done = {
        let g = handle.inner.lock().await;
        g.stream.as_ref().map(|p| p.merged).unwrap_or(false)
            && std::fs::metadata(&part_path).is_ok()
    };
    if merged_done {
        let size = std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
        let mut g = handle.inner.lock().await;
        g.total = Some(size);
        g.downloaded_single = size;
        return Ok(());
    }
    // A stale merged flag with no file: re-merge from the segments.
    {
        let mut g = handle.inner.lock().await;
        if let Some(p) = g.stream.as_mut() {
            if p.merged {
                p.merged = false;
            }
        }
    }
    std::fs::create_dir_all(&seg_dir)
        .map_err(|e| format!("gagal membuat folder segmen: {e}"))?;

    // 1. Plan: reuse the sidecar's, or build it fresh (prepare
    // normally builds it, so this only runs defensively).
    let mut plan = {
        let g = handle.inner.lock().await;
        g.stream.clone().unwrap()
    };
    if plan.segments.is_empty() {
        let (url, auth, kind) = {
            let g = handle.inner.lock().await;
            (g.url.clone(), g.auth.clone(), plan.kind)
        };
        let resolved = build_stream_plan(client, &url, &auth, kind).await?;
        if !resolved.keys.is_empty() && !stream::ffmpeg_available() {
            return Err(
                "stream terenkripsi AES-128 butuh ffmpeg — instal ffmpeg untuk mengunduh stream ini"
                    .to_string(),
            );
        }
        plan = stream::StreamPlan::from_resolved(resolved, &plan.output_ext);
        let mut g = handle.inner.lock().await;
        g.stream = Some(plan.clone());
        save_state(&mut g, true);
        update_db(&g);
    }
    if plan.encrypted() && !stream::ffmpeg_available() {
        return Err(
            "stream terenkripsi AES-128 butuh ffmpeg — instal ffmpeg untuk mengunduh stream ini"
                .to_string(),
        );
    }

    // 2. Learn segment sizes (HEAD, best effort), then download
    // every unfinished segment in parallel.
    head_stream_sizes(handle, client).await;
    download_stream_segments(handle, client, limiter, app, &seg_dir).await?;
    if handle.stop.load(Ordering::SeqCst) || handle.cancel.load(Ordering::SeqCst) {
        // Paused/cancelled mid-stream: the finalise in run_task
        // settles the status; the sidecar already holds the plan.
        let mut g = handle.inner.lock().await;
        save_state(&mut g, true);
        return Ok(());
    }
    // Every key file must be on disk before the merge (the local
    // playlist references them by name).
    for key in &plan.keys {
        if !seg_dir.join(&key.file).exists() {
            return Err(format!("file kunci {} tidak terunduh", key.file));
        }
    }

    // 3. Merge on a blocking thread (ffmpeg is a subprocess; the
    // concat itself is plain file I/O).
    let (plan2, seg_dir2, part2) = {
        let g = handle.inner.lock().await;
        (
            g.stream.clone().unwrap(),
            seg_dir.clone(),
            part_path.clone(),
        )
    };
    tokio::task::spawn_blocking(move || stream::merge_stream(&plan2, &seg_dir2, &part2))
        .await
        .map_err(|e| format!("merge gagal: {e}"))??;
    let size = std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
    {
        let mut g = handle.inner.lock().await;
        if let Some(p) = g.stream.as_mut() {
            p.merged = true;
        }
        g.total = Some(size);
        g.downloaded_single = size;
        save_state(&mut g, true);
        update_db(&g);
    }
    let _ = std::fs::remove_dir_all(&seg_dir);
    Ok(())
}

// ---------------------------------------------------------------- runner

async fn run_task(
    handle: Arc<TaskHandle>,
    app: Option<AppHandle>,
    limiter: Arc<AsyncMutex<RateLimiter>>,
    settings: Settings,
) {
    if let Err(e) = prepare(&handle, &settings).await {
        let mut g = handle.inner.lock().await;
        g.error = Some(e);
        g.status = "failed".to_string();
        // A failed prepare (live playlist, encrypted stream without
        // ffmpeg) still leaves a row: without it the failure is
        // invisible in the Completed list after a restart, because
        // restore_from_db reads the registry, not the in-memory
        // tasks — and the row carries the reason via update_db.
        if g.db_id.is_none() {
            let db_path = db::db_path_for(&g.dest_dir);
            if let Ok(id) = db::insert_download(
                &db_path,
                &g.url,
                &g.filename,
                &g.dest_path.to_string_lossy(),
                g.total.map(|t| t as i64),
                &g.category,
            ) {
                g.db_id = Some(id);
            }
        }
        update_db(&g);
        emit_progress(&app, &g);
        return;
    }

    let client = match build_client(&settings) {
        Ok(c) => c,
        Err(e) => {
            let mut g = handle.inner.lock().await;
            g.error = Some(e);
            g.status = "failed".to_string();
            update_db(&g);
            return;
        }
    };

    // Stream downloads (HLS/DASH) run their own pipeline — fetch the
    // playlist, download segments, merge — instead of the byte-range
    // workers. On success the merged file sits at the .part path and
    // the shared finalise below renames it, verifies and scans it.
    let is_stream = handle.inner.lock().await.stream.is_some();
    if is_stream {
        if let Err(e) = run_stream(&handle, &client, &limiter, &app).await {
            let mut g = handle.inner.lock().await;
            g.error = Some(e);
            g.status = "failed".to_string();
            save_state(&mut g, true);
            update_db(&g);
            emit_progress(&app, &g);
            return;
        }
    } else {
        // Open the .part file for shared offset writes.
        let part_path = handle.inner.lock().await.part_path.clone();
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&part_path)
        {
            Ok(f) => f,
            Err(e) => {
                let mut g = handle.inner.lock().await;
                g.error = Some(format!("cannot open part file: {e}"));
                g.status = "failed".to_string();
                update_db(&g);
                return;
            }
        };
        *handle.file.lock().unwrap() = Some(file);

        let n_workers = {
            let g = handle.inner.lock().await;
            if g.segments.is_empty() {
                1
            } else {
                g.connections as usize
            }
        };

        let mut set = tokio::task::JoinSet::new();
        for _ in 0..n_workers {
            let h = handle.clone();
            let c = client.clone();
            let l = limiter.clone();
            let a = app.clone();
            set.spawn(async move { worker(h, c, l, a).await });
        }
        while set.join_next().await.is_some() {}

        *handle.file.lock().unwrap() = None;
    }

    // Finalise — only on clean success. A failed/paused/cancelled task
    // must never be promoted to "completed".
    let mut g = handle.inner.lock().await;
    if handle.cancel.load(Ordering::SeqCst) {
        g.status = "cancelled".to_string();
        let _ = std::fs::remove_file(&g.part_path);
        let _ = std::fs::remove_file(&g.state_path);
        let _ = std::fs::remove_dir_all(stream::segments_dir(&g.dest_path));
        update_db(&g);
        emit_progress(&app, &g);
        return;
    }
    if handle.stop.load(Ordering::SeqCst) {
        if g.status == "downloading" {
            g.status = "paused".to_string();
        }
        save_state(&mut g, true);
        emit_progress(&app, &g);
        return;
    }
    if let Some(err) = g.error.clone() {
        g.status = "failed".to_string();
        let _ = err;
        save_state(&mut g, true);
        emit_progress(&app, &g);
        return;
    }
    if g.status != "downloading" {
        return;
    }
    if !g.segments.is_empty() && !g.segments.iter().all(|s| s.done) {
        g.status = "failed".to_string();
        g.error = Some("download ended with unfinished segments".to_string());
        save_state(&mut g, true);
        emit_progress(&app, &g);
        return;
    }
    if let Some(total) = g.total {
        if let Ok(meta) = std::fs::metadata(&g.part_path) {
            if meta.len() != total {
                g.status = "failed".to_string();
                g.error = Some("size mismatch after download".to_string());
                save_state(&mut g, true);
                emit_progress(&app, &g);
                return;
            }
        }
    }
    match std::fs::rename(&g.part_path, &g.dest_path) {
        Ok(_) => {
            if g.stream_note.is_some() {
                // The fallback note is this download's durable
                // record: the playlist file is only useful with the
                // explanation that ffmpeg is missing, so keep the
                // sidecar instead of deleting it — a restart restores
                // the note from it (restore_from_db reads it back).
                g.status = "completed".to_string();
                save_state(&mut g, true);
            } else {
                let _ = std::fs::remove_file(&g.state_path);
            }
            // Checksum verification: hash the finished file streaming,
            // on a blocking thread — never on the async runtime, never
            // whole in memory. The task lock is released while hashing
            // so pause/progress keep working.
            let (expected, dest_path) = (g.expected_hash.clone(), g.dest_path.clone());
            drop(g);
            let mut computed: Option<String> = None;
            let mut verified = VerifyState::Unavailable;
            if let Some(exp) = &expected {
                let algo = exp.algo;
                let path = dest_path.clone();
                if let Ok(Ok(hex)) =
                    tokio::task::spawn_blocking(move || checksum::hash_file(&path, algo)).await
                {
                    verified = if hex == exp.hex {
                        VerifyState::Verified
                    } else {
                        VerifyState::Mismatch
                    };
                    computed = Some(hex);
                }
                // A hash read failure leaves the state "unavailable":
                // the download itself succeeded.
            }
            let mut g = handle.inner.lock().await;
            g.computed_hash = computed.clone();
            g.verify_state = verified;
            if verified == VerifyState::Mismatch {
                // The file stays on disk (the user may keep it or
                // retry); only the status says it cannot be trusted.
                g.status = "failed".to_string();
                let exp_short = expected
                    .as_ref()
                    .map(|e| checksum::short_hash(&e.hex))
                    .unwrap_or_default();
                let got_short = computed
                    .as_deref()
                    .map(checksum::short_hash)
                    .unwrap_or_default();
                g.error = Some(format!(
                    "checksum mismatch (expected {exp_short}…, got {got_short}…)"
                ));
                save_state(&mut g, true);
                update_db(&g);
                emit_progress(&app, &g);
            } else {
                g.status = "completed".to_string();
                g.speed_bps = 0.0;
                update_db(&g);
                emit_progress(&app, &g);
                // ClamAV scan: detached, on a blocking thread — a
                // big file takes a while to scan and the queue must
                // never wait for it. The row shows "scanning" until
                // the detached task settles the outcome (clean, or
                // failed with "Virus terdeteksi" — file kept).
                if settings.clamav_scan {
                    g.scan_state = ScanState::Scanning;
                    emit_progress(&app, &g);
                    let handle2 = handle.clone();
                    let app2 = app.clone();
                    tokio::task::spawn(async move {
                        let dest_path =
                            { handle2.inner.lock().await.dest_path.clone() };
                        let result =
                            tokio::task::spawn_blocking(move || scan::scan_file(&dest_path))
                                .await
                                .unwrap_or_else(|_| scan::ScanResult::unavailable());
                        let mut g = handle2.inner.lock().await;
                        apply_scan_result(&mut g, result);
                        update_db(&g);
                        emit_progress(&app2, &g);
                    });
                }
            }
        }
        Err(e) => {
            g.status = "failed".to_string();
            g.error = Some(format!("rename failed: {e}"));
            save_state(&mut g, true);
            emit_progress(&app, &g);
        }
    }
}

/// Probe the server and lay out files/segments; resume from the sidecar
/// when one exists. Idempotent for a fresh task.
async fn prepare(handle: &Arc<TaskHandle>, settings: &Settings) -> Result<(), String> {
    let client = build_client(settings)?;
    let (url, filename_known, dest_path_known) = {
        let g = handle.inner.lock().await;
        (
            g.url.clone(),
            !g.filename.is_empty(),
            !g.dest_path.as_os_str().is_empty(),
        )
    };

    // If we already have paths + segments (restored from DB), only probe
    // for a filename when we do not have one yet.
    let info = probe(&client, &url).await;
    let mut g = handle.inner.lock().await;
    g.total = info.total;
    g.accepts_ranges = info.accepts_ranges;
    g.mime = info.mime.clone();
    // Expected hash, priority: user (already on the task) > response
    // header > sidecar file. The sidecar fetch is best-effort under
    // one shared 5 s timeout, and runs without holding the task
    // lock, so a slow sidecar host never stalls progress updates.
    if g.expected_hash.is_none() {
        g.expected_hash = info.expected_hash.clone();
    }
    let need_sidecar = g.expected_hash.is_none();
    let probe_filename = info.filename.clone();
    drop(g);
    if need_sidecar {
        if let Some(found) = checksum::fetch_sidecar_hash(&client, &url, &probe_filename).await {
            handle.inner.lock().await.expected_hash = Some(found);
        }
    }
    let mut g = handle.inner.lock().await;
    if g.preset {
        // Confirmed from the popup: the user chose filename, category
        // and destination — keep them exactly. Only the probe results
        // above (total / ranges / MIME) are applied.
    } else {
        if !filename_known {
            g.filename = info.filename.clone();
        }
        if g.filename.is_empty() {
            g.filename = "download".to_string();
        }
        g.filename = sanitize_filename(&g.filename);
        g.category = categorize(&g.filename, g.mime.as_deref());
        let folder = category_dir(&g.dest_dir, &g.category);
        if !dest_path_known {
            g.dest_path = folder.join(&g.filename);
            g.part_path = PathBuf::from(format!("{}.part", g.dest_path.display()));
            g.state_path = PathBuf::from(format!("{}.download.json", g.dest_path.display()));
        }
    }

    // Stream resume from sidecar: a stream task may have no .part
    // file yet (merge not done), so the normal sidecar gate below
    // (state + part both exist) would miss it. The plan carries the
    // segment list and the done flags.
    if g.state_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&g.state_path) {
            if let Ok(sc) = serde_json::from_str::<Sidecar>(&text) {
                if sc.url == g.url {
                    if let Some(plan) = sc.stream {
                        // The probe above must not clobber the plan:
                        // restore the stream fields and keep the
                        // playlist total out (it is the playlist
                        // file's size, not the video's).
                        g.stream = Some(plan);
                        g.stream_note = sc.stream_note;
                        g.total = sc.total;
                        g.downloaded_single = sc.downloaded_single;
                        g.verify_state = sc.verify_state;
                        g.computed_hash = sc.computed_hash;
                        if sc.expected_hash.is_some() {
                            g.expected_hash = sc.expected_hash;
                        }
                        return Ok(());
                    }
                }
            }
        }
    }

    // Resume from sidecar.
    if g.state_path.exists() && g.part_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&g.state_path) {
            if let Ok(sc) = serde_json::from_str::<Sidecar>(&text) {
                if sc.url == g.url && sc.total.is_some() {
                    g.total = sc.total;
                    g.segments = sc.segments;
                    g.downloaded_single = sc.downloaded_single;
                    // The sidecar is the freshest verification record
                    // for this download.
                    if sc.expected_hash.is_some() {
                        g.expected_hash = sc.expected_hash;
                    }
                    g.computed_hash = sc.computed_hash;
                    g.verify_state = sc.verify_state;
                    if !g.segments.is_empty() {
                        if let Some(total) = g.total {
                            if let Ok(meta) = std::fs::metadata(&g.part_path) {
                                if meta.len() < total {
                                    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&g.part_path) {
                                        let _ = f.set_len(total);
                                    }
                                }
                            }
                        }
                    } else if g.downloaded_single == 0 {
                        g.downloaded_single = std::fs::metadata(&g.part_path)
                            .map(|m| m.len())
                            .unwrap_or(0);
                    }
                    for s in &mut g.segments {
                        s.assigned = false;
                        s.pos = s.pos.clamp(s.start, s.end + 1);
                        s.done = s.pos > s.end;
                    }
                    return Ok(());
                }
            }
        }
    }

    // Stream manifests (.m3u8/.mpd): download every segment and
    // merge them into one video file instead of saving the playlist.
    // Detection is by URL extension first, then by Content-Type.
    let manifest_kind = stream::kind_for_url(&g.url)
        .or_else(|| g.mime.as_deref().and_then(stream::kind_for_mime));
    if let Some(kind) = manifest_kind {
        let ffmpeg = stream::ffmpeg_available();
        if kind == stream::StreamKind::Dash && !ffmpeg {
            // DASH cannot be merged without ffmpeg: fall back to
            // the old behaviour (the playlist file itself), with a
            // note that says so honestly.
            g.stream_note = Some(
                "ffmpeg tidak ditemukan — hanya playlist yang diunduh; instal ffmpeg untuk menggabungkan segmen menjadi video".to_string(),
            );
            // Fall through to the normal layout below: the playlist
            // downloads as an ordinary file.
        } else {
            // Fetch + parse the playlist now: the output extension
            // depends on its content (fMP4 is always .mp4; TS is
            // .mp4 with ffmpeg, .ts without), and a bad or live
            // playlist fails fast here with a clear message instead
            // of after a useless download.
            let (url, auth) = (g.url.clone(), g.auth.clone());
            drop(g);
            let resolved = build_stream_plan(&client, &url, &auth, kind).await?;
            if !resolved.keys.is_empty() && !ffmpeg {
                return Err(
                    "stream terenkripsi AES-128 butuh ffmpeg — instal ffmpeg untuk mengunduh stream ini"
                        .to_string(),
                );
            }
            let ext = if kind == stream::StreamKind::Dash || resolved.is_fmp4 || ffmpeg {
                "mp4"
            } else {
                "ts"
            };
            let mut g = handle.inner.lock().await;
            // The merged file takes the playlist's name with the
            // output extension. A preset name chosen in the popup
            // keeps its folder; otherwise the category is re-derived
            // from the new name (a .mp4 lands in Video).
            let out_name = stream::output_filename(&g.filename, ext);
            if out_name != g.filename {
                g.filename = out_name;
                let new_dest = if g.preset {
                    g.dest_path.parent().map(|p| p.join(&g.filename))
                } else {
                    g.category = categorize(&g.filename, g.mime.as_deref());
                    Some(category_dir(&g.dest_dir, &g.category).join(&g.filename))
                };
                if let Some(d) = new_dest {
                    g.dest_path = d;
                    g.part_path =
                        PathBuf::from(format!("{}.part", g.dest_path.display()));
                    g.state_path = PathBuf::from(format!(
                        "{}.download.json",
                        g.dest_path.display()
                    ));
                }
            }
            g.stream = Some(stream::StreamPlan::from_resolved(resolved, ext));
            g.stream_note = None;
            g.segments = Vec::new();
            g.total = None;
            g.downloaded_single = 0;
            // Register in the DB (insert once, update afterwards).
            if g.db_id.is_none() {
                let db_path = db::db_path_for(&g.dest_dir);
                match db::insert_download(
                    &db_path,
                    &g.url,
                    &g.filename,
                    &g.dest_path.to_string_lossy(),
                    None,
                    &g.category,
                ) {
                    Ok(id) => g.db_id = Some(id),
                    Err(_) => {}
                }
            } else {
                update_db(&g);
            }
            save_state(&mut g, true);
            return Ok(());
        }
    }

    // Fresh layout.
    if let Some(total) = g.total {
        if g.accepts_ranges && g.connections > 1 {
            g.segments = init_segments(total, g.connections);
            if let Ok(f) = std::fs::File::create(&g.part_path) {
                let _ = f.set_len(total);
            }
        } else {
            g.segments = Vec::new();
            g.downloaded_single = 0;
            if g.part_path.exists() {
                if g.accepts_ranges {
                    g.downloaded_single = std::fs::metadata(&g.part_path)
                        .map(|m| m.len())
                        .unwrap_or(0);
                } else {
                    let _ = std::fs::remove_file(&g.part_path);
                }
            }
        }
    } else {
        g.segments = Vec::new();
        g.downloaded_single = 0;
    }

    // Register in the DB (insert once, update afterwards).
    if g.db_id.is_none() {
        let db_path = db::db_path_for(&g.dest_dir);
        match db::insert_download(
            &db_path,
            &g.url,
            &g.filename,
            &g.dest_path.to_string_lossy(),
            g.total.map(|t| t as i64),
            &g.category,
        ) {
            Ok(id) => g.db_id = Some(id),
            Err(_) => {}
        }
    } else {
        update_db(&g);
    }
    save_state(&mut g, true);
    Ok(())
}

async fn worker(
    handle: Arc<TaskHandle>,
    client: reqwest::Client,
    limiter: Arc<AsyncMutex<RateLimiter>>,
    app: Option<AppHandle>,
) {
    loop {
        if handle.stop.load(Ordering::SeqCst) || handle.cancel.load(Ordering::SeqCst) {
            return;
        }
        {
            let g = handle.inner.lock().await;
            if g.error.is_some() {
                return;
            }
        }
        let segmented = {
            let g = handle.inner.lock().await;
            !g.segments.is_empty()
        };
        if segmented {
            let claim = {
                let mut g = handle.inner.lock().await;
                claim_or_split(&mut g.segments)
            };
            let Some(idx) = claim else {
                let done = {
                    let g = handle.inner.lock().await;
                    g.segments.iter().all(|s| s.done)
                };
                if done {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            if let Err(e) = download_segment(&handle, &client, &limiter, &app, idx).await {
                let mut g = handle.inner.lock().await;
                if g.error.is_none() && !handle.stop.load(Ordering::SeqCst) {
                    g.error = Some(e);
                }
                return;
            }
            let mut g = handle.inner.lock().await;
            if let Some(seg) = g.segments.get_mut(idx) {
                seg.assigned = false;
                if seg.pos > seg.end {
                    seg.done = true;
                }
            }
            save_state(&mut g, false);
        } else {
            match download_single(&handle, &client, &limiter, &app).await {
                Ok(_) => return,
                Err(e) => {
                    let mut g = handle.inner.lock().await;
                    if g.error.is_none() && !handle.stop.load(Ordering::SeqCst) {
                        g.error = Some(e);
                    }
                    return;
                }
            }
        }
    }
}

async fn download_segment(
    handle: &Arc<TaskHandle>,
    client: &reqwest::Client,
    limiter: &Arc<AsyncMutex<RateLimiter>>,
    app: &Option<AppHandle>,
    idx: usize,
) -> Result<(), String> {
    let mut retries = 0u32;
    loop {
        let (pos, end, clean_url, auth) = {
            let g = handle.inner.lock().await;
            let Some(seg) = g.segments.get(idx) else {
                return Ok(());
            };
            (seg.pos, seg.end, g.clean_url.clone(), g.auth.clone())
        };
        if pos > end || handle.stop.load(Ordering::SeqCst) || handle.cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        let mut req = client
            .get(&clean_url)
            .header("Range", format!("bytes={pos}-{end}"));
        if let Some(a) = &auth {
            req = req.header("Authorization", a.clone());
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                retries += 1;
                if retries > MAX_RETRIES || handle.stop.load(Ordering::SeqCst) {
                    return Err(format!("segment {pos}-{end} failed after {retries} retries: {e}"));
                }
                tokio::time::sleep(Duration::from_secs(1 << retries.min(4))).await;
                continue;
            }
        };
        if !resp.status().is_success() {
            return Err(format!(
                "segment {pos}-{end} got HTTP {}",
                resp.status()
            ));
        }
        // A server that ignores Range answers 200 with the body starting
        // at byte 0. Discard the prefix so the bytes we write still land
        // at `pos` instead of corrupting the file.
        let mut skip: u64 = if resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
            0
        } else {
            pos
        };
        let mut stream = resp.bytes_stream();
        loop {
            if handle.stop.load(Ordering::SeqCst) || handle.cancel.load(Ordering::SeqCst) {
                return Ok(());
            }
            let (cur_pos, cur_end) = {
                let g = handle.inner.lock().await;
                let Some(seg) = g.segments.get(idx) else {
                    return Ok(());
                };
                (seg.pos, seg.end)
            };
            if cur_pos > cur_end {
                return Ok(());
            }
            let mut chunk = match stream.next().await {
                Some(Ok(c)) => c,
                Some(Err(e)) => {
                    retries += 1;
                    if retries > MAX_RETRIES || handle.stop.load(Ordering::SeqCst) {
                        return Err(format!("segment {cur_pos}-{cur_end} failed after {retries} retries: {e}"));
                    }
                    tokio::time::sleep(Duration::from_secs(1 << retries.min(4))).await;
                    break; // re-request from current pos
                }
                None => return Ok(()),
            };
            if chunk.is_empty() {
                continue;
            }
            if skip > 0 {
                let drop_n = (skip as usize).min(chunk.len());
                chunk = chunk.slice(drop_n..);
                skip -= drop_n as u64;
                if chunk.is_empty() {
                    continue;
                }
            }
            let want = (cur_end - cur_pos + 1) as usize;
            let data = if chunk.len() > want { chunk.slice(..want) } else { chunk };
            let n = data.len();
            RateLimiter::acquire(limiter, n).await;
            {
                let mut file_guard = handle.file.lock().unwrap();
                if let Some(f) = file_guard.as_mut() {
                    if f.seek(SeekFrom::Start(cur_pos)).is_err() {
                        return Err("seek failed on part file".to_string());
                    }
                    if f.write_all(&data).is_err() {
                        return Err("write failed on part file".to_string());
                    }
                }
            }
            {
                let mut g = handle.inner.lock().await;
                if let Some(seg) = g.segments.get_mut(idx) {
                    seg.pos += n as u64;
                }
                note_bytes(&mut g, app, n);
                save_state(&mut g, false);
            }
            retries = 0;
        }
    }
}

async fn download_single(
    handle: &Arc<TaskHandle>,
    client: &reqwest::Client,
    limiter: &Arc<AsyncMutex<RateLimiter>>,
    app: &Option<AppHandle>,
) -> Result<(), String> {
    let mut retries = 0u32;
    loop {
        let (start, accepts_ranges, clean_url, auth) = {
            let g = handle.inner.lock().await;
            (
                g.downloaded_single,
                g.accepts_ranges,
                g.clean_url.clone(),
                g.auth.clone(),
            )
        };
        let offset = if accepts_ranges { start } else { 0 };
        let mut req = client.get(&clean_url);
        if accepts_ranges && offset > 0 {
            req = req.header("Range", format!("bytes={offset}-"));
        }
        if let Some(a) = &auth {
            req = req.header("Authorization", a.clone());
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                retries += 1;
                if retries > MAX_RETRIES || handle.stop.load(Ordering::SeqCst) {
                    return Err(format!("single-connection download failed after {retries} retries: {e}"));
                }
                tokio::time::sleep(Duration::from_secs(1 << retries.min(4))).await;
                continue;
            }
        };
        if !resp.status().is_success() {
            return Err(format!("single-connection download got HTTP {}", resp.status()));
        }
        // As in download_segment: a 200 to a Range request means the
        // body starts at byte 0, so discard the already-written prefix.
        let mut skip: u64 = if resp.status() == reqwest::StatusCode::PARTIAL_CONTENT || offset == 0 {
            0
        } else {
            offset
        };
        {
            let mut g = handle.inner.lock().await;
            if g.total.is_none() {
                if let Some(len) = resp
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                {
                    g.total = Some(len);
                }
            }
        }
        let mut stream = resp.bytes_stream();
        loop {
            if handle.stop.load(Ordering::SeqCst) || handle.cancel.load(Ordering::SeqCst) {
                return Ok(());
            }
            let mut chunk = match stream.next().await {
                Some(Ok(c)) => c,
                Some(Err(e)) => {
                    retries += 1;
                    if retries > MAX_RETRIES || handle.stop.load(Ordering::SeqCst) {
                        return Err(format!("single-connection download failed after {retries} retries: {e}"));
                    }
                    tokio::time::sleep(Duration::from_secs(1 << retries.min(4))).await;
                    break;
                }
                None => return Ok(()),
            };
            if chunk.is_empty() {
                continue;
            }
            if skip > 0 {
                let drop_n = (skip as usize).min(chunk.len());
                chunk = chunk.slice(drop_n..);
                skip -= drop_n as u64;
                if chunk.is_empty() {
                    continue;
                }
            }
            let n = chunk.len();
            RateLimiter::acquire(limiter, n).await;
            let pos = {
                let g = handle.inner.lock().await;
                g.downloaded_single
            };
            {
                let mut file_guard = handle.file.lock().unwrap();
                if let Some(f) = file_guard.as_mut() {
                    let _ = f.seek(SeekFrom::Start(pos));
                    if f.write_all(&chunk).is_err() {
                        return Err("write failed on part file".to_string());
                    }
                }
            }
            {
                let mut g = handle.inner.lock().await;
                g.downloaded_single += n as u64;
                note_bytes(&mut g, app, n);
                save_state(&mut g, false);
            }
            retries = 0;
        }
    }
}

// ---------------------------------------------------------------- unit tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_largest_segment() {
        // 4 segments of a 1 MiB file, all assigned (busy).
        let mut segs = init_segments(1_048_576, 4);
        for s in &mut segs {
            s.assigned = true;
        }
        let idx = claim_or_split(&mut segs).expect("should split");
        assert_eq!(segs.len(), 5);
        let new = &segs[idx];
        // Segment 0 is 0..262143; split at mid = 131072.
        assert_eq!(new.start, 131_073);
        assert_eq!(new.end, 262_143);
        assert!(new.assigned);
        assert_eq!(segs[0].end, 131_072);
    }

    #[test]
    fn claim_prefers_free_segment() {
        let mut segs = init_segments(1_048_576, 4);
        // Segment 0 is partly done; segments 1-3 are full-size and free.
        // The first largest free segment (segment 1) is claimed.
        segs[0].pos = 200_000;
        let idx = claim_or_split(&mut segs).expect("should claim");
        assert_eq!(segs[idx].start, 262_144);
        assert!(segs[idx].assigned);
        assert_eq!(segs.len(), 4, "no split when a free segment exists");
    }

    #[test]
    fn no_split_when_remainder_too_small() {
        let mut segs = vec![Segment {
            start: 0,
            end: 100_000,
            pos: 0,
            done: false,
            assigned: true,
        }];
        assert!(claim_or_split(&mut segs).is_none());
    }

    #[test]
    fn categorise_by_extension_and_mime() {
        assert_eq!(categorize("report.pdf", None), "Documents");
        assert_eq!(categorize("song.FLAC", None), "Music");
        assert_eq!(categorize("movie.mkv", None), "Video");
        assert_eq!(categorize("photo.webp", None), "Pictures");
        assert_eq!(categorize("setup.deb", None), "Programs");
        assert_eq!(categorize("archive.tar.gz", None), "Compressed");
        assert_eq!(categorize("unknown.xyz", None), "General");
        assert_eq!(categorize("noext", Some("video/mp4")), "Video");
        assert_eq!(categorize("noext", Some("audio/mpeg")), "Music");
    }

    #[test]
    fn resume_state_round_trip() {
        let segs = init_segments(1_000_000, 8);
        let sc = Sidecar {
            url: "https://example.com/f.bin".to_string(),
            filename: "f.bin".to_string(),
            total: Some(1_000_000),
            status: "paused".to_string(),
            segments: segs,
            downloaded_single: 0,
            expected_hash: None,
            computed_hash: None,
            verify_state: VerifyState::Unavailable,
            scan_state: ScanState::Unavailable,
            threat_name: None,
            stream: None,
            stream_note: None,
        };
        let text = serde_json::to_string(&sc).unwrap();
        let back: Sidecar = serde_json::from_str(&text).unwrap();
        assert_eq!(back.total, Some(1_000_000));
        assert_eq!(back.segments.len(), 8);
        assert_eq!(back.segments[7].end, 999_999);
        assert!(back.stream.is_none());
    }

    #[test]
    fn speed_limiter_math() {
        // 1 KiB/s bucket, empty: 2048 bytes needs ~2s of wait.
        let mut lim = RateLimiter::new(1.0);
        let wait = lim.try_take(2048);
        assert!((1.9..=2.1).contains(&wait), "wait was {wait}");
        // Unlimited never waits.
        let mut free = RateLimiter::new(0.0);
        assert_eq!(free.try_take(1_000_000), 0.0);
        // After a long idle the bucket refills to at most 1s of tokens.
        let mut lim2 = RateLimiter::new(1.0);
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(lim2.try_take(1024), 0.0);
        assert!(lim2.try_take(1024) > 0.0);
    }

    #[test]
    fn speed_limiter_wait_does_not_consume_tokens() {
        // Regression: the old try_take deducted a partial take on
        // every call, so a wait followed by a retry never converged
        // when another call had taken tokens in between.
        let mut lim = RateLimiter::new(1.0); // 1 KiB/s
        std::thread::sleep(Duration::from_millis(600));
        let wait = lim.try_take(1024);
        assert!(wait > 0.0 && wait < 1.0, "wait was {wait}");
        // The partial refill must still be there after the wait.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(lim.try_take(1024), 0.0);
    }

    #[tokio::test]
    async fn speed_limiter_acquire_large_chunk_completes() {
        // Regression: a chunk bigger than the one-second token cap
        // (here 2 KiB at 1 KiB/s) wedged acquire forever — the same
        // wait was recomputed on every retry while the tokens it
        // had already taken were gone.
        let limiter = Arc::new(AsyncMutex::new(RateLimiter::new(1.0)));
        tokio::time::timeout(Duration::from_secs(5), RateLimiter::acquire(&limiter, 2048))
            .await
            .expect("acquire(2048) at 1 KiB/s must finish in ~2 s");
    }

    #[tokio::test]
    async fn speed_limiter_concurrent_acquires_complete() {
        // Eight workers sharing one limiter (the multi-segment case):
        // interleaved takes must not wedge any of them.
        let limiter = Arc::new(AsyncMutex::new(RateLimiter::new(1.0)));
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let l = limiter.clone();
            set.spawn(async move { RateLimiter::acquire(&l, 1024).await });
        }
        tokio::time::timeout(Duration::from_secs(12), async {
            while set.join_next().await.is_some() {}
        })
        .await
        .expect("8 concurrent acquires of 1 KiB at 1 KiB/s must finish in ~8 s");
    }

    #[test]
    fn pending_add_dedupes() {
        let mut m = Manager::new(Settings::default());
        let first = m
            .add_pending("https://example.com/setup.exe")
            .unwrap()
            .expect("first add creates a pending item");
        assert_eq!(first.filename, "setup.exe");
        assert_eq!(first.category, "Programs");
        assert!(first.auto_filename);
        assert!(first.total.is_none());
        // Same URL again: deduped.
        assert!(m.add_pending("https://example.com/setup.exe").unwrap().is_none());
        assert_eq!(m.list_pending().len(), 1);
        // Invalid URL: error, nothing added.
        assert!(m.add_pending("not-a-url").is_err());
        assert!(m.add_pending("ftp://example.com/x.bin").is_err());
        assert_eq!(m.list_pending().len(), 1);
    }

    #[tokio::test]
    async fn pending_confirm_applies_user_choices_as_preset() {
        let mut m = Manager::new(Settings::default());
        let p = m
            .add_pending("https://example.com/orig.bin")
            .unwrap()
            .expect("pending created");
        let opts = ConfirmOptions {
            filename: "renamed.iso".to_string(),
            dest_dir: "/tmp/zdm-test-dest".to_string(),
            category: "Programs".to_string(),
            connections: 4,
            start_now: false,
            expected_sha256: None,
        };
        let handle = m.confirm_pending(&p.id, opts).await.unwrap();
        assert!(m.list_pending().is_empty());
        let g = handle.inner.lock().await;
        assert_eq!(g.filename, "renamed.iso");
        assert_eq!(g.category, "Programs");
        assert_eq!(g.dest_path.to_string_lossy(), "/tmp/zdm-test-dest/renamed.iso");
        assert_eq!(g.connections, 4);
        assert!(g.preset);
        assert_eq!(g.status, "queued");
        // Confirming the same id again fails — it is gone.
        let again = m
            .confirm_pending(
                &p.id,
                ConfirmOptions {
                    filename: "x.bin".to_string(),
                    dest_dir: String::new(),
                    category: "General".to_string(),
                    connections: 8,
                    start_now: false,
                    expected_sha256: None,
                },
            )
            .await;
        assert!(again.is_err());
    }

    #[test]
    fn pending_dismiss_removes_only_that_id() {
        let mut m = Manager::new(Settings::default());
        let a = m.add_pending("https://example.com/a.bin").unwrap().unwrap();
        let b = m.add_pending("https://example.com/b.bin").unwrap().unwrap();
        assert!(m.dismiss_pending(&a.id));
        assert!(!m.dismiss_pending(&a.id));
        assert!(!m.dismiss_pending("no-such-id"));
        let left = m.list_pending();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, b.id);
    }

    // Daily schedule ticks, with the clock injected (2026-11-09 is a
    // Monday). The window is 23:00–06:00, every day.
    fn schedule_manager() -> Manager {
        let mut m = Manager::new(Settings::default());
        m.settings.schedule = crate::schedule::DailySchedule {
            enabled: true,
            start: "23:00".to_string(),
            stop: "06:00".to_string(),
            days: [true; 7],
        };
        m
    }

    fn at(day: u32, hour: u32, min: u32) -> chrono::DateTime<chrono::Local> {
        use chrono::TimeZone;
        chrono::Local
            .with_ymd_and_hms(2026, 11, day, hour, min, 0)
            .unwrap()
    }

    #[test]
    fn schedule_tick_starts_inside_window_and_stops_outside() {
        let mut m = schedule_manager();
        assert_eq!(m.schedule_tick(at(9, 10, 0)), ScheduleAction::None);
        assert!(!m.queue_running);
        assert_eq!(m.schedule_tick(at(9, 23, 30)), ScheduleAction::Start);
        assert!(m.queue_running);
        // No change on the next tick inside the window.
        assert_eq!(m.schedule_tick(at(9, 23, 45)), ScheduleAction::None);
        assert_eq!(m.schedule_tick(at(10, 6, 30)), ScheduleAction::Stop);
        assert!(!m.queue_running);
    }

    #[test]
    fn schedule_tick_disabled_is_noop() {
        let mut m = Manager::new(Settings::default());
        assert!(!m.settings.schedule.enabled);
        m.queue_running = true;
        assert_eq!(m.schedule_tick(at(9, 23, 30)), ScheduleAction::None);
        assert!(m.queue_running);
    }

    #[test]
    fn schedule_tick_manual_hold_blocks_until_boundary() {
        let mut m = schedule_manager();
        // Manual stop inside the window: the hold keeps the queue
        // stopped until the window boundary.
        m.queue_running = false;
        m.schedule_hold = true;
        assert_eq!(m.schedule_tick(at(9, 23, 30)), ScheduleAction::None);
        assert!(!m.queue_running);
        assert_eq!(m.schedule_tick(at(9, 23, 45)), ScheduleAction::None);
        assert!(!m.queue_running);
        // 06:00 boundary: the hold expires, but outside the window
        // there is nothing to start.
        assert_eq!(m.schedule_tick(at(10, 6, 0)), ScheduleAction::None);
        // Next window start fires normally again.
        assert_eq!(m.schedule_tick(at(10, 23, 0)), ScheduleAction::Start);
        assert!(m.queue_running);
    }

    #[test]
    fn pending_probe_fills_size_but_not_edited_name() {
        let mut m = Manager::new(Settings::default());
        let p = m.add_pending("https://example.com/a.bin").unwrap().unwrap();
        let info = FileInfo {
            total: Some(12_345),
            accepts_ranges: true,
            filename: "server-name.bin".to_string(),
            mime: Some("application/octet-stream".to_string()),
            expected_hash: None,
        };
        m.update_pending_probe(&p.id, &info);
        let got = &m.list_pending()[0];
        assert_eq!(got.total, Some(12_345));
        // Still URL-derived, so the server filename wins.
        assert_eq!(got.filename, "server-name.bin");
        // A second probe with a different name no longer renames:
        // auto_filename is cleared by the first probe.
        let info2 = FileInfo {
            total: Some(12_345),
            accepts_ranges: true,
            filename: "other.bin".to_string(),
            mime: None,
            expected_hash: None,
        };
        m.update_pending_probe(&p.id, &info2);
        assert_eq!(m.list_pending()[0].filename, "server-name.bin");
    }

    #[test]
    fn strip_auth_and_filename() {
        let (clean, auth) = strip_auth("https://user:p%40ss@example.com/f.bin");
        assert_eq!(clean, "https://example.com/f.bin");
        assert!(auth.unwrap().starts_with("Basic "));
        assert_eq!(filename_from_url("https://example.com/a%20b.zip", None), "a b.zip");
        assert_eq!(
            filename_from_url("https://example.com/x", Some("attachment; filename=\"report.pdf\"")),
            "report.pdf"
        );
        assert_eq!(filename_from_url("https://example.com/", None), "download");
    }

    static TEST_CTR: AtomicU64 = AtomicU64::new(0);

    fn unique_dir() -> std::path::PathBuf {
        let n = TEST_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("zdm-engine-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn settings_in(dir: &std::path::Path) -> Settings {
        Settings {
            download_dir: dir.to_string_lossy().to_string(),
            ..Settings::default()
        }
    }

    /// Helper: a task as restored from the DB, in a given state.
    /// `interrupted` marks a row left as "downloading" when the app
    /// stopped; `by_schedule` marks a row the scheduler paused.
    fn restored_task(
        dir: &std::path::Path,
        status: &str,
        interrupted: bool,
        by_schedule: bool,
    ) -> Manager {
        let mut m = Manager::new(settings_in(dir));
        let id = m.add("http://127.0.0.1:9/f.bin", Some(1)).unwrap().id.clone();
        let h = m.tasks.get(&id).unwrap().clone();
        {
            let mut g = h.inner.blocking_lock();
            g.status = status.to_string();
            g.interrupted = interrupted;
            g.paused_by_schedule = by_schedule;
        }
        m
    }

    /// Set a task's in-memory state on an existing manager.
    fn set_state(m: &Manager, id: &str, status: &str, interrupted: bool, by_schedule: bool) {
        let h = m.tasks.get(id).unwrap().clone();
        let mut g = h.inner.blocking_lock();
        g.status = status.to_string();
        g.interrupted = interrupted;
        g.paused_by_schedule = by_schedule;
    }

    #[test]
    fn startup_resume_matrix() {
        let dir = unique_dir();
        let now = chrono::Local::now();
        // 1. An interrupted download (stale "downloading" in the DB)
        //    resumes; the candidate list holds exactly that one.
        let mut m = restored_task(&dir, "paused", true, false);
        assert_eq!(m.resume_candidate_ids().len(), 1);
        assert!(m.startup_resume(now));
        assert!(m.queue_running);
        // 2. A manually paused download stays paused: no candidates,
        //    the queue does not start.
        let mut m = restored_task(&dir, "paused", false, false);
        assert!(m.resume_candidate_ids().is_empty());
        assert!(!m.startup_resume(now));
        assert!(!m.queue_running);
        // 3. A scheduler-paused download resumes inside its window
        //    (00:00–23:59 covers any clock time).
        let mut settings = settings_in(&dir);
        settings.schedule.enabled = true;
        settings.schedule.start = "00:00".to_string();
        settings.schedule.stop = "23:59".to_string();
        let mut m = Manager::new(settings);
        let id = m.add("http://127.0.0.1:9/g.bin", Some(1)).unwrap().id.clone();
        set_state(&m, &id, "paused", false, true);
        assert_eq!(m.resume_candidate_ids().len(), 1);
        assert!(m.startup_resume(now));
        // 4. …and stays paused outside its window (23:00–06:00,
        //    checked at noon local time).
        let mut settings = settings_in(&dir);
        settings.schedule.enabled = true;
        settings.schedule.start = "23:00".to_string();
        settings.schedule.stop = "06:00".to_string();
        let mut m = Manager::new(settings);
        let id = m.add("http://127.0.0.1:9/h.bin", Some(1)).unwrap().id.clone();
        set_state(&m, &id, "paused", false, true);
        let outside = now
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .unwrap();
        assert!(!m.startup_resume(outside));
        assert!(!m.queue_running);
        // 5. Toggle OFF: nothing resumes, not even an interrupted
        //    download. (Manual resume still works — that path does
        //    not consult the setting.)
        let mut settings = settings_in(&dir);
        settings.resume_on_startup = false;
        let mut m = Manager::new(settings);
        let id = m.add("http://127.0.0.1:9/i.bin", Some(1)).unwrap().id.clone();
        set_state(&m, &id, "paused", true, false);
        assert_eq!(m.resume_candidate_ids().len(), 1);
        assert!(!m.startup_resume(now));
        assert!(!m.queue_running);
        // 6. A completed download is never a candidate.
        let m = restored_task(&dir, "completed", false, false);
        assert!(m.resume_candidate_ids().is_empty());
        // 7. A queued download is a candidate.
        let m = restored_task(&dir, "queued", false, false);
        assert_eq!(m.resume_candidate_ids().len(), 1);
    }

    #[test]
    fn restore_marks_stale_downloading_as_interrupted() {
        let dir = unique_dir();
        // Seed the DB directly: a row left as "downloading" (app
        // killed mid-download) plus a scheduler-paused row, then
        // restore into a fresh manager and inspect the flags.
        let db_path = db::db_path_for(&dir.to_string_lossy());
        let id = db::insert_download(
            &db_path,
            "http://127.0.0.1:9/j.bin",
            "j.bin",
            &dir.join("j.bin").to_string_lossy(),
            Some(1000),
            "General",
        )
        .unwrap();
        db::update_download(&db_path, id, 400, Some(1000), "downloading", false).unwrap();
        let id2 = db::insert_download(
            &db_path,
            "http://127.0.0.1:9/k.bin",
            "k.bin",
            &dir.join("k.bin").to_string_lossy(),
            Some(1000),
            "General",
        )
        .unwrap();
        db::update_download(&db_path, id2, 400, Some(1000), "paused", true).unwrap();
        let m = Manager::new(settings_in(&dir));
        assert_eq!(m.order.len(), 2);
        // `list_downloads` orders by created DESC and both rows share
        // a second, so look the tasks up by filename, not position.
        let by_name = |name: &str| {
            let id = m
                .order
                .iter()
                .find(|id| {
                    let h = m.tasks.get(*id).unwrap();
                    h.inner.blocking_lock().filename == name
                })
                .unwrap()
                .clone();
            m.tasks.get(&id).unwrap().clone()
        };
        let h = by_name("j.bin");
        let g = h.inner.blocking_lock();
        assert_eq!(g.status, "paused");
        assert!(g.interrupted);
        assert!(!g.paused_by_schedule);
        drop(g);
        let h = by_name("k.bin");
        let g = h.inner.blocking_lock();
        assert_eq!(g.status, "paused");
        assert!(!g.interrupted);
        assert!(g.paused_by_schedule);
        drop(g);
        // Both are resumable at startup (no schedule enabled).
        assert_eq!(m.resume_candidate_ids().len(), 2);
    }
}
