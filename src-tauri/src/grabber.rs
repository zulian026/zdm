//! Site Grabber lite — fetch one page, extract downloadable links and
//! embedded media (`<video>`/`<audio>`/`<source>`), resolve them against
//! the page URL, classify each result, and HEAD-probe sizes (best effort).
//! Mirrors `downloader/grabber.py` in the Python ZDM.

use serde::Serialize;
use std::collections::HashSet;
use std::time::Duration;
use tokio::task::JoinSet;

pub const GRAB_EXTENSIONS: &[&str] = &[
    ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".txt", ".md", ".epub", ".csv",
    ".mp3", ".flac", ".wav", ".ogg", ".m4a", ".aac", ".opus",
    ".mp4", ".mkv", ".avi", ".mov", ".webm", ".wmv", ".flv",
    ".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".svg",
    ".exe", ".msi", ".dmg", ".deb", ".rpm", ".appimage", ".apk", ".bin",
    ".zip", ".rar", ".7z", ".tar", ".gz", ".bz2", ".xz", ".zst",
    // Stream manifests: the engine downloads every segment and
    // merges them into one video (see stream.rs); live streams are
    // rejected and DASH needs ffmpeg.
    ".m3u8", ".mpd",
];

pub const VIDEO_EXTENSIONS: &[&str] = &[
    ".mp4", ".webm", ".mov", ".mkv", ".avi", ".wmv", ".flv", ".m4v", ".3gp",
];
pub const AUDIO_EXTENSIONS: &[&str] = &[
    ".mp3", ".flac", ".wav", ".ogg", ".m4a", ".aac", ".opus",
];
pub const MANIFEST_EXTENSIONS: &[&str] = &[".m3u8", ".mpd"];

/// What a grabbed result is. Serialised lowercase for the frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GrabKind {
    Video,
    Audio,
    Manifest,
    Link,
}

/// One grabbed result: the URL, its kind, and the HEAD-probed size
/// (None when the server did not answer or sent no Content-Length).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrabItem {
    pub url: String,
    pub kind: GrabKind,
    pub size: Option<u64>,
}

/// Extract downloadable links from raw HTML. Pure function — unit tested.
pub fn extract_links(page_url: &str, html: &str) -> Vec<String> {
    let base = match url::Url::parse(page_url) {
        Ok(u) => u,
        Err(_) => return Vec::new(),
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let lower = html.to_lowercase();
    let mut search_from = 0usize;
    while let Some(rel) = lower[search_from..].find("href=") {
        let abs = search_from + rel + 5;
        let rest = &html[abs..];
        let rest_lower = &lower[abs..];
        let (value, consumed) = if let Some(r) = rest.strip_prefix('"') {
            match r.find('"') {
                Some(end) => (&r[..end], end + 2),
                None => break,
            }
        } else if let Some(r) = rest.strip_prefix('\'') {
            match r.find('\'') {
                Some(end) => (&r[..end], end + 2),
                None => break,
            }
        } else {
            // Unquoted href value.
            let end = rest_lower
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(rest.len());
            (&rest[..end], end + 1)
        };
        search_from = abs + consumed;
        let value = value.trim();
        if value.is_empty() || value.starts_with('#') || value.starts_with("javascript:") {
            continue;
        }
        let Ok(joined) = base.join(value) else {
            continue;
        };
        if joined.scheme() != "http" && joined.scheme() != "https" {
            continue;
        }
        let path = joined.path().to_lowercase();
        let is_file = GRAB_EXTENSIONS
            .iter()
            .any(|ext| path.ends_with(ext) || path.contains(&format!("{ext}?")));
        if !is_file {
            continue;
        }
        let s = joined.to_string();
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    out
}

/// Classify a URL path (lowercase, no query) by media extension.
/// Returns None when the extension says nothing about the media type.
fn kind_for_path(path: &str) -> Option<GrabKind> {
    if MANIFEST_EXTENSIONS.iter().any(|e| path.ends_with(e)) {
        Some(GrabKind::Manifest)
    } else if VIDEO_EXTENSIONS.iter().any(|e| path.ends_with(e)) {
        Some(GrabKind::Video)
    } else if AUDIO_EXTENSIONS.iter().any(|e| path.ends_with(e)) {
        Some(GrabKind::Audio)
    } else {
        None
    }
}

/// Value of attribute `name` inside a tag's raw text (`<tag ...>`).
/// Handles double-quoted, single-quoted, and unquoted values.
fn attr_value(tag_lower: &str, tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let mut from = 0usize;
    while let Some(rel) = tag_lower[from..].find(&needle) {
        let abs = from + rel + needle.len();
        let rest = &tag[abs..];
        let rest_lower = &tag_lower[abs..];
        // `srcset=` also contains `src=`; skip matches where the char
        // before the name is not whitespace (e.g. the `src` in `srcset`).
        let before_ok = abs == needle.len() - 1
            || tag_lower[..abs - needle.len()]
                .chars()
                .last()
                .map(|c| c.is_whitespace())
                .unwrap_or(true);
        if !before_ok {
            from = abs;
            continue;
        }
        let value = if let Some(r) = rest.strip_prefix('"') {
            r.find('"').map(|end| &r[..end])
        } else if let Some(r) = rest.strip_prefix('\'') {
            r.find('\'').map(|end| &r[..end])
        } else {
            let end = rest_lower
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(rest.len());
            Some(&rest[..end])
        };
        return value.map(|v| v.trim().to_string());
    }
    None
}

/// Pick the download URL out of a `srcset` value (`url 720w, url 1080w`).
/// The last candidate is conventionally the largest; take it.
fn srcset_url(srcset: &str) -> Option<String> {
    srcset
        .split(',')
        .filter_map(|entry| entry.split_whitespace().next())
        .last()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Spans of `<tag ...>` opening tags: (byte offset of `<`, offset just
/// past `>`). The char after the tag name must be whitespace or `>`,
/// so `<video>` matches but `<videogame>` does not.
fn tag_spans(lower: &str, tag: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let needle = format!("<{tag}");
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(&needle) {
        let start = from + rel;
        let after = start + needle.len();
        let boundary_ok = lower[after..]
            .chars()
            .next()
            .map(|c| c.is_whitespace() || c == '>')
            .unwrap_or(false);
        if !boundary_ok {
            from = after;
            continue;
        }
        match lower[after..].find('>') {
            Some(end) => {
                out.push((start, after + end + 1));
                from = after + end + 1;
            }
            None => break,
        }
    }
    out
}

/// Extract embedded media from raw HTML: `<video>`/`<audio>` `src` and
/// `<source>` `src`/`srcset` inside them. Pure function — unit tested.
///
/// - URLs resolve against the page URL; `blob:`/`data:` and anything
///   that is not http(s) is skipped — the browser cannot hand those out.
/// - A `<source>` outside any media element (e.g. in `<picture>`) is
///   kept only when its extension is a media or manifest type.
/// - A `.m3u8`/`.mpd` URL is always a Manifest, never a Video.
pub fn extract_media(page_url: &str, html: &str) -> Vec<GrabItem> {
    let base = match url::Url::parse(page_url) {
        Ok(u) => u,
        Err(_) => return Vec::new(),
    };
    let lower = html.to_lowercase();
    // Media regions: (start, end, kind). A `<source>` inside a region
    // inherits the region's kind.
    let mut regions: Vec<(usize, usize, GrabKind)> = Vec::new();
    for (tag, kind) in [("video", GrabKind::Video), ("audio", GrabKind::Audio)] {
        for (start, end) in tag_spans(&lower, tag) {
            let close = format!("</{tag}>");
            let region_end = lower[start..]
                .find(&close)
                .map(|i| start + i + close.len())
                .unwrap_or(end);
            regions.push((start, region_end, kind));
        }
    }
    // (document position, raw URL, kind hint from the enclosing tag)
    let mut candidates: Vec<(usize, String, Option<GrabKind>)> = Vec::new();
    // `src` on the media tags themselves.
    for (start, end, kind) in &regions {
        let tag = &html[*start..*end];
        let tag_lower = &lower[*start..*end];
        if let Some(src) = attr_value(tag_lower, tag, "src")
            .filter(|s| !s.is_empty())
            .or_else(|| attr_value(tag_lower, tag, "srcset").and_then(|s| srcset_url(&s)))
        {
            candidates.push((*start, src, Some(*kind)));
        }
    }
    // `<source>` tags.
    for (start, end) in tag_spans(&lower, "source") {
        let tag = &html[start..end];
        let tag_lower = &lower[start..end];
        let Some(src) = attr_value(tag_lower, tag, "src")
            .filter(|s| !s.is_empty())
            .or_else(|| attr_value(tag_lower, tag, "srcset").and_then(|s| srcset_url(&s)))
        else {
            continue;
        };
        let hint = regions
            .iter()
            .find(|(rs, re, _)| start >= *rs && start < *re)
            .map(|(_, _, k)| *k);
        candidates.push((start, src, hint));
    }
    candidates.sort_by_key(|(pos, _, _)| *pos);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<GrabItem> = Vec::new();
    for (_, raw, hint) in candidates {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let Ok(joined) = base.join(raw) else {
            continue;
        };
        if joined.scheme() != "http" && joined.scheme() != "https" {
            continue;
        }
        let path = joined.path().to_lowercase();
        let kind = match kind_for_path(&path) {
            Some(GrabKind::Manifest) => GrabKind::Manifest,
            Some(k) => k,
            None => match hint {
                Some(k) => k,
                None => continue,
            },
        };
        let url = joined.to_string();
        if seen.insert(url.clone()) {
            out.push(GrabItem {
                url,
                kind,
                size: None,
            });
        }
    }
    out
}

/// Media first (document order), then plain file links not already
/// listed as media. Manifest links become Manifest, the rest Link.
pub fn extract_grab_items(page_url: &str, html: &str) -> Vec<GrabItem> {
    let mut items = extract_media(page_url, html);
    let mut seen: HashSet<String> = items.iter().map(|i| i.url.clone()).collect();
    for link in extract_links(page_url, html) {
        if !seen.insert(link.clone()) {
            continue;
        }
        let path = url::Url::parse(&link)
            .map(|u| u.path().to_lowercase())
            .unwrap_or_default();
        let kind = if MANIFEST_EXTENSIONS.iter().any(|e| path.ends_with(e)) {
            GrabKind::Manifest
        } else {
            GrabKind::Link
        };
        items.push(GrabItem {
            url: link,
            kind,
            size: None,
        });
    }
    items
}

/// HEAD-probe one URL for its Content-Length. Best effort: any failure
/// (timeout, non-2xx, no header) yields None instead of an error.
async fn probe_size(client: &reqwest::Client, url: &str) -> Option<u64> {
    let resp = tokio::time::timeout(Duration::from_secs(5), client.head(url).send())
        .await
        .ok()?
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.headers()
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

pub async fn grab_links(page_url: &str, client: &reqwest::Client) -> Result<Vec<GrabItem>, String> {
    let resp = client
        .get(page_url)
        .header("User-Agent", "ZDM/1.0")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let html = resp.text().await.map_err(|e| e.to_string())?;
    let mut items = extract_grab_items(page_url, &html);
    // Probe sizes concurrently, at most 24 items — a page with dozens
    // of links should not turn into dozens of HEAD requests.
    let mut set = JoinSet::new();
    for (i, item) in items.iter().enumerate().take(24) {
        let client = client.clone();
        let url = item.url.clone();
        set.spawn(async move { (i, probe_size(&client, &url).await) });
    }
    while let Some(res) = set.join_next().await {
        if let Ok((i, size)) = res {
            items[i].size = size;
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(items: &[GrabItem]) -> Vec<(String, GrabKind)> {
        items
            .iter()
            .map(|i| (i.url.clone(), i.kind))
            .collect()
    }

    #[test]
    fn grabber_extracts_only_files() {
        let html = r##"<a href="/files/report.pdf">pdf</a>
            <a href="https://cdn.example.com/song.mp3?x=1">mp3</a>
            <a href="/about">page</a>
            <a href="#top">anchor</a>
            <a href='/img/photo.JPG'>jpg</a>
            <a href="/files/report.pdf">dup</a>"##;
        let links = extract_links("https://example.com/docs/", html);
        assert_eq!(links.len(), 3);
        assert!(links.contains(&"https://example.com/files/report.pdf".to_string()));
        assert!(links.contains(&"https://cdn.example.com/song.mp3?x=1".to_string()));
        assert!(links.contains(&"https://example.com/img/photo.JPG".to_string()));
    }

    #[test]
    fn grabs_video_audio_and_source_tags() {
        let html = r##"<video src="/clip.mp4" controls></video>
            <audio src="https://cdn.example.com/song.mp3"></audio>
            <video controls><source src="/alt.webm" type="video/webm"></video>
            <audio controls><source src="/voice.ogg" type="audio/ogg"></audio>"##;
        let items = extract_media("https://example.com/page", html);
        assert_eq!(
            kinds(&items),
            vec![
                ("https://example.com/clip.mp4".to_string(), GrabKind::Video),
                ("https://cdn.example.com/song.mp3".to_string(), GrabKind::Audio),
                ("https://example.com/alt.webm".to_string(), GrabKind::Video),
                ("https://example.com/voice.ogg".to_string(), GrabKind::Audio),
            ]
        );
    }

    #[test]
    fn resolves_relative_media_urls() {
        let html = r##"<video src="../media/clip.mp4"></video><audio src="song.mp3"></audio>"##;
        let items = extract_media("https://example.com/docs/page.html", html);
        assert_eq!(
            kinds(&items),
            vec![
                ("https://example.com/media/clip.mp4".to_string(), GrabKind::Video),
                ("https://example.com/docs/song.mp3".to_string(), GrabKind::Audio),
            ]
        );
    }

    #[test]
    fn manifest_is_manifest_not_video() {
        let html = r##"<video><source src="/stream.m3u8"></video>
            <a href="/live.mpd">dash</a>"##;
        let items = extract_grab_items("https://example.com/", html);
        assert_eq!(
            kinds(&items),
            vec![
                ("https://example.com/stream.m3u8".to_string(), GrabKind::Manifest),
                ("https://example.com/live.mpd".to_string(), GrabKind::Manifest),
            ]
        );
    }

    #[test]
    fn blob_and_data_sources_skipped() {
        let html = r##"<video src="blob:https://example.com/abc"></video>
            <video><source src="data:video/mp4;base64,AAAA"></video>
            <video src="/real.mp4"></video>"##;
        let items = extract_media("https://example.com/", html);
        assert_eq!(
            kinds(&items),
            vec![("https://example.com/real.mp4".to_string(), GrabKind::Video),]
        );
    }

    #[test]
    fn srcset_picks_last_candidate() {
        let html = r##"<video><source srcset="/clip-720.mp4 720w, /clip-1080.mp4 1080w"></video>"##;
        let items = extract_media("https://example.com/", html);
        assert_eq!(
            kinds(&items),
            vec![("https://example.com/clip-1080.mp4".to_string(), GrabKind::Video),]
        );
    }

    #[test]
    fn picture_sources_skipped() {
        let html = r##"<picture><source srcset="/img.webp" type="image/webp"><img src="/img.jpg"></picture>
            <video src="/clip.mp4"></video>"##;
        let items = extract_media("https://example.com/", html);
        assert_eq!(
            kinds(&items),
            vec![("https://example.com/clip.mp4".to_string(), GrabKind::Video),]
        );
    }

    #[test]
    fn grab_items_dedupe_media_and_link() {
        let html = r##"<video src="/clip.mp4"></video><a href="/clip.mp4">same file</a>
            <a href="/report.pdf">pdf</a>"##;
        let items = extract_grab_items("https://example.com/", html);
        assert_eq!(
            kinds(&items),
            vec![
                ("https://example.com/clip.mp4".to_string(), GrabKind::Video),
                ("https://example.com/report.pdf".to_string(), GrabKind::Link),
            ]
        );
    }

    #[test]
    fn extensionless_media_src_keeps_tag_kind() {
        let html = r##"<video src="/watch/123/stream"></video>"##;
        let items = extract_media("https://example.com/", html);
        assert_eq!(
            kinds(&items),
            vec![("https://example.com/watch/123/stream".to_string(), GrabKind::Video),]
        );
    }
}
