//! Automatic checksum verification.
//!
//! A download can carry an expected hash from four sources, in
//! priority order: the user (an "Expected SHA-256" field in the Add
//! URL modal or the confirmation popup), the response `Digest` /
//! `Content-Digest` header (RFC 3230 / RFC 9530), the `Content-MD5`
//! header, or a `<url>.sha256` / `<url>.md5` sidecar file fetched
//! with a plain GET. The hash itself is always computed streaming
//! (64 KiB chunks) — a multi-gigabyte file is never loaded whole.
//!
//! Everything in this module is pure (no I/O except `hash_file` and
//! `fetch_sidecar_hash`) and unit tested. A download with no hash
//! from any source is "unavailable", never a failure: a missing
//! checksum must never fail a download.

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgo {
    Sha256,
    Md5,
}

impl HashAlgo {
    pub fn hex_len(self) -> usize {
        match self {
            HashAlgo::Sha256 => 64,
            HashAlgo::Md5 => 32,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HashAlgo::Sha256 => "SHA-256",
            HashAlgo::Md5 => "MD5",
        }
    }
}

/// The expected hash for one download, with where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedHash {
    pub algo: HashAlgo,
    /// Lowercase hex.
    pub hex: String,
    /// "user" | "digest" | "content-md5" | "sidecar".
    pub source: String,
}

/// Verification outcome for one download. Serialized lowercase so
/// the frontend and the DB both read the same words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifyState {
    #[default]
    Unavailable,
    Verified,
    Mismatch,
}

impl VerifyState {
    pub fn from_str(s: &str) -> Self {
        match s {
            "verified" => VerifyState::Verified,
            "mismatch" => VerifyState::Mismatch,
            _ => VerifyState::Unavailable,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            VerifyState::Unavailable => "unavailable",
            VerifyState::Verified => "verified",
            VerifyState::Mismatch => "mismatch",
        }
    }
}

/// First 12 hex characters, for display (full hash in the tooltip).
pub fn short_hash(hex: &str) -> String {
    hex.chars().take(12).collect()
}

/// Validate and normalise a hex hash: trim, lowercase, right length
/// and hex-only for the algorithm. Rejects anything else — a typo in
/// the user's field must fail loudly at entry time, not silently
/// pass every download.
pub fn normalize_hex(s: &str, algo: HashAlgo) -> Option<String> {
    let hex: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let hex = hex.to_lowercase();
    if hex.len() != algo.hex_len() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(hex)
}

/// SHA-256 only (the user field): 64 hex characters.
pub fn normalize_sha256(s: &str) -> Option<String> {
    normalize_hex(s, HashAlgo::Sha256)
}

fn b64_to_hex(b64: &str, algo: HashAlgo) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    if bytes.len() != algo.hex_len() / 2 {
        return None;
    }
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Parse a `Digest` / `Content-Digest` header (RFC 3230 / RFC 9530):
/// comma-separated `algo=base64` pairs, e.g.
/// `SHA-256=47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=, MD5=...`.
/// SHA-256 wins over MD5 when both are present. Algorithm names are
/// matched case-insensitively (`SHA-256`, `sha256`, `SHA256`).
pub fn parse_digest_header(value: &str) -> Option<ExpectedHash> {
    let mut md5: Option<ExpectedHash> = None;
    for part in value.split(',') {
        let (k, v) = part.split_once('=')?;
        let k = k.trim().to_ascii_lowercase().replace('-', "");
        let v = v.trim().trim_matches('"');
        match k.as_str() {
            "sha256" => {
                let hex = b64_to_hex(v, HashAlgo::Sha256)?;
                return Some(ExpectedHash {
                    algo: HashAlgo::Sha256,
                    hex,
                    source: "digest".to_string(),
                });
            }
            "md5" => {
                if md5.is_none() {
                    if let Some(hex) = b64_to_hex(v, HashAlgo::Md5) {
                        md5 = Some(ExpectedHash {
                            algo: HashAlgo::Md5,
                            hex,
                            source: "digest".to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    md5
}

/// Parse a `Content-MD5` header: one base64 MD5.
pub fn parse_content_md5(value: &str) -> Option<ExpectedHash> {
    let hex = b64_to_hex(value, HashAlgo::Md5)?;
    Some(ExpectedHash {
        algo: HashAlgo::Md5,
        hex,
        source: "content-md5".to_string(),
    })
}

/// Parse a `.sha256` / `.md5` sidecar body. Two formats:
/// bare hash (`<hex>`), or checksum-file lines (`<hash>  <filename>`,
/// one or two spaces). With several lines, the one whose filename
/// matches ours wins; otherwise the first valid line. Comment lines
/// (`# ...`) are skipped.
pub fn parse_sidecar(text: &str, filename: &str) -> Option<ExpectedHash> {
    let mut first: Option<ExpectedHash> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let name = parts.next();
        let found = if hash.len() == 64 {
            normalize_hex(hash, HashAlgo::Sha256).map(|hex| ExpectedHash {
                algo: HashAlgo::Sha256,
                hex,
                source: "sidecar".to_string(),
            })
        } else if hash.len() == 32 {
            normalize_hex(hash, HashAlgo::Md5).map(|hex| ExpectedHash {
                algo: HashAlgo::Md5,
                hex,
                source: "sidecar".to_string(),
            })
        } else {
            None
        };
        let Some(found) = found else {
            continue;
        };
        if let Some(name) = name {
            // Match on the bare filename, with or without a leading
            // `*` (binary-mode marker) or a path prefix.
            let base = name.rsplit('/').next().unwrap_or(name).trim_start_matches('*');
            if base == filename {
                return Some(found);
            }
        }
        if first.is_none() {
            first = Some(found);
        }
    }
    first
}

/// Stream a file through the hasher, 64 KiB at a time. Never loads
/// the file into memory.
pub fn hash_file(path: &Path, algo: HashAlgo) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut buf = [0u8; 64 * 1024];
    match algo {
        HashAlgo::Sha256 => {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            loop {
                let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Ok(format!("{:x}", h.finalize()))
        }
        HashAlgo::Md5 => {
            // md5 0.7 has no Digest-trait hasher: Context::consume +
            // compute is its streaming API.
            let mut h = md5::Context::new();
            loop {
                let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                h.consume(&buf[..n]);
            }
            Ok(format!("{:x}", h.compute()))
        }
    }
}

/// `<url>.sha256` / `<url>.md5`: append the extension to the URL path
/// (query and fragment dropped — they belong to the download, not to
/// the sidecar).
pub fn sidecar_url(url: &str, ext: &str) -> Option<String> {
    let mut parsed = url::Url::parse(url).ok()?;
    parsed.set_query(None);
    parsed.set_fragment(None);
    let path = format!("{}{}", parsed.path(), ext);
    parsed.set_path(&path);
    Some(parsed.to_string())
}

/// Best-effort sidecar fetch: try `.sha256`, then `.md5`, each under
/// one shared 5 s timeout. A missing sidecar (404), a timeout, or an
/// unparsable body all mean "no hash" — never an error.
pub async fn fetch_sidecar_hash(
    client: &reqwest::Client,
    url: &str,
    filename: &str,
) -> Option<ExpectedHash> {
    let filename = filename.to_string();
    let u256 = sidecar_url(url, ".sha256");
    let u5 = sidecar_url(url, ".md5");
    let attempt = async {
        for side in [u256, u5] {
            let Some(side) = side else {
                continue;
            };
            let Ok(resp) = client.get(&side).send().await else {
                continue;
            };
            if !resp.status().is_success() {
                continue;
            }
            let Ok(text) = resp.text().await else {
                continue;
            };
            if let Some(found) = parse_sidecar(&text, &filename) {
                return Some(found);
            }
        }
        None
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), attempt)
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    // sha256("abc") / md5("abc") — the standard test vectors.
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const ABC_MD5: &str = "900150983cd24fb0d6963f7d28e17f72";

    fn b64_of_hex(hex: &str) -> String {
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn digest_header_sha256_wins() {
        let v = format!("MD5={}, SHA-256={}", b64_of_hex(ABC_MD5), b64_of_hex(ABC_SHA256));
        let got = parse_digest_header(&v).unwrap();
        assert_eq!(got.algo, HashAlgo::Sha256);
        assert_eq!(got.hex, ABC_SHA256);
        assert_eq!(got.source, "digest");
    }

    #[test]
    fn digest_header_md5_only_and_bad_values() {
        let v = format!("MD5={}", b64_of_hex(ABC_MD5));
        let got = parse_digest_header(&v).unwrap();
        assert_eq!(got.algo, HashAlgo::Md5);
        assert_eq!(got.hex, ABC_MD5);
        // Wrong-length base64 is not a hash.
        assert!(parse_digest_header("SHA-256=AAAA").is_none());
        assert!(parse_digest_header("CRC32=AAAA").is_none());
        assert!(parse_digest_header("").is_none());
        // Quoted values are accepted.
        let v = format!("SHA-256=\"{}\"", b64_of_hex(ABC_SHA256));
        assert_eq!(parse_digest_header(&v).unwrap().hex, ABC_SHA256);
    }

    #[test]
    fn content_md5_parses() {
        let got = parse_content_md5(&b64_of_hex(ABC_MD5)).unwrap();
        assert_eq!(got.algo, HashAlgo::Md5);
        assert_eq!(got.hex, ABC_MD5);
        assert_eq!(got.source, "content-md5");
        assert!(parse_content_md5("not-base64!!").is_none());
    }

    #[test]
    fn sidecar_bare_and_checksum_lines() {
        // Bare hash.
        let got = parse_sidecar(ABC_SHA256, "f.bin").unwrap();
        assert_eq!(got.algo, HashAlgo::Sha256);
        assert_eq!(got.source, "sidecar");
        // "<hash>  <filename>" with two spaces.
        let got = parse_sidecar(&format!("{ABC_SHA256}  f.bin\n"), "f.bin").unwrap();
        assert_eq!(got.hex, ABC_SHA256);
        // Several lines: the matching filename wins over the first.
        let other = "00".repeat(32);
        let text = format!("{other}  other.bin\n{ABC_MD5}  f.bin\n");
        let got = parse_sidecar(&text, "f.bin").unwrap();
        assert_eq!(got.algo, HashAlgo::Md5);
        assert_eq!(got.hex, ABC_MD5);
        // No filename match: first valid line.
        let got = parse_sidecar(&text, "missing.bin").unwrap();
        assert_eq!(got.hex, other);
        // Comments and garbage are skipped; nothing valid -> None.
        assert!(parse_sidecar("# a comment\nzzzz\n", "f.bin").is_none());
        assert!(parse_sidecar("", "f.bin").is_none());
    }

    #[test]
    fn normalize_rejects_bad_hashes() {
        assert_eq!(normalize_sha256(ABC_SHA256), Some(ABC_SHA256.to_string()));
        // Uppercase is accepted and lowercased.
        assert_eq!(
            normalize_sha256(&ABC_SHA256.to_uppercase()),
            Some(ABC_SHA256.to_string())
        );
        assert!(normalize_sha256(&ABC_SHA256[..63]).is_none());
        assert!(normalize_sha256(&format!("{ABC_SHA256}00")).is_none());
        assert!(normalize_sha256("zz7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").is_none());
        assert!(normalize_sha256("").is_none());
    }

    #[test]
    fn hash_file_streams_known_vectors() {
        let dir = std::env::temp_dir().join("zdm-checksum-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("abc.bin");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(hash_file(&path, HashAlgo::Sha256).unwrap(), ABC_SHA256);
        assert_eq!(hash_file(&path, HashAlgo::Md5).unwrap(), ABC_MD5);
        assert!(hash_file(&dir.join("no-such-file"), HashAlgo::Sha256).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sidecar_url_appends_to_path() {
        assert_eq!(
            sidecar_url("https://example.com/dir/f.bin?x=1#frag", ".sha256").as_deref(),
            Some("https://example.com/dir/f.bin.sha256")
        );
        assert!(sidecar_url("not a url", ".sha256").is_none());
    }

    #[test]
    fn verify_state_round_trip() {
        assert_eq!(VerifyState::from_str("verified"), VerifyState::Verified);
        assert_eq!(VerifyState::from_str("mismatch"), VerifyState::Mismatch);
        assert_eq!(VerifyState::from_str("anything-else"), VerifyState::Unavailable);
        assert_eq!(VerifyState::Verified.as_str(), "verified");
        assert_eq!(short_hash(ABC_SHA256), "ba7816bf8f01");
    }
}
