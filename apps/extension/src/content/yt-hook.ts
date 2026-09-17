// Always-on capture hook for youtube.com, injected at document_start in the MAIN world.
//
// It must be present BEFORE the player makes its first `videoplayback` request, or the
// initialization segment (which carries the container header) is missed and the
// reassembled file has no valid header. So it is a declared content script, not a
// late executeScript injection. It only tees bytes the player fetches anyway and keeps
// them in a page global; nothing is sent anywhere until the user asks to download, at
// which point the extension drives the playhead and reads this global.
//
// Self-contained on purpose: content scripts are classic scripts, so no imports.
(() => {
  interface Seg { startRange: number; expected: number; got: number; parts: Uint8Array[] }
  interface Track { lmt: number; segs: Map<number, Seg> }
  const store: Record<number, Track> = {};
  (window as unknown as { __odYT: unknown }).__odYT = store;

  const uvar = (b: Uint8Array, o: number): [number, number] => {
    const f = b[o]!;
    const L = f < 128 ? 1 : f < 192 ? 2 : f < 224 ? 3 : f < 240 ? 4 : 5;
    let v: number;
    if (L === 1) v = f;
    else if (L === 2) v = (f & 0x3f) + 64 * b[o + 1]!;
    else if (L === 3) v = (f & 0x1f) + 32 * (b[o + 1]! + 256 * b[o + 2]!);
    else if (L === 4) v = (f & 0x0f) + 16 * (b[o + 1]! + 256 * (b[o + 2]! + 256 * b[o + 3]!));
    else v = b[o + 1]! + 256 * b[o + 2]! + 65536 * b[o + 3]! + 16777216 * b[o + 4]!;
    return [v, o + L];
  };
  const pvar = (b: Uint8Array, o: number): [number, number] => {
    let v = 0, s = 0;
    for (;;) { const c = b[o++]!; v += (c & 0x7f) * 2 ** s; if (!(c & 0x80)) break; s += 7; }
    return [v, o];
  };
  const parseHeader = (b: Uint8Array): Record<string, number> => {
    let o = 0; const h: Record<string, number> = {};
    while (o < b.length) {
      let t: number; [t, o] = pvar(b, o); const fn = t >> 3, wt = t & 7;
      if (wt === 0) { let v: number; [v, o] = pvar(b, o); if (fn === 1) h.headerId = v; else if (fn === 3) h.itag = v; else if (fn === 4) h.lmt = v; else if (fn === 6) h.startRange = v; else if (fn === 8) h.isInit = v; else if (fn === 9) h.seq = v; else if (fn === 13) h.contentLength = v; }
      else if (wt === 2) { let len: number; [len, o] = pvar(b, o); o += len; }
      else if (wt === 5) o += 4; else if (wt === 1) o += 8; else break;
    }
    return h;
  };
  const ingest = (buf: ArrayBuffer) => {
    const b = new Uint8Array(buf); let o = 0; const hdrs: Record<number, Record<string, number>> = {};
    while (o < b.length) {
      let type: number, size: number;
      [type, o] = uvar(b, o); [size, o] = uvar(b, o);
      if (size < 0 || o + size > b.length) break;
      const part = b.subarray(o, o + size); o += size;
      if (type === 20) { const h = parseHeader(part); if (h.headerId !== undefined) hdrs[h.headerId] = h; }
      else if (type === 21) {
        const h = hdrs[part[0]!];
        if (!h || h.itag === undefined) continue;
        // The initialization segment (the container header) carries the itag but NO
        // sequence number — it is the init, not a media sequence. YouTube also flags it
        // with is_init_segment (field 8). Keep it under the sentinel key -1 so it is not
        // dropped and sorts first (its startRange is 0). Dropping it was the missing
        // 1344 bytes that left the reassembled file with no valid EBML header.
        const isInit = h.isInit === 1 || (h.seq === undefined && (h.startRange ?? 0) === 0);
        const key = h.seq !== undefined ? h.seq : isInit ? -1 : undefined;
        if (key === undefined) continue;
        let track = store[h.itag];
        // Lock to the first variant (lmt) seen for this itag; YouTube's ABR serves the
        // same itag in several variants, and mixing their segments corrupts the file.
        if (!track) { track = { lmt: h.lmt ?? 0, segs: new Map() }; store[h.itag] = track; }
        if ((h.lmt ?? 0) !== track.lmt) continue;
        let seg = track.segs.get(key);
        if (!seg) { seg = { startRange: h.startRange ?? 0, expected: h.contentLength ?? 0, got: 0, parts: [] }; track.segs.set(key, seg); }
        if (seg.expected && seg.got >= seg.expected) continue;
        const payload = part.subarray(1);
        seg.parts.push(payload); seg.got += payload.length;
      }
    }
  };

  const isMedia = (url: string, method: string) =>
    /googlevideo\.com\/videoplayback/.test(url) && method === "POST";

  const origFetch = window.fetch;
  window.fetch = async function (this: unknown, ...a: Parameters<typeof fetch>) {
    const r = await origFetch.apply(this as never, a as never);
    try {
      const req = a[0]; const init = a[1];
      const url = typeof req === "string" ? req : (req as Request).url ?? "";
      const method = init?.method ?? (typeof req !== "string" ? (req as Request).method : "GET");
      if (isMedia(url, method)) r.clone().arrayBuffer().then(ingest).catch(() => {});
    } catch { /* never break the page's own fetch */ }
    return r;
  };

  // YouTube fetches some media (notably the initialization segment) over XHR rather than
  // fetch, so both are teed or the reassembled file loses its container header.
  const XHR = XMLHttpRequest.prototype;
  const origOpen = XHR.open;
  const origSend = XHR.send;
  XHR.open = function (this: XMLHttpRequest, method: string, url: string | URL, ...rest: unknown[]) {
    (this as unknown as { __od?: { url: string; method: string } }).__od = { url: String(url), method: String(method).toUpperCase() };
    return origOpen.apply(this, [method, url, ...rest] as never);
  };
  XHR.send = function (this: XMLHttpRequest, ...args: unknown[]) {
    const info = (this as unknown as { __od?: { url: string; method: string } }).__od;
    if (info && isMedia(info.url, info.method)) {
      this.addEventListener("load", () => {
        try { const resp = this.response; if (resp instanceof ArrayBuffer) ingest(resp); } catch { /* */ }
      });
      // Ensure we get bytes even if the page set a different responseType late.
      try { if (!this.responseType) this.responseType = "arraybuffer"; } catch { /* */ }
    }
    return origSend.apply(this, args as never);
  };
})();
