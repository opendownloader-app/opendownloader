// The download engine.
//
// This file performs every network and disk operation; it makes no decisions.
// What to fetch next, whether a resume is still valid, how a segment becomes
// MP4, and what the file hashes to are all answered by `dl_core`. If a
// conditional in here starts encoding download semantics rather than browser
// mechanics, it belongs in Rust where it can be tested.

import { engineConfig } from "./config";
import { crossOriginFromPage, fetchWithRetry } from "./fetch-retry";
import { runMerge } from "./merge";
import { updateJob } from "./jobs";
import { getSettings } from "./settings";
import type { Sink } from "./sinks";
import type {
  HlsSegment,
  HlsVariant,
  Job,
  MasterPlaylist,
  ParsedPlaylist,
  Progress,
  Rendition,
  Verification,
} from "./types";
import { loadCore, type DlCore } from "./wasm";

/** Persist resume state at most this often, to keep IndexedDB writes bounded. */
const STATE_FLUSH_MS = 1000;

type ProgressFn = (p: Progress) => void;

export interface RunOptions {
  onProgress: ProgressFn;
  /** Aborting mid-run pauses the job; progress already persisted is kept. */
  signal?: AbortSignal;
  /**
   * Parallel range requests, when the server and the sink both allow it.
   * Defaults to the user's setting.
   */
  connections?: number;
}

interface ProbeResult {
  total: number | null;
  acceptsRanges: boolean;
  validator: string | null;
  /** Kept apart as well as combined, so a later answer is compared like with like. */
  etag: string | null;
  lastModified: string | null;
  mime: string;
}

type Session = InstanceType<DlCore["DownloadSession"]>;

/** Raised when the user pauses. Distinguished from a failure by `runJob`. */
export class PausedError extends Error {
  constructor() {
    super("paused");
    this.name = "PausedError";
  }
}

function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted) throw new PausedError();
}

/**
 * A moving throughput estimate.
 *
 * A running average over the whole download reads far too low after a stall and
 * far too high right after one, which makes the ETA swing wildly. A short
 * window of recent samples tracks what the connection is doing now, which is
 * the number a person is actually asking for when they look at it.
 */
class Rate {
  private readonly samples: { at: number; bytes: number }[] = [];
  private static readonly WINDOW_MS = 8000;

  record(totalBytes: number, at = Date.now()): void {
    this.samples.push({ at, bytes: totalBytes });
    const cutoff = at - Rate.WINDOW_MS;
    while (this.samples.length > 2 && (this.samples[0]?.at ?? 0) < cutoff) {
      this.samples.shift();
    }
  }

  /** Bytes per second, or undefined until there is a measurable interval. */
  perSecond(): number | undefined {
    const first = this.samples[0];
    const last = this.samples[this.samples.length - 1];
    if (!first || !last) return undefined;
    const seconds = (last.at - first.at) / 1000;
    if (seconds <= 0.25) return undefined;
    const bytes = last.bytes - first.bytes;
    return bytes <= 0 ? 0 : bytes / seconds;
  }

  /** Seconds remaining, or null when the total is unknown or nothing is moving. */
  eta(received: number, total: number | null): number | null {
    const rate = this.perSecond();
    if (!rate || total === null || total <= received) return null;
    return (total - received) / rate;
  }
}

/**
 * Ask the server what it supports before committing to a strategy.
 *
 * A one-byte range request is used rather than HEAD: some CDNs answer HEAD
 * differently from GET (or reject it outright), and a `206` with a
 * `Content-Range` proves range support in a way `Accept-Ranges` alone does not.
 */
/**
 * What a failed response should be reported as.
 *
 * A status alone explains nothing — "server returned 409" is true and useless. Some of
 * the things this talks to answer with a sentence saying what actually happened, and the
 * local torrent bridge is one: a swarm with no seeders is not a fault anyone can fix, and
 * the count of peers that dropped the connection is the fact that says so. So the body is
 * used when it carries a message, and the status stands alone when it does not.
 */
async function failureFor(res: Response): Promise<Error> {
  const fallback = `server returned ${res.status} ${res.statusText}`;
  try {
    const text = (await res.text()).slice(0, 2000);
    const message = (JSON.parse(text) as { error?: string })?.error ?? null;
    return new Error(message && message.length > 0 ? message : fallback);
  } catch {
    return new Error(fallback);
  }
}

async function probe(url: string, signal?: AbortSignal): Promise<ProbeResult> {
  const res = await fetchWithRetry(url, {
    headers: { Range: "bytes=0-0" },
    signal,
  });
  if (!res.ok && res.status !== 206) {
    throw await failureFor(res);
  }
  // Drain so the connection can be reused rather than left dangling.
  await res.arrayBuffer();

  const contentRange = res.headers.get("content-range");
  const acceptsRanges = res.status === 206 && contentRange !== null;

  let total: number | null = null;
  if (contentRange) {
    // "bytes 0-0/12345" — the part after the slash is the full length, or "*".
    const size = contentRange.split("/")[1];
    const parsed = size === undefined || size === "*" ? NaN : Number(size);
    total = Number.isFinite(parsed) ? parsed : null;
  } else {
    const len = res.headers.get("content-length");
    const parsed = len === null ? NaN : Number(len);
    total = Number.isFinite(parsed) ? parsed : null;
  }

  return {
    total,
    acceptsRanges,
    // ETag is preferred over Last-Modified: it is exact, where a
    // second-granularity timestamp can miss a change made within the same second.
    validator: res.headers.get("etag") ?? res.headers.get("last-modified"),
    etag: res.headers.get("etag"),
    lastModified: res.headers.get("last-modified"),
    mime:
      res.headers.get("content-type")?.split(";")[0]?.trim() ??
      "application/octet-stream",
  };
}

/**
 * A chunk decryptor, or `null` when the job is plain bytes.
 *
 * Built once per job rather than per chunk: importing a key is not free and a large
 * download does this thousands of times.
 */
async function decryptorFor(
  job: Job,
): Promise<
  | ((offset: number, bytes: Uint8Array<ArrayBuffer>) => Promise<Uint8Array>)
  | null
> {
  if (!job.decrypt) return null;
  const raw = Uint8Array.from(atob(job.decrypt.key), (c) => c.charCodeAt(0));
  const nonce = Uint8Array.from(atob(job.decrypt.nonce), (c) =>
    c.charCodeAt(0),
  );
  const key = await crypto.subtle.importKey("raw", raw, "AES-CTR", false, [
    "decrypt",
  ]);

  return async (offset, bytes) => {
    // The counter block is the 8-byte nonce followed by the block index, big-endian.
    const counter = new Uint8Array(16);
    counter.set(nonce, 0);
    // The block index, big-endian, written by hand: a DataView over a Uint8Array's
    // buffer is typed as possibly shared, and the loop states the byte order plainly
    // anyway — which is the part worth being unambiguous about.
    let block = BigInt(Math.floor(offset / 16));
    for (let i = 15; i >= 8; i--) {
      counter[i] = Number(block & 0xffn);
      block >>= 8n;
    }

    // An offset that does not land on a block boundary would put the keystream out of
    // step. Pad to the boundary, decrypt, then drop the padding: CTR is a stream cipher,
    // so decrypting bytes we then discard costs nothing but alignment.
    const skip = offset % 16;
    let input: Uint8Array<ArrayBuffer> = bytes;
    if (skip !== 0) {
      input = new Uint8Array(skip + bytes.length);
      input.set(bytes, skip);
    }

    const out = new Uint8Array(
      await crypto.subtle.decrypt(
        { name: "AES-CTR", counter, length: 64 },
        key,
        input,
      ),
    );
    return skip === 0 ? out : out.subarray(skip);
  };
}

/**
 * How many chunks to queue per connection.
 *
 * Deep enough that a worker finishing early always has something waiting, shallow enough
 * that the plan is re-derived often enough to notice ranges recorded by a resumed
 * session. Eight is comfortably past the point where the barrier stops being visible.
 */
const QUEUE_DEPTH = 8;

/**
 * Whether a ranged answer describes a different file from the one probed.
 *
 * The check `If-Range` would have made, made on the response instead, for the requests
 * that cannot carry it. Only what the browser lets this page read is compared — a CDN
 * that exposes no headers leaves nothing to compare, and then nothing is concluded.
 */
function servedADifferentFile(res: Response, info: ProbeResult): boolean {
  const etag = res.headers.get("etag");
  if (info.etag !== null && etag !== null) return etag !== info.etag;
  const modified = res.headers.get("last-modified");
  if (info.lastModified !== null && modified !== null) return modified !== info.lastModified;
  const total = totalFromContentRange(res.headers.get("content-range"));
  return info.total !== null && total !== null && total !== info.total;
}

/** The `12345` in `bytes 0-99/12345`, or null when absent or `*`. */
function totalFromContentRange(header: string | null): number | null {
  const size = header?.split("/")[1];
  if (size === undefined || size === "*") return null;
  const parsed = Number(size);
  return Number.isFinite(parsed) ? parsed : null;
}

/** Whether the file a saved session was downloading is not the one there now. */
function resumedOntoADifferentFile(stateJson: string, info: ProbeResult): boolean {
  let saved: { validator?: string | null; total?: number | null } | undefined;
  try {
    saved = (JSON.parse(stateJson) as { resume?: typeof saved }).resume;
  } catch {
    return false;
  }
  if (!saved) return false;
  if (saved.validator && info.validator && saved.validator !== info.validator) return true;
  return (
    typeof saved.total === "number" && info.total !== null && saved.total !== info.total
  );
}

/** Run a progressive (single-resource) download to completion. */
async function runProgressive(
  job: Job,
  core: DlCore,
  sink: Sink,
  opts: RunOptions,
  connections: number,
): Promise<{ session: Session; total: number | null }> {
  const info = await probe(job.url, opts.signal);
  const decryptChunk = await decryptorFor(job);

  // Resume only if we have prior state *and* the server still supports ranges.
  // Restoring a range-based plan against a server that has stopped honouring
  // ranges would quietly write the whole body at every planned offset.
  const resuming = Boolean(job.stateJson) && info.acceptsRanges;

  // A resume is only safe onto the file the first bytes came from. This used to be left
  // to `If-Range` — but the validator sent was the one just probed, which matches
  // whatever the server holds now, so a file replaced between sessions resumed cleanly
  // and the two were spliced together. The session saved the original; compare that.
  if (resuming && resumedOntoADifferentFile(job.stateJson!, info)) {
    throw new Error("the file changed on the server; restart this download");
  }
  const session: Session = resuming
    ? core.DownloadSession.restore(job.stateJson)
    : new core.DownloadSession(
        info.total === null ? undefined : BigInt(info.total),
        info.acceptsRanges,
        false,
        false,
      );
  session.setValidator(info.validator ?? undefined);

  // A sink that cannot seek must receive bytes in order, so it gets one
  // connection regardless of what the server would allow.
  const parallel = sink.canSeek ? connections : 1;

  // Some hosts cap a single range and answer 403 above it — Google's media servers stop
  // at 1 MiB. The cap is a property of the host rather than a tuning choice, so it wins
  // over the configured chunk size rather than being averaged with it.
  const chunkSize = Math.min(
    engineConfig.chunkSize,
    job.maxChunkBytes ?? Number.MAX_SAFE_INTEGER,
  );
  const rate = new Rate();
  rate.record(Number(session.downloaded()));
  let lastFlush = 0;

  const conditionalCostsAPreflight = crossOriginFromPage(job.url);

  /** One chunk, start to disk. Lifted out so a worker can call it in a loop. */
  const fetchChunk = async (r: {
    start: number;
    end: number;
  }): Promise<void> => {
    const headers: Record<string, string> = {};
    if (info.acceptsRanges) {
      const openEnded = r.end >= Number.MAX_SAFE_INTEGER;
      headers.Range = openEnded
        ? `bytes=${r.start}-`
        : `bytes=${r.start}-${r.end}`;
      // If-Range makes the server answer 200-with-whole-body instead of 206
      // when the resource changed, rather than silently splicing two different
      // files together. Only where it is free, though: from a web page to another
      // origin it forces a CORS preflight, and a CDN that refuses those fails every
      // chunk (APP-82). There the answer itself is checked instead, below.
      if (info.validator && !conditionalCostsAPreflight) {
        headers["If-Range"] = info.validator;
      }
    }

    const res = await fetchWithRetry(job.url, {
      headers,
      signal: opts.signal,
      // See `Job.maxChunkBytes`: a host that states a request size throttles with
      // 403 rather than refusing outright, and waiting is the right response.
      retryForbidden: job.maxChunkBytes !== undefined,
      onRetry: (attempt, delay, reason) =>
        opts.onProgress({
          received: Number(session.downloaded()),
          total: info.total,
          status: "downloading",
          message: `${reason} — retry ${attempt} in ${Math.round(delay / 1000)}s`,
        }),
    });
    if (!res.ok && res.status !== 206) {
      // The server's own sentence where it has one — a torrent bridge explains a dead
      // swarm here, and "chunk 0 failed: 409" would throw that explanation away.
      const why = await failureFor(res);
      throw new Error(`chunk ${r.start} failed: ${why.message}`);
    }
    // A range asked of a server that does ranges and answered with the whole body: with
    // `If-Range` that is the server saying the file changed, and without it the body is
    // still the whole file, which written at this chunk's offset would corrupt it. Either
    // way nothing here can be kept.
    if (
      info.acceptsRanges &&
      (res.status === 200 || servedADifferentFile(res, info))
    ) {
      throw new Error("the file changed on the server; restart this download");
    }

    const raw = new Uint8Array(await res.arrayBuffer());
    if (raw.length === 0) return;
    // Decrypt before the sink, so what lands on disk is the plaintext and the
    // read-back digest describes the file the user actually has.
    const bytes = decryptChunk ? await decryptChunk(r.start, raw) : raw;
    await sink.write(r.start, bytes);
    session.record(BigInt(r.start), BigInt(r.start + bytes.length - 1));
    await afterChunk();
  };

  /**
   * Progress and resume state, per finished chunk.
   *
   * These used to sit after the batch, which was fine when a batch was one round of
   * `parallel` chunks. With a queue eight times deeper that became eight times less
   * often: the bar looked stuck, and — worse — a download paused early had never
   * flushed, so resuming started from zero. The e2e suite caught exactly that.
   */
  let flushing = false;
  const afterChunk = async (): Promise<void> => {
    const received = Number(session.downloaded());
    rate.record(received);
    opts.onProgress({
      received,
      total: info.total,
      status: "downloading",
      bytesPerSecond: rate.perSecond(),
      etaSeconds: rate.eta(received, info.total),
    });

    const now = Date.now();
    // Time-gated, and never two at once: workers finish concurrently, and overlapping
    // writes of the same row buy nothing.
    if (now - lastFlush > STATE_FLUSH_MS && !flushing) {
      flushing = true;
      lastFlush = now;
      try {
        await updateJob(job.id, {
          stateJson: session.stateJson(),
          receivedBytes: received,
        });
      } finally {
        flushing = false;
      }
    }
  };

  for (;;) {
    throwIfAborted(opts.signal);

    // Plan far more chunks than there are connections.
    //
    // This loop used to plan exactly `parallel` chunks and `Promise.all` them, which
    // made every batch a barrier: three connections finishing in 100ms sat idle until
    // the fourth finished, and a download's throughput became the slowest chunk of each
    // batch, repeatedly. Queueing depth ahead of the workers is what removes that — a
    // connection that finishes takes the next chunk immediately instead of waiting for
    // its peers. It is the cheaper half of what a dedicated download manager does.
    const ranges = JSON.parse(
      session.plan(BigInt(chunkSize), parallel * QUEUE_DEPTH),
    ) as { start: number; end: number }[];
    if (ranges.length === 0) break;

    // Workers pull from one shared queue, so a slow chunk delays only its own worker.
    let next = 0;
    try {
      await Promise.all(
        Array.from({ length: Math.min(parallel, ranges.length) }, async () => {
          for (;;) {
            throwIfAborted(opts.signal);
            const r = ranges[next++];
            if (!r) return;
            await fetchChunk(r);
          }
        }),
      );
    } catch (e) {
      // Persist what did land before giving up.
      //
      // A pause aborts every worker at once, and the periodic flush only runs when a
      // chunk *finishes* — so an abort that arrives while all of them are mid-request
      // left the row saying zero bytes and threw away real progress on resume. Rarer
      // with shallow batches, which is why it survived until the queue got deeper.
      await updateJob(job.id, {
        stateJson: session.stateJson(),
        receivedBytes: Number(session.downloaded()),
      }).catch(() => {
        // The original failure is the one worth reporting.
      });
      throw e;
    }

    // Without range support there is exactly one request, and it either
    // delivered everything or the download cannot be completed this way.
    if (!info.acceptsRanges) break;
  }

  return { session, total: info.total };
}

export interface ResolvedPlaylist {
  segments: HlsSegment[];
  init: HlsSegment | null;
  remux: boolean;
}

/** Everything a master playlist offers, for the UI to choose between. */
export interface PlaylistOptions {
  variants: HlsVariant[];
  audio: Rendition[];
  subtitles: Rendition[];
}

async function fetchPlaylist(
  url: string,
  core: DlCore,
  signal?: AbortSignal,
): Promise<ParsedPlaylist> {
  const text = await (await fetchWithRetry(url, { signal })).text();
  return JSON.parse(core.parse_playlist_js(text, url)) as ParsedPlaylist;
}

/**
 * Fetch and parse a playlist, following a master down to a media playlist.
 *
 * `variantUrl`, when given, selects a specific rendition instead of letting the
 * highest bandwidth win — that is what the quality picker passes in. `audioUrl`
 * overrides both, and is what an audio-only download of a stream whose audio is
 * a separate `#EXT-X-MEDIA` rendition follows.
 */
export async function resolvePlaylist(
  url: string,
  core: DlCore,
  variantUrl?: string,
  signal?: AbortSignal,
  audioUrl?: string | null,
): Promise<ResolvedPlaylist> {
  if (audioUrl) return resolvePlaylist(audioUrl, core, undefined, signal);

  const parsed = await fetchPlaylist(url, core, signal);

  if ("Master" in parsed) {
    const master = parsed.Master;
    const chosen =
      (variantUrl && master.variants.find((v) => v.url === variantUrl)) ||
      pickHighest(master.variants, core);
    if (!chosen) throw new Error("playlist lists no usable renditions");
    return resolvePlaylist(chosen.url, core, undefined, signal);
  }

  const media = parsed.Media;
  if (media.is_live) {
    throw new Error("live streams have no end to download to");
  }
  // An `#EXT-X-MAP` init segment means the variant is already fragmented MP4,
  // so its segments need concatenating, not remuxing. A TS variant has no init
  // segment and does need the demux/mux pipeline.
  return {
    segments: media.segments,
    init: media.init,
    remux: media.init === null,
  };
}

function pickHighest(
  variants: HlsVariant[],
  core: DlCore,
): HlsVariant | undefined {
  const index = core.select_variant_index(JSON.stringify(variants), true);
  return index === undefined ? undefined : variants[index];
}

/**
 * List what a master playlist offers: renditions, alternate audio, subtitles.
 *
 * A media playlist has none of those; it comes back as three empty lists, which
 * is what lets the UI say "single quality only" rather than leaving a button
 * that appears to do nothing.
 */
export async function listPlaylistOptions(
  url: string,
  core: DlCore,
  signal?: AbortSignal,
): Promise<PlaylistOptions> {
  const parsed = await fetchPlaylist(url, core, signal);
  if (!("Master" in parsed)) return { variants: [], audio: [], subtitles: [] };
  const master: MasterPlaylist = parsed.Master;
  return {
    variants: master.variants,
    audio: master.audio,
    subtitles: master.subtitles,
  };
}

/**
 * The alternate audio rendition to download for an audio-only job, if any.
 *
 * A stream whose audio is muxed into the video variant has none, and the
 * remuxer's audio-only mode handles it by dropping the video track. A CMAF
 * stream with a separate audio group has a dedicated playlist that is smaller
 * to download and needs no demuxing at all, so it wins when present.
 */
export function audioRenditionFor(
  options: PlaylistOptions,
  variantUrl?: string,
): Rendition | undefined {
  const variant = variantUrl
    ? options.variants.find((v) => v.url === variantUrl)
    : options.variants.slice().sort((a, b) => b.bandwidth - a.bandwidth)[0];
  const group = variant?.audio_group ?? null;
  const inGroup = options.audio.filter(
    (a) => a.url && (group === null || a.group_id === group),
  );
  return inGroup.find((a) => a.default) ?? inGroup[0];
}

/** Run an HLS download to completion. */
async function runHls(
  job: Job,
  core: DlCore,
  sink: Sink,
  opts: RunOptions,
): Promise<{ session: Session; total: number | null }> {
  // The segment list is resolved once and persisted, so a resumed job neither
  // refetches the playlist nor risks a different variant being chosen the
  // second time around.
  const resolved: ResolvedPlaylist = job.segments?.length
    ? {
        segments: job.segments,
        init: job.initSegment ?? null,
        remux: job.remux ?? true,
      }
    : await resolvePlaylist(
        job.url,
        core,
        job.variantUrl,
        opts.signal,
        job.audioRenditionUrl,
      );

  await updateJob(job.id, {
    segments: resolved.segments,
    initSegment: resolved.init,
    remux: resolved.remux,
  });

  // Audio-only is handled by the remuxer only when the audio is muxed into a
  // transport stream. When a separate audio rendition was followed, its
  // segments are already audio alone, so asking the remuxer to drop video would
  // be a no-op at best and, for an fMP4 rendition, is not even a code path.
  const dropVideo = (job.audioOnly ?? false) && !job.audioRenditionUrl;

  // A remuxing session restores its carry-over — fragment sequence, decode-time
  // origin, whether the init segment was already written — so resuming
  // continues the same output file instead of starting a second one on top of
  // it. Resume granularity is one segment, the smallest independently decodable
  // unit; anything finer would splice a partial fragment.
  const session: Session = job.stateJson
    ? core.DownloadSession.restore(job.stateJson)
    : new core.DownloadSession(undefined, false, resolved.remux, dropVideo);

  const startIndex = session.nextSegment();
  let written = Number(session.outputLen());
  const rate = new Rate();
  rate.record(written);

  if (startIndex === 0 && resolved.init) {
    const bytes = new Uint8Array(
      await (
        await fetchWithRetry(resolved.init.url, { signal: opts.signal })
      ).arrayBuffer(),
    );
    await sink.write(0, bytes);
    written += bytes.length;
    session.noteOutput(BigInt(bytes.length));
  }

  for (let i = startIndex; i < resolved.segments.length; i++) {
    throwIfAborted(opts.signal);
    const seg = resolved.segments[i];
    if (!seg) continue;

    const headers: Record<string, string> = {};
    if (seg.byte_range) {
      headers.Range = `bytes=${seg.byte_range.start}-${seg.byte_range.end}`;
    }
    const res = await fetchWithRetry(seg.url, {
      headers,
      signal: opts.signal,
      onRetry: (attempt, delay, reason) =>
        opts.onProgress({
          received: written,
          total: null,
          status: "downloading",
          message: `segment ${i + 1}: ${reason} — retry ${attempt} in ${Math.round(delay / 1000)}s`,
        }),
    });
    if (!res.ok && res.status !== 206) {
      const why = await failureFor(res);
      throw new Error(
        `segment ${i + 1}/${resolved.segments.length} failed: ${why.message}`,
      );
    }
    const raw = new Uint8Array(await res.arrayBuffer());

    // Remuxing happens in Rust; this side only moves bytes.
    //
    // Every segment goes through the session, including an fMP4 one that needs
    // no remuxing at all — `push_segment` returns those verbatim. That costs one
    // copy and buys the thing that matters: the session's segment counter is
    // what a resume restarts from, and a passthrough stream that skipped this
    // call would resume from segment zero and write the whole file again.
    const out = session.pushSegment(raw);
    await sink.write(written, out);
    written += out.length;

    rate.record(written);
    // Segment count is the only honest progress measure here: the output size
    // is not known until the last segment has been muxed.
    const done = i + 1;
    const perSegment = rate.perSecond();
    opts.onProgress({
      received: written,
      total: null,
      status: "downloading",
      message: `segment ${done} of ${resolved.segments.length}`,
      bytesPerSecond: perSegment,
      etaSeconds:
        perSegment && done > startIndex
          ? ((resolved.segments.length - done) * written) / done / perSegment
          : null,
    });
    // Persisted every segment, not on a timer: the segment index is the resume
    // point, so losing it costs a re-download of everything after it.
    await updateJob(job.id, {
      outputBytes: written,
      receivedBytes: written,
      stateJson: session.stateJson(),
    });
  }

  return { session, total: written };
}

/**
 * Run one job start to finish: download, verify, finalize.
 *
 * Verification reads the finished bytes back and hashes them, so the digest
 * describes what is actually on disk rather than what was intended. When the
 * user supplied an expected digest, the comparison is reported as
 * `verified`/`mismatch`; otherwise the digest is simply recorded.
 *
 * The sink is created by the caller, not here, and the reason is subtle:
 * `showSaveFilePicker` requires transient user activation, and activation is
 * consumed or expires across the `await`s this function performs before it would
 * reach the picker. Opening the sink in the click handler itself is the only
 * placement where the gesture is reliably still valid.
 */
export async function runJob(
  job: Job,
  sink: Sink,
  opts: RunOptions,
): Promise<void> {
  const core = await loadCore();
  const connections =
    opts.connections ?? (await getSettings()).connectionsPerJob;
  await updateJob(job.id, {
    status: "probing",
    error: null,
    sinkKind: sink.kind,
  });
  opts.onProgress({
    received: job.receivedBytes,
    total: job.totalBytes,
    status: "probing",
  });

  // A merge is its own path end to end: it fetches two streams, combines them, and
  // does its own verification, because the thing being hashed is output the muxer
  // produced rather than a copy of anything a server sent.
  if (job.kind === "merge") {
    const streams = job.mergeStreams;
    if (!streams || streams.length !== 2) {
      throw new Error(
        "this job is missing one of its two streams; remove it and pick again",
      );
    }
    const [video, audio] = streams;
    const merged = await runMerge(job, video, audio, sink, {
      signal: opts.signal,
      onProgress: opts.onProgress,
    });
    const wanted = job.expectedSha256?.trim().toLowerCase();
    await updateJob(job.id, {
      status: "done",
      sha256: merged.sha256,
      verification: !wanted
        ? "unverified"
        : wanted === merged.sha256
          ? "verified"
          : "mismatch",
      outputBytes: merged.bytes,
      receivedBytes: merged.bytes,
    });
    opts.onProgress({
      received: merged.bytes,
      total: merged.bytes,
      status: "done",
    });
    return;
  }

  const { session, total } =
    job.kind === "hlsplaylist"
      ? await runHls(job, core, sink, opts)
      : await runProgressive(job, core, sink, opts, connections);

  opts.onProgress({
    received: Number(session.downloaded()),
    total,
    status: "verifying",
  });
  await updateJob(job.id, {
    status: "verifying",
    stateJson: session.stateJson(),
  });

  // Order matters, and getting it wrong is silent. `finalize` is what commits
  // the File System Access swap file to the real file; reading back before that
  // returns the file as it was *before* the download, which for a fresh save
  // target is zero bytes — producing the empty-string digest for every download.
  // So: commit, then verify what was committed, then release.
  await sink.finalize();

  session.resetHash();
  // An ed2k link states its own hash, so that job's read-back computes both digests in
  // the one pass over the disk. Enabled before the read, never after: switching it on
  // partway would hash only the tail and call the result a match or a mismatch on the
  // strength of it.
  const wantedEd2k = job.expectedEd2k?.trim().toLowerCase();
  if (wantedEd2k) session.enableEd2k();

  await sink.readBack((bytes) => session.hashUpdate(bytes));
  const sha256 = session.hashHex();
  const ed2k = wantedEd2k ? session.ed2kHex() : undefined;
  const verifiedBytes = Number(session.hashedLen());

  await sink.cleanup();

  // Whichever digest the job was given, that is the one that decides. A job carrying an
  // ed2k hash still records its SHA-256, because that is the vocabulary the rest of the
  // manager speaks — but it is the ed2k hash that was promised, so it is the ed2k hash
  // that gets to say "verified".
  const expected = job.expectedSha256?.trim().toLowerCase();
  const verification: Verification = wantedEd2k
    ? wantedEd2k === ed2k
      ? "verified"
      : "mismatch"
    : !expected
      ? "unverified"
      : expected === sha256
        ? "verified"
        : "mismatch";

  await updateJob(job.id, {
    status: "done",
    sha256,
    ed2k: ed2k ?? null,
    verification,
    outputBytes: verifiedBytes,
    receivedBytes: verifiedBytes,
    stateJson: session.stateJson(),
  });
  opts.onProgress({
    received: verifiedBytes,
    total: verifiedBytes,
    status: "done",
  });

  if (verification === "mismatch") {
    // Not thrown: the file downloaded fine and is on disk, and deleting it
    // would be a worse outcome than telling the truth about its digest. The
    // job is left `done` with a mismatch flag the UI shows in red.
    console.warn(
      `[opendownloader] ${job.filename}: expected ${expected}, got ${sha256}`,
    );
  }
}
