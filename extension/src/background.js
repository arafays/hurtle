// Hurtle browser extension (background).
//  - Catches browser downloads and hands them to the Hurtle app.
//  - Sniffs media responses (video/audio files) per tab, like IDM's
//    "Download this video" panel.
//  - Opens the "Download file info" dialog (rename, connections, speed limit).
// Runs as a service worker in Chrome and an event page in Firefox; all
// state lives in storage.session so it survives the worker being unloaded.

const api = globalThis.browser ?? globalThis.chrome;
const HOST = "io.github.arafays.hurtle";
const DEFAULTS = {
  enabled: true, // catch browser downloads
  showDialog: true, // ask before each download (IDM's "Download file info")
  minSizeMB: 0, // leave smaller files to the browser
  skipExtensions: "", // e.g. "pdf jpg" stays in the browser
  sniffMedia: true, // detect video/audio on pages
};

const MEDIA_EXT = /\.(mp4|m4v|webm|mkv|mov|avi|flv|wmv|3gp|mp3|m4a|aac|ogg|oga|opus|wav|flac)(?:$|[?#])/i;
const STREAM_EXT = /\.(m3u8|mpd)(?:$|[?#])/i;
// Pieces of segmented streams: useless on their own.
const SEGMENT = /\.(ts|m4s|aac\.ts)(?:$|[?#])|[?&](range|rn|rbuf|sq)=|\/range\/|\/seg-?\d+|videoplayback/i;
const MIN_MEDIA_BYTES = 256 * 1024;
// Manifests the player builds from signed, per-session URLs. Downloading them
// directly picks a random audio dub and names the file after the manifest;
// yt-dlp reading the page gets it right.
const PAGE_ONLY = /(^|\.)(googlevideo\.com|youtube\.com|youtube-nocookie\.com)$/i;

const MIME_EXT = {
  "video/mp4": "mp4", "video/webm": "webm", "video/x-matroska": "mkv", "video/quicktime": "mov",
  "video/x-flv": "flv", "video/3gpp": "3gp", "video/ogg": "ogv", "audio/mpeg": "mp3",
  "audio/mp4": "m4a", "audio/aac": "aac", "audio/ogg": "ogg", "audio/opus": "opus",
  "audio/webm": "weba", "audio/wav": "wav", "audio/flac": "flac",
};

// ---------- helpers ----------

async function settings() {
  return { ...DEFAULTS, ...(await api.storage.local.get(Object.keys(DEFAULTS))) };
}

/** What happened to each hand-off, newest first; the popup's History tab. */
async function setStatus(ok, message) {
  const entry = { at: Date.now(), ok, message };
  const { history } = await api.storage.local.get("history");
  const next = [entry, ...(Array.isArray(history) ? history : [])].slice(0, 50);
  await api.storage.local.set({ status: entry, history: next });
}

function basename(path) {
  return (path || "").split(/[\\/]/).pop();
}

function extOf(url, mime) {
  try {
    const m = new URL(url).pathname.match(/\.([a-z0-9]{2,5})$/i);
    if (m) return m[1].toLowerCase();
  } catch {}
  return MIME_EXT[(mime || "").split(";")[0].trim()] || "";
}

function sanitize(name) {
  return (name || "")
    .replace(/[\\/:*?"<>|\u0000-\u001f]+/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .replace(/^\.+/, "")
    .slice(0, 180);
}

/** "Video title" + ".mp4", unless the title already ends in that extension. */
function titleFilename(title, url, mime) {
  const ext = extOf(url, mime);
  const base = sanitize(title);
  if (!base) return "";
  return ext && !base.toLowerCase().endsWith("." + ext) ? `${base}.${ext}` : base;
}

async function cookieHeader(url, storeId) {
  try {
    const cookies = await api.cookies.getAll(storeId ? { url, storeId } : { url });
    return cookies.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch (e) {
    dlog("cookies unavailable", { url: safeUrl(url), error: String(e?.message || e) }, "error");
    return "";
  }
}

// ---------- debug log ----------
// The last 400 lines live in storage.local ("debugLog"); the popup's
// "Debug log" page shows them. Errors and calls to Hurtle are always kept.
// With the "Detailed debug log" switch on, so is everything else, and the
// lines also go to Hurtle's own log file ($HURTLE_DATA/logs/hurtle.log).
const LOG_MAX = 400;
let logQueue = Promise.resolve();
let hostBatch = [];
let hostTimer = 0;

/** Origin + path only: queries often carry tokens. */
function safeUrl(u) {
  try {
    const x = new URL(u);
    return x.origin + x.pathname + (x.search ? "?…" : "");
  } catch {
    return String(u || "");
  }
}

function fmtData(data) {
  const parts = Object.entries(data || {})
    .filter(([, v]) => v !== undefined && v !== null && v !== "")
    .map(([k, v]) => `${k}=${typeof v === "string" ? JSON.stringify(v.length > 300 ? v.slice(0, 300) + "…" : v) : JSON.stringify(v)}`);
  return parts.length ? " " + parts.join(" ") : "";
}

/** level: "error" and "info" are always kept, "debug" only when enabled. */
function dlog(event, data, level = "info") {
  logQueue = logQueue
    .then(async () => {
      const { debug = false, debugLog = [] } = await api.storage.local.get(["debug", "debugLog"]);
      if (level === "debug" && !debug) return;
      const line = `${new Date().toISOString()} ${level.toUpperCase().padEnd(5)} ${event}${fmtData(data)}`;
      debugLog.push(line);
      await api.storage.local.set({ debugLog: debugLog.slice(-LOG_MAX) });
      if (debug) {
        hostBatch.push(line);
        clearTimeout(hostTimer);
        hostTimer = setTimeout(flushToHost, 2000);
      }
    })
    .catch(() => {});
}

/** Append the pending lines to Hurtle's log file (not itself logged). */
async function flushToHost() {
  const append = hostBatch.splice(0);
  if (append.length) await api.runtime.sendNativeMessage(HOST, { type: "log", append }).catch(() => {});
}

self.addEventListener?.("error", (e) => dlog("background error", { message: e.message, at: `${e.filename}:${e.lineno}` }, "error"));
self.addEventListener?.("unhandledrejection", (e) => dlog("background unhandled rejection", { reason: String(e.reason?.message || e.reason) }, "error"));

async function native(msg) {
  const t = Date.now();
  const what = `native ${msg.type}${msg.url ? " " + safeUrl(msg.url) : ""}`;
  const quiet = msg.type === "ping" || msg.type === "settings";
  try {
    const r = await api.runtime.sendNativeMessage(HOST, msg);
    const data = { ms: Date.now() - t, ok: r?.ok, error: r?.error, formats: r?.media?.choices?.length, site: r?.media?.site };
    dlog(what, data, r?.ok ? (quiet ? "debug" : "info") : "error");
    return r;
  } catch (e) {
    dlog(what, { ms: Date.now() - t, threw: String(e?.message || e) }, "error");
    throw e;
  }
}

/** Ask yt-dlp (via the host, without starting the app) what a page offers. */
// yt-dlp's answers per URL for a few minutes: the overlay asks when you
// hover a video, and the dialog then opens with the qualities already there.
const probeCache = new Map(); // url → { at, promise }
const PROBE_TTL = 10 * 60 * 1000;

function probeMedia(p, { reuseErrors = false } = {}) {
  const hit = probeCache.get(p.url);
  if (hit && Date.now() - hit.at < PROBE_TTL && (reuseErrors || !hit.failed)) return hit.promise;
  const entry = { at: Date.now(), failed: false };
  entry.promise = (async () =>
    native({
      type: "probe",
      url: p.url,
      referrer: p.referrer || null,
      cookies: (await cookieHeader(p.url, p.cookieStoreId)) || null,
      userAgent: navigator.userAgent,
    }))().then(
    (r) => {
      entry.failed = !r?.ok;
      return r;
    },
    (e) => {
      entry.failed = true;
      throw e;
    },
  );
  probeCache.set(p.url, entry);
  for (const [k, v] of probeCache) if (Date.now() - v.at > PROBE_TTL) probeCache.delete(k);
  return entry.promise;
}

/**
 * The sizes a page's video comes in, for the overlay button ("4 sizes ▾"):
 * yt-dlp's video choices with resolution and approximate size.
 */
async function pageSizes(pageUrl, cookieStoreId) {
  if (!/^https?:/i.test(pageUrl || "")) return { ok: false };
  const r = await probeMedia({ url: pageUrl, referrer: pageUrl, cookieStoreId }, { reuseErrors: true });
  if (!r?.ok || !r.media) return { ok: false, error: r?.error };
  const sizes = r.media.choices
    .filter((c) => !c.audioOnly && /^\d+p/.test(c.label))
    .map((c) => ({ label: c.label, quality: Number(/^(\d+)p/.exec(c.label)[1]), width: c.width, height: c.height, size: c.size }));
  return { ok: true, site: r.media.site, title: r.media.title, duration: r.media.duration, thumbnail: r.media.thumbnail, sizes, audio: r.media.audio?.length || 0 };
}

/** Send one download to the Hurtle app. */
async function sendToHurtle(p) {
  const msg = {
    type: "add",
    url: p.url,
    referrer: p.referrer || null,
    cookies: (await cookieHeader(p.url, p.cookieStoreId)) || null,
    userAgent: navigator.userAgent,
    connections: p.connections || null,
    speedLimit: p.speedLimit || null,
    paused: !!p.paused,
    media: !!p.media,
    format: p.format || null,
    audioLang: p.audioLang || null,
    container: p.container || null,
    // Only when chosen in the dialog; otherwise the file's type picks the folder.
    dir: p.dir || null,
    // Where it was found, to go back for a fresh link. With the size, a new
    // link to a paused or failed download resumes it (the reply says `resumed`).
    pageUrl: p.sourcePage || p.pageUrl || p.referrer || null,
    size: p.size > 0 ? p.size : null,
  };
  const name = sanitize(basename(p.filename));
  if (name) msg.filename = name;
  const reply = await native(msg);
  if (!reply?.ok) throw new Error(reply?.error || "Hurtle rejected the download");
  return reply;
}

function badge(text, color, tabId) {
  const action = api.action ?? api.browserAction;
  const opts = tabId != null && tabId >= 0 ? { tabId } : {};
  action.setBadgeBackgroundColor({ color, ...opts });
  action.setBadgeText({ text, ...opts });
}

function flashBadge(text, color) {
  badge(text, color);
  setTimeout(() => badge("", "#000"), 2500);
}

// ---------- the "Download file info" dialog ----------

async function openDialog(pending) {
  dlog("dialog open", { url: safeUrl(pending.url), media: !!pending.media, file: pending.filename, quality: pending.quality, pageUrl: pending.pageUrl && safeUrl(pending.pageUrl) });
  const key = `pending:${Date.now()}:${Math.random().toString(36).slice(2, 8)}`;
  // Stored before the window exists: the dialog reads it as soon as it loads.
  await api.storage.session.set({ [key]: { ...pending, windowId: null } });
  const win = await api.windows.create({
    url: api.runtime.getURL(`dialog.html#${encodeURIComponent(key)}`),
    type: "popup",
    width: 520,
    // The layout scrolls and keeps its buttons in view at any size; these are
    // just comfortable: quality picker (video pages), preview row (video files).
    height: pending.media ? 600 : /^(video|audio)\//.test(pending.mime || "") ? 540 : 470,
  });
  const cur = (await api.storage.session.get(key))[key];
  if (cur) await api.storage.session.set({ [key]: { ...cur, windowId: win.id } });
}

/** What the dialog's buttons do. */
async function resolvePending(key, action, choice = {}) {
  const got = await api.storage.session.get(key);
  const p = got[key];
  dlog(`dialog ${action}`, { url: p && safeUrl(p.url), found: !!p, file: choice.filename, dir: choice.dir, format: choice.format, audio: choice.audioLang, container: choice.container });
  if (!p) return { ok: false, error: "This download request expired." };
  await api.storage.session.remove(key);
  const dl = p.browserDownloadId;

  if (action === "browser") {
    if (dl != null) {
      const ok = await api.downloads.resume(dl).then(() => true, () => false);
      if (!ok) await restartInBrowser(p.url, dl);
    } else {
      await restartInBrowser(p.url, null, choice.filename || p.filename);
    }
    return { ok: true };
  }
  if (action === "cancel") {
    if (dl != null) await dropBrowserDownload(dl);
    return { ok: true };
  }
  // start / later
  try {
    const reply = await sendToHurtle({ ...p, ...choice, paused: action === "later" });
    if (dl != null) await dropBrowserDownload(dl);
    const verb = reply.resumed ? (action === "later" ? "New link for" : "Resumed") : "Sent";
    await setStatus(true, `${verb} ${choice.filename || p.filename || p.url}`);
    flashBadge("✓", "#16a34a");
    return reply;
  } catch (e) {
    // Put it back so the dialog can show the error and retry.
    await api.storage.session.set({ [key]: p });
    await setStatus(false, String(e?.message || e));
    return { ok: false, error: String(e?.message || e) };
  }
}

async function dropBrowserDownload(id) {
  await api.downloads.cancel(id).catch(() => {});
  await api.downloads.erase({ id }).catch(() => {});
}

// URLs handed back to the browser; don't intercept them again.
async function restartInBrowser(url, oldId, filename) {
  if (oldId != null) await dropBrowserDownload(oldId);
  const { passThrough = [] } = await api.storage.session.get("passThrough");
  await api.storage.session.set({ passThrough: [...passThrough, url] });
  const opts = { url };
  if (filename) opts.filename = sanitize(basename(filename));
  await api.downloads.download(opts).catch(() => {});
}

/**
 * Formats for the dialog. A stream sniffed on a page yt-dlp knows (Vimeo,
 * Twitter, …) is better fetched through the page: real title, every audio
 * language, separate best video + audio. Only a generic page falls back to
 * the raw manifest.
 */
const GENERIC_SITE = /^(generic|html5mediaembed)$/i;

async function dialogProbe(key) {
  const p = (await api.storage.session.get(key))[key];
  if (!p) return { ok: false, error: "This download request expired." };
  const pageFirst = p.pageUrl && p.pageUrl !== p.url && /^https?:/i.test(p.pageUrl);
  if (!pageFirst) return probeMedia(p);
  // Both at once: yt-dlp can take a while on either, and waiting for one
  // before starting the other doubled the time the dialog showed nothing.
  const pageProbe = probeMedia({ ...p, url: p.pageUrl }).catch(() => null);
  const streamProbe = probeMedia(p).catch((e) => ({ ok: false, error: String(e?.message || e) }));
  const r = await pageProbe;
  const switched = !!(r?.ok && r.media?.site && !GENERIC_SITE.test(r.media.site));
  dlog("probe page first", { page: safeUrl(p.pageUrl), site: r?.media?.site, error: r?.error, switched });
  if (switched) {
    const cur = (await api.storage.session.get(key))[key];
    if (cur) await api.storage.session.set({ [key]: { ...cur, url: p.pageUrl, pageUrl: null } });
    return { ...r, url: p.pageUrl };
  }
  return streamProbe;
}

// Closing the dialog window = cancel (as in IDM).
api.windows.onRemoved.addListener(async (windowId) => {
  const all = await api.storage.session.get(null);
  for (const [key, p] of Object.entries(all)) {
    if (key.startsWith("pending:") && p.windowId === windowId) {
      await resolvePending(key, "cancel");
    }
  }
});

// ---------- catching browser downloads ----------

api.downloads.onCreated.addListener(async (item) => {
  const url = item.finalUrl || item.url;
  if (!/^https?:/i.test(url) || item.incognito || item.state !== "in_progress") return;
  if (item.byExtensionId && item.byExtensionId === api.runtime.id) {
    // Our own "let the browser do it" restart.
    const { passThrough = [] } = await api.storage.session.get("passThrough");
    if (passThrough.includes(url)) {
      await api.storage.session.set({ passThrough: passThrough.filter((u) => u !== url) });
      return;
    }
  }

  const s = await settings();
  if (!s.enabled) return;
  if (s.minSizeMB > 0 && item.totalBytes > 0 && item.totalBytes < s.minSizeMB * 1024 * 1024) return;
  const skip = s.skipExtensions.toLowerCase().split(/[\s,]+/).filter(Boolean);
  if (skip.length && skip.includes(extOf(url, item.mime))) return;

  dlog("browser download caught", { url: safeUrl(url), mime: item.mime, size: item.totalBytes });
  // Hold the browser's copy while Hurtle (or the user) decides.
  await api.downloads.pause(item.id).catch(() => {});
  const pending = {
    url,
    filename: basename(item.filename),
    size: item.totalBytes > 0 ? item.totalBytes : item.fileSize > 0 ? item.fileSize : null,
    mime: item.mime || null,
    referrer: item.referrer || null,
    browserDownloadId: item.id,
    cookieStoreId: item.cookieStoreId,
  };
  if (s.showDialog) {
    await openDialog(pending);
    return;
  }
  try {
    const reply = await sendToHurtle(pending);
    await dropBrowserDownload(item.id);
    await setStatus(true, `${reply.resumed ? "Resumed" : "Sent"} ${pending.filename || url}`);
    flashBadge("✓", "#16a34a");
  } catch (e) {
    await setStatus(false, String(e?.message || e));
    flashBadge("!", "#dc2626");
    const resumed = await api.downloads.resume(item.id).then(() => true, () => false);
    if (!resumed) await restartInBrowser(url, item.id);
  }
});

// ---------- media sniffing ----------

function header(headers, name) {
  const h = headers?.find((x) => x.name.toLowerCase() === name);
  return h ? h.value : "";
}

async function tabMedia(tabId) {
  const key = `media:${tabId}`;
  return (await api.storage.session.get(key))[key] || [];
}

async function setTabMedia(tabId, list) {
  await api.storage.session.set({ [`media:${tabId}`]: list });
  const files = list.filter((m) => m.kind === "file").length;
  badge(files ? String(files) : "", "#0284c7", tabId);
  api.tabs.sendMessage(tabId, { type: "media-updated", count: list.length }).catch(() => {});
}

// Serialize updates so concurrent responses don't clobber each other.
let mediaQueue = Promise.resolve();

api.webRequest.onHeadersReceived.addListener(
  (d) => {
    if (d.tabId < 0 || d.statusCode >= 400 || d.method !== "GET") return;
    const mime = header(d.responseHeaders, "content-type").toLowerCase();
    const isStream =
      STREAM_EXT.test(d.url) || /mpegurl|dash\+xml/.test(mime);
    const isFile =
      !isStream && (/^(video|audio)\//.test(mime) || MEDIA_EXT.test(d.url)) && !/mp2t/.test(mime);
    if (!isStream && !isFile) return;
    if (isFile && SEGMENT.test(d.url)) return;
    try {
      if (PAGE_ONLY.test(new URL(d.url).hostname)) return;
    } catch {}

    const range = header(d.responseHeaders, "content-range");
    const total = range.includes("/") ? Number(range.split("/").pop()) : NaN;
    const size = Number.isFinite(total) ? total : Number(header(d.responseHeaders, "content-length")) || null;
    if (isFile && size != null && size < MIN_MEDIA_BYTES) return;

    mediaQueue = mediaQueue.then(async () => {
      if (!(await settings()).sniffMedia) return;
      const list = await tabMedia(d.tabId);
      if (list.some((m) => m.url === d.url)) return;
      const tab = await api.tabs.get(d.tabId).catch(() => null);
      dlog("sniffed", { kind: isStream ? "stream" : "file", url: safeUrl(d.url), mime, size, tab: d.tabId }, "debug");
      list.push({
        url: d.url,
        mime: mime.split(";")[0] || null,
        size,
        kind: isStream ? "stream" : "file",
        pageUrl: tab?.url || d.initiator || d.originUrl || null,
        pageTitle: tab?.title || "",
        at: Date.now(),
      });
      await setTabMedia(d.tabId, list.slice(-40));
      if (isStream) readManifest(d.tabId, d.url);
    });
  },
  { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "object", "other"] },
  ["responseHeaders"],
);

// ---------- what a stream offers ----------

/** Fetch a small text resource (a manifest), giving up after a few seconds. */
async function fetchText(url) {
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), 6000);
  try {
    const r = await fetch(url, { credentials: "include", signal: ctl.signal, cache: "force-cache" });
    if (!r.ok) return null;
    const text = await r.text();
    return text.length < 2_000_000 ? text : null;
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

/** ISO 8601 duration ("PT1H2M3.5S") → seconds. */
function isoSeconds(d) {
  const m = /P(?:(\d+)D)?T?(?:(\d+)H)?(?:(\d+)M)?(?:([\d.]+)S)?/.exec(d || "");
  return m ? (+m[1] || 0) * 86400 + (+m[2] || 0) * 3600 + (+m[3] || 0) * 60 + (+m[4] || 0) : 0;
}

/** Quality steps (short side, "720" for 1280x720 and 720x1280), highest first. */
function steps(sizes) {
  const q = sizes.map(([w, h]) => Math.min(w, h) || h).filter((n) => n > 0);
  return [...new Set(q)].sort((a, b) => b - a);
}

/**
 * Read an HLS or DASH manifest: the qualities it offers, its length, and
 * whether it's live. Pure text parsing, so it works in a service worker.
 */
async function manifestInfo(url, depth = 0) {
  const text = await fetchText(url);
  if (!text) return null;
  if (/^\s*#EXTM3U/.test(text)) {
    const variants = [];
    const lines = text.split(/\r?\n/);
    let firstVariant = null;
    for (let i = 0; i < lines.length; i++) {
      if (!lines[i].startsWith("#EXT-X-STREAM-INF:")) continue;
      const res = /RESOLUTION=(\d+)x(\d+)/i.exec(lines[i]);
      if (res) variants.push([+res[1], +res[2]]);
      const uri = lines.slice(i + 1).find((l) => l.trim() && !l.startsWith("#"));
      if (uri && !firstVariant) firstVariant = new URL(uri.trim(), url).href;
    }
    if (firstVariant) {
      // A master playlist: its length is in any of the variants.
      const sub = depth ? null : await manifestInfo(firstVariant, depth + 1);
      return { qualities: steps(variants), duration: sub?.duration || 0, live: !!sub?.live };
    }
    const duration = [...text.matchAll(/#EXTINF:([\d.]+)/g)].reduce((a, m) => a + Number(m[1]), 0);
    const live = !/#EXT-X-ENDLIST/.test(text) && !/#EXT-X-PLAYLIST-TYPE:VOD/.test(text);
    return { qualities: [], duration: live ? 0 : duration, live };
  }
  if (/<MPD[\s>]/.test(text)) {
    const sizes = [...text.matchAll(/<Representation\b[^>]*>/g)].map((m) => [
      +(/\swidth="(\d+)"/.exec(m[0])?.[1] || 0),
      +(/\sheight="(\d+)"/.exec(m[0])?.[1] || 0),
    ]);
    return {
      qualities: steps(sizes),
      duration: isoSeconds(/mediaPresentationDuration="([^"]+)"/.exec(text)?.[1]),
      live: /\btype="dynamic"/.test(text),
    };
  }
  return null;
}

async function readManifest(tabId, url) {
  const info = await manifestInfo(url).catch(() => null);
  if (!info) return;
  mediaQueue = mediaQueue.then(async () => {
    const list = await tabMedia(tabId);
    const m = list.find((x) => x.url === url);
    if (!m) return;
    Object.assign(m, info);
    await api.storage.session.set({ [`media:${tabId}`]: list });
  });
}

// New page in the tab → forget its old media.
// Queued with the sniffer, and keeping what the new page itself already
// loaded: the event can arrive after the page's first media responses.
api.tabs.onUpdated.addListener((tabId, change) => {
  if (change.status !== "loading" || !change.url) return;
  mediaQueue = mediaQueue.then(async () => {
    const list = await tabMedia(tabId);
    await setTabMedia(tabId, list.filter((m) => m.pageUrl === change.url));
  });
});
api.tabs.onRemoved.addListener((tabId) => api.storage.session.remove(`media:${tabId}`));

// ---------- context menu ----------

api.runtime.onInstalled.addListener(async () => {
  // Promise form works in both Chrome (MV3) and Firefox.
  await api.contextMenus.removeAll();
  api.contextMenus.create({
    id: "hurtle-download",
    title: "Download with Hurtle",
    contexts: ["link", "video", "audio", "image"],
  });
});

api.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId !== "hurtle-download") return;
  const url = info.linkUrl || info.srcUrl;
  if (!url || !/^https?:/i.test(url)) return;
  const isMedia = info.mediaType === "video" || info.mediaType === "audio";
  await openDialog({
    url,
    filename: isMedia ? titleFilename(tab?.title, url) : "",
    referrer: info.pageUrl || tab?.url,
    sourcePage: tab?.url || info.pageUrl || null,
    cookieStoreId: tab?.cookieStoreId,
  });
});

// ---------- messages from popup / content script / dialog ----------

api.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  const reply = (p) => {
    p.then(sendResponse, (e) => sendResponse({ ok: false, error: String(e?.message || e) }));
    return true; // async
  };
  if (msg?.type && !/^debug-/.test(msg.type)) {
    dlog(`message ${msg.type}`, { from: sender.tab ? `tab ${sender.tab.id}` : sender.url?.split("/").pop()?.split("#")[0] }, "debug");
  }
  switch (msg?.type) {
    case "debug-log":
      dlog(msg.event, msg.data, msg.level || "info");
      return reply(logQueue.then(() => ({ ok: true })));
    case "debug-read":
      return reply(logQueue.then(() => api.storage.local.get(["debug", "debugLog"])));
    case "debug-clear":
      return reply((logQueue = logQueue.then(() => api.storage.local.set({ debugLog: [] }))).then(() => ({ ok: true })));
    case "debug-host-log":
      // Hurtle's own log file (host, yt-dlp and app lines), read by the host.
      return reply(flushToHost().then(() => api.runtime.sendNativeMessage(HOST, { type: "log", tail: msg.tail || 200000 })));
    case "ping":
    case "show":
      return reply(native({ type: msg.type }));
    case "app-settings":
      // Works with the app closed too: the host then edits settings.json.
      return reply(
        native({
          type: "settings",
          connections: msg.connections ?? null,
          speedLimit: msg.speedLimit ?? null,
          downloadDir: msg.downloadDir ?? null,
          typeDirs: msg.typeDirs ?? null,
        }),
      );
    case "pick-folder":
      // The app's system folder picker (starts the app if needed).
      return reply(native({ type: "pickFolder", title: msg.title || null }));
    case "media-list": {
      const tabId = msg.tabId ?? sender.tab?.id;
      return reply(tabMedia(tabId));
    }
    case "download-media": {
      // From the page overlay or popup: open the dialog named after the page title.
      // HLS/DASH playlists go through yt-dlp (quality picker); files download directly.
      const tab = sender.tab;
      const m = msg.media;
      const stream = m.kind === "stream";
      return reply(
        openDialog({
          url: m.url,
          media: stream,
          filename: stream
            ? sanitize(msg.title || m.pageTitle || tab?.title)
            : titleFilename(msg.title || m.pageTitle || tab?.title, m.url, m.mime),
          size: m.size,
          mime: m.mime,
          quality: msg.quality || null,
          duration: m.duration || null,
          thumb: msg.thumb || null,
          referrer: msg.pageUrl || m.pageUrl || tab?.url || null,
          pageUrl: stream ? msg.pageUrl || m.pageUrl || tab?.url || null : null,
          sourcePage: msg.pageUrl || m.pageUrl || tab?.url || null,
          cookieStoreId: tab?.cookieStoreId,
        }).then(() => ({ ok: true })),
      );
    }
    case "download-page": {
      // "Video on this page": yt-dlp resolves the page itself (YouTube & co).
      // The title is only a placeholder; the probe's title replaces it
      // (single-page sites often leave a stale document title).
      const tab = sender.tab;
      const url = msg.pageUrl || tab?.url;
      if (!url || !/^https?:/i.test(url)) return reply(Promise.resolve({ ok: false, error: "Not a web page" }));
      return reply(
        openDialog({
          url,
          media: true,
          filename: sanitize(msg.title || tab?.title),
          quality: msg.quality || null,
          thumb: msg.thumb || null,
          referrer: url,
          cookieStoreId: msg.cookieStoreId ?? tab?.cookieStoreId,
        }).then(() => ({ ok: true })),
      );
    }
    case "page-sizes":
      return reply(pageSizes(msg.pageUrl || sender.tab?.url, msg.cookieStoreId ?? sender.tab?.cookieStoreId));
    case "dialog-probe":
      return reply(dialogProbe(msg.key));
    case "dialog-load":
      return reply(api.storage.session.get(msg.key).then((r) => r[msg.key] || null));
    case "dialog-resolve":
      return reply(resolvePending(msg.key, msg.action, msg.choice));
  }
});
