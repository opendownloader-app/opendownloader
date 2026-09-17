// Drive YouTube's own player to fetch a whole video, then reassemble one format.
//
// The capture hook (`yt-hook.ts`) runs at document_start on youtube.com and tees the
// player's SABR (`videoplayback`) media into `window.__odYT` as the player fetches it.
// This module drives the `<video>` playhead across the clip so the player fetches
// everything, then reads that global back and reassembles one adaptive format.
//
// Why this and not a reconstructed SABR request: YouTube serves a request carrying no
// fully-trusted session only ~60-70s (the "60 second wall"). The genuine player has full
// access, so borrowing its fetches sidesteps the wall entirely. It is inherently an
// extension technique — a relay/server has no real player.

import { ext } from "../platform/webext";

export interface YtCapture {
  itag: number;
  bytes: Uint8Array;
}

/** Drive the player on `tabId` and harvest one adaptive format (`itag`) in full. */
export async function captureYouTubeFormat(
  tabId: number,
  itag: number,
  durationSec: number,
  contentLength: number,
): Promise<YtCapture> {
  const results = await ext.scripting.executeScript({
    target: { tabId },
    world: "MAIN",
    args: [itag, durationSec, contentLength],
    func: driveAndCollect,
  });
  const result = results[0]?.result as { b64?: string; error?: string } | undefined;
  if (!result || result.error || !result.b64) {
    throw new Error(result?.error ?? "capture produced no data");
  }
  return { itag, bytes: Uint8Array.from(atob(result.b64), (c) => c.charCodeAt(0)) };
}

// Runs in the page's MAIN world (where `yt-hook` already installed `window.__odYT`).
async function driveAndCollect(
  itag: number,
  durationSec: number,
  contentLength: number,
): Promise<{ b64?: string; error?: string }> {
  interface Seg { startRange: number; expected: number; got: number; parts: Uint8Array[] }
  interface Track { lmt: number; segs: Map<number, Seg> }
  const store = (window as unknown as { __odYT?: Record<number, Track> }).__odYT;
  if (!store) return { error: "capture hook not present on this page" };
  const total = (): number => {
    const tr = store[itag]; if (!tr) return 0;
    let n = 0; for (const s of tr.segs.values()) n += s.got; return n;
  };
  const video = document.querySelector("video");
  if (!video) return { error: "no <video> on the page" };
  try { video.muted = true; await video.play().catch(() => {}); } catch { /* */ }

  const target = contentLength ? contentLength * 0.999 : Infinity;
  const deadline = Date.now() + 240_000;
  for (let pass = 0; pass < 6 && total() < target && Date.now() < deadline; pass++) {
    for (let t = 0; t <= durationSec + 5 && Date.now() < deadline; t += 12) {
      try { if (Math.abs(video.currentTime - t) > 2) video.currentTime = t; await video.play().catch(() => {}); } catch { /* */ }
      await new Promise((r) => setTimeout(r, 1300));
      if (total() >= target) break;
    }
  }
  for (let i = 0; i < 5 && total() < target; i++) await new Promise((r) => setTimeout(r, 1500));

  const tr = store[itag];
  if (!tr || tr.segs.size === 0) return { error: `no media captured for itag ${itag}` };
  const ordered = [...tr.segs.values()].sort((a, b) => a.startRange - b.startRange);
  let size = 0; for (const s of ordered) for (const p of s.parts) size += p.length;
  const out = new Uint8Array(size); let off = 0;
  for (const s of ordered) for (const p of s.parts) { out.set(p, off); off += p.length; }
  let bin = ""; for (let i = 0; i < out.length; i++) bin += String.fromCharCode(out[i]!);
  return { b64: btoa(bin) };
}
