// The queue: what runs, how many at once, and what happens when one finishes.
//
// The engine downloads one job. This decides which jobs it downloads and when,
// which is the whole difference between "click each file" and a batch you can
// walk away from.
//
// Two constraints shape it. First, a job can only start unattended when a sink
// can be opened without a user gesture — on Chrome and Edge that means a
// download folder has been granted; a save dialog per file cannot be automated
// and must not be attempted. Second, everything the queue knows lives in
// IndexedDB, so closing the tab pauses the queue rather than losing it.

import { PausedError, runJob, type RunOptions } from "./engine";
import { getJob, listJobs, updateJob } from "./jobs";
import type { Platform } from "./platform";
import { crossOriginFromPage, looksLikeCorsFailure } from "./fetch-retry";
import { getSettings } from "./settings";
import {
  canOpenSinkSilently,
  createSink,
  createSinkInteractive,
  type Sink,
} from "./sinks";
import type { Job, Progress } from "./types";

export interface QueueEvents {
  /** Something changed that the UI should re-render for. */
  onChange(): void;
  /** Live progress for one job. Not persisted — the UI holds it in memory. */
  onProgress(jobId: string, progress: Progress): void;
}

const MIME_FOR_KIND: Record<Job["kind"], string> = {
  hlsplaylist: "video/mp4",
  progressive: "application/octet-stream",
  merge: "video/mp4",
};

/**
 * The `Referer` a media request should carry, given the page it was found on.
 *
 * The origin, not the full URL: it is what these CDNs check, and the full path of the
 * page someone was watching is more than the request needs to carry.
 */
function refererFor(pageUrl: string | undefined): [string, string][] {
  if (!pageUrl) return [];
  try {
    const { origin } = new URL(pageUrl);
    if (!origin.startsWith("http")) return [];
    return [["Referer", `${origin}/`]];
  } catch {
    return [];
  }
}

export class Queue {
  private readonly running = new Map<string, AbortController>();
  /** Set while `tick` is deciding, so two callers cannot start the same job. */
  private ticking = false;
  private tickAgain = false;
  /** Suppresses auto-start until the user resumes. */
  private paused = false;

  constructor(
    private readonly platform: Platform,
    private readonly events: QueueEvents,
  ) {}

  isRunning(jobId: string): boolean {
    return this.running.has(jobId);
  }

  get activeCount(): number {
    return this.running.size;
  }

  get isPaused(): boolean {
    return this.paused;
  }

  /**
   * Start a job from a user gesture.
   *
   * The sink is opened first and synchronously, because `showSaveFilePicker`
   * needs the transient activation from the click that got us here and that
   * activation does not survive the awaits `runJob` performs.
   */
  async startInteractive(jobId: string): Promise<void> {
    const job = await getJob(jobId);
    if (!job || this.running.has(jobId)) return;
    this.paused = false;
    let sink: Sink;
    try {
      sink = await createSinkInteractive(this.sinkRequest(job));
    } catch (e) {
      // The user cancelling a save dialog is not a failure; leave the job
      // queued so the button still says "Start".
      if ((e as { name?: string }).name === "AbortError") return;
      await updateJob(jobId, { status: "error", error: describe(e) });
      this.events.onChange();
      return;
    }
    void this.run(job, sink);
  }

  /** Stop a running job. Progress already persisted is kept. */
  pause(jobId: string): void {
    this.running.get(jobId)?.abort();
  }

  /** Stop everything and do not auto-start anything until `resumeAll`. */
  pauseAll(): void {
    this.paused = true;
    for (const controller of this.running.values()) controller.abort();
    this.events.onChange();
  }

  /** Put every paused job back in the queue and start what fits. */
  async resumeAll(): Promise<void> {
    this.paused = false;
    for (const job of await listJobs()) {
      if (job.status === "paused")
        await updateJob(job.id, { status: "queued" });
    }
    this.events.onChange();
    await this.tick();
  }

  /** Re-queue everything that failed. */
  async retryFailed(): Promise<void> {
    this.paused = false;
    for (const job of await listJobs()) {
      if (job.status === "error")
        await updateJob(job.id, { status: "queued", error: null });
    }
    this.events.onChange();
    await this.tick();
  }

  /**
   * Start queued jobs up to the concurrency limit.
   *
   * Called after every change: a job finishing, a setting changing, a folder
   * being granted. Doing nothing is the common case and must stay cheap.
   */
  async tick(): Promise<void> {
    if (this.ticking) {
      // A tick triggered while one is in flight would read a stale job list.
      // Remember to run again rather than racing.
      this.tickAgain = true;
      return;
    }
    this.ticking = true;
    try {
      do {
        this.tickAgain = false;
        await this.tickOnce();
      } while (this.tickAgain);
    } finally {
      this.ticking = false;
    }
  }

  private async tickOnce(): Promise<void> {
    if (this.paused) return;
    const settings = await getSettings();
    if (!settings.autoStart) return;
    if (this.running.size >= settings.maxConcurrentJobs) return;
    if (!(await canOpenSinkSilently())) return;

    const jobs = await listJobs();
    for (const job of jobs) {
      if (this.running.size >= settings.maxConcurrentJobs) break;
      if (job.status !== "queued" || this.running.has(job.id)) continue;
      const sink = await createSink(this.sinkRequest(job));
      void this.run(job, sink);
    }
  }

  private sinkRequest(job: Job) {
    return {
      jobId: job.id,
      filename: job.filename,
      mime: MIME_FOR_KIND[job.kind],
      // A merge has no resume point (see `runMerge`), so it always opens a fresh file
      // rather than continuing into one that holds half an older attempt.
      resuming:
        job.kind !== "merge" && job.receivedBytes > 0 && Boolean(job.stateJson),
      platform: this.platform,
    };
  }

  private async run(job: Job, sink: Sink): Promise<void> {
    const controller = new AbortController();
    this.running.set(job.id, controller);
    this.events.onChange();

    const options: RunOptions = {
      signal: controller.signal,
      onProgress: (p) => this.events.onProgress(job.id, p),
    };

    let releaseHeaders: (() => Promise<void>) | undefined;
    try {
      // Re-read rather than trusting the captured object: the options row may
      // have changed variant, audio-only or expected hash since it was listed.
      const fresh = (await getJob(job.id)) ?? job;

      // A site extractor may require headers `fetch` refuses to set. Install them for
      // the life of this job and take them down afterwards, so a rule never outlives
      // the download that needed it.
      const urls = fresh.mergeStreams
        ? fresh.mergeStreams.map((s) => s.url)
        : [fresh.url];

      // A candidate the sniffer found carries no headers of its own — it was observed,
      // not negotiated. What it does carry is the page it was seen on, and that is
      // precisely what several CDNs check: TikTok's, Douyin's and Meta's all answer 403
      // to a media request with no `Referer`, which is how a detected video downloads to
      // nothing. `fetch` cannot set that header; a `declarativeNetRequest` rule can, and
      // being able to is a large part of why the extension reaches sites the web app
      // cannot. An extractor's own headers still win — it knows more than we can infer.
      const headers: [string, string][] = fresh.requestHeaders?.length
        ? fresh.requestHeaders
        : refererFor(fresh.pageUrl);

      if (headers.length && this.platform.applyRequestHeaders) {
        releaseHeaders = await this.platform.applyRequestHeaders(urls, headers);
      }

      await runJob(fresh, sink, options);
    } catch (e) {
      // Pausing is a user decision, not a failure — the distinction matters
      // because an errored job shows a red message and a paused one does not.
      const paused =
        e instanceof PausedError ||
        (e as { name?: string }).name === "AbortError";
      // The browser refusing this page a cross-origin read arrives as a bare
      // "Failed to fetch", which says nothing a visitor can act on. The extension is not
      // subject to it, so the row offers that route. Not terminal: a CDN can start
      // answering later — Twitch's does, as its cached copy of a file is refreshed.
      const refusedThisPage =
        !paused && looksLikeCorsFailure(e) && crossOriginFromPage(job.url);
      await updateJob(job.id, {
        status: paused ? "paused" : "error",
        error: paused
          ? null
          : refusedThisPage
            ? `${new URL(job.url).hostname} did not let this page read the file, or the ` +
              "connection dropped before it answered. The browser extension is not subject " +
              "to the first."
            : describe(e),
        // A host that served the start and refused the rest will refuse it again at the
        // same offset, so this job has no resume to offer. Recorded here rather than
        // re-derived from the message in the UI.
        terminal:
          !paused &&
          ["HostRefusedRemainder", "NeedsForbiddenHeader"].includes(
            (e as { name?: string }).name ?? "",
          ),
        // Retrying here cannot help either, but unlike the refusal above there *is*
        // something the user can do, so the row says what.
        needsExtension:
          refusedThisPage ||
          (!paused && (e as { name?: string }).name === "NeedsForbiddenHeader"),
      });
    } finally {
      // Taken down even if the download threw: a stale header rule would apply to
      // every later request to that host, including ones this extension did not make.
      await releaseHeaders?.().catch(() => undefined);
      this.running.delete(job.id);
      this.events.onChange();
      // A finished job frees a slot, so the next one can start immediately.
      await this.tick();
    }
  }
}

function describe(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
