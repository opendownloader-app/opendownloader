// The standalone web app.
//
// Same engine, same manager, same local tools as the extension — the difference
// is what a web page is allowed to do. It cannot watch what another site loads,
// so there is no detection here and the input is a link you paste. And its
// `fetch` is subject to CORS, which the extension's host permissions exempt it
// from; when a server refuses, that is explained and the relay is offered rather
// than the failure being reported as a generic network error.

import {
  audioOnly,
  configureEngine,
  extract,
  extractMp4Audio,
  fetchWithRetry,
  formatSize,
  getSettings,
  hasAudioChoice,
  isSupportedSite,
  jobIdFor,
  jobKindForStream,
  looksLikeCorsFailure,
  pair,
  pairingProblem,
  isMegaLink,
  isQuarkShare,
  listQuarkFolder,
  parseEd2kLink,
  QuarkNeedsAccount,
  readQuarkShare,
  resolveMegaFile,
  resolveQuarkDownload,
  type QuarkEntry,
  type QuarkShare,
  putJob,
  remuxLocalSegments,
  resolveDownloadLink,
  siteAcceptsFetchedPage,
  siteWorksWithoutATab,
  supportedSites,
  supportedSources,
  siteFor,
  updateSettings,
  webPlatform,
  type AudioChoice,
  type Extraction,
  type MediaOption,
  type VideoChoice,
} from "@opendownloader/engine";
import {
  Busy,
  Manager,
  TORRENT_STAGES,
  candidateForUrl,
  mountTools,
} from "@opendownloader/ui";

import { reflectAccountState } from "./account-badge";
import { mountTranscribePanel } from "./transcribe-panel";
import { initPageLocale, languagePicker, onLocaleChange, t } from "@opendownloader/ui";

// Test-only engine configuration, applied before anything can start a download.
// Automation can click a button but cannot answer a native save dialog, so the
// blob sink — which needs no dialog — is forced for the smoke test.
if (__OPENDOWNLOADER_E2E__) {
  configureEngine({ forceBlobSink: true });
}

const urlInput = document.getElementById("url") as HTMLInputElement;
const goButton = document.getElementById("go") as HTMLButtonElement;
const statusEl = document.getElementById("url-status") as HTMLParagraphElement;
// The bar above the status line, and the schedule of what to say while it sweeps. The
// paragraph is passed in rather than created, because everything on this page already
// writes its result there.
/**
 * The extension's own id, fixed by the `key` in its manifest.
 *
 * Stable across every install, which is what makes the ping below possible at all — an
 * unpacked extension without that key takes an id derived from its load path.
 */
const EXTENSION_ID = "cbeecjhjblfbdjncacbfmlcelgkmklda";

/**
 * Is the extension installed in this browser?
 *
 * There is no API for "is extension X installed", and the usual dodge — fetching a
 * web-accessible resource and seeing whether it loads — answers the question for every
 * site that tries it. The supported route is the other way round: the extension names
 * this origin in `externally_connectable`, which makes `chrome.runtime` appear on this
 * page and nowhere else, and answers a ping. A browser without it gives no object at
 * all, so absence is indistinguishable from not-Chromium — which is fine, because both
 * lead to the same advice.
 *
 * Resolves false rather than rejecting: this runs on a failure path, and a detection
 * that throws would replace a useful message with a worse one.
 */
async function extensionInstalled(): Promise<boolean> {
  const runtime = (globalThis as { chrome?: { runtime?: Record<string, unknown> } })
    .chrome?.runtime as
    | {
        sendMessage?: (
          id: string,
          m: unknown,
          cb: (r: { ok?: boolean } | undefined) => void,
        ) => void;
        lastError?: unknown;
      }
    | undefined;
  if (!runtime?.sendMessage) return false;
  return new Promise((resolve) => {
    // Firefox and an uninstalled extension both simply never call back.
    const giveUp = setTimeout(() => resolve(false), 600);
    try {
      runtime.sendMessage!(EXTENSION_ID, { type: "ping" }, (reply) => {
        clearTimeout(giveUp);
        // Reading `lastError` is what stops Chrome logging "Unchecked runtime.lastError"
        // to the console when nothing answered. It is a read for its side effect.
        void runtime.lastError;
        resolve(Boolean(reply?.ok));
      });
    } catch {
      clearTimeout(giveUp);
      resolve(false);
    }
  });
}

/**
 * Say why this page cannot do it, and offer the thing that can.
 *
 * Two different failures land here — a site that only its own pages may read, and one
 * that refuses this origin outright — and both have the same answer, so both get the
 * same panel rather than one useful message and one shrug. Which half of the panel is
 * shown depends on whether the extension is actually there, because "install it" and
 * "you already have it, open the video" are different instructions and guessing wrong
 * wastes the reader's time either way.
 */
async function offerTheExtension(message: string, pageUrl: string): Promise<void> {
  statusEl.className = "status-error";
  statusEl.replaceChildren(document.createTextNode(message + " "));

  const installed = await extensionInstalled();
  const action = document.createElement("a");
  action.rel = "noopener";
  if (installed) {
    // Opening the video is the whole of what this page can usefully do: the extension
    // reads the page you are on, so it has to be on that page. It cannot be opened from
    // here — a toolbar popup has no API a website may call.
    action.href = pageUrl;
    action.target = "_blank";
    action.textContent = "Open the video and click the OpenDownloader icon";
    statusEl.append(
      document.createTextNode(
        "You already have the extension, and it is not subject to any of this. ",
      ),
      action,
    );
  } else {
    action.href = RELEASES_URL;
    action.target = "_blank";
    action.textContent = "Get the extension";
    statusEl.append(action);
  }
}

/** Where every build is published. The only address the app points people at. */
const RELEASES_URL =
  "https://github.com/opendownloader-app/opendownloader/releases";

const openTorrentButton = document.getElementById(
  "open-torrent",
) as HTMLButtonElement | null;
const busy = new Busy(statusEl);

/**
 * Hold both buttons for the duration of either wait.
 *
 * Not just the one that was pressed. A torrent takes seconds to resolve, and the two
 * paths share this one bar and this one status line — so a `.torrent` opened while a
 * magnet is still resolving gives two waits writing over each other, and whichever
 * finishes first takes the bar down while the other is still going. Holding both is
 * also what stops a second press queueing the same files twice, which is what the old
 * silent wait invited.
 */
let pending = false;
function working(on: boolean): void {
  pending = on;
  goButton.disabled = on;
  if (openTorrentButton) openTorrentButton.disabled = on;
}
busy.root.style.marginTop = "10px";
statusEl.before(busy.root);
const siteOptionsEl = document.getElementById("site-options") as HTMLDivElement;

/**
 * Route fetches through the user's relay when they have configured one.
 *
 * Installed as a URL rewrite rather than a `fetch` wrapper so range requests,
 * `If-Range`, retries and aborts all behave identically whether a relay is in
 * play or not — the relay forwards those headers verbatim.
 */
let relay = { url: "", enabled: false };

/** The first relay-adoption probe, so callers can wait for it to settle before deciding. */
let relayReady: Promise<void> | null = null;

configureEngine({
  rewriteUrl: (url) => {
    if (!relay.enabled || !relay.url) return url;
    // Already relayed: rewriting twice would nest the query parameter.
    if (url.startsWith(relay.url)) return url;
    // Never relay something already on this machine. The relay refuses a private
    // address on purpose — it exists to reach sites that will not answer a page, and
    // proxying to loopback would let any page it serves reach services on the machine
    // running it. But the torrent bridge *is* on loopback, so relaying its URLs turned
    // every torrent download into "server returned 403 Forbidden" from our own relay.
    if (isLocalAddress(url)) return url;
    return `${relay.url.replace(/\/+$/, "")}/fetch?url=${encodeURIComponent(url)}`;
  },
});

/** Whether a URL points at this machine, and so must never go through the relay. */
function isLocalAddress(url: string): boolean {
  try {
    const host = new URL(url, location.href).hostname;
    return (
      host === "127.0.0.1" ||
      host === "::1" ||
      host === "localhost" ||
      host.endsWith(".localhost")
    );
  } catch {
    // A relative URL is same-origin, and the page itself is what serves the bridge.
    return true;
  }
}

/**
 * What a desktop browser calls itself.
 *
 * Sent, through the relay, on the page fetch below. Several of these sites answer a
 * request with no recognisable `User-Agent` with a stub page, and a stub page is
 * indistinguishable from a site that has changed shape.
 */
const DESKTOP_UA =
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 " +
  "(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/**
 * Fetch a watch page for an extractor that asked to read one.
 *
 * The extension answers `PageState` by reading the tab the user is looking at. This page
 * has no such tab, so it fetches the same URL instead — a thinner document, since none of
 * the page's own scripts have run against it. Only extractors that have said they can
 * continue from that copy are ever given this.
 *
 * `Referer` and `User-Agent` are forbidden to a page, so they travel under the relay's
 * `x-relay-*` names and are put back on the wire by the relay. With no relay configured
 * they simply do not arrive, and the site answers whatever it answers strangers — which
 * is the case the extractor has already promised to cope with.
 */
async function fetchPageState(url: string): Promise<string> {
  const response = await fetchWithRetry(url, {
    headers: {
      "x-relay-referer": `${new URL(url).origin}/`,
      "x-relay-user-agent": DESKTOP_UA,
      // Both are ordinary headers a page may set, and both matter: a watch page asked
      // for with `Accept: */*` and no language is not a shape any browser produces, and
      // the sites that screen for automation answer it with a refusal rather than a page.
      Accept:
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
      "Accept-Language": "zh-CN,zh;q=0.9,en-US;q=0.8,en;q=0.7",
    },
  });
  if (!response.ok) {
    throw new Error(
      `${new URL(url).hostname} answered ${response.status} for that page.`,
    );
  }
  return response.text();
}

/**
 * The BitTorrent bridge, if one is running on this machine.
 *
 * A `magnet:` link cannot be downloaded by a tab: joining a swarm means opening TCP and
 * uTP connections to peers, which a page has no way to do. The bridge is a local process
 * that does have those sockets, and it offers each file in a torrent over HTTP with byte
 * ranges — at which point it is an ordinary download, and everything below this line
 * (chunks, resume, the SHA-256 read back off disk) works against it unchanged.
 *
 * Discovered the same way as the relay: loopback only, only when it answers its own
 * health check, never guessed at anywhere else.
 */
const LOCAL_TORRENT_BRIDGE = "http://127.0.0.1:8089";

/**
 * Whether this page can reach a helper on loopback at all.
 *
 * It cannot when the page itself is served over https: the browser refuses an `http://`
 * subresource from an `https://` page, and answering Chrome's private-network preflight
 * does not lift it — measured, not assumed. So on the deployed site the relay and the
 * torrent bridge are unreachable no matter what the visitor runs, and telling them to
 * "start it and reload" is advice that cannot work. Served from loopback itself — which
 * is what `npm start` does — both are reachable.
 */
const CAN_REACH_LOOPBACK =
  location.protocol !== "https:" ||
  location.hostname === "127.0.0.1" ||
  location.hostname === "localhost";
let torrentBridge: string | null = null;

/**
 * Where the helpers might be, best first.
 *
 * Same origin comes first because that is the desktop build: one program serving the
 * app, the relay and the bridge on one port. It is the only arrangement with nothing to
 * start by hand, and the only one a page served over https could ever reach — mixed
 * content forbids a secure page from talking to `http://127.0.0.1`, whatever is
 * listening there.
 *
 * The separate ports stay as the second guess, for the repository checkout where the
 * three run as three programs.
 */
function candidateBases(sameOriginPath: string, loopback: string): [string, string] {
  return [`${location.origin}${sameOriginPath}`, loopback];
}

async function adoptTorrentBridge(): Promise<void> {
  for (const base of candidateBases("/torrent-bridge", LOCAL_TORRENT_BRIDGE)) {
    try {
      const probe = await fetch(`${base}/healthz`, {
        signal: AbortSignal.timeout(1200),
      });
      if (!probe.ok) continue;
      const health = (await probe.json()) as { service?: string };
      if (health.service !== "dl-torrent") continue;
      torrentBridge = base;
      return;
    } catch {
      // Nothing there. The common case for one of the two, and often for both.
    }
  }
}

/**
 * Hand a magnet link or `.torrent` to the bridge and offer its files.
 *
 * Returns false when this is not a peer-to-peer link at all, so the caller carries on
 * with the ordinary path.
 */
async function addFromTorrent(link: string): Promise<boolean> {
  const isTorrentFile = /\.torrent(\?|$)/i.test(link);
  if (!/^magnet:/i.test(link) && !/^ed2k:/i.test(link) && !isTorrentFile)
    return false;

  if (/^ed2k:/i.test(link)) return addFromEd2k(link);
  if (!torrentBridge) {
    throw new Error(
      CAN_REACH_LOOPBACK
        ? "This is a BitTorrent link, and a browser tab cannot join a swarm — that needs " +
            "TCP connections to other people's machines, which no page, permission or " +
            "relay can open. OpenDownloader ships a local bridge that does it for you: " +
            "install the OpenDownloader app, leave it running, and reload this page — the " +
            "files inside the torrent will be listed here like any other download."
        : "This is a BitTorrent link, and a browser tab cannot join a swarm. The bridge " +
            "that can is the OpenDownloader app, and this page cannot reach it: a page served " +
            "over https is not allowed to talk to a service on your own machine, whatever " +
            "is running there. Open the app itself instead — it has this same page inside " +
            "it, with torrents working.",
    );
  }

  busy.start("Asking the swarm what is in this torrent…", TORRENT_STAGES);
  const response = await fetch(`${torrentBridge}/torrent`, {
    method: "POST",
    // A magnet goes as text; a `.torrent` URL is fetched by the bridge itself, which can
    // reach hosts this page cannot.
    body: link,
  });
  if (!response.ok) {
    const detail = (await response.json().catch(() => null)) as {
      error?: string;
    } | null;
    throw new Error(detail?.error ?? `the bridge answered ${response.status}`);
  }
  const torrent = (await response.json()) as {
    id: number;
    name: string;
    files: { index: number; name: string; length: number; url: string }[];
  };
  renderTorrentFiles(torrent);
  busy.done();
  return true;
}

/**
 * A site whose page only a browser extension can read.
 *
 * Its own type rather than a plain `Error` so the handler can render the extension links
 * as links. A message that says "install the extension" and gives no way to is a message
 * that has stopped one step short.
 */
class SiteNeedsExtension extends Error {
  constructor(readonly site: string) {
    super(
      `${site} has to be read from its own page. A web page is not allowed to read ` +
        `another site's page, and ${site} tells a server that asks nothing useful — so ` +
        `this one genuinely cannot. The extension reads the page you are already on, ` +
        `and it is free.`,
    );
  }
}

/**
 * Act on an `ed2k://` link.
 *
 * The hash in the link is checkable — the engine computes it while reading the finished
 * file back off disk — but the eDonkey network itself is out of reach: joining it needs a
 * client this does not ship, and unlike BitTorrent there is no bridge for it. So a link
 * that carries an `|s=http://…|` source downloads and verifies like anything else, and a
 * link that does not says exactly what it is missing rather than failing vaguely.
 */
async function addFromEd2k(link: string): Promise<boolean> {
  const parsed = await parseEd2kLink(link);
  if (!parsed) throw new Error("that ed2k link could not be read.");

  if (parsed.kind === "server" || parsed.kind === "serverlist") {
    throw new Error(
      "That is an eDonkey server address, not a file — it tells a client where to " +
        "connect, and there is nothing at it to download.",
    );
  }

  if (!parsed.httpSource) {
    throw new Error(
      `This link names a file (${parsed.filename}, ${formatSize(parsed.size)}) by its ` +
        "eD2k hash, and nothing else — the bytes exist only on the eDonkey network, " +
        "which needs a client such as eMule or aMule. Some ed2k links also carry an " +
        "ordinary web address, and those download here. If you fetch this file " +
        "elsewhere, paste its web address and it will be checked against the hash in " +
        "this link.",
    );
  }

  statusEl.textContent = `Found a web source for ${parsed.filename} — checking it…`;
  await manager.enqueue(
    {
      url: parsed.httpSource,
      kind: "progressive",
      // The link's own name, not one guessed from the URL: an ed2k source URL is
      // routinely a numeric id on a mirror.
      filename: parsed.filename,
      mime: null,
      size: parsed.size,
    },
    { start: true, expectedEd2k: parsed.hash },
  );
  statusEl.textContent = `Queued ${parsed.filename}. It will be checked against the eD2k hash in the link.`;
  return true;
}

/**
 * Act on a Mega link.
 *
 * Mega is the one storage service that works from a plain page, and the reason is worth
 * stating: its API answers `Access-Control-Allow-Origin: *`, and the decryption key lives
 * in the URL fragment, which a browser never sends anywhere. So the key reaches Mega
 * neither through us nor at all, and the file is decrypted on its way to disk.
 *
 * Returns false when this is not a Mega link, so the caller carries on.
 */
async function addFromMega(url: string): Promise<boolean> {
  if (!(await isMegaLink(url))) return false;

  statusEl.textContent = "Asking Mega about that file\u2026";
  const file = await resolveMegaFile(url);

  await manager.enqueue(
    {
      url: file.url,
      kind: "progressive",
      // The name came out of the encrypted attribute blob, not the URL \u2014 a Mega
      // download URL is an opaque host and id and would name every file the same.
      filename: file.filename,
      mime: null,
      size: file.size,
    },
    { start: true, decrypt: { key: file.key, nonce: file.nonce } },
  );
  statusEl.textContent = `Queued ${file.filename}.`;
  return true;
}

/**
 * Open a Quark share and show what is in it.
 *
 * Returns false when this is not a Quark link, so the caller carries on.
 *
 * Quark is the one supported source where reading the share and fetching a file are
 * separate permissions. Anyone may read: the listing below is the same tree the share
 * page shows a visitor. Fetching is Quark's own gate, and it is not opened here \u2014 see
 * `queueQuarkFile`.
 */
async function addFromQuark(url: string): Promise<boolean> {
  if (!(await isQuarkShare(url))) return false;

  if (!relay.enabled || !relay.url) {
    throw new Error(
      CAN_REACH_LOOPBACK
        ? "Quark's API answers only its own site, so a page cannot call it and this " +
            "needs the relay the OpenDownloader app runs. Install it, leave it running, and " +
            "reload — the share will be listed here."
        : "Quark's API answers only its own site, so reading a share needs the relay \u2014 " +
            "a program you run on your own machine. This page is served over https and " +
            "is not allowed to talk to a plain-http service on your computer. Run the " +
            "OpenDownloader app instead — it has this same page inside it, with the relay " +
            "already running.",
    );
  }

  statusEl.textContent = "Opening that Quark share\u2026";
  const share = await readQuarkShare(url);
  renderQuarkEntries(share, share.title, share.entries, []);
  statusEl.textContent = "";
  return true;
}

/**
 * Show one directory of a share, with a way back up.
 *
 * `trail` is the folders opened to get here, so the caller can walk back out without a
 * second round trip \u2014 the entries are already in hand.
 */
function renderQuarkEntries(
  share: QuarkShare,
  title: string,
  entries: QuarkEntry[],
  trail: { name: string; entries: QuarkEntry[] }[],
): void {
  siteOptionsEl.replaceChildren();
  siteOptionsEl.hidden = false;

  const heading = document.createElement("p");
  heading.className = "muted";
  heading.textContent = trail.length
    ? `${share.title} \u2014 ${[...trail.map((t) => t.name), title].slice(1).join(" / ")}`
    : `${share.title} \u2014 ${entries.length} item${entries.length === 1 ? "" : "s"}`;
  siteOptionsEl.append(heading);

  if (trail.length) {
    const up = trail[trail.length - 1]!;
    siteOptionsEl.append(
      choiceRow("\u2191 Up a level", up.name, () => {
        renderQuarkEntries(share, up.name, up.entries, trail.slice(0, -1));
      }),
    );
  }

  // Folders first, then files: a share is browsed, and a directory listing that
  // interleaves the two is harder to scan than one that does not.
  const sorted = [...entries].sort((a, b) =>
    a.isDir === b.isDir ? b.size - a.size : a.isDir ? -1 : 1,
  );
  for (const entry of sorted) {
    siteOptionsEl.append(
      entry.isDir
        ? choiceRow(`\u{1F4C1} ${entry.name}`, "folder", () => {
            void openQuarkFolder(share, entry, entries, title, trail);
          })
        : choiceRow(entry.name, formatSize(entry.size), () => {
            void queueQuarkFile(share, entry);
          }),
    );
  }

  const note = document.createElement("p");
  note.className = "muted hint";
  note.textContent =
    "Quark lets anyone read a share, but hands over files only to an account it " +
    "recognises \u2014 and it cannot see your Quark sign-in from this page. Use the " +
    "browser extension on the share's own page for the download itself.";
  siteOptionsEl.append(note);
}

async function openQuarkFolder(
  share: QuarkShare,
  folder: QuarkEntry,
  siblings: QuarkEntry[],
  title: string,
  trail: { name: string; entries: QuarkEntry[] }[],
): Promise<void> {
  statusEl.className = "muted";
  statusEl.textContent = `Opening ${folder.name}\u2026`;
  try {
    const entries = await listQuarkFolder(share, folder.fid);
    renderQuarkEntries(share, folder.name, entries, [
      ...trail,
      { name: title, entries: siblings },
    ]);
    statusEl.textContent = "";
  } catch (e) {
    statusEl.className = "status-error";
    statusEl.textContent = e instanceof Error ? e.message : String(e);
  }
}

/**
 * Try to fetch one file from a share.
 *
 * The request is made rather than pre-judged. Quark decides whether a visitor may have a
 * given file, and the only measurement behind this is one share, where it refused every
 * file from 155 MB to 61 GB \u2014 not enough to tell a user their share will refuse too.
 * So it asks, and turns a refusal into a sentence that says what would lift it.
 */
async function queueQuarkFile(
  share: QuarkShare,
  entry: QuarkEntry,
): Promise<void> {
  statusEl.className = "muted";
  statusEl.textContent = `Asking Quark for ${entry.name}\u2026`;
  try {
    const url = await resolveQuarkDownload(share, entry);
    await manager.enqueue(
      {
        url,
        kind: "progressive",
        filename: entry.name,
        mime: null,
        size: entry.size,
      },
      { start: true },
    );
    siteOptionsEl.hidden = true;
    statusEl.textContent = `Queued ${entry.name}.`;
  } catch (e) {
    statusEl.className = "status-error";
    statusEl.replaceChildren(
      document.createTextNode(
        (e instanceof Error ? e.message : String(e)) + " ",
      ),
    );
    if (e instanceof QuarkNeedsAccount) {
      const a = document.createElement("a");
      a.href = `https://pan.quark.cn/s/${share.pwdId}`;
      a.target = "_blank";
      a.rel = "noreferrer";
      a.textContent = "Open the share on Quark";
      statusEl.append(a);
    }
  }
}

/**
 * List a torrent's files, each one a download.
 *
 * Deliberately the same shape as a supported site's option list: by this point a torrent
 * is not a special kind of thing, it is a set of URLs that answer range requests.
 */
function renderTorrentFiles(torrent: {
  id: number;
  name: string;
  files: { index: number; name: string; length: number; url: string }[];
}): void {
  siteOptionsEl.replaceChildren();
  siteOptionsEl.hidden = false;

  const heading = document.createElement("p");
  heading.className = "muted";
  heading.textContent = `${torrent.name} — ${torrent.files.length} file${
    torrent.files.length === 1 ? "" : "s"
  }`;
  siteOptionsEl.append(heading);

  // Biggest first: in a season pack or a release folder, the thing someone came for is
  // almost always the largest file, and the rest are samples, artwork and notes.
  const files = [...torrent.files].sort((a, b) => b.length - a.length);
  for (const file of files) {
    siteOptionsEl.append(
      choiceRow(file.name, formatSize(file.length), () => {
        void queueTorrentFile(torrent, file);
      }),
    );
  }

  const note = document.createElement("p");
  note.className = "muted hint";
  note.textContent =
    "Pieces are fetched as they are needed, so downloading one file does not fetch " +
    "the rest of the torrent. Nothing is uploaded back to the swarm.";
  siteOptionsEl.append(note);
}

async function queueTorrentFile(
  torrent: { id: number; name: string },
  file: { index: number; name: string; length: number; url: string },
): Promise<void> {
  // Built directly rather than through `candidateForUrl`, which exists to work out what
  // an unknown URL is by probing it. Nothing here is unknown: the bridge has already
  // said the name, the exact length and the file's place in the torrent, and the URL it
  // serves from ends in `/file/0` — a path with no extension, which is precisely the
  // shape that probing gives up on.
  await manager.enqueue(
    {
      url: `${torrentBridge}${file.url}`,
      kind: "progressive",
      // A torrent path can be `Season 1/ep01.mkv`; only the last segment is a filename.
      filename: file.name.split("/").pop() ?? file.name,
      mime: null,
      size: file.length,
    },
    { start: true },
  );
  siteOptionsEl.hidden = true;
  statusEl.textContent = `Queued ${file.name} from ${torrent.name}.`;
  void watchForPeers(torrent.id);
}

/**
 * Say when a torrent has nobody to download from.
 *
 * A dead swarm is indistinguishable from a slow one at first glance, and worse than
 * either: the bridge answers the range request immediately with the length the metadata
 * states, so the download appears to start and then sits at zero forever. That is not a
 * fault anything here can fix, and it is not something a reader can guess.
 *
 * Metadata comes from peers that need not hold any of the data, which is why a torrent
 * can name its files in seconds and still never transfer a byte. The distinguishing fact
 * is the peer counts: connections tried and dying, none staying live.
 */
async function watchForPeers(id: number): Promise<void> {
  // Long enough for a healthy swarm to be plainly under way — a live one reaches
  // megabytes a second in the first few seconds — and short enough to be worth waiting.
  const CHECK_AFTER_MS = 25_000;
  await new Promise((resolve) => setTimeout(resolve, CHECK_AFTER_MS));
  try {
    const response = await fetch(`${torrentBridge}/torrent/${id}`);
    if (!response.ok) return;
    const status = (await response.json()) as {
      progress_bytes?: number;
      peers?: { live?: number; seen?: number; dead?: number } | null;
    };
    const peers = status.peers;
    if ((status.progress_bytes ?? 0) > 0 || !peers) return;
    if ((peers.live ?? 0) > 0) return;

    statusEl.className = "status-error";
    statusEl.textContent =
      (peers.seen ?? 0) > 0
        ? `No one is sharing this torrent. ${peers.seen} peers were found and ` +
          `${peers.dead ?? 0} of them dropped the connection without sending anything, ` +
          "which is what a swarm with no seeders looks like. The file list came from " +
          "peers that hold the description but not the data."
        : "No peers could be found for this torrent at all. It may be too old, or the " +
          "link may name a swarm that no longer exists.";
  } catch {
    // The bridge going away is reported by the download itself; nothing to add here.
  }
}

/** Where `npm start` puts the relay, and the only address worth guessing. */
const LOCAL_RELAY = "http://127.0.0.1:8088";

/**
 * Adopt a relay running on this machine, if one is.
 *
 * Several sites refuse a web page outright — YouTube's player endpoint answers 403 to
 * every origin but its own — and the relay is the answer, but only if it is switched on.
 * Making someone read a paragraph, find Settings and type an address before anything
 * works is a bad first five minutes, and the relay they would type is one they started
 * themselves on the next port over.
 *
 * So: only ever a loopback address, only when it answers its health check, and Settings
 * says plainly that it was found and how to turn it off. A relay somewhere else is
 * something you configure deliberately, and this never guesses at one.
 */
async function adoptLocalRelay(): Promise<void> {
  const settings = await getSettings();
  relay = { url: settings.relayUrl, enabled: settings.useRelay };
  if (settings.relayUrl) return;

  const found = await findRelay();
  if (!found) return;

  relay = { url: found, enabled: true };
  await updateSettings({ relayUrl: found, useRelay: true });
  relayAdopted = true;
  mountRelaySettings();
}

/** True when a relay at `base` answers its health check within `timeoutMs`. */
async function relayAnswers(base: string, timeoutMs: number): Promise<boolean> {
  try {
    const probe = await fetch(`${base}/healthz`, {
      signal: AbortSignal.timeout(timeoutMs),
    });
    return probe.ok && (await probe.text()).trim() === "ok";
  } catch {
    return false;
  }
}

/**
 * Find a relay to adopt: the same-origin one first, then a loopback one.
 *
 * The same-origin `/relay` is part of the deployed product, not a maybe — so it is probed
 * patiently and retried. A single 1.2s probe lost the race against a cold first load (every
 * asset loading at once): a first-time visitor got no relay and the "install the extension"
 * message, while a reload — served from cache — made the window and worked, which is why it
 * was invisible to anyone whose browser had already adopted one. A relay that is genuinely
 * absent answers 404 fast, so the longer timeout only ever waits on one that is really there
 * but briefly slow. A loopback relay is optional and probed once, briefly.
 */
async function findRelay(): Promise<string | null> {
  const [sameOrigin, loopback] = candidateBases("/relay", LOCAL_RELAY);
  for (let attempt = 0; attempt < 3; attempt++) {
    if (await relayAnswers(sameOrigin, 5000)) return sameOrigin;
    if (attempt === 0 && (await relayAnswers(loopback, 1200))) return loopback;
    await new Promise((resolve) => setTimeout(resolve, 600));
  }
  return null;
}

/** Set when the relay was found rather than typed, so Settings can say so. */
let relayAdopted = false;

// Kept so a paste that arrives before the first probe settles can wait for it rather than
// deciding "no relay" and sending the user to the extension — the race behind a first-time
// visitor being told to install the extension when the relay was there all along.
relayReady = adoptLocalRelay();

// What this page can actually do depends on which helpers answered, so the hint under
// the link box and the site catalogue are drawn once now — in the state that is true on
// opendownloader.app, where neither can — and again once both probes have finished.
void Promise.allSettled([relayReady, adoptTorrentBridge()]).then(() => {
  renderUrlHelp();
  void renderSupportedSites(document.getElementById("site-list"));
});
// Only a hint on the header control. Lazy, failure-tolerant, and never on the path
// of anything the page actually does.
void reflectAccountState(document.getElementById("account-link"));
renderUrlHelp();
void renderSupportedSites(document.getElementById("site-list"));
onLocaleChange(() => void renderSupportedSites(document.getElementById("site-list")));

const manager = new Manager({
  root: document.getElementById("manager") as HTMLElement,
  platform: webPlatform,
  extensionUrl: RELEASES_URL,
  notice:
    "Downloads run in this tab. Closing it pauses them — progress is saved, and " +
    "coming back to this page resumes from where it stopped.",
});

const toolsRoot = document.getElementById("tools") as HTMLElement;
mountTools({ root: toolsRoot, platform: webPlatform });

// After the app has mounted, so the walk sees everything it built. The
// marketing prose above the app is not in the catalogues yet and stays
// English — `t()` falls back rather than showing a key, which is what
// makes translating this page in tranches possible at all.
initPageLocale();
document.getElementById("language-slot")?.append(languagePicker());
mountTranscribePanel(toolsRoot);

void manager.start().then(() => mountRelaySettings());

/**
 * Offer what a supported platform's own page says it has.
 *
 * The extension reads this out of the tab the user is on; a web page has no such tab, so
 * here it can only work where the site answers an ordinary request — which today means
 * the API-driven extractors, and for YouTube only through a relay, because its API
 * refuses every `Origin` but its own and its media URLs do the same. The message says so
 * rather than failing vaguely.
 */
async function addFromSite(url: string): Promise<boolean> {
  if (!(await isSupportedSite(url))) return false;

  const site = (await siteFor(url)) ?? "that site";
  // The broad question — can this be resolved here at all — not the narrow one about
  // fetched pages, which is false for every fetch-first extractor including YouTube.
  const reachable = await siteWorksWithoutATab(url);

  // Answer before trying, not after failing.
  //
  // These sites are known in advance to be unreadable from here — TikTok answers a
  // server with a bot-challenge page, Douyin's detail API wants a signature that
  // rotates, Facebook and Instagram answer 400 or a login shell — so attempting the
  // fetch first buys nothing but a slower, vaguer refusal. Naming the site and pointing
  // at the thing that does work is the whole of what this page can usefully do.
  if (!reachable) {
    throw new SiteNeedsExtension(site);
  }

  statusEl.textContent = `Asking ${site} what it has…`;
  // The narrow flag still gates the page fetch itself: only an extractor that asked
  // for page state, and said it can use a fetched copy, should be handed one.
  const extraction = await extract(url, {
    readPageState: (await siteAcceptsFetchedPage(url))
      ? fetchPageState
      : undefined,
  });

  if (extraction.videos.length === 0 && extraction.audios.length === 0) {
    throw new Error(
      `${site} did not offer anything downloadable for that link.`,
    );
  }

  renderChoices(extraction, url);
  statusEl.textContent = "";
  return true;
}

/**
 * The canonical watch URL for a YouTube link, or null if it is not one.
 *
 * Mirrors the relay's own gate (`dl-relay/src/youtube.rs`): a video id is exactly the 11
 * URL-safe characters YouTube uses, and only `watch`, `youtu.be`, `shorts`, `embed`, `v`
 * and `live` carry one. Anything else is not a YouTube video and falls through to the
 * ordinary site path.
 */
function youtubeWatchUrl(raw: string): string | null {
  let u: URL;
  try {
    u = new URL(raw.trim());
  } catch {
    return null;
  }
  if (u.protocol !== "http:" && u.protocol !== "https:") return null;
  const host = u.hostname.toLowerCase().replace(/^www\./, "");
  let id: string | null = null;
  if (host === "youtu.be") {
    id = u.pathname.slice(1);
  } else if (
    host === "youtube.com" ||
    host === "m.youtube.com" ||
    host === "music.youtube.com"
  ) {
    if (u.pathname === "/watch") {
      id = u.searchParams.get("v");
    } else {
      const seg = u.pathname.split("/").filter(Boolean);
      if (["shorts", "embed", "v", "live"].includes(seg[0] ?? "")) id = seg[1] ?? null;
    }
  }
  return id && /^[A-Za-z0-9_-]{11}$/.test(id)
    ? `https://www.youtube.com/watch?v=${id}`
    : null;
}

/**
 * Whether the adopted relay actually has the `/youtube` route switched on.
 *
 * The route answers `400` ("url required") when it is enabled and `404` when it is not —
 * so a call with no `url` distinguishes a relay built for this from a plain proxy (a
 * loopback relay someone started for cross-origin fetches need not have it) without
 * triggering an extraction.
 */
async function relaySupportsYouTube(): Promise<boolean> {
  // Wait for the first adoption probe rather than deciding "no relay" mid-probe — a paste
  // seconds after load used to lose to it and fall through to the extension message.
  if (relayReady) await relayReady;
  if (!relay.enabled || !relay.url) return false;
  try {
    const probe = await fetch(`${relay.url.replace(/\/+$/, "")}/youtube`, {
      signal: AbortSignal.timeout(2500),
    });
    return probe.status === 400;
  } catch {
    return false;
  }
}

/**
 * Download a whole YouTube video through the relay's `/youtube` route.
 *
 * The relay runs yt-dlp server-side (a real, trusted session) and streams a finished MP4,
 * which is the only way past the ~60-second wall a web page hits fetching YouTube's media
 * directly. It merges video and audio before the first byte, so there is a wait; the
 * browser then saves the file itself — no in-tab queue, because the bytes never pass
 * through this page.
 *
 * Returns false when this is not a YouTube video, or no relay that can serve it is
 * present, so the caller falls through to the extension-only path with its own message.
 */
async function addFromYouTube(url: string): Promise<boolean> {
  const watch = youtubeWatchUrl(url);
  if (!watch) return false;
  if (!(await relaySupportsYouTube())) return false;

  // Free tier: up to 1080p (the relay caps it there too). Higher resolutions are the
  // credit-gated path and are not offered here yet.
  const endpoint =
    `${relay.url.replace(/\/+$/, "")}/youtube` +
    `?url=${encodeURIComponent(watch)}&height=1080`;

  // A plain navigation to the endpoint would replace this page while it waited; an anchor
  // with `download` keeps the page and lets the browser save the response. The relay sets
  // Content-Disposition, so the file is named `youtube-<id>.mp4`.
  const a = document.createElement("a");
  a.href = endpoint;
  a.download = "";
  a.rel = "noopener";
  document.body.append(a);
  a.click();
  a.remove();

  statusEl.className = "status";
  statusEl.textContent =
    "Preparing your YouTube download. The relay fetches the full video and merges it " +
    "before sending, so it can take a minute before the file starts saving — the " +
    "browser shows it in its own downloads once it does.";
  return true;
}

function stem(title: string): string {
  const cleaned = title
    .replace(/[/\\:*?"<>|]/g, "_")
    .trim()
    .slice(0, 120);
  return cleaned || "video";
}

function optionSize(option: MediaOption): number | null {
  return option.streams.reduce<number | null>(
    (sum, s) => (sum === null || s.size === null ? null : sum + s.size),
    0,
  );
}

function metaFor(option: MediaOption): string {
  const size = optionSize(option);
  return (
    [
      size ? formatSize(size) : null,
      option.streams.length > 1 ? "video + audio, joined here" : null,
    ]
      .filter(Boolean)
      .join(" · ") || "size unknown"
  );
}

function choiceRow(
  label: string,
  meta: string,
  onClick: () => void,
): HTMLElement {
  const el = document.createElement("div");
  el.className = "card row";
  const text = document.createElement("div");
  text.className = "grow";
  const top = document.createElement("div");
  top.textContent = label;
  const bottom = document.createElement("div");
  bottom.className = "muted";
  bottom.textContent = meta;
  text.append(top, bottom);
  const button = document.createElement("button");
  button.className = "primary";
  button.textContent = "Download";
  button.addEventListener("click", () => {
    button.disabled = true;
    onClick();
  });
  el.append(text, button);
  return el;
}

function picker<T extends { id: string; best: boolean }>(
  caption: string,
  choices: T[],
  describe: (c: T) => string,
): { field: HTMLElement; select: HTMLSelectElement; get(): T } {
  const select = document.createElement("select");
  for (const choice of choices) {
    const option = document.createElement("option");
    option.value = choice.id;
    // Named rather than implied by position: a scrolled dropdown shows no position.
    option.textContent = choice.best
      ? `${describe(choice)} — best`
      : describe(choice);
    select.append(option);
  }
  // Opens on the recommendation, not on the largest. Defaulting to `[0]` puts the
  // picker straight into the one state that shows a warning and a disabled button,
  // which reads as broken rather than as a choice.
  select.value = (choices.find((c) => c.best) ?? choices[0])?.id ?? "";
  const field = document.createElement("label");
  field.className = "field";
  const span = document.createElement("span");
  span.textContent = caption;
  field.append(span, select);
  return {
    field,
    select,
    get: () => choices.find((c) => c.id === select.value) ?? choices[0]!,
  };
}

const describeVideo = (v: VideoChoice): string =>
  v.size ? `${v.label} · ${formatSize(v.size)}` : v.label;
const describeAudio = (a: AudioChoice): string =>
  a.size ? `${a.label} · ${formatSize(a.size)}` : a.label;

/** The best-first list, plus two pickers for anyone who wants to choose. */
function renderChoices(extraction: Extraction, pageUrl: string): void {
  siteOptionsEl.replaceChildren();
  siteOptionsEl.hidden = false;

  const name = stem(extraction.title);
  const heading = document.createElement("p");
  heading.className = "muted";
  heading.textContent = extraction.title;
  siteOptionsEl.append(heading);

  // Why the best entry below is lower than the site's own page offers. Without it the
  // menu is correct and complete and reads as broken.
  if (extraction.note) {
    const note = document.createElement("p");
    note.className = "muted hint warn";
    note.textContent = extraction.note;
    siteOptionsEl.append(note);
  }

  const queue = (option: MediaOption) =>
    void queueOption(extraction.site, option, pageUrl).then(() => {
      siteOptionsEl.hidden = true;
      statusEl.textContent = `Queued ${option.filename}.`;
    });

  // The flagged one, not the first: "best" means best *deliverable*, and the largest
  // rendition is routinely one that cannot be given sound. Taking `[0]` here is how the
  // one-click button ends up offering a combination the merger will refuse.
  const bestVideo =
    extraction.videos.find((v) => v.best) ?? extraction.videos[0];
  const bestAudio =
    extraction.audios.find((a) => a.best) ?? extraction.audios[0];

  if (bestVideo) {
    const best = pair(bestVideo, bestAudio, name);
    siteOptionsEl.append(
      choiceRow(`Best available — ${bestVideo.label}`, metaFor(best), () =>
        queue(best),
      ),
    );
  }
  if (bestAudio) {
    const only = audioOnly(bestAudio, name);
    siteOptionsEl.append(
      choiceRow(
        `Best audio only — ${bestAudio.label}`,
        bestAudio.size ? formatSize(bestAudio.size) : "sound without picture",
        () => queue(only),
      ),
    );
  }

  if (extraction.videos.length > 1 || hasAudioChoice(extraction)) {
    const details = document.createElement("details");
    details.className = "panel card";
    const summary = document.createElement("summary");
    summary.textContent = "Choose quality yourself";
    const body = document.createElement("div");
    body.className = "stack";

    const video = picker("Video", extraction.videos, describeVideo);
    body.append(video.field);
    const audio = hasAudioChoice(extraction)
      ? picker("Audio", extraction.audios, describeAudio)
      : undefined;
    if (audio) body.append(audio.field);

    // Declared before `refresh`, which disables it: a `const` referenced by a closure
    // that runs before the declaration is a temporal dead zone error, and it takes the
    // whole panel down rather than just the button.
    const go = document.createElement("button");
    go.className = "primary";
    go.textContent = "Download this combination";
    go.addEventListener("click", () => {
      go.disabled = true;
      queue(pair(video.get(), audio?.get(), name));
    });

    const line = document.createElement("div");
    line.className = "muted";
    const refresh = () => {
      const problem = pairingProblem(video.get(), audio?.get());
      line.className = problem ? "muted hint warn" : "muted";
      line.textContent =
        problem ?? metaFor(pair(video.get(), audio?.get(), name));
      go.disabled = problem !== null;
    };
    video.select.addEventListener("change", refresh);
    audio?.select.addEventListener("change", refresh);
    refresh();

    body.append(line, go);
    details.append(summary, body);
    siteOptionsEl.append(details);
  }
}

async function queueOption(
  site: string,
  option: MediaOption,
  pageUrl: string,
): Promise<void> {
  const id = jobIdFor(option.streams.map((s) => s.url).join("|"));
  const merged = option.streams.length > 1;
  const first = option.streams[0]!;
  const now = Date.now();
  await putJob({
    id,
    url: merged ? pageUrl : first.url,
    filename: option.filename,
    kind: merged ? "merge" : await jobKindForStream(first),
    status: "queued",
    stateJson: "",
    totalBytes: option.streams.reduce<number | null>(
      (sum, s) => (sum === null || s.size === null ? null : sum + s.size),
      0,
    ),
    receivedBytes: 0,
    outputBytes: 0,
    sha256: null,
    error: null,
    createdAt: now,
    order: now,
    pageUrl,
    site,
    quality: option.label,
    expectedSha256: null,
    verification: "unverified",
    maxChunkBytes: first.max_chunk ?? undefined,
    ...(merged
      ? { mergeStreams: [option.streams[0]!, option.streams[1]!] }
      : {}),
  });
  await manager.start();
}

/** Whether a relay this page can use is switched on. */
function relayActive(): boolean {
  return relay.enabled && relay.url !== "";
}

/**
 * Show the version of the hint under the link box that is true for this page.
 *
 * The markup carries the opendownloader.app version, where no helper is reachable. A
 * page that found a relay can also take the video sites that refuse a foreign origin,
 * and one that found the torrent bridge can take magnets. Hiding rather than rewriting
 * keeps every sentence a fixed catalogue key, so each is translated like any other.
 */
function renderUrlHelp(): void {
  const help = document.getElementById("url-help");
  if (!help) return;
  for (const el of help.querySelectorAll<HTMLElement>("[data-hint]")) {
    const which = el.dataset.hint;
    el.hidden =
      which === "web" ? relayActive() : which === "relay" ? !relayActive() : !torrentBridge;
  }
}

/**
 * List the sites this build can extract from, and say plainly where each one works.
 *
 * Read from the core rather than written out here: a store build compiles the
 * large-platform extractors out, and a list typed into the page would go on advertising
 * them. Marking the extension-only ones is the point of the section — the alternative is
 * someone pasting an Instagram link and learning it from an error.
 *
 * "Here" is decided by what this page can reach, not by what the extractor can do. It
 * used to follow `withoutATab` alone, which says an extractor can work from fetches and
 * nothing about whether a browser lets this origin read the answers — so YouTube,
 * Bilibili, Vimeo, Dailymotion and X were all marked "here" on a page that refused every
 * one of them within two seconds (APP-79). A site is here when it answers any origin, or
 * when a relay this page is using makes the calls for it.
 */
async function renderSupportedSites(root: HTMLElement | null): Promise<void> {
  if (!root) return;
  try {
    const sites = await supportedSites();
    const chip = (name: string, here: boolean, where: string, title: string): HTMLElement => {
      const el = document.createElement("span");
      el.className = here ? "site here" : "site";
      const label = document.createElement("span");
      label.textContent = name;
      const whereEl = document.createElement("span");
      whereEl.className = "where";
      whereEl.textContent = where;
      el.append(label, whereEl);
      el.title = title;
      return el;
    };
    // Built in full, then swapped in with one call. This runs more than once — on load,
    // when the helper probes settle, on a language change — and those runs overlap. With
    // the websites swapped in before an `await` and the link kinds appended after it, two
    // overlapping runs each appended their own set, and the live page listed Mega, Quark
    // and the rest three times over.
    const sources = await supportedSources();
    root.replaceChildren(
      ...sites.map((site) => {
        const here = site.fromAnyOrigin || (site.withoutATab && relayActive());
        return chip(
          site.name,
          here,
          here ? t("here") : t("extension"),
          here
            ? t("Paste a {site} link on this page.", { site: site.host })
            : t("{site} will not answer this page. Open the video in a tab and use the extension there.", {
                site: site.host,
              }),
        );
      }),
      // The link kinds, after the websites. Same chips, different question: a website
      // entry answers "can this page read it", a source entry answers "does this need a
      // program running on your machine". A helper is only worth naming where this page
      // could reach one; on opendownloader.app it cannot, and the answer is the app with
      // the page inside it.
      ...sources.map((source) => {
        const here =
          !source.needsLocalHelper || (source.name === "BitTorrent" ? !!torrentBridge : relayActive());
        return chip(
          source.name,
          here,
          here ? t("here") : t("app"),
          here ? source.accepts : t("Needs the OpenDownloader app running."),
        );
      }),
    );
  } catch {
    // The list is a courtesy; the link box works without it, and an error here would say
    // nothing a visitor could act on.
    root.remove();
  }
}

/**
 * Open a `.torrent` chosen from disk.
 *
 * The bridge has always accepted the bytes of one — it tells a link from a file by the
 * first byte, since bencode begins with `d` and a link never does. What was missing was
 * any way to give it a file, which is how most torrents arrive: saved from a page, not
 * copied as a link.
 */
async function addTorrentFile(file: File): Promise<void> {
  if (pending) return;
  working(true);
  busy.start(`Reading ${file.name}\u2026`, TORRENT_STAGES);
  try {
    if (!torrentBridge) {
      throw new Error(
        CAN_REACH_LOOPBACK
          ? "A torrent needs the OpenDownloader app, which joins the swarm on this " +
              "machine. Install it and open it once, then try this file again."
          : "A torrent needs the OpenDownloader app, and this page cannot reach it: a " +
              "page served over https is not allowed to talk to a service on your own " +
              "machine. Open the app itself — it has this same page inside it.",
      );
    }
    const response = await fetch(`${torrentBridge}/torrent`, {
      method: "POST",
      body: await file.arrayBuffer(),
    });
    if (!response.ok) {
      const detail = (await response.json().catch(() => null)) as {
        error?: string;
      } | null;
      throw new Error(
        detail?.error ?? `the bridge answered ${response.status}`,
      );
    }
    renderTorrentFiles(
      (await response.json()) as {
        id: number;
        name: string;
        files: { index: number; name: string; length: number; url: string }[];
      },
    );
    busy.done();
  } catch (e) {
    busy.fail(e instanceof Error ? e.message : String(e));
  } finally {
    working(false);
  }
}

async function add(): Promise<void> {
  const typed = urlInput.value.trim();
  if (!typed || pending) return;
  working(true);
  siteOptionsEl.hidden = true;
  // A peer link is the slow case and the one worth narrating: `addFromTorrent` takes it
  // from here with its own stages, but the app has to be found before that can even
  // start, and that is already a wait.
  const peer = /^magnet:/i.test(typed) || /\.torrent(\?|$)/i.test(typed);
  busy.start(
    peer ? "Looking for the OpenDownloader app…" : "Checking that link…",
    peer ? TORRENT_STAGES : [],
  );
  try {
    // A BitTorrent link goes to the local bridge, which has the sockets a tab does not.
    // When no bridge is running this throws with how to start one, rather than with the
    // flat "not possible" it used to.
    if (await addFromTorrent(typed)) {
      urlInput.value = "";
      return;
    }

    // A Quark share is a directory, not a file: it is listed rather than queued.
    if (await addFromQuark(typed)) {
      urlInput.value = "";
      return;
    }

    // Mega before the generic path: a mega.nz link is a page, not a file, and the
    // bytes behind it are encrypted with a key only this link carries.
    if (await addFromMega(typed)) {
      urlInput.value = "";
      return;
    }

    // `thunder://` and its cousins are an ordinary URL in a base64 wrapper. Unwrap it
    // and carry on as though that URL had been pasted, which is what it is.
    const unwrapped = await resolveDownloadLink(typed);
    const url = unwrapped ?? typed;
    if (unwrapped) statusEl.textContent = "Decoded that link — checking it…";

    // YouTube before the generic site path: its own media dies at ~60s for a web page,
    // so when a relay that can run yt-dlp server-side is present, the whole video comes
    // straight from its /youtube route instead of the extension-only extractor below.
    if (await addFromYouTube(url)) {
      urlInput.value = "";
      return;
    }

    // A supported platform is asked what it has; anything else is treated as a direct
    // link to a file.
    if (await addFromSite(url)) {
      urlInput.value = "";
      return;
    }
    const candidate = await candidateForUrl(url);
    // Started here rather than merely queued: the click is the user gesture a
    // save dialog needs, and on a browser with no download folder chosen the
    // queue deliberately will not start anything without one.
    await manager.enqueue(candidate, { start: true });
    urlInput.value = "";
    statusEl.textContent = `Queued ${candidate.filename}.`;
  } catch (e) {
    statusEl.className = "status-error";
    const message = e instanceof Error ? e.message : String(e);

    // The one refusal with somewhere to send you.
    if (e instanceof SiteNeedsExtension) {
      void offerTheExtension(message, typed);
      return;
    }
    // Two shapes of the same problem: the site answered 403, or the browser refused to
    // let this page read the answer at all. Both mean "a web page cannot ask this site
    // directly", and both have the same two answers — so they get the same sentence
    // rather than one useful message and one shrug.
    if (
      !relay.enabled &&
      (/\b403\b|refused this request/.test(message) || looksLikeCorsFailure(e))
    ) {
      // Same panel as above: this is the same problem wearing a different error, and it
      // used to end in a paragraph naming two things to go and find. Now it says which
      // of them applies to this browser.
      void offerTheExtension(
        CAN_REACH_LOOPBACK
          ? "That site will not answer a web page directly — it refuses every origin but " +
              "its own. The relay gets around it: install the OpenDownloader app, leave " +
              "it running, and this page will find it on reload."
          : "That site will not answer a web page directly — it refuses every origin but " +
              "its own, and a page served over https may not talk to a relay on your own " +
              "machine — so running one will not help this page.",
        typed,
      );
      return;
    }
    statusEl.textContent = looksLikeCorsFailure(e)
      ? "That server would not let this page read the file. This is a browser restriction, " +
        "not a broken link — the extension is not subject to it, and a relay you run yourself " +
        "gets around it. Both are free."
      : e instanceof Error
        ? e.message
        : String(e);
  } finally {
    // Every branch above has already written its own last word — a queued filename, a
    // refusal, a list of files. All this has to do is stop the bar sweeping under it.
    busy.settle();
    working(false);
  }
}

goButton.addEventListener("click", () => void add());

// The file picker beside it: a torrent that arrived as a file has nowhere else to go.
const torrentFileInput = document.getElementById(
  "torrent-file",
) as HTMLInputElement | null;
openTorrentButton?.addEventListener("click", () => {
  torrentFileInput?.click();
});
torrentFileInput?.addEventListener("change", () => {
  const file = torrentFileInput.files?.[0];
  if (!file) return;
  // Cleared so choosing the same file twice fires `change` the second time.
  void addTorrentFile(file).finally(() => {
    torrentFileInput.value = "";
  });
});
urlInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") void add();
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
 * The relay settings, appended to the manager's own settings panel.
 *
 * They live here rather than in the shared manager because the extension has no
 * use for them: its host permissions mean it is never blocked by CORS, and
 * offering it a relay setting would imply a problem it does not have.
 */
function mountRelaySettings(): void {
  const panel = document.querySelector("#manager details.panel .stack");
  if (!panel) return;
  // Mounted both after the manager renders and again if a relay is adopted a moment
  // later, so the previous row is replaced rather than stacked on top of itself.
  panel.querySelectorAll("[data-relay-row]").forEach((el) => el.remove());

  // On opendownloader.app a relay on this machine is unreachable (see
  // CAN_REACH_LOOPBACK), and offering a setting that cannot work contradicted the error
  // a refused paste already gives. So the page says where the relay does apply. The
  // controls stay only if one is configured, so it can still be switched off.
  if (!CAN_REACH_LOOPBACK && !relay.url) {
    const note = document.createElement("p");
    note.className = "muted hint";
    note.setAttribute("data-relay-row", "");
    note.textContent = t(
      "Sites that refuse a web page work in the extension. The relay in the repository does the same job for a copy of this page you run yourself — a page on the web cannot reach a program on your computer.",
    );
    panel.append(note);
    return;
  }

  const url = document.createElement("input");
  url.type = "url";
  url.className = "grow";
  url.placeholder = "http://localhost:8088";
  url.spellcheck = false;
  url.value = relay.url;

  const enabled = document.createElement("input");
  enabled.type = "checkbox";
  enabled.checked = relay.enabled;

  const save = async (): Promise<void> => {
    relay = { url: url.value.trim(), enabled: enabled.checked };
    await updateSettings({ relayUrl: relay.url, useRelay: relay.enabled });
  };
  url.addEventListener("change", () => void save());
  enabled.addEventListener("change", () => void save());

  const label = document.createElement("label");
  label.append(enabled, document.createTextNode(t("Fetch through a relay")));

  const row = document.createElement("div");
  row.className = "row wrap";
  row.setAttribute("data-relay-row", "");
  row.append(label, url);

  const help = document.createElement("p");
  help.className = "muted hint";
  help.textContent = relayAdopted
    ? t(
        "A relay is running at {address} on this machine, so this page is using it. It is what lets sites that refuse a web page work here. Untick to stop using it.",
        { address: LOCAL_RELAY },
      )
    : t(
        "Optional, and only useful if a server refuses to be read by this page. The relay is in the repository (crates/dl-relay) and is meant to be run by you, on your own machine or server. It enforces the same refusals this page does, and there is no hosted one to buy.",
      );

  help.setAttribute("data-relay-row", "");
  panel.append(row, help);
}

// Test-only hook, gated by the build flag so it is absent from a shipped bundle.
// It exposes the local tools the smoke test cannot otherwise reach: each of them
// starts at a file picker, which is browser UI and cannot be driven by
// automation, while everything after the picker is the code under test.
if (__OPENDOWNLOADER_E2E__) {
  (globalThis as unknown as { __test: unknown }).__test = {
    remuxLocalSegments,
    extractMp4Audio,
    // Lets a test point the page at a relay. The relay is the only way a page can send
    // a header the Fetch standard forbids, so it is the difference between a bilibili
    // download working and the CDN answering 403 — and that is worth being able to
    // exercise without a person clicking through the settings panel.
    updateSettings,
  };
}
