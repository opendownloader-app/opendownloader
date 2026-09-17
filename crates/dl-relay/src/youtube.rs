//! The `/youtube` route: server-side extraction, streamed.
//!
//! The plain proxy in [`crate`] relays bytes a page already knows the URL of. YouTube
//! does not work that way: the media URLs expire in minutes, carry an `n` parameter that
//! must be deciphered, and are gated behind a Proof-of-Origin token that only a real,
//! trusted browser session mints. A page that fetches the URL directly gets about the
//! first minute and then a wall of 403s — the "60 second wall" users reported.
//!
//! `yt-dlp` does the whole dance (PO token, `n`-decipher, SABR/UMP, mux), and is a large
//! community project that keeps doing it as YouTube changes. So the relay drives `yt-dlp`
//! and streams its stdout straight to the caller. This is the one route that runs an
//! external program, and it is off unless [`YoutubeConfig::enabled`](crate::config::YoutubeConfig).
//!
//! **Where it runs decides whether it works.** On a residential IP `yt-dlp` pulls full
//! videos with nothing configured. On a datacenter IP YouTube applies its harshest
//! distrust and the same ~60s wall returns — the fix is `cookies_file` + a residential
//! `proxy`, which is the arms race, not a relay bug. This is stated again in the config
//! docs and the README because deploying to a cloud VM without them is the one way to
//! ship this and have it "still not work".
//!
//! **Storage:** merging separate video and audio into a clean, seekable MP4 cannot be
//! done on a pipe — muxing an fMP4 to stdout leaves the video track unplayable `bin_data`,
//! measured. So `yt-dlp` writes the merged file into a **per-request scratch directory**,
//! the relay streams that file back, and the directory is deleted the instant the response
//! ends or the caller hangs up (a `Drop` guard, so a client disconnect cleans up too).
//! Nothing is cached, and nothing survives a request. The cost is latency: the caller
//! waits for the download-and-mux before the stream begins, in exchange for a correct file
//! with a real `Content-Length`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::io::ReaderStream;

use crate::Relay;

/// A scratch directory that deletes itself when dropped.
///
/// It rides inside the response body's stream state, so the one thing that is always true
/// — the body is eventually dropped, whether the download finished or the caller vanished
/// mid-stream — is also the thing that removes the merged file. There is no success path
/// and failure path to keep in sync.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A whole-extraction ceiling. Unlike the proxy's body, a `yt-dlp` run is a job that
/// should finish, so a wedged one must not hold a slot forever. Generous enough for a long
/// 1080p video on a slow link, short enough to reclaim a stuck process.
const MAX_EXTRACTION: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Distinguishes concurrent scratch dirs within one process-and-instant.
static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Deserialize)]
pub struct YoutubeQuery {
    url: Option<String>,
    /// The requested video height. Clamped to the configured ceiling; a request may ask
    /// for less than the cap but never more.
    height: Option<u32>,
}

/// The lowest height a request may ask for. Below this a "video" is a thumbnail; the value
/// only exists so a malformed `height=0` cannot select `bv*[height<=0]` and match nothing.
const MIN_HEIGHT: u32 = 144;

pub(crate) async fn youtube(
    State(relay): State<Arc<Relay>>,
    Query(query): Query<YoutubeQuery>,
) -> Response {
    let cfg = &relay.cfg.youtube;
    if !cfg.enabled {
        // Answer exactly as a relay that never had the route: an operator who has not
        // opted into running yt-dlp exposes no hint that the capability exists.
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let raw = query.url.unwrap_or_default();
    let Some(video) = canonical_watch_url(&raw) else {
        return (
            StatusCode::BAD_REQUEST,
            "url must be a YouTube watch link (youtube.com/watch, youtu.be/, or /shorts/)",
        )
            .into_response();
    };

    let height = clamp_height(query.height, cfg.max_height);
    let id = video_id(&video);

    // One slot per active extraction, held by the response body so it is released when the
    // stream ends or the caller hangs up — the same accounting the proxy uses.
    let Ok(permit) = Arc::clone(&relay.slots).try_acquire_owned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "relay busy").into_response();
    };

    // Per-request scratch dir, created now and owned by `Scratch` from here on so every
    // early return below deletes it.
    let scratch = match make_scratch() {
        Ok(dir) => Scratch(dir),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not create scratch dir: {e}"),
            )
                .into_response()
        }
    };
    let out_template = scratch.0.join("out.%(ext)s");

    let mut command = tokio::process::Command::new(&cfg.ytdlp_path);
    command
        .arg("--no-playlist")
        .arg("--no-cache-dir")
        .arg("--no-progress")
        .arg("--quiet")
        .arg("-f")
        .arg(format_selector(height))
        .arg("--merge-output-format")
        .arg("mp4")
        .arg("-o")
        .arg(&out_template)
        .arg(&video);
    if !cfg.js_runtime.is_empty() {
        command.arg("--js-runtimes").arg(&cfg.js_runtime);
    }
    if !cfg.cookies_file.is_empty() {
        command.arg("--cookies").arg(&cfg.cookies_file);
    }
    if !cfg.proxy.is_empty() {
        command.arg("--proxy").arg(&cfg.proxy);
    }
    command.stdin(Stdio::null()).kill_on_drop(true);

    eprintln!("youtube {id} height<={height} extracting");
    let output = match tokio::time::timeout(MAX_EXTRACTION, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            eprintln!("youtube {id} spawn-failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not start yt-dlp ({}): {e}", cfg.ytdlp_path),
            )
                .into_response();
        }
        Err(_) => {
            eprintln!("youtube {id} timed out after {}s", MAX_EXTRACTION.as_secs());
            return (StatusCode::GATEWAY_TIMEOUT, "extraction timed out").into_response();
        }
    };

    if !output.status.success() {
        // yt-dlp's own errors are prefixed "ERROR:"; surface the first, and log it.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr
            .lines()
            .find(|l| l.contains("ERROR"))
            .unwrap_or("yt-dlp failed")
            .trim();
        eprintln!("youtube {id} failed: {reason}");
        return (StatusCode::BAD_GATEWAY, format!("YouTube extraction failed: {reason}"))
            .into_response();
    }

    let Some(file) = merged_output(&scratch.0) else {
        eprintln!("youtube {id} produced no output file");
        return (StatusCode::BAD_GATEWAY, "extraction produced no file").into_response();
    };

    let handle = match tokio::fs::File::open(&file).await {
        Ok(f) => f,
        Err(e) => {
            eprintln!("youtube {id} cannot open output: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "cannot read merged file")
                .into_response();
        }
    };
    let len = handle.metadata().await.map(|m| m.len()).ok();
    eprintln!("youtube {id} streaming {} bytes", len.unwrap_or(0));

    // The file handle, the scratch guard and the permit ride together in the stream state,
    // so the file is streamed, then all three drop at once: the scratch dir is removed and
    // the slot freed.
    let state = StreamState {
        reader: ReaderStream::new(handle),
        _scratch: scratch,
        _permit: permit,
    };
    let body = Body::from_stream(futures_util::stream::unfold(state, next_chunk));

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"youtube-{id}.mp4\""),
        )
        .header(header::CACHE_CONTROL, "no-store");
    if let Some(len) = len {
        builder = builder.header(header::CONTENT_LENGTH, len);
    }
    builder
        .body(body)
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

struct StreamState {
    reader: ReaderStream<tokio::fs::File>,
    _scratch: Scratch,
    _permit: OwnedSemaphorePermit,
}

async fn next_chunk(
    mut state: StreamState,
) -> Option<(Result<axum::body::Bytes, std::io::Error>, StreamState)> {
    match state.reader.next().await {
        Some(Ok(chunk)) => Some((Ok(chunk), state)),
        Some(Err(e)) => Some((Err(e), state)),
        None => None,
    }
}

/// Create a fresh scratch directory under the system temp dir.
fn make_scratch() -> std::io::Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("dl-relay-yt-{}-{nanos}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The single merged file `yt-dlp` wrote into the scratch dir.
///
/// The output template is `out.%(ext)s`, so on a successful merge there is exactly one
/// `out.*`. Picking the largest guards against a stray sidecar (a `.json` info dump, say)
/// being chosen over the media.
fn merged_output(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .max_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
}

/// Clamp a requested height into `[MIN_HEIGHT, cap]`. A missing request means "the cap".
fn clamp_height(requested: Option<u32>, cap: u32) -> u32 {
    let cap = cap.max(MIN_HEIGHT);
    requested.unwrap_or(cap).clamp(MIN_HEIGHT, cap)
}

/// The `yt-dlp` format string for a height ceiling.
///
/// `bv*[height<=H]+ba` takes the best video at or below the ceiling plus the best audio
/// and merges them; the `/b[height<=H]` fallback covers a video that only offers a
/// pre-muxed stream. Capping here as well as in the caller means the relay never serves
/// above its tier even if the caller's own check is wrong or absent.
fn format_selector(height: u32) -> String {
    format!("bv*[height<={height}]+ba/b[height<={height}]")
}

/// The 11-character video id, for log lines and the download filename. Falls back to
/// `unknown` rather than logging the whole URL (which can carry a playlist and position).
fn video_id(watch_url: &str) -> String {
    url::Url::parse(watch_url)
        .ok()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "v")
                .map(|(_, v)| v.into_owned())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Accept only a YouTube watch link, and rewrite it to the canonical
/// `https://www.youtube.com/watch?v=<id>` form.
///
/// This is the whole security boundary of the route: `yt-dlp` is a general extractor for
/// a thousand sites and will happily fetch a `file://`, an intranet host, or any other URL
/// handed to it, so the relay must be certain it only ever receives a real YouTube video
/// before spawning it. Anything that is not a recognisable YouTube video id returns
/// `None`, and the route answers 400.
///
/// Recognised: `youtube.com/watch?v=ID`, `youtu.be/ID`, `youtube.com/shorts/ID`,
/// `youtube.com/embed/ID`, `m.` and `music.` subdomains. An id is the 11 URL-safe
/// characters YouTube uses; nothing else is treated as one.
pub fn canonical_watch_url(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw.trim()).ok()?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);

    let id = if host == "youtu.be" {
        url.path().trim_start_matches('/').to_string()
    } else if host == "youtube.com" || host == "m.youtube.com" || host == "music.youtube.com" {
        if url.path() == "/watch" {
            url.query_pairs()
                .find(|(k, _)| k == "v")
                .map(|(_, v)| v.into_owned())?
        } else {
            let mut segments = url.path_segments()?;
            match segments.next()? {
                "shorts" | "embed" | "v" | "live" => segments.next()?.to_string(),
                _ => return None,
            }
        }
    } else {
        return None;
    };

    if is_video_id(&id) {
        Some(format!("https://www.youtube.com/watch?v={id}"))
    } else {
        None
    }
}

/// A YouTube video id is exactly 11 characters from the URL-safe base64 alphabet.
fn is_video_id(candidate: &str) -> bool {
    candidate.len() == 11
        && candidate
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_watch_url_shape_canonicalises_to_the_same_thing() {
        let want = "https://www.youtube.com/watch?v=aqz-KE-bpKQ";
        for input in [
            "https://www.youtube.com/watch?v=aqz-KE-bpKQ",
            "https://youtube.com/watch?v=aqz-KE-bpKQ&list=PL123&index=2",
            "https://m.youtube.com/watch?v=aqz-KE-bpKQ",
            "https://music.youtube.com/watch?v=aqz-KE-bpKQ",
            "https://youtu.be/aqz-KE-bpKQ",
            "https://youtu.be/aqz-KE-bpKQ?t=42",
            "https://www.youtube.com/shorts/aqz-KE-bpKQ",
            "https://www.youtube.com/embed/aqz-KE-bpKQ",
            "  https://youtu.be/aqz-KE-bpKQ  ",
        ] {
            assert_eq!(
                canonical_watch_url(input).as_deref(),
                Some(want),
                "input: {input}"
            );
        }
    }

    #[test]
    fn anything_that_is_not_a_youtube_video_is_refused() {
        // The security boundary: yt-dlp must never be handed one of these.
        for input in [
            "https://example.com/watch?v=aqz-KE-bpKQ",
            "https://youtube.com.evil.test/watch?v=aqz-KE-bpKQ",
            "https://vimeo.com/12345",
            "file:///etc/passwd",
            "http://169.254.169.254/latest/meta-data/",
            "https://www.youtube.com/watch?v=short",           // id too short
            "https://www.youtube.com/watch?v=waytoolongforanid", // id too long
            "https://www.youtube.com/feed/subscriptions",      // not a video
            "https://www.youtube.com/@somechannel",            // a channel
            "not a url",
            "",
        ] {
            assert_eq!(canonical_watch_url(input), None, "input: {input}");
        }
    }

    #[test]
    fn a_channel_id_is_not_mistaken_for_a_video_id() {
        // 24-char channel ids start with UC and are the classic false positive.
        assert!(!is_video_id("UCXuqSBlHAE6Xw-yeJA0Tunw"));
        assert!(is_video_id("dQw4w9WgXcQ"));
    }

    #[test]
    fn the_height_cap_is_a_ceiling_not_a_default_only() {
        // Below the cap is honoured; above the cap is clamped down; absent means the cap.
        assert_eq!(clamp_height(Some(720), 1080), 720);
        assert_eq!(clamp_height(Some(2160), 1080), 1080);
        assert_eq!(clamp_height(None, 1080), 1080);
        // A degenerate request cannot select an empty format set.
        assert_eq!(clamp_height(Some(0), 1080), MIN_HEIGHT);
    }

    #[test]
    fn the_format_selector_carries_the_height_and_a_muxed_fallback() {
        let f = format_selector(1080);
        assert_eq!(f, "bv*[height<=1080]+ba/b[height<=1080]");
    }

    #[test]
    fn the_video_id_is_pulled_from_the_canonical_url() {
        assert_eq!(
            video_id("https://www.youtube.com/watch?v=aqz-KE-bpKQ"),
            "aqz-KE-bpKQ"
        );
    }
}
