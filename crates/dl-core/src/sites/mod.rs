//! Per-site extraction: turning a page URL into downloadable media.
//!
//! The passive sniffer in the extension sees whatever a page fetches, and for a great
//! many sites that is enough. The large video platforms are not among them: they hand
//! their player a JSON blob describing every rendition, and the player then fetches
//! segments the sniffer sees only as anonymous byte ranges. To offer the user "1080p" or
//! "just the audio" instead of "seg-0001.m4s", something has to read that blob.
//!
//! # The seam, unchanged
//!
//! Rust decides, TypeScript fetches. An [`Extractor`] is a **state machine that performs
//! no I/O**: it says what it needs, the host performs the fetch or reads the page, and
//! feeds the bytes back. That is what keeps every site's parsing assertable against a
//! captured fixture instead of a live network, which matters more here than anywhere
//! else in the codebase — these are the parts that rot.
//!
//! # Why reading the page beats scraping it
//!
//! Fetching these sites from outside a browser mostly fails: TikTok answers a bot wall,
//! Bilibili answers `412`, Facebook redirects to a login. Inside the extension none of
//! that applies, because the page has *already loaded* and already holds the data — the
//! player could not play without it. So the preferred input is [`Need::PageState`], and
//! network fetches are the fallback rather than the default.

use serde::{Deserialize, Serialize};

#[cfg(feature = "platform-sites")]
pub mod bilibili;
pub mod dailymotion;
#[cfg(feature = "platform-sites")]
pub mod douyin;
pub mod generic;
#[cfg(feature = "platform-sites")]
pub mod meta;
#[cfg(feature = "platform-sites")]
pub mod tiktok;
pub mod twitch;
pub mod twitter;
pub mod vimeo;
pub mod vimeo_adaptive;
#[cfg(feature = "platform-sites")]
pub mod weixin;
#[cfg(feature = "platform-sites")]
pub mod youtube;

/// What kind of stream one downloadable option is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    /// One file carrying both picture and sound. Nothing to merge.
    Muxed,
    /// Picture only. Needs an [`StreamKind::AudioOnly`] partner to be watchable.
    VideoOnly,
    /// Sound only. Downloadable on its own as an audio file.
    AudioOnly,
}

/// One fetchable stream belonging to a [`MediaOption`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    pub url: String,
    pub kind: StreamKind,
    /// The container/codec string the site reported, for display and for choosing an
    /// output extension. Never trusted for parsing decisions.
    pub mime: Option<String>,
    /// Bytes, when the site states it. Lets the UI show a size before starting.
    pub size: Option<u64>,
    /// Headers the fetch must carry. A `Referer` is the usual one, and on several of
    /// these sites its absence is the entire difference between 200 and 403.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// How much of this stream to ask for in one request, when the host cares.
    ///
    /// Set where a host rate-limits large sequential reads. Google's media servers answer
    /// `403` to an 8 MiB range and `206` to a 1 MiB one — but the mechanism is a per-URL
    /// limiter rather than a size ceiling, since after enough large reads every size is
    /// refused and a freshly issued URL is immediately fine again. So this is the request
    /// size the host tolerates, and a `403` part-way through is a throttle to back off
    /// from rather than a refusal to give up on.
    #[serde(default)]
    pub max_chunk: Option<u64>,
}

/// One thing the user can choose to download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaOption {
    /// What the UI shows: "1080p60", "Audio only · 128 kbps".
    pub label: String,
    /// Sort key, descending. Video height, or bitrate for audio-only options.
    pub rank: u64,
    /// One stream for a muxed option, two when video and audio must be merged.
    pub streams: Vec<Stream>,
    /// Suggested filename, extension included, derived from the media's own title.
    pub filename: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<u64>,
}

/// A resource the extractor needs before it can continue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub url: String,
    /// `GET` or `POST`.
    pub method: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Present only for `POST`.
    pub body: Option<String>,
}

impl Request {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: "GET".into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn post(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: "POST".into(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: Some(body.into()),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// What the extractor wants next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Need {
    /// Fetch these and feed the bodies back, in order.
    Fetch(Vec<Request>),
    /// Read the loaded page's own document and hand back its HTML.
    ///
    /// The extension satisfies this by injecting a reader into the tab; the web app
    /// cannot, and turns it into an ordinary fetch — which is exactly why the web app
    /// fails on the sites that block outsiders, and why the extension is the answer
    /// there.
    PageState,
}

/// Where an extractor has got to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    /// Not finished; satisfy this and call `feed`.
    Need(Need),
    /// Finished. Options are sorted best-first.
    Done(Extraction),
}

/// One video rendition the user can choose.
///
/// Separate from [`MediaOption`] because they answer different questions. An option is
/// "give me 1080p" — a ready-made pairing for the common case. A [`VideoChoice`] is one
/// half of "give me 1080p *with this audio*", which is the case a site with several audio
/// bitrates or several languages makes worth asking about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoChoice {
    /// Stable within one extraction; how a UI names the pairing it wants.
    pub id: String,
    /// What the UI shows: "1080p60 · AVC".
    pub label: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub bitrate: Option<u64>,
    pub codec: Option<String>,
    pub size: Option<u64>,
    pub stream: Stream,
    /// True when this rendition already carries sound, so no audio need be chosen.
    pub has_audio: bool,
    /// The highest quality that can actually be delivered as one playable file.
    ///
    /// Not simply the largest. YouTube's top renditions are VP9 and AV1 in WebM, and this
    /// build joins video to audio only inside MP4 — so recommending 2160p VP9 would
    /// recommend something that cannot be given sound. See [`Extraction::rank_choices`].
    pub best: bool,
    /// `"mp4"`, `"webm"`, or whatever the mime said. Filled in by `rank_choices`.
    #[serde(default)]
    pub container: Option<String>,
    /// Whether this rendition can be joined to an audio track by this build.
    ///
    /// The test is the **container**, not the codec, and that is worth stating because it
    /// looks too permissive: the fragmented merger copies each input's track description
    /// through verbatim rather than re-describing it, so it never needs to understand the
    /// codec at all. Checked live on 2026-09-05 — an AV1 video and an AAC track merged
    /// into a file `ffprobe` reads as `av1` plus `aac`, and H.264 likewise, while a WebM
    /// video failed exactly as this flag predicts.
    ///
    /// A WebM rendition is still perfectly downloadable on its own; it is only the
    /// *joining* that is MP4-only, because that is the container `dl-container` writes.
    #[serde(default)]
    pub mergeable: bool,
}

/// One audio rendition the user can choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioChoice {
    pub id: String,
    /// What the UI shows: "128 kbps · AAC" or "English · 256 kbps".
    pub label: String,
    pub bitrate: Option<u64>,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub size: Option<u64>,
    pub stream: Stream,
    /// The highest quality that can actually be joined to the recommended video.
    pub best: bool,
    /// `"mp4"`, `"webm"`, or whatever the mime said. Filled in by `rank_choices`.
    #[serde(default)]
    pub container: Option<String>,
    /// Whether this track can be joined to a video by this build. True for MP4/AAC.
    #[serde(default)]
    pub mergeable: bool,
}

/// Everything an extractor learned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extraction {
    pub site: String,
    pub title: String,
    /// Ready-made pairings, best first. What a UI offers when the user just wants "1080p".
    pub options: Vec<MediaOption>,
    /// Every video rendition, best first, for choosing picture and sound separately.
    ///
    /// A site that only serves muxed files lists them here with `has_audio: true`, so a
    /// UI can treat every site the same way and simply not show an audio picker when
    /// nothing needs pairing.
    #[serde(default)]
    pub videos: Vec<VideoChoice>,
    /// Every audio rendition, best first. Empty when the site muxes its audio in.
    #[serde(default)]
    pub audios: Vec<AudioChoice>,
    /// Subtitle tracks the site exposes, if any.
    #[serde(default)]
    pub subtitles: Vec<SubtitleTrack>,
    /// Something true about this result that the list of renditions does not say.
    ///
    /// For the case it was added for: bilibili publishes `accept_quality` naming every
    /// rendition a video *has*, then serves a signed-out caller only the lowest two. The
    /// menu is then correct and complete and still looks broken, because the 1080p the
    /// site advertises is not in it. A UI that shows this says why; one that ignores it
    /// is no worse off than before.
    #[serde(default)]
    pub note: Option<String>,
}

impl Extraction {
    /// Sort both choice lists best-first, work out what can be joined, and mark the best
    /// deliverable option in each.
    ///
    /// Called by every extractor rather than each doing its own sorting, so "best" means
    /// one thing across the whole product and a site cannot accidentally recommend its
    /// worst rendition. Ordering is by height, then width, then frame rate, then bitrate,
    /// then declared size: height is what a person means by "1080p" and is the one figure
    /// every site states, and the rest break the ties a 1080p at 2 Mbps and a 1080p60 at
    /// 6 Mbps would otherwise leave to chance.
    ///
    /// # Why "best" is not simply "largest"
    ///
    /// On YouTube the top renditions are VP9 and AV1 in WebM, and the largest audio is
    /// Opus — while this build joins a separate video and audio only inside MP4, because
    /// that is the container `dl-container` writes. Flagging 2160p VP9 as best would
    /// recommend a file that cannot be given sound, and the person who clicked the
    /// obvious button would get a silent video.
    ///
    /// So `best` marks the highest rendition that can actually be **delivered complete**:
    /// one that already carries its own audio, or an MP4 with an MP4 audio track to join
    /// it to. Every other rendition stays in the list and stays choosable — the point is
    /// to make the default right, not to hide anything.
    pub fn rank_choices(&mut self) {
        // Height first, not pixel count. Several sites — Vimeo, Dailymotion, Twitch —
        // state a height and no width at all, and multiplying the two ties every one of
        // their renditions at zero, leaving the order to whatever the extractor happened
        // to push. Height is the number those sites do give, and it is also the number a
        // person means by "1080p", so it is the right primary key; width only breaks ties
        // between two renditions of the same height.
        self.videos.sort_by(|a, b| {
            b.height
                .unwrap_or(0)
                .cmp(&a.height.unwrap_or(0))
                .then(b.width.unwrap_or(0).cmp(&a.width.unwrap_or(0)))
                .then(b.fps.unwrap_or(0).cmp(&a.fps.unwrap_or(0)))
                .then(b.bitrate.unwrap_or(0).cmp(&a.bitrate.unwrap_or(0)))
                .then(b.size.unwrap_or(0).cmp(&a.size.unwrap_or(0)))
        });
        self.audios.sort_by(|a, b| {
            b.bitrate
                .unwrap_or(0)
                .cmp(&a.bitrate.unwrap_or(0))
                .then(b.size.unwrap_or(0).cmp(&a.size.unwrap_or(0)))
        });
        for v in &mut self.videos {
            v.container = container_of(v.stream.mime.as_deref());
            v.mergeable = v.has_audio || v.container.as_deref() == Some("mp4");
            v.best = false;
        }
        for a in &mut self.audios {
            a.container = container_of(a.stream.mime.as_deref());
            a.mergeable = a.container.as_deref() == Some("mp4");
            a.best = false;
        }

        // The best audio to join with, if joining is needed at all.
        let joinable_audio = self.audios.iter().position(|a| a.mergeable);

        // The best video that can be delivered complete: one that carries its own sound,
        // or an MP4 with an MP4 audio track available to join to it.
        let best_video = self
            .videos
            .iter()
            .position(|v| v.has_audio || (v.mergeable && joinable_audio.is_some()))
            // Nothing is deliverable complete — a WebM-only stream with no MP4 audio, say.
            // Recommend the best there is rather than nothing, since a video-only download
            // is still a download and the UI says what it is.
            .or(if self.videos.is_empty() {
                None
            } else {
                Some(0)
            });

        if let Some(i) = best_video {
            self.videos[i].best = true;
        }
        if let Some(i) = joinable_audio.or(if self.audios.is_empty() {
            None
        } else {
            Some(0)
        }) {
            self.audios[i].best = true;
        }
    }

    /// The recommended video: the best that can be delivered complete.
    pub fn best_video(&self) -> Option<&VideoChoice> {
        self.videos
            .iter()
            .find(|v| v.best)
            .or_else(|| self.videos.first())
    }

    /// The recommended audio.
    pub fn best_audio(&self) -> Option<&AudioChoice> {
        self.audios
            .iter()
            .find(|a| a.best)
            .or_else(|| self.audios.first())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleTrack {
    pub label: String,
    pub language: Option<String>,
    pub url: String,
    /// `vtt`, `srt`, `json3`, `ttml` — what the URL actually returns.
    pub format: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteError {
    /// No extractor claims this URL.
    Unsupported,
    /// The response did not contain what this site is documented to return. Carries the
    /// site name so a failing extractor is identifiable without a stack trace — these
    /// break when a site changes, and the message is the bug report.
    Shape(String),
    /// The site said the media exists but cannot be played: private, deleted,
    /// age-gated, region-locked, or behind a login.
    Unavailable(String),
    /// The stream is encrypted. Refused everywhere, on every host.
    Encrypted,
}

impl core::fmt::Display for SiteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SiteError::Unsupported => f.write_str("no extractor handles this URL"),
            SiteError::Shape(site) => write!(
                f,
                "{site} did not return what was expected — the site has probably changed"
            ),
            SiteError::Unavailable(why) => write!(f, "{why}"),
            SiteError::Encrypted => f.write_str(
                "this stream is encrypted; opendownloader does not download protected streams",
            ),
        }
    }
}

/// One site's extraction logic.
///
/// Implementations are pure: `start` and `feed` may look at their inputs and their own
/// state and nothing else. No clock, no network, no randomness — which is what lets a
/// captured response be replayed as a test.
pub trait Extractor {
    /// The name that appears in errors and in the UI.
    fn site(&self) -> &'static str;
    /// What this extractor needs first.
    fn start(&mut self, url: &str) -> Result<Step, SiteError>;
    /// Feed the bodies of the previous [`Need`], in the order requested.
    fn feed(&mut self, bodies: &[&str]) -> Result<Step, SiteError>;

    /// Whether a [`Need::PageState`] may be satisfied by *fetching* the page instead of
    /// reading it out of a loaded tab.
    ///
    /// The two are not the same document. A tab's copy has been through the page's own
    /// scripts; a fetched copy is the server's first response, and on most of these sites
    /// the media is not in it. Answering `true` is a promise that this extractor either
    /// finds what it needs in that thinner copy or has another way to continue from it —
    /// not merely that it will fail politely.
    ///
    /// Default `false`, so a host without a tab is told plainly to use the extension
    /// rather than shown a parse failure that reads like the site broke.
    fn accepts_fetched_page(&self) -> bool {
        false
    }
}

/// Pick the extractor for a URL, or `None` when the passive sniffer should handle it.
pub fn extractor_for(url: &str) -> Option<Box<dyn Extractor>> {
    let host = crate::policy::host_of(url)?;
    for (matches, build) in PLATFORM_REGISTRY.iter().chain(GENERIC_REGISTRY) {
        if matches(&host) {
            return Some(build(url));
        }
    }
    None
}

/// Whether this URL's extractor can work from a fetched page rather than a loaded tab.
///
/// The web app asks this to decide between fetching the page through the relay and
/// telling the user, plainly, that this one needs the extension.
pub fn site_accepts_fetched_page(url: &str) -> bool {
    extractor_for(url).is_some_and(|e| e.accepts_fetched_page())
}

/// Whether a host with no tab to read — the web app — can resolve this URL at all.
///
/// Two ways that is true, and using only the second is a bug this has already had: an
/// extractor whose *first* move is a fetch never wanted a page in the first place
/// (YouTube), and one that did ask for a page may have said it can carry on from a
/// fetched copy (Bilibili). [`site_accepts_fetched_page`] answers only the second, so
/// asking it alone reports YouTube as extension-only.
///
/// Starting the extractor is the honest way to know, and costs nothing: `start` is pure
/// and performs no I/O.
pub fn site_works_without_a_tab(url: &str) -> bool {
    let Some(mut extractor) = extractor_for(url) else {
        return false;
    };
    matches!(extractor.start(url), Ok(Step::Need(Need::Fetch(_))))
        || extractor.accepts_fetched_page()
}

/// Whether any extractor claims this URL.
pub fn is_supported(url: &str) -> bool {
    extractor_for(url).is_some()
}

/// The name of the site that would handle this URL, for the UI.
pub fn site_for(url: &str) -> Option<&'static str> {
    extractor_for(url).map(|e| e.site())
}

type Matcher = fn(&str) -> bool;
/// Built from the URL, not from nothing.
///
/// `Meta` covers Instagram and Facebook with one implementation, so it cannot say which
/// of the two it is until it has seen a URL — and [`site_for`] is asked exactly that,
/// before `start` runs. Handing the URL to the constructor is what lets every extractor
/// name itself correctly from the moment it exists.
type Builder = fn(&str) -> Box<dyn Extractor>;

/// The large-platform extractors, compiled in only with the `platform-sites` feature.
///
/// The feature exists because there are genuinely two products here, and the difference
/// is a distribution constraint rather than a technical one. The Chrome Web Store's
/// developer policy prohibits extensions that download from YouTube, and Edge mirrors it
/// — so a build intended for those stores must not contain this code, and a build
/// distributed from the project's own site may. Making that a compile-time feature
/// rather than a runtime setting is deliberate: a reviewer can verify a store build does
/// not contain the code, which no setting could demonstrate.
///
/// `generic` is not in here. It reads `<video>` tags and Open Graph headers from pages
/// that simply state their media, which every store permits.
#[cfg(feature = "platform-sites")]
const PLATFORM_REGISTRY: &[(Matcher, Builder)] = &[
    (youtube::matches, |_| Box::new(youtube::YouTube::new())),
    (bilibili::matches, |_| Box::new(bilibili::Bilibili::new())),
    (tiktok::matches, |_| Box::new(tiktok::TikTok::new())),
    (douyin::matches, |_| Box::new(douyin::Douyin::new())),
    (meta::matches, meta::build),
    (weixin::matches, |_| Box::new(weixin::Weixin::new())),
];

#[cfg(not(feature = "platform-sites"))]
const PLATFORM_REGISTRY: &[(Matcher, Builder)] = &[];

/// The extractors present in every build, store or otherwise.
///
/// Separate from [`PLATFORM_REGISTRY`] because the reason that one is gated does not
/// apply here. Chrome's developer policy names YouTube; nothing in it, or in Edge's,
/// speaks to Vimeo, Dailymotion, Twitch clips or a post on X. Gating these too would
/// make the store build worse for no reason a reviewer would recognise.
///
/// Registration order is match order, so `generic` comes last: a URL none of the
/// dedicated extractors claims falls through to the page reader, and one nothing claims
/// at all falls through to the passive sniffer — the right behaviour for the long tail of
/// ordinary sites.
const GENERIC_REGISTRY: &[(Matcher, Builder)] = &[
    (vimeo::matches, |_| Box::new(vimeo::Vimeo::new())),
    (dailymotion::matches, |_| {
        Box::new(dailymotion::Dailymotion::new())
    }),
    (twitch::matches, |_| Box::new(twitch::Twitch::new())),
    (twitter::matches, |_| Box::new(twitter::Twitter::new())),
    (generic::matches, |_| Box::new(generic::Generic::new())),
];

/// One site in the catalogue the front ends show.
#[derive(Debug, Clone, Serialize)]
pub struct SiteInfo {
    /// What the extractor calls itself.
    pub name: &'static str,
    /// A URL of the shape this extractor claims, with a placeholder id that is
    /// nonetheless *shaped* correctly — the test starts the real extractor on it, and an
    /// id of the wrong shape fails to parse and would fail the test for the wrong
    /// reason. The front ends show the hostname from this, not the whole string.
    pub example: &'static str,
    /// Whether a host with no tab to read — the web app — can resolve this site at all.
    ///
    /// False means the extractor's first move is to read a loaded page and it has no
    /// fetch fallback, so only the extension can do it. Saying so up front is the
    /// difference between a considered limit and an error message after a failed paste.
    pub without_a_tab: bool,
    /// Whether every request the extractor makes is answered to *any* origin, so a page
    /// served from a different domain — the web app on opendownloader.app, with no relay
    /// and no extension — can read the answers.
    ///
    /// Narrower than [`SiteInfo::without_a_tab`], and the two were once treated as one.
    /// That field says the extractor can work from fetches; this one says a browser will
    /// hand those fetches' answers to a foreign page. The web app marked every
    /// `without_a_tab` site "works here", and five of the six refused a visitor in under
    /// two seconds (APP-79). Measured 13 September 2026 with a plain cross-origin GET:
    /// Twitch's GQL answers `Access-Control-Allow-Origin: *`; Vimeo's player config and
    /// Dailymotion's metadata send no such header; X's syndication answers only
    /// `platform.twitter.com`; YouTube and Bilibili refuse outright. Set it only from a
    /// measurement like that one, never from what an API is documented to allow.
    pub from_any_origin: bool,
}

/// Every site with a dedicated extractor in this build.
///
/// Ordered as the registry matches, and gated the same way: a store build compiles the
/// large-platform extractors out, and this list shrinks with them rather than promising
/// sites the binary cannot handle.
pub fn supported_sites() -> Vec<SiteInfo> {
    let mut sites: Vec<SiteInfo> = Vec::new();
    #[cfg(feature = "platform-sites")]
    sites.extend([
        SiteInfo {
            name: "YouTube",
            example: "https://www.youtube.com/watch?v=VIDEOID1234",
            without_a_tab: true,
            from_any_origin: false,
        },
        SiteInfo {
            name: "Bilibili",
            example: "https://www.bilibili.com/video/BV1xx411c7mD",
            without_a_tab: true,
            from_any_origin: false,
        },
        SiteInfo {
            name: "TikTok",
            example: "https://www.tiktok.com/@user/video/1234567890",
            without_a_tab: false,
            from_any_origin: false,
        },
        SiteInfo {
            name: "Douyin",
            example: "https://www.douyin.com/video/1234567890",
            without_a_tab: false,
            from_any_origin: false,
        },
        SiteInfo {
            name: "Instagram",
            example: "https://www.instagram.com/reel/ABCdef12345/",
            without_a_tab: false,
            from_any_origin: false,
        },
        SiteInfo {
            name: "Facebook",
            example: "https://www.facebook.com/watch/?v=1234567890",
            without_a_tab: false,
            from_any_origin: false,
        },
        SiteInfo {
            name: "WeChat",
            example: "https://mp.weixin.qq.com/s/AbCdEfGhIjKlMnOp",
            without_a_tab: false,
            from_any_origin: false,
        },
    ]);
    sites.extend([
        SiteInfo {
            name: "Vimeo",
            example: "https://vimeo.com/123456789",
            without_a_tab: true,
            from_any_origin: false,
        },
        SiteInfo {
            name: "Dailymotion",
            example: "https://www.dailymotion.com/video/x8abcde",
            without_a_tab: true,
            from_any_origin: false,
        },
        // Clips only, and named so: the extractor refuses VODs and channels outright, and a
        // catalogue entry reading just "Twitch" would promise the whole site.
        SiteInfo {
            name: "Twitch clips",
            example: "https://clips.twitch.tv/AbcDefGhi123",
            without_a_tab: true,
            from_any_origin: true,
        },
        SiteInfo {
            name: "X",
            example: "https://x.com/user/status/1234567890",
            without_a_tab: true,
            from_any_origin: false,
        },
    ]);
    sites
}

/// A source that is downloadable but has no extractor.
///
/// Kept apart from {@link supported_sites} on purpose. That list is checked against the
/// extractor registry entry by entry, and every one of these would fail that check for
/// the right reason: there is no page to read and nothing to extract. A magnet names a
/// swarm, a Mega link carries its own decryption key, a Quark link opens a directory.
/// Folding them in would mean loosening the test that keeps the site list honest.
#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    pub name: &'static str,
    /// What the user pastes, in words rather than a URL — these have no single shape.
    pub accepts: &'static str,
    /// Whether the web app alone can do it, or a local helper has to be running.
    pub needs_local_helper: bool,
}

/// Every non-extractor source this build accepts.
pub fn supported_sources() -> Vec<SourceInfo> {
    vec![
        SourceInfo {
            name: "Mega",
            accepts: "mega.nz file links",
            // Mega's API allows every origin, and the key is in the fragment. This is
            // the one storage service a plain page can do end to end.
            needs_local_helper: false,
        },
        SourceInfo {
            name: "Quark",
            accepts: "pan.quark.cn share links",
            // Quark's API answers only its own origin, so the relay has to make the call.
            needs_local_helper: true,
        },
        SourceInfo {
            name: "BitTorrent",
            accepts: "magnet links and .torrent files",
            needs_local_helper: true,
        },
        SourceInfo {
            name: "eD2k",
            accepts: "ed2k:// links that carry a web source",
            needs_local_helper: false,
        },
        SourceInfo {
            name: "Xunlei",
            accepts: "thunder://, flashget:// and qqdl:// links",
            needs_local_helper: false,
        },
    ]
}

/// The hosts a site needs beyond the one its page is served from.
///
/// This exists because of what a host permission actually gates, and it gates two
/// different things that both broke.
///
/// A **fetch** from an extension page is subject to CORS unless the extension holds a
/// permission for the target host. Vimeo's player config lives on `player.vimeo.com`
/// while the page is `vimeo.com`, so extraction failed with *"No
/// 'Access-Control-Allow-Origin' header is present"* — a message that names nothing a
/// user can act on.
///
/// The **network listener** only observes hosts the extension may access. Douyin's video
/// comes from `zjcdn.com`, so with only `douyin.com` granted the popup stayed empty and
/// reported that the site had changed.
///
/// Both are the same omission: the page's host is not where the work happens.
///
/// It is not the same question as [`Extractor`] routing. `matches` decides which
/// extractor claims a URL; putting an API or CDN host there would route a bare media URL
/// to a page extractor that cannot read it. A test asserts the two never agree.
///
/// Every entry is either a host this crate's own code requests, or one observed serving
/// media. A guessed CDN is a permission asked for and never used, which is worse than the
/// prompt it costs.
pub fn media_hosts(url: &str) -> Vec<&'static str> {
    let Some(host) = crate::policy::host_of(url) else {
        return Vec::new();
    };
    if youtube::matches(&host) {
        // The bytes are never on the page's host: every YouTube stream is served from a
        // numbered `googlevideo.com` node. Without permission for it the extraction
        // succeeds, the options render, and the download then fails with "Failed to
        // fetch" — the browser refusing a cross-origin read, which looks like a dead
        // link rather than a missing grant.
        //
        // `youtube.com` is here for the page hosts that are not `www`: the player
        // endpoint the extractor cannot work without lives on `www.youtube.com`, which
        // is a different host when the page is `youtu.be` or `m.youtube.com`.
        vec!["googlevideo.com", "youtube.com"]
    } else if dailymotion::matches(&host) {
        // Same shape: the metadata endpoint is on `www.dailymotion.com`, which a
        // `dai.ly` page does not have permission for, and the media is on `dmcdn.net`.
        vec!["dmcdn.net", "dailymotion.com"]
    } else if douyin::matches(&host) {
        // `zjcdn.com` observed serving a reel; the others are Douyin's sibling CDNs.
        vec!["zjcdn.com", "douyinvod.com", "bytecdn.cn"]
    } else if meta::matches(&host) {
        vec!["fbcdn.net", "cdninstagram.com"]
    } else if tiktok::matches(&host) {
        vec![
            "tiktokcdn.com",
            "tiktokcdn-us.com",
            "tiktokv.com",
            "byteoversea.com",
        ]
    } else if vimeo::matches(&host) {
        // `player.vimeo.com` is where the config this extractor cannot work without
        // lives, and it is a different host from the page.
        vec!["player.vimeo.com", "vimeocdn.com", "captions.vimeo.com"]
    } else if bilibili::matches(&host) {
        vec!["api.bilibili.com", "bilivideo.com", "akamaized.net"]
    } else if twitch::matches(&host) {
        vec!["gql.twitch.tv", "twitchcdn.net", "ttvnw.net"]
    } else if twitter::matches(&host) {
        vec!["cdn.syndication.twimg.com", "video.twimg.com"]
    } else if weixin::matches(&host) {
        vec!["qpic.cn", "video.qq.com"]
    } else {
        Vec::new()
    }
}

/// Whether this build carries the large-platform extractors.
pub const fn has_platform_sites() -> bool {
    cfg!(feature = "platform-sites")
}

/// The container a mime type names, lowercased: `"mp4"`, `"webm"`, or `None`.
///
/// Deliberately coarse. What the rest of the code needs to know is whether two streams
/// can be joined, and that question is answered by the container rather than by the exact
/// codec string a site chose to write.
pub fn container_of(mime: Option<&str>) -> Option<String> {
    let mime = mime?.to_ascii_lowercase();
    let base = mime.split(';').next().unwrap_or("").trim().to_string();
    let subtype = base.rsplit('/').next()?.to_string();
    Some(match subtype.as_str() {
        "mp4" | "m4a" | "x-m4a" | "quicktime" => "mp4".to_string(),
        "webm" | "x-matroska" => "webm".to_string(),
        other => other.to_string(),
    })
}

/// Suffix match on a host, boundary-aware — the same rule `policy` uses, so a lookalike
/// domain never matches a site extractor either.
pub fn host_is(host: &str, domain: &str) -> bool {
    host == domain
        || (host.len() > domain.len()
            && host.ends_with(domain)
            && host.as_bytes()[host.len() - domain.len() - 1] == b'.')
}

/// Strip characters a filesystem will not take, and bound the length.
///
/// Shared by every extractor because they all derive a filename from a title the site
/// supplied, and a title is arbitrary user input on every one of these platforms.
pub fn safe_filename(title: &str, extension: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => ' ',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.').trim();
    let stem: String = if trimmed.is_empty() {
        "video".to_string()
    } else {
        trimmed.chars().take(120).collect()
    };
    format!("{}.{extension}", stem.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renditions_that_state_no_width_still_rank_by_height() {
        // Vimeo, Dailymotion and Twitch all report a height and no width. Ranking by
        // pixel count would tie every one of them at zero and leave the recommendation to
        // whichever order the extractor happened to build its list in.
        let stream = || Stream {
            url: "https://x/y.mp4".into(),
            kind: StreamKind::Muxed,
            mime: Some("video/mp4".into()),
            size: None,
            headers: Vec::new(),
            max_chunk: None,
        };
        let choice = |id: &str, height: u32| VideoChoice {
            id: id.into(),
            label: format!("{height}p"),
            width: None,
            height: Some(height),
            fps: None,
            bitrate: None,
            codec: None,
            size: None,
            stream: stream(),
            has_audio: true,
            best: false,
            container: None,
            mergeable: false,
        };

        let mut extraction = Extraction {
            site: "test".into(),
            title: "t".into(),
            note: None,
            options: Vec::new(),
            // Deliberately worst-first, which is what a naive extractor produces.
            videos: vec![choice("a", 360), choice("b", 1080), choice("c", 720)],
            audios: Vec::new(),
            subtitles: Vec::new(),
        };
        extraction.rank_choices();

        let order: Vec<&str> = extraction.videos.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(order, vec!["b", "c", "a"], "tallest first");
        assert_eq!(extraction.best_video().map(|v| v.id.as_str()), Some("b"));
    }

    #[test]
    fn host_matching_respects_label_boundaries() {
        assert!(host_is("www.youtube.com", "youtube.com"));
        assert!(host_is("youtube.com", "youtube.com"));
        assert!(!host_is("notyoutube.com", "youtube.com"));
        assert!(!host_is("youtube.com.evil.test", "youtube.com"));
    }

    #[test]
    fn filenames_cannot_escape_the_download_directory() {
        let name = safe_filename("../../etc/passwd", "mp4");
        assert!(!name.contains('/'), "got {name}");
        assert!(!name.starts_with('.'), "got {name}");
        assert!(name.ends_with(".mp4"));
    }

    #[test]
    fn an_empty_or_control_only_title_still_produces_a_usable_name() {
        assert_eq!(safe_filename("", "mp4"), "video.mp4");
        assert_eq!(safe_filename("   ", "m4a"), "video.m4a");
        assert_eq!(safe_filename("\u{1}\u{2}", "mp4"), "video.mp4");
    }

    #[test]
    fn a_long_title_is_bounded() {
        let name = safe_filename(&"x".repeat(500), "mp4");
        assert!(name.chars().count() <= 124, "{}", name.chars().count());
    }

    #[test]
    fn an_unrelated_url_has_no_extractor_and_falls_through_to_the_sniffer() {
        assert!(extractor_for("https://example.com/video.mp4").is_none());
        assert!(!is_supported("https://example.com/video.mp4"));
    }

    #[cfg(feature = "platform-sites")]
    #[test]
    fn an_extractor_covering_two_sites_names_the_right_one_before_it_has_run() {
        // `Meta` is one implementation for Instagram and Facebook, and the UI asks which
        // site a URL belongs to before extraction starts. Naming it from the URL at
        // construction is what stops a Facebook link being labelled "Instagram".
        assert_eq!(
            site_for("https://www.instagram.com/p/abc/"),
            Some("Instagram")
        );
        assert_eq!(
            site_for("https://www.facebook.com/watch/?v=1"),
            Some("Facebook")
        );
        assert_eq!(site_for("https://fb.watch/abc/"), Some("Facebook"));
    }

    #[test]
    fn the_platform_extractors_are_present_exactly_when_the_feature_is() {
        // The store build must not merely decline to use this code — it must not contain
        // it, which is the one thing a reviewer can check and a runtime setting cannot
        // demonstrate.
        assert_eq!(
            is_supported("https://www.youtube.com/watch?v=aqz-KE-bpKQ"),
            has_platform_sites()
        );
        assert_eq!(
            is_supported("https://www.bilibili.com/video/BV1"),
            has_platform_sites()
        );
        // The generic reader is in every build: reading a page's own `<video>` tag
        // breaches no store policy.
        assert!(is_supported(
            "https://old.reddit.com/r/videos/comments/abc/"
        ));
        assert!(is_supported("https://streamable.com/abcdef"));
    }
}

#[cfg(test)]
mod catalogue {
    use super::*;
    /// A media host is asked for permission, not routed to an extractor.
    #[test]
    fn media_hosts_are_named_for_the_sites_that_stream_from_them() {
        let douyin = media_hosts("https://www.douyin.com/jingxuan?modal_id=1");
        assert!(
            douyin.contains(&"zjcdn.com"),
            "observed serving Douyin video"
        );

        assert!(media_hosts("https://www.facebook.com/reel/1").contains(&"fbcdn.net"));
        assert!(media_hosts("https://www.instagram.com/reel/x/").contains(&"cdninstagram.com"));

        // The host an extractor *fetches* from counts too, not only the one serving
        // bytes. Vimeo's config is on `player.vimeo.com` while the page is `vimeo.com`,
        // and without permission that fetch is refused by CORS before extraction starts.
        let vimeo = media_hosts("https://vimeo.com/1086925006");
        assert!(
            vimeo.contains(&"player.vimeo.com"),
            "the config host: {vimeo:?}"
        );
        assert!(
            vimeo.contains(&"vimeocdn.com"),
            "where the media is: {vimeo:?}"
        );
        assert!(media_hosts("https://www.bilibili.com/video/BV1").contains(&"api.bilibili.com"));

        // This assertion used to read `is_empty()`, describing YouTube as "a site with
        // no separate media host". That was simply false — every YouTube stream comes
        // from a numbered `googlevideo.com` node — and stating it here is what kept the
        // gap alive: extraction worked, the qualities rendered, and the download died
        // with "Failed to fetch" because permission for the CDN had never been asked
        // for. A test that asserts the absence of a permission is worth this much
        // suspicion.
        let youtube = media_hosts("https://www.youtube.com/watch?v=1");
        assert!(
            youtube.contains(&"googlevideo.com"),
            "where every YouTube stream is served from: {youtube:?}"
        );
        // From a short link the page host is `youtu.be`, so the player endpoint on
        // `www.youtube.com` is cross-origin and needs its own grant.
        let short = media_hosts("https://youtu.be/aqz-KE-bpKQ");
        assert!(
            short.contains(&"youtube.com"),
            "the player endpoint: {short:?}"
        );

        let dailymotion = media_hosts("https://www.dailymotion.com/video/x1");
        assert!(dailymotion.contains(&"dmcdn.net"), "{dailymotion:?}");
        assert!(dailymotion.contains(&"dailymotion.com"), "{dailymotion:?}");

        // A site with no extractor at all.
        assert!(media_hosts("https://example.com/a.mp4").is_empty());

        // The two lists are different questions: a CDN must not claim a page extractor.
        for host in douyin {
            assert!(
                !crate::sites::douyin::matches(host),
                "{host} is a media host; routing it to the page extractor would be wrong"
            );
        }
    }

    /// The catalogue must describe the registry, not a memory of it.
    ///
    /// Both halves drift in the same way: an extractor gains a fetch fallback, or loses
    /// one, and the list in `supported_sites` keeps saying what used to be true. Since
    /// the entry carries an example URL, the check is cheap — start the real extractor on
    /// the real URL and compare.
    #[test]
    fn every_entry_matches_the_extractor_it_names() {
        for site in supported_sites() {
            let extractor = extractor_for(site.example)
                .unwrap_or_else(|| panic!("no extractor claims {}", site.example));
            // `starts_with`, not equality: an entry may narrow the extractor's own name
            // where the extractor handles less than the brand ("Twitch clips" — VODs and
            // channels are refused outright). It may not contradict it, which is what
            // this still catches.
            assert!(
                site.name.starts_with(extractor.site()),
                "{} is matched by {}, which the entry calls {}",
                site.example,
                extractor.site(),
                site.name
            );

            // Without a tab, a host can only work from fetches. So the claim holds when
            // the first move is a fetch, or when the extractor has said it can carry on
            // from a page someone fetched for it.
            let reachable = site_works_without_a_tab(site.example);
            assert_eq!(
                site.without_a_tab,
                reachable,
                "{} claims without_a_tab={} but the extractor {}",
                site.name,
                site.without_a_tab,
                if reachable {
                    "can work from fetches"
                } else {
                    "needs a loaded tab"
                }
            );
        }
    }

    /// "Any origin can read it" is a claim about the site, and the only part of it the
    /// code can check is the part that must hold first: a foreign page has no tab to
    /// read, so an extractor it can use has to be one that works without one. The claim
    /// itself comes from a measured cross-origin request — see the field's note.
    #[test]
    fn a_site_readable_from_any_origin_is_one_that_needs_no_tab() {
        for site in supported_sites() {
            if site.from_any_origin {
                assert!(
                    site.without_a_tab,
                    "{} claims any origin can read it but needs a loaded tab",
                    site.name
                );
            }
        }
        // And the catalogue of the web app's own shortfall, pinned: moving a site into
        // this list is a promise to every visitor who pastes one, so it should take a
        // failing test and a measurement, not a one-word edit.
        let open: Vec<_> = supported_sites()
            .into_iter()
            .filter(|s| s.from_any_origin)
            .map(|s| s.name)
            .collect();
        assert_eq!(open, ["Twitch clips"]);
    }

    /// A store build compiles the platform extractors out, so the catalogue must shrink
    /// with them rather than advertising sites the binary cannot handle.
    #[test]
    fn the_catalogue_never_promises_more_than_the_build_carries() {
        for site in supported_sites() {
            assert!(
                extractor_for(site.example).is_some(),
                "{} is listed but nothing in this build claims it",
                site.name
            );
        }
        assert_eq!(
            supported_sites().iter().any(|s| s.name == "YouTube"),
            has_platform_sites(),
            "YouTube's presence must follow the platform-sites feature"
        );
    }
}
