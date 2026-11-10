//! Autostart at login + app-menu entry (freedesktop `.desktop` files).
//!
//! When the `autostart` setting is ON, ZDM installs two files:
//! - `~/.local/share/applications/zdm.desktop` — the app-menu entry,
//!   so ZDM shows up in the desktop's application launcher.
//! - `~/.config/autostart/zdm.desktop` — the autostart entry, whose
//!   Exec carries `--minimized` so login is not interrupted by a
//!   window popping up; ZDM starts hidden in the system tray.
//!
//! When the setting is OFF, the autostart entry is removed; the
//! app-menu entry stays (the user may still want to launch from the
//! menu). `sync_autostart` is idempotent and runs both at startup and
//! whenever the setting is saved, so the files always match the
//! setting even if the user deletes one by hand.

use std::path::{Path, PathBuf};

/// Flag that starts ZDM hidden in the tray (no window shown).
pub const MINIMIZED_FLAG: &str = "--minimized";

const DESKTOP_NAME: &str = "zdm.desktop";

/// True when the process was launched with the hidden-start flag.
pub fn is_minimized_start(mut args: impl Iterator<Item = String>) -> bool {
    args.any(|a| a == MINIMIZED_FLAG)
}

pub fn autostart_path(home: &Path) -> PathBuf {
    home.join(".config").join("autostart").join(DESKTOP_NAME)
}

pub fn app_menu_path(home: &Path) -> PathBuf {
    home.join(".local")
        .join("share")
        .join("applications")
        .join(DESKTOP_NAME)
}

/// The icon for the `.desktop` files: the repo's icon next to the
/// release binary first, then the cached copy the notifier writes
/// (`~/.cache/zdm/icon.png`), then the bare theme name `zdm`.
pub fn resolve_icon(home: &Path, exe: &Path) -> PathBuf {
    if let Some(dir) = exe.parent().and_then(|d| d.parent()) {
        let repo_icon = dir.join("src-tauri").join("icons").join("icon.png");
        if repo_icon.exists() {
            return repo_icon;
        }
    }
    let cached = home.join(".cache").join("zdm").join("icon.png");
    if cached.exists() {
        return cached;
    }
    PathBuf::from("zdm")
}

fn desktop_entry(exe: &Path, icon: &Path, minimized: bool) -> String {
    let exec = if minimized {
        format!("{} {MINIMIZED_FLAG}", exe.display())
    } else {
        exe.display().to_string()
    };
    format!(
        "[Desktop Entry]\nType=Application\nName=ZDM\nComment=ZDM Download Manager\nExec={exec}\nIcon={}\nCategories=Network;FileTransfer;\nTerminal=false\nStartupWMClass=zdm\n",
        icon.display()
    )
}

fn write_file(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, contents).map_err(|e| e.to_string())
}

/// Bring the installed `.desktop` files in line with `enabled`,
/// under `home`, for the binary at `exe`. Returns an error string on
/// the first filesystem failure; partial work (one file written, the
/// other not) is possible and the next sync repairs it.
pub fn sync_autostart_in(home: &Path, enabled: bool, exe: &Path) -> Result<(), String> {
    let icon = resolve_icon(home, exe);
    // The app-menu entry is installed on every sync: ZDM should
    // always be launchable from the menu, autostart or not.
    write_file(&app_menu_path(home), &desktop_entry(exe, &icon, false))?;
    if enabled {
        write_file(&autostart_path(home), &desktop_entry(exe, &icon, true))?;
    } else {
        let _ = std::fs::remove_file(autostart_path(home));
    }
    Ok(())
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()).into()
}

/// `sync_autostart_in` against the real home and the running binary.
pub fn sync_autostart(enabled: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    sync_autostart_in(&home_dir(), enabled, &exe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_CTR: AtomicU64 = AtomicU64::new(0);

    fn unique_dir() -> PathBuf {
        let n = TEST_CTR.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("zdm-autostart-test-{}-{}", std::process::id(), n))
    }

    /// Fake home + fake repo layout: `<root>/release/zdm` as the
    /// binary and `<root>/src-tauri/icons/icon.png` as the icon.
    fn fake_repo() -> (PathBuf, PathBuf) {
        let root = unique_dir();
        let exe = root.join("release").join("zdm");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"fake").unwrap();
        let icon = root.join("src-tauri").join("icons").join("icon.png");
        std::fs::create_dir_all(icon.parent().unwrap()).unwrap();
        std::fs::write(&icon, b"fake").unwrap();
        (root, exe)
    }

    #[test]
    fn minimized_flag_parsing() {
        assert!(is_minimized_start(
            ["zdm".to_string(), "--minimized".to_string()].into_iter()
        ));
        assert!(!is_minimized_start(["zdm".to_string()].into_iter()));
        assert!(!is_minimized_start(
            ["zdm".to_string(), "--minimize".to_string()].into_iter()
        ));
    }

    #[test]
    fn desktop_entry_minimized_carries_flag() {
        let exe = Path::new("/home/zyan/workspace/zdm-tauri/release/zdm");
        let icon = Path::new("/home/zyan/workspace/zdm-tauri/src-tauri/icons/icon.png");
        let auto = desktop_entry(exe, icon, true);
        assert!(auto.contains("Exec=/home/zyan/workspace/zdm-tauri/release/zdm --minimized"));
        assert!(auto.contains("Name=ZDM"));
        assert!(auto.contains("StartupWMClass=zdm"));
        let plain = desktop_entry(exe, icon, false);
        assert!(plain.contains("Exec=/home/zyan/workspace/zdm-tauri/release/zdm\n"));
        assert!(!plain.contains("--minimized"));
    }

    #[test]
    fn icon_resolution_prefers_repo_icon_then_cache() {
        let (root, exe) = fake_repo();
        assert_eq!(resolve_icon(&root, &exe), root.join("src-tauri/icons/icon.png"));
        // No repo icon: the cached copy wins.
        std::fs::remove_file(root.join("src-tauri/icons/icon.png")).unwrap();
        let cached = root.join(".cache").join("zdm").join("icon.png");
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"fake").unwrap();
        assert_eq!(resolve_icon(&root, &exe), cached);
        // Neither: bare theme name.
        std::fs::remove_file(&cached).unwrap();
        assert_eq!(resolve_icon(&root, &exe), PathBuf::from("zdm"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_writes_both_then_removes_autostart_only() {
        let (root, exe) = fake_repo();
        sync_autostart_in(&root, true, &exe).unwrap();
        let menu = std::fs::read_to_string(app_menu_path(&root)).unwrap();
        let auto = std::fs::read_to_string(autostart_path(&root)).unwrap();
        assert!(menu.contains("Exec="));
        assert!(!menu.contains("--minimized"));
        assert!(auto.contains("--minimized"));
        assert!(auto.contains(&*exe.display().to_string()));
        assert!(menu.contains(&*root.join("src-tauri/icons/icon.png").display().to_string()));

        sync_autostart_in(&root, false, &exe).unwrap();
        assert!(!autostart_path(&root).exists());
        // The app-menu entry stays: launching from the menu is
        // independent of autostart.
        assert!(app_menu_path(&root).exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
