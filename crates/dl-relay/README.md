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

**Tier 3 — high-maintenance frontier:** works, but fights an actively-maintained
anti-download arms race and will break periodically.
- **YouTube** — see below.

**Recommended rollout:** Tier 1 first (fast, low-risk, real UX win with no maintenance
burden), then Tier 2 as extractor work lands, and treat Tier 3 as ongoing R&D rather
than a launch blocker.

---

## YouTube status (APP-77) — what works and what doesn't

Investigated exhaustively against the live web player (2026-09-15/16). Full technical
notes and the request/response templates are archived; summary for the team:

**Solved — the 403 that blocked every YouTube download:**
1. PO token must be **content-bound to the video ID** (85 bytes; the visitor-bound
   595-byte token is rejected by the media endpoint — the web client dropped it).
2. The `n` URL parameter must be **deciphered** (nsig transform) or every media request
   403s.
3. The request must run from a **real browser session** (from Node it 403s).

With those, media flows: a correct request pulls 1080p video + audio, and audio
downloads **past the 60-second wall (~70s)** at protection status 2.

**Not solved — the deeper wall (the frontier):** after ~70s / ~1.1 MB the server stops
sending media (audio caps at status 2; any video escalates immediately to **status 3,
"attestation required"**). Getting the full file needs YouTube's ongoing BotGuard
re-attestation / SABR attestation exchange — the same actively-defended layer that makes
yt-dlp need constant updates and external PO-token providers. Tried and ruled out:
appending the pot to iOS/Android GET URLs (403), echoing SABR context updates (server
sends none), and seek-leapfrogging (0 bytes).

**Implication:** a reliable full-length YouTube downloader — relay *or* extension — means
taking on that arms race (recurring maintenance, not a one-time build). This is why
YouTube is Tier 3 above, and why the plan ships Tier 1 first.

The full pipeline (BotGuard PO-token minting via `bgutils-js`, SABR/UMP via `googlevideo`,
`n`-decipher) is the same server-side or in the extension — and it's **easier
server-side** in the relay (no CORS, no Trusted-Types), which is a point in favour of the
hosted relay for the YouTube track when we take it on.
