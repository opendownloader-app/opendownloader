//! A local media server for exercising the download engine deterministically.
//!
//! Everything it serves is generated from `dl_container::fixtures`, the same code
//! the unit tests build their input from — so an end-to-end run and a `cargo test`
//! run assert against byte-identical media, and neither can drift from the other.
//!
//! It exists to reproduce the conditions that are awkward to find in the wild and
//! impossible to find on demand: a server that refuses ranges, one whose ETag
//! changes mid-download, one that truncates a response it promised, one that
//! returns 503 a few times before relenting.
//!
//! Hand-rolled over `std::net` rather than pulling in an HTTP framework. The
//! surface is a dozen routes and one header parser; a dependency here would be
//! larger than the thing it replaced.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dl_container::fixtures;
use sha2::{Digest, Sha256};

/// Segments per HLS variant.
const SEGMENT_COUNT: usize = 4;
/// 90 kHz ticks per segment; matches the two-frame fixture's duration.
const SEGMENT_TICKS: u64 = 6000;

/// WebVTT subtitle segments per subtitle rendition.
const SUBTITLE_SEGMENTS: usize = 3;
/// Seconds each subtitle segment covers. Longer than a media segment on purpose:
/// subtitle renditions are routinely packaged at a coarser cadence than video.
const SUBTITLE_SEGMENT_SECONDS: u64 = 6;
/// The 90 kHz presentation-time origin every fixture stream starts at. Ten seconds
/// in, so any code that forgets to subtract the origin produces a visibly wrong
/// result rather than a coincidentally correct one.
const PTS_ORIGIN: u64 = 900_000;

/// AAC frames in the generated progressive MP4, and samples per `stsc` chunk.
const MP4_FRAMES: usize = 12;
const MP4_SAMPLES_PER_CHUNK: u32 = 4;

/// Per-path request counters, which is what makes "fail the first K times" and
/// "change the ETag after the first request" possible.
type Counters = Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>>;

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(0);
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("failed to bind");
    let bound = listener.local_addr().expect("no local addr");

    // Printed on its own line and flushed so a test harness can parse the port
    // out of stdout rather than guessing or hardcoding one.
    println!("listening on http://{bound}");
    let _ = std::io::stdout().flush();

    let counters: Counters = Arc::new(Mutex::new(HashMap::new()));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let counters = Arc::clone(&counters);
        std::thread::spawn(move || {
            // A panic in one connection must not take the server down mid-suite.
            let _ = handle(stream, counters);
        });
    }
}

struct Request {
    /// The full request target, path and query. Used as the counter key so two
    /// scenarios exercising the same path with different parameters — `fail=2` and
    /// `flip=1`, say — do not consume each other's request counts.
    target: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
}

fn handle(mut stream: TcpStream, counters: Counters) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }

    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut headers = HashMap::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let (path, query) = split_query(&target);

    // A CDN that serves plain cross-origin GETs and refuses every preflight: Twitch's
    // CloudFront answers `OPTIONS` with a bare 403 while the same URL's GET carries
    // `Access-Control-Allow-Origin: *`. A client that adds one non-safelisted header to a
    // chunk request fails there before a byte arrives (APP-82). Only when asked, so the
    // preflights every other test makes still succeed and still count as they did.
    if method == "OPTIONS" && query.get("no_preflight").map(String::as_str) == Some("1") {
        return send_without_cors(&mut stream, 403, "text/plain", b"preflight refused");
    }

    let req = Request {
        target: target.clone(),
        path: path.to_string(),
        query,
        headers,
    };
    route(&mut stream, &req, &counters)
}

fn split_query(target: &str) -> (&str, HashMap<String, String>) {
    let (path, raw) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let mut query = HashMap::new();
    for pair in raw.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(k.to_string(), v.to_string());
    }
    (path, query)
}

fn counter(counters: &Counters, key: &str) -> Arc<AtomicUsize> {
    let mut map = counters.lock().expect("counter mutex poisoned");
    Arc::clone(
        map.entry(key.to_string())
            .or_insert_with(|| Arc::new(AtomicUsize::new(0))),
    )
}

fn route(out: &mut TcpStream, req: &Request, counters: &Counters) -> std::io::Result<()> {
    match req.path.as_str() {
        "/" | "/page.html" => send(
            out,
            200,
            "text/html; charset=utf-8",
            &[],
            test_page().as_bytes(),
        ),

        // The progressive-download workhorse. See `serve_fixture` for the knobs.
        "/fixture.bin" => serve_fixture(out, req, counters, "application/octet-stream"),
        "/fixture.mp4" => serve_fixture(out, req, counters, "video/mp4"),

        // The expected digest, so a test can assert the downloaded file matches
        // without reimplementing the generator on the JavaScript side.
        "/fixture.sha256" => {
            let size = usize_param(req, "size", 1 << 20);
            let digest = hex(&Sha256::digest(fixtures::progressive_bytes(size)));
            send(out, 200, "text/plain", &[], digest.as_bytes())
        }

        // The digest the browser's remuxed output must match. Computed by running
        // the same remuxer natively over the same segments, so an end-to-end run
        // asserts byte equality with the Rust implementation rather than merely
        // "produced something MP4-shaped".
        "/hls/expected.sha256" => {
            let mut remuxer = dl_container::Remuxer::new();
            let mut hasher = Sha256::new();
            for i in 0..SEGMENT_COUNT as u64 {
                let segment = fixtures::ts_segment(900_000 + i * SEGMENT_TICKS);
                match remuxer.push_ts_segment(&segment) {
                    Ok(out) => hasher.update(&out),
                    Err(e) => {
                        return send(out, 500, "text/plain", &[], e.to_string().as_bytes());
                    }
                }
            }
            let digest = hex(&hasher.finalize());
            send(out, 200, "text/plain", &[], digest.as_bytes())
        }

        // A master that declares alternate audio and two subtitle renditions, so
        // the `#EXT-X-MEDIA` path has something real to parse. The plain master
        // above deliberately keeps none, so the two cases stay distinguishable.
        "/hls/master-alt.m3u8" => send(
            out,
            200,
            "application/vnd.apple.mpegurl",
            &[],
            master_playlist_with_alternates().as_bytes(),
        ),

        // The alternate audio rendition. Its segments are the same transport
        // streams the video variants use: the extractor drops the video track,
        // so what matters here is that the rendition URL resolves and downloads,
        // not that the bytes differ from the muxed case.
        "/hls/audio-en.m3u8" => send(
            out,
            200,
            "application/vnd.apple.mpegurl",
            &[],
            media_playlist("low").as_bytes(),
        ),

        "/hls/subs-en.m3u8" => send(
            out,
            200,
            "application/vnd.apple.mpegurl",
            &[],
            subtitle_playlist("en").as_bytes(),
        ),
        "/hls/subs-de.m3u8" => send(
            out,
            200,
            "application/vnd.apple.mpegurl",
            &[],
            subtitle_playlist("de").as_bytes(),
        ),

        // The merged documents a correct client must produce. Hand-checked
        // literals rather than a computation: this crate deliberately does not
        // depend on `dl-core`, so asserting against its own merge would only
        // prove the merge agrees with itself.
        "/hls/subs-en.expected.srt" => {
            send(out, 200, "text/plain", &[], EXPECTED_EN_SRT.as_bytes())
        }
        "/hls/subs-de.expected.srt" => {
            send(out, 200, "text/plain", &[], EXPECTED_DE_SRT.as_bytes())
        }

        p if p.starts_with("/hls/subs-") && p.ends_with(".vtt") => {
            let name = p.trim_start_matches("/hls/subs-").trim_end_matches(".vtt");
            let (lang, index) = name.split_at(2);
            let index: usize = index.parse().unwrap_or(0);
            match subtitle_segment(lang, index) {
                Some(body) => send(out, 200, "text/vtt", &[], body.as_bytes()),
                None => send(out, 404, "text/plain", &[], b"no such subtitle segment"),
            }
        }

        // A real progressive MP4, as opposed to `/fixture.bin` renamed. The audio
        // chunks are interleaved with video chunks inside the `mdat`, so an
        // extractor that assumed the media data was all audio would produce
        // plausible-looking garbage rather than failing.
        "/media.mp4" => {
            let frames = usize_param(req, "frames", MP4_FRAMES);
            serve_bytes(out, req, "video/mp4", &progressive_mp4(frames))
        }

        // The digest of the M4A a correct extraction produces, computed here by
        // running the native extractor over the very bytes served above.
        "/media.mp4.m4a.sha256" => {
            let frames = usize_param(req, "frames", MP4_FRAMES);
            match extracted_audio(frames) {
                Ok(bytes) => send(
                    out,
                    200,
                    "text/plain",
                    &[],
                    hex(&Sha256::digest(bytes)).as_bytes(),
                ),
                Err(e) => send(out, 500, "text/plain", &[], e.as_bytes()),
            }
        }

        // The other half of the CORS story: a resource a web page cannot read.
        // The extension downloads it fine; the standalone site must explain why
        // it cannot, rather than reporting a bare network error.
        "/no-cors.bin" => {
            let size = usize_param(req, "size", 1 << 16);
            let body = fixtures::progressive_bytes(size);
            send_without_cors(out, 200, "application/octet-stream", &body)
        }

        "/page-alt.html" => send(
            out,
            200,
            "text/html; charset=utf-8",
            &[],
            alt_test_page().as_bytes(),
        ),

        "/hls/master.m3u8" => send(
            out,
            200,
            "application/vnd.apple.mpegurl",
            &[],
            master_playlist().as_bytes(),
        ),
        "/hls/encrypted.m3u8" => send(
            out,
            200,
            "application/vnd.apple.mpegurl",
            &[],
            encrypted_playlist().as_bytes(),
        ),
        "/hls/live.m3u8" => send(
            out,
            200,
            "application/vnd.apple.mpegurl",
            &[],
            live_playlist().as_bytes(),
        ),

        p if p.starts_with("/hls/") && p.ends_with("/index.m3u8") => {
            let variant = p
                .trim_start_matches("/hls/")
                .trim_end_matches("/index.m3u8");
            send(
                out,
                200,
                "application/vnd.apple.mpegurl",
                &[],
                media_playlist(variant).as_bytes(),
            )
        }

        p if p.starts_with("/hls/") && p.ends_with(".ts") => {
            let index: u64 = p
                .rsplit('/')
                .next()
                .and_then(|f| {
                    f.trim_start_matches("seg")
                        .trim_end_matches(".ts")
                        .parse()
                        .ok()
                })
                .unwrap_or(0);
            // Deliberately offset so the output timeline must be normalised: a
            // remuxer that writes source timestamps verbatim produces a file
            // starting ten seconds in, which this catches.
            let segment = fixtures::ts_segment(900_000 + index * SEGMENT_TICKS);
            send(out, 200, "video/mp2t", &[], &segment)
        }

        _ => send(out, 404, "text/plain", &[], b"not found"),
    }
}

/// Serve the generated fixture, with every failure mode the engine claims to handle.
///
/// Query parameters:
/// - `size`     total bytes (default 1 MiB)
/// - `ranges=0` refuse range requests entirely, always answering 200 with the whole body
/// - `flip=1`   change the ETag after the first request, so a resume must be rejected
/// - `fail=K`   answer 503 with `Retry-After: 1` for the first K requests
/// - `truncate=1` promise the full length but send half and hang up
/// - `truncate_first=K` do that to the first K responses only, then serve normally —
///   which is the shape a reader has to survive: a connection cut part-way that
///   succeeds when the missing tail is asked for again
/// - `no_preflight=1` refuse CORS preflights with a bare 403, as Twitch's CloudFront
///   does, while GETs stay readable from any origin
/// - `serve_to=N` answer `206` for a range starting below N and `403` for any range at
///   or beyond it, which is what Google's media addresses do — they serve to about
///   1.1 MB and refuse the rest, whatever the range size or the order asked in
fn serve_fixture(
    out: &mut TcpStream,
    req: &Request,
    counters: &Counters,
    content_type: &str,
) -> std::io::Result<()> {
    let size = usize_param(req, "size", 1 << 20);
    let ranges_enabled = req.query.get("ranges").map(String::as_str) != Some("0");
    let hits = counter(counters, &req.target).fetch_add(1, Ordering::SeqCst);

    let fail_times = usize_param(req, "fail", 0);
    if hits < fail_times {
        return send(
            out,
            503,
            "text/plain",
            &[("Retry-After", "1")],
            b"try again shortly",
        );
    }

    let body = fixtures::progressive_bytes(size);
    // A changed ETag is how a resumed download discovers the file it was fetching
    // is not the file that is there now.
    let etag = if req.query.get("flip").map(String::as_str) == Some("1") && hits > 0 {
        "\"v2\""
    } else {
        "\"v1\""
    };

    // Always, or only for the opening responses. The second form is what a flaky CDN
    // looks like: the retry is expected to succeed, so a reader that resumes finishes
    // and one that treats a short read as fatal does not.
    let truncate = req.query.get("truncate").map(String::as_str) == Some("1")
        || hits < usize_param(req, "truncate_first", 0);
    let range = req.headers.get("range").and_then(|r| parse_range(r, size));

    // `If-Range` that no longer matches means the client must be given the whole
    // resource, not the range it asked for — that is the signal to restart.
    let if_range_stale = req.headers.get("if-range").is_some_and(|v| v != etag);

    if !ranges_enabled || range.is_none() || if_range_stale {
        let headers: &[(&str, &str)] = if ranges_enabled {
            &[("Accept-Ranges", "bytes")]
        } else {
            &[]
        };
        let mut headers = headers.to_vec();
        headers.push(("ETag", etag));
        return send_maybe_truncated(out, 200, content_type, &headers, &body, truncate);
    }

    let (start, end) = range.expect("checked above");

    // A host that serves the beginning of a file and refuses every later offset. Real
    // enough to matter: it is the shape a download hits on Google's media addresses, and
    // the engine has to end such a job without offering to resume it.
    if let Some(limit) = req.query.get("serve_to").and_then(|v| v.parse().ok()) {
        if start >= limit {
            return send(out, 403, "text/plain", &[], b"forbidden");
        }
    }

    let slice = &body[start..=end];
    let content_range = format!("bytes {start}-{end}/{size}");
    let headers = vec![
        ("Accept-Ranges", "bytes"),
        ("ETag", etag),
        ("Content-Range", content_range.as_str()),
    ];
    send_maybe_truncated(out, 206, content_type, &headers, slice, truncate)
}

fn usize_param(req: &Request, key: &str, default: usize) -> usize {
    req.query
        .get(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Parse `bytes=start-end`, clamped to the resource, supporting the open-ended form.
fn parse_range(header: &str, size: usize) -> Option<(usize, usize)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: usize = start.trim().parse().ok()?;
    if start >= size {
        return None;
    }
    let end = match end.trim() {
        "" => size - 1,
        e => e.parse::<usize>().ok()?.min(size - 1),
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

fn send(
    out: &mut TcpStream,
    status: u16,
    content_type: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    send_maybe_truncated(out, status, content_type, extra, body, false)
}

/// Cross-origin headers, sent on everything.
///
/// Media CDNs routinely permit cross-origin reads, and without that the
/// standalone web app cannot fetch anything at all — its `fetch` is subject to
/// CORS in a way the extension's host permissions exempt it from. Sending these
/// is therefore what makes the web app testable against this server; the
/// *absence* of them is a real scenario too, and `/no-cors.bin` exists to
/// reproduce it.
const CORS_HEADERS: &[(&str, &str)] = &[
    ("Access-Control-Allow-Origin", "*"),
    ("Access-Control-Allow-Headers", "range, if-range"),
    (
        "Access-Control-Expose-Headers",
        "content-range, accept-ranges, etag, content-length",
    ),
];

/// Send a response with no cross-origin headers at all.
fn send_without_cors(
    out: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        body.len()
    );
    out.write_all(head.as_bytes())?;
    out.write_all(body)?;
    out.flush()
}

fn send_maybe_truncated(
    out: &mut TcpStream,
    status: u16,
    content_type: &str,
    extra: &[(&str, &str)],
    body: &[u8],
    truncate: bool,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in CORS_HEADERS.iter().chain(extra) {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;

    if truncate {
        // Declare the full length, deliver half, hang up. This is what a dropped
        // connection looks like to a client, and it is what the retry path exists for.
        out.write_all(&body[..body.len() / 2])?;
    } else {
        out.write_all(body)?;
    }
    out.flush()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn master_playlist() -> String {
    // Two renditions, so variant selection has something real to choose between.
    "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.42c01e,mp4a.40.2\"\n\
low/index.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS=\"avc1.42c01e,mp4a.40.2\"\n\
high/index.m3u8\n"
        .to_string()
}

fn media_playlist(variant: &str) -> String {
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n");
    for i in 0..SEGMENT_COUNT {
        out.push_str("#EXTINF:0.067,\n");
        out.push_str(&format!("/hls/{variant}/seg{i}.ts\n"));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

fn encrypted_playlist() -> String {
    // Must be refused outright rather than downloaded, and refusing it is a policy
    // decision the engine should make without ever fetching the key.
    "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n\
#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/key.bin\"\n\
#EXTINF:0.067,\n/hls/low/seg0.ts\n#EXT-X-ENDLIST\n"
        .to_string()
}

fn live_playlist() -> String {
    // No #EXT-X-ENDLIST: there is no end to download to.
    "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n\
#EXTINF:0.067,\n/hls/low/seg0.ts\n"
        .to_string()
}

// ---------------------------------------------------------------------------
// Alternate renditions, subtitles, and a real progressive MP4
// ---------------------------------------------------------------------------

fn master_playlist_with_alternates() -> String {
    // Relative URIs, resolved against the playlist's own URL — the case that
    // catches a resolver which only handles absolute paths.
    "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio-en.m3u8\"\n\
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI=\"subs-en.m3u8\"\n\
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Deutsch\",LANGUAGE=\"de\",DEFAULT=NO,AUTOSELECT=YES,FORCED=NO,URI=\"subs-de.m3u8\"\n\
#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,GROUP-ID=\"cc\",NAME=\"CC1\",LANGUAGE=\"en\",INSTREAM-ID=\"CC1\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.42c01e,mp4a.40.2\",AUDIO=\"aud\",SUBTITLES=\"subs\",CLOSED-CAPTIONS=\"cc\"\n\
low/index.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS=\"avc1.42c01e,mp4a.40.2\",AUDIO=\"aud\",SUBTITLES=\"subs\",CLOSED-CAPTIONS=\"cc\"\n\
high/index.m3u8\n"
        .to_string()
}

fn subtitle_playlist(lang: &str) -> String {
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n");
    for i in 0..SUBTITLE_SEGMENTS {
        out.push_str(&format!("#EXTINF:{SUBTITLE_SEGMENT_SECONDS}.000,\n"));
        out.push_str(&format!("/hls/subs-{lang}{i}.vtt\n"));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// One WebVTT segment, in one of the two packagings that exist in the wild.
///
/// The distinction is the whole reason merging is not concatenation, so both are
/// served and each rendition commits to one of them:
///
/// - **`en` — per-segment timestamp maps, segment-relative cues.** Every segment
///   restarts its clock at zero and carries an `X-TIMESTAMP-MAP` saying where in
///   the presentation it belongs. Concatenating these produces a track whose
///   subtitles all pile up in the first six seconds.
/// - **`de` — one constant timestamp map, absolute cues.** Nothing needs
///   shifting, but a cue that straddles a segment boundary is written into both
///   segments, so concatenating produces duplicates.
///
/// Both maps are anchored at [`PTS_ORIGIN`], matching the media segments: a
/// merger that used the raw MPEGTS value instead of the offset from the first
/// segment would put every cue ten seconds late.
fn subtitle_segment(lang: &str, index: usize) -> Option<String> {
    if index >= SUBTITLE_SEGMENTS {
        return None;
    }
    let start = index as u64 * SUBTITLE_SEGMENT_SECONDS;

    match lang {
        "en" => {
            let mpegts = PTS_ORIGIN + index as u64 * SUBTITLE_SEGMENT_SECONDS * 90_000;
            Some(format!(
                "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:{mpegts}\n\n\
NOTE This block exists to be skipped by the parser.\n\n\
{index}a\n00:00:00.000 --> 00:00:03.000\nEnglish {index} a\n\n\
00:00:03.000 --> 00:00:06.000\nEnglish {index} b\n"
            ))
        }
        "de" => {
            // The same map on every segment, and cues already in presentation time.
            let half = SUBTITLE_SEGMENT_SECONDS / 2;

            // Every segment after the first repeats the cue it shares with its
            // predecessor, byte for byte.
            let mut cues = String::new();
            if index > 0 {
                cues.push_str(&cue(
                    (start - half) * 1000,
                    start * 1000,
                    &format!("Deutsch grenze {}", index - 1),
                ));
            }
            cues.push_str(&cue(
                start * 1000,
                (start + half) * 1000,
                &format!("Deutsch {index}"),
            ));
            cues.push_str(&cue(
                (start + half) * 1000,
                (start + SUBTITLE_SEGMENT_SECONDS) * 1000,
                &format!("Deutsch grenze {index}"),
            ));

            Some(format!(
                "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:{PTS_ORIGIN},LOCAL:00:00:00.000\n\n{cues}"
            ))
        }
        _ => None,
    }
}

/// One WebVTT cue block, trailing blank line included.
fn cue(start_ms: u64, end_ms: u64, text: &str) -> String {
    format!(
        "{} --> {}\n{text}\n\n",
        vtt_time(start_ms),
        vtt_time(end_ms)
    )
}

/// `HH:MM:SS.mmm`, the only timestamp form WebVTT accepts above an hour.
fn vtt_time(ms: u64) -> String {
    let (h, m, s, milli) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    format!("{h:02}:{m:02}:{s:02}.{milli:03}")
}

/// The English rendition, merged and converted. Hand-checked, and the reason it
/// can be: each segment contributes exactly two cues, shifted by six seconds per
/// segment, with nothing to de-duplicate.
const EXPECTED_EN_SRT: &str = "1\n00:00:00,000 --> 00:00:03,000\nEnglish 0 a\n\n\
2\n00:00:03,000 --> 00:00:06,000\nEnglish 0 b\n\n\
3\n00:00:06,000 --> 00:00:09,000\nEnglish 1 a\n\n\
4\n00:00:09,000 --> 00:00:12,000\nEnglish 1 b\n\n\
5\n00:00:12,000 --> 00:00:15,000\nEnglish 2 a\n\n\
6\n00:00:15,000 --> 00:00:18,000\nEnglish 2 b\n\n";

/// The German rendition, merged and converted. Six cues are served across the
/// three segments and two of them are duplicates of their predecessor, so a
/// correct merge yields four.
const EXPECTED_DE_SRT: &str = "1\n00:00:00,000 --> 00:00:03,000\nDeutsch 0\n\n\
2\n00:00:03,000 --> 00:00:06,000\nDeutsch grenze 0\n\n\
3\n00:00:06,000 --> 00:00:09,000\nDeutsch 1\n\n\
4\n00:00:09,000 --> 00:00:12,000\nDeutsch grenze 1\n\n\
5\n00:00:12,000 --> 00:00:15,000\nDeutsch 2\n\n\
6\n00:00:15,000 --> 00:00:18,000\nDeutsch grenze 2\n\n";

/// Deterministic AAC payloads, one per frame.
///
/// A cheap integer hash rather than a counter, for the same reason
/// `fixtures::progressive_bytes` uses one: a repeating pattern would let a
/// misordered or duplicated chunk still produce plausible-looking output, which
/// is precisely the failure an extraction test needs to catch.
fn aac_frames(count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| {
            (0..16u64)
                .map(|j| {
                    let x = (i as u64 * 16 + j).wrapping_mul(2_654_435_761);
                    (x >> 13) as u8
                })
                .collect()
        })
        .collect()
}

/// A progressive MP4 whose audio chunks are interleaved with video chunks.
fn progressive_mp4(frames: usize) -> Vec<u8> {
    dl_container::mp4::builder::progressive_mp4(
        &aac_frames(frames),
        44_100,
        2,
        MP4_SAMPLES_PER_CHUNK,
        true,
    )
}

/// Extract that MP4's audio with the native extractor, the same way the browser does.
fn extracted_audio(frames: usize) -> Result<Vec<u8>, String> {
    let file = progressive_mp4(frames);
    let moov = find_box(&file, b"moov").ok_or("generated MP4 has no moov")?;
    let mut extractor = dl_container::AudioExtractor::from_moov(&file[moov.0..moov.1])
        .map_err(|e| e.to_string())?;
    let chunks: Vec<dl_container::ChunkPlan> = extractor.chunks().to_vec();

    let mut out = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let start = chunk.offset as usize;
        let end = start + chunk.len as usize;
        let bytes = file
            .get(start..end)
            .ok_or("chunk plan points past the file")?;
        out.extend(extractor.push_chunk(i, bytes).map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// Byte range of a top-level box, header included.
fn find_box(file: &[u8], kind: &[u8; 4]) -> Option<(usize, usize)> {
    let mut offset = 0;
    while offset + 8 <= file.len() {
        let header = dl_container::box_header(&file[offset..])?;
        let size = if header.size == 0 {
            (file.len() - offset) as u64
        } else {
            header.size
        } as usize;
        if &header.kind == kind {
            return Some((offset, offset + size));
        }
        if size < 8 {
            return None;
        }
        offset += size;
    }
    None
}

/// Serve a fixed body with full range support, so a client can read slices of it.
fn serve_bytes(
    out: &mut TcpStream,
    req: &Request,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let size = body.len();
    match req.headers.get("range").and_then(|r| parse_range(r, size)) {
        Some((start, end)) => {
            let content_range = format!("bytes {start}-{end}/{size}");
            let headers = vec![
                ("Accept-Ranges", "bytes"),
                ("Content-Range", content_range.as_str()),
            ];
            send(out, 206, content_type, &headers, &body[start..=end])
        }
        None => send(out, 200, content_type, &[("Accept-Ranges", "bytes")], body),
    }
}

/// A page loading the alternate-rendition master and the real MP4, for the sniffer.
fn alt_test_page() -> String {
    "<!doctype html><meta charset=\"utf-8\"><title>opendownloader alternates</title>\n\
<h1>alternate renditions</h1>\n\
<video id=\"v\" src=\"/media.mp4\" controls></video>\n\
<script>\n\
  Promise.all([\n\
    fetch('/hls/master-alt.m3u8').then(r => r.text()),\n\
    fetch('/media.mp4', { headers: { Range: 'bytes=0-0' } }),\n\
  ]).then(() => { document.title = 'ready'; });\n\
</script>\n"
        .to_string()
}

/// A page that loads media the sniffer should detect, plus links it should ignore.
fn test_page() -> String {
    "<!doctype html><meta charset=\"utf-8\"><title>opendownloader test page</title>\n\
<h1>test page</h1>\n\
<video id=\"v\" src=\"/fixture.mp4?size=65536\" controls></video>\n\
<p><a id=\"hls\" href=\"/hls/master.m3u8\">master playlist</a></p>\n\
<script>\n\
  // Fetching the playlist is what makes it observable to a webRequest listener;\n\
  // a bare <a href> is never requested and so is never a download candidate.\n\
  fetch('/hls/master.m3u8').then(r => r.text()).then(() => {\n\
    document.title = 'ready';\n\
  });\n\
</script>\n"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_english_subtitle_segment_carries_its_own_timestamp_map() {
        // Six seconds apart at 90 kHz, anchored on the same origin the media
        // segments use. A merger that ignored these would stack all nine cues on
        // top of each other in the first six seconds.
        for i in 0..SUBTITLE_SEGMENTS {
            let body = subtitle_segment("en", i).expect("segment exists");
            let expected = PTS_ORIGIN + i as u64 * SUBTITLE_SEGMENT_SECONDS * 90_000;
            assert!(
                body.contains(&format!("MPEGTS:{expected}")),
                "segment {i} should map to {expected}, got:\n{body}"
            );
            assert!(body.starts_with("WEBVTT"));
        }
    }

    #[test]
    fn the_german_segments_share_one_map_and_repeat_the_boundary_cue() {
        let first = subtitle_segment("de", 0).expect("segment exists");
        let second = subtitle_segment("de", 1).expect("segment exists");
        assert!(first.contains(&format!("MPEGTS:{PTS_ORIGIN}")));
        assert!(second.contains(&format!("MPEGTS:{PTS_ORIGIN}")));

        // The cue the two segments share must appear in both, byte for byte —
        // that is what makes de-duplication necessary rather than decorative.
        let shared = "00:00:03.000 --> 00:00:06.000\nDeutsch grenze 0";
        assert!(first.contains(shared), "first segment:\n{first}");
        assert!(second.contains(shared), "second segment:\n{second}");
    }

    #[test]
    fn a_subtitle_segment_past_the_end_is_absent_rather_than_empty() {
        assert!(subtitle_segment("en", SUBTITLE_SEGMENTS).is_none());
        assert!(subtitle_segment("fr", 0).is_none());
    }

    #[test]
    fn the_expected_documents_describe_what_the_segments_actually_contain() {
        // Every cue the segments serve must appear in the expected SRT, and the
        // German one must be shorter than the sum of its parts because two of
        // its cues are duplicates.
        for i in 0..SUBTITLE_SEGMENTS {
            assert!(EXPECTED_EN_SRT.contains(&format!("English {i} a")));
            assert!(EXPECTED_EN_SRT.contains(&format!("English {i} b")));
            assert!(EXPECTED_DE_SRT.contains(&format!("Deutsch {i}")));
        }
        assert_eq!(EXPECTED_EN_SRT.matches(" --> ").count(), 6);
        assert_eq!(EXPECTED_DE_SRT.matches(" --> ").count(), 6);
        // Nine German cues are served across three segments; three are repeats.
        let served: usize = (0..SUBTITLE_SEGMENTS)
            .map(|i| subtitle_segment("de", i).unwrap().matches(" --> ").count())
            .sum();
        assert_eq!(served, 8, "two boundary cues are served twice");
        // Both start at zero: the timestamp-map origin must cancel out.
        assert!(EXPECTED_EN_SRT.starts_with("1\n00:00:00,000"));
        assert!(EXPECTED_DE_SRT.starts_with("1\n00:00:00,000"));
    }

    #[test]
    fn the_master_playlist_with_alternates_declares_both_subtitle_renditions() {
        let m = master_playlist_with_alternates();
        assert_eq!(m.matches("TYPE=SUBTITLES").count(), 2);
        assert_eq!(m.matches("TYPE=AUDIO").count(), 1);
        // A closed-captions entry has no URI and must be ignorable rather than
        // parsed as a downloadable track.
        assert!(m.contains("TYPE=CLOSED-CAPTIONS"));
        assert!(m.contains("AUDIO=\"aud\""));
        assert!(m.contains("SUBTITLES=\"subs\""));
    }

    #[test]
    fn the_generated_mp4_is_a_real_mp4_with_a_findable_moov() {
        let file = progressive_mp4(MP4_FRAMES);
        assert_eq!(&file[4..8], b"ftyp");
        let moov = find_box(&file, b"moov").expect("moov must be locatable");
        assert!(moov.1 > moov.0);
        assert!(find_box(&file, b"mdat").is_some());
    }

    #[test]
    fn extracting_the_audio_is_deterministic_and_lossless() {
        let first = extracted_audio(MP4_FRAMES).expect("extraction succeeds");
        let second = extracted_audio(MP4_FRAMES).expect("extraction succeeds");
        assert_eq!(first, second, "the digest route would be useless otherwise");
        assert_eq!(&first[4..8], b"ftyp");

        // Every AAC payload must survive into the output unchanged: the point of
        // this path is that nothing is re-encoded.
        for frame in aac_frames(MP4_FRAMES) {
            assert!(
                first.windows(frame.len()).any(|w| w == frame),
                "an input frame did not reach the extracted audio"
            );
        }
    }

    #[test]
    fn the_extraction_follows_the_chunk_offsets_rather_than_assuming_contiguity() {
        // The builder interleaves video chunks between the audio ones, so an
        // extractor that walked the mdat linearly would pick up video bytes.
        let file = progressive_mp4(MP4_FRAMES);
        let moov = find_box(&file, b"moov").unwrap();
        let extractor = dl_container::AudioExtractor::from_moov(&file[moov.0..moov.1]).unwrap();
        let plans = extractor.chunks();
        assert!(plans.len() > 1);
        let contiguous = plans
            .windows(2)
            .all(|w| w[0].offset + w[0].len == w[1].offset);
        assert!(
            !contiguous,
            "the fixture must not have contiguous audio chunks"
        );
    }

    #[test]
    fn ranges_are_clamped_to_the_resource() {
        assert_eq!(parse_range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=0-500", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=200-300", 100), None);
        assert_eq!(parse_range("nonsense", 100), None);
    }

    #[test]
    fn vtt_timestamps_are_written_in_the_long_form() {
        assert_eq!(vtt_time(0), "00:00:00.000");
        assert_eq!(vtt_time(3_000), "00:00:03.000");
        assert_eq!(vtt_time(3_723_456), "01:02:03.456");
    }
}
