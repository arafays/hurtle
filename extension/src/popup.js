const api = globalThis.browser ?? globalThis.chrome;
const KEYS = { overlay: true, enabled: true, showDialog: true, sniffMedia: true, minSizeMB: 0, skipExtensions: "", debug: false, preferredQuality: 1080 };
const $ = (id) => document.getElementById(id);
document.head.append(Object.assign(document.createElement("style"), { textContent: HurtleUI.CSS }));

function showStatus(ok, message) {
  $("status").textContent = message || "";
  $("status").className = "status" + (ok ? "" : " bad");
}

async function loadPrefs() {
  const s = { ...KEYS, ...(await api.storage.local.get(Object.keys(KEYS))) };
  for (const k of Object.keys(KEYS)) {
    const el = $(k);
    if (el.type === "checkbox") el.checked = s[k];
    else el.value = s[k];
    el.addEventListener("change", () => {
      if (k === "enabled") syncDependents();
      const v = el.type === "checkbox" ? el.checked : el.type === "number" || el.tagName === "SELECT" ? Number(el.value) || 0 : el.value;
      api.storage.local.set({ [k]: v });
    });
  }
  syncDependents();
}

/** These only matter while Hurtle catches browser downloads. */
function syncDependents() {
  for (const id of ["showDialog", "minSizeMB", "skipExtensions"]) $(id).disabled = !$("enabled").checked;
}

/** Past hand-offs, newest first. The tab only shows when there are some. */
async function loadHistory() {
  const render = async () => {
    const { history, status } = await api.storage.local.get(["history", "status"]);
    // Stores from before the history kept only the last status.
    const list = Array.isArray(history) ? history : status ? [status] : [];
    $("tab-history").hidden = list.length === 0;
    if (!list.length && $("tab-history").getAttribute("aria-selected") === "true") selectTab($("tab-page"));
    const ul = $("historyList");
    ul.replaceChildren();
    for (const h of list) {
      const li = document.createElement("li");
      if (!h.ok) li.className = "bad";
      const dot = document.createElement("span");
      dot.className = "dot";
      const msg = document.createElement("span");
      msg.className = "msg";
      msg.textContent = msg.title = h.message || "";
      const time = document.createElement("time");
      const at = new Date(h.at);
      time.dateTime = at.toISOString();
      time.title = at.toLocaleString();
      time.textContent = at.toDateString() === new Date().toDateString()
        ? at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
        : at.toLocaleDateString([], { month: "short", day: "numeric" });
      li.append(dot, msg, time);
      ul.append(li);
    }
  };
  $("historyClear").addEventListener("click", async () => {
    await api.storage.local.remove(["history", "status"]);
    render();
  });
  api.storage.onChanged.addListener((c, area) => {
    if ((c.history || c.status) && (!area || area === "local")) render();
  });
  render();
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
  const pref = (await api.storage.local.get("preferredQuality")).preferredQuality;
  const preferred = pref == null ? KEYS.preferredQuality : Number(pref) || 0;
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
  const SUI = HurtleUI;
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
    // Ask for the sizes whenever there's something to download here: a
    // <video>, or media the sniffer caught (as the page button does).
    const hasVideo = info?.videos?.length > 0 || list.length > 0;
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
            row.setChips(SUI.sizeChips(r.sizes, preferred));
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
      chips: stream && m.qualities?.length > 1 ? SUI.heightChips(m.qualities, preferred) : null,
      onClick: () => send(m),
      onChip: (q) => send(m, q),
    });
    row.el.title = m.url;
    add(row);
  }
}

/** Hurtle isn't installed (or isn't registered with this browser). */
function showSetup() {
  $("setup").hidden = false;
  $("setupGet").href = HurtleFolders.INSTALL_URL;
  $("open").textContent = "Get the app";
  $("open").onclick = () => api.tabs.create({ url: HurtleFolders.INSTALL_URL }).then(() => window.close());
}

async function loadApp() {
  const ping = await api.runtime.sendMessage({ type: "ping" });
  const pill = $("host");
  if (!ping?.ok) {
    pill.textContent = "host not installed";
    pill.className = "pill bad";
    showSetup();
  } else {
    $("setup").hidden = true;
    pill.textContent = ping.error ? "app not running" : `connected v${ping.version}`;
    pill.className = ping.error ? "pill" : "pill ok";
    pill.title = ping.error ? "Hurtle starts with your next download" : "";
  }

  // The host reads (and changes) the settings itself when the app is closed.
  const r = ping?.ok ? await api.runtime.sendMessage({ type: "app-settings" }) : null;
  const s = r?.settings;
  loadFolders(s);
  $("appNote").hidden = !!s;
  const conns = $("appConnections");
  const limit = $("appSpeedLimit");
  for (const n of HurtleFolders.CONNECTIONS) conns.add(new Option(n, n, false, n === (s?.connections ?? 8)));
  const kb = Math.round((s?.speed_limit ?? 0) / 1024);
  for (const [v, label] of HurtleFolders.LIMITS) limit.add(new Option(label, v, false, v === kb));
  if (kb && !HurtleFolders.LIMITS.some(([v]) => v === kb)) limit.add(new Option(`${kb} KB/s`, kb, true, true));
  const retry = $("appAutoRetry");
  const notify = $("appNotify");
  retry.checked = s?.auto_retry ?? true;
  notify.checked = s?.notify ?? true;
  conns.disabled = limit.disabled = retry.disabled = notify.disabled = !s;
  retry.onchange = () => api.runtime.sendMessage({ type: "app-settings", autoRetry: retry.checked });
  notify.onchange = () => api.runtime.sendMessage({ type: "app-settings", notify: notify.checked });
  conns.onchange = () => api.runtime.sendMessage({ type: "app-settings", connections: Number(conns.value) });
  limit.onchange = () => api.runtime.sendMessage({ type: "app-settings", speedLimit: Number(limit.value) * 1024 });
}

/** Main folder plus one per file type; empty = the main folder. */
function loadFolders(s) {
  const box = $("folders");
  box.replaceChildren();
  const note = $("foldersNote");
  if (!s) {
    note.textContent = "Folders show up once Hurtle is set up for this browser.";
    return;
  }
  const save = async (input, change) => {
    input.classList.remove("saved", "failed");
    const r = await api.runtime.sendMessage({ type: "app-settings", ...change }).catch((e) => ({ ok: false, error: String(e) }));
    input.classList.add(r?.ok ? "saved" : "failed");
    note.className = r?.ok ? "status" : "status bad";
    note.textContent = r?.ok ? "Saved." : `Not saved: ${r?.error || "no answer from Hurtle"}`;
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
    input.value = HurtleFolders.short(value || "");
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
      note.textContent = "Choose a folder in the window Hurtle opened…";
      const r = await api.runtime.sendMessage({ type: "pick-folder", title: `${label} folder` }).catch(() => null);
      pick.disabled = false;
      if (r?.folder) {
        input.value = HurtleFolders.short(r.folder);
        save(input, changeFor(r.folder));
      } else {
        note.textContent = r?.ok ? "" : `Couldn't open the folder picker: ${r?.error || "no answer"}`;
      }
    });
    box.append(l, input, pick);
  };
  add("main", "Main", s.download_dir, "~/Downloads", (v) => ({ downloadDir: v }));
  const main = HurtleFolders.short(s.download_dir);
  for (const [id, label] of HurtleFolders.CATEGORIES) {
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
  if (!$("setup").hidden) return;
  const r = await api.runtime.sendMessage({ type: "show" });
  if (r?.ok) window.close();
  else showStatus(false, r?.error || "Could not reach Hurtle");
});

/** Hostname from whatever was typed: "https://WWW.Example.com:8080/a?b" -> "example.com". */
function normalizeHost(input) {
  let s = String(input || "").trim();
  if (!s) return "";
  if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(s)) s = `http://${s}`;
  let h = "";
  try {
    h = new URL(s).hostname;
  } catch {
    return "";
  }
  h = h.toLowerCase().replace(/\.$/, "").replace(/^www\./, "");
  return /^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)*$/.test(h) ? h : "";
}

/** The ✕ icon on each site row, built as DOM (AMO rejects innerHTML). */
function removeIcon() {
  const NS = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(NS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS(NS, "path");
  path.setAttribute("d", "M6 6l12 12M18 6L6 18");
  svg.append(path);
  return svg;
}

/** Per-site switch and the manager for sites the button is hidden on. */
async function loadOverlaySites() {
  const [tab] = await api.tabs.query({ active: true, currentWindow: true });
  let host = "";
  try {
    if (/^https?:/i.test(tab?.url || "")) host = new URL(tab.url).hostname;
  } catch {}
  const read = async () => {
    const v = (await api.storage.local.get("overlayOff")).overlayOff;
    return Array.isArray(v) ? v : [];
  };
  const msg = (text, bad) => {
    $("offMsg").textContent = text || "";
    $("offMsg").className = "status" + (bad ? " bad" : "");
  };
  const render = async () => {
    const list = await read();
    $("siteRow").hidden = !host;
    if (host) {
      $("siteHost").textContent = host;
      // Shown unless the host itself or a parent domain is listed.
      $("siteOn").checked = !HurtleUI.hostBlocked(list, host);
      // A parent domain blocks it: unticking/ticking here edits that entry.
      $("siteOn").title = HurtleUI.hostBlocked(list, host) && !list.includes(host) ? "Hidden through a parent domain in the Sites tab" : "";
    }
    const q = $("offFilter").value.trim().toLowerCase();
    $("offFilter").hidden = list.length < 6 && !q;
    const shown = list.filter((h) => !q || String(h).toLowerCase().includes(q)).sort();
    $("offEmpty").hidden = shown.length > 0;
    $("offEmpty").textContent = list.length ? "No site matches the filter." : "No sites yet. Hide the button on a page, or add a site above.";
    const ul = $("offList");
    ul.replaceChildren();
    for (const h of shown) {
      const li = document.createElement("li");
      const name = document.createElement("span");
      name.textContent = h;
      if (host && HurtleUI.hostBlocked([h], host)) {
        const here = document.createElement("small");
        here.className = "here";
        here.textContent = "this page";
        name.append(here);
      }
      const x = document.createElement("button");
      x.type = "button";
      x.title = x.ariaLabel = `Show the button on ${h} again`;
      x.append(removeIcon());
      x.addEventListener("click", async () => {
        await api.storage.local.set({ overlayOff: (await read()).filter((e) => e !== h) });
        msg("");
        render();
      });
      li.append(name, x);
      ul.append(li);
    }
  };
  $("siteOn").addEventListener("change", async () => {
    const cur = await read();
    // Showing again removes the host and any parent domain that blocks it.
    const next = $("siteOn").checked ? cur.filter((h) => !HurtleUI.hostBlocked([h], host)) : [...cur, host];
    await api.storage.local.set({ overlayOff: next });
    render();
  });
  $("offAdd").addEventListener("submit", async (e) => {
    e.preventDefault();
    const h = normalizeHost($("offNew").value);
    if (!h) return msg("That doesn't look like a site name.", true);
    const cur = await read();
    if (cur.some((x) => String(x).toLowerCase() === h)) return msg(`${h} is already in the list.`, true);
    await api.storage.local.set({ overlayOff: [...cur, h] });
    $("offNew").value = "";
    msg(`Hidden on ${h} and its subdomains.`);
    render();
  });
  $("offFilter").addEventListener("input", render);
  // Changes from the page button (or another popup) show up live.
  api.storage.onChanged.addListener((c, area) => {
    if (c.overlayOff && (!area || area === "local")) render();
  });
  render();
}

/** Tabs: click or arrow keys; "This page" opens first each time. */
function selectTab(t, focus) {
  for (const x of document.querySelectorAll('[role="tab"]')) {
    const on = x === t;
    x.setAttribute("aria-selected", on);
    x.tabIndex = on ? 0 : -1;
    $(x.getAttribute("aria-controls")).hidden = !on;
  }
  if (focus) t.focus();
}

function initTabs() {
  for (const t of document.querySelectorAll('[role="tab"]')) {
    t.addEventListener("click", () => selectTab(t));
    t.addEventListener("keydown", (e) => {
      // Hidden tabs (History when empty) are skipped.
      const tabs = [...document.querySelectorAll('[role="tab"]:not([hidden])')];
      const i = tabs.indexOf(t);
      const to = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: tabs.length - 1 }[e.key];
      if (to == null) return;
      e.preventDefault();
      selectTab(tabs[(to + tabs.length) % tabs.length], true);
    });
  }
}

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

initTabs();
loadPrefs();
loadHistory();
loadOverlayState();
loadOverlaySites();
loadMedia();
loadApp();
checkAccess();
