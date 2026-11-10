//! Desktop notifications for terminal download states.
//!
//! How it works:
//! - The queue loop in `lib.rs` snapshots every task's status each
//!   tick (500 ms, the existing tick — no new timer, no new thread)
//!   and hands it to [`Notifier::decide`], a pure function: no I/O,
//!   no D-Bus. It reports which notifications to send, if any.
//! - One notification per download, deduped by (id, terminal status).
//!   A transition the user watched in the app (window focused) is
//!   recorded as handled and never fires later.
//! - A "queue drained" summary fires only when the last active
//!   download finished and at least 2 downloads completed while the
//!   window was unfocused. A single file needs no summary — its own
//!   notification already said so.
//! - Nothing is sent while the main window is focused: if the user
//!   is watching ZDM, the list itself is the notification.
//! - Sending goes through the official `tauri-plugin-notification`
//!   (notify-rust over D-Bus on Linux). Clicking a notification
//!   shows and focuses the main window (handler registered in
//!   `lib.rs` at startup). A missing notification daemon is an error
//!   the caller logs — the app never crashes over it.

use std::collections::HashSet;
use tauri::{AppHandle, Manager as _};
use tauri_plugin_notification::NotificationExt;

/// The bits of a task the notifier needs: a snapshot, not a lock.
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub id: String,
    pub filename: String,
    pub total: Option<u64>,
    pub status: String,
    pub error: Option<String>,
    /// Expected / computed hash hex, when the download carries a
    /// checksum: a mismatch gets its own notification title.
    pub expected_hash: Option<String>,
    pub computed_hash: Option<String>,
}

/// One notification to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyOutcome {
    pub title: String,
    pub body: String,
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

/// Human size: "3.6 GB", "573.4 KB", "73 B". Unknown total stays
/// honest instead of inventing a number.
pub fn fmt_bytes(total: Option<u64>) -> String {
    let Some(bytes) = total else {
        return "unknown size".to_string();
    };
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.1} GB", b / GB)
    }
}

fn short_error(error: Option<&str>) -> String {
    let Some(e) = error.map(str::trim).filter(|e| !e.is_empty()) else {
        return "unknown reason".to_string();
    };
    let one_line: String = e.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    if one_line.chars().count() > 80 {
        format!("{}…", one_line.chars().take(79).collect::<String>())
    } else {
        one_line
    }
}

pub struct Notifier {
    /// (download id, terminal status) already handled this run.
    notified: HashSet<(String, String)>,
    /// Completions in the current batch (since the queue last went
    /// active) that happened while the window was unfocused.
    completed_in_batch: u32,
    /// Whether the queue was active on the previous decision.
    was_active: bool,
    /// Whether the summary for the current batch was sent.
    summary_sent: bool,
    /// First call only records terminal states that already exist
    /// (downloads restored from the DB) without notifying for them.
    primed: bool,
}

impl Notifier {
    pub fn new() -> Self {
        Self {
            notified: HashSet::new(),
            completed_in_batch: 0,
            was_active: false,
            summary_sent: false,
            primed: false,
        }
    }

    /// Decide which notifications to send for this tick. Pure apart
    /// from the state it keeps: no I/O, no D-Bus, no window access.
    pub fn decide(
        &mut self,
        tasks: &[TaskSnapshot],
        enabled: bool,
        window_unfocused: bool,
    ) -> Vec<NotifyOutcome> {
        let active = tasks
            .iter()
            .any(|t| t.status == "downloading" || t.status == "queued");
        if !self.primed {
            // Restored downloads must not notify for work done in a
            // previous session. Record them, send nothing.
            self.primed = true;
            self.was_active = active;
            for t in tasks {
                if is_terminal(&t.status) {
                    self.notified.insert((t.id.clone(), t.status.clone()));
                }
            }
            return Vec::new();
        }
        if !enabled {
            self.was_active = active;
            return Vec::new();
        }
        if active && !self.was_active {
            // A new batch started: the previous batch's completions
            // no longer count toward a summary.
            self.completed_in_batch = 0;
            self.summary_sent = false;
        }
        self.was_active = active;

        let mut out = Vec::new();
        for t in tasks {
            if !is_terminal(&t.status) {
                continue;
            }
            if !self.notified.insert((t.id.clone(), t.status.clone())) {
                continue; // already handled: never notify twice
            }
            if !window_unfocused {
                // The user watched this finish in the app: handled,
                // not sent, and not counted toward a summary.
                continue;
            }
            match t.status.as_str() {
                "completed" => {
                    self.completed_in_batch += 1;
                    out.push(NotifyOutcome {
                        title: "Download selesai".to_string(),
                        body: format!("{} ({})", t.filename, fmt_bytes(t.total)),
                    });
                }
                "failed" => {
                    // A virus is its own notification: the file
                    // downloaded fine, it is just dangerous — that
                    // deserves a different title than a network
                    // failure or a checksum mismatch.
                    let virus = t
                        .error
                        .as_deref()
                        .is_some_and(|e| e.starts_with("Virus terdeteksi"));
                    if virus {
                        out.push(NotifyOutcome {
                            title: "Virus terdeteksi".to_string(),
                            body: format!("{} — {}", t.filename, short_error(t.error.as_deref())),
                        });
                    } else if t
                        .error
                        .as_deref()
                        .is_some_and(|e| e.starts_with("checksum mismatch"))
                    {
                        // A checksum mismatch is its own notification:
                        // the file downloaded fine, it just cannot be
                        // trusted — that deserves a different title than
                        // a network failure.
                        let exp = t
                            .expected_hash
                            .as_deref()
                            .map(crate::checksum::short_hash)
                            .unwrap_or_default();
                        let got = t
                            .computed_hash
                            .as_deref()
                            .map(crate::checksum::short_hash)
                            .unwrap_or_default();
                        out.push(NotifyOutcome {
                            title: "Checksum tidak cocok".to_string(),
                            body: format!("{} — expected {exp}… got {got}…", t.filename),
                        });
                    } else {
                        out.push(NotifyOutcome {
                            title: "Download gagal".to_string(),
                            body: format!("{} — {}", t.filename, short_error(t.error.as_deref())),
                        });
                    }
                }
                _ => {} // cancelled: silent
            }
        }
        if !active && !self.summary_sent && self.completed_in_batch >= 2 {
            out.push(NotifyOutcome {
                title: "Semua download selesai".to_string(),
                body: format!("{} download selesai", self.completed_in_batch),
            });
            self.summary_sent = true;
            self.completed_in_batch = 0;
        }
        out
    }
}

/// True when a notification may be shown: the main window is not
/// the focused window, is minimised, or is hidden (tray).
pub fn window_unfocused(app: &AppHandle) -> bool {
    let Some(win) = app.get_webview_window("main") else {
        return false;
    };
    let focused = win.is_focused().unwrap_or(false);
    let minimized = win.is_minimized().unwrap_or(false);
    let visible = win.is_visible().unwrap_or(true);
    !focused || minimized || !visible
}

/// The ZDM icon as a `file://` URI. First the bundle resource
/// (AppImage/.deb); the standalone binary has no resource dir, so
/// fall back to a cached copy of the icon embedded in the binary
/// (`~/.cache/zdm/icon.png`, written once). The freedesktop
/// notification spec takes a theme icon name or a `file://` URI —
/// without a real path the daemon falls back to the executable name
/// and shows a generic icon.
fn icon_file_uri(app: &AppHandle) -> Option<String> {
    if let Ok(dir) = app.path().resource_dir() {
        let path = dir.join("icons").join("icon.png");
        if path.is_file() {
            return Some(format!("file://{}", path.display()));
        }
    }
    cached_icon_uri()
}

fn cached_icon_uri() -> Option<String> {
    const ICON: &[u8] = include_bytes!("../icons/icon.png");
    let base = std::env::var("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".cache"))
                .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"))
        });
    let path = base.join("zdm").join("icon.png");
    let needs_write = std::fs::metadata(&path)
        .map(|m| m.len() != ICON.len() as u64)
        .unwrap_or(true);
    if needs_write {
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, ICON).ok()?;
    }
    Some(format!("file://{}", path.display()))
}

/// Send one notification. Returns an error (e.g. no notification
/// daemon on the session bus) instead of panicking; the caller logs
/// it and the app keeps running.
pub fn send(app: &AppHandle, outcome: &NotifyOutcome) -> Result<(), String> {
    let mut builder = app
        .notification()
        .builder()
        .title(&outcome.title)
        .body(&outcome.body);
    if let Some(icon) = icon_file_uri(app) {
        builder = builder.icon(icon);
    }
    builder.show().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, status: &str) -> TaskSnapshot {
        TaskSnapshot {
            id: id.to_string(),
            filename: format!("{id}.bin"),
            total: Some(1_048_576),
            status: status.to_string(),
            error: None,
            expected_hash: None,
            computed_hash: None,
        }
    }

    #[test]
    fn completed_transition_fires_once() {
        let mut n = Notifier::new();
        // Prime with nothing running.
        assert!(n.decide(&[], true, true).is_empty());
        let tasks = vec![snap("a", "downloading")];
        assert!(n.decide(&tasks, true, true).is_empty());
        let tasks = vec![snap("a", "completed")];
        let out = n.decide(&tasks, true, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Download selesai");
        assert_eq!(out[0].body, "a.bin (1.0 MB)");
        // Same state again: no duplicate.
        assert!(n.decide(&tasks, true, true).is_empty());
    }

    #[test]
    fn single_completion_gets_no_summary() {
        let mut n = Notifier::new();
        assert!(n.decide(&[], true, true).is_empty());
        let out = n.decide(&[snap("a", "completed")], true, true);
        assert_eq!(out.len(), 1); // per-file only
        assert!(out.iter().all(|o| o.title != "Semua download selesai"));
    }

    #[test]
    fn two_completions_get_summary_after_drain() {
        let mut n = Notifier::new();
        assert!(n.decide(&[], true, true).is_empty());
        // Batch of two running, both finish while unfocused.
        let running = vec![snap("a", "downloading"), snap("b", "downloading")];
        assert!(n.decide(&running, true, true).is_empty());
        let done = vec![snap("a", "completed"), snap("b", "completed")];
        let out = n.decide(&done, true, true);
        assert_eq!(out.len(), 3); // two per-file + one summary
        assert_eq!(out[2].title, "Semua download selesai");
        assert_eq!(out[2].body, "2 download selesai");
        // Summary fires once.
        assert!(n.decide(&done, true, true).is_empty());
    }

    #[test]
    fn checksum_mismatch_gets_its_own_title() {
        let mut n = Notifier::new();
        assert!(n.decide(&[], true, true).is_empty());
        let mut bad = snap("a", "failed");
        bad.error = Some("checksum mismatch (expected ba7816bf8f01…, got 900150983cd2…)".to_string());
        bad.expected_hash = Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string());
        bad.computed_hash = Some("900150983cd24fb0d6963f7d28e17f72".to_string());
        let out = n.decide(&[bad], true, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Checksum tidak cocok");
        assert!(out[0].body.contains("expected ba7816bf8f01…"));
        assert!(out[0].body.contains("got 900150983cd2…"));
    }

    #[test]
    fn virus_detection_gets_its_own_title() {
        let mut n = Notifier::new();
        assert!(n.decide(&[], true, true).is_empty());
        let mut bad = snap("a", "failed");
        bad.error = Some("Virus terdeteksi: Eicar-Test-Signature".to_string());
        let out = n.decide(&[bad], true, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Virus terdeteksi");
        assert!(out[0].body.contains("Eicar-Test-Signature"));
    }

    #[test]
    fn failed_fires_with_reason_and_cancelled_is_silent() {
        let mut n = Notifier::new();
        assert!(n.decide(&[], true, true).is_empty());
        let mut bad = snap("a", "failed");
        bad.error = Some("connection reset by peer".to_string());
        let out = n.decide(&[bad, snap("b", "cancelled")], true, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Download gagal");
        assert!(out[0].body.contains("connection reset by peer"));
    }

    #[test]
    fn focused_window_suppresses_and_never_refires() {
        let mut n = Notifier::new();
        assert!(n.decide(&[], true, true).is_empty());
        let tasks = vec![snap("a", "completed")];
        // User watched it finish: nothing sent.
        assert!(n.decide(&tasks, true, false).is_empty());
        // They minimise later: still nothing — the moment passed.
        assert!(n.decide(&tasks, true, true).is_empty());
    }

    #[test]
    fn disabled_sends_nothing() {
        let mut n = Notifier::new();
        assert!(n.decide(&[], true, true).is_empty());
        assert!(n
            .decide(&[snap("a", "completed"), snap("b", "failed")], false, true)
            .is_empty());
    }

    #[test]
    fn priming_skips_downloads_restored_from_db() {
        let mut n = Notifier::new();
        // First tick already sees completed downloads: restored
        // from a previous session, not new work.
        let tasks = vec![snap("a", "completed"), snap("b", "failed")];
        assert!(n.decide(&tasks, true, true).is_empty());
        // And they never fire on later ticks either.
        assert!(n.decide(&tasks, true, true).is_empty());
    }

    #[test]
    fn new_batch_can_summarize_again() {
        let mut n = Notifier::new();
        assert!(n.decide(&[], true, true).is_empty());
        let done1 = vec![snap("a", "completed"), snap("b", "completed")];
        let out = n.decide(&done1, true, true);
        assert_eq!(out.len(), 3);
        // A new batch starts and drains with two more completions.
        let running = vec![snap("c", "downloading"), snap("d", "downloading")];
        assert!(n.decide(&running, true, true).is_empty());
        let done2 = vec![snap("c", "completed"), snap("d", "completed")];
        let out = n.decide(&done2, true, true);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].body, "2 download selesai");
    }

    #[test]
    fn cached_icon_uri_returns_a_real_file() {
        let uri = cached_icon_uri().expect("cache dir should be writable");
        assert!(uri.starts_with("file://"));
        assert!(uri.ends_with("zdm/icon.png"));
        let path = uri.trim_start_matches("file://");
        assert_eq!(
            std::fs::metadata(path).unwrap().len(),
            include_bytes!("../icons/icon.png").len() as u64
        );
    }

    #[test]
    fn fmt_bytes_cases() {
        assert_eq!(fmt_bytes(None), "unknown size");
        assert_eq!(fmt_bytes(Some(73)), "73 B");
        assert_eq!(fmt_bytes(Some(573_400)), "560.0 KB");
        assert_eq!(fmt_bytes(Some(3_800_000_000)), "3.5 GB");
    }
}
