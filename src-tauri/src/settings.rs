//! Settings persisted at `~/.config/zdm/settings.json` (same path as the
//! Python ZDM, so the two apps share user preferences).

use crate::schedule::DailySchedule;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_CONNECTIONS: u32 = 8;
pub const MAX_CONNECTIONS: u32 = 32;
pub const DEFAULT_MAX_CONCURRENT: u32 = 3;

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub connections: u32,
    pub max_concurrent: u32,
    pub download_dir: String,
    pub speed_limit_kbps: f64,
    /// Optional explicit proxy URL, e.g. `http://proxy:8080`.
    /// Empty means "use HTTP_PROXY/HTTPS_PROXY from the environment".
    pub proxy: String,
    /// Browser-intercepted downloads wait for confirmation in the
    /// popup instead of auto-starting. Manual Add URL is unaffected.
    /// `serde(default)` keeps old settings.json files (written before
    /// this field existed) loading with the default of ON.
    #[serde(default = "default_true")]
    pub ask_before_download: bool,
    /// A minimised window is hidden to the tray (detected by polling:
    /// Tauri 2 has no minimise window event on Linux).
    #[serde(default = "default_true")]
    pub minimize_to_tray: bool,
    /// Closing the window hides it to the tray instead of quitting.
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    /// Desktop notifications for finished / failed downloads.
    /// They are only sent when the main window is not focused (see
    /// `notify::window_unfocused`); watching the app is notification
    /// enough. `serde(default)` keeps old settings.json files loading
    /// with the default of ON.
    #[serde(default = "default_true")]
    pub desktop_notifications: bool,
    /// Start ZDM at login, hidden in the system tray (see
    /// `autostart.rs`: installs `~/.config/autostart/zdm.desktop`
    /// with `--minimized`, plus the app-menu entry). `serde(default)`
    /// keeps old settings.json files loading with autostart OFF.
    #[serde(default)]
    pub autostart: bool,
    /// Resume interrupted and queued downloads automatically when
    /// ZDM opens, from the saved offsets — never from zero. A
    /// manually paused download stays paused; a download the daily
    /// scheduler paused only resumes inside its window. `serde
    /// (default)` keeps old settings.json files loading with the
    /// default of ON.
    #[serde(default = "default_true")]
    pub resume_on_startup: bool,
    /// Scan finished downloads with ClamAV (`clamscan`). A virus
    /// marks the download failed (file kept); a missing scanner never
    /// fails a download. `serde(default)` keeps old settings.json
    /// files loading with the default of ON.
    #[serde(default = "default_true")]
    pub clamav_scan: bool,
    /// URL of the remote update manifest (JSON, see `update.rs` and
    /// `updates/version.json`). Empty disables update checks: the
    /// checker reports "no source configured" and never offers an
    /// update. `serde(default)` keeps old settings.json files
    /// loading with checks disabled.
    #[serde(default)]
    pub update_manifest_url: String,
    /// Daily recurring queue window (see `schedule.rs`). `serde
    /// (default)` keeps old settings.json files loading with the
    /// schedule disabled.
    #[serde(default)]
    pub schedule: DailySchedule,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            connections: DEFAULT_CONNECTIONS,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            download_dir: default_download_dir(),
            speed_limit_kbps: 0.0,
            proxy: String::new(),
            ask_before_download: true,
            minimize_to_tray: true,
            close_to_tray: true,
            desktop_notifications: true,
            autostart: false,
            resume_on_startup: true,
            clamav_scan: true,
            update_manifest_url: String::new(),
            schedule: DailySchedule::default(),
        }
    }
}

impl Settings {
    pub fn normalized(mut self) -> Self {
        self.connections = self.connections.clamp(1, MAX_CONNECTIONS);
        self.max_concurrent = self.max_concurrent.max(1);
        if self.speed_limit_kbps < 0.0 {
            self.speed_limit_kbps = 0.0;
        }
        if self.download_dir.trim().is_empty() {
            self.download_dir = default_download_dir();
        }
        self.update_manifest_url = self.update_manifest_url.trim().to_string();
        // A malformed "HH:MM" (hand-edited file) falls back to the
        // defaults instead of breaking the scheduler.
        if DailySchedule::parse_time(&self.schedule.start).is_none() {
            self.schedule.start = crate::schedule::DEFAULT_START.to_string();
        }
        if DailySchedule::parse_time(&self.schedule.stop).is_none() {
            self.schedule.stop = crate::schedule::DEFAULT_STOP.to_string();
        }
        self
    }
}

pub fn default_download_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{home}/Downloads/ZDM")
}

pub fn settings_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config").join("zdm").join("settings.json")
}

pub fn load_settings() -> Settings {
    let path = settings_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Settings>(&text)
            .map(|s| s.normalized())
            .unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

pub fn save_settings(settings: &Settings) -> Result<(), String> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_normalize_clamps_connections() {
        let s = Settings {
            connections: 99,
            max_concurrent: 0,
            speed_limit_kbps: -5.0,
            ..Settings::default()
        }
        .normalized();
        assert_eq!(s.connections, MAX_CONNECTIONS);
        assert_eq!(s.max_concurrent, 1);
        assert_eq!(s.speed_limit_kbps, 0.0);
    }

    #[test]
    fn settings_default_values() {
        let s = Settings::default();
        assert_eq!(s.connections, 8);
        assert_eq!(s.max_concurrent, 3);
        assert_eq!(s.speed_limit_kbps, 0.0);
        assert!(s.download_dir.ends_with("Downloads/ZDM"));
        assert!(s.ask_before_download);
        assert!(s.minimize_to_tray);
        assert!(s.close_to_tray);
        assert!(s.desktop_notifications);
        assert!(!s.autostart);
        assert!(s.resume_on_startup);
        assert!(s.clamav_scan);
        assert!(s.update_manifest_url.is_empty());
        assert!(!s.schedule.enabled);
        assert_eq!(s.schedule.start, "23:00");
        assert_eq!(s.schedule.stop, "06:00");
        assert_eq!(s.schedule.days, [true; 7]);
    }

    #[test]
    fn settings_schedule_bad_times_normalized() {
        let s = Settings {
            schedule: DailySchedule {
                enabled: true,
                start: "25:99".to_string(),
                stop: String::new(),
                days: [true; 7],
            },
            ..Settings::default()
        }
        .normalized();
        assert_eq!(s.schedule.start, "23:00");
        assert_eq!(s.schedule.stop, "06:00");
        assert!(s.schedule.enabled);
    }

    #[test]
    fn settings_old_file_without_new_fields_defaults_on() {
        // settings.json written before ask/minimize/close existed.
        let s: Settings = serde_json::from_str(
            r#"{"connections":8,"max_concurrent":3,"download_dir":"/tmp/x","speed_limit_kbps":0,"proxy":""}"#,
        )
        .unwrap();
        assert!(s.ask_before_download);
        assert!(s.minimize_to_tray);
        assert!(s.close_to_tray);
        assert!(s.desktop_notifications);
        // And an explicit false survives a round-trip.
        let s2: Settings = serde_json::from_str(
            r#"{"connections":8,"max_concurrent":3,"download_dir":"/tmp/x","speed_limit_kbps":0,"proxy":"","ask_before_download":false,"minimize_to_tray":false,"close_to_tray":false,"desktop_notifications":false}"#,
        )
        .unwrap();
        assert!(!s2.ask_before_download);
        assert!(!s2.minimize_to_tray);
        assert!(!s2.close_to_tray);
        assert!(!s2.desktop_notifications);
        assert!(!s2.autostart);
        // resume_on_startup also defaults ON in an old file, and an
        // explicit false survives a round-trip.
        assert!(s.resume_on_startup);
        let s3: Settings = serde_json::from_str(
            r#"{"connections":8,"max_concurrent":3,"download_dir":"/tmp/x","speed_limit_kbps":0,"proxy":"","resume_on_startup":false}"#,
        )
        .unwrap();
        assert!(!s3.resume_on_startup);
        // clamav_scan also defaults ON in an old file, and an
        // explicit false survives a round-trip.
        assert!(s.clamav_scan);
        // update_manifest_url defaults to empty (checks disabled)
        // in an old file.
        assert!(s.update_manifest_url.is_empty());
        let s4: Settings = serde_json::from_str(
            r#"{"connections":8,"max_concurrent":3,"download_dir":"/tmp/x","speed_limit_kbps":0,"proxy":"","clamav_scan":false}"#,
        )
        .unwrap();
        assert!(!s4.clamav_scan);
    }
}
