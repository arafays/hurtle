const api = globalThis.browser ?? globalThis.chrome;
const $ = (id) => document.getElementById(id);
const key = decodeURIComponent(location.hash.slice(1));

/** Into the extension's debug log (popup → "Debug log"). */
function report(event, data, level = "info") {
  api.runtime.sendMessage({ type: "debug-log", event: `dialog: ${event}`, data, level }).catch(() => {});
}
addEventListener("error", (e) => report("script error", { message: e.message, at: `${e.filename}:${e.lineno}` }, "error"));
addEventListener("unhandledrejection", (e) => report("unhandled rejection", { reason: String(e.reason?.message || e.reason) }, "error"));

const LIMITS = [
  [0, "Unlimited"], [128, "128 KB/s"], [256, "256 KB/s"], [512, "512 KB/s"], [1024, "1 MB/s"],
  [2048, "2 MB/s"], [5120, "5 MB/s"], [10240, "10 MB/s"], [20480, "20 MB/s"],
];

function fmtSize(n) {
  if (!n) return "unknown";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) (n /= 1024), i++;
  return `${n.toFixed(i ? 1 : 0)} ${u[i]}`;
}

// ---- where it's saved: the folder for this file's type, unless changed here
let settings = null;
let dirChosen = false; // typed or picked in this dialog: sent as this download's folder
let isMedia = false;

function fileCategory() {
  if (isMedia) {
    const c = qualities?.[$("quality").value];
    return HurtleFolders.mediaCategory(c?.format, $("container").value || (c?.audioOnly ? "m4a" : ""));
  }
  return HurtleFolders.category($("filename").value);
}

/** Show the folder this download will go to (until the user picks one). */
function updateDir() {
  if (dirChosen || !settings) return;
  const cat = fileCategory();
  const own = cat && settings.type_dirs?.[cat];
  $("dir").value = HurtleFolders.short(HurtleFolders.dirFor(settings, cat));
  const label = HurtleFolders.CATEGORIES.find(([id]) => id === cat)?.[1];
  $("dirHint").textContent = own ? `· ${label} folder` : label ? `· main folder (no ${label} folder set)` : "· main folder";
}

async function loadFolders() {
  const r = await api.runtime.sendMessage({ type: "app-settings" }).catch(() => null);
  settings = r?.settings || null;
  updateDir();
}
$("dir").addEventListener("input", () => {
  dirChosen = true;
  $("dirHint").textContent = "· this download only";
});
$("filename").addEventListener("input", updateDir);
$("container").addEventListener("change", updateDir);
$("pickDir").onclick = async () => {
  $("pickDir").disabled = true;
  const r = await api.runtime.sendMessage({ type: "pick-folder", title: "Save this download to" }).catch((e) => ({ ok: false, error: String(e) }));
  $("pickDir").disabled = false;
  if (r?.folder) {
    $("dir").value = HurtleFolders.short(r.folder);
    dirChosen = true;
    $("dirHint").textContent = "· this download only";
  } else if (!r?.ok) {
    $("error").textContent = `Couldn't open the folder picker: ${r?.error || "no answer"}`;
  }
};

async function init() {
  const p = await api.runtime.sendMessage({ type: "dialog-load", key });
  if (!p) {
    $("error").textContent = "This download request expired.";
    for (const b of document.querySelectorAll("footer button")) b.disabled = b.id !== "cancel";
    return;
  }
  $("url").value = p.url;
  $("filename").value = p.filename || "";
  $("size").textContent = fmtSize(p.size);
  $("browser").hidden = !!p.media; // the browser can't download a page's video
  // The page's own picture right away; yt-dlp's thumbnail replaces it later.
  if (p.thumb) showThumb(p.thumb);
  isMedia = !!p.media;
  loadFolders();
  if (p.media) loadQualities(p);
  else if (/^(video|audio)\//.test(p.mime || "") || /\.(mp4|m4v|webm|mkv|mov|mp3|m4a|ogg|opus)(?:$|[?#])/i.test(p.url)) preview(p);

  // Defaults come from the Hurtle app's settings when it's reachable.
  const r = await api.runtime.sendMessage({ type: "app-settings" }).catch(() => null);
  const def = r?.settings?.connections || 8;
  $("host").textContent = r?.ok ? (r.settings ? "Hurtle ready" : "Hurtle will start") : "host missing";
  $("host").className = "pill " + (r?.ok ? "ok" : "bad");
  for (const n of [1, 2, 4, 6, 8, 12, 16, 24, 32]) {
    const o = new Option(`${n}`, n, false, n === def);
    $("connections").add(o);
  }
  if (![...$("connections").options].some((o) => o.selected)) $("connections").add(new Option(def, def, true, true));
  for (const [kb, label] of LIMITS) $("speedLimit").add(new Option(label, kb, false, kb === 0));

  const f = $("filename");
  f.focus();
  // Select the name without the extension, like a file manager rename.
  const dot = f.value.lastIndexOf(".");
  f.setSelectionRange(0, dot > 0 ? dot : f.value.length);
}

/** A direct video file: show a frame, its resolution and length. */
function preview(p) {
  const v = document.createElement("video");
  v.muted = true;
  v.preload = "metadata";
  v.addEventListener("loadedmetadata", () => {
    const parts = [];
    if (v.videoWidth) parts.push(`${Math.min(v.videoWidth, v.videoHeight)}p · ${v.videoWidth}×${v.videoHeight}`);
    if (Number.isFinite(v.duration) && v.duration > 0) parts.push(fmtTime(v.duration));
    if (!parts.length) return;
    $("previewInfo").textContent = parts.join(" · ");
    $("previewRow").hidden = false;
    if (v.duration > 2) v.currentTime = Math.min(1, v.duration / 4);
  }, { once: true });
  v.addEventListener("error", () => ($("previewRow").hidden = true), { once: true });
  v.src = p.url;
  $("previewBox").replaceChildren(v);
}

function fmtTime(s) {
  if (!Number.isFinite(s) || s <= 0) return "";
  s = Math.round(s);
  const h = Math.floor(s / 3600);
  const m = String(Math.floor((s % 3600) / 60)).padStart(h ? 2 : 1, "0");
  return `${h ? h + ":" : ""}${m}:${String(s % 60).padStart(2, "0")}`;
}

let qualities = null;
const VIDEO_TYPES = [
  ["mp4", "MP4 (plays everywhere)"],
  ["mkv", "MKV"],
  ["original", "Keep original (no remux)"],
  ["mp3", "MP3 audio"],
  ["m4a", "M4A audio"],
];
const AUDIO_TYPES = [
  ["m4a", "M4A (AAC)"],
  ["mp3", "MP3"],
  ["opus", "Opus"],
];

function fillTypes(audioOnly) {
  const c = $("container");
  const prev = c.value;
  c.replaceChildren();
  for (const [v, label] of audioOnly ? AUDIO_TYPES : VIDEO_TYPES) c.add(new Option(label, v));
  if ([...c.options].some((o) => o.value === prev)) c.value = prev;
}
let nameEdited = false;
$("filename").addEventListener("input", () => (nameEdited = true));

function showThumb(src) {
  const img = $("thumb");
  img.onerror = () => (img.hidden = true);
  img.src = src;
  img.hidden = false;
}

// A little longer than the host gives yt-dlp (90 s), so its own error wins.
const PROBE_TIMEOUT = 110;

/** Video pages / streams: ask yt-dlp which qualities exist. */
async function loadQualities(p) {
  $("qualityRow").hidden = false;
  fillTypes(false);
  $("filename").placeholder = "Use the video title";
  $("size").textContent = "…";
  const q = $("quality");
  const started = Date.now();
  const tick = () => {
    const s = Math.round((Date.now() - started) / 1000);
    q.options[0].text = s < 2 ? "Reading formats…" : `Reading formats… ${s}s`;
    $("probeHint").hidden = s < 4;
  };
  const timer = setInterval(tick, 1000);
  report("probe start", { url: p.url, pageUrl: p.pageUrl });
  const r = await Promise.race([
    api.runtime.sendMessage({ type: "dialog-probe", key }).catch((e) => ({ ok: false, error: String(e?.message || e) })),
    new Promise((res) => setTimeout(() => res({ ok: false, error: `no answer after ${PROBE_TIMEOUT}s`, timeout: true }), PROBE_TIMEOUT * 1000)),
  ]);
  clearInterval(timer);
  $("probeHint").hidden = true;
  const secs = ((Date.now() - started) / 1000).toFixed(1);
  report(
    "probe done",
    { secs, ok: !!r?.ok, error: r?.error, site: r?.media?.site, formats: r?.media?.choices?.length, audio: r?.media?.audio?.length, thumb: !!r?.media?.thumbnail },
    r?.ok ? "info" : "error",
  );
  q.replaceChildren();
  if (!r?.ok || !r.media) {
    q.add(new Option("Best available", ""));
    $("error").textContent = r?.error
      ? `Couldn't list formats: ${r.error}. You can still start: Hurtle picks the best quality.`
      : "Couldn't list formats. You can still start: Hurtle picks the best quality.";
    $("size").textContent = "unknown";
    oneTrack();
    return;
  }
  const m = r.media;
  qualities = m.choices;
  for (const [i, c] of m.choices.entries()) {
    q.add(new Option(choiceText(c), i));
  }
  // A quality picked in the page's menu (e.g. "720p"), else the preferred
  // resolution from the popup's settings: the closest one at or below it,
  // else the lowest above it (the same pick the menu's highlighted chip shows).
  const pref = (await api.storage.local.get("preferredQuality").catch(() => ({}))).preferredQuality;
  const want = p.quality || (pref == null ? 1080 : Number(pref) || 0);
  if (want) {
    let below = null;
    let above = null;
    for (const [i, c] of m.choices.entries()) {
      if (c.audioOnly) continue;
      const h = Number(/^(\d+)p/.exec(c.label)?.[1]);
      if (!h) continue;
      if (h <= want && (below == null || h > below[1])) below = [i, h];
      if (h > want && (above == null || h < above[1])) above = [i, h];
    }
    const best = below || above;
    if (best) q.value = String(best[0]);
  }
  q.disabled = false;
  q.onchange = () => {
    const c = qualities[q.value];
    // What will actually be downloaded: approximate size and frame size.
    $("size").textContent = [c?.size ? `≈ ${fmtSize(c.size)}` : "unknown", dims(c)].filter(Boolean).join("  ·  ");
    fillTypes(!!c?.audioOnly);
    updateDir();
  };
  q.onchange();
  // Several audio tracks (YouTube auto-dubs): default to the original.
  if (m.audio?.length > 1) {
    const a = $("audio");
    a.replaceChildren();
    for (const t of m.audio) {
      const tag = t.original ? " (original)" : t.dubbed ? " (auto-dubbed)" : "";
      a.add(new Option(t.label + tag, t.original ? "" : t.lang));
    }
    $("audioRow").hidden = false;
  } else {
    oneTrack();
  }
  // The probe may have swapped a raw stream for its page (better title, tracks).
  if (r.url) $("url").value = r.url;
  const len = fmtTime(m.duration || p.duration);
  $("site").textContent = [m.site, len].filter(Boolean).map((x) => `· ${x}`).join(" ");
  if (m.thumbnail) showThumb(m.thumbnail);
  // The page title can be stale on single-page sites; yt-dlp's is the real one.
  // A raw stream (generic extractor) only has its file name as title: keep the page's.
  const generic = !m.site || /^(generic|html5mediaembed)$/i.test(m.site);
  if (m.title && !nameEdited && (!generic || !$("filename").value)) {
    $("filename").value = m.title.replace(/[\\/:*?"<>|\u0000-\u001f]+/g, " ").trim();
  }
}

const dims = (c) => (c?.width && c?.height ? `${c.width}×${c.height}` : "");

/** "1080p  ·  1920×1080  ·  ≈ 45.2 MB" ("Best quality" names its resolution too). */
function choiceText(c) {
  const res = dims(c);
  const label = c.label === "Best quality" && c.height ? `Best quality (${Math.min(c.width || c.height, c.height)}p)` : c.label;
  return [label, res, c.size ? `≈ ${fmtSize(c.size)}` : ""].filter(Boolean).join("  ·  ");
}

/** Most sites have one audio track: say so instead of hiding the row. */
function oneTrack() {
  const a = $("audio");
  a.replaceChildren(new Option("Only one audio track", ""));
  a.disabled = true;
  $("audioRow").hidden = false;
}

let resolving = false;
async function resolve(action) {
  if (resolving) return;
  resolving = true;
  for (const b of document.querySelectorAll("footer button")) b.disabled = true;
  const choice = {
    filename: $("filename").value.trim(),
    connections: Number($("connections").value) || null,
    speedLimit: (Number($("speedLimit").value) || 0) * 1024 || null,
  };
  if (qualities) choice.format = qualities[$("quality").value]?.format || null;
  if (dirChosen && $("dir").value.trim()) choice.dir = $("dir").value.trim();
  if (!$("qualityRow").hidden) {
    choice.container = $("container").value || null;
    choice.audioLang = $("audioRow").hidden ? null : $("audio").value || null;
  }
  const r = await api.runtime.sendMessage({ type: "dialog-resolve", key, action, choice });
  if (r?.ok) return window.close();
  resolving = false;
  $("error").textContent = r?.error || "Something went wrong";
  for (const b of document.querySelectorAll("footer button")) b.disabled = false;
}

$("start").onclick = () => resolve("start");
$("later").onclick = () => resolve("later");
$("cancel").onclick = () => resolve("cancel");
$("browser").onclick = () => resolve("browser");
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") return resolve("cancel");
  // Enter starts from the text fields; on a button or list it keeps its own meaning.
  if (e.key === "Enter" && e.target instanceof HTMLInputElement && !e.isComposing) resolve("start");
});
init();
