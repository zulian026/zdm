//! ClamAV scan of finished downloads.
//!
//! After a download completes (checksum verified, or no hash source
//! at all — the scan does not depend on the checksum), the finished
//! file is scanned with `clamscan --no-summary <file>` on a blocking
//! thread, in a detached task: a big file takes a while to scan and
//! the queue must never wait for it. clamscan's exit codes are the
//! contract: 0 = clean, 1 = virus found, 2 = error. The threat name
//! comes from the output line (`<path>: <Threat.Name> FOUND`).
//!
//! The outcome mirrors the checksum-mismatch policy exactly:
//! - FOUND marks the download **failed** with "Virus terdeteksi:
//!   <threat>" — the file is **kept** on disk (never deleted
//!   silently); a desktop notification says "Virus terdeteksi";
//!   Retry re-downloads and re-scans.
//! - Clean is recorded as `clean` on the download row.
//! - ClamAV missing (no `clamscan` on PATH) or any other scan error
//!   is "unavailable", never a failure: a missing scanner must never
//!   fail a download.
//!
//! A manual re-scan (`scan_download`) flips a virus-failed download
//! back to completed when the file on disk is clean again — the same
//! flip-back the checksum re-verify has.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Scan outcome for one download. Serialized lowercase so the
/// frontend and the DB both read the same words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanState {
    #[default]
    Unavailable,
    /// Transient: the detached scan is still running. Never written
    /// to the DB (a restart must not restore a stale "scanning").
    Scanning,
    Clean,
    Found,
}

impl ScanState {
    pub fn from_str(s: &str) -> Self {
        match s {
            "scanning" => ScanState::Scanning,
            "clean" => ScanState::Clean,
            "found" => ScanState::Found,
            _ => ScanState::Unavailable,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ScanState::Unavailable => "unavailable",
            ScanState::Scanning => "scanning",
            ScanState::Clean => "clean",
            ScanState::Found => "found",
        }
    }
}

/// The result of one scan: the state plus the threat name when a
/// virus was found (`Eicar-Test-Signature`, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanResult {
    pub state: ScanState,
    pub threat: Option<String>,
}

impl ScanResult {
    pub fn unavailable() -> Self {
        Self {
            state: ScanState::Unavailable,
            threat: None,
        }
    }

    pub fn clean() -> Self {
        Self {
            state: ScanState::Clean,
            threat: None,
        }
    }

    pub fn found(threat: String) -> Self {
        Self {
            state: ScanState::Found,
            threat: Some(threat),
        }
    }
}

/// Whether a scanner is available, for the Settings row: the UI must
/// say "ClamAV tidak terpasang" honestly when there is none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScannerStatus {
    pub available: bool,
    /// e.g. "ClamAV 1.4.2/28234/…". None when unavailable.
    pub version: Option<String>,
}

/// The standard EICAR test file (68 bytes): harmless by design, but
/// every antivirus flags it. Used by the tests and the laptop e2e.
pub const EICAR: &str = "X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*";

/// Resolve the scanner binary: the `ZDM_CLAMSCAN` environment
/// variable (a test seam — integration tests point it at a fake
/// `clamscan` script) wins, otherwise `clamscan` from PATH.
fn scanner_program() -> PathBuf {
    std::env::var_os("ZDM_CLAMSCAN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("clamscan"))
}

/// Parse one clamscan run: exit code 0 = clean, 1 = found (threat
/// name from the `FOUND` line), anything else = unavailable. The
/// output line looks like `/path/file: Eicar-Test-Signature FOUND`
/// or `/path/file: OK`.
pub fn parse_scan_output(code: Option<i32>, stdout: &str) -> ScanResult {
    match code {
        Some(0) => ScanResult::clean(),
        Some(1) => {
            let threat = stdout.lines().find_map(|line| {
                let (_, rest) = line.split_once(':')?;
                let rest = rest.trim();
                rest.strip_suffix(" FOUND").map(|t| t.trim().to_string())
            });
            match threat {
                Some(t) => ScanResult::found(t),
                // Exit 1 without a parseable FOUND line: a virus was
                // found, the name just did not survive parsing.
                None => ScanResult::found("unknown threat".to_string()),
            }
        }
        _ => ScanResult::unavailable(),
    }
}

/// Scan one file with the resolved scanner binary. Blocking — call
/// from `spawn_blocking`, never on the async runtime. A missing
/// binary, a spawn failure, or any clamscan error is "unavailable".
pub fn scan_file(path: &Path) -> ScanResult {
    scan_file_with(path, &scanner_program())
}

/// Scan one file with an explicit scanner binary (tests use a fake
/// `clamscan` script; the real-binary test passes `"clamscan"`).
pub fn scan_file_with(path: &Path, scanner: &Path) -> ScanResult {
    let output = match Command::new(scanner)
        .arg("--no-summary")
        .arg(path)
        .output()
    {
        Ok(o) => o,
        Err(_) => return ScanResult::unavailable(),
    };
    parse_scan_output(output.status.code(), &String::from_utf8_lossy(&output.stdout))
}

/// Probe the scanner for the Settings row: `clamscan --version`
/// prints e.g. `ClamAV 1.4.2/28234/Tue Nov  4 06:10:22 2026`.
pub fn scanner_status() -> ScannerStatus {
    let program = scanner_program();
    let Ok(output) = Command::new(&program).arg("--version").output() else {
        return ScannerStatus {
            available: false,
            version: None,
        };
    };
    if !output.status.success() {
        return ScannerStatus {
            available: false,
            version: None,
        };
    }
    let version = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());
    ScannerStatus {
        available: version.is_some(),
        version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const EICAR_LINE: &str = "/tmp/eicar.com: Eicar-Test-Signature FOUND";
    const CLEAN_LINE: &str = "/tmp/clean.bin: OK";

    #[test]
    fn parse_clean_found_and_errors() {
        assert_eq!(parse_scan_output(Some(0), CLEAN_LINE), ScanResult::clean());
        assert_eq!(
            parse_scan_output(Some(1), EICAR_LINE),
            ScanResult::found("Eicar-Test-Signature".to_string())
        );
        // Exit 1 with no parseable FOUND line still counts as found.
        assert_eq!(
            parse_scan_output(Some(1), "garbage\n"),
            ScanResult::found("unknown threat".to_string())
        );
        // Exit 2 (bad database, unreadable file, …) is unavailable.
        assert_eq!(
            parse_scan_output(Some(2), "ERROR: Can't open file\n"),
            ScanResult::unavailable()
        );
        // A killed process (no code) is unavailable, not found.
        assert_eq!(parse_scan_output(None, EICAR_LINE), ScanResult::unavailable());
        // A FOUND line with exit 0 cannot happen from clamscan; the
        // exit code wins.
        assert_eq!(parse_scan_output(Some(0), EICAR_LINE), ScanResult::clean());
    }

    #[test]
    fn scan_state_round_trip() {
        assert_eq!(ScanState::from_str("clean"), ScanState::Clean);
        assert_eq!(ScanState::from_str("found"), ScanState::Found);
        assert_eq!(ScanState::from_str("scanning"), ScanState::Scanning);
        assert_eq!(ScanState::from_str("anything-else"), ScanState::Unavailable);
        assert_eq!(ScanState::Found.as_str(), "found");
        assert_eq!(ScanState::default(), ScanState::Unavailable);
    }

    /// Write a fake `clamscan` script: exit 1 + FOUND when the file
    /// contains the EICAR string, exit 0 + OK otherwise, exit 2 when
    /// asked (via the FAKE_CLAMSCAN_FAIL env var) to simulate a
    /// broken scanner. The real binary is never needed for the
    /// spawn + parse path.
    fn fake_clamscan(dir: &Path) -> PathBuf {
        let path = dir.join("clamscan");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "#!/bin/sh\n\
             if [ \"$FAKE_CLAMSCAN_FAIL\" = \"1\" ]; then echo 'ERROR: fake scanner broken' >&2; exit 2; fi\n\
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
        path
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zdm-scan-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_file_with_fake_binary() {
        let dir = tmp_dir("fake");
        let fake = fake_clamscan(&dir);
        let clean = dir.join("clean.bin");
        std::fs::write(&clean, b"nothing suspicious here").unwrap();
        // Retry the spawn a few times: under parallel test load a
        // single process spawn can fail transiently (EAGAIN), and a
        // retry distinguishes that from a real logic failure — a
        // genuinely broken scanner fails every attempt.
        let mut got = ScanResult::unavailable();
        for _ in 0..5 {
            got = scan_file_with(&clean, &fake);
            if got == ScanResult::clean() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if got != ScanResult::clean() {
            // Diagnose the flake: re-run the same spawn by hand and
            // report the raw exit code + output instead of guessing.
            let retry = std::process::Command::new(&fake)
                .arg("--no-summary")
                .arg(&clean)
                .output();
            panic!(
                "scan failed: got={got:?} script_len={:?} retry={retry:?}",
                std::fs::metadata(&fake).map(|m| m.len()).ok(),
            );
        }
        let eicar = dir.join("eicar.com");
        std::fs::write(&eicar, EICAR).unwrap();
        // Same retry as the clean scan above: a transient spawn
        // failure under parallel load must not flake the test.
        let mut got = ScanResult::unavailable();
        for _ in 0..5 {
            got = scan_file_with(&eicar, &fake);
            if got == ScanResult::found("Eicar-Test-Signature".to_string()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(
            got,
            ScanResult::found("Eicar-Test-Signature".to_string())
        );
        // A missing binary is unavailable, never an error.
        assert_eq!(
            scan_file_with(&clean, &dir.join("no-such-binary")),
            ScanResult::unavailable()
        );
        // A scanner that exits 2 is unavailable too.
        std::env::set_var("FAKE_CLAMSCAN_FAIL", "1");
        assert_eq!(scan_file_with(&clean, &fake), ScanResult::unavailable());
        std::env::remove_var("FAKE_CLAMSCAN_FAIL");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scanner_status_with_fake_binary() {
        let dir = tmp_dir("status");
        let fake = fake_clamscan(&dir);
        // scanner_status() resolves through ZDM_CLAMSCAN.
        std::env::set_var("ZDM_CLAMSCAN", &fake);
        // Retry like the scan tests: a transient spawn failure under
        // parallel load must not flake the test.
        let mut st = scanner_status();
        for _ in 0..5 {
            if st.available {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            st = scanner_status();
        }
        assert!(st.available);
        assert_eq!(st.version.as_deref(), Some("ClamAV 9.9.9/1/Fake"));
        std::env::remove_var("ZDM_CLAMSCAN");
        // A missing binary reports unavailable.
        std::env::set_var("ZDM_CLAMSCAN", dir.join("no-such-binary"));
        let st = scanner_status();
        assert!(!st.available);
        assert!(st.version.is_none());
        std::env::remove_var("ZDM_CLAMSCAN");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_file_real_clamscan_eicar() {
        // Real-binary proof — runs only where a working clamscan with
        // a database exists. Without one (no binary, or no signature
        // DB so even a clean file does not scan), the test skips
        // instead of failing: the fake-binary tests above cover the
        // spawn + parse path on every machine.
        let dir = tmp_dir("real");
        let clean = dir.join("clean.bin");
        std::fs::write(&clean, b"nothing suspicious here").unwrap();
        if scan_file_with(&clean, Path::new("clamscan")) != ScanResult::clean() {
            eprintln!("real clamscan unavailable or has no database — skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let eicar = dir.join("eicar.com");
        std::fs::write(&eicar, EICAR).unwrap();
        assert_eq!(
            scan_file_with(&eicar, Path::new("clamscan")),
            ScanResult::found("Eicar-Test-Signature".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
