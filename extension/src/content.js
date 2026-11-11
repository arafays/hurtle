// Hurtle page overlay: a "Download video" button over playing media, like
// IDM's floating panel. Lives in a closed shadow root so page CSS can't touch it.
(() => {
  if (window.__hurtleOverlay) return;
  window.__hurtleOverlay = true;

  const api = globalThis.browser ?? globalThis.chrome;

  // Magnet links open Hurtle's dialog (file list, streaming) instead of the
  // system's torrent app, while "catch downloads" is on.
  let catching = true;
  api.storage.local.get("enabled").then((r) => (catching = r.enabled !== false), () => {});
  api.storage.onChanged.addListener((c, area) => {
    if (area === "local" && c.enabled) catching = c.enabled.newValue !== false;
  });
  document.addEventListener(
    "click",
    (e) => {
      // Only the user's own clicks: a page calling a.click() mustn't start a torrent.
      if (!catching || !e.isTrusted || e.defaultPrevented || e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) return;
      const a = e.composedPath().find((n) => n instanceof HTMLAnchorElement && n.href);
      if (!a || !/^magnet:\?/i.test(a.href)) return;
      e.preventDefault();
      e.stopPropagation();
      const href = a.href;
      api.runtime
        .sendMessage({ type: "download-magnet", url: href, title: a.textContent.trim().slice(0, 200), pageUrl: location.href })
        .then(
          (r) => r?.ok || (location.href = href),
          () => (location.href = href),
        );
    },
    true,
  );

  const MIN_W = 200;
  const MIN_H = 110;

  const host = document.createElement("hurtle-overlay");
  host.style.cssText = "all: initial; position: fixed; z-index: 2147483647; top: 0; left: 0; display: none;";
  const root = host.attachShadow({ mode: "closed" });
  root.innerHTML = `
    <style>
      :host { all: initial; }
      .wrap { font: 13px/1.3 system-ui, sans-serif; color: #f8fafc; display: flex; flex-direction: column; align-items: flex-end; }
      button.main {
        display: flex; align-items: center; gap: 6px; padding: 6px 10px; border: 0; border-radius: 8px;
        background: rgba(17, 24, 39, .92); color: inherit; font: inherit; cursor: pointer;
        box-shadow: 0 2px 10px rgba(0,0,0,.35);
      }
      button.main:hover { background: #0369a1; }
      .menu {
        margin-top: 6px; width: 380px; max-width: calc(100vw - 16px); max-height: min(420px, 70vh); overflow: auto;
        background: rgba(17, 24, 39, .97); border-radius: 10px; box-shadow: 0 6px 24px rgba(0,0,0,.45); padding: 4px;
      }
      .item:hover:not([disabled]) { background: rgba(3, 105, 161, .85); }
      .note { padding: 6px 8px; font-size: 12px; opacity: .8; }
      .head { padding: 4px 8px 2px; font-size: 10px; letter-spacing: .05em; text-transform: uppercase; opacity: .6; }
      .bar { display: flex; align-items: stretch; gap: 4px; }
      button.x { padding: 6px 8px; border: 0; border-radius: 8px; background: rgba(17, 24, 39, .92); color: inherit;
        font: inherit; opacity: .7; cursor: pointer; box-shadow: 0 2px 10px rgba(0,0,0,.35); }
      button.x:hover { opacity: 1; background: #0369a1; }
      button:focus-visible { outline: 2px solid #38bdf8; outline-offset: 1px; }
      .count { padding: 0 6px; border-radius: 999px; background: #0284c7; color: #fff; font-size: 11px; font-weight: 600; line-height: 17px; }
      button.main:hover .count:not(.busy) { background: #fff; color: #0369a1; }
      .count.busy { background: rgba(255,255,255,.18); font-weight: 400; }
      .off { display: block; width: 100%; margin-top: 2px; padding: 6px 8px; border: 0; border-top: 1px solid rgba(255,255,255,.12);
        background: none; color: inherit; font: inherit; font-size: 11px; text-align: left; opacity: .65; cursor: pointer; }
      .off:hover { opacity: 1; text-decoration: underline; }
      .caret { font-size: 11px; opacity: .85; margin-left: -2px; }
    </style>
    <div class="wrap">
      <div class="bar">
      <button class="main" title="Download with Hurtle" aria-haspopup="menu" aria-expanded="false">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"
          stroke-linecap="round" stroke-linejoin="round"><path d="M12 4v12M6 11l6 6 6-6M5 20h14"/></svg>
        <span class="label">Download video</span>
        <span class="count" hidden></span>
        <span class="caret" aria-hidden="true" hidden>▾</span>
      </button>
      <button type="button" class="x" aria-label="Hide on this page" title="Hide on this page (bring it back from the Hurtle toolbar button)">✕</button>
      </div>
      <div class="menu" hidden></div>
    </div>`;
  const shared = document.createElement("style");
  shared.textContent = HurtleUI.CSS;
  root.prepend(shared);
  const button = root.querySelector("button.main");
  const close = root.querySelector(".x");
  const menu = root.querySelector(".menu");
  const count = root.querySelector(".count");
  const caret = root.querySelector(".caret");
  menu.setAttribute("role", "menu");
  new MutationObserver(() => {
    button.setAttribute("aria-expanded", String(!menu.hidden));
    if (!menu.hidden) for (const i of menu.querySelectorAll("button.item")) i.setAttribute("role", "menuitem");
  }).observe(menu, { attributes: true, attributeFilter: ["hidden"] });
  // Escape closes the menu and gives focus back to the button.
  root.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !menu.hidden) {
      e.stopPropagation();
      menu.hidden = true;
      onSizes = null;
      button.focus();
      place();
    }
  });

  let target = null; // the media element the button belongs to
  let hideTimer = 0;
  // ✕ hides the button on this page until it's re-enabled from the toolbar
  // popup ("Show it again on this page") or the page is reloaded.
  let dismissed = false;
  let enabled = true; // popup switch "Show button over videos"
  let siteOff = false; // this site is in the "don't show on" list (overlayOff)
  let ready = false; // nothing shows until the settings above are read
  let preferred = 1080; // popup setting "Preferred resolution" (0 = best)
  // The site in the address bar: a player in an iframe (an embed host) is
  // hidden when the page around it is, as the popup's per-site switch shows.
  let topHost = window === top ? location.hostname : "";
  const blockedNow = (list) =>
    HurtleUI.hostBlocked(list, location.hostname) || (!!topHost && HurtleUI.hostBlocked(list, topHost));
  Promise.all([
    api.storage.local.get(["overlay", "overlayOff", "preferredQuality"]),
    topHost
      ? null
      : api.runtime.sendMessage({ type: "top-host" }).then((r) => (topHost = r?.host || ""), () => {}),
  ]).then(([r]) => {
    enabled = r.overlay !== false;
    if (r.preferredQuality != null) preferred = Number(r.preferredQuality) || 0;
    siteOff = blockedNow(r.overlayOff);
    ready = true;
    if (siteOff || !enabled) hide();
  }, () => (ready = true));
  api.storage.onChanged.addListener((c, area) => {
    if (area !== "local") return;
    if (c.preferredQuality) preferred = Number(c.preferredQuality.newValue ?? 1080) || 0;
    if (c.overlay) {
      enabled = c.overlay.newValue !== false;
      if (!enabled) hide();
    }
    if (c.overlayOff) {
      siteOff = blockedNow(c.overlayOff.newValue);
      if (siteOff) hide();
    }
  });

  function pageTitle() {
    const og = document.querySelector('meta[property="og:title"], meta[name="twitter:title"]')?.content;
    // Single-page sites (YouTube Shorts) leave og:title stale; the document
    // title is current but carries an unread count: "(2) Title - YouTube".
    const spa = /(^|\.)youtube\.com$/.test(location.hostname);
    let t = ((spa ? document.title : og || document.title) || "").trim().replace(/^\(\d+\+?\)\s+/, "");
    if (spa) t = t.replace(/\s+-\s+YouTube$/i, "");
    // Drop " - YouTube", " | Vimeo"… when it matches the site name.
    const site = document.querySelector('meta[property="og:site_name"]')?.content;
    if (site) t = t.replace(new RegExp(`\\s*[-|–—·]\\s*${site.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\s*$`, "i"), "");
    return t;
  }

  function place() {
    if (!target || !target.isConnected) return hide();
    const r = target.getBoundingClientRect();
    if (r.width < MIN_W || r.height < MIN_H || r.bottom < 0 || r.top > innerHeight) return hide();
    host.style.display = "block";
    // The open menu is wider than the button: keep both on screen.
    const w = Math.max(button.getBoundingClientRect().width || 160, menu.hidden ? 0 : menu.getBoundingClientRect().width);
    const h = button.getBoundingClientRect().height || 30;
    host.style.left = `${Math.max(4, Math.min(r.right - w, innerWidth - w - 4))}px`;
    // Sit just above the video so it never covers the picture; only when
    // there's no room above (video at the top of the screen) tuck it inside.
    const above = r.top - h - 6;
    host.style.top = `${above >= 4 ? above : Math.max(4, r.top + 8)}px`;
  }

  // What yt-dlp says this page's video comes in, asked once per page URL
  // when the button first shows: { url, pending, info, empty }. `empty`: yt-dlp
  // found no video (a feed, channel or home page with hover previews) and no
  // file or stream was caught either, so the button has nothing to offer.
  let sizes = { url: "", pending: false, info: null, empty: false };
  let sniffed = 0; // media files/streams the background caught on this page
  let onSizes = null; // re-renders the open menu when the sizes arrive

  function fetchSizes() {
    if (sizes.url === location.href) return;
    sizes = { url: location.href, pending: true, info: null, empty: false };
    const asked = sizes;
    updateButton();
    api.runtime
      .sendMessage({ type: "page-sizes", pageUrl: location.href })
      .catch(() => null)
      .then(async (r) => {
        if (asked !== sizes) return; // the page moved on (single-page sites)
        const info = r?.ok && r.sizes?.length ? r : null;
        // The background's list, not `sniffed`: media caught before this
        // script loaded never sent us a count.
        const list = info ? null : await candidates().catch(() => []);
        if (asked !== sizes) return;
        sizes.pending = false;
        sizes.info = info;
        sizes.empty = !info && !list.length;
        if (sizes.empty) return hide();
        updateButton();
        onSizes?.();
      });
  }

  /** "Download video [4 sizes] ▾": how many sizes, and that a menu opens. */
  function updateButton() {
    const n = sizes.info?.sizes.length || 0;
    count.classList.toggle("busy", sizes.pending && !n);
    if (n) {
      count.textContent = n === 1 ? "1 size" : `${n} sizes`;
      count.title = `Available: ${sizes.info.sizes.map((z) => (z.width && z.height ? `${z.label} (${z.width}×${z.height})` : z.label)).join(", ")}`;
    } else if (sizes.pending) {
      count.textContent = "…";
      count.title = "Reading the available sizes";
    } else if (sniffed > 1) {
      count.textContent = `${sniffed} found`;
      count.title = `${sniffed} videos found on this page`;
    } else {
      count.textContent = "";
    }
    count.hidden = !count.textContent;
    // The caret says a menu opens: several sizes or videos to choose from.
    caret.hidden = !(n >= 2 || sniffed >= 2 || (sizes.pending && !sniffed));
    button.title = caret.hidden ? "Download with Hurtle" : "Choose a size to download with Hurtle";
    if (target) place();
  }

  function show(el) {
    if (!ready || dismissed || !enabled || siteOff) return;
    fetchSizes(); // first: a new URL on a single-page site starts over
    if (sizes.empty && !/^https?:/i.test(srcOf(el))) return;
    clearTimeout(hideTimer);
    if (target !== el) menu.hidden = true;
    target = el;
    if (!host.isConnected) document.documentElement.appendChild(host);
    place();
  }

  function hide() {
    host.style.display = "none";
    menu.hidden = true;
    onSizes = null;
    target = null;
  }

  function hideSoon() {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
      if (menu.hidden) hide();
    }, 1800);
  }

  function download(media, quality) {
    menu.hidden = true;
    api.runtime.sendMessage({ type: "download-media", media, quality, thumb: pageImage(target), title: pageTitle(), pageUrl: location.href });
  }

  function downloadPage(quality) {
    menu.hidden = true;
    api.runtime.sendMessage({ type: "download-page", quality, thumb: pageImage(target), title: pageTitle(), pageUrl: location.href });
  }

  /** The page's own picture for it: the video's poster, else og:image. */
  function pageImage(video) {
    const og = document.querySelector('meta[property="og:image"], meta[name="twitter:image"]')?.content;
    const src = video?.poster || og || "";
    try {
      return src ? new URL(src, location.href).href : "";
    } catch {
      return "";
    }
  }

  const srcOf = (v) => v?.currentSrc || v?.src || "";

  async function candidates() {
    const list = (await api.runtime.sendMessage({ type: "media-list" }).catch(() => null)) || [];
    const src = target?.currentSrc || target?.src || "";
    if (/^https?:/i.test(src) && !list.some((m) => m.url === src)) {
      list.unshift({ url: src, kind: "file", mime: null, size: null });
    }
    // The playing video first, then files biggest first (usually the actual
    // video, not a preview), then streams.
    const rank = (m) => (m.url === src ? 0 : m.kind === "file" ? 1 : 2);
    return list.sort((a, b) => rank(a) - rank(b) || (b.size || 0) - (a.size || 0));
  }

  close.addEventListener("click", (e) => {
    e.preventDefault();
    e.stopPropagation();
    dismissed = true;
    hide();
  });

  button.addEventListener("click", async (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (!menu.hidden) {
      menu.hidden = true;
      onSizes = null;
      return place();
    }
    const list = await candidates();
    const files = list.filter((m) => m.kind === "file");
    const choices = sizes.info?.sizes.length || 0;
    // One file and no sizes to pick from: straight to its dialog.
    if (choices < 2 && files.length === 1 && list.length === 1) return download(files[0]);
    // Nothing found at all: let yt-dlp read the page in the dialog.
    if (!list.length && !sizes.pending && !choices) return downloadPage();
    renderMenu(list);
  });

  function renderMenu(list) {
    menu.replaceChildren();
    const title = pageTitle() || location.hostname;
    const current = srcOf(target);
    const SUI = HurtleUI;
    const info = sizes.info;
    const page = SUI.row({
      title: info?.title || title,
      sub: info
        ? SUI.sizesLine(info)
        : sizes.pending
          ? "Reading the available sizes…"
          : "Choose quality and audio language · via yt-dlp",
      file: [location.hostname, info?.site].filter(Boolean).join(" · "),
      thumbOpts: { live: target, image: pageImage(target) || info?.thumbnail },
      chips: info ? SUI.sizeChips(info.sizes, preferred) : null,
      onClick: () => downloadPage(),
      onChip: (q) => downloadPage(q),
    });
    // Sizes still being read: fill them in when they arrive.
    onSizes = sizes.pending ? () => !menu.hidden && renderMenu(list) : null;
    const head = (text) => {
      const h = document.createElement("div");
      h.className = "head";
      h.textContent = text;
      return h;
    };
    // yt-dlp couldn't read the page: only the caught files and streams can download.
    if (info || sizes.pending) menu.append(head("This page"), page.el);
    if (list.length) menu.append(head(list.length === 1 ? "Found 1 video" : `Found ${list.length} videos`));
    for (const m of list.slice(0, 12)) {
      const mine = m.url === current;
      const stream = m.kind === "stream";
      const r = SUI.row({
        title: mine || stream ? title : SUI.fileName(m.url),
        badge: mine ? "Playing" : null,
        sub: SUI.details(m, mine ? { w: target.videoWidth, h: target.videoHeight, duration: target.duration } : {}),
        file: mine || stream ? SUI.fileName(m.url) : null,
        // The playing video shows its own frame; others load a metadata-only preview.
        thumbOpts: mine
          ? { live: target }
          : stream
            ? { image: pageImage(target) }
            : { previewUrl: m.url, onMeta: (meta) => r.setSub(SUI.details(m, meta)) },
        chips: stream && m.qualities?.length > 1 ? SUI.heightChips(m.qualities, preferred) : null,
        onClick: () => download(m),
        onChip: (q) => download(m, q),
      });
      menu.append(r.el);
    }
    for (const i of menu.querySelectorAll("button.item")) i.setAttribute("role", "menuitem");
    const off = document.createElement("button");
    off.type = "button";
    off.className = "off";
    off.textContent = `Don't show on ${topHost || location.hostname}`;
    off.title = "Hide this button on this site; bring it back from the Hurtle toolbar button";
    off.addEventListener("click", async (e) => {
      e.preventDefault();
      e.stopPropagation();
      const cur = (await api.storage.local.get("overlayOff").catch(() => ({}))).overlayOff;
      const list = Array.isArray(cur) ? cur : [];
      // The site you see (the popup lists it), not an embedded player's host.
      const site = topHost || location.hostname;
      if (!HurtleUI.hostBlocked(list, site)) list.push(site);
      siteOff = true;
      hide();
      api.storage.local.set({ overlayOff: list }).catch(() => {});
    });
    menu.append(off);
    menu.hidden = false;
    place();
  }

  host.addEventListener("mouseenter", () => clearTimeout(hideTimer));
  host.addEventListener("mouseleave", hideSoon);

  // Hovering any video/audio element (or something layered over it) shows the button.
  document.addEventListener(
    "mouseover",
    (e) => {
      if (e.composedPath().includes(host)) return;
      const el = e.target?.closest?.("video, audio") || mediaUnder(e.clientX, e.clientY);
      if (el) show(el);
      else if (target) hideSoon();
    },
    true,
  );

  // Players often put a transparent layer over the <video>; look underneath it.
  function mediaUnder(x, y) {
    for (const v of document.querySelectorAll("video")) {
      const r = v.getBoundingClientRect();
      if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) return v;
    }
    return null;
  }

  addEventListener("scroll", () => target && place(), { passive: true, capture: true });
  addEventListener("resize", () => target && place(), { passive: true });
  document.addEventListener("fullscreenchange", () => {
    // Re-parent into the fullscreen element so the button stays visible.
    const fs = document.fullscreenElement;
    (fs && fs.tagName !== "VIDEO" ? fs : document.documentElement).appendChild(host);
    if (target) place();
  });

  api.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
    if (msg?.type === "media-updated") {
      sniffed = msg.count || 0;
      if (sniffed) sizes.empty = false; // something to offer after all
      updateButton();
    }
    // Only a frame that hid its button answers, so any one of them is enough.
    if (msg?.type === "overlay-state" && dismissed) sendResponse({ dismissed });
    // The popup asks what's on the page: title, picture, and each video's
    // size and length (it can't see the page's <video> elements itself).
    if (msg?.type === "page-info" && window === top) {
      const videos = [...document.querySelectorAll("video")]
        .filter((v) => /^https?:/i.test(srcOf(v)) || v.poster)
        .map((v) => ({ src: srcOf(v), poster: v.poster ? new URL(v.poster, location.href).href : "", w: v.videoWidth, h: v.videoHeight, duration: Number.isFinite(v.duration) ? v.duration : 0 }));
      sendResponse({ title: pageTitle(), image: pageImage(document.querySelector("video")), videos });
    }
    if (msg?.type === "overlay-reset") {
      dismissed = false;
      sendResponse({ ok: true });
    }
  });
})();
