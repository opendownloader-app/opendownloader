import { resolve } from "node:path";
import { defineConfig } from "vite";

// Two HTML entries (popup, manager) plus the background service worker.
//
// There is deliberately no content script. opendownloader observes network
// responses through `webRequest`, which needs no page injection at all — so the
// extension never runs code in a page it was not asked to touch.
//
// TARGET_BROWSER switches only the output directory and which manifest
// `copy-static.mjs` installs. Every JS entry compiles identically for both
// browsers; `src/platform/webext.ts` picks the right runtime API at load time,
// and `src/manager/sinks.ts` feature-detects the filesystem API.
const targetBrowser = process.env.TARGET_BROWSER === "firefox" ? "firefox" : "chrome";

// OPENDOWNLOADER_E2E gates two test-only affordances: a `__test` hook on the
// manager page (so a suite can seed jobs without driving the popup, which cannot
// be opened by automation) and a much smaller range-chunk size (so a small
// fixture still exercises the multi-chunk resume path). Unset, `if (false)`
// dead-code-eliminates the hook out of the shipped bundle entirely, and
// copy-static.mjs leaves the manifest's permissions alone.
const e2e = process.env.OPENDOWNLOADER_E2E === "1";

export default defineConfig({
  root: resolve(__dirname),
  resolve: {
    // The workspace packages are consumed as TypeScript source, not as a build
    // artifact: there is no compile step between them and this bundle, so a
    // change in the engine shows up here with no rebuild dance and `tsc
    // --noEmit` checks one program rather than a chain of .d.ts files.
    alias: {
      "@opendownloader/engine/convert": resolve(__dirname, "../../packages/engine/src/convert.ts"),
      "@opendownloader/engine": resolve(__dirname, "../../packages/engine/src/index.ts"),
      "@opendownloader/ui": resolve(__dirname, "../../packages/ui/src/index.ts"),
    },
  },
  define: {
    __OPENDOWNLOADER_E2E__: JSON.stringify(e2e),
  },
  build: {
    // The end-to-end build goes to its own folder, and that separation is the point.
    // It widens `host_permissions` to `<all_urls>` so a suite need not click permission
    // prompts, and it used to overwrite `dist` — the very folder a developer has loaded
    // unpacked in their browser. Running the suite therefore swapped their extension for
    // one holding every permission, and a reload at the wrong moment left it there,
    // announcing itself only as "Optional permission '<all_urls>' is redundant".
    //
    // Nothing that widens permissions may share an output directory with the build
    // people actually run.
    outDir: e2e
      ? targetBrowser === "firefox"
        ? "dist-e2e-firefox"
        : "dist-e2e"
      : targetBrowser === "firefox"
        ? "dist-firefox"
        : "dist",
    emptyOutDir: true,
    target: "es2022",
    // Each extension page lives in its own isolated JS world with no shared
    // module cache, so Vite's modulepreload polyfill is meaningless here and
    // Chrome logs a cross-world resource mismatch for the unused <link>.
    modulePreload: false,
    rollupOptions: {
      input: {
        popup: resolve(__dirname, "src/popup/popup.html"),
        manager: resolve(__dirname, "src/manager/manager.html"),
        background: resolve(__dirname, "src/background/index.ts"),
        "yt-hook": resolve(__dirname, "src/content/yt-hook.ts"),
      },
      output: {
        entryFileNames: "[name].js",
        chunkFileNames: "chunks/[name]-[hash].js",
        assetFileNames: "assets/[name][extname]",
      },
    },
  },
});
