//! Tauri command layer for ZDM — wires the engine to the frontend.

pub mod autostart;
pub mod checksum;
pub mod db;
pub mod engine;
pub mod grabber;
pub mod notify;
pub mod scan;
pub mod schedule;
pub mod settings;
pub mod stream;
pub mod update;

use engine::{ConfirmOptions, DownloadView, Manager, PendingDownload, Stats};
use settings::{load_settings, save_settings as persist_settings, Settings};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tauri::menu::{Menu, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager as _, State};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::Mutex as AsyncMutex;

pub type AppState = Arc<AsyncMutex<Manager>>;

/// Path of the browser queue file, shared with the Python ZDM native
/// host (`browser/zdm-native-host.py`) and the Python ZDM
/// (`download-manager-linux/browser/native_host.py`): a JSON array of
/// URL strings inside the download dir.
pub fn queue_file_path(settings: &Settings) -> PathBuf {
    PathBuf::from(&settings.download_dir).join("queue.json")
}

fn queue_lock_path(settings: &Settings) -> PathBuf {
    PathBuf::from(&settings.download_dir).join(".queue.lock")
}

/// Parse queue-file contents (JSON array of URL strings). Anything
/// unparsable yields no URLs — a corrupt file must never break the
/// queue loop. Duplicates and blank entries are dropped.
pub fn parse_queue_file(text: &str) -> Vec<String> {
    let Ok(urls) = serde_json::from_str::<Vec<String>>(text) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for u in urls {
        let u = u.trim().to_string();
        if !u.is_empty() && !out.contains(&u) {
            out.push(u);
        }
    }
    out
}

/// Try to take the queue-file lock (exclusive-create lock file).
/// The browser native host uses the same file, so a browser download
/// arriving mid-drain is never lost. A lock older than 10 s is
/// treated as stale (crashed holder) and removed once. Returns the
/// lock path on success; the caller must remove the file afterwards.
fn try_queue_lock(settings: &Settings) -> Option<PathBuf> {
    let lock = queue_lock_path(settings);
    if let Some(parent) = lock.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let open = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .is_ok()
    };
    if open() {
        return Some(lock);
    }
    if let Ok(meta) = std::fs::metadata(&lock) {
        let stale = meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|age| age.as_secs() > 10)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&lock);
            if open() {
                return Some(lock);
            }
        }
    }
    None
}

async fn get_handle(state: &State<'_, AppState>, id: &str) -> Result<Arc<engine::TaskHandle>, String> {
    state
        .lock()
        .await
        .tasks
        .get(id)
        .cloned()
        .ok_or_else(|| "download not found".to_string())
}

/// Snapshot the bits a task-start needs, without holding the lock.
async fn start_context(state: &State<'_, AppState>) -> (Settings, Arc<AsyncMutex<engine::RateLimiter>>, Option<AppHandle>) {
    let m = state.lock().await;
    (
        m.settings.clone(),
        m.limiter.clone(),
        m.app.clone(),
    )
}

#[tauri::command]
async fn add_download(
    state: State<'_, AppState>,
    url: String,
    connections: Option<u32>,
    expected_sha256: Option<String>,
) -> Result<String, String> {
    let (handle, queue_running) = {
        let mut m = state.lock().await;
        let handle = m.add(&url, connections)?;
        // User-supplied expected hash wins over any header or
        // sidecar. An invalid value fails loudly at entry time.
        if let Some(raw) = expected_sha256.as_deref() {
            let raw = raw.trim();
            if !raw.is_empty() {
                let hex = checksum::normalize_sha256(raw)
                    .ok_or_else(|| "expected SHA-256 must be 64 hex characters".to_string())?;
                handle.inner.lock().await.expected_hash = Some(checksum::ExpectedHash {
                    algo: checksum::HashAlgo::Sha256,
                    hex,
                    source: "user".to_string(),
                });
            }
        }
        (handle, m.queue_running)
    };
    let id = handle.id.clone();
    if queue_running {
        let (settings, limiter, app) = start_context(&state).await;
        Manager::start_task(handle, settings, limiter, app).await;
    }
    Ok(id)
}

/// Re-verify a finished download against its stored expected hash.
#[tauri::command]
async fn verify_checksum(state: State<'_, AppState>, id: String) -> Result<engine::VerifyResult, String> {
    state.lock().await.verify_checksum(&id).await
}

/// Re-scan a finished download with ClamAV.
#[tauri::command]
async fn scan_download(state: State<'_, AppState>, id: String) -> Result<scan::ScanResult, String> {
    state.lock().await.scan_download(&id).await
}

/// Whether a ClamAV scanner is available, for the Settings row.
#[tauri::command]
fn get_clamav_status() -> scan::ScannerStatus {
    scan::scanner_status()
}

/// ffmpeg availability for stream merging (HLS/DASH): the Grab tab
/// labels DASH items honestly when ffmpeg is missing.
#[derive(serde::Serialize)]
struct FfmpegStatus {
    available: bool,
    version: Option<String>,
}

/// The update-check result the UI shows, plus the dismissed version
/// (a dismissed offer stays hidden until a newer version appears).
/// A stored check only counts for the manifest URL it was fetched
/// from — changing the URL invalidates the old offer.
#[derive(serde::Serialize)]
struct UpdateStatusView {
    check: Option<update::UpdateCheck>,
    dismissed_version: Option<String>,
}

#[tauri::command]
async fn check_for_updates(state: State<'_, AppState>) -> Result<update::UpdateCheck, String> {
    let url = state.lock().await.settings.update_manifest_url.clone();
    let check = if url.trim().is_empty() {
        update::UpdateCheck {
            state: update::UpdateState::NoSource,
            current_version: update::CURRENT_VERSION.to_string(),
            manifest: None,
            source_url: String::new(),
            checked_at: chrono::Local::now().to_rfc3339(),
            message: "No update source configured.".to_string(),
        }
    } else {
        let client = {
            let m = state.lock().await;
            engine::build_client(&m.settings)?
        };
        update::check_manifest(&client, url.trim(), update::CURRENT_VERSION).await
    };
    let mut file = update::load_update_state();
    file.last_checked = Some(check.checked_at.clone());
    file.last_check = Some(check.clone());
    update::save_update_state(&file)?;
    Ok(check)
}

#[tauri::command]
async fn get_update_status(state: State<'_, AppState>) -> Result<UpdateStatusView, String> {
    let url = state.lock().await.settings.update_manifest_url.clone();
    let file = update::load_update_state();
    let check = file.last_check.filter(|c| c.source_url == url);
    Ok(UpdateStatusView {
        check,
        dismissed_version: file.dismissed_version,
    })
}

#[tauri::command]
async fn dismiss_update(version: String) -> Result<(), String> {
    let mut file = update::load_update_state();
    file.dismissed_version = Some(version);
    update::save_update_state(&file)
}

/// The once-a-day startup check: a few seconds after launch, fetch
/// the manifest once if a source is configured and today has not
/// been checked yet. Runs in a detached task — a dead source must
/// never delay startup, and the result is only ever read from the
/// state file, never pushed anywhere.
async fn maybe_auto_check(state: AppState) {
    tokio::time::sleep(Duration::from_secs(5)).await;
    let (url, settings) = {
        let m = state.lock().await;
        (m.settings.update_manifest_url.clone(), m.settings.clone())
    };
    if url.trim().is_empty() {
        return;
    }
    let file = update::load_update_state();
    if !update::should_auto_check(file.last_checked.as_deref(), chrono::Local::now()) {
        return;
    }
    let Ok(client) = engine::build_client(&settings) else {
        return;
    };
    let check = update::check_manifest(&client, url.trim(), update::CURRENT_VERSION).await;
    let mut file = update::load_update_state();
    file.last_checked = Some(check.checked_at.clone());
    file.last_check = Some(check);
    let _ = update::save_update_state(&file);
}

#[tauri::command]
fn get_ffmpeg_status() -> FfmpegStatus {
    FfmpegStatus {
        available: stream::ffmpeg_available(),
        version: stream::ffmpeg_version(),
    }
}

#[tauri::command]
async fn pause_download(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let handle = get_handle(&state, &id).await?;
    Manager::pause_task(handle).await;
    Ok(())
}

#[tauri::command]
async fn resume_download(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let handle = get_handle(&state, &id).await?;
    let status = handle.inner.lock().await.status.clone();
    if !["paused", "queued", "failed"].contains(&status.as_str()) {
        return Err(format!("cannot resume a {status} download"));
    }
    let (settings, limiter, app) = start_context(&state).await;
    Manager::start_task(handle, settings, limiter, app).await;
    Ok(())
}

#[tauri::command]
async fn cancel_download(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let handle = get_handle(&state, &id).await?;
    Manager::cancel_task(handle).await;
    Ok(())
}

#[tauri::command]
async fn delete_download(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let handle = get_handle(&state, &id).await?;
    let (dest_path, db_id, download_dir) = {
        let g = handle.inner.lock().await;
        (
            g.dest_path.clone(),
            g.db_id,
            g.dest_dir.clone(),
        )
    };
    Manager::cancel_task(handle).await;
    let _ = std::fs::remove_file(&dest_path);
    let _ = std::fs::remove_dir_all(stream::segments_dir(&dest_path));
    if let Some(db_id) = db_id {
        let _ = db::delete_download(&db::db_path_for(&download_dir), db_id);
    }
    let mut m = state.lock().await;
    m.tasks.remove(&id);
    m.order.retain(|x| x != &id);
    Ok(())
}

#[tauri::command]
async fn list_downloads(state: State<'_, AppState>) -> Result<Vec<DownloadView>, String> {
    Ok(state.lock().await.list_views().await)
}

#[tauri::command]
async fn get_stats(state: State<'_, AppState>) -> Result<Stats, String> {
    Ok(state.lock().await.stats().await)
}

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> Result<Settings, String> {
    Ok(state.lock().await.settings.clone())
}

#[tauri::command]
async fn save_settings(state: State<'_, AppState>, settings: Settings) -> Result<(), String> {
    let settings = settings.normalized();
    persist_settings(&settings)?;
    let mut m = state.lock().await;
    let limit = settings.speed_limit_kbps;
    m.settings = settings.clone();
    m.limiter.lock().await.set_limit(limit);
    // Keep the installed .desktop files in line with the toggle.
    // The setting itself is already saved above, so a failure here
    // only means the files lag until the next startup sync.
    autostart::sync_autostart(settings.autostart)?;
    Ok(())
}

#[tauri::command]
async fn add_batch(state: State<'_, AppState>, urls: Vec<String>) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    let mut handles = Vec::new();
    {
        let mut m = state.lock().await;
        for url in urls {
            let url = url.trim().to_string();
            if url.is_empty() {
                continue;
            }
            let handle = m.add(&url, None)?;
            ids.push(handle.id.clone());
            handles.push(handle);
        }
        if !m.queue_running {
            return Ok(ids);
        }
    }
    let (settings, limiter, app) = start_context(&state).await;
    for handle in handles {
        Manager::start_task(handle, settings.clone(), limiter.clone(), app.clone()).await;
    }
    Ok(ids)
}

#[tauri::command]
async fn grab_links(page_url: String) -> Result<Vec<grabber::GrabItem>, String> {
    let settings = load_settings();
    let client = engine::build_client(&settings)?;
    grabber::grab_links(&page_url, &client).await
}

#[tauri::command]
async fn start_queue(state: State<'_, AppState>) -> Result<(), String> {
    let mut m = state.lock().await;
    m.queue_running = true;
    m.scheduled_at = None;
    // A manual start holds the daily window until its next boundary.
    m.schedule_hold = true;
    Ok(())
}

#[tauri::command]
async fn stop_queue(state: State<'_, AppState>) -> Result<(), String> {
    let active: Vec<_> = {
        let mut m = state.lock().await;
        m.queue_running = false;
        m.scheduled_at = None;
        // A manual stop holds the daily window until its next boundary.
        m.schedule_hold = true;
        m.tasks.values().cloned().collect()
    };
    for h in active {
        let g = h.inner.lock().await;
        if g.status == "downloading" {
            drop(g);
            Manager::pause_task(h).await;
        }
    }
    Ok(())
}

#[tauri::command]
async fn schedule_queue(state: State<'_, AppState>, start_time: String) -> Result<(), String> {
    let when = chrono::NaiveDateTime::parse_from_str(&start_time, "%Y-%m-%d %H:%M")
        .map_err(|e| format!("bad start time (use YYYY-MM-DD HH:MM): {e}"))?;
    let mut m = state.lock().await;
    m.scheduled_at = Some(when);
    m.queue_running = true;
    // A pending one-shot holds the daily window until it fires.
    m.schedule_hold = true;
    Ok(())
}

/// Current state of the daily schedule, for the Settings and
/// Downloads views ("Queue starts today 23:00" and the like).
#[derive(serde::Serialize)]
struct ScheduleStatus {
    enabled: bool,
    in_window: bool,
    next_label: Option<String>,
}

#[tauri::command]
async fn get_schedule_status(state: State<'_, AppState>) -> Result<ScheduleStatus, String> {
    let m = state.lock().await;
    let now = chrono::Local::now();
    Ok(ScheduleStatus {
        enabled: m.settings.schedule.enabled,
        in_window: m.settings.schedule.contains(now),
        next_label: m.settings.schedule.next_label(now),
    })
}

// ------------------------------------------------------------ pending

#[tauri::command]
async fn list_pending(state: State<'_, AppState>) -> Result<Vec<PendingDownload>, String> {
    Ok(state.lock().await.list_pending())
}

/// Confirm a pending download with the user's popup choices.
/// `start_now` ("Start Download") spawns the runner immediately
/// without touching the queue's running flag; "Add to Queue" leaves
/// the task queued for the queue to pick up.
#[tauri::command]
async fn confirm_pending(
    state: State<'_, AppState>,
    id: String,
    options: ConfirmOptions,
) -> Result<String, String> {
    let start_now = options.start_now;
    let handle = {
        let mut m = state.lock().await;
        m.confirm_pending(&id, options).await?
    };
    let task_id = handle.id.clone();
    if start_now {
        let (settings, limiter, app) = start_context(&state).await;
        Manager::start_task(handle, settings, limiter, app).await;
    }
    Ok(task_id)
}

#[tauri::command]
async fn dismiss_pending(state: State<'_, AppState>, id: String) -> Result<(), String> {
    if state.lock().await.dismiss_pending(&id) {
        Ok(())
    } else {
        Err("pending download not found".to_string())
    }
}

async fn pause_all_tasks(state: &AppState) {
    let active: Vec<_> = state.lock().await.tasks.values().cloned().collect();
    for h in active {
        if h.inner.lock().await.status == "downloading" {
            Manager::pause_task(h).await;
        }
    }
}

/// Pause all running downloads because the daily window ended:
/// they are marked so the queue resumes them when the window opens
/// again (a manual Pause All does not mark them).
async fn pause_all_tasks_scheduled(state: &AppState) {
    let active: Vec<_> = state.lock().await.tasks.values().cloned().collect();
    for h in active {
        if h.inner.lock().await.status == "downloading" {
            Manager::pause_task_scheduled(h).await;
        }
    }
}

#[tauri::command]
async fn pause_all(state: State<'_, AppState>) -> Result<(), String> {
    pause_all_tasks(&state).await;
    Ok(())
}

fn fmt_speed(bps: f64) -> String {
    if !bps.is_finite() || bps <= 0.0 {
        return "0 KB/s".to_string();
    }
    if bps < 1024.0 {
        return format!("{} B/s", bps.round() as u64);
    }
    if bps < 1024.0 * 1024.0 {
        return format!("{:.1} KB/s", bps / 1024.0);
    }
    format!("{:.1} MB/s", bps / (1024.0 * 1024.0))
}

/// Refresh the tray tooltip from live manager state (throttled: the
/// queue loop calls this every ~2 s, never per progress event).
fn update_tray_tooltip(m: &Manager) {
    let Some(app) = &m.app else {
        return;
    };
    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    let mut active = 0u64;
    let mut queued = 0u64;
    let mut speed = 0.0f64;
    for h in m.tasks.values() {
        if let Ok(g) = h.inner.try_lock() {
            match g.status.as_str() {
                "downloading" => {
                    active += 1;
                    speed += g.speed_bps;
                }
                "queued" => queued += 1,
                _ => {}
            }
        }
    }
    let text = if active > 0 {
        format!("ZDM — {active} downloading, {}", fmt_speed(speed))
    } else if queued > 0 {
        format!("ZDM — {queued} queued")
    } else {
        "ZDM — idle".to_string()
    };
    let _ = tray.set_tooltip(Some(text));
}

/// Tauri 2 has no minimise window event on Linux, so "minimise to
/// tray" is detected by polling: a minimised window is hidden.
/// Idempotent — hiding an already-hidden window is a no-op.
fn hide_minimised_window(m: &Manager) {
    if !m.settings.minimize_to_tray {
        return;
    }
    let Some(app) = &m.app else {
        return;
    };
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    if win.is_minimized().unwrap_or(false) {
        let _ = win.hide();
    }
}

/// Probe a pending URL in the background (10 s cap so a dead server
/// never hangs anything): fills in file size / server filename, then
/// re-emits `pending-download` so the popup shows the size.
async fn probe_pending(state: AppState, id: String, url: String) {
    let settings = {
        let m = state.lock().await;
        m.settings.clone()
    };
    let Ok(client) = engine::build_client(&settings) else {
        return;
    };
    let probed = tokio::time::timeout(Duration::from_secs(10), engine::probe(&client, &url)).await;
    let Ok(info) = probed else {
        return;
    };
    let mut m = state.lock().await;
    m.update_pending_probe(&id, &info);
    if let Some(app) = &m.app {
        if let Some(p) = m.pending.iter().find(|p| p.id == id) {
            let _ = app.emit("pending-download", p.clone());
        }
    }
}

async fn queue_loop(state: AppState) {
    // Queue ticks run every 500 ms; the browser queue file is polled
    // and the tray tooltip refreshed every ~2 s (every 4th tick).
    let mut tick: u64 = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        tick += 1;
        let (new_pendings, app, outcomes, schedule_stop) = {
            let mut m = state.lock().await;
            let mut pendings = Vec::new();
            if tick % 4 == 0 {
                let (_added, p) = m.drain_queue_file().await;
                pendings = p;
                update_tray_tooltip(&m);
            }
            hide_minimised_window(&m);
            // Daily schedule, ~every 30 s (every 60th tick), before
            // the queue tick so a Stop prevents starting in the same
            // tick. Pausing runs after the lock is released.
            let schedule_stop = if tick % 60 == 0 {
                m.schedule_tick(chrono::Local::now()) == engine::ScheduleAction::Stop
            } else {
                false
            };
            m.queue_tick().await;
            // Desktop notifications ride this existing tick: snapshot
            // every task's status, let the pure notifier decide, and
            // send afterwards, outside the manager lock (the D-Bus
            // call must never hold it).
            let outcomes = {
                let enabled = m.settings.desktop_notifications;
                let unfocused = m
                    .app
                    .as_ref()
                    .map(notify::window_unfocused)
                    .unwrap_or(false);
                let snaps: Vec<notify::TaskSnapshot> = m
                    .tasks
                    .values()
                    .filter_map(|h| {
                        h.inner.try_lock().ok().map(|g| notify::TaskSnapshot {
                            id: g.id.clone(),
                            filename: g.filename.clone(),
                            total: g.total,
                            status: g.status.clone(),
                            error: g.error.clone(),
                            expected_hash: g.expected_hash.as_ref().map(|e| e.hex.clone()),
                            computed_hash: g.computed_hash.clone(),
                        })
                    })
                    .collect();
                m.notifier.decide(&snaps, enabled, unfocused)
            };
            (pendings, m.app.clone(), outcomes, schedule_stop)
        };
        if schedule_stop {
            // The window ended: pause running downloads (never
            // cancel — they resume when the window opens again).
            pause_all_tasks_scheduled(&state).await;
        }
        if let Some(app) = &app {
            for outcome in &outcomes {
                if let Err(e) = notify::send(app, outcome) {
                    // No daemon on the session bus, for example:
                    // log and keep running, never crash over it.
                    eprintln!("[zdm] desktop notification failed: {e}");
                }
            }
        }
        if !new_pendings.is_empty() {
            if let Some(app) = &app {
                show_main_window(app);
                if let Some(first) = new_pendings.first() {
                    let _ = app.emit("pending-download", first);
                }
            }
            for p in new_pendings {
                let state2 = state.clone();
                tauri::async_runtime::spawn(async move {
                    probe_pending(state2, p.id, p.url).await;
                });
            }
        }
    }
}

// ------------------------------------------------------------ tray

fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

fn main_window_visible(app: &AppHandle) -> bool {
    app.get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false)
}

fn toggle_main_window(app: &AppHandle) {
    if main_window_visible(app) {
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.hide();
        }
    } else {
        show_main_window(app);
    }
}

fn build_tray_menu<R: tauri::Runtime>(app: &AppHandle<R>, queue_running: bool) -> tauri::Result<Menu<R>> {
    // "Show / Hide ZDM" is one static label: the menu closes on click
    // anyway, and rebuilding it on every toggle would need the menu
    // item handle kept in app state.
    let show = MenuItemBuilder::new("Show / Hide ZDM")
        .id("tray-show")
        .build(app)?;
    let add_url = MenuItemBuilder::new("Add URL...")
        .id("tray-add-url")
        .build(app)?;
    let queue_label = if queue_running {
        "Stop Queue"
    } else {
        "Start Queue"
    };
    let queue = MenuItemBuilder::new(queue_label)
        .id("tray-toggle-queue")
        .build(app)?;
    let pause_all = MenuItemBuilder::new("Pause All")
        .id("tray-pause-all")
        .build(app)?;
    let quit = MenuItemBuilder::new("Quit").id("tray-quit").build(app)?;
    let sep = PredefinedMenuItem::separator(app)?;
    Menu::with_items(app, &[&show, &add_url, &queue, &pause_all, &sep, &quit])
}

fn refresh_tray_menu(app: &AppHandle, queue_running: bool) {
    if let Some(tray) = app.tray_by_id("main") {
        if let Ok(menu) = build_tray_menu(app, queue_running) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

/// Create the tray icon. Never panics on failure: the caller logs the
/// error and the app keeps running without a tray.
fn build_tray(app: &AppHandle) -> Result<(), String> {
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png"))
        .map_err(|e| e.to_string())?;
    let menu = build_tray_menu(app, false).map_err(|e| e.to_string())?;
    TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("ZDM — idle")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let id: &str = event.id().as_ref();
            match id {
                "tray-show" => toggle_main_window(app),
                "tray-add-url" => {
                    show_main_window(app);
                    let _ = app.emit("open-add-modal", ());
                }
                "tray-toggle-queue" => {
                    let state: AppState = AppState::clone(&app.state::<AppState>());
                    let app2 = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let running = {
                            let mut m = state.lock().await;
                            m.queue_running = !m.queue_running;
                            m.scheduled_at = None;
                            m.queue_running
                        };
                        if !running {
                            pause_all_tasks(&state).await;
                        }
                        refresh_tray_menu(&app2, running);
                    });
                }
                "tray-pause-all" => {
                    let state: AppState = AppState::clone(&app.state::<AppState>());
                    tauri::async_runtime::spawn(async move {
                        pause_all_tasks(&state).await;
                    });
                }
                "tray-quit" => app.exit(0),
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button,
                button_state,
                ..
            } = event
            {
                if button == MouseButton::Left && button_state == MouseButtonState::Up {
                    toggle_main_window(tray.app_handle());
                }
            }
        })
        .build(app)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_CTR: AtomicU64 = AtomicU64::new(0);

    fn test_settings(dir: &std::path::Path) -> Settings {
        Settings {
            download_dir: dir.to_string_lossy().to_string(),
            ..Settings::default()
        }
    }

    /// Unique temp dir per test — the drain tests share the process
    /// and run in parallel, so a fixed dir would race on queue.json.
    fn unique_dir() -> std::path::PathBuf {
        let n = TEST_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("zdm-queue-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn queue_file_parses_and_dedupes() {
        let urls = parse_queue_file(
            r#"["https://a.example/x.bin", "https://a.example/x.bin", "  ", "https://b.example/y.bin"]"#,
        );
        assert_eq!(
            urls,
            vec![
                "https://a.example/x.bin".to_string(),
                "https://b.example/y.bin".to_string()
            ]
        );
        assert!(parse_queue_file("not json").is_empty());
        assert!(parse_queue_file("[]").is_empty());
        assert!(parse_queue_file("").is_empty());
    }

    #[tokio::test]
    async fn drain_queue_file_adds_and_dedupes() {
        // Ask OFF: the old auto-start behaviour.
        let dir = unique_dir();
        let mut settings = test_settings(&dir);
        settings.ask_before_download = false;
        let mut m = Manager::new(settings);
        std::fs::write(
            dir.join("queue.json"),
            r#"["https://example.com/a.bin", "https://example.com/b.bin"]"#,
        )
        .unwrap();
        let (added, pendings) = m.drain_queue_file().await;
        assert_eq!(added.len(), 2);
        assert!(pendings.is_empty());
        assert!(m.queue_running);
        assert_eq!(
            std::fs::read_to_string(dir.join("queue.json")).unwrap(),
            "[]"
        );
        // Second drain with the same URL adds nothing (dedupe by URL).
        std::fs::write(dir.join("queue.json"), r#"["https://example.com/a.bin"]"#).unwrap();
        let (added, _) = m.drain_queue_file().await;
        assert!(added.is_empty());
        // Invalid URLs are dropped, valid ones still added.
        std::fs::write(
            dir.join("queue.json"),
            r#"["not-a-url", "https://example.com/c.bin"]"#,
        )
        .unwrap();
        let (added, _) = m.drain_queue_file().await;
        assert_eq!(added.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn drain_queue_file_creates_pending_when_ask_on() {
        // Default settings: ask_before_download ON.
        let dir = unique_dir();
        let mut m = Manager::new(test_settings(&dir));
        assert!(m.settings.ask_before_download);
        std::fs::write(
            dir.join("queue.json"),
            r#"["https://example.com/a.bin", "https://example.com/b.bin"]"#,
        )
        .unwrap();
        let (added, pendings) = m.drain_queue_file().await;
        assert!(added.is_empty());
        assert_eq!(pendings.len(), 2);
        assert_eq!(m.list_pending().len(), 2);
        // Pending arrivals must NOT auto-start the queue: nothing
        // downloads until the user confirms in the popup.
        assert!(!m.queue_running);
        assert_eq!(
            std::fs::read_to_string(dir.join("queue.json")).unwrap(),
            "[]"
        );
        // Same URL again: deduped against the pending list.
        std::fs::write(dir.join("queue.json"), r#"["https://example.com/a.bin"]"#).unwrap();
        let (added, pendings) = m.drain_queue_file().await;
        assert!(added.is_empty() && pendings.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn drain_pending_confirm_creates_queued_task() {
        let dir = unique_dir();
        let mut m = Manager::new(test_settings(&dir));
        std::fs::write(dir.join("queue.json"), r#"["https://example.com/x.bin"]"#).unwrap();
        let (_, pendings) = m.drain_queue_file().await;
        assert_eq!(pendings.len(), 1);
        let opts = ConfirmOptions {
            filename: "x.bin".to_string(),
            dest_dir: pendings[0].dest_dir.clone(),
            category: "Programs".to_string(),
            connections: 2,
            start_now: false,
            expected_sha256: None,
        };
        let handle = m.confirm_pending(&pendings[0].id, opts).await.unwrap();
        assert_eq!(handle.inner.lock().await.status, "queued");
        assert!(m.list_pending().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

pub fn run() {
    let settings = load_settings();
    let autostart_enabled = settings.autostart;
    let manager = Manager::new(settings);
    let state: AppState = Arc::new(AsyncMutex::new(manager));

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(state.clone())
        .setup(move |app| {
            let handle: AppHandle = app.handle().clone();
            // Clicking a notification shows the main window. The
            // handler runs on a background thread, so it only
            // touches the window through the cloned handle. It must
            // be registered at startup: notifications shown before
            // a handler exists report nothing.
            {
                let handle2 = handle.clone();
                if let Err(e) = handle.notification().on_action(move |action| {
                    if action.action_id() == "tap" {
                        show_main_window(&handle2);
                    }
                }) {
                    eprintln!("[zdm] notification click handler unavailable: {e}");
                }
            }
            // Tray failure (e.g. no tray host in the session) must
            // never take the app down: log and continue tray-less.
            if let Err(e) = build_tray(&handle) {
                eprintln!("[zdm] system tray unavailable, continuing without it: {e}");
            }
            // Autostart + app menu: keep the installed .desktop files
            // in line with the setting (best-effort at startup; the
            // same sync runs when the setting is saved).
            if let Err(e) = autostart::sync_autostart(autostart_enabled) {
                eprintln!("[zdm] autostart sync failed: {e}");
            }
            // Hidden start (`zdm --minimized`, used by the autostart
            // entry): no window, tray only. Clicking the tray still
            // shows/hides the window as usual.
            if autostart::is_minimized_start(std::env::args()) {
                eprintln!("[zdm] starting minimized to tray");
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.hide();
                }
            }
            let state2 = state.clone();
            tauri::async_runtime::spawn(async move {
                // Give the manager its AppHandle for progress events,
                // then run the queue loop forever.
                {
                    let mut m = state2.lock().await;
                    m.app = Some(handle);
                }
                // Auto-resume: interrupted and queued downloads
                // continue from their saved offsets right away (the
                // queue loop's 500 ms tick starts them within the
                // concurrent slots) — no waiting for the ~30 s
                // scheduler tick, and never outside an enabled daily
                // window or when the user turned it off.
                {
                    let mut m = state2.lock().await;
                    m.startup_resume(chrono::Local::now());
                }
                // Update check, once a day: runs in its own task so a
                // dead source never delays the queue loop.
                tauri::async_runtime::spawn(maybe_auto_check(state2.clone()));
                queue_loop(state2).await;
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Settings are re-read from disk: save_settings persists
                // immediately, so the file always holds the current value.
                if load_settings().close_to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            add_download,
            verify_checksum,
            scan_download,
            get_clamav_status,
            get_ffmpeg_status,
            check_for_updates,
            get_update_status,
            dismiss_update,
            pause_download,
            resume_download,
            cancel_download,
            delete_download,
            list_downloads,
            get_stats,
            get_settings,
            save_settings,
            add_batch,
            grab_links,
            start_queue,
            stop_queue,
            schedule_queue,
            get_schedule_status,
            list_pending,
            confirm_pending,
            dismiss_pending,
            pause_all
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
