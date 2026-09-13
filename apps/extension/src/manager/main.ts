// The manager tab.
//
// It is a thin host: the queue, the job list and the local tools all live in
// `@opendownloader/ui` so the standalone web app runs exactly the same code.
// What belongs here is only what is true of an extension page — the platform
// that saves through the downloads API, and the test hook.
//
// Downloads run in this tab rather than the service worker because an MV3
// worker is terminated after ~30s idle, after 5 minutes on a request, and if a
// `fetch()` response takes over 30s — all of which a real download violates
// routinely. A normal document has none of those limits.

import {
  audioRenditionFor,
  configureEngine,
  deleteJob,
  enqueueCandidate,
  putJob,
  resolveVimeoManifest,
  vimeoStreams,
  extract,
  extractMp4Audio,
  fetchSubtitleRendition,
  isSupportedSite,
  listJobs,
  listPlaylistOptions,
  loadCore,
  remuxLocalSegments,
  updateSettings,
} from "@opendownloader/engine";
import { Manager, candidateForUrl, mountTools } from "@opendownloader/ui";

import { extensionPlatform, onJobsChanged } from "../platform/webext";

// Test-only engine configuration, applied before anything can start a download.
//
// The chunk size is shrunk drastically so a small fixture still produces many
// chunks — the multi-chunk resume path is the one worth testing, and an 8 MiB
// chunk would never reach it against a 512 KiB file. The blob sink is forced
// because automation cannot answer a native save dialog.
// The extension puts forbidden headers on the wire with `declarativeNetRequest`, so a
// 403 here is the host's answer to a complete request — never a header we failed to send.
configureEngine({ canSendForbiddenHeaders: true });

if (__OPENDOWNLOADER_E2E__) {
  configureEngine({ chunkSize: 64 * 1024, forceBlobSink: true });
}

const manager = new Manager({
  root: document.getElementById("manager") as HTMLElement,
  platform: extensionPlatform,
  showUrlInput: true,
  addLink: addPastedLink,
  addTorrentFile,
  notice:
    "Downloads run in this tab. Closing it pauses them — progress is saved, and " +
    "reopening this page resumes from where it stopped.",
});

mountTools({
  root: document.getElementById("tools") as HTMLElement,
  platform: extensionPlatform,
});

// Test-only hook, installed before the first render so it exists from first
// paint. Seeding a job normally happens in the popup, which automation cannot
// open — Chrome's toolbar popup is not a tab and there is no element to click.
// This calls the same `enqueueCandidate` the popup calls, so the suite can then
// drive the *real* Start button and exercise the real engine rather than a
// stand-in for it.
if (__OPENDOWNLOADER_E2E__) {
  (globalThis as unknown as { __test: unknown }).__test = {
    enqueue: async (
      candidate: Parameters<typeof enqueueCandidate>[0],
      opts?: Parameters<typeof enqueueCandidate>[1],
    ) => {
      const job = await enqueueCandidate(candidate, opts);
      await manager.start();
      return job;
    },
    listJobs,
    manager,
    updateSettings,
    // The media pipelines, reachable without the file picker automation cannot
    // drive. Everything after the picker is the code under test.
    listPlaylistOptions: async (url: string) =>
      listPlaylistOptions(url, await loadCore()),
    audioRenditionFor,
    fetchSubtitleRendition,
    extractMp4Audio,
    remuxLocalSegments,
    // Site extraction, driven with a page supplied by the test rather than read from a
    // tab — automation has no second tab to read, and the parsing is the part under test.
    extract,
    // What the popup hands `extract` alongside the page: the host's way of putting
    // `Referer` and `User-Agent` on the wire. Without it a site API call from here is a
    // CORS failure that says nothing about the extractor.
    platform: extensionPlatform,
    isSupportedSite,
    // Vimeo's JSON adaptive path, which the popup drives from a toolbar click that
    // automation cannot produce reliably — the button needs a focused window.
    resolveVimeoManifest,
    vimeoStreams,
    putJob,
    reset: async () => {
      for (const j of await listJobs()) await deleteJob(j.id);
      await manager.start();
    },
  };
}

void manager.start();

// Learn about jobs queued somewhere else — the popup, almost always.
//
// Two signals, because neither alone is enough. The announcement covers the case the
// popup creates: it queues, then focuses this tab, which until now went on showing the
// list it had read when it loaded. Becoming visible again is the backstop for anything
// that queued without announcing, and it costs one read of an IndexedDB table.
onJobsChanged(() => void manager.sync());
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") void manager.sync();
});

// A download in progress must survive an accidental tab close no worse than a
// pause: the state is already persisted, so warn and let the user decide.
window.addEventListener("beforeunload", (e) => {
  if (manager.hasRunningJobs()) {
    e.preventDefault();
    e.returnValue = "";
  }
});

/**
 * Where a torrent bridge might be listening on this machine.
 *
 * Two shapes, because there are two ways to have one. The app serves everything on one
 * port and mounts the bridge under a path — and it walks up from 5180 when that port is
 * taken, so a few are worth trying. The standalone `dl-torrent` sits on 8089.
 *
 * An extension page may talk to loopback, which is what makes any of this possible here:
 * a page on https may not, whatever is running.
 */
const BRIDGE_CANDIDATES = [
  "http://127.0.0.1:8089",
  // The same span the app itself walks when its preferred port is taken. Five was not
  // enough: with two stale servers holding 5180 and 5181 the app landed on 5182, and one
  // more collision would have put it past the end of this list. A probe range narrower
  // than where the app can be is a bridge that is running and not found.
  ...Array.from(
    { length: 12 },
    (_, i) => `http://127.0.0.1:${5180 + i}/torrent-bridge`,
  ),
];

/**
 * Ask the browser to start the bridge, and get back the port it is on.
 *
 * This is the arrangement with nothing to run: the app registers itself as a native
 * messaging host when it is first opened, and from then on the *browser* starts the
 * bridge on demand and stops it when this page lets go. The app itself need not be
 * running, and nothing is typed anywhere.
 *
 * Returns null when no host is registered — the app has never been opened, or is not
 * installed — so the caller falls back to looking for one already listening.
 */
async function startBridgeViaBrowser(): Promise<string | null> {
  if (!chrome.runtime?.connectNative) {
    nativeFailure =
      "this browser did not grant permission to start local programs";
    return null;
  }
  return new Promise((resolve) => {
    let port: chrome.runtime.Port;
    try {
      port = chrome.runtime.connectNative("app.opendownloader.bridge");
    } catch (e) {
      nativeFailure = e instanceof Error ? e.message : String(e);
      return resolve(null);
    }
    // Held open deliberately: the host lives as long as this port does, so dropping it
    // would stop the bridge in the middle of the download it was started for.
    nativePort = port;

    const settle = (value: string | null) => {
      if (value === null && nativePort === port) nativePort = null;
      resolve(value);
    };
    port.onMessage.addListener(
      (message: { ok?: boolean; url?: string; error?: string }) => {
        // A full URL rather than a port: the host may hand back a bridge that is already
        // running, and those are not all mounted at the same path.
        if (message?.ok && message.url) return settle(message.url);
        nativeFailure =
          message?.error ?? "the helper replied with nothing usable";
        settle(null);
      },
    );
    port.onDisconnect.addListener(() => {
      // The reason matters and used to be thrown away. "Specified native messaging host
      // not found" means the app has never been opened; "Access to the specified native
      // messaging host is forbidden" means it was opened before this extension existed
      // and does not know its id yet. Those need different answers, and neither is
      // "install it" — which is what the reader was told for both.
      nativeFailure =
        chrome.runtime.lastError?.message ??
        "the helper stopped without saying why";
      settle(null);
    });
    port.postMessage({ type: "start" });
    // A host that is registered but wedged would otherwise hang this forever.
    setTimeout(() => {
      nativeFailure = "the helper did not answer";
      settle(null);
    }, 8000);
  });
}

/** Why the browser could not start the helper, for the message when nothing works. */
let nativeFailure: string | null = null;

/** Kept for the life of the page, because the bridge stops when this closes. */
let nativePort: chrome.runtime.Port | null = null;

/** The first bridge that answers, or null. Probed together so this costs one wait. */
async function findBridge(): Promise<string | null> {
  const probes = BRIDGE_CANDIDATES.map(async (base) => {
    const response = await fetch(`${base}/healthz`, {
      signal: AbortSignal.timeout(1200),
    });
    const health = (await response.json()) as { service?: string };
    if (health.service !== "dl-torrent") throw new Error("not the bridge");
    return base;
  });
  // `any` rather than `all`: the first that answers wins and the rest are irrelevant.
  return Promise.any(probes).catch(() => null);
}

/**
 * Handle a link pasted into the manager's box.
 *
 * It used to fetch whatever it was given as a file, so a magnet was refused here while
 * the very same magnet worked in the web app — two boxes that look alike and behave
 * differently, and this is the one that sits beside the downloads.
 */
async function addPastedLink(url: string): Promise<void> {
  const isPeerLink = /^magnet:/i.test(url) || /\.torrent(\?|$)/i.test(url);
  if (!isPeerLink) {
    await manager.enqueue(await candidateForUrl(url), { start: true });
    return;
  }
  await queueTorrent(await bridgeForTorrent(), url);
}

/**
 * Find the bridge, saying so while it happens.
 *
 * Starting the app through the browser is the first thing tried and the slowest part of
 * the whole wait — the native host is launched, binds a port, and answers — so the
 * manager's own bar is told what it is waiting for rather than sweeping over nothing.
 */
async function bridgeForTorrent(): Promise<string> {
  // The browser-started host first: it needs nothing to be running. Falling back to a
  // bridge already listening covers the app being open, or the standalone binary.
  const bridge = (await startBridgeViaBrowser()) ?? (await findBridge());
  if (!bridge) throw new Error(bridgeMissingMessage());
  manager.say("Asking the swarm what is in this torrent…");
  return bridge;
}

/**
 * Open a `.torrent` held on disk.
 *
 * The bridge has always accepted the bytes of one — it tells a link from a file by the
 * first byte, since bencode begins with `d` and a link never does. What was missing was
 * any way to hand it a file, which is how most torrents actually arrive.
 */
async function addTorrentFile(file: File): Promise<void> {
  const bytes = await file.arrayBuffer();
  await queueTorrent(await bridgeForTorrent(), bytes);
}

/** Give the bridge a magnet or a torrent's bytes, and queue everything inside it. */
async function queueTorrent(
  bridge: string,
  body: string | ArrayBuffer,
): Promise<void> {
  const response = await fetch(`${bridge}/torrent`, { method: "POST", body });
  if (!response.ok) {
    const detail = (await response.json().catch(() => null)) as {
      error?: string;
    } | null;
    throw new Error(detail?.error ?? `the bridge answered ${response.status}`);
  }
  const torrent = (await response.json()) as {
    name: string;
    files: { index: number; name: string; length: number; url: string }[];
  };
  if (torrent.files.length === 0) {
    throw new Error(`${torrent.name} contains no files.`);
  }

  // Every file, largest first. A torrent is a thing someone asked for whole, and picking
  // one of them here would be guessing — the queue shows them all and each can be removed.
  for (const file of [...torrent.files].sort((a, b) => b.length - a.length)) {
    await manager.enqueue(
      {
        url: `${bridge}${file.url}`,
        kind: "progressive",
        // A torrent path can be `Season 1/ep01.mkv`; only the last segment is a name.
        filename: file.name.split("/").pop() ?? file.name,
        mime: null,
        size: file.length,
      },
      { start: true },
    );
  }
}

/** Why there is no bridge, in words that fit the reason the browser gave. */
function bridgeMissingMessage(): string {
  const forbidden = /forbidden/i.test(nativeFailure ?? "");
  const missing = /not found|no such native/i.test(nativeFailure ?? "");
  return forbidden
    ? "The OpenDownloader app is installed but does not yet know this extension. " +
        "Open the app once more — it looks up which extensions are installed each " +
        "time it starts — then try again."
    : missing
      ? "A torrent needs the OpenDownloader app, which is not installed yet, or has " +
        "never been opened. Install it and open it once; after that this page starts " +
        "it by itself and it does not have to be running."
      : "A torrent needs the OpenDownloader app, and the browser could not start " +
        `it: ${nativeFailure ?? "no reason given"}.`;
}
