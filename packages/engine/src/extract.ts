// Driving a site extractor.
//
// The Rust side is a state machine that performs no I/O: it says what it needs — a fetch,
// or the loaded page's HTML — and this feeds the answer back until it is done. That is
// the same seam the download engine uses, and it exists for the same reason: every
// site's parsing stays assertable against a captured response instead of a live network.
//
// These are the parts of the product that rot. A site changes its page shape and its
// extractor stops finding what it expects; the error names the site so the failure is
// legible instead of mysterious.

import { fetchWithRetry, FORBIDDEN_HEADERS } from "./fetch-retry";
import type { ExtractedStream } from "./types";
import { loadCore } from "./wasm";

export type { ExtractedStream };

/** Whether a stream carries picture, sound, or both. */
export type StreamKind = ExtractedStream["kind"];

/** One thing the user can choose to download. */
export interface MediaOption {
  label: string;
  rank: number;
  /** One stream when muxed, two when video and audio must be merged. */
  streams: ExtractedStream[];
  filename: string;
  width: number | null;
  height: number | null;
  duration_ms: number | null;
}

export interface ExtractedSubtitle {
  label: string;
  language: string | null;
  url: string;
  format: string;
}

/** One video rendition the user can choose, paired with any audio they like. */
export interface VideoChoice {
  id: string;
  label: string;
  width: number | null;
  height: number | null;
  fps: number | null;
  bitrate: number | null;
  codec: string | null;
  size: number | null;
  stream: ExtractedStream;
  /** True when this rendition already carries sound, so no audio need be chosen. */
  has_audio: boolean;
  /** The highest quality that can actually be delivered as one playable file. */
  best: boolean;
  /** `"mp4"`, `"webm"`, or whatever the site's mime said. */
  container: string | null;
  /**
   * Whether this rendition can be joined to an audio track.
   *
   * A container question rather than a codec one: the merger copies track descriptions
   * through verbatim, so AV1 and H.264 in MP4 both work and WebM does not.
   */
  mergeable: boolean;
}

/** One audio rendition the user can choose. */
export interface AudioChoice {
  id: string;
  label: string;
  bitrate: number | null;
  codec: string | null;
  language: string | null;
  size: number | null;
  stream: ExtractedStream;
  best: boolean;
  container: string | null;
  mergeable: boolean;
}

export interface Extraction {
  site: string;
  title: string;
  /** Ready-made pairings, best first — the one-click path. */
  options: MediaOption[];
  /** Every video rendition, best first, for choosing picture and sound separately. */
  videos: VideoChoice[];
  /** Every audio rendition, best first. Empty when the site muxes its audio in. */
  audios: AudioChoice[];
  subtitles: ExtractedSubtitle[];
  /**
   * Something true about this result that the rendition list does not say.
   *
   * Bilibili is the case it exists for: it advertises every quality a video has, then
   * serves a signed-out caller only the lowest, so a correct and complete menu still
   * looks broken because the advertised 1080p is not in it.
   */
  note?: string | null;
}

/**
 * Whether this extraction offers a real choice of audio.
 *
 * A site that muxes sound into every rendition returns no audio choices at all, and a UI
 * that showed an audio picker there would be inviting a decision that does not exist.
 */
export function hasAudioChoice(extraction: Extraction): boolean {
  return (
    extraction.audios.length > 0 && extraction.videos.some((v) => !v.has_audio)
  );
}

/** Why a chosen combination cannot be produced, or null when it can. */
export function pairingProblem(
  video: VideoChoice,
  audio: AudioChoice | undefined,
): string | null {
  if (video.has_audio) return null;
  if (!audio)
    return "This rendition has no sound of its own, so it needs an audio track.";
  if (!video.mergeable) {
    return (
      `A ${video.container ?? "non-MP4"} video cannot be joined to an audio track here — ` +
      "only MP4 can. Pick an MP4 rendition, or download this one on its own without sound."
    );
  }
  if (!audio.mergeable) {
    return (
      `A ${audio.container ?? "non-MP4"} audio track cannot be joined to a video here — ` +
      "only MP4 can. Pick an MP4 track, or download this one on its own."
    );
  }
  return null;
}

/**
 * Build a downloadable option from a chosen video and, where the site separates them, a
 * chosen audio.
 *
 * The pairing lives here rather than in each front end so the extension and the web app
 * cannot drift on what "1080p with the 128 kbps track" means.
 *
 * A rendition that carries its own sound ignores `audio` entirely. One that does not and
 * is given none produces a silent file — which is occasionally what someone wants and is
 * never what someone wants by accident, so the caller is expected to have defaulted to
 * the best audio and the label says plainly when there is none.
 */
export function pair(
  video: VideoChoice,
  audio: AudioChoice | undefined,
  titleForFilename: string,
  durationMs: number | null = null,
): MediaOption {
  const needsAudio = !video.has_audio && audio !== undefined;
  const streams = needsAudio ? [video.stream, audio.stream] : [video.stream];
  const label = video.has_audio
    ? video.label
    : audio
      ? `${video.label} + ${audio.label}`
      : `${video.label} · no sound`;

  return {
    label,
    // Ranked by pixel count so a caller sorting these agrees with the extractor's own
    // ordering rather than inventing a second one.
    rank: (video.width ?? 0) * (video.height ?? 0),
    streams,
    filename: `${titleForFilename}.mp4`,
    width: video.width,
    height: video.height,
    duration_ms: durationMs,
  };
}

/** An audio-only option from a chosen audio track. */
export function audioOnly(
  audio: AudioChoice,
  titleForFilename: string,
): MediaOption {
  return {
    label: audio.label,
    rank: audio.bitrate ?? 0,
    streams: [audio.stream],
    filename: `${titleForFilename}.m4a`,
    width: null,
    height: null,
    duration_ms: null,
  };
}

interface ExtractRequest {
  url: string;
  method: string;
  headers: [string, string][];
  body: string | null;
}

type Need = { Fetch: ExtractRequest[] } | "PageState";
type Step = { Need: Need } | { Done: Extraction };

export interface ExtractOptions {
  /**
   * Supply the loaded page's HTML. The extension injects a reader into the tab; the web
   * app has no tab to read and omits this, which is exactly why the web app cannot reach
   * the sites that block outsiders and the extension can.
   */
  readPageState?: (url: string) => Promise<string>;
  /**
   * Apply headers `fetch` is forbidden from setting, for the duration of one request.
   *
   * `Origin` and `Referer` are both on the Fetch standard's forbidden list, and both are
   * load-bearing here: YouTube's InnerTube endpoint answers 403 to every `Origin` except
   * its own, which means an extension page's fetch — which carries
   * `Origin: chrome-extension://…` — is refused unless the header is rewritten on the
   * way out. The extension does that with a `declarativeNetRequest` rule; a web page
   * cannot, and needs the relay instead.
   */
  applyRequestHeaders?: (
    urls: string[],
    headers: [string, string][],
  ) => Promise<() => Promise<void>>;
  signal?: AbortSignal;
}

/** Whether any extractor claims this URL. */
export async function isSupportedSite(url: string): Promise<boolean> {
  const core = await loadCore();
  return core.site_is_supported(url);
}

/** The name of the site that would handle this URL, for the UI. */
export async function siteFor(url: string): Promise<string | undefined> {
  const core = await loadCore();
  return core.site_name_for(url) ?? undefined;
}

/**
 * Whether a host with no tab to read can resolve this URL at all.
 *
 * Ask this before refusing a link. `siteAcceptsFetchedPage` below answers only half of
 * it — an extractor whose first move is a fetch never wanted a page, so that flag is
 * false for YouTube even though YouTube works here.
 */
export async function siteWorksWithoutATab(url: string): Promise<boolean> {
  const core = await loadCore();
  return core.site_works_without_a_tab(url);
}

/**
 * Whether this URL's extractor can work from a fetched page rather than a loaded tab.
 *
 * A host with no tab to read asks this before offering to fetch the page itself; the
 * sites where that cannot work still get the plain "use the extension there" answer.
 */
export async function siteAcceptsFetchedPage(url: string): Promise<boolean> {
  const core = await loadCore();
  return core.site_accepts_fetched_page(url);
}

/**
 * Decode a `thunder://`, `flashget://` or `qqdl://` link into the URL inside it.
 *
 * These schemes are not protocols: each is an ordinary http/https/ftp URL wrapped in
 * base64 so a link would open in one particular download manager. Undoing the wrapper
 * turns them into a download like any other.
 */
export async function resolveDownloadLink(
  url: string,
): Promise<string | undefined> {
  const core = await loadCore();
  return core.resolve_download_link(url) ?? undefined;
}

/**
 * Why a `magnet:`, `.torrent` or `ed2k://` link cannot be downloaded here, or `undefined`.
 *
 * These name content held by other people's machines, reached over TCP connections a
 * browser tab cannot open. The sentence explains that rather than reporting a bad link.
 */
export async function peerLinkRefusal(
  url: string,
): Promise<string | undefined> {
  const core = await loadCore();
  return core.peer_link_refusal(url) ?? undefined;
}

/** One `ed2k://` link, as the core reads it. */
export type Ed2kLink =
  | {
      kind: "file";
      filename: string;
      size: number;
      hash: string;
      httpSource: string | null;
      aich: string | null;
    }
  | { kind: "server"; host: string; port: number }
  | { kind: "serverlist"; url: string };

/**
 * Parse an `ed2k://` link, or `undefined` when it is not one.
 *
 * A file link names its contents by hash. That hash is checkable here — the engine will
 * compute it during the read-back — but reaching the eDonkey network to *fetch* those
 * bytes is not something a browser can do, so only a link carrying an `httpSource` is
 * downloadable. The caller is expected to say which case it is looking at.
 */
export async function parseEd2kLink(
  uri: string,
): Promise<Ed2kLink | undefined> {
  const core = await loadCore();
  const json = core.parse_ed2k_link(uri);
  return json ? (JSON.parse(json) as Ed2kLink) : undefined;
}

/** One site with a dedicated extractor in this build. */
export interface SupportedSite {
  name: string;
  host: string;
  /** False when only a loaded page will do, so the web app cannot resolve it. */
  withoutATab: boolean;
  /**
   * True when the site answers a page on another domain, so the hosted web app can
   * resolve it with no relay. Most `withoutATab` sites do not: they work from fetches,
   * but only fetches a relay or the extension makes.
   */
  fromAnyOrigin: boolean;
}

/**
 * Every site this build can extract from.
 *
 * Read from the core rather than written out in the UI: a store build compiles the
 * large-platform extractors out, and a hardcoded list would keep advertising them.
 */
export async function supportedSites(): Promise<SupportedSite[]> {
  const core = await loadCore();
  return JSON.parse(core.supported_sites()) as SupportedSite[];
}

/**
 * Host patterns to request permission for besides the page's own.
 *
 * A site streams its video from a CDN on another domain, and a host permission is what
 * lets the extension's network listener see requests to it. Granting only the page's
 * host leaves the listener blind to exactly the requests that matter.
 */
export async function mediaHostPatterns(url: string): Promise<string[]> {
  const core = await loadCore();
  return JSON.parse(core.media_host_patterns(url)) as string[];
}

/** Which track a sniffed URL carries. `"muxed"` means it is a whole file on its own. */
export async function trackKind(
  url: string,
  mime: string | null,
): Promise<"video" | "audio" | "muxed"> {
  const core = await loadCore();
  return core.track_kind(url, mime ?? undefined) as "video" | "audio" | "muxed";
}

/** A source with no extractor: a link kind rather than a website. */
export interface SupportedSource {
  name: string;
  /** What the user pastes, in words — these have no single URL shape. */
  accepts: string;
  /** True when a program on the user's own machine has to be running. */
  needsLocalHelper: boolean;
}

/** Every non-extractor source this build accepts — Mega, Quark, torrents, ed2k, Xunlei. */
export async function supportedSources(): Promise<SupportedSource[]> {
  const core = await loadCore();
  return JSON.parse(core.supported_sources()) as SupportedSource[];
}

/**
 * The job kind an option's first stream should be downloaded as.
 *
 * Both front ends used to hardcode `"progressive"` for anything not being merged, which
 * meant an HLS master playlist was fetched as if it were the file: a couple of kilobytes
 * of `.m3u8` written to a `.mp4`, reported complete, and hashed. The core decides now.
 */
export async function jobKindForStream(stream: {
  url: string;
  mime?: string | null;
}): Promise<"progressive" | "hlsplaylist"> {
  const core = await loadCore();
  return core.stream_is_hls_playlist(stream.mime ?? undefined, stream.url)
    ? "hlsplaylist"
    : "progressive";
}

/** Guards against a site whose extractor loops. Each step is one network round trip. */
const MAX_STEPS = 6;

/**
 * Headers the Fetch standard forbids script from setting.
 *
 * Only the ones an extractor plausibly needs are listed; the full list is longer. These
 * are silently dropped by `fetch` rather than raising, which is why they are routed to
 * the host instead — a silently dropped `Referer` looks exactly like a site that has
 * started refusing you.
 */

/**
 * Run a site's extractor to completion.
 *
 * Throws with the extractor's own message, which is written to be shown to a user: a
 * private post, an age-gated video and a site that has changed its page shape are three
 * different sentences, and the difference matters to whoever is reading it.
 */
export async function extract(
  url: string,
  opts: ExtractOptions = {},
): Promise<Extraction> {
  const core = await loadCore();
  const session = new core.SiteExtractor(url);

  let step = JSON.parse(session.start()) as Step;

  for (let i = 0; i < MAX_STEPS; i++) {
    if ("Done" in step) return step.Done;

    const need = step.Need;
    let bodies: string[];

    if (need === "PageState") {
      if (!opts.readPageState) {
        throw new Error(
          "this site has to be read from the page itself. Open the video in a tab, " +
            "enable this site in the extension, and use the extension there — a web page " +
            "cannot read another site's page for you.",
        );
      }
      bodies = [await opts.readPageState(url)];
    } else {
      bodies = [];
      for (const request of need.Fetch) {
        // Split the headers by whether script may set them. The forbidden ones are
        // handed to the host to apply out-of-band; the rest go on the fetch directly.
        const direct: Record<string, string> = {};
        const forbidden: [string, string][] = [];
        for (const [name, value] of request.headers) {
          if (FORBIDDEN_HEADERS.has(name.toLowerCase()))
            forbidden.push([name, value]);
          else direct[name] = value;
        }

        let release: (() => Promise<void>) | undefined;
        if (forbidden.length > 0 && opts.applyRequestHeaders) {
          release = await opts.applyRequestHeaders([request.url], forbidden);
        } else if (forbidden.length > 0) {
          // No host mechanism — the web app. The relay can send these on the page's
          // behalf, and asks for them through `x-relay-*`, which are ordinary headers a
          // page is allowed to set. When no relay is configured the rewrite is the
          // identity function, the real headers never arrive, and the site says 403 —
          // which is what the caller then explains.
          for (const [name, value] of forbidden) {
            direct[`x-relay-${name.toLowerCase()}`] = value;
          }
        }

        try {
          const response = await fetchWithRetry(request.url, {
            headers: direct,
            signal: opts.signal,
            method: request.method,
            body: request.body ?? undefined,
          });
          if (!response.ok) {
            throw new Error(
              response.status === 403 &&
                forbidden.length > 0 &&
                !opts.applyRequestHeaders
                ? `${new URL(request.url).hostname} refused this request. It requires a header ` +
                    "a web page is not allowed to send, so this site needs the browser " +
                    "extension, or a relay you run yourself."
                : response.status === 403
                  ? // A 403 from a site's own player API is nearly always the video
                    // being restricted rather than anything wrong at this end: Vimeo
                    // returns it for clips playable only on their own page or on
                    // approved domains, and no header or retry changes that. Saying so
                    // stops the reader debugging their setup.
                    `${new URL(request.url).hostname} refused this video (403). It is ` +
                    "usually restricted by whoever posted it — playable on the site " +
                    "itself, or only on domains they approved, and no permission or " +
                    "retry here changes that."
                  : `${new URL(request.url).hostname} answered ${response.status}`,
            );
          }
          bodies.push(await response.text());
        } finally {
          await release?.().catch(() => undefined);
        }
      }
    }

    step = JSON.parse(session.feed(JSON.stringify(bodies))) as Step;
  }

  throw new Error(
    "the extractor did not finish; this looks like a bug rather than a site change",
  );
}

/** True when an option needs two streams merged into one file. */
export function needsMerge(option: MediaOption): boolean {
  return option.streams.length > 1;
}

/** The stream of a given kind, if the option has one. */
export function streamOfKind(
  option: MediaOption,
  kind: StreamKind,
): ExtractedStream | undefined {
  return option.streams.find((s) => s.kind === kind);
}
