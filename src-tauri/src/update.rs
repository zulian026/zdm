//! Update checker: ZDM checks a configurable remote manifest JSON
//! for a newer version and offers it — never a fake update.
//!
//! There is no built-in release channel (no GitHub repo, no account
//! was created for this): the manifest URL comes from the setting
//! `update_manifest_url` and is empty by default. An empty URL is a
//! first-class state (`NoSource`), not an error: the UI says so
//! honestly and no check runs.
//!
//! The manifest format (see `updates/version.json` for an example
//! and `updates/make-manifest.py` for a generator):
//! ```json
//! {
//!   "version": "0.2.0",
//!   "download_url": "https://example.com/zdm/releases/0.2.0/ZDM_0.2.0_amd64.AppImage",
//!   "sha256": "<64 hex characters of the asset>",
//!   "changelog": "What changed in this release."
//! }
//! ```
//!
//! States — a fetch failure, a malformed manifest, or a version
//! that is not newer never produces an offer:
//! - `NoSource` — no manifest URL configured.
//! - `UpToDate` — the manifest version is the running version or older.
//! - `UpdateAvailable` — the manifest version is newer.
//! - `Unreachable` — the source could not be fetched (network, DNS,
//!   non-2xx status). Never interpreted as "an update exists".
//! - `InvalidManifest` — the fetched document is not a valid
//!   manifest (bad JSON, unparseable version, non-http(s) URL,
//!   malformed sha256).
//!
//! The last check (and the dismissed version) persist in
//! `~/.config/zdm/update_state.json`, separate from settings.json:
//! they are check state, not user configuration, and the Settings
//! form must never be able to wipe them by saving without them.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::PathBuf;

/// The running version (Cargo package version; tauri.conf.json
/// carries the same number).
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a manifest fetch may take before it counts as
/// unreachable. A dead source must never hang the startup check.
pub const FETCH_TIMEOUT_SECS: u64 = 15;

/// A semantic version: major.minor.patch plus an optional
/// pre-release list. Build metadata (`+…`) is parsed and ignored —
/// it never affects precedence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<String>,
}

/// Parse a version string. A leading `v` is accepted (`v0.2.0`);
/// anything else must be strict `major.minor.patch` with numeric
/// core components and dot-separated pre-release identifiers.
pub fn parse_version(raw: &str) -> Option<SemVer> {
    let s = raw.trim().strip_prefix('v').unwrap_or(raw.trim());
    let (core_pre, _) = match s.split_once('+') {
        Some((a, _)) => (a, ()),
        None => (s, ()),
    };
    let (core, pre) = match core_pre.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (core_pre, None),
    };
    let mut parts = core.split('.');
    let (major, minor, patch) = (
        parts.next()?.parse::<u64>().ok()?,
        parts.next()?.parse::<u64>().ok()?,
        parts.next()?.parse::<u64>().ok()?,
    );
    if parts.next().is_some() {
        return None;
    }
    let mut pre_list = Vec::new();
    if let Some(pre) = pre {
        // A trailing `-` with nothing after it is not a pre-release.
        if pre.is_empty() {
            return None;
        }
        for ident in pre.split('.') {
            if ident.is_empty()
                || !ident
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                return None;
            }
            pre_list.push(ident.to_string());
        }
    }
    Some(SemVer {
        major,
        minor,
        patch,
        pre: pre_list,
    })
}

/// Semantic-version precedence: core numbers first; a release
/// (no pre-release) outranks any pre-release of the same core;
/// pre-release identifiers compare dot by dot — numeric identifiers
/// numerically and below alphanumeric ones, a shorter list below a
/// longer one when it is a prefix of it.
pub fn compare_versions(a: &SemVer, b: &SemVer) -> Ordering {
    let core = (a.major, a.minor, a.patch).cmp(&(b.major, b.minor, b.patch));
    if core != Ordering::Equal {
        return core;
    }
    match (a.pre.is_empty(), b.pre.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            for (x, y) in a.pre.iter().zip(b.pre.iter()) {
                let ord = compare_pre_ident(x, y);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            a.pre.len().cmp(&b.pre.len())
        }
    }
}

fn compare_pre_ident(a: &str, b: &str) -> Ordering {
    let (an, bn) = (a.parse::<u64>(), b.parse::<u64>());
    match (an, bn) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

/// One release, as published in the manifest. `sha256` is the hex
/// hash of the asset at `download_url` — the Download button passes
/// it to the engine as the expected hash, so a tampered asset fails
/// exactly like a checksum mismatch (file kept, never installed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateManifest {
    pub version: String,
    pub download_url: String,
    pub sha256: Option<String>,
    pub changelog: Option<String>,
}

/// The outcome of one check. Serialized lowercase so the frontend
/// reads the same words as the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    #[default]
    NoSource,
    UpToDate,
    UpdateAvailable,
    Unreachable,
    InvalidManifest,
}

impl UpdateState {
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateState::NoSource => "no_source",
            UpdateState::UpToDate => "up_to_date",
            UpdateState::UpdateAvailable => "update_available",
            UpdateState::Unreachable => "unreachable",
            UpdateState::InvalidManifest => "invalid_manifest",
        }
    }
}

/// The full result of one check, persisted so the offer survives a
/// restart without re-fetching. `source_url` records where it came
/// from: a stored check is only valid for the URL it was fetched
/// from, so changing the setting invalidates the old offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCheck {
    pub state: UpdateState,
    pub current_version: String,
    pub manifest: Option<UpdateManifest>,
    pub source_url: String,
    pub checked_at: String,
    pub message: String,
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Parse and validate a manifest document. Unknown extra fields are
/// ignored; a missing or invalid required field rejects the whole
/// document — a malformed manifest must never produce an offer.
pub fn parse_manifest(text: &str) -> Result<UpdateManifest, String> {
    let m: UpdateManifest =
        serde_json::from_str(text).map_err(|e| format!("invalid manifest JSON: {e}"))?;
    parse_version(&m.version).ok_or_else(|| format!("invalid version in manifest: {}", m.version))?;
    let url = url::Url::parse(&m.download_url)
        .map_err(|e| format!("invalid download_url in manifest: {e}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("download_url in manifest must be http(s)".to_string());
    }
    if let Some(sha) = &m.sha256 {
        if !is_hex64(sha) {
            return Err("sha256 in manifest must be 64 hex characters".to_string());
        }
    }
    Ok(m)
}

/// Pure decision: is the manifest version newer than the running one?
pub fn evaluate(manifest: &UpdateManifest, current: &str) -> UpdateState {
    let (Some(mv), Some(cv)) = (parse_version(&manifest.version), parse_version(current)) else {
        return UpdateState::InvalidManifest;
    };
    if compare_versions(&mv, &cv) == Ordering::Greater {
        UpdateState::UpdateAvailable
    } else {
        UpdateState::UpToDate
    }
}

fn message_for(state: UpdateState, current: &str, manifest: Option<&UpdateManifest>) -> String {
    match state {
        UpdateState::NoSource => "No update source configured.".to_string(),
        UpdateState::UpToDate => format!("ZDM {current} is up to date."),
        UpdateState::UpdateAvailable => format!(
            "ZDM {} is available.",
            manifest.map(|m| m.version.as_str()).unwrap_or("?")
        ),
        UpdateState::Unreachable => "Could not reach the update source.".to_string(),
        UpdateState::InvalidManifest => "The update manifest is invalid.".to_string(),
    }
}

/// Fetch the manifest at `url` and decide. Any fetch failure is
/// `Unreachable`, never an offer; any parse failure is
/// `InvalidManifest`, never an offer.
pub async fn check_manifest(
    client: &reqwest::Client,
    url: &str,
    current: &str,
) -> UpdateCheck {
    let now = Local::now().to_rfc3339();
    let mk = |state: UpdateState, manifest: Option<UpdateManifest>| UpdateCheck {
        message: message_for(state, current, manifest.as_ref()),
        state,
        current_version: current.to_string(),
        manifest,
        source_url: url.to_string(),
        checked_at: now.clone(),
    };
    let resp = match client
        .get(url)
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return mk(UpdateState::Unreachable, None),
    };
    if !resp.status().is_success() {
        return mk(UpdateState::Unreachable, None);
    }
    let text = match resp.text().await {
        Ok(t) => t,
        Err(_) => return mk(UpdateState::Unreachable, None),
    };
    let manifest = match parse_manifest(&text) {
        Ok(m) => m,
        Err(_) => return mk(UpdateState::InvalidManifest, None),
    };
    let state = evaluate(&manifest, current);
    mk(state, Some(manifest))
}

/// The once-a-day rule: check again only when the last check was
/// not today (local date). A missing timestamp always checks.
pub fn should_auto_check(last_checked: Option<&str>, now: DateTime<Local>) -> bool {
    let Some(last) = last_checked else {
        return true;
    };
    let today = now.format("%Y-%m-%d").to_string();
    !last.starts_with(&today)
}

// ------------------------------------------------------------ state file

/// Durable check state: when the last check ran, which version the
/// user dismissed, and the last check result (so the offer survives
/// a restart). Kept out of settings.json on purpose — see the
/// module docs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateStateFile {
    pub last_checked: Option<String>,
    pub dismissed_version: Option<String>,
    pub last_check: Option<UpdateCheck>,
}

pub fn update_state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("zdm")
        .join("update_state.json")
}

pub fn load_update_state() -> UpdateStateFile {
    match std::fs::read_to_string(update_state_path()) {
        Ok(text) => serde_json::from_str::<UpdateStateFile>(&text).unwrap_or_default(),
        Err(_) => UpdateStateFile::default(),
    }
}

pub fn save_update_state(state: &UpdateStateFile) -> Result<(), String> {
    let path = update_state_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    fn v(s: &str) -> SemVer {
        parse_version(s).unwrap()
    }

    #[test]
    fn version_parse_and_compare() {
        assert_eq!(parse_version("0.1.0"), Some(v("0.1.0")));
        assert_eq!(parse_version("v0.2.0"), Some(v("0.2.0")));
        assert_eq!(parse_version("1.0.0-beta.1+build.5"), Some(v("1.0.0-beta.1")));
        assert_eq!(parse_version("0.2"), None);
        assert_eq!(parse_version("0.2.0.1"), None);
        assert_eq!(parse_version("a.b.c"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("1.0.0-"), None);
        assert_eq!(parse_version("1.0.0-beta..1"), None);

        // Core numbers decide first.
        assert_eq!(compare_versions(&v("0.2.0"), &v("0.1.0")), Greater);
        assert_eq!(compare_versions(&v("0.1.0"), &v("0.1.0")), Equal);
        assert_eq!(compare_versions(&v("0.1.9"), &v("0.2.0")), Less);
        assert_eq!(compare_versions(&v("1.0.0"), &v("0.99.99")), Greater);
        // A release outranks any pre-release of the same core.
        assert_eq!(compare_versions(&v("1.0.0"), &v("1.0.0-beta.1")), Greater);
        assert_eq!(compare_versions(&v("1.0.0-beta.1"), &v("1.0.0")), Less);
        // Pre-release identifiers: numeric < alphanumeric, numeric
        // compares numerically, shorter list loses on a prefix tie.
        assert_eq!(compare_versions(&v("1.0.0-alpha"), &v("1.0.0-alpha.1")), Less);
        assert_eq!(compare_versions(&v("1.0.0-alpha.1"), &v("1.0.0-alpha.beta")), Less);
        assert_eq!(compare_versions(&v("1.0.0-alpha.beta"), &v("1.0.0-beta")), Less);
        assert_eq!(compare_versions(&v("1.0.0-beta.2"), &v("1.0.0-beta.11")), Less);
        assert_eq!(compare_versions(&v("1.0.0-rc.1"), &v("1.0.0")), Less);
        // Build metadata never affects precedence.
        assert_eq!(
            compare_versions(&v("1.0.0+build.1"), &v("1.0.0+build.2")),
            Equal
        );
    }

    const GOOD_SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn manifest_json(version: &str) -> String {
        format!(
            r#"{{"version":"{version}","download_url":"https://example.com/zdm-0.2.0.AppImage","sha256":"{GOOD_SHA}","changelog":"Faster."}}"#
        )
    }

    #[test]
    fn parse_manifest_valid_and_invalid() {
        let m = parse_manifest(&manifest_json("0.2.0")).unwrap();
        assert_eq!(m.version, "0.2.0");
        assert_eq!(m.sha256.as_deref(), Some(GOOD_SHA));
        assert_eq!(m.changelog.as_deref(), Some("Faster."));
        // Optional fields may be absent.
        let m = parse_manifest(
            r#"{"version":"0.2.0","download_url":"https://example.com/z.AppImage"}"#,
        )
        .unwrap();
        assert!(m.sha256.is_none() && m.changelog.is_none());
        // Unknown extra fields are ignored.
        let m = parse_manifest(
            r#"{"version":"0.2.0","download_url":"https://example.com/z.AppImage","extra":true}"#,
        )
        .unwrap();
        assert_eq!(m.version, "0.2.0");
        // Every invalid shape rejects the whole document.
        assert!(parse_manifest("not json").is_err());
        assert!(parse_manifest(r#"{"download_url":"https://example.com/z"}"#).is_err());
        assert!(parse_manifest(r#"{"version":"0.2","download_url":"https://example.com/z"}"#)
            .is_err());
        assert!(parse_manifest(
            r#"{"version":"0.2.0","download_url":"ftp://example.com/z"}"#
        )
        .is_err());
        assert!(parse_manifest(
            r#"{"version":"0.2.0","download_url":"https://example.com/z","sha256":"xyz"}"#
        )
        .is_err());
        assert!(parse_manifest(
            r#"{"version":"0.2.0","download_url":"https://example.com/z","sha256":"0123"}"#
        )
        .is_err());
    }

    #[test]
    fn evaluate_newer_same_and_older() {
        let newer = parse_manifest(&manifest_json("0.2.0")).unwrap();
        assert_eq!(evaluate(&newer, "0.1.0"), UpdateState::UpdateAvailable);
        let same = parse_manifest(&manifest_json("0.1.0")).unwrap();
        assert_eq!(evaluate(&same, "0.1.0"), UpdateState::UpToDate);
        let older = parse_manifest(&manifest_json("0.0.9")).unwrap();
        assert_eq!(evaluate(&older, "0.1.0"), UpdateState::UpToDate);
        // A pre-release of the running version is not newer.
        let pre = parse_manifest(&manifest_json("0.1.0-beta.1")).unwrap();
        assert_eq!(evaluate(&pre, "0.1.0"), UpdateState::UpToDate);
        // But a newer pre-release is an offer.
        let pre_new = parse_manifest(&manifest_json("0.2.0-beta.1")).unwrap();
        assert_eq!(evaluate(&pre_new, "0.1.0"), UpdateState::UpdateAvailable);
    }

    #[test]
    fn should_auto_check_once_a_day() {
        let now = Local::now();
        assert!(should_auto_check(None, now));
        let today = now.to_rfc3339();
        assert!(!should_auto_check(Some(&today), now));
        let yesterday = (now - chrono::Duration::days(1)).to_rfc3339();
        assert!(should_auto_check(Some(&yesterday), now));
        // A garbage timestamp checks again rather than skipping.
        assert!(should_auto_check(Some("garbage"), now));
    }

    #[test]
    fn state_enum_serializes_lowercase_snake() {
        assert_eq!(
            serde_json::to_string(&UpdateState::UpdateAvailable).unwrap(),
            "\"update_available\""
        );
        assert_eq!(
            serde_json::to_string(&UpdateState::NoSource).unwrap(),
            "\"no_source\""
        );
    }
}
