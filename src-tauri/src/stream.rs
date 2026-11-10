//! HLS (.m3u8) and DASH (.mpd) stream download + merge (grabber v2).
//!
//! Before this module, downloading a manifest saved the playlist file
//! itself. Now a manifest URL downloads every media segment and merges
//! them into one playable video file:
//!
//! - HLS: the master playlist's highest-bandwidth variant is picked,
//!   its media playlist is parsed, segments are downloaded (in
//!   parallel, resumable), then merged. MPEG-TS segments are
//!   concatenated and remuxed to .mp4 when ffmpeg is available, or
//!   kept as .ts when it is not. fMP4 segments (EXT-X-MAP) are
//!   concatenated after the init segment. AES-128 encrypted streams
//!   are decrypted through ffmpeg with a rewritten local playlist
//!   (the key file is downloaded first); without ffmpeg an encrypted
//!   stream is rejected honestly instead of producing garbage.
//! - DASH: the MPD is parsed (SegmentTemplate + SegmentTimeline or a
//!   fixed segment duration), the best video and audio adaptation
//!   sets are downloaded (init segment + media segments), then muxed
//!   with ffmpeg into one .mp4. DASH needs ffmpeg: without it the
//!   download falls back to saving just the playlist, with a note
//!   saying so.
//! - Live streams (HLS without EXT-X-ENDLIST, dynamic MPD) are
//!   rejected with a clear message — only finite VOD playlists are
//!   merged.
//!
//! The segment list lives in `StreamPlan`, persisted in the task
//! sidecar, so a pause / crash / restart resumes segment-by-segment:
//! a segment whose file is already complete on disk is never
//! downloaded again, and the merge step is idempotent.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// HLS or DASH. Serialised lowercase so the frontend and the DB both
/// read the same words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    Hls,
    Dash,
}

impl StreamKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamKind::Hls => "hls",
            StreamKind::Dash => "dash",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "hls" => Some(StreamKind::Hls),
            "dash" => Some(StreamKind::Dash),
            _ => None,
        }
    }
}

/// A manifest URL is a stream when its path ends with .m3u8/.mpd
/// (query strings ignored).
pub fn kind_for_url(url: &str) -> Option<StreamKind> {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_lowercase();
    if path.ends_with(".m3u8") {
        Some(StreamKind::Hls)
    } else if path.ends_with(".mpd") {
        Some(StreamKind::Dash)
    } else {
        None
    }
}

/// A manifest by Content-Type: the HLS and DASH MIME types.
pub fn kind_for_mime(mime: &str) -> Option<StreamKind> {
    let m = mime.split(';').next().unwrap_or("").trim().to_lowercase();
    match m.as_str() {
        "application/vnd.apple.mpegurl" | "application/x-mpegurl" => Some(StreamKind::Hls),
        "application/dash+xml" => Some(StreamKind::Dash),
        _ => None,
    }
}

/// One media segment: its URL, its file name inside the segments
/// directory, its size when known, and whether it is complete on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamSegment {
    pub url: String,
    pub file: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub done: bool,
    /// EXTINF / timeline duration in seconds (0 when unknown).
    #[serde(default)]
    pub duration: f64,
    /// Index into `StreamPlan::keys` when this segment is encrypted.
    #[serde(default)]
    pub key: Option<usize>,
}

/// An AES-128 key for HLS: its URL, its local file name, and the IV
/// exactly as written in the playlist (None = the media sequence
/// number, per RFC 8216).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamKey {
    pub url: String,
    pub file: String,
    #[serde(default)]
    pub iv: Option<String>,
}

/// The whole stream plan, persisted in the task sidecar so a resume
/// never re-downloads a finished segment and never re-plans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamPlan {
    pub kind: StreamKind,
    pub playlist_url: String,
    /// Main track segments (HLS: all segments; DASH: video).
    #[serde(default)]
    pub segments: Vec<StreamSegment>,
    /// HLS EXT-X-MAP init segment, or the DASH video init segment.
    #[serde(default)]
    pub init: Option<StreamSegment>,
    /// DASH audio track segments (HLS has none — audio is muxed in).
    #[serde(default)]
    pub audio_segments: Vec<StreamSegment>,
    /// DASH audio init segment.
    #[serde(default)]
    pub audio_init: Option<StreamSegment>,
    /// AES-128 keys (HLS only).
    #[serde(default)]
    pub keys: Vec<StreamKey>,
    /// Sum of segment durations, seconds.
    #[serde(default)]
    pub total_duration: f64,
    /// "mp4" or "ts" — the merged file's extension.
    pub output_ext: String,
    /// True once the merged file is written at the .part path.
    #[serde(default)]
    pub merged: bool,
}

impl StreamPlan {
    pub fn pending(kind: StreamKind, playlist_url: String, output_ext: &str) -> Self {
        Self {
            kind,
            playlist_url,
            segments: Vec::new(),
            init: None,
            audio_segments: Vec::new(),
            audio_init: None,
            keys: Vec::new(),
            total_duration: 0.0,
            output_ext: output_ext.to_string(),
            merged: false,
        }
    }

    /// Build a plan from a resolved playlist (see `ResolvedPlan`).
    /// Segment file names are index-based so they sort in order and
    /// never collide with each other or with the init/key files.
    pub fn from_resolved(r: ResolvedPlan, output_ext: &str) -> Self {
        let mut plan = Self::pending(r.kind, r.playlist_url, output_ext);
        plan.total_duration = r.total_duration;
        let ext_of = |url: &str, fallback: &str| -> String {
            let path = url.split(['?', '#']).next().unwrap_or(url);
            let ext = path.rsplit('.').next().unwrap_or("");
            if ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric()) && !ext.is_empty()
            {
                ext.to_lowercase()
            } else {
                fallback.to_string()
            }
        };
        // Keys first (deduped by URL — rotation reuses the table).
        let mut key_ids: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for (url, iv) in &r.keys {
            if !key_ids.contains_key(url) {
                let idx = plan.keys.len();
                key_ids.insert(url.clone(), idx);
                plan.keys.push(StreamKey {
                    url: url.clone(),
                    file: format!("key-{idx}.bin"),
                    iv: iv.clone(),
                });
            }
        }
        if let Some(init_url) = r.init {
            let ext = ext_of(&init_url, "mp4");
            plan.init = Some(StreamSegment {
                url: init_url,
                file: format!("init.{ext}"),
                size: None,
                done: false,
                duration: 0.0,
                key: None,
            });
        }
        for (i, seg) in r.segments.iter().enumerate() {
            let ext = ext_of(&seg.url, "ts");
            plan.segments.push(StreamSegment {
                url: seg.url.clone(),
                file: format!("seg-{i:06}.{ext}"),
                size: None,
                done: false,
                duration: seg.duration,
                key: seg
                    .key
                    .as_ref()
                    .and_then(|(url, _)| key_ids.get(url).copied()),
            });
        }
        if let Some(init_url) = r.audio_init {
            let ext = ext_of(&init_url, "mp4");
            plan.audio_init = Some(StreamSegment {
                url: init_url,
                file: format!("a-init.{ext}"),
                size: None,
                done: false,
                duration: 0.0,
                key: None,
            });
        }
        for (i, seg) in r.audio_segments.iter().enumerate() {
            let ext = ext_of(&seg.url, "m4s");
            plan.audio_segments.push(StreamSegment {
                url: seg.url.clone(),
                file: format!("a-{i:06}.{ext}"),
                size: None,
                done: false,
                duration: seg.duration,
                key: None,
            });
        }
        plan
    }

    /// Segments finished / total, counting both tracks.
    pub fn progress(&self) -> (u64, u64) {
        let all = self
            .segments
            .iter()
            .chain(self.audio_segments.iter())
            .chain(self.init.iter())
            .chain(self.audio_init.iter());
        let total = all.clone().count() as u64;
        let done = all.filter(|s| s.done).count() as u64;
        (done, total)
    }

    pub fn encrypted(&self) -> bool {
        !self.keys.is_empty()
    }

    pub fn all_done(&self) -> bool {
        self.segments.iter().all(|s| s.done)
            && self.audio_segments.iter().all(|s| s.done)
            && self.init.as_ref().map(|s| s.done).unwrap_or(true)
            && self.audio_init.as_ref().map(|s| s.done).unwrap_or(true)
    }
}

/// A fetched + parsed playlist, ready to become a `StreamPlan`.
/// Produced by the engine (fetch + parse); `keys` holds every
/// distinct AES-128 key URL (with IV).
pub struct ResolvedSegment {
    pub url: String,
    pub duration: f64,
    pub key: Option<(String, Option<String>)>,
}

pub struct ResolvedPlan {
    pub kind: StreamKind,
    /// Media playlist / MPD URL actually parsed (the chosen variant
    /// for a master playlist).
    pub playlist_url: String,
    pub segments: Vec<ResolvedSegment>,
    pub init: Option<String>,
    pub audio_segments: Vec<ResolvedSegment>,
    pub audio_init: Option<String>,
    pub keys: Vec<(String, Option<String>)>,
    pub total_duration: f64,
    /// HLS EXT-X-MAP present (fMP4 segments — the merged file is
    /// always .mp4, even without ffmpeg).
    pub is_fmp4: bool,
}

/// Directory holding a stream's segment files, next to the
/// destination file: `<dest>.segments/`.
pub fn segments_dir(dest_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.segments", dest_path.display()))
}

/// ffmpeg lookup. `ZDM_FFMPEG` overrides the lookup (empty = pretend
/// ffmpeg is missing — the test seam, like `ZDM_CLAMSCAN`); otherwise
/// the first `ffmpeg` on PATH wins.
pub fn ffmpeg_path() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("ZDM_FFMPEG") {
        if v.is_empty() {
            return None;
        }
        return Some(PathBuf::from(v));
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join("ffmpeg");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

pub fn ffmpeg_available() -> bool {
    ffmpeg_path().is_some()
}

/// First line of `ffmpeg -version` ("ffmpeg version 8.1.2 …").
pub fn ffmpeg_version() -> Option<String> {
    let path = ffmpeg_path()?;
    let out = Command::new(path).arg("-version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|s| s.to_string())
}

/// Merged output extension for a fresh stream: HLS is .mp4 with
/// ffmpeg and .ts without it; DASH is always .mp4 (it needs ffmpeg).
pub fn output_extension(kind: StreamKind, ffmpeg: bool) -> &'static str {
    match kind {
        StreamKind::Hls => {
            if ffmpeg {
                "mp4"
            } else {
                "ts"
            }
        }
        StreamKind::Dash => "mp4",
    }
}

/// Swap a manifest file name for the merged output name: "stream.m3u8"
/// -> "stream.mp4". Any other name is left alone (the user chose it).
pub fn output_filename(filename: &str, ext: &str) -> String {
    let lower = filename.to_lowercase();
    if lower.ends_with(".m3u8") {
        let stem = &filename[..filename.len() - 5];
        format!("{stem}.{ext}")
    } else if lower.ends_with(".mpd") {
        let stem = &filename[..filename.len() - 4];
        format!("{stem}.{ext}")
    } else if lower.ends_with(".mp4") || lower.ends_with(".ts") || lower.ends_with(".mkv") {
        filename.to_string()
    } else if filename.contains('.') {
        filename.to_string()
    } else {
        format!("{filename}.{ext}")
    }
}

fn resolve(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    match url::Url::parse(base).and_then(|b| b.join(rel)) {
        Ok(u) => u.to_string(),
        Err(_) => rel.to_string(),
    }
}

// ---------------------------------------------------------------- HLS

/// One HLS media playlist segment, with the AES-128 key in force
/// for it (URL, IV) when the playlist is encrypted.
#[derive(Debug)]
pub struct HlsSegment {
    pub url: String,
    pub duration: f64,
    pub key: Option<(String, Option<String>)>,
}

/// One HLS media playlist: segments, the EXT-X-MAP init segment,
/// and whether the playlist ends (VOD).
#[derive(Debug)]
pub struct HlsMedia {
    pub segments: Vec<HlsSegment>,
    pub map: Option<String>,
    pub endlist: bool,
}

/// Parse `#EXT-X-STREAM-INF` lines and return the highest-bandwidth
/// variant's URL. `Ok(None)` when the text is already a media
/// playlist (no variants at all).
pub fn parse_hls_master(text: &str, base: &str) -> Result<Option<String>, String> {
    let mut best: Option<(u64, u64, String)> = None; // (bandwidth, resolution px, url)
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.trim();
        let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") else {
            continue;
        };
        let mut bandwidth: u64 = 0;
        let mut pixels: u64 = 0;
        for part in attrs.split(',') {
            let (k, v) = match part.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => continue,
            };
            if k == "BANDWIDTH" {
                bandwidth = v.parse().unwrap_or(0);
            } else if k == "RESOLUTION" {
                if let Some((w, h)) = v.split_once('x') {
                    pixels = w.parse().unwrap_or(0) * h.parse().unwrap_or(0);
                }
            }
        }
        // The variant URI is the next non-empty, non-directive line.
        let mut uri = None;
        while let Some(next) = lines.next() {
            let next = next.trim();
            if next.is_empty() || next.starts_with('#') {
                continue;
            }
            uri = Some(next.to_string());
            break;
        }
        let Some(uri) = uri else { continue };
        let url = resolve(base, &uri);
        let better = match &best {
            None => true,
            Some((b, p, _)) => bandwidth > *b || (bandwidth == *b && pixels > *p),
        };
        if better {
            best = Some((bandwidth, pixels, url));
        }
    }
    Ok(best.map(|(_, _, url)| url))
}

fn parse_key_attrs(attrs: &str) -> Result<Option<(String, Option<String>)>, String> {
    let mut method = "";
    let mut uri: Option<String> = None;
    let mut iv: Option<String> = None;
    for part in attrs.split(',') {
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue,
        };
        match k {
            "METHOD" => method = v,
            "URI" => uri = Some(v.trim_matches('"').to_string()),
            "IV" => iv = Some(v.to_string()),
            _ => {}
        }
    }
    match method {
        "NONE" => Ok(None),
        "AES-128" => match uri {
            Some(u) => Ok(Some((u, iv))),
            None => Err("EXT-X-KEY AES-128 tanpa URI".to_string()),
        },
        other => Err(format!("enkripsi HLS {other} tidak didukung")),
    }
}

/// Parse an HLS media playlist. Live playlists (no EXT-X-ENDLIST)
/// are rejected — only finite VOD playlists are merged.
pub fn parse_hls_media(text: &str, base: &str) -> Result<HlsMedia, String> {
    let endlist = text.contains("#EXT-X-ENDLIST");
    if !endlist {
        return Err(
            "live stream tidak didukung — hanya VOD (playlist berhingga)".to_string(),
        );
    }
    let mut segments = Vec::new();
    let mut key: Option<(String, Option<String>)> = None;
    let mut map: Option<String> = None;
    let mut pending_duration: Option<f64> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(attrs) = line.strip_prefix("#EXTINF:") {
            pending_duration = attrs
                .split(',')
                .next()
                .and_then(|v| v.trim().parse::<f64>().ok());
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            // The key URI is relative to the playlist, like the
            // segment URIs — resolve it now, not at download time.
            key = parse_key_attrs(attrs)?.map(|(u, iv)| (resolve(base, &u), iv));
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
            // URI="init.mp4" (other attributes ignored).
            for part in attrs.split(',') {
                if let Some((k, v)) = part.split_once('=') {
                    if k.trim() == "URI" {
                        map = Some(resolve(base, v.trim().trim_matches('"')));
                    }
                }
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            let dur = pending_duration.take().unwrap_or(0.0);
            segments.push(HlsSegment {
                url: resolve(base, line),
                duration: dur,
                key: key.clone(),
            });
        }
    }
    if segments.is_empty() {
        return Err("playlist HLS tidak berisi segmen".to_string());
    }
    Ok(HlsMedia {
        segments,
        map,
        endlist,
    })
}

// ---------------------------------------------------------------- DASH

#[derive(Debug)]
pub struct DashTrack {
    pub init_url: String,
    pub segments: Vec<(String, f64)>,
}

#[derive(Debug)]
pub struct DashPlan {
    pub video: DashTrack,
    pub audio: Option<DashTrack>,
    /// mediaPresentationDuration, seconds (0 when unknown).
    pub duration: f64,
}

fn attr(tag: &str, name: &str) -> Option<String> {
    // Tolerates name="value" with either quote style.
    let needle = format!("{name}=");
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let end = rest[1..].find(quote)?;
    Some(rest[1..1 + end].to_string())
}

fn parse_iso_duration(s: &str) -> f64 {
    // PT12.0S, PT1M30.5S, PT2H1M — the forms ffmpeg writes.
    let s = s.strip_prefix("PT").unwrap_or(s);
    let mut total = 0.0f64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else {
            let v: f64 = num.parse().unwrap_or(0.0);
            num.clear();
            match c {
                'H' => total += v * 3600.0,
                'M' => total += v * 60.0,
                'S' => total += v,
                _ => {}
            }
        }
    }
    total
}

/// Expand a SegmentTemplate token: $RepresentationID$, $Number$,
/// $Number%05d$, $Time$, $Bandwidth$.
fn expand_template(
    template: &str,
    representation_id: &str,
    number: u64,
    time: u64,
    bandwidth: u64,
) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        let Some(end) = after.find('$') else {
            out.push('$');
            out.push_str(after);
            break;
        };
        let token = &after[..end];
        match token {
            "RepresentationID" => out.push_str(representation_id),
            "Number" => out.push_str(&number.to_string()),
            "Time" => out.push_str(&time.to_string()),
            "Bandwidth" => out.push_str(&bandwidth.to_string()),
            _ => {
                // $Number%05d$ — printf-style zero padding.
                if let Some(fmt) = token.strip_prefix("Number%") {
                    if let Some(width) = fmt.strip_suffix('d') {
                        if let Ok(w) = width.parse::<usize>() {
                            out.push_str(&format!("{number:0>w$}"));
                        } else {
                            out.push_str(&number.to_string());
                        }
                    } else {
                        out.push_str(&number.to_string());
                    }
                } else if let Some(fmt) = token.strip_prefix("Time%") {
                    if let Some(width) = fmt.strip_suffix('d') {
                        if let Ok(w) = width.parse::<usize>() {
                            out.push_str(&format!("{time:0>w$}"));
                        } else {
                            out.push_str(&time.to_string());
                        }
                    } else {
                        out.push_str(&time.to_string());
                    }
                } else {
                    out.push('$');
                    out.push_str(token);
                    out.push('$');
                }
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Parse an MPD (SegmentTemplate only). Dynamic MPDs are rejected —
/// only static VOD manifests are merged.
pub fn parse_mpd(text: &str, base: &str) -> Result<DashPlan, String> {
    let Some(mpd_start) = text.find("<MPD") else {
        return Err("MPD tidak valid: tag <MPD> tidak ditemukan".to_string());
    };
    let mpd_tag_end = text[mpd_start..]
        .find('>')
        .map(|i| mpd_start + i)
        .unwrap_or(text.len());
    let mpd_tag = &text[mpd_start..mpd_tag_end];
    if attr(mpd_tag, "type").as_deref() == Some("dynamic") {
        return Err(
            "live stream tidak didukung — hanya VOD (MPD statis)".to_string(),
        );
    }
    let duration = attr(mpd_tag, "mediaPresentationDuration")
        .map(|s| parse_iso_duration(&s))
        .unwrap_or(0.0);

    // Split into AdaptationSet blocks.
    let mut sets: Vec<(bool, bool, String)> = Vec::new(); // (is_video, is_audio, block)
    let mut search = 0usize;
    while let Some(start) = text[search..].find("<AdaptationSet") {
        let abs = search + start;
        let end = text[abs..]
            .find("</AdaptationSet>")
            .map(|i| abs + i + "</AdaptationSet>".len())
            .unwrap_or(text.len());
        let block = &text[abs..end];
        let head = &block[..block.find('>').unwrap_or(block.len())];
        let ct = attr(head, "contentType").unwrap_or_default();
        let is_video = ct == "video" || block.contains("mimeType=\"video/");
        let is_audio = ct == "audio" || block.contains("mimeType=\"audio/");
        sets.push((is_video, is_audio, block.to_string()));
        search = end;
    }
    if sets.is_empty() {
        return Err("MPD tidak berisi AdaptationSet".to_string());
    }
    let pick = |want_video: bool| -> Result<Option<DashTrack>, String> {
        let mut best: Option<(u64, DashTrack)> = None;
        let mut last_err: Option<String> = None;
        for (is_video, is_audio, block) in &sets {
            if want_video && !*is_video {
                continue;
            }
            if !want_video && !*is_audio {
                continue;
            }
            // Highest-bandwidth Representation in this set.
            let mut reps: Vec<(u64, String, String)> = Vec::new(); // (bw, id, segmenttemplate)
            let mut rsearch = 0usize;
            while let Some(rstart) = block[rsearch..].find("<Representation") {
                let rabs = rsearch + rstart;
                let rtag_end = block[rabs..]
                    .find('>')
                    .map(|i| rabs + i)
                    .unwrap_or(block.len());
                let rtag = &block[rabs..rtag_end];
                let bw = attr(rtag, "bandwidth")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let id = attr(rtag, "id").unwrap_or_default();
                // SegmentTemplate belonging to this Representation:
                // the first full element after the tag, before the
                // next Representation (its SegmentTimeline children
                // live inside the element, not in the opening tag).
                let next_rep = block[rtag_end..]
                    .find("<Representation")
                    .map(|i| rtag_end + i)
                    .unwrap_or(block.len());
                let scope = &block[rtag_end..next_rep];
                let tmpl = scope
                    .find("<SegmentTemplate")
                    .map(|i| {
                        let rest = &scope[i..];
                        let close = rest
                            .find("</SegmentTemplate>")
                            .map(|j| j + "</SegmentTemplate>".len())
                            .or_else(|| rest.find("/>").map(|j| j + 2))
                            .unwrap_or(rest.len());
                        rest[..close].to_string()
                    })
                    .unwrap_or_default();
                reps.push((bw, id, tmpl));
                rsearch = rtag_end;
            }
            reps.sort_by_key(|(bw, _, _)| *bw);
            if let Some((bw, id, tmpl)) = reps.into_iter().last() {
                if tmpl.is_empty() {
                    continue;
                }
                let track = match build_dash_track(&tmpl, &id, bw, duration, base) {
                    Ok(t) => t,
                    Err(e) => {
                        last_err = Some(e);
                        continue;
                    }
                };
                let better = match &best {
                    None => true,
                    Some((b, _)) => bw > *b,
                };
                if better {
                    best = Some((bw, track));
                }
            }
        }
        // When nothing built, the honest reason is the build error
        // (e.g. no SegmentTimeline), not "no video track".
        match best {
            Some((_, t)) => Ok(Some(t)),
            None => match last_err {
                Some(e) => Err(e),
                None => Ok(None),
            },
        }
    };
    let video = match pick(true) {
        Ok(Some(t)) => t,
        Ok(None) => return Err("MPD tidak berisi track video".to_string()),
        Err(e) => return Err(e),
    };
    let audio = pick(false).ok().flatten();
    Ok(DashPlan {
        video,
        audio,
        duration,
    })
}

/// Build one track's segment list from a SegmentTemplate tag.
fn build_dash_track(
    tmpl: &str,
    representation_id: &str,
    bandwidth: u64,
    mpd_duration: f64,
    base: &str,
) -> Result<DashTrack, String> {
    let Some(media) = attr(tmpl, "media") else {
        return Err("SegmentTemplate tanpa media".to_string());
    };
    let init_tpl = attr(tmpl, "initialization").unwrap_or_default();
    let timescale: f64 = attr(tmpl, "timescale")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    let start_number: u64 = attr(tmpl, "startNumber")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let seg_duration_attr: Option<f64> = attr(tmpl, "duration").and_then(|v| v.parse().ok());

    // SegmentTimeline: <S t=".." d=".." r=".."/> — r repeats the
    // element (r+1 segments total for that S).
    let mut timeline: Vec<(u64, u64)> = Vec::new(); // (start time, duration)
    let mut tsearch = 0usize;
    let mut cursor: u64 = 0;
    while let Some(sstart) = tmpl[tsearch..].find("<S ") {
        let sabs = tsearch + sstart;
        let stag_end = tmpl[sabs..]
            .find('>')
            .map(|i| sabs + i)
            .unwrap_or(tmpl.len());
        let stag = &tmpl[sabs..stag_end];
        let t = attr(stag, "t").and_then(|v| v.parse().ok());
        let d: u64 = attr(stag, "d").and_then(|v| v.parse().ok()).unwrap_or(0);
        let r: u64 = attr(stag, "r").and_then(|v| v.parse().ok()).unwrap_or(0);
        let start = t.unwrap_or(cursor);
        for i in 0..=r {
            timeline.push((start + i * d, d));
        }
        cursor = start + (r + 1) * d;
        tsearch = stag_end;
    }

    let mut segments = Vec::new();
    if !timeline.is_empty() {
        for (i, (time, d)) in timeline.iter().enumerate() {
            let number = start_number + i as u64;
            let url = resolve(
                base,
                &expand_template(&media, representation_id, number, *time, bandwidth),
            );
            segments.push((url, *d as f64 / timescale));
        }
    } else if let Some(d) = seg_duration_attr {
        if mpd_duration <= 0.0 {
            return Err(
                "MPD tanpa SegmentTimeline dan tanpa durasi total".to_string(),
            );
        }
        let seg_secs = d / timescale;
        let count = (mpd_duration / seg_secs).ceil() as u64;
        let mut time: u64 = 0;
        for i in 0..count {
            let number = start_number + i;
            let url = resolve(
                base,
                &expand_template(&media, representation_id, number, time, bandwidth),
            );
            segments.push((url, seg_secs));
            time += d as u64;
        }
    } else {
        return Err(
            "MPD tidak didukung: SegmentTemplate tanpa SegmentTimeline dan tanpa duration"
                .to_string(),
        );
    }
    if segments.is_empty() {
        return Err("MPD tidak berisi segmen".to_string());
    }
    let init_url = if init_tpl.is_empty() {
        String::new()
    } else {
        resolve(
            base,
            &expand_template(&init_tpl, representation_id, start_number, 0, bandwidth),
        )
    };
    Ok(DashTrack {
        init_url,
        segments,
    })
}

// ---------------------------------------------------------------- merge

fn run_ffmpeg(args: &[String]) -> Result<(), String> {
    let path = ffmpeg_path().ok_or_else(|| "ffmpeg tidak ditemukan".to_string())?;
    let out = Command::new(path)
        .args(args)
        .output()
        .map_err(|e| format!("gagal menjalankan ffmpeg: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let tail: Vec<&str> = err.lines().rev().take(3).collect();
    Err(format!(
        "ffmpeg gagal: {}",
        tail.iter().rev().cloned().collect::<Vec<_>>().join(" | ")
    ))
}

fn concat_files(files: &[PathBuf], out: &Path) -> Result<(), String> {
    let mut w = std::fs::File::create(out).map_err(|e| format!("gagal membuat {out:?}: {e}"))?;
    let mut buf = [0u8; 65536];
    for f in files {
        let mut r = std::fs::File::open(f).map_err(|e| format!("gagal membuka {f:?}: {e}"))?;
        loop {
            let n = r.read(&mut buf).map_err(|e| format!("gagal membaca {f:?}: {e}"))?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n])
                .map_err(|e| format!("gagal menulis {out:?}: {e}"))?;
        }
    }
    Ok(())
}

/// Merge a fully downloaded plan into one file at `out_path`
/// (written via a temp file, then renamed). Returns the extension
/// actually written ("mp4" or "ts"). Blocking — call from
/// `spawn_blocking`.
pub fn merge_stream(plan: &StreamPlan, seg_dir: &Path, out_path: &Path) -> Result<String, String> {
    let tmp = |name: &str| seg_dir.join(name);
    if plan.encrypted() {
        // Decryption goes through ffmpeg with a rewritten local
        // playlist (key URIs point at the downloaded key files, IVs
        // preserved). Binary concat cannot decrypt.
        let ffmpeg = ffmpeg_path().ok_or_else(|| {
            "stream terenkripsi AES-128 butuh ffmpeg — instal ffmpeg untuk mengunduh stream ini"
                .to_string()
        })?;
        let _ = ffmpeg;
        let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");
        let max_dur = plan
            .segments
            .iter()
            .map(|s| s.duration)
            .fold(0.0f64, f64::max)
            .ceil() as u64;
        playlist.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", max_dur.max(1)));
        playlist.push_str("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n");
        let mut current_key: Option<usize> = None;
        for seg in &plan.segments {
            if seg.key != current_key {
                current_key = seg.key;
                if let Some(kidx) = seg.key {
                    let key = &plan.keys[kidx];
                    playlist.push_str(&format!(
                        "#EXT-X-KEY:METHOD=AES-128,URI=\"{}\"{}",
                        key.file,
                        key.iv
                            .as_ref()
                            .map(|iv| format!(",IV={iv}"))
                            .unwrap_or_default()
                    ));
                    playlist.push('\n');
                }
            }
            playlist.push_str(&format!("#EXTINF:{:.6},\n{}\n", seg.duration, seg.file));
        }
        playlist.push_str("#EXT-X-ENDLIST\n");
        let local = tmp("local.m3u8");
        std::fs::write(&local, playlist).map_err(|e| format!("gagal menulis playlist lokal: {e}"))?;
        let out_tmp = tmp("merged.mp4");
        run_ffmpeg(&[
            "-y".into(),
            "-allowed_extensions".into(),
            "ALL".into(),
            "-i".into(),
            local.to_string_lossy().into_owned(),
            "-c".into(),
            "copy".into(),
            out_tmp.to_string_lossy().into_owned(),
        ])?;
        std::fs::rename(&out_tmp, out_path)
            .map_err(|e| format!("gagal memindahkan hasil merge: {e}"))?;
        return Ok("mp4".to_string());
    }

    match plan.kind {
        StreamKind::Hls => {
            let is_fmp4 = plan.init.is_some();
            // Binary concat: init first (fMP4), then segments in order.
            let mut files: Vec<PathBuf> = Vec::new();
            if let Some(init) = &plan.init {
                files.push(seg_dir.join(&init.file));
            }
            for s in &plan.segments {
                files.push(seg_dir.join(&s.file));
            }
            if is_fmp4 {
                let tmp_mp4 = tmp("merged.mp4");
                concat_files(&files, &tmp_mp4)?;
                if ffmpeg_available() {
                    // Remux to normalise the file (same bytes, clean
                    // container). A failure here falls back to the
                    // concatenated file itself — it already plays.
                    let remuxed = tmp("remuxed.mp4");
                    if run_ffmpeg(&[
                        "-y".into(),
                        "-i".into(),
                        tmp_mp4.to_string_lossy().into_owned(),
                        "-c".into(),
                        "copy".into(),
                        remuxed.to_string_lossy().into_owned(),
                    ])
                    .is_ok()
                    {
                        std::fs::rename(&remuxed, out_path)
                            .map_err(|e| format!("gagal memindahkan hasil merge: {e}"))?;
                        return Ok("mp4".to_string());
                    }
                }
                std::fs::rename(&tmp_mp4, out_path)
                    .map_err(|e| format!("gagal memindahkan hasil merge: {e}"))?;
                Ok("mp4".to_string())
            } else {
                let tmp_ts = tmp("merged.ts");
                concat_files(&files, &tmp_ts)?;
                if ffmpeg_available() {
                    let remuxed = tmp("remuxed.mp4");
                    // AAC in TS needs the adtstoasc bitstream filter
                    // for a clean MP4; retry plain when it fails
                    // (no audio, or a different audio codec).
                    let ok = run_ffmpeg(&[
                        "-y".into(),
                        "-i".into(),
                        tmp_ts.to_string_lossy().into_owned(),
                        "-c".into(),
                        "copy".into(),
                        "-bsf:a".into(),
                        "aac_adtstoasc".into(),
                        remuxed.to_string_lossy().into_owned(),
                    ])
                    .is_ok()
                        || run_ffmpeg(&[
                            "-y".into(),
                            "-i".into(),
                            tmp_ts.to_string_lossy().into_owned(),
                            "-c".into(),
                            "copy".into(),
                            remuxed.to_string_lossy().into_owned(),
                        ])
                        .is_ok();
                    if ok {
                        std::fs::rename(&remuxed, out_path)
                            .map_err(|e| format!("gagal memindahkan hasil merge: {e}"))?;
                        return Ok("mp4".to_string());
                    }
                }
                std::fs::rename(&tmp_ts, out_path)
                    .map_err(|e| format!("gagal memindahkan hasil merge: {e}"))?;
                Ok("ts".to_string())
            }
        }
        StreamKind::Dash => {
            let Some(_) = ffmpeg_path() else {
                return Err(
                    "DASH butuh ffmpeg untuk menggabungkan segmen — instal ffmpeg untuk mengunduh stream ini"
                        .to_string(),
                );
            };
            let mut video_files: Vec<PathBuf> = Vec::new();
            if let Some(init) = &plan.init {
                video_files.push(seg_dir.join(&init.file));
            }
            for s in &plan.segments {
                video_files.push(seg_dir.join(&s.file));
            }
            let video_cat = tmp("video.mp4");
            concat_files(&video_files, &video_cat)?;
            let mut args: Vec<String> = vec!["-y".into(), "-i".into(), video_cat.to_string_lossy().into_owned()];
            let audio_cat = if !plan.audio_segments.is_empty() {
                let mut audio_files: Vec<PathBuf> = Vec::new();
                if let Some(init) = &plan.audio_init {
                    audio_files.push(seg_dir.join(&init.file));
                }
                for s in &plan.audio_segments {
                    audio_files.push(seg_dir.join(&s.file));
                }
                let ac = tmp("audio.mp4");
                concat_files(&audio_files, &ac)?;
                args.push("-i".into());
                args.push(ac.to_string_lossy().into_owned());
                Some(ac)
            } else {
                None
            };
            let _ = audio_cat;
            // Mux to a .mp4 temp file, then rename: out_path is the
            // .part file, and ffmpeg cannot infer the muxer from
            // that extension ("Invalid argument" on the output).
            let merged = tmp("merged.mp4");
            args.push("-c".into());
            args.push("copy".into());
            args.push(merged.to_string_lossy().into_owned());
            run_ffmpeg(&args)?;
            std::fs::rename(&merged, out_path)
                .map_err(|e| format!("gagal memindahkan hasil merge: {e}"))?;
            Ok("mp4".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720\nhigh.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1920x1080\nhigh2.m3u8\n";

    const MEDIA: &str = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x00000000000000000000000000000001\n#EXTINF:2.000000,\nseg0.ts\n#EXTINF:2.000000,\n../seg1.ts\n#EXT-X-ENDLIST\n";

    const LIVE: &str = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXTINF:2.000000,\nseg0.ts\n";

    const MPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<MPD type="static" mediaPresentationDuration="PT12.0S" minBufferTime="PT4.0S">
<Period id="0" start="PT0.0S">
<AdaptationSet id="0" contentType="video">
<Representation id="0" mimeType="video/mp4" codecs="avc1.64000c" bandwidth="40316" width="320" height="240">
<SegmentTemplate timescale="10240" initialization="init-stream$RepresentationID$.m4s" media="chunk-stream$RepresentationID$-$Number%05d$.m4s" startNumber="1">
<SegmentTimeline><S t="0" d="20480" r="5" /></SegmentTimeline>
</SegmentTemplate>
</Representation>
<Representation id="1" mimeType="video/mp4" codecs="avc1.64000c" bandwidth="999999" width="640" height="360">
<SegmentTemplate timescale="10240" initialization="init-stream$RepresentationID$.m4s" media="chunk-stream$RepresentationID$-$Number%05d$.m4s" startNumber="1">
<SegmentTimeline><S t="0" d="20480" r="5" /></SegmentTimeline>
</SegmentTemplate>
</Representation>
</AdaptationSet>
<AdaptationSet id="1" contentType="audio">
<Representation id="1" mimeType="audio/mp4" codecs="mp4a.40.2" bandwidth="69000" audioSamplingRate="44100">
<SegmentTemplate timescale="44100" initialization="init-stream$RepresentationID$.m4s" media="chunk-stream$RepresentationID$-$Number%05d$.m4s" startNumber="1">
<SegmentTimeline><S t="0" d="79872" /><S d="88064" r="2" /><S d="89088" r="1" /><S d="7168" /></SegmentTimeline>
</SegmentTemplate>
</Representation>
</AdaptationSet>
</Period>
</MPD>"#;

    #[test]
    fn hls_master_picks_highest_bandwidth() {
        // Same bandwidth on two variants: the higher resolution wins.
        let got = parse_hls_master(MASTER, "https://x.example/vod/master.m3u8")
            .unwrap()
            .unwrap();
        assert_eq!(got, "https://x.example/vod/high2.m3u8");
        // A media playlist has no variants.
        assert!(parse_hls_master(MEDIA, "https://x.example/vod/a.m3u8")
            .unwrap()
            .is_none());
    }

    #[test]
    fn hls_media_parses_segments_key_and_relative_urls() {
        let m = parse_hls_media(MEDIA, "https://x.example/vod/a.m3u8").unwrap();
        assert!(m.endlist);
        assert_eq!(m.segments.len(), 2);
        assert_eq!(m.segments[0].url, "https://x.example/vod/seg0.ts");
        assert_eq!(m.segments[1].url, "https://x.example/seg1.ts");
        assert!((m.segments[0].duration - 2.0).abs() < f64::EPSILON);
        // The key applies to every segment after its KEY line.
        let (key_url, iv) = m.segments[0].key.clone().unwrap();
        assert_eq!(key_url, "https://x.example/vod/key.bin");
        assert_eq!(iv.as_deref(), Some("0x00000000000000000000000000000001"));
        assert!(m.segments[1].key.is_some());
        assert!(m.map.is_none());
    }

    #[test]
    fn hls_live_playlist_is_rejected() {
        let err = parse_hls_media(LIVE, "https://x.example/live.m3u8").unwrap_err();
        assert!(err.contains("live stream"), "{err}");
    }

    #[test]
    fn hls_unknown_encryption_is_rejected() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"k.bin\"\n#EXTINF:2.0,\ns.ts\n#EXT-X-ENDLIST\n";
        let err = parse_hls_media(text, "https://x.example/a.m3u8").unwrap_err();
        assert!(err.contains("tidak didukung"), "{err}");
    }

    #[test]
    fn mpd_parses_video_audio_and_timeline() {
        let p = parse_mpd(MPD, "https://x.example/vod/dash.mpd").unwrap();
        assert!((p.duration - 12.0).abs() < f64::EPSILON);
        // Highest-bandwidth video representation wins (id 1).
        assert_eq!(
            p.video.init_url,
            "https://x.example/vod/init-stream1.m4s"
        );
        assert_eq!(p.video.segments.len(), 6);
        assert_eq!(
            p.video.segments[0].0,
            "https://x.example/vod/chunk-stream1-00001.m4s"
        );
        assert_eq!(
            p.video.segments[5].0,
            "https://x.example/vod/chunk-stream1-00006.m4s"
        );
        assert!((p.video.segments[0].1 - 2.0).abs() < 0.01);
        let audio = p.audio.unwrap();
        assert_eq!(audio.segments.len(), 7);
        assert_eq!(
            audio.init_url,
            "https://x.example/vod/init-stream1.m4s"
        );
    }

    #[test]
    fn mpd_dynamic_is_rejected() {
        let text = MPD.replace("type=\"static\"", "type=\"dynamic\"");
        let err = parse_mpd(&text, "https://x.example/vod/dash.mpd").unwrap_err();
        assert!(err.contains("live stream"), "{err}");
    }

    #[test]
    fn mpd_without_timeline_and_duration_is_rejected() {
        // Strip every SegmentTimeline element (opening tag, S
        // entries, closing tag): the templates then have neither a
        // timeline nor a duration attribute, so parsing must fail
        // honestly instead of producing zero segments.
        let mut text = MPD.to_string();
        while let Some(start) = text.find("<SegmentTimeline>") {
            let end = text[start..]
                .find("</SegmentTimeline>")
                .map(|i| start + i + "</SegmentTimeline>".len())
                .unwrap_or(text.len());
            text.replace_range(start..end, "");
        }
        let err = parse_mpd(&text, "https://x.example/vod/dash.mpd").unwrap_err();
        assert!(err.contains("tidak didukung"), "{err}");
    }

    #[test]
    fn kind_detection_and_output_names() {
        assert_eq!(
            kind_for_url("https://x.example/vod/a.m3u8?token=1"),
            Some(StreamKind::Hls)
        );
        assert_eq!(
            kind_for_url("https://x.example/vod/a.MPD"),
            Some(StreamKind::Dash)
        );
        assert_eq!(kind_for_url("https://x.example/vod/a.mp4"), None);
        assert_eq!(
            kind_for_mime("application/vnd.apple.mpegurl"),
            Some(StreamKind::Hls)
        );
        assert_eq!(
            kind_for_mime("application/dash+xml; charset=utf-8"),
            Some(StreamKind::Dash)
        );
        assert_eq!(kind_for_mime("video/mp4"), None);
        assert_eq!(output_filename("stream.m3u8", "mp4"), "stream.mp4");
        assert_eq!(output_filename("stream.MPD", "mp4"), "stream.mp4");
        assert_eq!(output_filename("my video.mp4", "mp4"), "my video.mp4");
        assert_eq!(output_filename("video", "mp4"), "video.mp4");
        assert_eq!(output_extension(StreamKind::Hls, false), "ts");
        assert_eq!(output_extension(StreamKind::Hls, true), "mp4");
        assert_eq!(output_extension(StreamKind::Dash, true), "mp4");
    }

    #[test]
    fn concat_files_joins_in_order() {
        let dir = std::env::temp_dir().join("zdm-stream-test-concat");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        std::fs::write(&a, b"hello ").unwrap();
        std::fs::write(&b, b"world").unwrap();
        let out = dir.join("out.bin");
        concat_files(&[a, b], &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"hello world");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ffmpeg_env_seam_can_pretend_missing() {
        // ZDM_FFMPEG="" forces "missing" even when ffmpeg exists.
        std::env::set_var("ZDM_FFMPEG", "");
        assert!(!ffmpeg_available());
        std::env::remove_var("ZDM_FFMPEG");
        // On this machine ffmpeg is installed; the real binary is
        // found on PATH. (If it is ever missing, this fails loudly
        // instead of silently skipping — the feature needs it.)
        assert!(ffmpeg_available());
        assert!(ffmpeg_version().unwrap().starts_with("ffmpeg version"));
    }

    #[test]
    fn stream_plan_round_trips_through_json() {
        let mut plan = StreamPlan::pending(
            StreamKind::Dash,
            "https://x.example/vod/dash.mpd".to_string(),
            "mp4",
        );
        plan.segments.push(StreamSegment {
            url: "https://x.example/vod/chunk-00001.m4s".to_string(),
            file: "v-000001.m4s".to_string(),
            size: Some(1234),
            done: true,
            duration: 2.0,
            key: None,
        });
        let text = serde_json::to_string(&plan).unwrap();
        let back: StreamPlan = serde_json::from_str(&text).unwrap();
        assert_eq!(back.segments.len(), 1);
        assert!(back.segments[0].done);
        assert_eq!(back.progress(), (1, 1));
        assert!(!back.encrypted());
    }
}
