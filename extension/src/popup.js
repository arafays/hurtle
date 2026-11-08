const api = globalThis.browser ?? globalThis.chrome;
const KEYS = { overlay: true, enabled: true, showDialog: true, sniffMedia: true, minSizeMB: 0, skipExtensions: "", debug: false };
const LIMITS = [
  [0, "Unlimited"], [128, "128 KB/s"], [256, "256 KB/s"], [512, "512 KB/s"], [1024, "1 MB/s"],
  [2048, "2 MB/s"], [5120, "5 MB/s"], [10240, "10 MB/s"], [20480, "20 MB/s"], [51200, "50 MB/s"],
];
const $ = (id) => document.getElementById(id);
document.head.append(Object.assign(document.createElement("style"), { textContent: SurgeUI.CSS }));

function showStatus(ok, message) {
  $("status").textContent = message || "";
  $("status").className = "status" + (ok ? "" : " bad");
}

async function loadPrefs() {
  const s = { ...KEYS, ...(await api.storage.local.get([...Object.keys(KEYS), "status"])) };
  for (const k of Object.keys(KEYS)) {
    const el = $(k);
    if (el.type === "checkbox") el.checked = s[k];
    else el.value = s[k];
    el.addEventListener("change", () => {
      const v = el.type === "checkbox" ? el.checked : el.type === "number" ? Number(el.value) || 0 : el.value;
      api.storage.local.set({ [k]: v });
    });
  }
  if (s.status) showStatus(s.status.ok, s.status.message);
}

function report(event, data, level = "info") {
  return api.runtime.sendMessage({ type: "debug-log", event: `popup: ${event}`, data, level }).catch(() => {});
}
addEventListener("error", (e) => report("script error", { message: e.message, at: `${e.filename}:${e.lineno}` }, "error"));
addEventListener("unhandledrejection", (e) => report("unhandled rejection", { reason: String(e.reason?.message || e.reason) }, "error"));

/**
 * Open the download dialog. The popup closes once the background has it
 * (or after a moment either way), and shows the error if it refuses.
 */
async function go(msg) {
  report(`click ${msg.type}`, { quality: msg.quality });
  showStatus(true, "Opening the download dialog…");
  const r = await Promise.race([
    api.runtime.sendMessage(msg).catch((e) => ({ ok: false, error: String(e?.message || e) })),
    new Promise((res) => setTimeout(() => res({ ok: true, slow: true }), 1500)),
  ]);
  if (r?.ok === false) {
    report(`${msg.type} failed`, { error: r.error }, "error");
    return showStatus(false, r.error || "Couldn't open the dialog");
  }
  window.close();
}

async function loadMedia() {
  const [tab] = await api.tabs.query({ active: true, currentWindow: true });
  const [list, info] = await Promise.all([
    api.runtime.sendMessage({ type: "media-list", tabId: tab?.id }).then((l) => l || []),
    // The page knows its title, picture and each <video>'s size and length.
    tab?.id != null ? api.tabs.sendMessage(tab.id, { type: "page-info" }).catch(() => null) : null,
  ]);
  // Videos the page plays from a plain URL that the sniffer missed (e.g.
  // loaded before the extension woke up). blob: sources are MSE streams.
  for (const v of info?.videos || []) {
    if (/^https?:/i.test(v.src) && !list.some((m) => m.url === v.src)) list.push({ url: v.src, kind: "file", mime: null, size: null });
  }
  const SUI = SurgeUI;
  const title = info?.title || tab?.title || "";
  const ul = $("media");
  ul.replaceChildren();
  const add = (row) => {
    const li = document.createElement("li");
    li.append(row.el);
    ul.append(li);
  };
  if (/^https?:/i.test(tab?.url || "")) {
    const page = { type: "download-page", pageUrl: tab.url, title, thumb: info?.image, cookieStoreId: tab.cookieStoreId };
    const hasVideo = info?.videos?.length > 0;
    const row = SUI.row({
      title: title || new URL(tab.url).hostname,
      sub: hasVideo ? "Reading the available sizes…" : "Video on this page · choose quality and audio",
      file: new URL(tab.url).hostname,
      thumbOpts: { image: info?.image, previewUrl: info?.videos?.find((v) => /^https?:/i.test(v.src))?.src },
      onClick: () => go(page),
      onChip: (quality) => go({ ...page, quality }),
    });
    add(row);
    // Which sizes yt-dlp offers (shared with the page button, so usually instant).
    if (hasVideo) {
      api.runtime
        .sendMessage({ type: "page-sizes", pageUrl: tab.url, cookieStoreId: tab.cookieStoreId })
        .catch(() => null)
        .then((r) => {
          if (r?.ok && r.sizes?.length) {
            row.setSub(SUI.sizesLine(r));
            row.setChips(SUI.sizeChips(r.sizes));
          } else {
            row.setSub("Video on this page · choose quality and audio");
          }
        });
    }
  }
  $("mediaEmpty").hidden = list.length > 0;
  const send = async (m, quality) => {
    const v = info?.videos?.find((x) => x.src === m.url);
    await go({ type: "download-media", media: m, quality, title, thumb: v?.poster || info?.image, pageUrl: tab?.url });
  };
  // Newest first; at most a dozen previews.
  for (const m of list.slice().reverse().slice(0, 12)) {
    const v = info?.videos?.find((x) => x.src === m.url);
    const stream = m.kind === "stream";
    const meta = v ? { w: v.w, h: v.h, duration: v.duration } : {};
    const row = SUI.row({
      title: v || stream ? title || SUI.fileName(m.url) : SUI.fileName(m.url),
      badge: v ? "On page" : null,
      sub: SUI.details(m, meta),
      file: v || stream ? SUI.fileName(m.url) : null,
      thumbOpts: stream
        ? { image: v?.poster || info?.image }
        : { image: v?.poster, previewUrl: m.url, onMeta: (x) => row.setSub(SUI.details(m, x)) },
      chips: stream && m.qualities?.length > 1 ? m.qualities : null,
      onClick: () => send(m),
      onChip: (q) => send(m, q),
    });
    row.el.title = m.url;
    add(row);
  }
}

async function loadApp() {
  const ping = await api.runtime.sendMessage({ type: "ping" });
  const pill = $("host");
  if (!ping?.ok) {
    pill.textContent = "host not installed";
    pill.className = "pill bad";
    showStatus(false, "Surge isn't set up for this browser yet: start the Surge app once (it registers itself), or run surge-host --register. Then restart the browser.");
  } else {
    pill.textContent = ping.error ? "app not running" : `connected v${ping.version}`;
    pill.className = "pill ok";
  }

  // The host reads (and changes) the settings itself when the app is closed.
  const r = ping?.ok ? await api.runtime.sendMessage({ type: "app-settings" }) : null;
  const s = r?.settings;
  loadFolders(s);
  $("appNote").hidden = !!s;
  const conns = $("appConnections");
  const limit = $("appSpeedLimit");
  for (const n of [1, 2, 4, 6, 8, 12, 16, 24, 32]) conns.add(new Option(n, n, false, n === (s?.connections ?? 8)));
  const kb = Math.round((s?.speed_limit ?? 0) / 1024);
  for (const [v, label] of LIMITS) limit.add(new Option(label, v, false, v === kb));
  if (kb && !LIMITS.some(([v]) => v === kb)) limit.add(new Option(`${kb} KB/s`, kb, true, true));
  conns.disabled = limit.disabled = !s;
  conns.onchange = () => api.runtime.sendMessage({ type: "app-settings", connections: Number(conns.value) });
  limit.onchange = () => api.runtime.sendMessage({ type: "app-settings", speedLimit: Number(limit.value) * 1024 });
}

/** Main folder plus one per file type; empty = the main folder. */
function loadFolders(s) {
  const box = $("folders");
  box.replaceChildren();
  const note = $("foldersNote");
  if (!s) {
    note.textContent = "Folders show up once Surge is set up for this browser.";
    return;
  }
  const save = async (input, change) => {
    input.classList.remove("saved", "failed");
    const r = await api.runtime.sendMessage({ type: "app-settings", ...change }).catch((e) => ({ ok: false, error: String(e) }));
    input.classList.add(r?.ok ? "saved" : "failed");
    note.className = r?.ok ? "status" : "status bad";
    note.textContent = r?.ok ? "Saved." : `Not saved: ${r?.error || "no answer from Surge"}`;
    if (r?.ok) report("folders changed", change);
  };
  const add = (id, label, value, placeholder, changeFor) => {
    const l = document.createElement("label");
    l.textContent = label;
    l.htmlFor = `dir-${id}`;
    const input = document.createElement("input");
    input.type = "text";
    input.id = `dir-${id}`;
    input.spellcheck = false;
    input.value = SurgeFolders.short(value || "");
    input.placeholder = placeholder;
    input.title = value || placeholder;
    input.addEventListener("change", () => save(input, changeFor(input.value.trim())));
    const pick = document.createElement("button");
    pick.type = "button";
    pick.textContent = "…";
    pick.title = `Choose the ${label.toLowerCase()} folder`;
    pick.addEventListener("click", async () => {
      pick.disabled = true;
      note.className = "status";
      note.textContent = "Choose a folder in the window Surge opened…";
      const r = await api.runtime.sendMessage({ type: "pick-folder", title: `${label} folder` }).catch(() => null);
      pick.disabled = false;
      if (r?.folder) {
        input.value = SurgeFolders.short(r.folder);
        save(input, changeFor(r.folder));
      } else {
        note.textContent = r?.ok ? "" : `Couldn't open the folder picker: ${r?.error || "no answer"}`;
      }
    });
    box.append(l, input, pick);
  };
  add("main", "Main", s.download_dir, "~/Downloads", (v) => ({ downloadDir: v }));
  const main = SurgeFolders.short(s.download_dir);
  for (const [id, label] of SurgeFolders.CATEGORIES) {
    add(id, label, s.type_dirs?.[id], `Main (${main})`, (v) => ({ typeDirs: { [id]: v } }));
  }
}

async function checkAccess() {
  const all = { origins: ["<all_urls>"] };
  const granted = await api.permissions.contains(all);
  $("access").hidden = granted;
  if (!granted) report("site access not granted", {}, "error");
  $("grant").onclick = async () => {
    if (await api.permissions.request(all)) $("access").hidden = true;
  };
}

$("open").addEventListener("click", async () => {
  const r = await api.runtime.sendMessage({ type: "show" });
  if (r?.ok) window.close();
  else showStatus(false, r?.error || "Could not reach Surge");
});

async function loadOverlayState() {
  const [tab] = await api.tabs.query({ active: true, currentWindow: true });
  if (!tab?.id) return;
  const r = await api.tabs.sendMessage(tab.id, { type: "overlay-state" }).catch(() => null);
  $("overlayHidden").hidden = !r?.dismissed;
  $("overlayReset").onclick = async (e) => {
    e.preventDefault();
    // Every frame resets (the video may sit in an iframe).
    await api.tabs.sendMessage(tab.id, { type: "overlay-reset" }).catch(() => {});
    $("overlayHidden").hidden = true;
  };
}

$("debugOpen").onclick = (e) => {
  e.preventDefault();
  api.tabs.create({ url: api.runtime.getURL("log.html") });
  window.close();
};

loadPrefs();
loadOverlayState();
loadMedia();
loadApp();
checkAccess();
