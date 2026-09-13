//! Bilibili — `bilibili.com`, `b23.tv`, and the `bilivideo.com` CDN.
//!
//! # The shape this was built against
//!
//! A watch page hands its player the complete DASH manifest inline, as
//! `window.__playinfo__ = {"code":0,…,"data":{"dash":{"video":[…],"audio":[…]}}}` in an
//! ordinary `<script>`. Older and some region-limited pages carry `data.durl[]` instead —
//! a muxed FLV/MP4 list rather than separate video and audio adaptations. Both are
//! handled.
//!
//! **Honesty about provenance:** this was written on 2026-09-05 against that documented
//! shape and is *unverified against a live page*. Bilibili answers `412` to a fetch that
//! does not come from a logged-in browser, so no real fixture could be captured; the
//! tests below assert against hand-written samples of the shape. When bilibili changes
//! it, this fails as [`SiteError::Shape`] carrying the site name, which is the bug report.
//!
//! # The page first, the API as the fallback
//!
//! The page the viewer is already looking at contains the manifest, because the player
//! needed it to play — and it is the *better* source, since it carries whatever that
//! logged-in session is entitled to. So [`Need::PageState`] is still the first ask.
//!
//! But that page only carries it when it is read from inside the tab. Fetched from
//! outside — by the relay for the web app, or by the extension with no tab open — the
//! same HTML arrives with `window.__playinfo__` referenced by the player bootstrap and
//! never assigned, because the player fetches the manifest itself. So when the manifest
//! is missing, this reads the `bvid`/`cid` the page *does* carry and asks
//! `x/player/playurl` for it directly. The signed `wbi` variant needs a daily-rotating
//! key; the plain one still answers, anonymously, capped at the lower renditions. A
//! 480p answer beats refusing the link.

use super::{
    host_is, safe_filename, AudioChoice, Extraction, Extractor, MediaOption, Need, Request,
    SiteError, Step, Stream, StreamKind, VideoChoice,
};
use serde_json::Value;

pub fn matches(host: &str) -> bool {
    host_is(host, "bilibili.com") || host_is(host, "b23.tv") || host_is(host, "bilivideo.com")
}

const SITE: &str = "Bilibili";

fn shape() -> SiteError {
    SiteError::Shape(SITE.to_string())
}

/// The watch page named no video at all: no state object, no manifest, not even the
/// bvid from its own URL.
///
/// Not [`shape`], because in practice that is rarely what it is. A tab read before the
/// player has finished loading, a risk-control interstitial, a page that was never a
/// video — all of them land here, and every one is cured by loading the page again.
/// "The site has probably changed" sent the first-visit failure in APP-80 to the wrong
/// conclusion: it reads as the extension being broken, and nobody reloads a broken
/// extension. The site changing is still named, as what it means if a reload does not help.
fn unreadable_page() -> SiteError {
    SiteError::Unavailable(format!(
        "{SITE}'s page did not say which video it is. Reload the video page and try \
         again — if it still fails after a reload, {SITE} has changed its page and \
         OpenDownloader needs an update."
    ))
}

// ---------------------------------------------------------------------------
// Shared HTML-scraping helpers
//
// Every page-state extractor in this directory needs the same handful of primitives:
// lift a JSON value out of a `<script>` by brace matching, undo JSON string escaping,
// undo HTML entity escaping, read one attribute. They live here, in the first site that
// needs them, and the other four import them — five subtly different brace matchers is
// precisely the class of bug this avoids. They are not in `sites/mod.rs` because that
// file holds the seam types and the registry, and nothing else.
// ---------------------------------------------------------------------------

/// Extract the JSON object that follows `marker`, braces included.
///
/// A regex cannot do this — balanced braces are not a regular language — and a naive
/// `find('}')` is an outright bug: every one of these pages contains `}` inside a string
/// literal (a caption, a CSS blob, a URL with a template fragment) long before the object
/// ends. So the scan tracks whether it is inside a string and whether the previous
/// character was a backslash, and only counts braces that are outside a string.
///
/// `None` when the marker is absent, no `{` follows it, or the object is truncated.
pub fn json_object_after<'a>(haystack: &'a str, marker: &str) -> Option<&'a str> {
    balanced_after(haystack, marker, '{', '}')
}

/// The object *assigned to* a global, as opposed to merely mentioned near one.
///
/// [`json_object_after`] takes the first `{` after the first mention of the marker, which
/// is right when the marker only ever appears as an assignment and wrong the moment it
/// does not. Bilibili's watch page is the counter-example: it assigns
/// `window.__playinfo__` only when the player is being served the manifest, but its
/// bootstrap *mentions* the same name unconditionally, in
/// `if (window.__playinfo__) { primarySetting.prefetch = { … } }`. Matching on the
/// mention hands back that `if` body, which is JavaScript and not JSON, and the site is
/// reported as having changed shape when it has done nothing of the kind.
///
/// So: find an occurrence the page actually assigns to — the next non-space character is
/// a lone `=` — and take the object after that.
pub fn json_object_assigned_to<'a>(haystack: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0usize;
    while let Some(offset) = haystack[from..].find(name) {
        let at = from + offset;
        let rest = &haystack[at + name.len()..];
        let trimmed = rest.trim_start();
        // `=` but not `==` or `=>`, either of which is a comparison or an arrow and not
        // the assignment being looked for.
        if trimmed.starts_with('=') && !trimmed.starts_with("==") && !trimmed.starts_with("=>") {
            if let Some(object) = balanced_after(rest, "=", '{', '}') {
                return Some(object);
            }
        }
        from = at + name.len();
    }
    None
}

/// The array form of [`json_object_after`], for markers whose value is a `[…]`.
pub fn json_array_after<'a>(haystack: &'a str, marker: &str) -> Option<&'a str> {
    balanced_after(haystack, marker, '[', ']')
}

fn balanced_after<'a>(haystack: &'a str, marker: &str, open: char, close: char) -> Option<&'a str> {
    let after = &haystack[haystack.find(marker)? + marker.len()..];
    let body = &after[after.find(open)?..];
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
        } else if c == open {
            depth += 1;
        } else if c == close {
            // `body` starts at an opening delimiter, so depth is at least 1 here.
            depth -= 1;
            if depth == 0 {
                return Some(&body[..i + c.len_utf8()]);
            }
        }
    }
    None
}

/// The text between `<script id="…">` and the next `</script`.
///
/// Located by `id` rather than by position because attribute order varies between server
/// renders, and the `type="application/json"` attribute is sometimes absent.
pub fn script_tag_body<'a>(html: &'a str, id: &str) -> Option<&'a str> {
    let at = html
        .find(&format!("id=\"{id}\""))
        .or_else(|| html.find(&format!("id='{id}'")))?;
    let open = html[..at].rfind("<script")?;
    // The id has to be inside that opening tag, not in some later text that happens to
    // follow a script element.
    if html[open..at].contains('>') {
        return None;
    }
    let gt = html[at..].find('>')? + at;
    let end = html[gt + 1..].find("</script")? + gt + 1;
    Some(html[gt + 1..end].trim())
}

/// Undo JSON string escaping in text lifted straight out of an HTML document.
///
/// The media URLs on these sites live inside JSON that was serialised into HTML, so they
/// arrive as `https:\/\/cdn.example\/v\/x.mp4?oh=1&oe=2`. A parser that skips this
/// step yields a URL that 404s while *looking* correct, which is the most common way one
/// of these extractors is quietly wrong.
pub fn decode_json_escapes(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\' || i + 1 >= chars.len() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let escape = chars[i + 1];
        i += 2;
        match escape {
            'u' => {
                let Some(first) = hex4(&chars, i) else {
                    out.push_str("\\u");
                    continue;
                };
                i += 4;
                // A character outside the BMP arrives as a surrogate pair, i.e. two
                // consecutive \uXXXX escapes that must be recombined; emitting them
                // separately produces two replacement characters.
                let code = if (0xD800..0xDC00).contains(&first) {
                    match (chars.get(i), chars.get(i + 1), hex4(&chars, i + 2)) {
                        (Some('\\'), Some('u'), Some(low)) if (0xDC00..0xE000).contains(&low) => {
                            i += 6;
                            0x10000 + ((first - 0xD800) << 10) + (low - 0xDC00)
                        }
                        _ => first,
                    }
                } else {
                    first
                };
                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
            }
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            // `\/`, `\\`, `\"` and anything unrecognised stand for themselves.
            other => out.push(other),
        }
    }
    out
}

fn hex4(chars: &[char], at: usize) -> Option<u32> {
    if at + 4 > chars.len() {
        return None;
    }
    let mut value = 0u32;
    for c in &chars[at..at + 4] {
        value = value * 16 + c.to_digit(16)?;
    }
    Some(value)
}

/// Undo HTML entity escaping.
///
/// Needed because `og:video` content attributes carry `&amp;` where the URL has `&`, and
/// a query string with `&amp;` in it is a different request.
pub fn decode_html_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        // An entity is short; a bare `&` followed by prose is far more common than a
        // long one, so bound the search rather than scanning to the next `;` anywhere.
        let Some(end) = after[1..].find(';').filter(|j| *j <= 8).map(|j| j + 1) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let name = &after[1..end];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                Some(hex) => u32::from_str_radix(hex, 16).ok().and_then(char::from_u32),
                None => name
                    .strip_prefix('#')
                    .and_then(|d| d.parse::<u32>().ok())
                    .and_then(char::from_u32),
            },
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Read one quoted attribute out of a single tag's text.
///
/// The name must sit on a whitespace boundary, so asking for `title` does not match the
/// `title` inside `class="video-title"` — which is exactly the tag bilibili emits.
pub fn attr(tag: &str, name: &str) -> Option<String> {
    let mut rest = tag;
    loop {
        let i = rest.find(name)?;
        let on_boundary = i == 0
            || matches!(
                rest.as_bytes()[i - 1],
                b' ' | b'\t' | b'\n' | b'\r' | b'/' | b'<'
            );
        let after = rest[i + name.len()..].trim_start();
        if on_boundary {
            if let Some(value) = after.strip_prefix('=') {
                let value = value.trim_start();
                let quote = value.chars().next()?;
                if quote == '"' || quote == '\'' {
                    return Some(value[quote.len_utf8()..].split(quote).next()?.to_string());
                }
                return Some(
                    value
                        .split([' ', '\t', '\n', '\r', '>'])
                        .next()
                        .unwrap_or("")
                        .to_string(),
                );
            }
        }
        rest = &rest[i + name.len()..];
    }
}

/// `content="…"` from the `<meta>` tag whose `property` or `name` is `key`, entity-decoded.
pub fn meta_content(html: &str, key: &str) -> Option<String> {
    for chunk in html.split("<meta").skip(1) {
        let tag = chunk.split('>').next().unwrap_or(chunk);
        let is_key = attr(tag, "property").as_deref() == Some(key)
            || attr(tag, "name").as_deref() == Some(key);
        if is_key {
            if let Some(v) = attr(tag, "content").filter(|v| !v.is_empty()) {
                return Some(decode_html_entities(&v));
            }
        }
    }
    None
}

/// The document `<title>`, entity-decoded.
pub fn html_title(html: &str) -> Option<String> {
    let at = html.find("<title")?;
    let gt = html[at..].find('>')? + at;
    let end = html[gt + 1..].find("</title")? + gt + 1;
    let text = decode_html_entities(html[gt + 1..end].trim());
    (!text.is_empty()).then_some(text)
}

/// The string value that follows `"key":` in raw JSON-inside-HTML text, escapes decoded.
///
/// Deliberately works on the document text rather than a parsed tree: these pages carry
/// megabytes of JSON split across many script tags, several of which do not parse on
/// their own, and only one value is wanted.
pub fn json_string_value(haystack: &str, key: &str) -> Option<String> {
    for needle in [format!("\"{key}\":\""), format!("\"{key}\": \"")] {
        let Some(start) = haystack.find(&needle).map(|i| i + needle.len()) else {
            continue;
        };
        let rest = &haystack[start..];
        let mut escaped = false;
        for (i, c) in rest.char_indices() {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                let value = decode_json_escapes(&rest[..i]);
                return (!value.is_empty()).then_some(value);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Shared label primitives
//
// Here for the same reason the scrapers above are. Every site builds the same "quality ·
// codec · bitrate" string for its choice lists, and a bitrate that reads "2.4 Mbps" on one
// site and "2400 kbps" on the next is a picker that looks assembled rather than designed.
// ---------------------------------------------------------------------------

/// The handful of codecs these sites actually ship, under the names people use for them.
///
/// Anything else keeps its raw string: a name invented here for a codec nobody recognises
/// would say less than the string the site published, not more.
pub fn friendly_codec(codecs: &str) -> String {
    // A muxed format lists both codecs at once: `avc1.42001E, mp4a.40.2`. The picture is
    // what a video choice is being labelled by, and it comes first.
    let first = codecs.split(',').next().unwrap_or(codecs).trim();
    let lower = first.to_ascii_lowercase();
    for (prefix, name) in [
        ("avc1", "AVC"),
        ("avc3", "AVC"),
        ("vp09", "VP9"),
        ("vp9", "VP9"),
        ("vp08", "VP8"),
        ("vp8", "VP8"),
        ("av01", "AV1"),
        ("hev1", "HEVC"),
        ("hvc1", "HEVC"),
        ("mp4a", "AAC"),
        ("opus", "Opus"),
        ("flac", "FLAC"),
        ("ec-3", "EC-3"),
        ("ac-3", "AC-3"),
    ] {
        if lower.starts_with(prefix) {
            return name.to_string();
        }
    }
    first.to_string()
}

/// "2.4 Mbps" above a megabit and "643 kbps" below it.
///
/// `None` under a kilobit, which in practice means the site stated no bitrate at all: a
/// label reading "0 kbps" claims a fact, where leaving it out merely omits one.
pub fn bitrate_text(bits_per_second: u64) -> Option<String> {
    match bits_per_second {
        b if b >= 1_000_000 => Some(format!("{:.1} Mbps", b as f64 / 1_000_000.0)),
        b if b >= 1_000 => Some(format!("{} kbps", b / 1000)),
        _ => None,
    }
}

/// Join the parts of a label that are actually known, so an unknown bitrate leaves
/// "1080p · AV1" rather than "1080p · AV1 · ".
pub fn join_parts(parts: &[Option<String>]) -> String {
    parts
        .iter()
        .flatten()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Reduce a site-supplied identifier to something safe to put in a choice id.
pub fn id_token(raw: &str) -> String {
    let token: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .take(24)
        .collect();
    if token.is_empty() {
        "x".to_string()
    } else {
        token
    }
}

/// Keep `base` unique within `taken`, remembering what it hands out.
///
/// A UI pairs a chosen video id with a chosen audio id, so two renditions answering to one
/// name is not an untidiness — it is the user getting a different file from the one they
/// picked.
pub fn unique_id(taken: &mut Vec<String>, base: String) -> String {
    let mut id = base.clone();
    let mut n = 2;
    while taken.contains(&id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    taken.push(id.clone());
    id
}

// ---------------------------------------------------------------------------
// Bilibili proper
// ---------------------------------------------------------------------------

/// Headers every bilibili media URL must carry.
///
/// **This is the single most important detail in the file.** `*.bilivideo.com` checks
/// `Referer` on every segment request and answers `403` without it — the manifest URL is
/// perfectly valid, the bytes simply never arrive — and it checks the `User-Agent` in the
/// same breath. A download built here without these two headers fails in a way that looks
/// like a network fault rather than a missing header, so they are attached centrally in
/// [`headers`] and never at a call site where one could be forgotten.
const REFERER: &str = "https://www.bilibili.com/";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

fn headers() -> Vec<(String, String)> {
    vec![
        ("Referer".to_string(), REFERER.to_string()),
        ("User-Agent".to_string(), USER_AGENT.to_string()),
    ]
}

#[derive(Debug, Default)]
pub struct Bilibili {
    page_url: String,
    /// Set once the watch page came back without a manifest in it, so the next body fed
    /// back is the player API's answer rather than more HTML.
    awaiting_playurl: bool,
    /// The bvid, while the `pagelist` hop is out fetching the `cid` to go with it. Only
    /// set when the page itself no longer carries one.
    awaiting_pagelist: Option<String>,
    /// The page's own title, carried across the extra hop: the API answer has no title
    /// in it, only streams.
    title: Option<String>,
}

impl Bilibili {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Extractor for Bilibili {
    fn site(&self) -> &'static str {
        SITE
    }

    fn start(&mut self, url: &str) -> Result<Step, SiteError> {
        // Remembered because nothing hands the URL back later and the title falls back to
        // the `BV…` id in it when the page carries no readable one.
        self.page_url = url.to_string();
        Ok(Step::Need(Need::PageState))
    }

    fn feed(&mut self, bodies: &[&str]) -> Result<Step, SiteError> {
        let body = bodies.first().copied().ok_or_else(shape)?;

        if self.awaiting_playurl {
            let title = self
                .title
                .clone()
                .unwrap_or_else(|| page_title("", &self.page_url));
            return Ok(Step::Done(parse_playurl(body, title)?));
        }

        // The `cid` this page would not give up. Checked before the HTML branches below,
        // because what comes back here is JSON and would fail every one of them.
        if let Some(bvid) = self.awaiting_pagelist.take() {
            let cid = parse_pagelist(body, url_part(&self.page_url)).ok_or_else(shape)?;
            self.awaiting_playurl = true;
            return Ok(Step::Need(Need::Fetch(vec![playurl_request(&VideoIds {
                bvid,
                cid,
            })])));
        }

        // Read from the tab the player is in, the page carries the manifest inline and it
        // is the better answer: it is whatever that session is entitled to, which for a
        // logged-in viewer is the high renditions.
        if json_object_assigned_to(body, "window.__playinfo__").is_some() {
            return Ok(Step::Done(parse_watch_page(body, &self.page_url)?));
        }

        // Fetched from outside that tab — by the relay, or by the extension without the
        // page open — the same HTML arrives with `__playinfo__` merely *referenced* by
        // the player bootstrap and never assigned, because the player fetches it itself.
        // What the page does carry is the pair of ids the player would have asked with,
        // so ask with them. Anonymous callers are capped at the lower renditions, which
        // is a smaller loss than refusing the link outright.
        self.title = Some(page_title(body, &self.page_url));

        // Both ids in the page: ask the player API and be done in one more hop.
        if let Some(ids) = video_ids(body) {
            self.awaiting_playurl = true;
            return Ok(Step::Need(Need::Fetch(vec![playurl_request(&ids)])));
        }

        // Only the bvid. `videoData` is now served as a stub — `owner` and `stat`, with
        // both ids absent — and the page fills itself in after load, so a fetched copy
        // never has the `cid` the player API cannot work without. One more hop asks for
        // it. Before this, the missing `cid` surfaced as "the site has probably changed",
        // which was true and unhelpful in equal measure.
        let bvid = page_bvid(body, &self.page_url).ok_or_else(unreadable_page)?;
        self.awaiting_pagelist = Some(bvid.clone());
        Ok(Step::Need(Need::Fetch(vec![pagelist_request(&bvid)])))
    }

    /// Yes: a fetched page still carries `__INITIAL_STATE__`, and the ids in it are
    /// enough to ask the player API for the manifest the page itself withheld.
    fn accepts_fetched_page(&self) -> bool {
        true
    }
}

/// The `bvid`/`cid` pair that names one playable part of one video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoIds {
    pub bvid: String,
    pub cid: String,
}

/// Lift the ids out of `window.__INITIAL_STATE__`, which every watch page assigns.
///
/// `cid` is the part id, not the video id: a multi-part video has one `bvid` and a `cid`
/// per part, and asking with the wrong one returns a different part's audio. This reads
/// `videoData.cid`, which is the part the URL actually addresses.
pub fn video_ids(html: &str) -> Option<VideoIds> {
    let root = initial_state(html)?;
    let video = root.get("videoData")?;
    let bvid = id_string(video.get("bvid")?)?;
    let cid = id_string(video.get("cid")?)?;
    Some(VideoIds { bvid, cid })
}

/// `window.__INITIAL_STATE__`, parsed.
fn initial_state(html: &str) -> Option<Value> {
    serde_json::from_str(json_object_assigned_to(html, "window.__INITIAL_STATE__")?).ok()
}

/// A bvid or cid, however this page happens to have serialised it.
///
/// A cid is a number and large enough that a float round-trip would corrupt it, so it is
/// read from the token rather than through `as_u64`.
fn id_string(value: &Value) -> Option<String> {
    let text = match value {
        Value::Number(n) => n.to_string(),
        Value::String(t) => t.clone(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// The bvid, from wherever this page still keeps it.
///
/// `videoData` used to hold both ids and now arrives as a stub — `owner` and `stat`, with
/// `bvid` and `cid` both absent — because the page hydrates itself after load. The bvid
/// survives at the root of the same object, and failing that it is in the URL, which is
/// where it came from in the first place and cannot go stale.
pub fn page_bvid(html: &str, page_url: &str) -> Option<String> {
    // A watch page or nothing. The URL is a fine source for the id, but only once this
    // body has proved it is the page it claims to be — reaching for the URL whenever the
    // HTML is unreadable would turn "I could not read that page" into an API call and a
    // vaguer failure one hop later.
    if let Some(root) = initial_state(html) {
        return root
            .get("bvid")
            .and_then(id_string)
            .or_else(|| root.get("videoData")?.get("bvid").and_then(id_string))
            .or_else(|| bvid_in_url(page_url));
    }
    // The live tab, read by the extension. Bilibili deletes the inline
    // `__INITIAL_STATE__` script once it has run, so the object is in the page's memory
    // and nowhere in its markup — on every visit, measured 13 September 2026. A second
    // visit gets away with it only because by then `__playinfo__` is assigned; a first
    // visit, with no cookie yet, has neither, and used to end here as "the site has
    // probably changed". What the markup still has is the bvid itself, in the canonical
    // link and a dozen other places, and a page that names the video in its URL is the
    // proof the rule above asks for.
    let bvid = bvid_in_url(page_url)?;
    html.contains(&bvid).then_some(bvid)
}

/// The `BV…` id out of a watch URL.
fn bvid_in_url(url: &str) -> Option<String> {
    let rest = url.split("/video/").nth(1)?;
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (id.len() > 2 && id.starts_with("BV")).then_some(id)
}

/// Ask which parts this video has, to learn the `cid` of the first.
///
/// `x/player/pagelist` rather than `x/web-interface/view`, which is the endpoint that
/// would also carry the title: `view` refuses a server outright — it answers `412` to a
/// browser User-Agent and its own `-404` to anything else — while `pagelist` answers both
/// shapes. The title is already in hand from the page, so `view` buys nothing here.
pub fn pagelist_request(bvid: &str) -> Request {
    Request {
        url: format!("https://api.bilibili.com/x/player/pagelist?bvid={bvid}"),
        method: "GET".to_string(),
        headers: headers(),
        body: None,
    }
}

/// The `cid` of the part the URL addresses, from a `pagelist` answer.
///
/// `part` is the `?p=` of the watch URL, 1-based. The first part is right for every
/// single-part video, and for a multi-part one it is a different episode's audio — which
/// would be a quiet wrong download rather than an error, so the part is matched by its
/// own `page` number and not by position.
pub fn parse_pagelist(body: &str, part: u32) -> Option<String> {
    let root: Value = serde_json::from_str(body).ok()?;
    let parts = root.get("data")?.as_array()?;
    let chosen = parts
        .iter()
        .find(|p| p.get("page").and_then(Value::as_u64) == Some(u64::from(part)))
        .or_else(|| (part <= 1).then(|| parts.first()).flatten())?;
    chosen.get("cid").and_then(id_string)
}

/// The `?p=` of a watch URL: which part of a multi-part video it addresses.
fn url_part(url: &str) -> u32 {
    url.split(['?', '&'])
        .skip(1)
        .find_map(|kv| kv.strip_prefix("p="))
        .and_then(|n| n.split('#').next()?.parse().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(1)
}

/// The player's own manifest request.
///
/// `x/player/playurl` rather than `x/player/wbi/playurl`: the `wbi` form needs a signature
/// derived from a key that rotates daily, and the plain form still answers. `fnval=4048`
/// asks for DASH with every adaptation bilibili will give an anonymous caller; without it
/// the answer is a single muxed FLV.
pub fn playurl_request(ids: &VideoIds) -> Request {
    Request {
        url: format!(
            "https://api.bilibili.com/x/player/playurl?bvid={}&cid={}&fnval=4048&fourk=1",
            ids.bvid, ids.cid
        ),
        method: "GET".to_string(),
        headers: headers(),
        body: None,
    }
}

/// Turn the player API's answer into an [`Extraction`].
pub fn parse_playurl(json: &str, title: String) -> Result<Extraction, SiteError> {
    let root: Value = serde_json::from_str(json).map_err(|_| shape())?;
    // The envelope reports refusals in `code`, with a message written for a viewer. A
    // `0` body that simply has no streams in it is a shape change; a non-zero one is the
    // site telling us why, and repeating that is more use than "the site has changed".
    match root.get("code").and_then(Value::as_i64) {
        Some(0) | None => {}
        Some(_) => {
            let why = root
                .get("message")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .unwrap_or("Bilibili would not serve this video to a signed-out request");
            return Err(SiteError::Unavailable(why.to_string()));
        }
    }
    extraction_from_manifest(json, title)
}

/// Turn a watch page's HTML into an [`Extraction`].
///
/// Pure and public so the tests drive it directly rather than through the state machine.
pub fn parse_watch_page(html: &str, page_url: &str) -> Result<Extraction, SiteError> {
    let playinfo = json_object_assigned_to(html, "window.__playinfo__").ok_or_else(shape)?;
    extraction_from_manifest(playinfo, page_title(html, page_url))
}

/// The half both paths share: one manifest, already lifted out of wherever it was, and a
/// title from whichever source had one.
///
/// The page's inline `__playinfo__` and the player API's response body are the same
/// document — the page is literally the API's answer, pasted into a `<script>` — so
/// exactly one parse serves both and neither can drift away from the other.
fn extraction_from_manifest(manifest: &str, title: String) -> Result<Extraction, SiteError> {
    // Refuse before anything else. Turning a protected rendition into a fetchable URL and
    // then declining to use it is worse than never producing the URL at all.
    if manifest.contains("ContentProtection") {
        return Err(SiteError::Encrypted);
    }
    let root: Value = serde_json::from_str(manifest).map_err(|_| shape())?;
    if has_drm(&root) {
        return Err(SiteError::Encrypted);
    }

    // The manifest is normally wrapped in the API envelope (`{code, message, data}`) but
    // some renders inline the payload directly.
    let data = root.get("data").unwrap_or(&root);
    let duration_ms = duration_ms(data);

    let (options, videos, audios) = if let Some(dash) = data.get("dash") {
        let video_renditions = renditions(dash.get("video"));
        // Only `dash.audio[]`. Bilibili also publishes `dash.dolby.audio[]` and
        // `dash.flac.audio` on the videos that have them, and nothing in this file parses
        // either — deliberately, because inventing a parse for a shape no capture exists
        // for is how an extractor acquires a branch nobody can test. When a page carrying
        // one is captured, those entries belong here beside `dash.audio`, and
        // [`audio_quality_name`] already knows what to call them.
        let audio_renditions = renditions(dash.get("audio"));
        (
            dash_options(&video_renditions, &audio_renditions, &title, duration_ms),
            dash_video_choices(&video_renditions),
            dash_audio_choices(&audio_renditions),
        )
    } else if let Some(durl) = data.get("durl").and_then(Value::as_array) {
        (
            durl_options(durl, data, &title, duration_ms),
            // A `durl` entry is one file with the sound already in it, so there is nothing
            // to pair and no audio list to offer.
            durl_video_choices(durl, data),
            Vec::new(),
        )
    } else {
        return Err(shape());
    };

    if options.is_empty() {
        return Err(shape());
    }
    let mut extraction = Extraction {
        note: None,
        site: SITE.to_string(),
        title,
        options,
        videos,
        audios,
        subtitles: Vec::new(),
    };
    extraction.note = withheld_note(data);
    // Ordering and the single `best` flag are decided in one place for every site, so
    // "best" cannot come to mean two things — see [`Extraction::rank_choices`].
    extraction.rank_choices();
    Ok(extraction)
}

/// What bilibili says this video has, set against what it just handed over.
///
/// `accept_quality` lists every rendition the video exists in and `quality` is the one
/// served. A signed-out request gets the lowest — 480p on a video that also exists in
/// 1080p — and asking for more with `qn` changes nothing, which is the point: it is an
/// account gate, not a parameter. Without this the menu is correct, complete, and looks
/// broken, because the resolution the site itself advertises is missing from it.
fn withheld_note(data: &Value) -> Option<String> {
    let accepted: Vec<i64> = data
        .get("accept_quality")?
        .as_array()?
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    let served = data.get("quality").and_then(Value::as_i64)?;
    let best = accepted.iter().copied().max()?;
    if best <= served {
        return None;
    }
    // `accept_description` runs parallel to `accept_quality`, best first, and holds
    // bilibili's own name for each — which is what the viewer sees on bilibili itself.
    let index = accepted.iter().position(|q| *q == best)?;
    let name = data
        .get("accept_description")?
        .as_array()?
        .get(index)?
        .as_str()?
        .trim();
    Some(format!(
        "Bilibili lists “{name}” for this video but serves only its lower renditions to a \
         signed-out request. Sign in to Bilibili in your browser and use the OpenDownloader \
         extension on the video's own page to get what your account is entitled to."
    ))
}

/// True when the manifest marks anything as protected.
///
/// Two forms are known: a `drm` key on the manifest or on one rendition, and a DASH
/// `ContentProtection` element carried through into the JSON (checked separately, on the
/// raw text). A `drm` key set to `false`/`0`/`null` is bilibili saying *not* protected, so
/// only a truthy value counts — otherwise every ordinary video would be refused.
fn has_drm(v: &Value) -> bool {
    match v {
        Value::Object(map) => map
            .iter()
            .any(|(k, val)| (k.eq_ignore_ascii_case("drm") && is_truthy(val)) || has_drm(val)),
        Value::Array(items) => items.iter().any(has_drm),
        _ => false,
    }
}

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty() && s != "0",
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn duration_ms(data: &Value) -> Option<u64> {
    if let Some(seconds) = data.pointer("/dash/duration").and_then(Value::as_f64) {
        if seconds > 0.0 {
            return Some((seconds * 1000.0).round() as u64);
        }
    }
    // `timelength` is already milliseconds; the two fields differ by a factor of 1000 and
    // mixing them up is a silent bug, so they are read separately rather than "whichever
    // is present".
    data.get("timelength")
        .and_then(Value::as_u64)
        .filter(|ms| *ms > 0)
}

/// Bilibili's quality codes. Unknown ids stay visible rather than being dropped: a new
/// code means a new rendition that probably still downloads.
fn quality_name(id: i64) -> String {
    match id {
        6 => "240P".to_string(),
        16 => "360P".to_string(),
        32 => "480P".to_string(),
        64 => "720P".to_string(),
        74 => "720P60".to_string(),
        80 => "1080P".to_string(),
        112 => "1080P+".to_string(),
        116 => "1080P60".to_string(),
        120 => "4K".to_string(),
        125 => "HDR".to_string(),
        126 => "Dolby Vision".to_string(),
        127 => "8K".to_string(),
        other => format!("quality {other}"),
    }
}

/// Approximate picture height for a quality code, used as the sort key when the manifest
/// omits `height` (the `durl` form always does).
fn quality_rank(id: i64) -> u64 {
    match id {
        6 => 240,
        16 => 360,
        32 => 480,
        64 | 74 => 720,
        80 | 112 | 116 => 1080,
        120 | 125 | 126 => 2160,
        127 => 4320,
        other => other.max(0) as u64,
    }
}

/// Prefer AVC when bilibili publishes the same quality several times over.
///
/// A 1080P entry commonly appears three times — `avc1`, `hev1`, `av01` — and the merged
/// options built here pair a video adaptation with an audio one for `dl-container` to mux
/// into MP4. That merger handles AVC in MP4; HEVC and AV1 would need a different remux
/// path, so picking AVC is what makes the resulting file actually play.
fn codec_preference(codecs: &str) -> u8 {
    let c = codecs.to_ascii_lowercase();
    if c.starts_with("avc1") || c.starts_with("avc3") {
        0
    } else if c.starts_with("hev1") || c.starts_with("hvc1") {
        1
    } else {
        2
    }
}

#[derive(Debug, Clone)]
struct Rendition {
    id: i64,
    url: String,
    bandwidth: u64,
    codecs: String,
    mime: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<String>,
}

impl Rendition {
    /// The frame rate as a whole number, when the manifest states one at all.
    fn fps(&self) -> Option<u32> {
        self.frame_rate
            .as_deref()
            .and_then(|f| f.parse::<f64>().ok())
            .filter(|f| *f > 0.0)
            .map(|f| f.round() as u32)
    }

    /// Built here rather than at each call site, so no rendition can escape without the
    /// `Referer` the CDN answers `403` without.
    fn stream(&self, kind: StreamKind) -> Stream {
        Stream {
            url: self.url.clone(),
            kind,
            mime: self.mime.clone(),
            size: None,
            headers: headers(),
            max_chunk: None,
        }
    }
}

fn renditions(v: Option<&Value>) -> Vec<Rendition> {
    let Some(items) = v.and_then(Value::as_array) else {
        return Vec::new();
    };
    items.iter().filter_map(rendition).collect()
}

fn rendition(item: &Value) -> Option<Rendition> {
    Some(Rendition {
        id: item.get("id").and_then(Value::as_i64).unwrap_or(0),
        url: first_url(item)?,
        bandwidth: item.get("bandwidth").and_then(Value::as_u64).unwrap_or(0),
        codecs: item
            .get("codecs")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        mime: item
            .get("mimeType")
            .or_else(|| item.get("mime_type"))
            .and_then(Value::as_str)
            .map(str::to_string),
        width: dimension(item, "width"),
        height: dimension(item, "height"),
        frame_rate: item
            .get("frameRate")
            .or_else(|| item.get("frame_rate"))
            .and_then(number_or_string),
    })
}

fn dimension(item: &Value, key: &str) -> Option<u32> {
    item.get(key)
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .map(|n| n as u32)
}

fn number_or_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `baseUrl`, falling back to the first `backupUrl`.
///
/// The backups are the same bytes on a different CDN edge. A [`Stream`] carries one URL,
/// so the base is preferred and a backup is a rescue for a manifest that omits it rather
/// than a second option offered to the user.
fn first_url(item: &Value) -> Option<String> {
    for key in ["baseUrl", "base_url", "url"] {
        if let Some(u) = item
            .get(key)
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
        {
            return Some(u.to_string());
        }
    }
    for key in ["backupUrl", "backup_url"] {
        if let Some(arr) = item.get(key).and_then(Value::as_array) {
            for u in arr {
                if let Some(u) = u.as_str().filter(|u| !u.is_empty()) {
                    return Some(u.to_string());
                }
            }
        }
    }
    None
}

fn dash_options(
    videos: &[Rendition],
    audios: &[Rendition],
    title: &str,
    duration_ms: Option<u64>,
) -> Vec<MediaOption> {
    let best_audio = audios.iter().max_by_key(|a| a.bandwidth);

    let mut options = Vec::new();
    // One option per *quality*, not per entry: the three codec variants of 1080P are the
    // same picture, and offering all of them is a menu of indistinguishable choices.
    let mut seen_ids: Vec<i64> = Vec::new();
    for video in videos {
        if seen_ids.contains(&video.id) {
            continue;
        }
        seen_ids.push(video.id);
        let best = videos
            .iter()
            .filter(|v| v.id == video.id)
            .min_by(|a, b| {
                codec_preference(&a.codecs)
                    .cmp(&codec_preference(&b.codecs))
                    .then(b.bandwidth.cmp(&a.bandwidth))
            })
            .unwrap_or(video);

        let mut streams = vec![best.stream(StreamKind::VideoOnly)];
        // No audio adaptation at all is rare but real (silent uploads). The video-only
        // option is still worth offering; the merger simply has nothing to pair.
        if let Some(audio) = best_audio {
            streams.push(audio.stream(StreamKind::AudioOnly));
        }

        options.push(MediaOption {
            label: dash_label(best),
            rank: best.height.map(u64::from).unwrap_or(quality_rank(best.id)),
            streams,
            filename: safe_filename(title, "mp4"),
            width: best.width,
            height: best.height,
            duration_ms,
        });
    }

    if let Some(audio) = best_audio {
        let kbps = audio.bandwidth / 1000;
        options.push(MediaOption {
            label: if kbps > 0 {
                format!("Audio only · {kbps} kbps")
            } else {
                "Audio only".to_string()
            },
            rank: kbps,
            streams: vec![audio.stream(StreamKind::AudioOnly)],
            filename: safe_filename(title, "m4a"),
            width: None,
            height: None,
            duration_ms,
        });
    }

    options.sort_by_key(|o| std::cmp::Reverse(o.rank));
    options
}

fn dash_label(r: &Rendition) -> String {
    let name = quality_name(r.id);
    // Codes 74 and 116 already say "60" in their names; anything else with a high frame
    // rate is worth calling out, since it is the reason the file is twice the size.
    match r.frame_rate.as_deref().and_then(|f| f.parse::<f64>().ok()) {
        Some(fps) if fps >= 50.0 && !name.contains("60") => format!("{name} · {}fps", fps.round()),
        _ => name,
    }
}

/// The codec part of a choice id: `avc1.640032` → `avc1`.
///
/// The bare quality code is not enough on its own, because bilibili publishes the same
/// 1080P three times over and the codec is the only thing that tells the three apart.
fn codec_token(codecs: &str) -> String {
    let head = codecs.split('.').next().unwrap_or(codecs).trim();
    if head.is_empty() {
        "na".to_string()
    } else {
        id_token(&head.to_ascii_lowercase())
    }
}

/// The published bitrate of a bilibili audio quality code.
///
/// The codes *are* the quality, and the rate they stand for is a truer label than the
/// `bandwidth` of one particular encode, which drifts with the content. Unknown codes fall
/// back to the manifest's own figure rather than being labelled with a guess.
fn audio_bitrate(id: i64) -> Option<u64> {
    match id {
        30216 => Some(64_000),
        30232 => Some(132_000),
        30280 => Some(192_000),
        _ => None,
    }
}

/// The two audio codes that are a *format* rather than a bitrate, in the names bilibili's
/// own player uses for them.
fn audio_quality_name(id: i64) -> Option<&'static str> {
    match id {
        30250 => Some("Dolby Atmos"),
        30251 => Some("Hi-Res"),
        _ => None,
    }
}

/// Every dash video rendition, every codec.
///
/// [`dash_options`] deliberately shows one entry per quality and prefers AVC, because the
/// merger writes MP4. This list has no such duty: a user asking for the HEVC or AV1
/// rendition of a 1080P is asking for something real, and hiding it because the one-click
/// path cannot use it is the gap these lists exist to close.
fn dash_video_choices(videos: &[Rendition]) -> Vec<VideoChoice> {
    let mut taken = Vec::new();
    videos
        .iter()
        .map(|v| {
            let codec = (!v.codecs.is_empty()).then(|| friendly_codec(&v.codecs));
            VideoChoice {
                id: unique_id(&mut taken, format!("v{}-{}", v.id, codec_token(&v.codecs))),
                label: join_parts(&[
                    Some(dash_label(v)),
                    codec.clone(),
                    bitrate_text(v.bandwidth),
                ]),
                width: v.width,
                height: v.height,
                fps: v.fps(),
                bitrate: (v.bandwidth > 0).then_some(v.bandwidth),
                codec,
                // The dash manifest states a bandwidth, never a byte count.
                size: None,
                stream: v.stream(StreamKind::VideoOnly),
                has_audio: false,
                best: false,
                // Decided centrally by `rank_choices`, from the mime this stream carries.
                container: None,
                mergeable: false,
            }
        })
        .collect()
}

fn dash_audio_choices(audios: &[Rendition]) -> Vec<AudioChoice> {
    let mut taken = Vec::new();
    audios
        .iter()
        .map(|a| {
            let codec = (!a.codecs.is_empty()).then(|| friendly_codec(&a.codecs));
            let bitrate = audio_bitrate(a.id).or((a.bandwidth > 0).then_some(a.bandwidth));
            AudioChoice {
                id: unique_id(&mut taken, format!("a{}", a.id)),
                label: join_parts(&[
                    audio_quality_name(a.id).map(str::to_string),
                    bitrate.and_then(bitrate_text),
                    codec.clone(),
                ]),
                bitrate,
                codec,
                // Bilibili names no language on these; a multi-language upload publishes a
                // second video page rather than a second audio adaptation.
                language: None,
                size: None,
                stream: a.stream(StreamKind::AudioOnly),
                best: false,
                // Decided centrally by `rank_choices`, from the mime this stream carries.
                container: None,
                mergeable: false,
            }
        })
        .collect()
}

/// One `durl` entry, resolved to the three things both the option and the choice need.
///
/// Shared so a part cannot be called one thing in the menu and another in the picker.
struct DurlPart {
    url: String,
    /// `flv` or `mp4`, read from the URL's own path.
    extension: &'static str,
    label: String,
}

fn durl_part(part: &Value, index: usize, multipart: bool, name: &str) -> Option<DurlPart> {
    let url = first_url(part)?;
    let extension = if url.split('?').next().unwrap_or(&url).ends_with(".flv") {
        "flv"
    } else {
        "mp4"
    };
    let label = if multipart {
        format!("{name} · part {}", index + 1)
    } else {
        name.to_string()
    };
    Some(DurlPart {
        url,
        extension,
        label,
    })
}

/// The muxed parts, as video choices that need no audio picked for them.
fn durl_video_choices(durl: &[Value], data: &Value) -> Vec<VideoChoice> {
    let id = data.get("quality").and_then(Value::as_i64).unwrap_or(0);
    let name = quality_name(id);
    let multipart = durl.len() > 1;
    let mut taken = Vec::new();
    durl.iter()
        .enumerate()
        .filter_map(|(index, part)| {
            let resolved = durl_part(part, index, multipart, &name)?;
            let base = if multipart {
                format!("v{id}-p{}", index + 1)
            } else {
                format!("v{id}")
            };
            Some(VideoChoice {
                id: unique_id(&mut taken, base),
                label: resolved.label,
                width: dimension(data, "width"),
                height: dimension(data, "height"),
                fps: None,
                bitrate: None,
                codec: None,
                size: part.get("size").and_then(Value::as_u64),
                stream: Stream {
                    url: resolved.url,
                    kind: StreamKind::Muxed,
                    mime: Some(format!("video/{}", resolved.extension)),
                    size: part.get("size").and_then(Value::as_u64),
                    headers: headers(),
                    max_chunk: None,
                },
                has_audio: true,
                best: false,
                // Decided centrally by `rank_choices`, from the mime this stream carries.
                container: None,
                mergeable: false,
            })
        })
        .collect()
}

/// The legacy muxed form.
///
/// A `durl` list with more than one entry is the old split-FLV delivery: the entries are
/// consecutive *parts* of one video, not alternative qualities. They are offered as
/// separate options because joining FLV parts is not something this crate does, and
/// labelled as parts so the user is not misled into picking "the best one".
fn durl_options(
    durl: &[Value],
    data: &Value,
    title: &str,
    duration_ms: Option<u64>,
) -> Vec<MediaOption> {
    let id = data.get("quality").and_then(Value::as_i64).unwrap_or(0);
    let name = quality_name(id);
    let multipart = durl.len() > 1;

    let mut options = Vec::new();
    for (index, part) in durl.iter().enumerate() {
        let Some(DurlPart {
            url,
            extension,
            label,
        }) = durl_part(part, index, multipart, &name)
        else {
            continue;
        };
        let filename = if multipart {
            safe_filename(&format!("{title} ({})", index + 1), extension)
        } else {
            safe_filename(title, extension)
        };
        options.push(MediaOption {
            label,
            // Parts share a quality, so they share a rank and the stable sort keeps them
            // in playback order.
            rank: quality_rank(id),
            streams: vec![Stream {
                url,
                kind: StreamKind::Muxed,
                mime: Some(format!("video/{extension}")),
                size: part.get("size").and_then(Value::as_u64),
                headers: headers(),
                max_chunk: None,
            }],
            filename,
            width: dimension(data, "width"),
            height: dimension(data, "height"),
            duration_ms: part
                .get("length")
                .and_then(Value::as_u64)
                .filter(|ms| *ms > 0)
                .or(duration_ms),
        });
    }
    options.sort_by_key(|o| std::cmp::Reverse(o.rank));
    options
}

/// Three sources, in descending order of reliability.
///
/// `__INITIAL_STATE__` is what the page itself renders from, the `<h1 title>` is what the
/// user can see, and `<title>` is last because bilibili suffixes it with its own branding.
fn page_title(html: &str, page_url: &str) -> String {
    if let Some(state) = json_object_after(html, "window.__INITIAL_STATE__") {
        if let Ok(v) = serde_json::from_str::<Value>(state) {
            if let Some(t) = v
                .pointer("/videoData/title")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                return t.to_string();
            }
        }
    }
    for chunk in html.split("<h1").skip(1) {
        let tag = chunk.split('>').next().unwrap_or(chunk);
        if tag.contains("video-title") {
            if let Some(t) = attr(tag, "title").filter(|t| !t.is_empty()) {
                return decode_html_entities(&t);
            }
        }
    }
    if let Some(t) = html_title(html) {
        let stripped = t
            .trim_end_matches("_哔哩哔哩_bilibili")
            .trim_end_matches("_哔哩哔哩bilibili")
            .trim();
        if !stripped.is_empty() {
            return stripped.to_string();
        }
    }
    video_id(page_url).unwrap_or_else(|| "bilibili video".to_string())
}

/// The `BV…`/`av…` id out of a watch URL, used only as a last-resort title.
fn video_id(page_url: &str) -> Option<String> {
    let after = page_url.split("/video/").nth(1)?;
    let id = after.split(['/', '?', '#']).next()?;
    (!id.is_empty()).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DASH_PAGE: &str = r#"<!doctype html><html><head>
<title>A video} with a brace_哔哩哔哩_bilibili</title></head><body>
<h1 class="video-title" title="Ignored, the state wins">x</h1>
<script>window.__INITIAL_STATE__={"videoData":{"title":"A video} with a brace","bvid":"BV1xx411c7mD"}};(function(){var t=1;})();</script>
<script>window.__playinfo__={"code":0,"message":"0","data":{"quality":80,"timelength":123456,"dash":{"duration":123,"video":[
{"id":80,"baseUrl":"https://cn.bilivideo.com/v/1080-hev.m4s","bandwidth":2500000,"codecs":"hev1.1.6.L120.90","width":1920,"height":1080,"frameRate":"30","mimeType":"video/mp4"},
{"id":80,"baseUrl":"https://cn.bilivideo.com/v/1080-avc.m4s","backupUrl":["https://backup.bilivideo.com/v/1080-avc.m4s"],"bandwidth":2000000,"codecs":"avc1.640032","width":1920,"height":1080,"frameRate":"30","mimeType":"video/mp4"},
{"id":32,"baseUrl":"https://cn.bilivideo.com/v/480-avc.m4s","bandwidth":500000,"codecs":"avc1.64001e","width":852,"height":480,"frameRate":"30","mimeType":"video/mp4"}],
"audio":[{"id":30216,"baseUrl":"https://cn.bilivideo.com/a/64.m4s","bandwidth":64000,"codecs":"mp4a.40.2","mimeType":"audio/mp4"},
{"id":30280,"baseUrl":"https://cn.bilivideo.com/a/192.m4s","bandwidth":192000,"codecs":"mp4a.40.2","mimeType":"audio/mp4"}]}}}</script>
</body></html>"#;

    const DURL_PAGE: &str = r#"<html><head><title>Legacy clip_哔哩哔哩_bilibili</title></head>
<script>window.__playinfo__={"code":0,"data":{"quality":64,"timelength":60000,"durl":[
{"order":1,"length":30000,"size":1048576,"url":"https://cn.bilivideo.com/x/1.flv","backup_url":["https://b.bilivideo.com/x/1.flv"]},
{"order":2,"length":30000,"size":2097152,"url":"https://cn.bilivideo.com/x/2.flv"}]}}</script></html>"#;

    fn video_ids(x: &Extraction) -> Vec<String> {
        x.videos.iter().map(|v| v.id.clone()).collect()
    }

    fn audio_ids(x: &Extraction) -> Vec<String> {
        x.audios.iter().map(|a| a.id.clone()).collect()
    }

    fn extract(html: &str) -> Extraction {
        let mut e = Bilibili::new();
        assert_eq!(
            e.start("https://www.bilibili.com/video/BV1xx411c7mD")
                .unwrap(),
            Step::Need(Need::PageState)
        );
        match e.feed(&[html]).unwrap() {
            Step::Done(x) => x,
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn brace_matching_survives_a_closing_brace_inside_a_string() {
        let html =
            r#"junk window.__playinfo__ = {"a":"} not the end \" still not","b":{"c":1}} tail"#;
        assert_eq!(
            json_object_after(html, "window.__playinfo__"),
            Some(r#"{"a":"} not the end \" still not","b":{"c":1}}"#)
        );
    }

    #[test]
    fn brace_matching_reports_nothing_for_a_missing_or_truncated_object() {
        assert_eq!(json_object_after("nothing here", "window.__x__"), None);
        assert_eq!(
            json_object_after("window.__x__ = {\"a\":1", "window.__x__"),
            None
        );
        assert_eq!(json_object_after("window.__x__ = ", "window.__x__"), None);
    }

    #[test]
    fn array_matching_is_the_same_scan_with_different_delimiters() {
        let text = r#"junk "video_versions":[{"url":"a]b"},{"url":"c"}] tail"#;
        assert_eq!(
            json_array_after(text, "\"video_versions\":"),
            Some(r#"[{"url":"a]b"},{"url":"c"}]"#)
        );
    }

    #[test]
    fn json_escapes_including_surrogate_pairs_are_decoded() {
        assert_eq!(
            decode_json_escapes(r"https:\/\/x.test\/a?b=1&c=2"),
            "https://x.test/a?b=1&c=2"
        );
        assert_eq!(decode_json_escapes(r#"a\nb\tc\\d\"e"#), "a\nb\tc\\d\"e");
        // U+1F600, which only round-trips if the two surrogate halves are recombined.
        assert_eq!(decode_json_escapes(r"\ud83d\ude00"), "\u{1f600}");
        assert_eq!(decode_json_escapes("nothing to do"), "nothing to do");
    }

    #[test]
    fn html_entities_are_decoded_and_stray_ampersands_survive() {
        assert_eq!(
            decode_html_entities("https://x.test/a?b=1&amp;c=2&#38;d=3&#x26;e=4"),
            "https://x.test/a?b=1&c=2&d=3&e=4"
        );
        assert_eq!(decode_html_entities("rock & roll"), "rock & roll");
        assert_eq!(decode_html_entities("&quot;q&quot; &lt;t&gt;"), "\"q\" <t>");
    }

    #[test]
    fn an_attribute_is_not_matched_inside_another_attributes_name() {
        let tag = r#" class="video-title" title="The real one""#;
        assert_eq!(attr(tag, "title").as_deref(), Some("The real one"));
        assert_eq!(attr(tag, "class").as_deref(), Some("video-title"));
        assert_eq!(attr(tag, "missing"), None);
    }

    #[test]
    fn a_script_body_is_found_by_its_id() {
        let html =
            r#"<script>other</script><script id="X" type="application/json">{"a":1}</script>"#;
        assert_eq!(script_tag_body(html, "X"), Some(r#"{"a":1}"#));
        assert_eq!(script_tag_body(html, "Y"), None);
    }

    #[test]
    fn bilibili_claims_its_own_hosts_and_not_lookalikes() {
        assert!(matches("www.bilibili.com"));
        assert!(matches("b23.tv"));
        assert!(matches("upos-hz.bilivideo.com"));
        assert!(!matches("notbilibili.com"));
        assert!(!matches("bilibili.com.evil.test"));
    }

    #[test]
    fn extraction_begins_by_asking_for_the_page_state() {
        let mut e = Bilibili::new();
        assert_eq!(
            e.start("https://www.bilibili.com/video/BV1").unwrap(),
            Step::Need(Need::PageState)
        );
    }

    #[test]
    fn a_dash_page_yields_one_option_per_quality_best_first() {
        let x = extract(DASH_PAGE);
        assert_eq!(x.site, "Bilibili");
        assert_eq!(x.title, "A video} with a brace");
        let labels: Vec<&str> = x.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["1080P", "480P", "Audio only · 192 kbps"]);
        assert!(x.options.windows(2).all(|w| w[0].rank >= w[1].rank));
        assert_eq!(x.options[0].height, Some(1080));
        assert_eq!(x.options[0].duration_ms, Some(123_000));
    }

    #[test]
    fn avc_wins_over_hevc_for_the_same_quality_so_the_merge_produces_a_playable_mp4() {
        let x = extract(DASH_PAGE);
        assert_eq!(
            x.options[0].streams[0].url,
            "https://cn.bilivideo.com/v/1080-avc.m4s"
        );
    }

    #[test]
    fn a_merged_option_pairs_video_with_the_highest_bitrate_audio() {
        let x = extract(DASH_PAGE);
        let kinds: Vec<StreamKind> = x.options[0].streams.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, [StreamKind::VideoOnly, StreamKind::AudioOnly]);
        assert_eq!(
            x.options[0].streams[1].url,
            "https://cn.bilivideo.com/a/192.m4s"
        );
    }

    #[test]
    fn every_stream_carries_the_referer_without_which_the_cdn_answers_403() {
        for page in [DASH_PAGE, DURL_PAGE] {
            let x = extract(page);
            let mut streams: Vec<(&str, &Stream)> = Vec::new();
            for option in &x.options {
                streams.extend(option.streams.iter().map(|s| (option.label.as_str(), s)));
            }
            // The choice lists are fetched exactly as the options are, so the header the
            // CDN refuses without is asserted over both.
            streams.extend(x.videos.iter().map(|v| (v.label.as_str(), &v.stream)));
            streams.extend(x.audios.iter().map(|a| (a.label.as_str(), &a.stream)));
            for (label, stream) in streams {
                assert!(
                    stream
                        .headers
                        .contains(&("Referer".to_string(), REFERER.to_string())),
                    "{label} is missing the Referer"
                );
                assert!(stream.headers.iter().any(|(k, _)| k == "User-Agent"));
            }
        }
    }

    #[test]
    fn every_dash_video_is_a_choice_whatever_its_codec() {
        // `options` shows one entry per quality and prefers AVC, because the merger writes
        // MP4. The choice list has no such duty, and hiding the HEVC rendition because the
        // one-click path cannot use it is what these lists exist to stop.
        let x = extract(DASH_PAGE);
        assert_eq!(video_ids(&x), vec!["v80-hev1", "v80-avc1", "v32-avc1"]);
        assert_eq!(
            x.videos
                .iter()
                .map(|v| v.label.as_str())
                .collect::<Vec<_>>(),
            vec![
                "1080P · HEVC · 2.5 Mbps",
                "1080P · AVC · 2.0 Mbps",
                "480P · AVC · 500 kbps"
            ]
        );
        for v in &x.videos {
            let codec = v.codec.as_deref().expect("no codec");
            assert!(v.label.contains(codec), "{} omits {codec}", v.label);
            assert!(!v.has_audio, "a dash video adaptation carries no sound");
        }
        assert_eq!(x.videos[0].fps, Some(30));
    }

    #[test]
    fn the_choice_lists_are_ranked_best_first_with_exactly_one_best_in_each() {
        let x = extract(DASH_PAGE);
        let heights: Vec<Option<u32>> = x.videos.iter().map(|v| v.height).collect();
        assert_eq!(heights, vec![Some(1080), Some(1080), Some(480)]);
        assert_eq!(audio_ids(&x), vec!["a30280", "a30216"]);
        assert_eq!(x.videos.iter().filter(|v| v.best).count(), 1);
        assert_eq!(x.audios.iter().filter(|a| a.best).count(), 1);
        assert_eq!(x.best_video().map(|v| v.id.as_str()), Some("v80-hev1"));
        assert_eq!(x.best_audio().map(|a| a.id.as_str()), Some("a30280"));
    }

    #[test]
    fn an_audio_choice_is_labelled_with_the_bitrate_its_quality_code_stands_for() {
        let x = extract(DASH_PAGE);
        assert_eq!(
            x.audios
                .iter()
                .map(|a| a.label.as_str())
                .collect::<Vec<_>>(),
            vec!["192 kbps · AAC", "64 kbps · AAC"]
        );
        assert_eq!(x.audios[0].bitrate, Some(192_000));
    }

    #[test]
    fn a_code_bilibili_has_not_published_yet_falls_back_to_the_stated_bandwidth() {
        // The published rate of a known code is truer than the bandwidth of one encode, but
        // an unknown code must still produce a label rather than being dropped.
        let html = r#"<title>New code_哔哩哔哩_bilibili</title><script>window.__playinfo__={"data":{"dash":{"video":[{"id":80,"baseUrl":"https://x/v.m4s","codecs":"avc1.640032","height":1080,"bandwidth":2000000}],"audio":[{"id":30999,"baseUrl":"https://x/a.m4s","codecs":"mp4a.40.2","bandwidth":320000}]}}}</script>"#;
        let x = extract(html);
        assert_eq!(x.audios[0].label, "320 kbps · AAC");
        assert_eq!(x.audios[0].bitrate, Some(320_000));
    }

    #[test]
    fn the_audio_quality_codes_are_the_bitrate_and_two_of_them_are_a_format() {
        // Bilibili's audio ids *are* the quality. `dash.dolby.audio[]` and `dash.flac.audio`
        // are not parsed anywhere in this file — no page carrying either has been captured
        // — but the two codes are named here so that when one turns up it reads correctly
        // rather than as "quality 30251".
        assert_eq!(audio_bitrate(30216), Some(64_000));
        assert_eq!(audio_bitrate(30232), Some(132_000));
        assert_eq!(audio_bitrate(30280), Some(192_000));
        assert_eq!(audio_bitrate(30250), None);
        assert_eq!(audio_quality_name(30250), Some("Dolby Atmos"));
        assert_eq!(audio_quality_name(30251), Some("Hi-Res"));
        assert_eq!(audio_quality_name(30280), None);
    }

    #[test]
    fn a_durl_page_yields_muxed_video_choices_and_nothing_to_pair_them_with() {
        let x = extract(DURL_PAGE);
        assert_eq!(x.videos.len(), 2);
        assert!(x.audios.is_empty(), "a muxed file needs no audio picked");
        for v in &x.videos {
            assert!(v.has_audio);
            assert_eq!(v.stream.kind, StreamKind::Muxed);
            assert!(v.label.starts_with("720P · part "), "{}", v.label);
        }
        // The shared ranking orders by picture and then by bytes, so the larger part comes
        // first here. Playback order is what `options` is for; this list answers "which
        // rendition", and a `durl` page only ever has the one.
        assert_eq!(video_ids(&x), vec!["v64-p2", "v64-p1"]);
    }

    #[test]
    fn a_choice_id_is_unique_within_its_own_list() {
        // A UI pairs a video id with an audio id, so two renditions answering to one name
        // is the user getting a different file from the one they chose. Bilibili publishes
        // the same quality code once per codec, which is a real collision, not a
        // hypothetical one.
        for page in [DASH_PAGE, DURL_PAGE] {
            let x = extract(page);
            for list in [video_ids(&x), audio_ids(&x)] {
                let mut sorted = list.clone();
                sorted.sort_unstable();
                sorted.dedup();
                assert_eq!(sorted.len(), list.len(), "duplicate id in {list:?}");
            }
        }
    }

    #[test]
    fn an_audio_only_option_is_offered_with_an_audio_extension() {
        let x = extract(DASH_PAGE);
        let audio = x.options.last().unwrap();
        assert_eq!(audio.streams.len(), 1);
        assert_eq!(audio.streams[0].kind, StreamKind::AudioOnly);
        assert!(audio.filename.ends_with(".m4a"), "{}", audio.filename);
    }

    #[test]
    fn a_durl_page_yields_muxed_parts_in_playback_order() {
        let x = extract(DURL_PAGE);
        assert_eq!(x.title, "Legacy clip");
        assert_eq!(x.options.len(), 2);
        assert_eq!(x.options[0].label, "720P · part 1");
        assert_eq!(x.options[1].label, "720P · part 2");
        assert_eq!(x.options[0].streams[0].kind, StreamKind::Muxed);
        assert_eq!(x.options[0].streams[0].size, Some(1_048_576));
        assert_eq!(x.options[0].duration_ms, Some(30_000));
        assert!(
            x.options[0].filename.ends_with(".flv"),
            "{}",
            x.options[0].filename
        );
    }

    #[test]
    fn a_single_durl_entry_is_not_labelled_as_a_part() {
        let html = r#"<title>One_哔哩哔哩_bilibili</title><script>window.__playinfo__={"data":{"quality":16,"durl":[{"url":"https://cn.bilivideo.com/x/1.mp4"}]}}</script>"#;
        let x = extract(html);
        assert_eq!(x.options[0].label, "360P");
        assert!(x.options[0].filename.ends_with(".mp4"));
    }

    #[test]
    fn an_unknown_quality_code_still_produces_a_readable_label() {
        assert_eq!(quality_name(999), "quality 999");
        assert_eq!(quality_rank(999), 999);
    }

    #[test]
    fn a_high_frame_rate_is_called_out_only_when_the_quality_name_omits_it() {
        let sixty = Rendition {
            id: 80,
            url: "u".into(),
            bandwidth: 1,
            codecs: "avc1".into(),
            mime: None,
            width: None,
            height: None,
            frame_rate: Some("59.94".into()),
        };
        assert_eq!(dash_label(&sixty), "1080P · 60fps");
        let named = Rendition {
            id: 116,
            ..sixty.clone()
        };
        assert_eq!(dash_label(&named), "1080P60");
        let ordinary = Rendition {
            frame_rate: Some("30".into()),
            ..sixty
        };
        assert_eq!(dash_label(&ordinary), "1080P");
    }

    #[test]
    fn a_protected_manifest_is_refused_rather_than_downloaded() {
        let drm = r#"<script>window.__playinfo__={"data":{"drm":true,"dash":{"video":[{"id":80,"baseUrl":"https://x/y.m4s"}]}}}</script>"#;
        let mut e = Bilibili::new();
        e.start("https://www.bilibili.com/video/BV1").unwrap();
        assert_eq!(e.feed(&[drm]), Err(SiteError::Encrypted));

        let cp = r#"<script>window.__playinfo__={"data":{"dash":{"ContentProtection":"widevine","video":[{"id":80,"baseUrl":"https://x/y.m4s"}]}}}</script>"#;
        let mut e = Bilibili::new();
        e.start("https://www.bilibili.com/video/BV1").unwrap();
        assert_eq!(e.feed(&[cp]), Err(SiteError::Encrypted));
    }

    #[test]
    fn a_falsy_drm_flag_is_bilibili_saying_this_one_is_fine() {
        let html = r#"<script>window.__playinfo__={"data":{"drm":false,"dash":{"video":[{"id":80,"baseUrl":"https://x/y.m4s","height":1080}],"audio":[]}}}</script>"#;
        let x = extract(html);
        assert_eq!(x.options.len(), 1);
    }

    /// Measured 8 September 2026: `accept_quality` advertises 1080P+ down to 360P and
    /// the signed-out answer carries only 480P and 360P. `qn=80` and `qn=112` change
    /// nothing, which is what makes it an account gate rather than a parameter.
    #[test]
    fn a_signed_out_answer_says_which_renditions_bilibili_kept_back() {
        let manifest = r#"{"code":0,"data":{"quality":32,
            "accept_quality":[112,80,64,32,16],
            "accept_description":["高清 1080P+","高清 1080P","高清 720P","清晰 480P","流畅 360P"],
            "dash":{"video":[{"id":32,"baseUrl":"https://x/v.m4s","height":480,
                              "codecs":"avc1.64001F"}],
                    "audio":[{"id":30280,"baseUrl":"https://x/a.m4s"}]}}}"#;
        let x = parse_playurl(manifest, "t".into()).unwrap();
        let note = x.note.expect("a capped answer must say so");
        assert!(note.contains("1080P+"), "{note}");
        assert!(
            note.contains("extension"),
            "names the way to get it: {note}"
        );
    }

    /// Nothing withheld, nothing said. A note on every result would be noise.
    #[test]
    fn an_answer_that_carries_the_best_rendition_says_nothing() {
        let manifest = r#"{"code":0,"data":{"quality":80,
            "accept_quality":[80,32],"accept_description":["高清 1080P","清晰 480P"],
            "dash":{"video":[{"id":80,"baseUrl":"https://x/v.m4s","height":1080,
                              "codecs":"avc1.64001F"}],
                    "audio":[{"id":30280,"baseUrl":"https://x/a.m4s"}]}}}"#;
        assert_eq!(parse_playurl(manifest, "t".into()).unwrap().note, None);
    }

    /// The shape bilibili actually serves now, captured 8 September 2026.
    ///
    /// `videoData` arrives as a stub — `owner` and `stat`, both ids gone — because the
    /// page hydrates itself after load, so a fetched copy never carries the `cid`. The
    /// bvid survives at the root. Before the `pagelist` hop this produced "the site has
    /// probably changed", which was accurate and no help to anyone.
    #[test]
    fn a_watch_page_whose_video_data_is_a_stub_asks_the_pagelist_api_for_the_cid() {
        let page = r#"<script>window.__INITIAL_STATE__={"bvid":"BV1GJ411x7h7",
            "videoData":{"owner":{"name":"someone"},"stat":{"view":1}}};</script>"#;
        let mut e = Bilibili::new();
        e.start("https://www.bilibili.com/video/BV1GJ411x7h7")
            .unwrap();

        let Step::Need(Need::Fetch(requests)) = e.feed(&[page]).unwrap() else {
            panic!("a page with no cid must ask for one");
        };
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .url
                .contains("x/player/pagelist?bvid=BV1GJ411x7h7"),
            "{}",
            requests[0].url
        );

        // The cid comes back, and only then is the player API asked.
        let Step::Need(Need::Fetch(requests)) = e
            .feed(&[r#"{"code":0,"data":[{"cid":137649199,"page":1}]}"#])
            .unwrap()
        else {
            panic!("the cid must lead to the player API");
        };
        assert!(
            requests[0].url.contains("bvid=BV1GJ411x7h7")
                && requests[0].url.contains("cid=137649199"),
            "{}",
            requests[0].url
        );
    }

    /// A page that still carries both ids must not spend the extra hop.
    #[test]
    fn a_watch_page_that_still_has_both_ids_goes_straight_to_the_player_api() {
        let page = r#"<script>window.__INITIAL_STATE__={
            "videoData":{"bvid":"BV1GJ411x7h7","cid":137649199}};</script>"#;
        let mut e = Bilibili::new();
        e.start("https://www.bilibili.com/video/BV1GJ411x7h7")
            .unwrap();
        let Step::Need(Need::Fetch(requests)) = e.feed(&[page]).unwrap() else {
            panic!("expected the player API");
        };
        assert!(
            requests[0].url.contains("x/player/playurl"),
            "{}",
            requests[0].url
        );
    }

    #[test]
    fn a_pagelist_answer_with_no_parts_in_it_reports_shape() {
        let page = r#"<script>window.__INITIAL_STATE__={"bvid":"BV1GJ411x7h7",
            "videoData":{"owner":{}}};</script>"#;
        let mut e = Bilibili::new();
        e.start("https://www.bilibili.com/video/BV1GJ411x7h7")
            .unwrap();
        e.feed(&[page]).unwrap();
        assert!(matches!(
            e.feed(&[r#"{"code":0,"data":[]}"#]),
            Err(SiteError::Shape(_))
        ));
    }

    /// The first thing to try is a reload, so that is what the message says first.
    #[test]
    fn a_page_that_names_no_video_asks_for_a_reload_and_names_the_site() {
        let mut e = Bilibili::new();
        e.start("https://www.bilibili.com/video/BV1").unwrap();
        let err = e.feed(&["<html>nothing useful</html>"]).unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, SiteError::Unavailable(_)), "{err:?}");
        assert!(text.contains("Bilibili"), "{text}");
        assert!(text.contains("Reload"), "{text}");
        assert!(!text.starts_with("Bilibili did not return"), "{text}");
    }

    /// APP-80, in the shape the extension actually reads it: the live DOM of a first
    /// visit from a fresh profile, captured 13 September 2026 with Playwright. The
    /// `__INITIAL_STATE__` script has been removed by the page after it ran — only
    /// conditionals that *mention* it are left — and `__playinfo__` is absent entirely.
    /// The bvid is still in the markup, in the canonical link.
    #[test]
    fn a_first_visit_with_neither_state_nor_manifest_in_the_markup_still_reaches_the_player_api() {
        let page = r#"<html><head><title>iPhone Duo上手体验！折痕控制太离谱了_哔哩哔哩_bilibili</title>
            <link rel="canonical" href="https://www.bilibili.com/video/BV1FDYb6qEQ5/"></head>
            <body><script>window.webAbTest||(window.webAbTest={});try{window.__INITIAL_STATE__&&
            (window.webAbTest.pageVersion=window.__INITIAL_STATE__.pageVersion)}catch(w){}</script>
            </body></html>"#;
        let mut e = Bilibili::new();
        e.start("https://www.bilibili.com/video/BV1FDYb6qEQ5/?spm_id_from=333.1007")
            .unwrap();

        let Step::Need(Need::Fetch(requests)) = e.feed(&[page]).unwrap() else {
            panic!("a first visit must ask for the cid, not fail");
        };
        assert!(
            requests[0].url.contains("x/player/pagelist?bvid=BV1FDYb6qEQ5"),
            "{}",
            requests[0].url
        );
        let Step::Need(Need::Fetch(requests)) = e
            .feed(&[r#"{"code":0,"data":[{"cid":41739422307,"page":1}]}"#])
            .unwrap()
        else {
            panic!("the cid must lead to the player API");
        };
        assert!(
            requests[0].url.contains("bvid=BV1FDYb6qEQ5") && requests[0].url.contains("cid=41739422307"),
            "{}",
            requests[0].url
        );
    }

    /// The URL is only trusted once the page agrees with it. A tab whose markup never
    /// mentions the id is not shown to be that video, and asking the API anyway would
    /// trade a clear message for a vaguer one a hop later.
    #[test]
    fn the_id_in_the_url_is_not_used_when_the_page_never_mentions_it() {
        let mut e = Bilibili::new();
        e.start("https://www.bilibili.com/video/BV1FDYb6qEQ5/").unwrap();
        assert!(matches!(
            e.feed(&["<html><title>验证码</title></html>"]),
            Err(SiteError::Unavailable(_))
        ));
    }

    /// `?p=2` is the second episode. Taking the first entry from `pagelist` would download
    /// the wrong one without any error at all.
    #[test]
    fn a_multi_part_url_gets_the_cid_of_the_part_it_names() {
        let list = r#"{"code":0,"data":[{"cid":111,"page":1},{"cid":222,"page":2},{"cid":333,"page":3}]}"#;
        assert_eq!(parse_pagelist(list, 1).as_deref(), Some("111"));
        assert_eq!(parse_pagelist(list, 2).as_deref(), Some("222"));
        assert_eq!(parse_pagelist(list, 4), None, "a part that is not there is not part 1");
        assert_eq!(url_part("https://www.bilibili.com/video/BV1x/?p=2"), 2);
        assert_eq!(url_part("https://www.bilibili.com/video/BV1x/?spm=a&p=3#t"), 3);
        assert_eq!(url_part("https://www.bilibili.com/video/BV1x/?spm_id_from=p=9"), 1);
        assert_eq!(url_part("https://www.bilibili.com/video/BV1x/"), 1);
    }

    #[test]
    fn a_manifest_with_neither_dash_nor_durl_reports_shape() {
        let mut e = Bilibili::new();
        e.start("https://www.bilibili.com/video/BV1").unwrap();
        assert!(matches!(
            e.feed(&[r#"<script>window.__playinfo__={"code":0,"data":{}}</script>"#]),
            Err(SiteError::Shape(_))
        ));
    }

    #[test]
    fn a_manifest_whose_entries_carry_no_url_reports_shape_rather_than_an_empty_menu() {
        let mut e = Bilibili::new();
        e.start("https://www.bilibili.com/video/BV1").unwrap();
        assert!(matches!(
            e.feed(&[
                r#"<script>window.__playinfo__={"data":{"dash":{"video":[{"id":80}]}}}</script>"#
            ]),
            Err(SiteError::Shape(_))
        ));
    }

    #[test]
    fn a_backup_url_rescues_a_rendition_with_no_base_url() {
        let html = r#"<script>window.__playinfo__={"data":{"dash":{"video":[{"id":80,"baseUrl":"","backupUrl":["https://backup.bilivideo.com/v/a.m4s"],"height":1080}]}}}</script>"#;
        let x = extract(html);
        assert_eq!(
            x.options[0].streams[0].url,
            "https://backup.bilivideo.com/v/a.m4s"
        );
    }

    #[test]
    fn the_title_falls_back_from_state_to_h1_to_the_branded_document_title() {
        let h1 = r#"<title>ignored_哔哩哔哩_bilibili</title><h1 class="video-title" title="From the h1 &amp; nowhere else">x</h1><script>window.__playinfo__={"data":{"durl":[{"url":"https://x/y.mp4"}]}}</script>"#;
        assert_eq!(extract(h1).title, "From the h1 & nowhere else");

        let doc = r#"<title>Just the document title_哔哩哔哩_bilibili</title><script>window.__playinfo__={"data":{"durl":[{"url":"https://x/y.mp4"}]}}</script>"#;
        assert_eq!(extract(doc).title, "Just the document title");
    }

    #[test]
    fn with_no_title_anywhere_the_url_supplies_the_video_id() {
        let mut e = Bilibili::new();
        e.start("https://www.bilibili.com/video/BV1xx411c7mD?t=3")
            .unwrap();
        let Step::Done(x) = e
            .feed(&[r#"<script>window.__playinfo__={"data":{"durl":[{"url":"https://x/y.mp4"}]}}</script>"#])
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(x.title, "BV1xx411c7mD");
    }

    #[test]
    fn feeding_nothing_reports_shape_instead_of_panicking() {
        let mut e = Bilibili::new();
        e.start("https://www.bilibili.com/video/BV1").unwrap();
        assert!(matches!(e.feed(&[]), Err(SiteError::Shape(_))));
    }
}

#[cfg(test)]
mod assignment_matching {
    use super::*;

    #[test]
    fn a_mention_in_a_conditional_is_not_an_assignment() {
        // The exact shape bilibili's player bootstrap ships, which used to be mistaken
        // for the manifest and parsed as JSON.
        let html = r#"<script>if (window.__playinfo__) { primary.prefetch = { playUrl: window.__playinfo__ } }</script>"#;
        assert!(json_object_assigned_to(html, "window.__playinfo__").is_none());
    }

    #[test]
    fn the_assignment_is_found_even_when_a_mention_comes_first() {
        let html = concat!(
            r#"<script>if (window.__playinfo__) { x = 1 }</script>"#,
            r#"<script>window.__playinfo__={"code":0,"data":{"dash":{}}}</script>"#,
        );
        let found = json_object_assigned_to(html, "window.__playinfo__").expect("assignment");
        assert_eq!(found, r#"{"code":0,"data":{"dash":{}}}"#);
    }

    #[test]
    fn whitespace_around_the_equals_is_tolerated() {
        let html = r#"window.__INITIAL_STATE__   =   {"videoData":{"bvid":"BV1","cid":42}};"#;
        let ids = video_ids(html).expect("ids");
        assert_eq!(ids.bvid, "BV1");
        assert_eq!(ids.cid, "42");
    }

    #[test]
    fn an_equality_test_is_not_an_assignment() {
        let html = r#"if (window.__playinfo__ == {a:1}) {}"#;
        assert!(json_object_assigned_to(html, "window.__playinfo__").is_none());
    }

    #[test]
    fn a_page_with_no_manifest_still_yields_the_ids_the_api_needs() {
        let html = concat!(
            r#"<script>window.__INITIAL_STATE__={"videoData":{"bvid":"BV1m6bL6wEeG","cid":41615295412}};</script>"#,
            r#"<script>if (window.__playinfo__) { prefetch() }</script>"#,
        );
        assert!(json_object_assigned_to(html, "window.__playinfo__").is_none());
        let ids = video_ids(html).expect("ids");
        assert_eq!(ids.bvid, "BV1m6bL6wEeG");
        // Serialised as a number too large for an f64 round trip; it must survive intact.
        assert_eq!(ids.cid, "41615295412");
        assert!(playurl_request(&ids).url.contains("cid=41615295412"));
    }
}
