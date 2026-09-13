//! The JavaScript-facing surface.
//!
//! Every export here is a thin shell over a type that already exists and is already
//! tested natively. Nothing decides anything at this layer — if logic appears in this
//! file, it has escaped the native test suite and belongs somewhere else.
//!
//! Structured values cross the boundary as JSON strings rather than as `serde-wasm-bindgen`
//! objects. JSON costs a serialization step on a call that happens a few times per
//! download, and in exchange the boundary stays trivially debuggable from devtools.

use wasm_bindgen::prelude::*;

use crate::classify;
use crate::hls;
use crate::policy;
use crate::session;
use crate::subs;

/// Classify one observed response. Input is a `RequestMeta` as JSON; output is a
/// `MediaCandidate` as JSON, or `undefined` when the response is not worth offering.
#[wasm_bindgen]
pub fn classify_request(meta_json: &str) -> Option<String> {
    let meta: classify::RequestMeta = serde_json::from_str(meta_json).ok()?;
    let candidate = classify::classify(&meta)?;
    serde_json::to_string(&candidate).ok()
}

/// Parse an HLS playlist, resolving its URLs against `base_url`.
#[wasm_bindgen]
pub fn parse_playlist_js(text: &str, base_url: &str) -> Result<String, JsValue> {
    let playlist =
        hls::parse_playlist(text, base_url).map_err(|e| JsValue::from_str(&e.to_string()))?;
    serde_json::to_string(&playlist).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Pick a rendition from a JSON array of variants, returning its index.
#[wasm_bindgen]
pub fn select_variant_index(variants_json: &str, prefer_highest: bool) -> Option<usize> {
    let variants: Vec<hls::Variant> = serde_json::from_str(variants_json).ok()?;
    let chosen = hls::select_variant(&variants, prefer_highest)?;
    variants.iter().position(|v| v == chosen)
}

/// Whether a URL pair is barred by policy. Exposed so the UI can explain a refusal
/// rather than silently showing nothing.
#[wasm_bindgen]
pub fn is_restricted(page_origin: &str, media_url: &str) -> bool {
    policy::is_restricted(page_origin, media_url)
}

/// Stitch HLS WebVTT segments (a JSON array of strings) into one WebVTT document.
#[wasm_bindgen]
pub fn merge_vtt_segments_js(segments_json: &str) -> Result<String, JsValue> {
    let segments: Vec<String> =
        serde_json::from_str(segments_json).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let refs: Vec<&str> = segments.iter().map(String::as_str).collect();
    Ok(subs::merge_vtt_segments(&refs))
}

/// Convert a WebVTT document to SRT.
#[wasm_bindgen]
pub fn vtt_to_srt(vtt: &str) -> String {
    subs::vtt_to_srt(vtt)
}

/// Parse WebVTT or SRT into a JSON array of cues.
#[wasm_bindgen]
pub fn parse_cues_js(text: &str) -> String {
    serde_json::to_string(&subs::parse_cues(text)).unwrap_or_else(|_| "[]".to_string())
}

/// Render a JSON array of cues as WebVTT.
#[wasm_bindgen]
pub fn cues_to_vtt_js(cues_json: &str) -> Result<String, JsValue> {
    let cues: Vec<subs::Cue> =
        serde_json::from_str(cues_json).map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(subs::cues_to_vtt(&cues))
}

/// Render a JSON array of cues as SRT.
#[wasm_bindgen]
pub fn cues_to_srt_js(cues_json: &str) -> Result<String, JsValue> {
    let cues: Vec<subs::Cue> =
        serde_json::from_str(cues_json).map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(subs::cues_to_srt(&cues))
}

/// One download job.
#[wasm_bindgen(js_name = DownloadSession)]
pub struct WasmSession {
    inner: session::DownloadSession,
}

#[wasm_bindgen(js_class = DownloadSession)]
impl WasmSession {
    /// `total` may be omitted when the server does not report a length.
    #[wasm_bindgen(constructor)]
    pub fn new(
        total: Option<u64>,
        accepts_ranges: bool,
        remux: bool,
        audio_only: bool,
    ) -> WasmSession {
        WasmSession {
            inner: session::DownloadSession::new(total, accepts_ranges, remux, audio_only),
        }
    }

    /// Rebuild from `stateJson()` after a reload.
    pub fn restore(state_json: &str) -> Result<WasmSession, JsValue> {
        session::DownloadSession::restore(state_json)
            .map(|inner| WasmSession { inner })
            .map_err(|e| JsValue::from_str(&e))
    }

    /// The byte ranges to fetch next, as JSON `[{"start":n,"end":n}]`.
    pub fn plan(&self, chunk_size: u64, max_parallel: usize) -> String {
        serde_json::to_string(&self.inner.plan(chunk_size, max_parallel))
            .unwrap_or_else(|_| "[]".to_string())
    }

    /// Mark a range as written to the sink.
    pub fn record(&mut self, start: u64, end: u64) {
        self.inner.record(start, end);
    }

    #[wasm_bindgen(js_name = setValidator)]
    pub fn set_validator(&mut self, validator: Option<String>) {
        self.inner.set_validator(validator);
    }

    /// Feed one HLS segment; returns the bytes to append to the output file.
    #[wasm_bindgen(js_name = pushSegment)]
    pub fn push_segment(&mut self, bytes: &[u8]) -> Result<Vec<u8>, JsValue> {
        self.inner
            .push_segment(bytes)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = hashUpdate)]
    pub fn hash_update(&mut self, bytes: &[u8]) {
        self.inner.hash_update(bytes);
    }

    #[wasm_bindgen(js_name = hashHex)]
    pub fn hash_hex(&self) -> String {
        self.inner.hash_hex()
    }

    #[wasm_bindgen(js_name = hashedLen)]
    pub fn hashed_len(&self) -> u64 {
        self.inner.hashed_len()
    }

    #[wasm_bindgen(js_name = resetHash)]
    pub fn reset_hash(&mut self) {
        self.inner.reset_hash();
    }

    /// Also compute the eD2k hash during the read-back. Only for a job that carries one
    /// to check — see `Session::enable_ed2k`.
    #[wasm_bindgen(js_name = enableEd2k)]
    pub fn enable_ed2k(&mut self) {
        self.inner.enable_ed2k();
    }

    #[wasm_bindgen(js_name = ed2kHex)]
    pub fn ed2k_hex(&self) -> Option<String> {
        self.inner.ed2k_hex()
    }

    #[wasm_bindgen(js_name = noteOutput)]
    pub fn note_output(&mut self, len: u64) {
        self.inner.note_output(len);
    }

    #[wasm_bindgen(js_name = stateJson)]
    pub fn state_json(&self) -> String {
        self.inner.state_json()
    }

    pub fn downloaded(&self) -> u64 {
        self.inner.downloaded()
    }

    #[wasm_bindgen(js_name = outputLen)]
    pub fn output_len(&self) -> u64 {
        self.inner.output_len()
    }

    #[wasm_bindgen(js_name = nextSegment)]
    pub fn next_segment(&self) -> usize {
        self.inner.next_segment()
    }

    #[wasm_bindgen(js_name = isComplete)]
    pub fn is_complete(&self) -> bool {
        self.inner.is_complete()
    }

    /// Progress as a fraction, or `undefined` when the total size is unknown.
    pub fn fraction(&self) -> Option<f64> {
        self.inner.fraction()
    }
}

/// Extracting the audio track of a progressive MP4, losslessly.
///
/// Unlike [`WasmSession`], which is fed a stream of segments as they arrive, this is
/// driven the other way around: it reads a `moov` box, says which byte ranges of the
/// file it needs, and the caller supplies them. That inversion is what keeps a
/// multi-gigabyte video from having to fit in memory to give up its soundtrack.
#[wasm_bindgen(js_name = Mp4AudioExtractor)]
pub struct WasmMp4AudioExtractor {
    inner: dl_container::AudioExtractor,
    hasher: crate::integrity::Hasher,
}

#[wasm_bindgen(js_class = Mp4AudioExtractor)]
impl WasmMp4AudioExtractor {
    /// Index a complete `moov` box (its header included).
    #[wasm_bindgen(js_name = fromMoov)]
    pub fn from_moov(moov: &[u8]) -> Result<WasmMp4AudioExtractor, JsValue> {
        dl_container::AudioExtractor::from_moov(moov)
            .map(|inner| WasmMp4AudioExtractor {
                inner,
                hasher: crate::integrity::Hasher::new(),
            })
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// The file ranges to read, in order, as JSON `[{"offset":n,"len":n,"sample_count":n}]`.
    pub fn chunks(&self) -> String {
        serde_json::to_string(self.inner.chunks()).unwrap_or_else(|_| "[]".to_string())
    }

    /// Feed chunk `index`; returns the fMP4 bytes to append to the output.
    #[wasm_bindgen(js_name = pushChunk)]
    pub fn push_chunk(&mut self, index: usize, bytes: &[u8]) -> Result<Vec<u8>, JsValue> {
        self.inner
            .push_chunk(index, bytes)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = sampleRate)]
    pub fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    pub fn channels(&self) -> u8 {
        self.inner.channels()
    }

    /// Track duration in the track's own timescale, which is normally the sample rate.
    pub fn duration(&self) -> u64 {
        self.inner.duration()
    }

    pub fn timescale(&self) -> u32 {
        self.inner.timescale()
    }

    #[wasm_bindgen(js_name = isComplete)]
    pub fn is_complete(&self) -> bool {
        self.inner.is_complete()
    }

    // The same read-back digest a download produces, so a file extracted here and a
    // file downloaded can be compared with one vocabulary.

    #[wasm_bindgen(js_name = hashUpdate)]
    pub fn hash_update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    #[wasm_bindgen(js_name = hashHex)]
    pub fn hash_hex(&self) -> String {
        self.hasher.finish_hex()
    }

    #[wasm_bindgen(js_name = resetHash)]
    pub fn reset_hash(&mut self) {
        self.hasher = crate::integrity::Hasher::new();
    }
}

/// Parse an `ed2k://` link into JSON, or `None` when it is not one.
///
/// A file link becomes `{"kind":"file","filename":…,"size":…,"hash":…,"httpSource":…}`.
/// The `httpSource` is the only part a browser can act on by itself; without it the
/// bytes live only on the eDonkey network, and the caller says so.
#[wasm_bindgen]
pub fn parse_ed2k_link(uri: &str) -> Option<String> {
    use crate::ed2k::Ed2kUri;
    let value = match crate::ed2k::parse(uri)? {
        Ed2kUri::File(f) => serde_json::json!({
            "kind": "file",
            "filename": f.filename,
            "size": f.size,
            "hash": f.hash,
            "httpSource": f.http_source,
            "aich": f.aich,
        }),
        Ed2kUri::Server { host, port } => {
            serde_json::json!({ "kind": "server", "host": host, "port": port })
        }
        Ed2kUri::ServerList(url) => serde_json::json!({ "kind": "serverlist", "url": url }),
    };
    serde_json::to_string(&value).ok()
}

/// Decode a `thunder://`, `flashget://` or `qqdl://` link into the URL inside it.
///
/// `None` when the link is not one of those, or when what it hides is not something this
/// can download.
#[wasm_bindgen]
pub fn resolve_download_link(url: &str) -> Option<String> {
    crate::links::resolve_download_link(url)
}

/// Why a `magnet:`, `.torrent` or `ed2k://` link cannot be downloaded here, or `None`.
#[wasm_bindgen]
pub fn peer_link_refusal(url: &str) -> Option<String> {
    crate::links::peer_link_refusal(url)
}

/// Read a Mega file link, or `None` when it is not one.
///
/// `{"handle":…,"key":…,"nonce":…,"metaMac":…}`, the three byte fields base64 so the
/// caller can `atob` them straight into WebCrypto. The key never went near a network:
/// it lives in the URL fragment, which browsers do not send.
#[wasm_bindgen]
pub fn parse_mega_link(url: &str) -> Option<String> {
    let link = crate::mega::parse(url)?;
    serde_json::to_string(&serde_json::json!({
        "handle": link.handle,
        "key": crate::mega::base64_encode(&link.key),
        "nonce": crate::mega::base64_encode(&link.nonce),
        "metaMac": crate::mega::base64_encode(&link.meta_mac),
    }))
    .ok()
}

/// Whether this URL is a Mega *folder* link, which names a directory rather than a file.
#[wasm_bindgen]
pub fn is_mega_folder_link(url: &str) -> bool {
    crate::mega::is_folder_link(url)
}

/// Read a Quark share link, or `None` when it is not one.
///
/// `{"pwdId":…,"passcode":…}` — the two values the share's token call takes.
#[wasm_bindgen]
pub fn parse_quark_share(url: &str) -> Option<String> {
    let share = crate::quark::parse(url)?;
    serde_json::to_string(&serde_json::json!({
        "pwdId": share.pwd_id,
        "passcode": share.passcode,
    }))
    .ok()
}

/// Whether a stream is an HLS playlist rather than a file.
///
/// Front ends ask before deciding what kind of job to create; guessing produced a
/// playlist saved as a video and marked verified.
#[wasm_bindgen]
pub fn stream_is_hls_playlist(mime: Option<String>, url: &str) -> bool {
    crate::classify::is_hls_playlist(mime.as_deref(), url)
}

/// Whether any site extractor claims this URL.
#[wasm_bindgen]
pub fn site_is_supported(url: &str) -> bool {
    crate::sites::is_supported(url)
}

/// The name of the site that would handle this URL, for the UI.
#[wasm_bindgen]
pub fn site_name_for(url: &str) -> Option<String> {
    crate::sites::site_for(url).map(str::to_string)
}

/// Whether a URL is one of Vimeo's JSON adaptive manifests.
#[wasm_bindgen]
pub fn is_vimeo_manifest(url: &str) -> bool {
    crate::sites::vimeo_adaptive::is_adaptive_playlist(url)
}

/// Read Vimeo's JSON adaptive manifest, or `None` when it is not one.
///
/// Returns the renditions with every segment URL already absolute and every byte offset
/// laid out, so a caller can serve any range of a rendition by fetching only the
/// segments it covers.
#[wasm_bindgen]
pub fn parse_vimeo_manifest(json: &str, playlist_url: &str) -> Option<String> {
    let parsed = crate::sites::vimeo_adaptive::parse(json, playlist_url)?;
    serde_json::to_string(&parsed).ok()
}

/// Which track a sniffed URL carries: `"video"`, `"audio"` or `"muxed"`.
#[wasm_bindgen]
pub fn track_kind(url: &str, mime: Option<String>) -> String {
    crate::classify::track_kind(url, mime.as_deref()).to_string()
}

/// Host patterns to request permission for alongside a site's own page, as JSON.
///
/// `["*://*.zjcdn.com/*", …]` — the CDNs that site streams from. Without these the
/// network listener never sees the media on a site that plays through MSE, and the popup
/// offers nothing while looking correctly set up.
#[wasm_bindgen]
pub fn media_host_patterns(url: &str) -> String {
    let patterns: Vec<String> = crate::sites::media_hosts(url)
        .into_iter()
        .map(|h| format!("*://*.{h}/*"))
        .collect();
    serde_json::to_string(&patterns).unwrap_or_else(|_| "[]".to_string())
}

/// Every non-extractor source this build accepts, as JSON.
///
/// `[{"name":…,"accepts":…,"needsLocalHelper":bool}]`. Separate from `supported_sites`
/// because these have no page to extract from and no single URL shape.
#[wasm_bindgen]
pub fn supported_sources() -> String {
    let sources: Vec<_> = crate::sites::supported_sources()
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "accepts": s.accepts,
                "needsLocalHelper": s.needs_local_helper,
            })
        })
        .collect();
    serde_json::to_string(&sources).unwrap_or_else(|_| "[]".to_string())
}

/// Every site with a dedicated extractor in this build, as JSON.
///
/// `[{"name":…,"host":…,"withoutATab":bool,"fromAnyOrigin":bool}]`. `withoutATab` is
/// false for the sites that can only be read from a loaded page. `fromAnyOrigin` is the
/// narrower one a hosted page needs: the site answers a page on another domain at all.
/// The web app marks a site as working on the page only when that holds, or a relay it
/// can reach makes up the difference.
#[wasm_bindgen]
pub fn supported_sites() -> String {
    let sites: Vec<_> = crate::sites::supported_sites()
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                // The host, not the whole probe URL: the example carries a placeholder id
                // that exists for the consistency test, and showing it would read as a
                // link worth clicking.
                "host": crate::policy::host_of(s.example).unwrap_or_default(),
                "withoutATab": s.without_a_tab,
                "fromAnyOrigin": s.from_any_origin,
            })
        })
        .collect();
    serde_json::to_string(&sites).unwrap_or_else(|_| "[]".to_string())
}

/// Whether a host with no tab to read can resolve this URL at all.
///
/// This is the one a front end should ask before refusing a link. See the note on
/// `sites::site_works_without_a_tab`: the narrower question below answers only half of
/// it, and asking that alone reports YouTube as extension-only.
#[wasm_bindgen]
pub fn site_works_without_a_tab(url: &str) -> bool {
    crate::sites::site_works_without_a_tab(url)
}

/// Whether this URL's extractor can work from a fetched page rather than a loaded tab.
///
/// A host with no tab to read — the web app — asks this before offering to fetch the
/// page itself, so that the sites where that genuinely cannot work still get the plain
/// "use the extension there" answer instead of a parse failure.
#[wasm_bindgen]
pub fn site_accepts_fetched_page(url: &str) -> bool {
    crate::sites::site_accepts_fetched_page(url)
}

/// Driving one site's extractor.
///
/// The extractor performs no I/O of its own: it answers with what it needs — a fetch, or
/// the loaded page's HTML — and the caller supplies it. Keeping the loop on the
/// JavaScript side is what lets the extension satisfy `PageState` by reading the tab the
/// user is looking at, which is the only thing that works on the sites that refuse
/// requests from outside a browser.
#[wasm_bindgen(js_name = SiteExtractor)]
pub struct WasmSiteExtractor {
    inner: Box<dyn crate::sites::Extractor>,
    url: String,
}

#[wasm_bindgen(js_class = SiteExtractor)]
impl WasmSiteExtractor {
    #[wasm_bindgen(constructor)]
    pub fn new(url: &str) -> Result<WasmSiteExtractor, JsValue> {
        let inner = crate::sites::extractor_for(url)
            .ok_or_else(|| JsValue::from_str(&crate::sites::SiteError::Unsupported.to_string()))?;
        Ok(WasmSiteExtractor {
            inner,
            url: url.to_string(),
        })
    }

    /// The site's display name.
    pub fn site(&self) -> String {
        self.inner.site().to_string()
    }

    /// Begin. Returns a `Step` as JSON.
    pub fn start(&mut self) -> Result<String, JsValue> {
        let step = self
            .inner
            .start(&self.url)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        serde_json::to_string(&step).map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// Feed the bodies of the previous step's `Need`, as a JSON array of strings.
    pub fn feed(&mut self, bodies_json: &str) -> Result<String, JsValue> {
        let bodies: Vec<String> = serde_json::from_str(bodies_json)
            .map_err(|e| JsValue::from_str(&format!("bad bodies payload: {e}")))?;
        let refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
        let step = self
            .inner
            .feed(&refs)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        serde_json::to_string(&step).map_err(|e| JsValue::from_str(&e.to_string()))
    }
}

/// Merging a video-only and an audio-only MP4 into one file.
///
/// The platforms that need this — YouTube above 360p, Bilibili's DASH — no longer serve
/// a combined file at all, so "1080p with sound" is two downloads and this step. Nothing
/// is re-encoded: the samples are copied and only their framing changes.
///
/// Driven the same way [`WasmMp4AudioExtractor`] is. It states the byte ranges it needs,
/// in the order the output wants them, and the caller supplies them — so a merge streams
/// rather than holding either input in memory.
#[wasm_bindgen(js_name = Muxer)]
pub struct WasmMuxer {
    inner: dl_container::Muxer,
    hasher: crate::integrity::Hasher,
}

#[wasm_bindgen(js_class = Muxer)]
impl WasmMuxer {
    /// Index both inputs from their `moov` boxes, headers included.
    #[wasm_bindgen(js_name = fromMoovs)]
    pub fn from_moovs(video_moov: &[u8], audio_moov: &[u8]) -> Result<WasmMuxer, JsValue> {
        dl_container::Muxer::from_moovs(video_moov, audio_moov)
            .map(|inner| WasmMuxer {
                inner,
                hasher: crate::integrity::Hasher::new(),
            })
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// The reads to perform, in order, as JSON
    /// `[{"source":"Video"|"Audio","offset":n,"len":n,"sample_count":n}]`.
    pub fn reads(&self) -> String {
        serde_json::to_string(self.inner.reads()).unwrap_or_else(|_| "[]".to_string())
    }

    /// Feed read `index`; returns the fMP4 bytes to append.
    pub fn push(&mut self, index: usize, bytes: &[u8]) -> Result<Vec<u8>, JsValue> {
        self.inner
            .push(index, bytes)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = isComplete)]
    pub fn is_complete(&self) -> bool {
        self.inner.is_complete()
    }

    #[wasm_bindgen(js_name = durationMs)]
    pub fn duration_ms(&self) -> u64 {
        self.inner.duration_ms()
    }

    // The same read-back digest an ordinary download produces, so a merged file and a
    // downloaded one are described in one vocabulary.

    #[wasm_bindgen(js_name = hashUpdate)]
    pub fn hash_update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    #[wasm_bindgen(js_name = hashHex)]
    pub fn hash_hex(&self) -> String {
        self.hasher.finish_hex()
    }

    #[wasm_bindgen(js_name = hashedLen)]
    pub fn hashed_len(&self) -> u64 {
        self.hasher.len()
    }

    #[wasm_bindgen(js_name = resetHash)]
    pub fn reset_hash(&mut self) {
        self.hasher = crate::integrity::Hasher::new();
    }
}

/// Merging a **fragmented** video stream with a fragmented audio stream.
///
/// The sibling [`WasmMuxer`] reads progressive sample tables; this reads neither, because
/// a fragmented file has none worth reading — its tables are deliberately empty and each
/// `moof` describes itself. That is what YouTube and Bilibili actually serve, so this is
/// the path a real platform download takes.
///
/// Merging is renumbering rather than rewriting: the audio input's fragments are made to
/// claim track 2, one sequence counter is shared across both, and every sample passes
/// through untouched.
#[wasm_bindgen(js_name = FragmentMerger)]
pub struct WasmFragmentMerger {
    inner: dl_container::FragmentMerger,
    hasher: crate::integrity::Hasher,
}

#[wasm_bindgen(js_class = FragmentMerger)]
impl WasmFragmentMerger {
    /// Each input's bytes from 0 up to and including its `sidx` — in practice the first
    /// 256 KiB, which the caller has already fetched to decide which merger applies.
    #[wasm_bindgen(js_name = fromHeads)]
    pub fn from_heads(video_head: &[u8], audio_head: &[u8]) -> Result<WasmFragmentMerger, JsValue> {
        dl_container::FragmentMerger::from_heads(video_head, audio_head)
            .map(|inner| WasmFragmentMerger {
                inner,
                hasher: crate::integrity::Hasher::new(),
            })
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// Index inputs that arrive as several self-indexing pieces.
    ///
    /// Each side is passed flat — every piece's bytes concatenated, the length of each,
    /// and the offset each begins at in its own stream — because a list of byte arrays
    /// does not cross this boundary cheaply and the flat form needs no allocation on
    /// either side beyond the slices it is given.
    ///
    /// Used where a rendition is an init segment plus media segments that each carry
    /// their own `sidx`, which is how Vimeo's adaptive format ships one.
    #[wasm_bindgen(js_name = fromSegmentHeads)]
    pub fn from_segment_heads(
        video_blob: &[u8],
        video_lens: &[u32],
        video_offsets: &[f64],
        audio_blob: &[u8],
        audio_lens: &[u32],
        audio_offsets: &[f64],
    ) -> Result<WasmFragmentMerger, JsValue> {
        fn spans<'a>(
            blob: &'a [u8],
            lens: &[u32],
            offsets: &[f64],
        ) -> Result<Vec<(&'a [u8], u64)>, JsValue> {
            if lens.len() != offsets.len() {
                return Err(JsValue::from_str("a span is missing its length or offset"));
            }
            let mut out = Vec::with_capacity(lens.len());
            let mut at = 0usize;
            for (len, offset) in lens.iter().zip(offsets) {
                let len = *len as usize;
                let end = at
                    .checked_add(len)
                    .filter(|e| *e <= blob.len())
                    .ok_or_else(|| {
                        JsValue::from_str("the span lengths do not add up to the bytes given")
                    })?;
                out.push((&blob[at..end], *offset as u64));
                at = end;
            }
            Ok(out)
        }

        let video = spans(video_blob, video_lens, video_offsets)?;
        let audio = spans(audio_blob, audio_lens, audio_offsets)?;
        dl_container::FragmentMerger::from_segment_heads(&video, &audio)
            .map(|inner| WasmFragmentMerger {
                inner,
                hasher: crate::integrity::Hasher::new(),
            })
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// The `moof`+`mdat` ranges to fetch, interleaved by decode time, as JSON
    /// `[{"source":"Video"|"Audio","offset":n,"len":n}]`.
    pub fn reads(&self) -> String {
        serde_json::to_string(self.inner.reads()).unwrap_or_else(|_| "[]".to_string())
    }

    /// Feed read `index`; returns the bytes to append. The combined init segment comes
    /// back prepended to the first fragment.
    pub fn push(&mut self, index: usize, bytes: &[u8]) -> Result<Vec<u8>, JsValue> {
        self.inner
            .push(index, bytes)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = isComplete)]
    pub fn is_complete(&self) -> bool {
        self.inner.is_complete()
    }

    #[wasm_bindgen(js_name = durationMs)]
    pub fn duration_ms(&self) -> u64 {
        self.inner.duration_ms()
    }

    #[wasm_bindgen(js_name = hashUpdate)]
    pub fn hash_update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    #[wasm_bindgen(js_name = hashHex)]
    pub fn hash_hex(&self) -> String {
        self.hasher.finish_hex()
    }

    #[wasm_bindgen(js_name = hashedLen)]
    pub fn hashed_len(&self) -> u64 {
        self.hasher.len()
    }

    #[wasm_bindgen(js_name = resetHash)]
    pub fn reset_hash(&mut self) {
        self.hasher = crate::integrity::Hasher::new();
    }
}
