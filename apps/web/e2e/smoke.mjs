// Smoke test for the standalone web app.
//
// It answers what a build alone cannot: does the wasm engine load in a plain
// page, does a pasted link produce a download that verifies against the server's
// own digest, and do the local tools work outside the extension?
//
// The extension has a full Playwright suite; this is deliberately smaller. What
// it covers is the part that is *different* here — a page with no host
// permissions, no extension APIs and no service worker — rather than re-testing
// the engine, which is the same code either way.
//
// Playwright is borrowed from the workspace rather than added as a dependency,
// the same arrangement the sibling products use. The media comes from
// `dl-testserver`, which generates it from the Rust fixtures the unit tests
// assert against, so this run and `cargo test` cannot drift.
//
// Run: node e2e/smoke.mjs   (after `npm run build`)

import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import { rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = fileURLToPath(new URL(".", import.meta.url));
const DIST = resolve(HERE, "../dist");
const REPO = resolve(HERE, "../../..");
const SERVER_BIN =
  process.env.DL_TESTSERVER_BIN ?? "/tmp/opendownloader-target/debug/dl-testserver";

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json",
  ".svg": "image/svg+xml",
  ".png": "image/png",
};

let failures = 0;
function check(name, ok, detail = "") {
  console.log(`  ${ok ? "✓" : "✗"} ${name}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures++;
}

/**
 * Serve `dist/` over http.
 *
 * Not file://, which gives the page an opaque origin with no IndexedDB — the
 * job store would fail in a way no real deployment ever does.
 */
function serveDist() {
  const server = createServer((req, res) => {
    const path = decodeURIComponent(new URL(req.url, "http://localhost").pathname);
    const file = join(DIST, path === "/" ? "/index.html" : path);
    if (!file.startsWith(DIST) || !existsSync(file)) {
      res.writeHead(404).end("not found");
      return;
    }
    res.writeHead(200, {
      "content-type": MIME[extname(file)] ?? "application/octet-stream",
      // The real deployment sets one; without it here the page would be tested
      // under looser rules than it ships with.
      "content-security-policy":
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src *; img-src 'self' data:; style-src 'self' 'unsafe-inline'",
    });
    res.end(readFileSync(file));
  });
  return new Promise((done) =>
    server.listen(0, "127.0.0.1", () =>
      done({ server, url: `http://127.0.0.1:${server.address().port}` }),
    ),
  );
}

function startTestServer() {
  const proc = spawn(SERVER_BIN, [], { stdio: ["ignore", "pipe", "pipe"] });
  return new Promise((done, fail) => {
    const timer = setTimeout(() => fail(new Error("test server did not start")), 10_000);
    proc.stdout.on("data", (chunk) => {
      const match = /listening on (\S+)/.exec(chunk.toString());
      if (match) {
        clearTimeout(timer);
        done({ proc, url: match[1] });
      }
    });
    proc.on("error", fail);
  });
}

/** Read the job store the way the page wrote it, since jobs are persisted, not held. */
const READ_JOBS = () =>
  new Promise((resolve, reject) => {
    const open = indexedDB.open("opendownloader");
    open.onerror = () => reject(open.error);
    open.onsuccess = () => {
      const db = open.result;
      const all = db.transaction("jobs", "readonly").objectStore("jobs").getAll();
      all.onerror = () => reject(all.error);
      all.onsuccess = () => resolve(all.result);
    };
  });

async function main() {
  if (!existsSync(DIST)) {
    console.error("dist/ is missing — run `npm run build` first.");
    process.exit(1);
  }
  if (!existsSync(SERVER_BIN)) {
    console.error(
      `test media server not built at ${SERVER_BIN}.\n` +
        "  CARGO_TARGET_DIR=/tmp/opendownloader-target cargo build -p dl-testserver",
    );
    process.exit(1);
  }

  // `playwright-core` rather than `@playwright/test`: the test runner's entry
  // point exports the fixture API, not the browser launchers. Its ES module
  // shape puts the launchers on the default export, not on named ones.
  const playwright = await import(resolve(REPO, "node_modules/playwright-core/index.js"));
  const { chromium } = playwright.default ?? playwright;
  const { server, url: siteUrl } = await serveDist();
  const { proc, url: mediaUrl } = await startTestServer();
  const profile = mkdtempSync(join(tmpdir(), "opendownloader-web-"));
  const browser = await chromium.launchPersistentContext(profile, {
    channel: "chromium",
    acceptDownloads: true,
  });
  const page = await browser.newPage();

  const pageErrors = [];
  page.on("pageerror", (e) => pageErrors.push(String(e)));

  try {
    console.log("\nweb app smoke test");
    await page.goto(siteUrl);

    // The manager renders only after the job store and the settings have loaded,
    // so its presence already proves IndexedDB works in this context.
    await page.waitForSelector("#manager .toolbar", { timeout: 20_000 });
    check("the page loads and the manager mounts", true);

    // The tools panel is what proves the shared UI mounted outside an extension.
    const panels = await page.locator("#tools details.panel").count();
    check("the local tools mount", panels >= 4, `${panels} panels`);

    // The catalogue renders from the wasm core, and its own error path *removes* the
    // section — a courtesy that would also hide a bug in it. So assert it is there, and
    // that both halves of it are: the websites, and the link kinds beside them.
    await page.waitForSelector("#site-list .site", { timeout: 5000 }).catch(() => {});
    const chips = await page.locator("#site-list .site").allTextContents();
    check(
      "the supported-sites catalogue renders",
      chips.length > 0,
      `${chips.length} entries`,
    );
    check(
      "the catalogue names the link kinds, not only the websites",
      ["Mega", "Quark", "BitTorrent", "eD2k", "Xunlei"].every((n) =>
        chips.some((c) => c.startsWith(n)),
      ),
      chips.join(", "),
    );

    // ---- a pasted link downloads and verifies -----------------------------
    const size = 128 * 1024;
    const expected = (await (await fetch(`${mediaUrl}/fixture.sha256?size=${size}`)).text()).trim();

    // The finished file is handed to the browser as a download; accept it so the
    // save does not hang waiting on a dialog.
    const saved = page.waitForEvent("download", { timeout: 60_000 }).catch(() => null);

    // A `.mp4`, not the `.bin` the extension suite uses: a pasted link is
    // classified from what the server says about it, and an `application/
    // octet-stream` with no recognisable extension is deliberately refused —
    // it is as likely to be a font as a video.
    await page.fill("#url", `${mediaUrl}/fixture.mp4?size=${size}`);
    await page.click("#go");

    // An explicit loop rather than `waitForFunction`: that helper resolves on
    // the *Promise object* an async predicate returns, which is always truthy,
    // so it would complete immediately and yield nothing. The loop also lets a
    // timeout report the status the job was actually stuck on.
    const job = await (async () => {
      const deadline = Date.now() + 60_000;
      let last = null;
      for (;;) {
        const jobs = await page.evaluate(READ_JOBS);
        last = jobs.find((j) => j.status === "done" || j.status === "error") ?? jobs[0] ?? null;
        if (last && (last.status === "done" || last.status === "error")) return last;
        if (Date.now() > deadline) return last ?? { status: "(no job was created)" };
        await page.waitForTimeout(250);
      }
    })();

    check("a pasted link downloads", job.status === "done", job.error ?? "");
    // The assertion that matters: the digest is computed by reading the finished
    // bytes back, so it describes what actually landed rather than what was meant.
    check(
      "the finished file verifies against the server's own digest",
      job.sha256 === expected,
      job.sha256 === expected ? "" : `got ${job.sha256}, expected ${expected}`,
    );
    const download = await saved;
    check("the file is handed to the browser to save", download !== null, download?.suggestedFilename() ?? "");

    // ---- a CDN that refuses preflights ------------------------------------
    // This page and the media server are on different ports, so every request here is
    // cross-origin and meets the browser's CORS checks for real — no request routing,
    // which changes how Chromium handles preflights and once hid exactly this. Twitch's
    // CloudFront allows a plain cross-origin GET and answers every preflight with 403;
    // the engine's `If-Range` forced one on each chunk, so the only video site the web
    // app advertised failed at 0 bytes (APP-82).
    const pasteAndWait = async (url) => {
      await page.fill("#url", url);
      await page.click("#go");
      const deadline = Date.now() + 60_000;
      for (;;) {
        const mine = (await page.evaluate(READ_JOBS)).find((j) => j.url === url);
        if (mine && (mine.status === "done" || mine.status === "error")) return mine;
        if (Date.now() > deadline) return mine ?? { status: "(no job was created)" };
        await page.waitForTimeout(250);
      }
    };
    // Two chunks, so the second request is a real mid-file range rather than the start.
    const strictSize = 8 * 1024 * 1024 + 4096;
    const strictExpected = (
      await (await fetch(`${mediaUrl}/fixture.sha256?size=${strictSize}`)).text()
    ).trim();
    const strictSaved = page.waitForEvent("download", { timeout: 120_000 }).catch(() => null);
    const strict = await pasteAndWait(`${mediaUrl}/fixture.mp4?size=${strictSize}&no_preflight=1`);
    await strictSaved;
    check(
      "a host that refuses CORS preflights still downloads, and verifies",
      strict.status === "done" && strict.sha256 === strictExpected,
      strict.status === "done" ? (strict.sha256 === strictExpected ? "" : `digest ${strict.sha256}`) : strict.error ?? strict.status,
    );

    // Without `If-Range` on those requests, a file replaced mid-download has to be caught
    // from the answers themselves. `flip=1` changes the ETag after the probe.
    const flipped = await pasteAndWait(`${mediaUrl}/fixture.mp4?size=${size}&no_preflight=1&flip=1`);
    check(
      "a file that changes mid-download is refused rather than spliced",
      flipped.status === "error" && /changed on the server/.test(flipped.error ?? ""),
      `${flipped.status}: ${flipped.error ?? ""}`,
    );

    // ---- the local remux tool --------------------------------------------
    //
    // Driven through the module the page already loaded rather than the file
    // picker, which is browser UI and cannot be automated. Everything after the
    // picker — the wasm remuxer, the hashing — is the code under test.
    const remuxExpected = (await (await fetch(`${mediaUrl}/hls/expected.sha256`)).text()).trim();
    const remux = await page.evaluate(async (base) => {
      const files = [];
      for (let i = 0; i < 4; i++) {
        const bytes = await (await fetch(`${base}/hls/low/seg${i}.ts`)).arrayBuffer();
        files.push(new File([bytes], `seg${i}.ts`, { type: "video/mp2t" }));
      }
      const out = await globalThis.__test.remuxLocalSegments({ files });
      return { sha256: out.sha256, size: out.blob.size };
    }, mediaUrl).catch((e) => ({ error: String(e) }));

    if (remux.error) {
      check("local .ts segments remux into a fragmented MP4", false, remux.error);
    } else {
      check(
        "local .ts segments remux to the same bytes the native remuxer produces",
        remux.sha256 === remuxExpected,
        `${remux.size} bytes`,
      );
    }

    // The platform name must not reach anything a visitor can read. This is a
    // regression test, not a style check: the string that got through last time was
    // inside a vendored element's shadow DOM — `<openapps-login>` defaults its heading
    // to "Sign in to OpenApps" — so grepping our own source would have passed while the
    // rendered page said it plainly. Hence: read what the page actually shows.
    const visibleText = await page.evaluate(() =>
      [
        document.body.innerText,
        ...[...document.querySelectorAll("*")]
          .filter((el) => el.shadowRoot)
          .map((el) => el.shadowRoot.textContent ?? ""),
      ].join(" "),
    );
    const leaked = visibleText.match(/OpenApps/g) ?? [];
    check(
      "the platform name is not visible anywhere on the page",
      leaked.length === 0,
      leaked.length ? visibleText.match(/.{0,70}OpenApps.{0,70}/)[0].replace(/\s+/g, " ") : "",
    );

    // The other half of the same property: no shipped source outside the one module that
    // defines the hostnames may name the backend. Two literals is how OpenCapture moved
    // to a custom domain and left one call site pointing at the old host.
    const bundleLeak = await page.evaluate(async () => {
      const srcs = [...document.querySelectorAll("script[src]")].map((s) => s.src);
      const bodies = await Promise.all(
        srcs.map((u) => fetch(u).then((r) => r.text()).catch(() => "")),
      );
      return bodies.filter((b) => b.includes("accounts.openapps.network")).length;
    });
    check(
      "no shipped bundle names the backend directly",
      bundleLeak === 0,
      bundleLeak ? `${bundleLeak} bundle(s) contain accounts.openapps.network` : "",
    );

    // Encrypted downloads: the counter arithmetic, in the browser's own WebCrypto.
    //
    // This is the part of Mega support most likely to be silently wrong, and wrong here
    // does not throw — it writes plausible-looking noise to disk and reports success.
    // The cases below are the ones a real download produces: chunk boundaries, offsets
    // that do not land on an AES block (a resumed range), and ranges arriving out of
    // order from parallel connections.
    const ctr = await page.evaluate(async () => {
      const fill = (n) => {
        const a = new Uint8Array(n);
        for (let i = 0; i < n; i += 65536)
          crypto.getRandomValues(a.subarray(i, Math.min(i + 65536, n)));
        return a;
      };
      const keyRaw = fill(16), nonce = fill(8), plain = fill(200_000);
      const counter0 = new Uint8Array(16);
      counter0.set(nonce, 0);
      const encKey = await crypto.subtle.importKey("raw", keyRaw, "AES-CTR", false, ["encrypt"]);
      const cipher = new Uint8Array(await crypto.subtle.encrypt(
        { name: "AES-CTR", counter: counter0, length: 64 }, encKey, plain));

      const key = await crypto.subtle.importKey("raw", keyRaw, "AES-CTR", false, ["decrypt"]);
      const decrypt = async (offset, bytes) => {
        const counter = new Uint8Array(16);
        counter.set(nonce, 0);
        let block = BigInt(Math.floor(offset / 16));
        for (let i = 15; i >= 8; i--) { counter[i] = Number(block & 0xffn); block >>= 8n; }
        const skip = offset % 16;
        let input = bytes;
        if (skip !== 0) { input = new Uint8Array(skip + bytes.length); input.set(bytes, skip); }
        const out = new Uint8Array(
          await crypto.subtle.decrypt({ name: "AES-CTR", counter, length: 64 }, key, input));
        return skip === 0 ? out : out.subarray(skip);
      };
      const same = (a, b) => a.length === b.length && a.every((v, i) => v === b[i]);

      const run = async (step, shuffle) => {
        const offsets = [];
        for (let o = 0; o < cipher.length; o += step) offsets.push(o);
        if (shuffle) offsets.sort(() => Math.random() - 0.5);
        const out = new Uint8Array(plain.length);
        for (const o of offsets)
          out.set(await decrypt(o, cipher.subarray(o, Math.min(o + step, cipher.length))), o);
        return same(out, plain);
      };
      return {
        aligned: await run(65536, false),
        unaligned: await run(1000, false),
        outOfOrder: await run(4096, true),
      };
    });
    check(
      "encrypted chunks decrypt at any offset and in any order",
      ctr.aligned && ctr.unaligned && ctr.outOfOrder,
      JSON.stringify(ctr),
    );

    // ---- what a visitor to the hosted page is told ------------------------
    // Everything above runs from http://127.0.0.1, the one place a relay or the torrent
    // bridge could answer — which is exactly why the catalogue once marked five video
    // sites "here" that refused every visitor to opendownloader.app within two seconds
    // (APP-79). So load the same build from an https origin that is not loopback, with
    // the loopback helpers unreachable as they are for a real visitor, and hold the
    // page to what it can actually do there.
    const hosted = await browser.newPage();
    hosted.on("pageerror", (e) => pageErrors.push(`hosted: ${e}`));
    await hosted.route("http://127.0.0.1:*/**", (r) => r.abort());
    await hosted.route("https://opendownloader.test/**", async (r) => {
      const path = decodeURIComponent(new URL(r.request().url()).pathname);
      const file = join(DIST, path === "/" ? "/index.html" : path);
      if (!file.startsWith(DIST) || !existsSync(file)) return r.fulfill({ status: 404, body: "not found" });
      // The core arrives late, as it does over a real network. The helper probes settle
      // first, so the catalogue's second draw starts while its first is still waiting —
      // the overlap that tripled the link kinds on the live site and never happened
      // against a local server fast enough to hide it.
      if (/dl_core_bg-.*\.wasm$/.test(path)) await new Promise((done) => setTimeout(done, 2500));
      return r.fulfill({
        status: 200,
        headers: { "content-type": MIME[extname(file)] ?? "application/octet-stream" },
        body: readFileSync(file),
      });
    });
    const catalogue = async () => {
      await hosted.waitForSelector("#site-list .site", { timeout: 20_000 });
      // Both probes have to have settled before the labels are final.
      await hosted.waitForTimeout(5000);
      return hosted.$$eval("#site-list .site", (els) =>
        els.map((el) => [el.firstChild.textContent, el.classList.contains("here"), el.lastChild.textContent]),
      );
    };
    await hosted.goto("https://opendownloader.test/");
    const chipsHosted = await catalogue();
    // The catalogue is drawn more than once as the page learns what it can reach, and
    // overlapping draws once appended the link kinds three times on the live site.
    const names = chipsHosted.map(([n]) => n);
    check(
      "every catalogue entry appears exactly once",
      new Set(names).size === names.length,
      names.join(", "),
    );
    const videoSites = ["YouTube", "Bilibili", "Vimeo", "Dailymotion", "Twitch clips", "X"];
    const hereOnHosted = chipsHosted.filter(([n, here]) => here && videoSites.includes(n)).map(([n]) => n);
    check(
      "on the hosted page, only the video sites that answer any origin are marked here",
      JSON.stringify(hereOnHosted) === JSON.stringify(["Twitch clips"]),
      JSON.stringify(chipsHosted.filter(([n]) => videoSites.includes(n))),
    );
    const helpShown = await hosted.$$eval("#url-help [data-hint]", (els) =>
      els.filter((el) => !el.hidden).map((el) => el.dataset.hint),
    );
    check(
      "the hint under the link box is the hosted page's version",
      JSON.stringify(helpShown) === JSON.stringify(["web", "web"]),
      JSON.stringify(helpShown),
    );
    const helpText = await hosted.$$eval("#url-help [data-hint]:not([hidden])", (els) =>
      els.map((el) => el.textContent).join(" "),
    );
    check(
      "the hint does not claim YouTube or Bilibili work on the page",
      /YouTube, Bilibili, Vimeo, Dailymotion and X links need the extension/.test(helpText) &&
        !/A video page on YouTube/.test(helpText),
      helpText,
    );
    await hosted.click("#manager details.panel summary").catch(() => {});
    const relayRows = await hosted.$$eval("[data-relay-row]", (els) => els.map((el) => el.textContent));
    check(
      "Settings does not offer a relay the hosted page cannot reach",
      relayRows.length === 1 && !relayRows[0].includes("Fetch through a relay") &&
        relayRows[0].includes("extension"),
      JSON.stringify(relayRows),
    );

    // The same, in 简体中文: the hint and the catalogue were the two places left in English.
    await hosted.evaluate(() => localStorage.setItem("opendownloader.locale", "zh-Hans"));
    await hosted.goto("https://opendownloader.test/");
    const chipsZh = await catalogue();
    const helpZh = await hosted.$$eval("#url-help [data-hint]:not([hidden])", (els) =>
      els.map((el) => el.textContent).join(" "),
    );
    const headingZh = await hosted.textContent("#sites h2");
    check(
      "in 简体中文 the hint, the heading and the catalogue labels are translated",
      helpZh.includes("需要使用扩展") && headingZh.trim() === "支持的网站" &&
        chipsZh.some(([n, , w]) => n === "YouTube" && w === "扩展") &&
        chipsZh.some(([n, , w]) => n === "Twitch clips" && w === "本页"),
      JSON.stringify({ helpZh, headingZh, sample: chipsZh.slice(0, 3) }),
    );
    await hosted.close();

    check("no uncaught page errors", pageErrors.length === 0, pageErrors.join("; "));
  } finally {
    await browser.close();
    server.close();
    proc.kill();
    await rm(profile, { recursive: true, force: true });
  }

  console.log(failures === 0 ? "\nall checks passed\n" : `\n${failures} check(s) failed\n`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
