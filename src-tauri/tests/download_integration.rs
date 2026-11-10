//! Integration tests: a local Range-capable HTTP server (hand-rolled on
//! std::net, like the Python tests — hyper is not a dependency) serves a
//! 5 MiB file; the real engine downloads it with 8 connections and the
//! SHA-256 must match, pause/resume must work, and a server without
//! Range support must fall back to a single connection.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zdm_lib::engine::{ConfirmOptions, DownloadView, Manager};
use zdm_lib::settings::Settings;

static CTR: AtomicU64 = AtomicU64::new(0);

// Serialize the stream tests: ZDM_FFMPEG is process-global and the
// no-ffmpeg test hides it, so two stream tests running in parallel
// would poison each other. (tokio Mutex: its guard is Send, unlike
// std's, so it can be held across awaits.)
static STREAM_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn lock_stream_tests() -> tokio::sync::MutexGuard<'static, ()> {
    STREAM_LOCK.lock().await
}

/// Restores ZDM_FFMPEG on drop, even if the test panics mid-way —
/// otherwise a leftover "" would poison every later stream test.
struct HideFfmpeg;
impl HideFfmpeg {
    fn hide() -> Self {
        std::env::set_var("ZDM_FFMPEG", "");
        HideFfmpeg
    }
}
impl Drop for HideFfmpeg {
    fn drop(&mut self) {
        std::env::remove_var("ZDM_FFMPEG");
    }
}

fn test_data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// Tiny HTTP/1.1 server. Endpoints:
///   /file.bin       — Range-capable (Accept-Ranges, 206 + Content-Range)
///   /plain.bin      — no Range support at all (always 200, full body)
///   /chunked.bin    — no Content-Length (chunked transfer encoding)
///   /digest.bin     — Range-capable + `Digest: SHA-256=<correct>` header
///   /digest-bad.bin — Range-capable + `Digest` with a wrong hash
///   /side.bin       — Range-capable, no Digest header
///   /side.bin.sha256 — the sidecar for /side.bin (`<hex>  side.bin`)
///   /eicar.com      — the 68-byte EICAR test file (ClamAV flags it)
fn serve(listener: TcpListener, data: Arc<Vec<u8>>, digest_b64: Arc<String>, wrong_b64: Arc<String>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let data = data.clone();
        let digest_b64 = digest_b64.clone();
        let wrong_b64 = wrong_b64.clone();
        std::thread::spawn(move || {
            // Read until the header block is complete (\r\n\r\n); a
            // single read() can return a partial request.
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        raw.extend_from_slice(&buf[..n]);
                        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => return,
                }
            }
            let req = String::from_utf8_lossy(&raw).into_owned();
            let mut lines = req.lines();
            let first = lines.next().unwrap_or("");
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or("GET");
            let path = parts.next().unwrap_or("/");
            let mut range: Option<(u64, u64)> = None;
            for line in lines {
                if line.is_empty() {
                    break;
                }
                // Header names are case-insensitive; reqwest sends
                // them lowercase ("range: bytes=...").
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("range:") {
                    let v = v.trim().trim_start_matches("bytes=");
                    if let Some((a, b)) = v.split_once('-') {
                        if let (Ok(a), Ok(b)) = (a.parse::<u64>(), b.parse::<u64>()) {
                            range = Some((a, b));
                        }
                    }
                }
            }
            let total = data.len() as u64;
            let is_head = method == "HEAD";
            let mut headers = String::new();
            let mut body: Vec<u8> = Vec::new();
            match path {
                "/page.html" => {
                    // Grabber test page: video + audio tags, a <source>
                    // child, a plain file link, a manifest link, and a
                    // blob: video that must be skipped.
                    let page = format!(
                        "<!doctype html><html><body>\
                        <video src=\"/clip.mp4\" controls></video>\
                        <video controls><source src=\"/alt.webm\"></video>\
                        <audio src=\"/song.mp3\"></audio>\
                        <video src=\"blob:http://example.com/x\"></video>\
                        <a href=\"/file.bin\">file</a>\
                        <a href=\"/stream.m3u8\">stream</a>\
                        <a href=\"/clip.mp4\">dup</a>\
                        </body></html>"
                    );
                    headers.push_str(&format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        page.len()
                    ));
                    if !is_head {
                        body.extend_from_slice(page.as_bytes());
                    }
                }
                "/chunked.bin" => {
                    headers.push_str("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
                    if !is_head {
                        body.extend_from_slice(format!("{:x}\r\n", data.len()).as_bytes());
                        body.extend_from_slice(&data);
                        body.extend_from_slice(b"\r\n0\r\n\r\n");
                    }
                }
                "/plain.bin" => {
                    headers.push_str(&format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                    ));
                    if !is_head {
                        body.extend_from_slice(&data);
                    }
                }
                "/side.bin.sha256" => {
                    // Checksum-file format: "<hex>  <filename>".
                    let text = format!("{}  side.bin\n", sha256_hex(&data));
                    headers.push_str(&format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        text.len()
                    ));
                    if !is_head {
                        body.extend_from_slice(text.as_bytes());
                    }
                }
                "/eicar.com" => {
                    // The standard EICAR test string: harmless, but
                    // every antivirus flags it.
                    let eicar = zdm_lib::scan::EICAR.as_bytes();
                    headers.push_str(&format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        eicar.len()
                    ));
                    if !is_head {
                        body.extend_from_slice(eicar);
                    }
                }
                _ => {
                    // Range-capable: /file.bin, /digest.bin,
                    // /digest-bad.bin, /side.bin.
                    let digest = match path {
                        "/digest.bin" => format!("Digest: SHA-256={digest_b64}\r\n"),
                        "/digest-bad.bin" => format!("Digest: SHA-256={wrong_b64}\r\n"),
                        _ => String::new(),
                    };
                    if let Some((a, b)) = range {
                        let end = b.min(total - 1);
                        let slice = &data[a as usize..=end as usize];
                        headers.push_str(&format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nAccept-Ranges: bytes\r\n{digest}Content-Range: bytes {a}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            slice.len()
                        ));
                        if !is_head {
                            body.extend_from_slice(slice);
                        }
                    } else {
                        headers.push_str(&format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nAccept-Ranges: bytes\r\n{digest}Content-Length: {total}\r\nConnection: close\r\n\r\n"
                        ));
                        if !is_head {
                            body.extend_from_slice(&data);
                        }
                    }
                }
            }
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        });
    }
}

struct Server {
    base: String,
    _data: Arc<Vec<u8>>,
}

fn start_server() -> Server {
    start_server_sized(5 * 1024 * 1024)
}

fn start_server_sized(size: usize) -> Server {
    use base64::Engine as _;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let data = Arc::new(test_data(size));
    let mut h = Sha256::new();
    h.update(data.as_slice());
    let digest_b64 = Arc::new(base64::engine::general_purpose::STANDARD.encode(h.finalize()));
    let wrong_b64 = Arc::new(base64::engine::general_purpose::STANDARD.encode([0u8; 32]));
    let d2 = data.clone();
    let b2 = digest_b64.clone();
    let w2 = wrong_b64.clone();
    std::thread::spawn(move || serve(listener, d2, b2, w2));
    Server {
        base: format!("http://127.0.0.1:{}/", addr.port()),
        _data: data,
    }
}

fn test_settings() -> (Settings, std::path::PathBuf) {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("zdm-tauri-test-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    let mut s = Settings::default();
    s.download_dir = dir.to_string_lossy().into_owned();
    (s, dir)
}

async fn wait_for_status(
    manager: &zdm_lib::engine::Manager,
    id: &str,
    want: &str,
    timeout_secs: u64,
) -> DownloadView {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let views = manager.list_views().await;
        let view = views.iter().find(|v| v.id == id).unwrap().clone();
        if view.status == want || view.status == "failed" {
            return view;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for {want}, last status: {}", view.status);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn file_sha256(path: &std::path::Path) -> String {
    let data = std::fs::read(path).unwrap();
    sha256_hex(&data)
}

#[tokio::test]
async fn multi_segment_download_matches_hash() {
    let server = start_server();
    let expected = sha256_hex(&test_data(5 * 1024 * 1024));
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}file.bin", server.base);
    let handle = manager.add(&url, Some(8)).unwrap();
    let id = handle.id.clone();
    Manager::start_task(
        handle,
        settings.clone(),
        manager.limiter.clone(),
        None,
    )
    .await;
    let view = wait_for_status(&manager, &id, "completed", 60).await;
    assert_eq!(view.status, "completed");
    assert_eq!(view.total, Some(5 * 1024 * 1024));
    // Starts at 8 segments; dynamic segmentation splits the largest
    // remainder, so the final count is >= 8.
    assert!(view.segments.len() >= 8, "segments: {}", view.segments.len());
    assert!(view.segments.iter().all(|s| s.done));
    let dest = std::path::PathBuf::from(&view.dest_path);
    assert!(dest.exists());
    assert_eq!(file_sha256(&dest), expected);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn pause_resume_download_matches_hash() {
    let server = start_server();
    let expected = sha256_hex(&test_data(5 * 1024 * 1024));
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}file.bin", server.base);
    let handle = manager.add(&url, Some(8)).unwrap();
    let id = handle.id.clone();
    Manager::start_task(
        handle.clone(),
        settings.clone(),
        manager.limiter.clone(),
        None,
    )
    .await;
    // Let it run briefly, then pause.
    tokio::time::sleep(Duration::from_millis(100)).await;
    Manager::pause_task(handle.clone()).await;
    let views = manager.list_views().await;
    let view = views.iter().find(|v| v.id == id).unwrap();
    assert!(
        view.status == "paused" || view.status == "completed",
        "status after pause: {}",
        view.status
    );
    if view.status == "paused" {
        // Sidecar + part file must exist for resume.
        let part = format!("{}.part", view.dest_path);
        let sidecar = format!("{}.download.json", view.dest_path);
        assert!(std::path::Path::new(&part).exists());
        assert!(std::path::Path::new(&sidecar).exists());
        Manager::start_task(
            handle,
            settings.clone(),
            manager.limiter.clone(),
            None,
        )
        .await;
        let view = wait_for_status(&manager, &id, "completed", 60).await;
        assert_eq!(view.status, "completed");
        assert_eq!(file_sha256(&std::path::PathBuf::from(&view.dest_path)), expected);
    } else {
        // It finished before the pause landed — still a valid download.
        assert_eq!(file_sha256(&std::path::PathBuf::from(&view.dest_path)), expected);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn pending_confirm_downloads_with_user_choices() {
    // Full popup flow at the engine level: browser arrival -> pending
    // (nothing downloaded) -> user renames + picks a category folder
    // -> confirm -> the file lands at the chosen path with the
    // right hash, and prepare() kept the preset choices.
    let server = start_server();
    let expected = sha256_hex(&test_data(5 * 1024 * 1024));
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}file.bin", server.base);
    let pending = manager.add_pending(&url).unwrap().expect("pending created");
    assert_eq!(manager.list_pending().len(), 1);
    assert!(manager.tasks.is_empty(), "pending must not start a task");
    let dest_folder = dir.join("Programs");
    let opts = ConfirmOptions {
        filename: "renamed.bin".to_string(),
        dest_dir: dest_folder.to_string_lossy().into_owned(),
        category: "Programs".to_string(),
        connections: 8,
        start_now: false,
        expected_sha256: None,
    };
    let handle = manager.confirm_pending(&pending.id, opts).await.unwrap();
    assert!(manager.list_pending().is_empty());
    let id = handle.id.clone();
    Manager::start_task(
        handle,
        settings.clone(),
        manager.limiter.clone(),
        None,
    )
    .await;
    let view = wait_for_status(&manager, &id, "completed", 60).await;
    assert_eq!(view.status, "completed");
    assert_eq!(view.filename, "renamed.bin");
    assert_eq!(view.category, "Programs");
    let dest = dest_folder.join("renamed.bin");
    assert_eq!(view.dest_path, dest.to_string_lossy());
    assert!(dest.exists());
    assert_eq!(file_sha256(&dest), expected);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn no_range_server_falls_back_to_single_connection() {
    let server = start_server();
    let expected = sha256_hex(&test_data(5 * 1024 * 1024));
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}plain.bin", server.base);
    let handle = manager.add(&url, Some(8)).unwrap();
    let id = handle.id.clone();
    Manager::start_task(
        handle,
        settings.clone(),
        manager.limiter.clone(),
        None,
    )
    .await;
    let view = wait_for_status(&manager, &id, "completed", 60).await;
    assert_eq!(view.status, "completed");
    assert!(view.segments.is_empty(), "no-Range server must use single connection");
    assert_eq!(file_sha256(&std::path::PathBuf::from(&view.dest_path)), expected);
    // No hash from any source: still completed, state "unavailable"
    // (never a failure), and re-verify has nothing to compare.
    assert_eq!(view.verify_state, "unavailable");
    assert!(view.computed_hash.is_none());
    assert!(manager.verify_checksum(&id).await.is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unknown_size_chunked_download_completes() {
    let server = start_server();
    let expected = sha256_hex(&test_data(5 * 1024 * 1024));
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}chunked.bin", server.base);
    let handle = manager.add(&url, Some(1)).unwrap();
    let id = handle.id.clone();
    Manager::start_task(
        handle,
        settings.clone(),
        manager.limiter.clone(),
        None,
    )
    .await;
    let view = wait_for_status(&manager, &id, "completed", 60).await;
    assert_eq!(view.status, "completed");
    assert_eq!(file_sha256(&std::path::PathBuf::from(&view.dest_path)), expected);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn digest_header_verifies_on_completion() {
    let server = start_server();
    let expected = sha256_hex(&test_data(5 * 1024 * 1024));
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}digest.bin", server.base);
    let handle = manager.add(&url, Some(8)).unwrap();
    let id = handle.id.clone();
    Manager::start_task(
        handle,
        settings.clone(),
        manager.limiter.clone(),
        None,
    )
    .await;
    let view = wait_for_status(&manager, &id, "completed", 60).await;
    assert_eq!(view.status, "completed");
    assert_eq!(view.verify_state, "verified");
    assert_eq!(view.expected_hash.as_deref(), Some(expected.as_str()));
    assert_eq!(view.computed_hash.as_deref(), Some(expected.as_str()));
    // Manual re-verify through the manager API agrees.
    let res = manager.verify_checksum(&id).await.unwrap();
    assert!(res.verified);
    assert_eq!(res.computed_hash, expected);
    assert_eq!(res.expected_hash, expected);
    // Corrupt the file on disk: re-verify fails it (file kept).
    std::fs::write(&view.dest_path, b"tampered").unwrap();
    let res = manager.verify_checksum(&id).await.unwrap();
    assert!(!res.verified);
    let views = manager.list_views().await;
    let view = views.iter().find(|v| v.id == id).unwrap();
    assert_eq!(view.status, "failed");
    assert_eq!(view.verify_state, "mismatch");
    assert!(std::path::Path::new(&view.dest_path).exists());
    // Repair the file on disk, then re-verify: it flips back to
    // completed.
    std::fs::write(&view.dest_path, test_data(5 * 1024 * 1024)).unwrap();
    let res = manager.verify_checksum(&id).await.unwrap();
    assert!(res.verified);
    let views = manager.list_views().await;
    let view = views.iter().find(|v| v.id == id).unwrap();
    assert_eq!(view.status, "completed");
    assert_eq!(view.verify_state, "verified");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn digest_mismatch_fails_but_keeps_file() {
    let server = start_server();
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}digest-bad.bin", server.base);
    let handle = manager.add(&url, Some(8)).unwrap();
    let id = handle.id.clone();
    Manager::start_task(
        handle.clone(),
        settings.clone(),
        manager.limiter.clone(),
        None,
    )
    .await;
    let view = wait_for_status(&manager, &id, "failed", 60).await;
    assert_eq!(view.status, "failed");
    assert_eq!(view.verify_state, "mismatch");
    assert!(
        view.error.as_deref().unwrap_or("").starts_with("checksum mismatch"),
        "error: {:?}",
        view.error
    );
    // The file is kept on disk, not deleted.
    assert!(std::path::Path::new(&view.dest_path).exists());
    assert_ne!(view.computed_hash, view.expected_hash);
    // Retry re-downloads and fails the same way (same bad header).
    Manager::start_task(
        handle,
        settings.clone(),
        manager.limiter.clone(),
        None,
    )
    .await;
    let view = wait_for_status(&manager, &id, "failed", 60).await;
    assert_eq!(view.status, "failed");
    assert_eq!(view.verify_state, "mismatch");
    assert!(std::path::Path::new(&view.dest_path).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sidecar_hash_verifies_on_completion() {
    // /side.bin has no Digest header: the <url>.sha256 sidecar is
    // the only hash source.
    let server = start_server();
    let expected = sha256_hex(&test_data(5 * 1024 * 1024));
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}side.bin", server.base);
    let handle = manager.add(&url, Some(8)).unwrap();
    let id = handle.id.clone();
    Manager::start_task(
        handle,
        settings.clone(),
        manager.limiter.clone(),
        None,
    )
    .await;
    let view = wait_for_status(&manager, &id, "completed", 60).await;
    assert_eq!(view.status, "completed");
    assert_eq!(view.verify_state, "verified");
    assert_eq!(view.expected_hash.as_deref(), Some(expected.as_str()));
    assert_eq!(view.computed_hash.as_deref(), Some(expected.as_str()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn user_supplied_hash_verifies() {
    // Same field the Add URL modal / confirmation popup sets: the
    // engine path, with a server that sends no hash of its own.
    let server = start_server();
    let expected = sha256_hex(&test_data(5 * 1024 * 1024));
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}plain.bin", server.base);
    let handle = manager.add(&url, Some(1)).unwrap();
    handle.inner.lock().await.expected_hash = Some(zdm_lib::checksum::ExpectedHash {
        algo: zdm_lib::checksum::HashAlgo::Sha256,
        hex: expected.clone(),
        source: "user".to_string(),
    });
    let id = handle.id.clone();
    Manager::start_task(
        handle,
        settings.clone(),
        manager.limiter.clone(),
        None,
    )
    .await;
    let view = wait_for_status(&manager, &id, "completed", 60).await;
    assert_eq!(view.status, "completed");
    assert_eq!(view.verify_state, "verified");
    assert_eq!(view.computed_hash.as_deref(), Some(expected.as_str()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn scheduled_pause_resumes_when_window_opens() {
    // Full schedule loop with the clock injected (2026-11-09 is a
    // Monday; the window is 23:00–06:00 every day). The speed limit
    // keeps the 5 MiB file in flight long enough to pause it.
    use chrono::TimeZone;
    let server = start_server();
    let expected = sha256_hex(&test_data(5 * 1024 * 1024));
    let (mut settings, dir) = test_settings();
    settings.speed_limit_kbps = 200.0;
    settings.schedule = zdm_lib::schedule::DailySchedule {
        enabled: true,
        start: "23:00".to_string(),
        stop: "06:00".to_string(),
        days: [true; 7],
    };
    let mut manager = Manager::new(settings.clone());
    let outside = chrono::Local.with_ymd_and_hms(2026, 11, 9, 10, 0, 0).unwrap();
    let inside = chrono::Local.with_ymd_and_hms(2026, 11, 9, 23, 30, 0).unwrap();

    let url_a = format!("{}file.bin", server.base);
    let handle_a = manager.add(&url_a, Some(8)).unwrap();
    let id_a = handle_a.id.clone();
    let url_b = format!("{}plain.bin", server.base);
    let handle_b = manager.add(&url_b, Some(1)).unwrap();
    let id_b = handle_b.id.clone();
    manager.queue_running = true;
    for h in [&handle_a, &handle_b] {
        Manager::start_task(
            h.clone(),
            settings.clone(),
            manager.limiter.clone(),
            None,
        )
        .await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Window ends: the tick stops the queue; A is paused by the
    // scheduler, B by hand.
    assert_eq!(
        manager.schedule_tick(outside),
        zdm_lib::engine::ScheduleAction::Stop
    );
    assert!(!manager.queue_running);
    Manager::pause_task_scheduled(handle_a.clone()).await;
    Manager::pause_task(handle_b.clone()).await;
    let views = manager.list_views().await;
    assert_eq!(views.iter().find(|v| v.id == id_a).unwrap().status, "paused");
    assert_eq!(views.iter().find(|v| v.id == id_b).unwrap().status, "paused");

    // Window opens: the tick starts the queue and queue_tick resumes
    // only the scheduler-paused task; the manual pause stays paused.
    assert_eq!(
        manager.schedule_tick(inside),
        zdm_lib::engine::ScheduleAction::Start
    );
    assert!(manager.queue_running);
    manager.queue_tick().await;
    let view_a = wait_for_status(&manager, &id_a, "completed", 90).await;
    assert_eq!(view_a.status, "completed");
    assert_eq!(
        file_sha256(&std::path::PathBuf::from(&view_a.dest_path)),
        expected
    );
    let views = manager.list_views().await;
    assert_eq!(
        views.iter().find(|v| v.id == id_b).unwrap().status,
        "paused",
        "a manual pause must not be resumed by the schedule"
    );
    Manager::cancel_task(handle_b.clone()).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn startup_resume_continues_interrupted_download() {
    // Kill-and-reopen: a download cut off mid-flight (runner task
    // aborted — no cleanup runs, exactly like a crashed process: the
    // DB row stays "downloading" and the .part + sidecar keep their
    // bytes) continues from its saved offset when a fresh Manager
    // opens the same dir, and finishes with the right hash.
    let server = start_server();
    let expected = sha256_hex(&test_data(5 * 1024 * 1024));
    let (mut settings, dir) = test_settings();
    settings.speed_limit_kbps = 500.0;

    // --- Run 1: start the download, let it get most of the way.
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}file.bin", server.base);
    let handle = manager.add(&url, Some(8)).unwrap();
    manager.queue_running = true;
    Manager::start_task(handle.clone(), settings.clone(), manager.limiter.clone(), None).await;
    // Wait until ~4 MiB of 5 MiB is in (500 KB/s, ~8 s), polling the
    // in-memory progress.
    let deadline = Instant::now() + Duration::from_secs(60);
    let frozen = loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let d = handle.inner.lock().await.downloaded();
        if d >= 4 * 1024 * 1024 {
            break d;
        }
        if Instant::now() > deadline {
            panic!("download stalled before the kill point, downloaded={d}");
        }
    };
    assert!(frozen < 5 * 1024 * 1024);
    // The kill: abort the runner task. No cleanup runs.
    if let Some(j) = handle.join.lock().await.take() {
        j.abort();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(manager);
    // The DB row still says "downloading" with a real offset saved
    // (periodic saves run at most 1 s apart).
    let db_path = zdm_lib::db::db_path_for(&settings.download_dir);
    let rows = zdm_lib::db::list_downloads(&db_path).unwrap();
    let row = rows.iter().find(|r| r.filename.as_deref() == Some("file.bin")).unwrap();
    assert_eq!(row.status, "downloading");
    assert!(row.downloaded > 0, "no progress was persisted before the kill");

    // --- Run 2: a fresh Manager opens the same dir (the app was
    // reopened). The interrupted task is a resume candidate and
    // startup_resume starts the queue immediately.
    let mut manager2 = Manager::new(settings.clone());
    // The restored task gets a fresh in-memory id on restore
    // (next_id), so it is identified by filename, not by the old id.
    assert_eq!(manager2.resume_candidate_ids().len(), 1);
    assert!(manager2.startup_resume(chrono::Local::now()));
    assert!(manager2.queue_running);
    // The restored task already carries the frozen offset — it did
    // not come back at zero.
    let views = manager2.list_views().await;
    let restored = views.iter().find(|v| v.filename == "file.bin").unwrap();
    let id = restored.id.clone();
    assert_eq!(restored.status, "paused");
    assert!(restored.downloaded > 0 && restored.downloaded < 5 * 1024 * 1024);
    manager2.queue_tick().await;
    // From ~4 MiB at 500 KB/s the remaining ~1 MiB takes ~2 s; a
    // from-zero restart would need ~10 s. The 6 s bound is the proof
    // it continued from the offset, not from zero.
    let view = wait_for_status(&manager2, &id, "completed", 6).await;
    assert_eq!(view.status, "completed");
    assert_eq!(
        file_sha256(&std::path::PathBuf::from(&view.dest_path)),
        expected
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn resumed_download_never_counts_bytes_twice() {
    // Regression: after a kill mid-download + resume, the DB
    // `downloaded` ended up past `total` (33,189,513 for a
    // 31,457,280-byte file) while the file itself hashed fine.
    // Root cause is a race in download_segment: an idle worker
    // splits a segment while a chunk is in flight, and the worker
    // writes the chunk against the stale (larger) end — the bytes
    // past the new end are counted twice, once in each segment's
    // `pos`. The fix clamps every write to the segment's live end.
    // This test asserts the invariant at every persisted save and
    // at completion: downloaded <= total throughout, == total at
    // the end, hash match.
    //
    // The file is 64 MiB and the polls run every 10 ms: without a
    // speed limit the local download is fast enough that a smaller
    // file could finish between two polls (the kill point would be
    // missed — a 5 MiB file flaked exactly that way, and a 16 MiB
    // file too), and a shared rate limit evens the segments out so
    // the split race never triggers. The 64 MiB run takes ~30 s on
    // this VM. The race itself needs no kill to fire — it also
    // triggers on a plain fast download — but the kill/resume
    // cycles below are what the original e2e exercised.
    let total: u64 = 64 * 1024 * 1024;
    let server = start_server_sized(total as usize);
    // The expected hash comes from the server's own data instead of
    // regenerating a second 64 MiB vector.
    let expected = sha256_hex(&server._data);
    let (mut settings, dir) = test_settings();
    let db_path = zdm_lib::db::db_path_for(&settings.download_dir);

    // The persisted value is always a sampled in-memory value
    // (update_db writes g.downloaded(), and it only grows), so
    // checking the DB after each phase catches any excess a save
    // wrote during it. The live counter is checked every poll
    // instead: polling SQLite from this async loop every 10 ms
    // would block the runtime threads and starve the download.
    let assert_invariant = |db_path: &std::path::Path| {
        let rows = zdm_lib::db::list_downloads(db_path).unwrap();
        for r in &rows {
            if let Some(t) = r.total {
                assert!(
                    r.downloaded <= t,
                    "downloaded {} exceeded total {} (status {})",
                    r.downloaded, t, r.status
                );
            }
        }
    };

    // --- Run 1: start, kill at ~32 MiB.
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}file.bin", server.base);
    let handle = manager.add(&url, Some(8)).unwrap();
    manager.queue_running = true;
    Manager::start_task(handle.clone(), settings.clone(), manager.limiter.clone(), None).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    let frozen = loop {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let d = handle.inner.lock().await.downloaded();
        assert!(d <= total, "in-memory downloaded {d} exceeded total");
        if d >= 32 * 1024 * 1024 {
            break d;
        }
        if Instant::now() > deadline {
            panic!("download stalled before the kill point, downloaded={d}");
        }
    };
    assert!(frozen < total);
    if let Some(j) = handle.join.lock().await.take() {
        j.abort();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(manager);
    assert_invariant(&db_path);

    // --- Run 2: a fresh Manager resumes; kill again at ~48 MiB.
    // A second kill/resume cycle re-rolls the race (uneven segment
    // positions after each resume are what make idle workers split
    // busy segments) and gives the invariant a second workout.
    let mut manager2 = Manager::new(settings.clone());
    assert_eq!(manager2.resume_candidate_ids().len(), 1);
    assert!(manager2.startup_resume(chrono::Local::now()));
    let views = manager2.list_views().await;
    let restored = views.iter().find(|v| v.filename == "file.bin").unwrap();
    let id2 = restored.id.clone();
    manager2.queue_tick().await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let frozen2 = loop {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let v = manager2
            .list_views()
            .await
            .into_iter()
            .find(|v| v.id == id2)
            .unwrap();
        assert!(
            v.downloaded <= v.total.unwrap_or(u64::MAX),
            "live downloaded {} exceeded total {:?}",
            v.downloaded,
            v.total
        );
        if v.downloaded >= 48 * 1024 * 1024 {
            break v.downloaded;
        }
        if v.status == "completed" || v.status == "failed" {
            panic!("download finished before the second kill point");
        }
        if Instant::now() > deadline {
            panic!("resume stalled, last status: {}", v.status);
        }
    };
    assert!(frozen2 < total);
    // Kill the runner: the handle is found by the restored id
    // (before the manager is dropped — the Arc survives it).
    let handle2 = manager2
        .tasks
        .values()
        .find(|h| h.id == id2)
        .unwrap()
        .clone();
    if let Some(j) = handle2.join.lock().await.take() {
        j.abort();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(manager2);
    assert_invariant(&db_path);

    // --- Run 3: a fresh Manager resumes; poll the invariant while
    // it finishes.
    let mut manager3 = Manager::new(settings.clone());
    assert_eq!(manager3.resume_candidate_ids().len(), 1);
    assert!(manager3.startup_resume(chrono::Local::now()));
    let views = manager3.list_views().await;
    let restored = views.iter().find(|v| v.filename == "file.bin").unwrap();
    let id = restored.id.clone();
    manager3.queue_tick().await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let view = loop {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let v = manager3
            .list_views()
            .await
            .into_iter()
            .find(|v| v.id == id)
            .unwrap();
        assert!(
            v.downloaded <= v.total.unwrap_or(u64::MAX),
            "live downloaded {} exceeded total {:?}",
            v.downloaded,
            v.total
        );
        if v.status == "completed" || v.status == "failed" {
            break v;
        }
        if Instant::now() > deadline {
            panic!("resume stalled, last status: {}", v.status);
        }
    };
    assert_eq!(view.status, "completed");
    assert_eq!(view.downloaded, total, "final downloaded must equal total");
    assert_eq!(
        file_sha256(&std::path::PathBuf::from(&view.dest_path)),
        expected
    );
    let rows = zdm_lib::db::list_downloads(&db_path).unwrap();
    let row = rows
        .iter()
        .find(|r| r.filename.as_deref() == Some("file.bin"))
        .unwrap();
    assert_eq!(row.downloaded, total as i64);
    assert_eq!(row.total, Some(total as i64));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn startup_resume_leaves_manual_pause_and_toggle_off_alone() {
    // Same kill-and-reopen setup, but the download was paused by the
    // user before the restart: it must stay paused. And with the
    // toggle OFF, even an interrupted download must not auto-resume.
    let server = start_server();
    let (mut settings, dir) = test_settings();
    settings.speed_limit_kbps = 500.0;

    let mut manager = Manager::new(settings.clone());
    let url_a = format!("{}file.bin", server.base);
    let handle_a = manager.add(&url_a, Some(8)).unwrap();
    let url_b = format!("{}plain.bin", server.base);
    let handle_b = manager.add(&url_b, Some(1)).unwrap();
    manager.queue_running = true;
    for h in [&handle_a, &handle_b] {
        Manager::start_task(h.clone(), settings.clone(), manager.limiter.clone(), None).await;
    }
    // Wait until B's runner has registered its DB row: prepare()
    // inserts the row only after the HEAD probe, and under load the
    // probe can take a while. Killing B before its row exists leaves
    // nothing to restore, so a fixed sleep here flaked.
    let db_path = zdm_lib::db::db_path_for(&settings.download_dir);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let rows = zdm_lib::db::list_downloads(&db_path).unwrap();
        if rows
            .iter()
            .any(|r| r.filename.as_deref() == Some("plain.bin"))
        {
            break;
        }
        if Instant::now() > deadline {
            panic!("plain.bin never registered in the DB");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // A: paused by hand. B: killed mid-download (stays "downloading").
    Manager::pause_task(handle_a.clone()).await;
    if let Some(j) = handle_b.join.lock().await.take() {
        j.abort();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(manager);

    // Reopen with the toggle OFF: nothing auto-resumes.
    let mut settings_off = settings.clone();
    settings_off.resume_on_startup = false;
    let mut manager2 = Manager::new(settings_off);
    assert!(!manager2.startup_resume(chrono::Local::now()));
    assert!(!manager2.queue_running);
    // Reopen with the toggle ON: B is the only candidate; A stays
    // paused and queue_tick leaves it alone. (Restored tasks get
    // fresh in-memory ids, so candidates are counted, not matched
    // against the old ids.)
    let mut manager3 = Manager::new(settings.clone());
    assert_eq!(manager3.resume_candidate_ids().len(), 1);
    assert!(manager3.startup_resume(chrono::Local::now()));
    manager3.queue_tick().await;
    // B takes a moment to leave "paused" (HEAD probe + segment
    // setup); poll instead of a fixed sleep — a fixed 500 ms sleep
    // flaked once under load.
    let views = manager3.list_views().await;
    let id_b = views
        .iter()
        .find(|v| v.filename == "plain.bin")
        .unwrap()
        .id
        .clone();
    let view_b = wait_for_status(&manager3, &id_b, "downloading", 10).await;
    assert_eq!(view_b.status, "downloading");
    let views = manager3.list_views().await;
    assert_eq!(
        views.iter().find(|v| v.filename == "file.bin").unwrap().status,
        "paused",
        "a manual pause must survive a restart"
    );
    assert_eq!(
        views.iter().find(|v| v.filename == "plain.bin").unwrap().status,
        "downloading",
        "the interrupted download must have resumed"
    );
    Manager::cancel_task(handle_a.clone()).await;
    Manager::cancel_task(handle_b.clone()).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn grabber_page_finds_media() {
    let server = start_server();
    let client = reqwest::Client::builder().build().unwrap();
    let items = zdm_lib::grabber::grab_links(&format!("{}page.html", server.base), &client)
        .await
        .unwrap();
    use zdm_lib::grabber::GrabKind;
    let found: Vec<(String, GrabKind, Option<u64>)> = items
        .iter()
        .map(|i| (i.url.clone(), i.kind, i.size))
        .collect();
    let total = 5 * 1024 * 1024u64;
    // Media first (document order), then plain links; the duplicate
    // /clip.mp4 link is dropped and the blob: video is skipped.
    assert_eq!(
        found,
        vec![
            (format!("{}clip.mp4", server.base), GrabKind::Video, Some(total)),
            (format!("{}alt.webm", server.base), GrabKind::Video, Some(total)),
            (format!("{}song.mp3", server.base), GrabKind::Audio, Some(total)),
            (format!("{}file.bin", server.base), GrabKind::Link, Some(total)),
            (format!("{}stream.m3u8", server.base), GrabKind::Manifest, Some(total)),
        ]
    );
}

/// --- ClamAV scan -----------------------------------------------------------
/// A fake `clamscan` at one fixed path, shared by every scan test:
/// exit 1 + FOUND when the file contains the EICAR string, exit 0 +
/// OK otherwise. Every test points ZDM_CLAMSCAN at the same path, so
/// concurrent tests always set the identical value. The real binary
/// is never needed here — `scan::tests` covers it where one exists.

fn fake_clamscan_path() -> std::path::PathBuf {
    use std::io::Write as _;
    let dir = std::env::temp_dir().join("zdm-test-clamscan");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("clamscan");
    if !path.exists() {
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then echo 'ClamAV 9.9.9/1/Fake'; exit 0; fi\n\
             file=\"$2\"\n\
             if grep -q 'EICAR-STANDARD-ANTIVIRUS-TEST-FILE' \"$file\" 2>/dev/null; then\n\
             echo \"$file: Eicar-Test-Signature FOUND\"; exit 1; fi\n\
             echo \"$file: OK\"; exit 0"
        )
        .unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    path
}

fn use_fake_clamscan() {
    std::env::set_var("ZDM_CLAMSCAN", fake_clamscan_path());
}

/// Poll until the download's scan_state settles to `want`
/// (the automatic scan runs detached after completion). If the
/// automatic scan itself failed transiently — a process spawn can
/// fail under parallel load, which the engine reports as
/// "unavailable" by design — trigger one manual re-scan and keep
/// waiting: the scan logic is what the test proves, not the luck of
/// one fork under load.
async fn wait_for_scan(
    manager: &zdm_lib::engine::Manager,
    id: &str,
    want: &str,
    timeout_secs: u64,
) -> DownloadView {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut rescanned = false;
    loop {
        let views = manager.list_views().await;
        let view = views.iter().find(|v| v.id == id).unwrap().clone();
        if view.scan_state == want {
            return view;
        }
        if view.scan_state == "unavailable" && !rescanned && want != "unavailable" {
            rescanned = true;
            let _ = manager.scan_download(id).await;
        }
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for scan_state {want}, last: {} (status {})",
                view.scan_state, view.status
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn clean_download_scans_clean() {
    use_fake_clamscan();
    let server = start_server();
    let expected = sha256_hex(&test_data(5 * 1024 * 1024));
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}file.bin", server.base);
    let handle = manager.add(&url, Some(8)).unwrap();
    let id = handle.id.clone();
    Manager::start_task(handle, settings.clone(), manager.limiter.clone(), None).await;
    let view = wait_for_status(&manager, &id, "completed", 60).await;
    assert_eq!(view.status, "completed");
    // The scan runs after completion: the row must settle to clean,
    // with no threat, and the hash still matches.
    let view = wait_for_scan(&manager, &id, "clean", 30).await;
    assert_eq!(view.status, "completed");
    assert_eq!(view.scan_state, "clean");
    assert!(view.threat_name.is_none());
    assert_eq!(file_sha256(&std::path::PathBuf::from(&view.dest_path)), expected);
    // The settled state is persisted in the DB (a restart restores it).
    let db_path = zdm_lib::db::db_path_for(&settings.download_dir);
    let rows = zdm_lib::db::list_downloads(&db_path).unwrap();
    let row = rows.iter().find(|r| r.filename.as_deref() == Some("file.bin")).unwrap();
    assert_eq!(row.scan_state.as_deref(), Some("clean"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn eicar_download_fails_but_keeps_file_and_rescan_flips_back() {
    use_fake_clamscan();
    let server = start_server();
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}eicar.com", server.base);
    let handle = manager.add(&url, Some(1)).unwrap();
    let id = handle.id.clone();
    Manager::start_task(handle, settings.clone(), manager.limiter.clone(), None).await;
    // The download lands completed first; the detached scan then
    // fails it. Wait for either terminal state before judging the
    // scan (a transiently failed auto-scan leaves it completed —
    // wait_for_scan below re-scans manually in that case).
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let views = manager.list_views().await;
        let v = views.iter().find(|v| v.id == id).unwrap().clone();
        if v.status == "completed" || v.status == "failed" {
            break;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for a terminal status, last: {}", v.status);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let view = wait_for_scan(&manager, &id, "found", 30).await;
    assert_eq!(view.status, "failed");
    assert_eq!(view.scan_state, "found");
    assert_eq!(view.threat_name.as_deref(), Some("Eicar-Test-Signature"));
    assert!(
        view.error.as_deref().unwrap_or("").starts_with("Virus terdeteksi: Eicar-Test-Signature"),
        "error: {:?}",
        view.error
    );
    // The file is kept on disk, not deleted.
    let dest = std::path::PathBuf::from(&view.dest_path);
    assert!(dest.exists());
    assert_eq!(std::fs::read(&dest).unwrap(), zdm_lib::scan::EICAR.as_bytes());
    // Replace the file with clean content and re-scan manually: it
    // flips back to completed, mirroring the checksum re-verify.
    std::fs::write(&dest, b"cleaned").unwrap();
    let res = manager.scan_download(&id).await.unwrap();
    assert_eq!(res.state, zdm_lib::scan::ScanState::Clean);
    let views = manager.list_views().await;
    let view = views.iter().find(|v| v.id == id).unwrap();
    assert_eq!(view.status, "completed");
    assert_eq!(view.scan_state, "clean");
    assert!(view.error.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn scan_disabled_by_toggle_but_manual_scan_still_works() {
    use_fake_clamscan();
    let server = start_server();
    let (mut settings, dir) = test_settings();
    settings.clamav_scan = false;
    let mut manager = Manager::new(settings.clone());
    let url = format!("{}file.bin", server.base);
    let handle = manager.add(&url, Some(8)).unwrap();
    let id = handle.id.clone();
    Manager::start_task(handle, settings.clone(), manager.limiter.clone(), None).await;
    let view = wait_for_status(&manager, &id, "completed", 60).await;
    assert_eq!(view.status, "completed");
    // No automatic scan ran: the state stays unavailable after a
    // grace period (the fake scanner answers instantly, so 2 s is
    // plenty for a scan to have happened if one were scheduled).
    tokio::time::sleep(Duration::from_secs(2)).await;
    let views = manager.list_views().await;
    let view = views.iter().find(|v| v.id == id).unwrap();
    assert_eq!(view.scan_state, "unavailable");
    // The toggle gates only the automatic scan: an explicit manual
    // scan still runs and records its outcome.
    let res = manager.scan_download(&id).await.unwrap();
    assert_eq!(res.state, zdm_lib::scan::ScanState::Clean);
    let views = manager.list_views().await;
    let view = views.iter().find(|v| v.id == id).unwrap();
    assert_eq!(view.scan_state, "clean");
    assert_eq!(view.status, "completed");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- streams
//
// HLS/DASH merging: a local file server serves ffmpeg-generated VOD
// streams (6 s testsrc + sine, 3 x 2 s segments); the real engine
// downloads the playlist, fetches every segment, merges with ffmpeg
// and the result must play (ffprobe duration + video stream) with
// exactly the source's video frames (framemd5).

use std::collections::HashMap;
use std::sync::Mutex;

/// Tiny static file server with Range support, per-path dribble
/// delays (first half of the body, a pause, then the rest — so a
/// pause lands mid-segment), and per-path request counters (GETs,
/// and GETs that carried a Range header, for the resume proofs).
fn serve_files(
    listener: TcpListener,
    root: std::path::PathBuf,
    dribble: Arc<HashMap<String, Duration>>,
    counts: Arc<Mutex<HashMap<String, usize>>>,
    ranged: Arc<Mutex<HashMap<String, usize>>>,
) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let root = root.clone();
        let dribble = dribble.clone();
        let counts = counts.clone();
        let ranged = ranged.clone();
        std::thread::spawn(move || {
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        raw.extend_from_slice(&buf[..n]);
                        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => return,
                }
            }
            let req = String::from_utf8_lossy(&raw).into_owned();
            let mut lines = req.lines();
            let first = lines.next().unwrap_or("");
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or("GET");
            let path = parts.next().unwrap_or("/").to_string();
            let mut range: Option<(u64, u64)> = None;
            for line in lines {
                if line.is_empty() {
                    break;
                }
                // Header names are case-insensitive; reqwest sends
                // them lowercase ("range: bytes=...").
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("range:") {
                    let v = v.trim().trim_start_matches("bytes=");
                    if let Some((a, b)) = v.split_once('-') {
                        if let Ok(a) = a.parse::<u64>() {
                            range = Some((a, b.parse::<u64>().unwrap_or(u64::MAX)));
                        }
                    }
                }
            }
            let is_head = method == "HEAD";
            let rel = path.trim_start_matches('/');
            let file_path = root.join(rel);
            let data = match std::fs::read(&file_path) {
                Ok(d) => d,
                Err(_) => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
            };
            if method == "GET" {
                *counts.lock().unwrap().entry(path.clone()).or_insert(0) += 1;
                if range.is_some() {
                    *ranged.lock().unwrap().entry(path.clone()).or_insert(0) += 1;
                }
            }
            let ctype = if rel.ends_with(".m3u8") {
                "application/vnd.apple.mpegurl"
            } else if rel.ends_with(".mpd") {
                "application/dash+xml"
            } else if rel.ends_with(".ts") {
                "video/mp2t"
            } else if rel.ends_with(".m4s") {
                "video/iso.segment"
            } else if rel.ends_with(".mp4") {
                "video/mp4"
            } else {
                "application/octet-stream"
            };
            let total = data.len() as u64;
            let (status, slice_start, slice_end) = match range {
                Some((a, b)) if a < total => {
                    ("206 Partial Content", a, b.min(total - 1))
                }
                _ => ("200 OK", 0, total.saturating_sub(1)),
            };
            let body_len = if total == 0 { 0 } else { slice_end - slice_start + 1 };
            let mut headers = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nAccept-Ranges: bytes\r\n"
            );
            if status.starts_with("206") {
                headers.push_str(&format!(
                    "Content-Range: bytes {slice_start}-{slice_end}/{total}\r\n"
                ));
            }
            headers.push_str(&format!(
                "Content-Length: {body_len}\r\nConnection: close\r\n\r\n"
            ));
            let _ = stream.write_all(headers.as_bytes());
            if is_head || total == 0 {
                return;
            }
            let body = &data[slice_start as usize..=slice_end as usize];
            if let Some(delay) = dribble.get(&path) {
                // First half now, the rest after the delay: a pause
                // issued in between lands while bytes are in flight.
                let half = body.len() / 2;
                let _ = stream.write_all(&body[..half]);
                let _ = stream.flush();
                std::thread::sleep(*delay);
                let _ = stream.write_all(&body[half..]);
            } else {
                let _ = stream.write_all(body);
            }
            let _ = stream.flush();
        });
    }
}

struct FileServer {
    base: String,
    counts: Arc<Mutex<HashMap<String, usize>>>,
    ranged: Arc<Mutex<HashMap<String, usize>>>,
}

fn start_file_server(root: std::path::PathBuf, dribble: HashMap<String, Duration>) -> FileServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let counts = Arc::new(Mutex::new(HashMap::new()));
    let ranged = Arc::new(Mutex::new(HashMap::new()));
    let c2 = counts.clone();
    let r2 = ranged.clone();
    std::thread::spawn(move || {
        serve_files(listener, root, Arc::new(dribble), c2, r2)
    });
    FileServer {
        base: format!("http://127.0.0.1:{}/", addr.port()),
        counts,
        ranged,
    }
}

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_checked(cmd: &mut std::process::Command) {
    let out = cmd.output().expect("failed to spawn command");
    assert!(
        out.status.success(),
        "command {:?} failed: {}",
        cmd,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Generate the VOD fixtures with ffmpeg into `dir`:
/// source.mp4 (6 s), playlist.m3u8 (3 x 2 s TS), manifest.mpd
/// (fMP4, SegmentTimeline), enc.m3u8 (AES-128), live.m3u8 (no
/// EXT-X-ENDLIST). The key URL in the encrypted playlist is the
/// file server's base, so the server must be up first.
fn generate_stream_fixtures(dir: &std::path::Path, base: &str) {
    run_checked(
        std::process::Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i",
                "testsrc=duration=6:size=320x240:rate=10",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=6",
                "-c:v", "libx264", "-pix_fmt", "yuv420p", "-r", "10",
                // One keyframe every 2 s: -c copy HLS/DASH can only
                // split at keyframes, so without this the 6 s source
                // becomes a single segment.
                "-g", "20",
                "-c:a", "aac", "-shortest"])
            .arg(dir.join("source.mp4")),
    );
    run_checked(
        std::process::Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-i"])
            .arg(dir.join("source.mp4"))
            .args(["-c", "copy", "-f", "hls", "-hls_time", "2",
                "-hls_list_size", "0", "-hls_segment_filename"])
            .arg(dir.join("seg%d.ts"))
            .arg(dir.join("playlist.m3u8")),
    );
    run_checked(
        std::process::Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-i"])
            .arg(dir.join("source.mp4"))
            .args(["-c", "copy", "-f", "dash", "-seg_duration", "2",
                "-use_template", "1", "-use_timeline", "1"])
            .arg(dir.join("manifest.mpd")),
    );
    // AES-128 HLS: a fixed 16-byte key; the keyinfo file's first
    // line is the URL ffmpeg writes into the playlist.
    std::fs::write(dir.join("enc.key"), [0x11u8; 16]).unwrap();
    std::fs::write(
        dir.join("keyinfo"),
        format!("{base}enc.key\n{}\n", dir.join("enc.key").display()),
    )
    .unwrap();
    run_checked(
        std::process::Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-i"])
            .arg(dir.join("source.mp4"))
            .args(["-c", "copy", "-hls_time", "2", "-hls_list_size", "0",
                "-hls_segment_filename"])
            .arg(dir.join("enc%d.ts"))
            .args(["-hls_key_info_file"])
            .arg(dir.join("keyinfo"))
            .arg(dir.join("enc.m3u8")),
    );
    std::fs::write(
        dir.join("live.m3u8"),
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n\
         #EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2.0,\nseg0.ts\n\
         #EXTINF:2.0,\nseg1.ts\n",
    )
    .unwrap();
}

/// (duration seconds, has a video stream) from ffprobe.
fn ffprobe_info(path: &std::path::Path) -> (f64, bool) {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration",
            "-show_entries", "stream=codec_type", "-of", "json"])
        .arg(path)
        .output()
        .expect("ffprobe");
    assert!(out.status.success(), "ffprobe failed on {}", path.display());
    let text = String::from_utf8_lossy(&out.stdout);
    let duration = text
        .split("\"duration\": \"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    (duration, text.contains("\"codec_type\": \"video\""))
}

/// Per-frame MD5s of the video stream, in presentation order —
/// the content check: a -c copy merge must reproduce the source's
/// frames exactly, whatever container it came through.
fn frame_hashes(path: &std::path::Path) -> Vec<String> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-f", "framemd5", "-"])
        .output()
        .expect("ffmpeg framemd5");
    assert!(
        out.status.success(),
        "framemd5 failed on {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|l| l.to_string())
        .collect()
}

fn stream_fixtures_dir() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("zdm-stream-test-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn download_stream(
    manager: &mut Manager,
    settings: &Settings,
    url: &str,
) -> DownloadView {
    let handle = manager.add(url, Some(8)).unwrap();
    let id = handle.id.clone();
    Manager::start_task(handle, settings.clone(), manager.limiter.clone(), None).await;
    wait_for_status(manager, &id, "completed", 60).await
}

fn assert_merged_matches_source(dest: &std::path::Path, fixtures: &std::path::Path) {
    assert!(dest.exists(), "merged file missing: {}", dest.display());
    let (duration, has_video) = ffprobe_info(dest);
    assert!(has_video, "merged file has no video stream");
    assert!(
        (5.5..=6.5).contains(&duration),
        "merged duration {duration} not ~= 6 s"
    );
    assert_eq!(
        frame_hashes(dest),
        frame_hashes(&fixtures.join("source.mp4")),
        "merged video frames differ from the source"
    );
}

#[tokio::test]
async fn hls_stream_merges_to_playable_mp4() {
    let _stream_guard = lock_stream_tests().await;
    if !ffmpeg_available() {
        eprintln!("ffmpeg missing, skipping HLS integration test");
        return;
    }
    let fixtures = stream_fixtures_dir();
    let server = start_file_server(fixtures.clone(), HashMap::new());
    generate_stream_fixtures(&fixtures, &server.base);
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let view = download_stream(
        &mut manager,
        &settings,
        &format!("{}playlist.m3u8", server.base),
    )
    .await;
    assert_eq!(view.status, "completed", "error: {:?}", view.error);
    assert_eq!(view.stream_kind.as_deref(), Some("hls"));
    assert_eq!(view.segments_done, 3);
    assert_eq!(view.segments_total, 3);
    assert_eq!(view.filename, "playlist.mp4");
    assert_merged_matches_source(&std::path::PathBuf::from(&view.dest_path), &fixtures);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&fixtures);
}

#[tokio::test]
async fn dash_stream_muxes_to_playable_mp4() {
    let _stream_guard = lock_stream_tests().await;
    if !ffmpeg_available() {
        eprintln!("ffmpeg missing, skipping DASH integration test");
        return;
    }
    let fixtures = stream_fixtures_dir();
    let server = start_file_server(fixtures.clone(), HashMap::new());
    generate_stream_fixtures(&fixtures, &server.base);
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let view = download_stream(
        &mut manager,
        &settings,
        &format!("{}manifest.mpd", server.base),
    )
    .await;
    assert_eq!(view.status, "completed", "error: {:?}", view.error);
    assert_eq!(view.stream_kind.as_deref(), Some("dash"));
    // Video init + video segments + audio init + audio segments;
    // the exact count depends on ffmpeg's segmentation (audio gets
    // a trailing partial segment), so assert done == total.
    assert_eq!(view.segments_done, view.segments_total);
    assert!(view.segments_total >= 6, "total: {}", view.segments_total);
    assert_eq!(view.filename, "manifest.mp4");
    assert_merged_matches_source(&std::path::PathBuf::from(&view.dest_path), &fixtures);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&fixtures);
}

#[tokio::test]
async fn aes128_hls_stream_decrypts_and_merges() {
    let _stream_guard = lock_stream_tests().await;
    if !ffmpeg_available() {
        eprintln!("ffmpeg missing, skipping AES-128 integration test");
        return;
    }
    let fixtures = stream_fixtures_dir();
    let server = start_file_server(fixtures.clone(), HashMap::new());
    generate_stream_fixtures(&fixtures, &server.base);
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let view = download_stream(
        &mut manager,
        &settings,
        &format!("{}enc.m3u8", server.base),
    )
    .await;
    assert_eq!(view.status, "completed", "error: {:?}", view.error);
    assert_eq!(view.stream_kind.as_deref(), Some("hls"));
    assert_merged_matches_source(&std::path::PathBuf::from(&view.dest_path), &fixtures);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&fixtures);
}

#[tokio::test]
async fn live_hls_playlist_is_rejected() {
    let _stream_guard = lock_stream_tests().await;
    if !ffmpeg_available() {
        eprintln!("ffmpeg missing, skipping live-playlist integration test");
        return;
    }
    let fixtures = stream_fixtures_dir();
    let server = start_file_server(fixtures.clone(), HashMap::new());
    generate_stream_fixtures(&fixtures, &server.base);
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let handle = manager
        .add(&format!("{}live.m3u8", server.base), Some(8))
        .unwrap();
    let id = handle.id.clone();
    Manager::start_task(handle, settings.clone(), manager.limiter.clone(), None).await;
    let view = wait_for_status(&manager, &id, "failed", 30).await;
    assert_eq!(view.status, "failed");
    let err = view.error.clone().unwrap_or_default();
    assert!(
        err.contains("live stream"),
        "live playlist must fail honestly, got: {err}"
    );
    // The failure is durable: a row with the reason must exist in
    // the registry, or the failure vanishes from Completed after a
    // restart (restore_from_db reads the DB, not the tasks).
    let db_path = dir.join("downloads.db");
    let row: (String, String) = {
        let con = rusqlite::Connection::open(&db_path).unwrap();
        con.query_row(
            "SELECT status, error FROM downloads WHERE filename='live.m3u8'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(row.0, "failed");
    assert!(row.1.contains("live stream"), "db error: {}", row.1);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&fixtures);
}

#[tokio::test]
async fn dash_without_ffmpeg_downloads_playlist_only() {
    let _stream_guard = lock_stream_tests().await;
    if !ffmpeg_available() {
        eprintln!("ffmpeg missing, skipping DASH-no-ffmpeg integration test");
        return;
    }
    let fixtures = stream_fixtures_dir();
    let server = start_file_server(fixtures.clone(), HashMap::new());
    generate_stream_fixtures(&fixtures, &server.base);
    // Hide ffmpeg from the engine for this test only (the same
    // ZDM_FFMPEG seam the ClamAV tests use for their fake scanner).
    // HideFfmpeg restores the variable on drop, even on panic.
    let _hide = HideFfmpeg::hide();
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let view = download_stream(
        &mut manager,
        &settings,
        &format!("{}manifest.mpd", server.base),
    )
    .await;
    assert_eq!(view.status, "completed", "error: {:?}", view.error);
    assert!(view.stream_kind.is_none(), "no merge without ffmpeg");
    assert_eq!(view.filename, "manifest.mpd");
    assert!(
        view.stream_note.as_deref().unwrap_or_default().contains("ffmpeg"),
        "fallback must say ffmpeg is missing, note: {:?}",
        view.stream_note
    );
    let dest = std::path::PathBuf::from(&view.dest_path);
    assert!(dest.exists());
    assert!(
        std::fs::read(&dest).unwrap().starts_with(b"<?xml"),
        "fallback downloads the playlist file itself"
    );
    // The note must survive on disk, not just in memory: the
    // completed download's sidecar is the note's durable home —
    // the success path used to delete the sidecar unconditionally,
    // so the note never reached the disk (laptop e2e phase 5).
    let sidecar = std::path::PathBuf::from(format!("{}.download.json", dest.display()));
    let sidecar_text = std::fs::read_to_string(&sidecar)
        .unwrap_or_else(|_| panic!("fallback sidecar missing: {}", sidecar.display()));
    assert!(
        sidecar_text.contains("ffmpeg"),
        "sidecar must carry the fallback note: {sidecar_text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&fixtures);
}

#[tokio::test]
async fn stream_pause_resume_skips_finished_segments() {
    let _stream_guard = lock_stream_tests().await;
    if !ffmpeg_available() {
        eprintln!("ffmpeg missing, skipping stream resume integration test");
        return;
    }
    let fixtures = stream_fixtures_dir();
    // seg1/seg2 dribble their second half after 1.5 s, so a pause
    // at ~300 ms lands while bytes are in flight; seg0 is instant.
    let mut dribble = HashMap::new();
    dribble.insert("/seg1.ts".to_string(), Duration::from_millis(1500));
    dribble.insert("/seg2.ts".to_string(), Duration::from_millis(1500));
    let server = start_file_server(fixtures.clone(), dribble);
    generate_stream_fixtures(&fixtures, &server.base);
    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let handle = manager
        .add(&format!("{}playlist.m3u8", server.base), Some(8))
        .unwrap();
    let id = handle.id.clone();
    Manager::start_task(handle.clone(), settings.clone(), manager.limiter.clone(), None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    Manager::pause_task(handle.clone()).await;
    let views = manager.list_views().await;
    let view = views.iter().find(|v| v.id == id).unwrap();
    assert_eq!(view.status, "paused", "pause must land before the merge");
    let counts = server.counts.lock().unwrap();
    assert_eq!(counts.get("/seg0.ts").copied().unwrap_or(0), 1);
    assert_eq!(counts.get("/seg1.ts").copied().unwrap_or(0), 1);
    assert_eq!(counts.get("/seg2.ts").copied().unwrap_or(0), 1);
    drop(counts);
    // Simulate a kill mid-segment: truncate seg-000001.ts (the
    // second segment) to half its bytes. Resume must notice the
    // size mismatch, re-fetch only the tail with a Range request,
    // and never touch the finished segments.
    let seg_dir = std::path::PathBuf::from(format!("{}.segments", view.dest_path));
    let seg1 = seg_dir.join("seg-000001.ts");
    {
        use std::io::Write as _;
        let data = std::fs::read(&seg1).unwrap();
        let mut f = std::fs::File::create(&seg1).unwrap();
        f.write_all(&data[..data.len() / 2]).unwrap();
    }
    Manager::start_task(handle, settings.clone(), manager.limiter.clone(), None).await;
    let view = wait_for_status(&manager, &id, "completed", 60).await;
    assert_eq!(view.status, "completed", "error: {:?}", view.error);
    let counts = server.counts.lock().unwrap();
    assert_eq!(counts.get("/seg0.ts").copied().unwrap_or(0), 1, "finished seg0 re-downloaded");
    assert_eq!(counts.get("/seg2.ts").copied().unwrap_or(0), 1, "finished seg2 re-downloaded");
    assert_eq!(counts.get("/seg1.ts").copied().unwrap_or(0), 2, "truncated seg1 must be re-fetched once");
    drop(counts);
    let ranged = server.ranged.lock().unwrap();
    assert_eq!(
        ranged.get("/seg1.ts").copied().unwrap_or(0),
        1,
        "truncated seg1 must resume with a Range request, not a full re-download"
    );
    drop(ranged);
    // The merged file is the final proof the resumed tail landed
    // in the right place (the segments dir is deleted after the
    // merge, so segment sizes can no longer be read here).
    assert_merged_matches_source(&std::path::PathBuf::from(&view.dest_path), &fixtures);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&fixtures);
}

// ------------------------------------------------------- update checker --

/// Tiny manifest server: GET /version.json serves the manifest body,
/// GET /asset.bin serves the asset bytes. Anything else is a 404.
struct ManifestServer {
    base: String,
}

fn serve_manifest(listener: TcpListener, manifest: Arc<String>, asset: Arc<Vec<u8>>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let manifest = manifest.clone();
        let asset = asset.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let Ok(n) = stream.read(&mut buf) else { return };
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .unwrap_or("");
            let (status, ctype, body): (&str, &str, &[u8]) = match path {
                "/version.json" => ("200 OK", "application/json", manifest.as_bytes()),
                "/asset.bin" => ("200 OK", "application/octet-stream", &asset),
                _ => ("404 Not Found", "text/plain", b"not found"),
            };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = stream.write_all(body);
            let _ = stream.flush();
        });
    }
}

fn start_manifest_server_raw(manifest: String, asset: Vec<u8>) -> ManifestServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}/", listener.local_addr().unwrap().port());
    let manifest = Arc::new(manifest);
    let asset = Arc::new(asset);
    std::thread::spawn(move || serve_manifest(listener, manifest, asset));
    ManifestServer { base }
}

/// Like the raw server, but the manifest body is built from the
/// server's own base URL, so download_url points at its asset.
fn start_manifest_server(version: &str, sha: &str, asset: Vec<u8>) -> ManifestServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}/", listener.local_addr().unwrap().port());
    let manifest = Arc::new(format!(
        r#"{{"version":"{version}","download_url":"{base}asset.bin","sha256":"{sha}","changelog":"Faster downloads."}}"#
    ));
    let asset = Arc::new(asset);
    std::thread::spawn(move || serve_manifest(listener, manifest, asset));
    ManifestServer { base }
}

fn update_client() -> reqwest::Client {
    let (settings, _dir) = test_settings();
    zdm_lib::engine::build_client(&settings).unwrap()
}

#[tokio::test]
async fn update_check_newer_version_offers_update() {
    let asset = test_data(64 * 1024);
    let sha = sha256_hex(&asset);
    let server = start_manifest_server("0.2.0", &sha, asset);
    let client = update_client();
    let check = zdm_lib::update::check_manifest(
        &client,
        &format!("{}version.json", server.base),
        zdm_lib::update::CURRENT_VERSION,
    )
    .await;
    assert_eq!(check.state, zdm_lib::update::UpdateState::UpdateAvailable);
    let manifest = check.manifest.unwrap();
    assert_eq!(manifest.version, "0.2.0");
    assert_eq!(manifest.download_url, format!("{}asset.bin", server.base));
    assert_eq!(manifest.sha256.as_deref(), Some(sha.as_str()));
}

#[tokio::test]
async fn update_check_same_version_is_up_to_date() {
    let asset = test_data(64 * 1024);
    let sha = sha256_hex(&asset);
    let current = zdm_lib::update::CURRENT_VERSION;
    let server = start_manifest_server(current, &sha, asset);
    let client = update_client();
    let check = zdm_lib::update::check_manifest(
        &client,
        &format!("{}version.json", server.base),
        current,
    )
    .await;
    assert_eq!(check.state, zdm_lib::update::UpdateState::UpToDate);
    assert!(check.manifest.is_some());
}

#[tokio::test]
async fn update_check_broken_manifest_is_invalid() {
    let server = start_manifest_server_raw("this is not json{".to_string(), vec![]);
    let client = update_client();
    let check = zdm_lib::update::check_manifest(
        &client,
        &format!("{}version.json", server.base),
        zdm_lib::update::CURRENT_VERSION,
    )
    .await;
    assert_eq!(check.state, zdm_lib::update::UpdateState::InvalidManifest);
    assert!(check.manifest.is_none());
}

#[tokio::test]
async fn update_check_dead_source_is_unreachable() {
    // Bind a port, then drop the listener: nothing answers there.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let client = update_client();
    let check = zdm_lib::update::check_manifest(
        &client,
        &format!("http://127.0.0.1:{port}/version.json"),
        zdm_lib::update::CURRENT_VERSION,
    )
    .await;
    assert_eq!(check.state, zdm_lib::update::UpdateState::Unreachable);
    assert!(check.manifest.is_none());
}

#[tokio::test]
async fn update_asset_wrong_sha_fails_but_keeps_file() {
    // The manifest offers 0.2.0 with a sha that does not match the
    // asset — the Download button would queue exactly this URL +
    // sha, and the engine must fail it like a checksum mismatch
    // (file kept, never installed).
    let asset = test_data(64 * 1024);
    let wrong_sha = sha256_hex(b"tampered asset, not the real bytes");
    let server = start_manifest_server("0.2.0", &wrong_sha, asset);
    let client = update_client();
    let check = zdm_lib::update::check_manifest(
        &client,
        &format!("{}version.json", server.base),
        zdm_lib::update::CURRENT_VERSION,
    )
    .await;
    assert_eq!(check.state, zdm_lib::update::UpdateState::UpdateAvailable);
    let manifest = check.manifest.unwrap();

    let (settings, dir) = test_settings();
    let mut manager = Manager::new(settings.clone());
    let handle = manager.add(&manifest.download_url, Some(1)).unwrap();
    handle.inner.lock().await.expected_hash = Some(zdm_lib::checksum::ExpectedHash {
        algo: zdm_lib::checksum::HashAlgo::Sha256,
        hex: manifest.sha256.unwrap(),
        source: "update manifest".to_string(),
    });
    let id = handle.id.clone();
    Manager::start_task(
        handle,
        settings.clone(),
        manager.limiter.clone(),
        None,
    )
    .await;
    let view = wait_for_status(&manager, &id, "failed", 60).await;
    assert_eq!(view.status, "failed");
    assert_eq!(view.verify_state, "mismatch");
    assert!(
        view.error.as_deref().unwrap_or("").starts_with("checksum mismatch"),
        "error: {:?}",
        view.error
    );
    assert!(std::path::Path::new(&view.dest_path).exists());
    let _ = std::fs::remove_dir_all(&dir);
}
