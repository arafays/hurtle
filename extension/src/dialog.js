const api = globalThis.browser ?? globalThis.chrome;
const $ = (id) => document.getElementById(id);
let hostGone = false;
const key = decodeURIComponent(location.hash.slice(1));

/** Into the extension's debug log (popup → "Debug log"). */
function report(event, data, level = "info") {
  api.runtime.sendMessage({ type: "debug-log", event: `dialog: ${event}`, data, level }).catch(() => {});
}
addEventListener("error", (e) => report("script error", { message: e.message, at: `${e.filename}:${e.lineno}` }, "error"));
addEventListener("unhandledrejection", (e) => report("unhandled rejection", { reason: String(e.reason?.message || e.reason) }, "error"));


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
let isTorrent = false;

function fileCategory() {
  if (isTorrent) return torrentCategory();
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
  isTorrent = !!p.torrent;
  // The page's own picture right away; yt-dlp's thumbnail replaces it later.
  if (p.thumb) showThumb(p.thumb);
  isMedia = !!p.media;
  loadFolders();
  // Defaults come from the Hurtle app's settings when it's reachable.
  const r = await api.runtime.sendMessage({ type: "app-settings" }).catch(() => null);
  if (!r?.ok) hostMissing();
  if (isTorrent) loadTorrent(p, r?.settings);
  else if (p.media) loadQualities(p);
  else if (/^(video|audio)\//.test(p.mime || "") || /\.(mp4|m4v|webm|mkv|mov|mp3|m4a|ogg|opus)(?:$|[?#])/i.test(p.url)) preview(p);

  const def = r?.settings?.connections || 8;
  $("host").textContent = r?.ok ? (r.settings ? "Hurtle ready" : "Hurtle will start") : "host missing";
  $("host").className = "pill " + (r?.ok ? "ok" : "bad");
  for (const n of HurtleFolders.CONNECTIONS) {
    const o = new Option(`${n}`, n, false, n === def);
    $("connections").add(o);
  }
  if (![...$("connections").options].some((o) => o.selected)) $("connections").add(new Option(def, def, true, true));
  const globalKb = Math.round((r?.settings?.speed_limit || 0) / 1024);
  for (const [kb, label] of HurtleFolders.LIMITS) {
    const text = kb === 0 ? (globalKb ? `No per-file limit (global ${globalKb} KB/s applies)` : "Unlimited") : label;
    $("speedLimit").add(new Option(text, kb, false, kb === 0));
  }

  const f = $("filename");
  f.focus();
  // Select the name without the extension, like a file manager rename.
  const dot = f.value.lastIndexOf(".");
  f.setSelectionRange(0, dot > 0 ? dot : f.value.length);
  // With no host the primary action is "Use browser", which hostMissing() focused.
  if (hostGone && !$("browser").hidden) $("browser").focus();
}

/** No native host: Start can't work, so steer to the browser or the install page. */
function hostMissing() {
  hostGone = true;
  $("error").replaceChildren(
    "Hurtle isn't installed or the native host is missing. ",
    Object.assign(document.createElement("a"), { href: HurtleFolders.INSTALL_URL, target: "_blank", rel: "noopener", textContent: "Get the Hurtle app" }),
    ".",
  );
  $("start").disabled = $("later").disabled = true;
  if (!$("browser").hidden) {
    $("browser").classList.add("primary");
    $("start").classList.remove("primary");
    $("browser").focus();
  }
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
  if (hostGone) {
    $("quality").replaceChildren(new Option("Unavailable", ""));
    $("quality").disabled = true;
    $("size").textContent = "unknown";
    return;
  }
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
  if (r?.noVideo) return noVideo(r.error);
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

/** A channel, playlist or home page: nothing here can be downloaded, so only Cancel stays. */
let listPage = false;
function noVideo(error) {
  listPage = true;
  const msg = error ? error[0].toUpperCase() + error.slice(1) : "No downloadable video found on this page";
  $("error").textContent = `${msg}.`;
  $("qualityRow").hidden = true;
  $("size").textContent = "unknown";
  for (const id of ["start", "later"]) {
    $(id).disabled = true;
    $(id).title = "Open a video page first";
  }
  $("cancel").classList.add("primary");
  $("cancel").focus();
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

// ---- torrents: which files, seeding, play while downloading

let torrentFiles = null; // [{path, size, selected, padding}] from the app
const fileBoxes = []; // [index, checkbox]

function torrentCategory() {
  // The biggest chosen file decides the folder, as in the app.
  let best = null;
  for (const [i, box] of fileBoxes) {
    const f = torrentFiles[i];
    if (box.checked && (!best || f.size > best.size)) best = f;
  }
  return best ? HurtleFolders.category(best.path) : null;
}

const PLAYABLE = /\.(mp4|m4v|mkv|webm|avi|mov|wmv|flv|ts|m2ts|mpg|mpeg|ogv|mp3|m4a|flac|ogg|opus|wav|aac)$/i;

function torrentChanged() {
  const chosen = fileBoxes.filter(([, b]) => b.checked);
  const bytes = chosen.reduce((n, [i]) => n + torrentFiles[i].size, 0);
  $("size").textContent = fmtSize(bytes);
  $("torrentHint").textContent = `· ${chosen.length} of ${fileBoxes.length} chosen`;
  const none = !chosen.length;
  $("start").disabled = $("later").disabled = none || hostGone;
  $("start").title = none ? "Choose at least one file" : "";
  $("streamNow").disabled = !chosen.some(([i]) => PLAYABLE.test(torrentFiles[i].path));
  if ($("streamNow").disabled) $("streamNow").checked = false;
  updateDir();
}

function setAll(pick) {
  for (const [i, b] of fileBoxes) b.checked = pick(torrentFiles[i]);
  torrentChanged();
}
$("allFiles").onclick = () => setAll(() => true);
$("noFiles").onclick = () => setAll(() => false);
$("mediaFiles").onclick = () => setAll((f) => PLAYABLE.test(f.path));

/** Magnet links and .torrent files: list the files (a magnet asks its peers first). */
async function loadTorrent(p, appSettings) {
  $("torrentRow").hidden = false;
  $("connectionsCol").hidden = true; // peers, not connections, for torrents
  $("filename").readOnly = true;
  $("filename").title = "The torrent's own name";
  $("browser").hidden = !p.torrentData && !/^magnet:/i.test(p.url) && p.browserDownloadId == null;
  $("browser").textContent = /^magnet:/i.test(p.url) ? "Use torrent app" : "Use browser";
  const def = appSettings?.seed_ratio ?? 1;
  const seed = $("seedRatio");
  for (const [v, label] of [[0, "Don't seed"], [0.5, "Until ratio 0.5"], [1, "Until ratio 1"], [2, "Until ratio 2"], [5, "Until ratio 5"]]) {
    seed.add(new Option(label, v, false, v === def));
  }
  if (![...seed.options].some((o) => o.selected)) seed.add(new Option(`Until ratio ${def}`, def, true, true));
  $("size").textContent = "…";
  $("streamNow").disabled = true;
  if (hostGone) {
    $("files").replaceChildren(Object.assign(document.createElement("li"), { className: "hint", textContent: "Unavailable without the Hurtle app." }));
    return;
  }
  const magnet = /^magnet:/i.test(p.url);
  const started = Date.now();
  const status = $("files").firstElementChild;
  const timer = setInterval(() => {
    const s = Math.round((Date.now() - started) / 1000);
    status.textContent = magnet
      ? `Asking peers for the file list… ${s}s. You can start now; Hurtle then gets every file.`
      : `Reading the torrent… ${s}s`;
  }, 1000);
  report("torrent info start", { magnet });
  const r = await api.runtime.sendMessage({ type: "dialog-torrent", key }).catch((e) => ({ ok: false, error: String(e?.message || e) }));
  clearInterval(timer);
  report("torrent info done", { ok: !!r?.ok, error: r?.error, files: r?.torrent?.files?.length, secs: ((Date.now() - started) / 1000).toFixed(1) }, r?.ok ? "info" : "error");
  if (!r?.ok || !r.torrent) {
    status.textContent = "Couldn't list the files. You can still start: Hurtle gets every file.";
    $("error").textContent = r?.error || "";
    $("size").textContent = "unknown";
    $("streamNow").disabled = false;
    return;
  }
  const t = r.torrent;
  torrentFiles = t.files;
  if (t.name && !nameEdited) $("filename").value = t.name;
  const list = [];
  for (const [i, f] of t.files.entries()) {
    if (f.padding) continue;
    const li = document.createElement("li");
    const box = Object.assign(document.createElement("input"), { type: "checkbox", checked: f.selected !== false });
    const label = document.createElement("label");
    const path = document.createElement("span");
    path.className = "path";
    const slash = f.path.lastIndexOf("/");
    if (slash > 0) path.append(Object.assign(document.createElement("em"), { textContent: f.path.slice(0, slash + 1) }));
    path.append(f.path.slice(slash + 1));
    path.title = f.path;
    const size = Object.assign(document.createElement("span"), { className: "sz", textContent: fmtSize(f.size) });
    label.append(box, path, size);
    li.append(label);
    box.addEventListener("change", torrentChanged);
    fileBoxes.push([i, box]);
    list.push(li);
  }
  $("files").replaceChildren(...list);
  $("site").textContent = "";
  torrentChanged();
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
  if (isTorrent) {
    // Every file chosen (or the list never arrived): null, so Hurtle takes all.
    const all = !fileBoxes.length || fileBoxes.every(([, b]) => b.checked);
    choice.files = all ? null : fileBoxes.filter(([, b]) => b.checked).map(([i]) => i);
    choice.seedRatio = Number($("seedRatio").value);
    choice.stream = $("streamNow").checked && action === "start";
    delete choice.filename;
    delete choice.connections;
  }
  if (!$("qualityRow").hidden) {
    choice.container = $("container").value || null;
    choice.audioLang = $("audioRow").hidden ? null : $("audio").value || null;
  }
  const r = await api.runtime.sendMessage({ type: "dialog-resolve", key, action, choice });
  if (r?.ok) return window.close();
  resolving = false;
  $("error").textContent = r?.error || "Something went wrong";
  for (const b of document.querySelectorAll("footer button")) b.disabled = (listPage || hostGone) && (b.id === "start" || b.id === "later");
  if (isTorrent && fileBoxes.length) torrentChanged();
}

$("start").onclick = () => resolve("start");
$("later").onclick = () => resolve("later");
$("cancel").onclick = () => resolve("cancel");
$("browser").onclick = () => resolve("browser");
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") return resolve("cancel");
  // Enter starts from the text fields; on a button or list it keeps its own meaning.
  if (e.key === "Enter" && e.target instanceof HTMLInputElement && !e.isComposing && !listPage && !hostGone) resolve("start");
});
init();
