# dl-relay

A streaming HTTP relay that fetches media on a web page's behalf and answers with
CORS headers, so the **web app** can download from hosts that refuse a cross-origin
request. The **extension never needs it** (host permissions exempt it from CORS).

The relay is a **pipe, not a cache**: bodies stream straight through
(`Body::from_stream` over `reqwest`'s `bytes_stream()`), never buffered to memory or
disk. A 20 GiB file passes through in chunks; **nothing is stored, cached, or logged as
content**. See `src/lib.rs` for the mechanics and `src/guard.rs` for what it refuses
before opening the upstream socket.

---

## ⚠️ Direction change — read this (decided 2026-09-16 with Darius)

Historically this crate shipped **as source to self-host only**, on purpose: a hosted
relay is the one part of OpenDownloader with a bandwidth bill, and the product charged
for nothing. **That is changing.** We are standing up a **hosted relay** so the web app
does paste→download directly, with no extension — matching the UX of sites like
ytdown.to (which work because they run a server; we were serverless by design).

The extension stays the free, unlimited, client-side path. The hosted relay is a
**convenience tier** on top, gated to keep the bandwidth bill and abuse in check.

### Gating model (what to build)

| Tier | Who | Limit |
|---|---|---|
| **Free** | anonymous, **no account** | up to and including **1080p** |
| **Above 1080p** (1440p/4K/8K) | must **log in** | spend **1000 credits** per download, **or** use the extension (free) |
| **Extension** | anyone | free, unlimited, any resolution — zero server cost |

- **Anti-abuse for the free tier** is IP rate-limiting + a global throttle (no sign-in —
  explicit product call). Sign-in is required only to spend credits for >1080p.
- **Credits** go through the existing OpenApps account + gateway (see the
  `openapps-integration` skill). Charge **after** a successful download (refund-safety).
- **Storage: none.** Streaming pass-through only, video+audio merged in-flight in a
  bounded buffer. The relay keeps only content-free rate-limit counters + ops metrics.
- **Resolution cap is enforced server-side** by the relay, not trusted from the client.

Bandwidth is the only real server cost, and resolution is the lever — hence 1080p is the
free line and 4K is where credits kick in.

---

## Site coverage through the relay — stability tiers

The relay is a server-side **fetcher**, not a browser. It helps sites whose extraction is
an API/HTML fetch (`without_a_tab: true` in `dl-core/src/sites/mod.rs`); it does **not**
automatically help sites that need a rendered page DOM (`without_a_tab: false`).

**Tier 1 — stable, ship first (no arms race):** relay does the fetch server-side, web app
works directly.
- **Bilibili** — player API (relay supplies the headers the browser can't send)
- **Vimeo** — player config API
- **Dailymotion** — metadata API
- **Twitch clips** — GQL API; the relay also *fixes* the flaky CDN-CORS from APP-82
- **X** — syndication API

**Tier 2 — needs per-site extractor work:** currently read the loaded page DOM
(`without_a_tab: false`), so the relay's raw fetch isn't enough yet. TikTok and Douyin
put enough in their initial HTML to be adaptable to a server fetch; TikTok has its own
bot protection. Instagram/Facebook are heavily client-rendered and login-walled →
extension-realistic for now.
- **TikTok, Douyin** (adaptable, second wave) · **Instagram, Facebook, WeChat** (hard)

**Tier 3 — high-maintenance frontier:** works via a dedicated route, but fights an
actively-maintained anti-download arms race and will break periodically.
- **YouTube** — now has its own `/youtube` route (server-side `yt-dlp`), verified working
  from a residential IP. On a datacenter IP it needs cookies + a residential proxy. See
  the YouTube section below — this is recurring maintenance, not a one-time build.

**Recommended rollout:** Tier 1 first (fast, low-risk, real UX win with no maintenance
burden), then Tier 2 as extractor work lands, and treat Tier 3 as ongoing R&D rather
than a launch blocker.

---

## YouTube (APP-77) — the `/youtube` route

**Decision (2026-09-17, with Darius): server-side extraction, built on `yt-dlp`.** Rather
than hand-maintain a bespoke PO-token + SABR/UMP pipeline (which we proved caps at ~70s
for any session YouTube does not fully trust — the "60-second wall"), the relay drives
`yt-dlp`. It is a large, actively-maintained community project that absorbs the arms race
for us, and it is Unlicense (no GPL entanglement). This is the `/youtube` route, and it is
**off unless `[youtube].enabled` is set** — it is the one route that runs an external
program.

**Verified end-to-end (2026-09-17):** `GET /youtube?url=<watch>&height=720` against the
635 s "Big Buck Bunny" case — the one that failed at ~58 s in the extension — returned a
clean `av1 720p + opus` MP4, `ffprobe duration = 634.601 s`. The full video, past the
wall, playable.

How it works:
- **Strict URL gate first.** Only a real YouTube watch link (11-char video id) is
  accepted; anything else is 400 *before* `yt-dlp` is spawned. `yt-dlp` extracts a
  thousand sites and would fetch a `file://` or an intranet host if handed one, so this
  gate is the route's security boundary (`youtube.rs::canonical_watch_url`, tested).
- **Server-side height cap.** `max_height` (default 1080) is enforced by the relay, so it
  is never the weak link in the free-tier gate even if the caller's own check is wrong.
- **Merge to a scratch dir, then stream, then delete.** Muxing to a pipe leaves the video
  track unplayable `bin_data` (measured), so `yt-dlp` writes the merged MP4 to a
  per-request scratch directory; the relay streams that file (with a real
  `Content-Length`) and a `Drop` guard removes the directory when the response ends *or*
  the caller disconnects. Nothing is cached; nothing survives a request. The cost is
  latency — the caller waits for download-and-mux before bytes flow.

### ⚠️ Where you run it decides whether it works

YouTube flags **datacenter IPs** hardest. The verification above was from a **residential**
IP with nothing else configured. **On a cloud VM the same ~60-second wall returns** unless
you supply, in `[youtube]`:
- `cookies_file` — a Netscape-format cookies export from a logged-in account, and
- `proxy` — a **residential** egress proxy.

This is not a bug in the relay; it is the arms race, and those two fields are how you pay
into it. Also set `js_runtime = "deno"` in production: without a JS runtime, current
`yt-dlp` cannot decipher `n` for some clients and warns that formats may be missing.

**So the honest deployment note for the team:** the code is done and proven, but a naive
datacenter deploy *will* reproduce the "still doesn't work at 60s" report. Budget for a
residential proxy + a cookie pool + tracking `yt-dlp` releases (this is recurring
maintenance, not a one-time build).

### Deployed and live (2026-09-17)

The hosted relay is running on the opendownloader.app box and the web app uses it.

- **Service:** `dl-relay` built natively on the server, at `/usr/local/bin/dl-relay`, run by
  `dl-relay.service` (systemd) as `www-data` with `PrivateTmp`, `ProtectSystem=strict`,
  `NoNewPrivileges`, auto-restart, bound to `127.0.0.1:8088`. Config at
  `/etc/dl-relay/dl-relay.toml` (`[youtube] enabled=true, js_runtime="deno", max_height=1080`).
- **Dependencies on the box:** `yt-dlp` (standalone binary in `/usr/local/bin`), `ffmpeg`
  (apt), and **`deno`** in `/usr/local/bin` with `DENO_DIR=/tmp/deno` in the unit. deno is
  not optional in practice: without a JS runtime yt-dlp falls back to weaker player
  clients and 403s intermittently — that was a live failure here until deno was installed.
- **Exposure:** same-origin at `https://opendownloader.app/relay/` — an nginx `location`
  on the existing vhost, `proxy_buffering off`, `proxy_read_timeout 1800s`. Same-origin, so
  no CORS and it reuses the site's TLS cert.
- **Abuse control:** nginx `limit_req` (20 req/min/IP, burst 10) + `limit_conn` (3/IP) on
  `/relay/` (zones in `conf.d/relay-ratelimit.conf`), plus the relay's own
  `max_concurrent=4`. This is the interim gate; the credit-gate for >1080p is still to come.
- **Web app:** `apps/web/src/main.ts` auto-adopts the same-origin `/relay` (its
  `adoptLocalRelay` already probes `${origin}/relay/healthz`) and routes YouTube watch URLs
  to `/relay/youtube` as a direct browser download (`addFromYouTube`).

**Verified end-to-end (public):** `https://opendownloader.app/relay/youtube?url=…&height=144…720`
returns full-length MP4s (`ffprobe duration=634.601s`) reliably across repeated runs and
several videos.

### Still to do

1. **Credit-gating for >1080p** — the relay caps at 1080 today (free tier); wire login +
   1000-credit unlock for higher resolutions (`openapps-integration`).
2. **Keep `yt-dlp` current** — the arms race. A cron `yt-dlp -U` (or re-fetching the
   binary) plus watching for extraction-failure log lines. This IP is not flagged today;
   if it becomes so, add `cookies_file` + a residential `proxy` (both already supported).
3. **Redeploying the binary** after a relay code change: rebuild on the box from synced
   source and `systemctl restart dl-relay` (there is no CI for this crate — `publish = false`).
