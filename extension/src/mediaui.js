// Shared by the page overlay (content script) and the toolbar popup:
// one row per detected video with a preview, resolution, length and size,
// so you can tell which one you're about to download.
globalThis.HurtleUI ??= (() => {
  function fmtSize(n) {
    if (!n) return "";
    const u = ["B", "KB", "MB", "GB", "TB"];
    let i = 0;
    while (n >= 1024 && i < u.length - 1) (n /= 1024), i++;
    return `${n.toFixed(i ? 1 : 0)} ${u[i]}`;
  }

  function fmtTime(s) {
    if (!Number.isFinite(s) || s <= 0) return "";
    s = Math.round(s);
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = String(s % 60).padStart(2, "0");
    return h ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
  }

  /** "720p" for 1280×720 and for a vertical 720×1280 alike (the short side). */
  function quality(w, h) {
    const q = Math.min(w || 0, h || 0) || h || 0;
    return q ? `${q}p` : "";
  }

  function fileName(url) {
    try {
      const last = new URL(url).pathname.split("/").filter(Boolean).pop() || new URL(url).hostname;
      return decodeURIComponent(last);
    } catch {
      return url;
    }
  }

  const KIND = { "video/mp4": "MP4", "video/webm": "WebM", "video/x-matroska": "MKV", "video/quicktime": "MOV", "audio/mpeg": "MP3", "audio/mp4": "M4A" };

  function typeLabel(m) {
    if (m.kind === "stream") return /\.mpd(?:$|[?#])|dash/i.test(m.url + (m.mime || "")) ? "DASH stream" : "HLS stream";
    const ext = /\.([a-z0-9]{2,4})(?:$|[?#])/i.exec(new URL(m.url).pathname)?.[1];
    return KIND[m.mime] || (ext ? ext.toUpperCase() : m.mime || "Media");
  }

  /**
   * Detail line: "720p · 1280×720 · 2:31 · 45.2 MB · MP4" for files,
   * "HLS stream · 1080p 720p 480p · 2:31" for streams. `meta` is what a
   * <video> reported ({ w, h, duration }); it wins over sniffed values.
   */
  function details(m, meta = {}) {
    const parts = [];
    if (m.kind === "stream") {
      parts.push(m.live ? `${typeLabel(m)} · live` : typeLabel(m));
      if (m.qualities?.length) parts.push(m.qualities.map((q) => `${q}p`).join(" "));
      else if (meta.w) parts.push(quality(meta.w, meta.h));
    } else {
      const w = meta.w || m.width;
      const h = meta.h || m.height;
      if (w && h) parts.push(`${quality(w, h)} · ${w}×${h}`);
      else if (/^audio\//.test(m.mime || "")) parts.push("Audio");
    }
    const t = fmtTime(meta.duration || m.duration);
    if (t) parts.push(t);
    if (m.size) parts.push(fmtSize(m.size));
    if (m.kind !== "stream") parts.push(typeLabel(m));
    return parts.filter(Boolean).join(" · ");
  }

  /**
   * Only the best few resolutions get a quick chip; the dialog offers the
   * rest. Takes heights (720) or size objects ({ quality }); highest first,
   * equal heights once.
   */
  function topQualities(list, n = 3, preferred = 0) {
    const key = (q) => (typeof q === "object" ? q.quality || q.height || 0 : q);
    const seen = new Set();
    const all = (list || [])
      .filter((q) => {
        const k = key(q);
        if (!k || seen.has(k)) return false;
        seen.add(k);
        return true;
      })
      .sort((a, b) => key(b) - key(a));
    const top = all.slice(0, n);
    // The preferred resolution always gets a chip, even below the top few.
    const pick = pickHeight(all.map(key), preferred);
    if (preferred && pick && !top.some((q) => key(q) === pick)) top.splice(n - 1, 1, all.find((q) => key(q) === pick));
    return top;
  }

  /**
   * The height the user's preferred resolution lands on: the highest one at
   * or below it, else the lowest there is. 0 = best available.
   */
  function pickHeight(heights, preferred) {
    const hs = (heights || []).filter((h) => h > 0);
    if (!hs.length) return 0;
    if (!preferred) return Math.max(...hs);
    const below = hs.filter((h) => h <= preferred);
    return below.length ? Math.max(...below) : Math.min(...hs);
  }

  /** Stream heights (720) as chips, the preferred one marked. */
  function heightChips(list, preferred) {
    const top = topQualities(list, 3, preferred);
    const pick = pickHeight(top, preferred);
    return top.map((h) => ({ label: `${h}p`, value: h, pick: h === pick }));
  }

  /** Is `host` the blocked hostname or a subdomain of it? */
  function hostBlocked(list, host) {
    host = (host || "").toLowerCase();
    return !!host && (list || []).some((h) => {
      h = String(h).toLowerCase();
      return host === h || host.endsWith(`.${h}`);
    });
  }

  /**
   * yt-dlp's sizes for a page ({ label, quality, width, height, size }) as
   * chips: "1080p · 45 MB", with the frame size in the tooltip.
   */
  function sizeChips(sizes, preferred = 0) {
    const pick = pickHeight((sizes || []).map((z) => z.quality), preferred);
    return topQualities(sizes, 3, preferred).map((z) => {
      const dims = z.width && z.height ? `${z.width}×${z.height}` : "";
      const size = z.size ? `≈ ${fmtSize(z.size)}` : "";
      return {
        label: [z.label, z.size ? fmtSize(z.size) : ""].filter(Boolean).join(" · "),
        value: z.quality,
        pick: z.quality === pick,
        title: `Download ${[z.label, dims, size].filter(Boolean).join(" · ")}`,
      };
    });
  }

  /** "4 sizes · up to 1920×1080 · 3:12" for the page row. */
  function sizesLine(info) {
    const top = info.sizes[0];
    const best = top ? (top.width && top.height ? `up to ${top.width}×${top.height}` : `up to ${top.label}`) : "";
    const n = info.sizes.length;
    return [n === 1 ? "1 size" : `${n} sizes`, best, fmtTime(info.duration), info.audio > 1 ? `${info.audio} audio tracks` : ""]
      .filter(Boolean)
      .join(" · ");
  }

  function playIcon() {
    const NS = "http://www.w3.org/2000/svg";
    const svg = document.createElementNS(NS, "svg");
    for (const [k, v] of Object.entries({ viewBox: "0 0 24 24", width: "18", height: "18", fill: "currentColor" })) svg.setAttribute(k, v);
    const path = document.createElementNS(NS, "path");
    path.setAttribute("d", "M8 5v14l11-7z");
    svg.append(path);
    return svg;
  }

  /**
   * A 96×54 thumbnail. Sources, best first: a live <video> on the page
   * (its current frame), a poster/og:image, or a muted preview that loads
   * only metadata and seeks one second in. Reports { w, h, duration }.
   */
  function thumb({ live, image, previewUrl, onMeta }) {
    const box = document.createElement("div");
    box.className = "thumb";
    box.append(playIcon());
    const badge = document.createElement("span");
    badge.className = "dur";
    const report = (v) => {
      const meta = { w: v.videoWidth, h: v.videoHeight, duration: Number.isFinite(v.duration) ? v.duration : 0 };
      if (meta.duration) badge.textContent = fmtTime(meta.duration);
      onMeta?.(meta);
    };
    const put = (el) => {
      box.replaceChildren(el, badge);
    };
    if (live && live.readyState >= 2 && live.videoWidth) {
      const c = document.createElement("canvas");
      c.width = 192;
      c.height = 108;
      try {
        const g = c.getContext("2d");
        const s = Math.min(c.width / live.videoWidth, c.height / live.videoHeight);
        const w = live.videoWidth * s;
        const h = live.videoHeight * s;
        g.drawImage(live, (c.width - w) / 2, (c.height - h) / 2, w, h);
        put(c);
      } catch {}
      report(live);
      return box;
    }
    if (live) report(live);
    // Image first; if it fails (404, page CSP), the preview, then the icon.
    const icon = () => box.replaceChildren(playIcon());
    const loadPreview = () => {
      if (!previewUrl) return icon();
      const v = document.createElement("video");
      v.muted = true;
      v.preload = "metadata";
      v.playsInline = true;
      v.addEventListener("loadedmetadata", () => {
        report(v);
        if (v.duration > 2) v.currentTime = Math.min(1, v.duration / 4);
      }, { once: true });
      v.addEventListener("error", icon, { once: true });
      v.src = previewUrl;
      put(v);
    };
    if (image) {
      const img = document.createElement("img");
      img.alt = "";
      img.referrerPolicy = "no-referrer";
      img.addEventListener("error", loadPreview, { once: true });
      img.src = image;
      put(img);
    } else if (previewUrl) {
      loadPreview();
    }
    return box;
  }

  /**
   * A whole row: thumbnail, title, details, file name. Returns the element
   * and a setter for the details once the preview has loaded.
   */
  function row({ tag = "button", title, sub, file, thumbOpts, chips, onClick, onChip, badge }) {
    const el = document.createElement(tag);
    el.className = "item";
    if (tag === "button") el.type = "button";
    const meta = document.createElement("div");
    meta.className = "meta";
    const t = document.createElement("div");
    t.className = "title";
    t.textContent = title;
    t.title = title;
    if (badge) {
      const b = document.createElement("span");
      b.className = "tag";
      b.textContent = badge;
      t.prepend(b);
    }
    const s = document.createElement("div");
    s.className = "sub";
    s.textContent = sub || "";
    meta.append(t, s);
    if (file) {
      const f = document.createElement("div");
      f.className = "file";
      f.textContent = file;
      meta.append(f);
    }
    const c = document.createElement("div");
    c.className = "chips";
    meta.append(c);
    const setChips = (list) => {
      c.replaceChildren();
      // A chip is a height (720 → "720p") or { label, value, title }.
      for (const chip of list || []) {
        const { label, value, title: tip, pick } = typeof chip === "object" ? chip : { label: `${chip}p`, value: chip };
        const b = document.createElement("span");
        b.className = pick ? "chip pick" : "chip";
        b.textContent = label;
        b.title = (tip || `Download ${label}`) + (pick ? " (your preferred resolution; clicking the row does the same)" : "");
        b.addEventListener("click", (e) => {
          e.stopPropagation();
          e.preventDefault();
          onChip?.(value);
        });
        c.append(b);
      }
    };
    if (chips?.length) setChips(chips);
    el.append(thumb(thumbOpts || {}), meta);
    if (onClick) el.addEventListener("click", onClick);
    return { el, setSub: (text) => (s.textContent = text), setChips: (list) => setChips(list) };
  }

  // Overlay and popup style these the same way; colors come from each host.
  const CSS = `
    .item { display: flex; gap: 10px; align-items: center; width: 100%; text-align: left; padding: 6px; border: 0;
      border-radius: 8px; background: none; color: inherit; font: inherit; cursor: pointer; }
    .thumb { position: relative; flex: none; width: 96px; height: 54px; border-radius: 6px; overflow: hidden;
      background: rgba(127,127,127,.25); display: grid; place-items: center; color: rgba(255,255,255,.75); }
    .thumb > img, .thumb > video, .thumb > canvas { width: 100%; height: 100%; object-fit: cover; display: block; }
    .thumb .dur { position: absolute; right: 3px; bottom: 3px; padding: 0 4px; border-radius: 3px; font-size: 10px;
      line-height: 15px; background: rgba(0,0,0,.75); color: #fff; }
    .thumb .dur:empty { display: none; }
    .meta { flex: 1; min-width: 0; }
    .meta .title { font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .meta .sub { font-size: 11px; opacity: .8; }
    .meta .file { font-size: 10px; opacity: .55; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .tag { display: inline-block; margin-right: 6px; padding: 0 5px; border-radius: 4px; font-size: 10px; font-weight: 600;
      line-height: 16px; vertical-align: 1px; background: #0284c7; color: #fff; }
    .chips:empty { display: none; }
    .chips { display: flex; flex-wrap: wrap; gap: 4px; margin-top: 3px; }
    .chip { padding: 0 6px; border-radius: 999px; border: 1px solid currentColor; font-size: 10px; line-height: 16px; opacity: .8; }
    .chip:hover, .chip.pick { opacity: 1; background: #0284c7; border-color: #0284c7; color: #fff; }
  `;

  return { fmtSize, fmtTime, quality, fileName, typeLabel, details, sizeChips, heightChips, pickHeight, topQualities, hostBlocked, sizesLine, thumb, row, CSS };
})();
