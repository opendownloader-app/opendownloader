//! `dl-relay` — an optional, self-hosted streaming HTTP relay.
//!
//! The opendownloader **extension** never needs this. An extension holds host
//! permissions, so its `fetch()` is not subject to CORS and it can read any media
//! response directly. A **web page** cannot: a media server that sends no
//! `Access-Control-Allow-Origin` header has its response withheld from the page by the
//! browser before a single byte is readable. Nothing the page does can change that. The
//! only fix is a server that fetches on the page's behalf and answers with CORS headers,
//! and that server is this crate.
//!
//! It ships as source to run yourself rather than as a hosted service on purpose. A
//! hosted relay would be the one part of opendownloader with a bandwidth bill, and a
//! bandwidth bill is how a product that charges for nothing starts charging for
//! something. Running your own costs you what it costs and answers to you.
//!
//! The relay is a pipe, not a cache. Bodies are streamed straight through with
//! [`axum::body::Body::from_stream`] over `reqwest`'s `bytes_stream()`: a 20 GiB file
//! passes through in chunks and is never buffered in memory, which is also what lets a
//! single small server serve downloads far larger than its own RAM.
//!
//! Everything it refuses lives in [`guard`], and the ordering there matters: every
//! check runs before the upstream socket is opened.

pub mod config;
mod guard;
mod youtube;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tower_http::cors::{AllowOrigin, CorsLayer};

use config::Config;
use guard::Refusal;

/// Response headers copied from upstream.
///
/// An allow-list rather than a blocklist. `set-cookie`, `www-authenticate`,
/// `authorization` and anything else that carries or asks for identity are absent by
/// construction: the relay is anonymous in both directions, and a header that made a
/// caller's browser store an upstream cookie would quietly break that.
const COPIED_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "content-range",
    "accept-ranges",
    "etag",
    "last-modified",
    "content-disposition",
    // The relay forwards the body byte for byte and does not decode it, so if upstream
    // compressed it, the caller has to be told or it reads the bytes as text and gets
    // binary. Media is never compressed twice, so this only ever fires on the HTML and
    // JSON hops — which is exactly where it was missing: a page fetched through the relay
    // arrived as gzip labelled `text/html`, and every extractor reading it saw a document
    // with none of its markers in it and reported the site as changed.
    "content-encoding",
];

/// Request headers forwarded upstream verbatim.
///
/// The first two are what makes a resumable multi-connection download still work through
/// the relay: the client plans its byte ranges, and the relay must not second-guess them.
/// Rewriting or adding a `Range` here would silently turn four parallel range requests
/// into four full-file downloads.
///
/// `Content-Type` joins them for the POST route, where an API that is told its JSON is
/// `text/plain` rejects it.
/// `Accept` and `Accept-Language` are here because the relay is not only used for media.
/// Fetching a *watch page* with reqwest's default `Accept: */*` and no language at all is
/// not what any browser sends, and the sites that screen for automation notice: bilibili
/// answers that shape with a 412 and no page. Both are CORS-safelisted, so the caller can
/// set them itself and simply needs them passed along.
const FORWARDED_HEADERS: &[HeaderName] = &[
    header::RANGE,
    header::IF_RANGE,
    header::CONTENT_TYPE,
    header::ACCEPT,
    header::ACCEPT_LANGUAGE,
];

/// Headers the relay sets on the upstream request on the caller's behalf.
///
/// `Origin` and `Referer` are on the Fetch standard's forbidden list, so a browser page
/// cannot set them however much it needs to — and several media APIs answer 403 without
/// the right one. YouTube's player endpoint refuses every origin but its own. The caller
/// asks for them through `x-relay-origin` / `x-relay-referer`, which are ordinary headers
/// a page *may* set, and the relay rewrites them into the real ones.
///
/// This is the relay earning its place: it is the one component in the product that can
/// send a header a page cannot.
const HEADER_OVERRIDES: &[(&str, HeaderName)] = &[
    ("x-relay-origin", header::ORIGIN),
    ("x-relay-referer", header::REFERER),
    ("x-relay-user-agent", header::USER_AGENT),
];

struct Relay {
    cfg: Config,
    client: reqwest::Client,
    /// Caps upstream requests in flight. The permit is held by the response body, not
    /// by the handler, so a slot is occupied for as long as bytes are actually moving.
    slots: Arc<Semaphore>,
}

#[derive(Debug, Deserialize)]
struct FetchQuery {
    url: Option<String>,
}

/// The relay's routes, ready to serve or to mount inside another server.
///
/// Public because the desktop build runs the web app, this and the torrent bridge in one
/// process on one port. Same origin, so nothing has to cross CORS or mixed content — and
/// nothing has to be started by hand.
pub fn router(cfg: Config, client: reqwest::Client, cors: CorsLayer) -> Router {
    let slots = Arc::new(Semaphore::new(cfg.max_concurrent.max(1)));
    let state = Arc::new(Relay { cfg, client, slots });

    Router::new()
        .route("/healthz", get(healthz))
        // Both methods, because the sites that need a relay most are the ones whose
        // media has to be *asked for* first: YouTube's player endpoint is a POST with a
        // JSON body, and a GET-only relay turns that into a silent failure the page
        // cannot explain. See `fetch_post`.
        .route("/fetch", get(fetch).post(fetch_post))
        // Server-side YouTube extraction, off unless configured. It runs `yt-dlp`; see
        // `youtube.rs` for why the proxy above cannot do this and where it does or does
        // not work (residential vs datacenter IP).
        .route("/youtube", get(youtube::youtube))
        .layer(cors)
        .with_state(state)
}

/// The deploy check's target: no dependencies, no upstream, no config.
async fn healthz() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "text/plain; charset=utf-8")],
        "ok",
    )
}

async fn fetch(
    State(relay): State<Arc<Relay>>,
    method: Method,
    Query(query): Query<FetchQuery>,
    headers: HeaderMap,
) -> Response {
    let raw = query.url.unwrap_or_default();
    match relay_one(&relay, &raw, &headers, None).await {
        Ok(response) => {
            log(&method, &raw, response.status(), None);
            response
        }
        Err(refusal) => {
            log(&method, &raw, refusal.status(), Some(refusal.tag()));
            refusal.into_response()
        }
    }
}

/// Redirect hops the relay is willing to follow.
///
/// Followed by hand rather than by the HTTP client, because the check that matters is
/// asynchronous. `reqwest`'s redirect callback is synchronous and so cannot resolve a
/// hostname, which leaves exactly one hole: an allowed host that redirects to a *name*
/// resolving to 127.0.0.1 or a metadata endpoint would be followed. Doing the loop here
/// means every hop goes through the same DNS-aware guard as the first request, and that
/// hole is closed rather than documented.
const MAX_REDIRECTS: usize = 5;

/// Vet a URL completely: parse, compliance policy, allow-list, then DNS and address space.
async fn vet(relay: &Relay, raw: &str) -> Result<guard::Target, Refusal> {
    let target = guard::check(&relay.cfg, raw)?;
    // The SSRF guard, before the socket exists.
    if !relay.cfg.allow_private_hosts {
        guard::host_is_reachable(&target.host, target.port).await?;
    }
    Ok(target)
}

/// The same relay, for an upstream that has to be asked rather than simply read.
///
/// Kept as its own handler rather than folded into [`fetch`] so the GET path stays
/// exactly what it was — a streaming pipe with no body to buffer — and so the size cap
/// applies to the request body here as well as to the response.
async fn fetch_post(
    State(relay): State<Arc<Relay>>,
    method: Method,
    Query(query): Query<FetchQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let raw = query.url.unwrap_or_default();
    if body.len() as u64 > MAX_REQUEST_BODY {
        let refusal = Refusal::TooLarge(MAX_REQUEST_BODY);
        log(&method, &raw, refusal.status(), Some(refusal.tag()));
        return refusal.into_response();
    }
    match relay_one(&relay, &raw, &headers, Some(body)).await {
        Ok(response) => {
            log(&method, &raw, response.status(), None);
            response
        }
        Err(refusal) => {
            log(&method, &raw, refusal.status(), Some(refusal.tag()));
            refusal.into_response()
        }
    }
}

/// The largest request body the relay will forward.
///
/// An API call, not an upload: the endpoints this exists for send a few kilobytes of
/// JSON. A generous ceiling still refuses anyone trying to use the relay as a file drop.
const MAX_REQUEST_BODY: u64 = 256 * 1024;

async fn relay_one(
    relay: &Relay,
    raw: &str,
    headers: &HeaderMap,
    body: Option<Bytes>,
) -> Result<Response, Refusal> {
    let permit = Arc::clone(&relay.slots)
        .try_acquire_owned()
        .map_err(|_| Refusal::Busy)?;

    // The configured timeout bounds connect plus response headers only. Once the
    // upstream has answered, the body is allowed to take as long as it takes — a large
    // file over a slow link is this program's normal case, not an abuse of it.
    let timeout = Duration::from_secs(relay.cfg.timeout_seconds.max(1));

    let mut url = raw.to_string();
    let mut followed = 0usize;
    let upstream = loop {
        let target = vet(relay, &url).await?;

        let mut request = match &body {
            Some(bytes) => relay.client.post(&target.url).body(bytes.clone()),
            None => relay.client.get(&target.url),
        };
        for name in FORWARDED_HEADERS {
            if let Some(value) = headers.get(name) {
                request = request.header(name, value);
            }
        }
        // Headers a page is forbidden from setting, asked for by proxy.
        for (asked, real) in HEADER_OVERRIDES {
            if let Some(value) = headers.get(*asked) {
                request = request.header(real, value);
            }
        }
        // Deliberately *not* `Accept-Encoding: identity`, tempting as it is. This build
        // of reqwest has no decompression feature — the relay is a byte-for-byte
        // pass-through, which is what keeps ranged and resumed downloads honest — so an
        // uncompressed body would be simpler to forward. But no real browser asks for
        // one, and sites that screen for bots notice: bilibili answers `identity` with a
        // 412 and serves nothing. So the body arrives compressed and is forwarded as it
        // is, with the `content-encoding` that says so.

        let response = tokio::time::timeout(timeout, request.send())
            .await
            .map_err(|_| Refusal::Upstream("timed out waiting for response headers".into()))?
            .map_err(|e| Refusal::Upstream(e.to_string()))?;

        // A 304 carries no body and no Location; it is an answer, not a redirect.
        let Some(location) = redirect_target(&response) else {
            break response;
        };

        // Bounded, so a redirect loop cannot hold a concurrency slot indefinitely.
        followed += 1;
        if followed > MAX_REDIRECTS {
            return Err(Refusal::Upstream("too many redirects".into()));
        }
        url = guard::resolve_redirect(&target.url, &location)?;
    };

    let status = upstream.status();
    let max_bytes = relay.cfg.max_bytes;
    if let Some(len) = upstream.content_length() {
        if len > max_bytes {
            return Err(Refusal::TooLarge(max_bytes));
        }
    }

    let mut builder = Response::builder().status(status);
    for name in COPIED_HEADERS {
        if let Some(value) = upstream.headers().get(*name) {
            builder = builder.header(*name, value);
        }
    }

    // Streamed, never buffered: chunks go out as they arrive, so peak memory is one
    // chunk regardless of whether the file is 2 MiB or 20 GiB.
    //
    // The cap is enforced a second time here because a chunked response declares no
    // length up front, so `content-length` above cannot be trusted to have caught it.
    // Ending the stream with an error is the only signal available this late: the
    // status line is long gone, and a truncated body the client can detect is better
    // than an unbounded one it cannot.
    let mut sent: u64 = 0;
    let body = Body::from_stream(upstream.bytes_stream().map(move |chunk| {
        // The permit rides with the stream so the slot is released when the body ends
        // or the client hangs up, not when the handler returned some time ago.
        let _held = &permit;
        let chunk = chunk.map_err(std::io::Error::other)?;
        sent += chunk.len() as u64;
        if sent > max_bytes {
            return Err(std::io::Error::other(format!(
                "relay cap of {max_bytes} bytes exceeded"
            )));
        }
        Ok(chunk)
    }));

    builder
        .body(body)
        .map_err(|e| Refusal::Upstream(e.to_string()))
}

/// One line per request: method, the upstream host, the status, and — when it was
/// refused — why. The URL itself is not logged; the host is enough to debug a refusal
/// and a relay should not keep a record of exactly what passed through it.
fn log(method: &Method, raw: &str, status: StatusCode, refused: Option<&str>) {
    let host = guard::parse_target(raw)
        .map(|t| t.host)
        .unwrap_or_else(|_| "-".to_string());
    match refused {
        Some(tag) => println!("{method} {host} {} refused={tag}", status.as_u16()),
        None => println!("{method} {host} {}", status.as_u16()),
    }
}

/// Build the upstream client: anonymous, redirect-capped, and re-checked at each hop.
///
/// No cookie store and no credentials of any kind. Whatever the relay fetches, it
/// fetches as nobody — which is the only way a shared relay can be safe to point at a
/// URL somebody else supplied.
/// Build the upstream client: anonymous, and never following a redirect on its own.
///
/// No cookie store and no credentials of any kind. Whatever the relay fetches, it
/// fetches as nobody — which is the only way a shared relay can be safe to point at a
/// URL somebody else supplied.
///
/// `Policy::none()` is deliberate. Letting the client follow redirects would mean the
/// only check available at each hop is `reqwest`'s synchronous callback, which cannot
/// resolve a hostname and so cannot tell that `evil.example` points at 127.0.0.1.
/// `relay_one` follows them instead, running the full DNS-aware guard on every hop.
pub fn build_client(cfg: &Config) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .cookie_store(false)
        .connect_timeout(Duration::from_secs(cfg.timeout_seconds.max(1)))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("dl-relay/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// The `Location` of a redirect response, or `None` when this is the final answer.
///
/// A 304 is excluded on purpose: it is a legitimate answer to a conditional request —
/// which the relay forwards, since `If-Range` is one of the two headers it passes
/// through — and it carries no body to follow anywhere.
fn redirect_target(response: &reqwest::Response) -> Option<String> {
    let status = response.status();
    if !status.is_redirection() || status == StatusCode::NOT_MODIFIED {
        return None;
    }
    response
        .headers()
        .get(header::LOCATION)?
        .to_str()
        .ok()
        .map(str::to_string)
}

pub fn cors_layer(cfg: &Config) -> Result<CorsLayer, String> {
    let origin = if cfg.allows_any_origin() {
        AllowOrigin::any()
    } else {
        let mut values = Vec::with_capacity(cfg.allowed_origins.len());
        for origin in &cfg.allowed_origins {
            values.push(
                HeaderValue::from_str(origin)
                    .map_err(|_| format!("allowed_origins contains a bad origin: {origin}"))?,
            );
        }
        AllowOrigin::list(values)
    };
    Ok(CorsLayer::new()
        .allow_origin(origin)
        .allow_methods([Method::GET, Method::HEAD, Method::POST, Method::OPTIONS])
        // The `x-relay-*` trio are how a page asks for headers it is forbidden to set;
        // without them on this list the browser's preflight refuses the request before it
        // is ever made, which looks from the page like the relay being down.
        .allow_headers([
            header::RANGE,
            header::IF_RANGE,
            header::CONTENT_TYPE,
            // Both are CORS-safelisted, so a browser would send them without asking —
            // but `accept-language` is safelisted only for a restricted set of values,
            // and being explicit costs nothing and removes the edge case.
            header::ACCEPT,
            header::ACCEPT_LANGUAGE,
            HeaderName::from_static("x-relay-origin"),
            HeaderName::from_static("x-relay-referer"),
            HeaderName::from_static("x-relay-user-agent"),
        ])
        // Without this the browser can read the body but not `content-range` or
        // `content-length`, and a resumable download needs both.
        .expose_headers(
            COPIED_HEADERS
                .iter()
                .map(|h| HeaderName::from_static(h))
                .collect::<Vec<_>>(),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_headers_a_download_depends_on_are_forwarded() {
        // `content-type` joined the two range headers when the relay learned to forward
        // a POST: an API told its JSON is `text/plain` rejects it. `accept` and
        // `accept-language` joined them when it learned to fetch watch pages: reqwest's
        // default `Accept: */*` with no language is not a shape any browser sends, and
        // the sites that screen for automation turn it away. Nothing identity-bearing is
        // on this list, and that is the property worth pinning.
        assert_eq!(
            FORWARDED_HEADERS,
            &[
                header::RANGE,
                header::IF_RANGE,
                header::CONTENT_TYPE,
                header::ACCEPT,
                header::ACCEPT_LANGUAGE,
            ]
        );
        for name in FORWARDED_HEADERS {
            assert!(
                !["cookie", "authorization", "x-api-key"].contains(&name.as_str()),
                "{name} carries identity and must not be forwarded"
            );
        }
    }

    #[test]
    fn the_forbidden_headers_a_page_cannot_set_are_asked_for_by_proxy() {
        // `Origin` and `Referer` are on the Fetch standard's forbidden list, so a page
        // cannot set them however much an upstream needs one. This trio is the whole
        // reason a relay can reach an API a page cannot.
        let asked: Vec<&str> = HEADER_OVERRIDES.iter().map(|(a, _)| *a).collect();
        assert_eq!(
            asked,
            vec!["x-relay-origin", "x-relay-referer", "x-relay-user-agent"]
        );
        let real: Vec<&str> = HEADER_OVERRIDES.iter().map(|(_, r)| r.as_str()).collect();
        assert_eq!(real, vec!["origin", "referer", "user-agent"]);
    }

    // The endpoints this exists for send a few kilobytes of JSON, so the cap is what
    // stops the relay being used as a file drop. Checked at compile time: the value is a
    // constant, and a runtime assertion over two constants is a test that can never fail
    // for any reason a reader would care about.
    const _: () = assert!(MAX_REQUEST_BODY <= 1024 * 1024);
    const _: () = assert!(MAX_REQUEST_BODY >= 64 * 1024);

    #[test]
    fn no_identity_bearing_response_header_is_copied() {
        for forbidden in [
            "set-cookie",
            "set-cookie2",
            "authorization",
            "www-authenticate",
            "proxy-authenticate",
            "cookie",
        ] {
            assert!(
                !COPIED_HEADERS.contains(&forbidden),
                "{forbidden} must not be relayed"
            );
        }
    }

    #[test]
    fn every_copied_header_is_a_valid_lowercase_header_name() {
        for name in COPIED_HEADERS {
            assert_eq!(*name, name.to_ascii_lowercase());
            // Panics if the name is not valid, which is what the CORS layer would do
            // at startup rather than at request time.
            let _ = HeaderName::from_static(name);
        }
    }

    #[test]
    fn a_wildcard_origin_and_an_explicit_list_both_build_a_cors_layer() {
        let cfg = Config::default();
        assert!(cors_layer(&cfg).is_ok());
        let cfg = Config {
            allowed_origins: vec!["https://app.example.com".into()],
            ..Config::default()
        };
        assert!(cors_layer(&cfg).is_ok());
    }

    #[test]
    fn a_malformed_origin_is_rejected_at_startup_not_at_request_time() {
        let cfg = Config {
            allowed_origins: vec!["not a header value\n".into()],
            ..Config::default()
        };
        assert!(cors_layer(&cfg).is_err());
    }

    #[test]
    fn the_client_builds_with_cookies_and_credentials_off() {
        assert!(build_client(&Config::default()).is_ok());
    }
}
