// End-to-end test: headless Chromium with the extension loaded, a throwaway
// profile whose NativeMessagingHosts points at target/debug/hurtle-host, a
// local page with a <video>, and a fresh Hurtle app instance.
//
//   mise run ext:e2e
//
// Checks: media sniffing → overlay button → dialog named from og:title →
// "Start download" reaches Hurtle with the chosen connections/limit; and
// a browser download is intercepted → dialog → "Download later"; a magnet
// link click and a .torrent download open the dialog with the file list.

import { spawn, execFileSync } from "node:child_process";
import { createServer } from "node:http";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dirname, "../..");
const ext = join(root, "extension/dist/chrome");
const chromeId = readFileSync(join(root, "extension/chrome-id.txt"), "utf8").trim();
const work = mkdtempSync(join(tmpdir(), "hurtle-e2e-"));
const profile = join(work, "profile");
const data = join(work, "data");
const downloads = join(work, "downloads");
for (const d of [profile, data, downloads]) mkdirSync(d, { recursive: true });
// Torrents stay on loopback: no DHT, UPnP or fixed port, and no seeding.
writeFileSync(
  join(data, "settings.json"),
  JSON.stringify({ download_dir: downloads, connections: 8, torrent_dht: false, torrent_upnp: false, torrent_port: 0, seed_ratio: 0 }),
);

// Native host registered only inside this profile.
mkdirSync(join(profile, "NativeMessagingHosts"));
writeFileSync(
  join(profile, "NativeMessagingHosts/io.github.arafays.hurtle.json"),
  JSON.stringify({
    name: "io.github.arafays.hurtle",
    description: "Hurtle (e2e)",
    path: join(root, "target/debug/hurtle-host"),
    type: "stdio",
    allowed_origins: [`chrome-extension://${chromeId}/`],
  }),
);

// A short HLS stream for the yt-dlp path.
const hlsDir = join(work, "hls");
mkdirSync(hlsDir);
execFileSync("ffmpeg", [
  "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=duration=6:size=640x360:rate=25",
  "-f", "lavfi", "-i", "sine=frequency=440:duration=6", "-c:v", "libx264", "-preset", "ultrafast",
  "-c:a", "aac", "-f", "hls", "-hls_time", "2", "-hls_playlist_type", "vod", join(hlsDir, "stream.m3u8"),
]);

// A master playlist, so the menu can show the stream's qualities.
writeFileSync(
  join(hlsDir, "master.m3u8"),
  "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=640x360\nstream.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720\nstream.m3u8\n",
);
// A real (playable) clip for the page's <video>, so previews have a size and length.
const clipPath = join(work, "clip.mp4");
execFileSync("ffmpeg", [
  "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=duration=8:size=640x360:rate=25",
  "-c:v", "libx264", "-preset", "ultrafast", "-b:v", "1M", "-pix_fmt", "yuv420p", "-movflags", "+faststart", clipPath,
]);
const clip = readFileSync(clipPath);

// A torrent of a folder (the clip and a text file), seeded on loopback by
// the `seed` example; its magnet names the seeder (x.pe), so no tracker or DHT.
const pack = join(work, "seedsrc", "Pack");
mkdirSync(pack, { recursive: true });
writeFileSync(join(pack, "clip.mp4"), clip);
writeFileSync(join(pack, "readme.txt"), "not wanted\n");
const seeder = spawn(join(root, "target/debug/examples/seed"), [pack], { env: { ...process.env, TMPDIR: work }, stdio: ["ignore", "pipe", "ignore"] });
const magnet = await new Promise((res, rej) => {
  let out = "";
  seeder.stdout.on("data", (b) => {
    out += b;
    const line = out.split("\n").find((l) => l.startsWith("magnet:"));
    if (line) res(line.trim());
  });
  seeder.on("exit", () => rej(new Error("seeder exited")));
  setTimeout(() => rej(new Error("seeder didn't start")), 20000);
});
// A file server for the clip, other files (any bytes do), streams and a page.
const blob = Buffer.alloc(3 * 1024 * 1024, 7);
const server = createServer((req, res) => {
  if (process.env.E2E_DEBUG) console.log("req", req.url, req.headers.range || "");
  if (req.url.startsWith("/hls/")) {
    const file = join(hlsDir, req.url.slice(5).split("?")[0]);
    if (!file.startsWith(hlsDir) || !existsSync(file)) return res.writeHead(404).end();
    const type = file.endsWith(".m3u8") ? "application/vnd.apple.mpegurl" : "video/mp2t";
    res.writeHead(200, { "content-type": type });
    return res.end(readFileSync(file));
  }
  if (req.url === "/torrent") {
    res.writeHead(200, { "content-type": "text/html" });
    return res.end(`<!doctype html><title>Torrents</title><a id="m" href="${magnet.replace(/"/g, "&quot;")}">Pack (magnet)</a>`);
  }
  if (req.url === "/files/pack.torrent") {
    res.writeHead(200, { "content-type": "application/x-bittorrent", "content-disposition": 'attachment; filename="Pack.torrent"' });
    return res.end(readFileSync(join(work, "Pack.torrent")));
  }
  if (req.url === "/page") {
    res.writeHead(200, { "content-type": "text/html" });
    return res.end(`<!doctype html><html><head><title>My Test Video - TestTube</title>
      <meta property="og:title" content="My Test Video - TestTube">
      <meta property="og:site_name" content="TestTube">
      <meta property="og:image" content="/poster.png"></head>
      <body style="margin:0"><video id="v" src="/media/clip.mp4" width="640" height="360" preload="auto" muted></video></body></html>`);
  }
  const body = req.url.endsWith(".mp4") ? clip : blob;
  const m = /bytes=(\d+)-(\d*)/.exec(req.headers.range || "");
  const type = req.url.endsWith(".mp4") ? "video/mp4" : "application/zip";
  const disp = req.url.endsWith(".zip") ? { "content-disposition": 'attachment; filename="archive.zip"' } : {};
  if (m) {
    const s = Number(m[1]);
    const e = m[2] ? Math.min(Number(m[2]), body.length - 1) : body.length - 1;
    res.writeHead(206, { "content-type": type, "content-range": `bytes ${s}-${e}/${body.length}`, "content-length": e - s + 1, "accept-ranges": "bytes", ...disp });
    return res.end(body.subarray(s, e + 1));
  }
  res.writeHead(200, { "content-type": type, "content-length": body.length, "accept-ranges": "bytes", ...disp });
  res.end(body);
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${server.address().port}`;

const env = { ...process.env, HURTLE_DATA: data, HURTLE_APP: join(root, "target/debug/hurtle"), HURTLE_SOCKET: join(work, "hurtle.sock"), HURTLE_APP_ID: "hurtle-test" };
const port = 9300 + Math.floor(Math.random() * 500);
const browser = process.env.HURTLE_BROWSER || "chromium";
console.log(`browser: ${browser}`);
const chrome = spawn(
  browser,
  [
    "--headless=new",
    `--user-data-dir=${profile}`,
    `--remote-debugging-port=${port}`,
    `--disable-extensions-except=${ext}`,
    `--load-extension=${ext}`,
    "--disable-features=DisableLoadExtensionCommandLineSwitch",
    "--no-first-run",
    "--no-default-browser-check",
    "--autoplay-policy=no-user-gesture-required",
    "about:blank",
  ],
  { env, stdio: "ignore" },
);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;
const check = (ok, what) => {
  console.log(`${ok ? "PASS" : "FAIL"}  ${what}`);
  if (!ok) failures++;
};

async function until(fn, ms = 10000, what = "condition") {
  const end = Date.now() + ms;
  for (;;) {
    const v = await Promise.resolve().then(fn).catch(() => undefined);
    if (v) return v;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(200);
  }
}

// ---- minimal CDP client
const version = await until(() => fetch(`http://127.0.0.1:${port}/json/version`).then((r) => r.json()), 15000, "chromium");
const ws = new WebSocket(version.webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let nextId = 1;
const pending = new Map();
ws.onmessage = (ev) => {
  const msg = JSON.parse(ev.data);
  if (msg.id && pending.has(msg.id)) {
    const { resolve, reject } = pending.get(msg.id);
    pending.delete(msg.id);
    msg.error ? reject(new Error(msg.error.message)) : resolve(msg.result);
  }
};
const send = (method, params = {}, sessionId) =>
  new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params, sessionId }));
  });
async function attach(targetId) {
  const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
  return sessionId;
}
async function evaluate(sessionId, expression) {
  const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true }, sessionId);
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
  return r.result.value;
}
const targets = async () => (await send("Target.getTargets")).targetInfos;

try {
  const sw = await until(
    async () => (await targets()).find((t) => t.type === "service_worker" && t.url.startsWith(`chrome-extension://${chromeId}/`)),
    15000,
    "extension service worker",
  );
  check(true, `extension loaded (${chromeId})`);
  const swSession = await attach(sw.targetId);
  // A just-installed extension's webRequest listener can miss the very first
  // requests; a real clip is fetched only once, so give it a moment.
  await sleep(1000);

  // 1. Media sniffing on a page with a <video>.
  const { targetId: pageId } = await send("Target.createTarget", { url: `${base}/page` });
  const page = await attach(pageId);
  await send("Page.enable", {}, page);
  const media = await until(async () => {
    const all = await evaluate(swSession, "chrome.storage.session.get(null)");
    const list = Object.entries(all).find(([k]) => k.startsWith("media:"))?.[1];
    return list?.length ? list : null;
  }, 10000, "sniffed media").catch(async (e) => {
    console.log("session storage:", JSON.stringify(await evaluate(swSession, "chrome.storage.session.get(null)")));
    console.log("tabs:", JSON.stringify(await evaluate(swSession, "chrome.tabs.query({}).then(t => t.map(x => [x.id, x.url, x.status]))")));
    throw e;
  });
  check(media[0].url.endsWith("/media/clip.mp4") && media[0].size === clip.length, `sniffed ${media[0].url} (${media[0].size} bytes)`);

  // 2. Hover the video → overlay button appears; click it → dialog opens.
  await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: 300, y: 180 }, page);
  const rect = await until(
    () => evaluate(page, `(() => { const h = document.querySelector('hurtle-overlay'); if (!h || h.style.display !== 'block') return null; const r = h.getBoundingClientRect(); return { x: r.left, y: r.top, w: r.width, h: r.height }; })()`),
    5000,
    "overlay button",
  );
  check(rect.w > 0, `overlay button shown at ${Math.round(rect.x)},${Math.round(rect.y)}`);
  // A video lower on the page: the button must sit above it, not on the picture.
  const above = await evaluate(page, `(async () => {
    const v = document.getElementById('v'); v.style.marginTop = '200px';
    v.dispatchEvent(new MouseEvent('mouseover', { bubbles: true }));
    await new Promise(r => setTimeout(r, 100));
    const h = document.querySelector('hurtle-overlay').getBoundingClientRect();
    const vr = v.getBoundingClientRect();
    v.style.marginTop = '0px';
    v.dispatchEvent(new MouseEvent('mouseover', { bubbles: true }));
    await new Promise(r => setTimeout(r, 100));
    return { bottom: h.bottom, videoTop: vr.top };
  })()`);
  check(above.bottom <= above.videoTop, `button sits above the video (button bottom ${Math.round(above.bottom)} ≤ video top ${Math.round(above.videoTop)})`);
  const rect2 = await evaluate(page, `(() => { const r = document.querySelector('hurtle-overlay').getBoundingClientRect(); return { x: r.left, y: r.top }; })()`);
  rect.x = rect2.x; rect.y = rect2.y;
  const cx = rect.x + 30;
  const cy = rect.y + 14;
  await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: cx, y: cy }, page);
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x: cx, y: cy, button: "left", clickCount: 1 }, page);
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x: cx, y: cy, button: "left", clickCount: 1 }, page);

  const dialogFor = async (seen) =>
    until(async () => (await targets()).find((t) => t.url.includes("/dialog.html") && !seen.has(t.targetId)), 8000, "dialog window");
  const seen = new Set();
  const d1 = await dialogFor(seen);
  seen.add(d1.targetId);
  const s1 = await attach(d1.targetId);
  const name = await until(() => evaluate(s1, `document.getElementById('filename').value || null`), 5000, "dialog filename");
  check(name === "My Test Video.mp4", `dialog named the file from the page title: "${name}"`);
  await until(() => evaluate(s1, `document.getElementById('connections').options.length > 0`), 5000, "dialog options");
  await evaluate(
    s1,
    `document.getElementById('connections').value = '4';
     document.getElementById('speedLimit').value = '2048';
     document.getElementById('start').click(); true`,
  );

  // The host launches Hurtle (this test's own instance) and adds the download.
  const store = join(data, "downloads.json");
  const added = await until(() => {
    if (!existsSync(store)) return null;
    const list = JSON.parse(readFileSync(store, "utf8"));
    return list.find((d) => d.filename === "My Test Video.mp4");
  }, 20000, "download in Hurtle");
  check(added.connections === 4 && added.speed_limit === 2048 * 1024, `Hurtle got it: ${added.connections} connections, limit ${added.speed_limit} B/s`);
  check(added.headers.some(([k, v]) => k === "Referer" && v.endsWith("/page")), "referrer forwarded");

  // 2b. The page also loads an HLS master playlist → the menu lists the
  // playing file (resolution, length) and the stream (its qualities).
  await evaluate(page, `fetch('/hls/master.m3u8').then(r => r.text()).then(() => true)`);
  const stream = await until(async () => {
    const all = await evaluate(swSession, "chrome.storage.session.get(null)");
    const list = Object.entries(all).find(([k]) => k.startsWith("media:"))?.[1] || [];
    return list.find((x) => x.kind === "stream" && x.qualities);
  }, 10000, "stream manifest read");
  check(
    stream.qualities.join() === "720,360" && Math.round(stream.duration) === 6 && !stream.live,
    `stream manifest read: qualities ${stream.qualities.join("/")}, ${stream.duration}s`,
  );
  await evaluate(page, `document.getElementById('v').play().then(() => new Promise(r => setTimeout(r, 300))).then(() => true)`);
  await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: 300, y: 180 }, page);
  await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: cx, y: cy }, page);
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x: cx, y: cy, button: "left", clickCount: 1 }, page);
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x: cx, y: cy, button: "left", clickCount: 1 }, page);
  // The overlay's shadow root is closed; read it the way DevTools can.
  const menuText = await until(async () => {
    const { root: doc } = await send("DOM.getDocument", { depth: -1, pierce: true }, page);
    const texts = [];
    const walk = (n) => {
      if (n.nodeName === "HURTLE-OVERLAY") {
        const collect = (x) => {
          if (x.nodeType === 3) texts.push(x.nodeValue);
          if (x.nodeName === "STYLE") return;
          for (const c of [...(x.shadowRoots || []), ...(x.children || [])]) collect(c);
        };
        collect(n);
      }
      for (const c of n.children || []) walk(c);
    };
    walk(doc);
    const t = texts.join(" | ");
    // Wait for yt-dlp's answer about the page too (the count shows "…" until then).
    return t.includes("Found 2 videos") && t.includes("Playing") && !t.includes("| … |") ? t : null;
  }, 20000, "overlay menu");
  check(menuText.includes("360p · 640×360") && menuText.includes("0:08"), "menu shows the playing video's resolution and length");
  check(menuText.includes("720p") && menuText.includes("HLS stream") && menuText.includes("0:06"), "menu shows the stream's qualities and length");
  // The button says a menu opens (▾) and how many sizes/videos it offers.
  const count = /Download video \| ([^|]+?) \| ▾/.exec(menuText)?.[1];
  check(/^\d+ (sizes|found)$|^1 size$/.test(count || ""), `button shows the count and a caret: "${count} ▾"`);
  if (process.env.E2E_DEBUG) console.log(menuText);
  check(menuText.includes("My Test Video"), "menu items are named after the page, not the file");
  const shots = process.env.HURTLE_SHOTS;
  const shoot = async (session, name) => {
    const { data } = await send("Page.captureScreenshot", { format: "png" }, session);
    writeFileSync(join(shots, name), Buffer.from(data, "base64"));
  };
  if (shots) await shoot(page, "overlay-menu.png");
  // The popup, pointed at the test page's tab: its rows, then a click on the page row.
  {
    const { targetId: popId } = await send("Target.createTarget", { url: `chrome-extension://${chromeId}/popup.html` });
    const pop = await attach(popId);
    await send("Page.enable", {}, pop);
    await until(() => evaluate(pop, `typeof loadMedia === 'function'`), 5000, "popup");
    await evaluate(pop, `(async () => {
      const [t] = await chrome.tabs.query({ url: '${base}/page' });
      chrome.tabs.query = async () => [t];
      await loadMedia();
      await new Promise(r => setTimeout(r, 1500));
      return true;
    })()`);
    const rows = await evaluate(pop, `[...document.querySelectorAll('#media .item .title')].map(t => t.textContent)`);
    check(rows.length === 3 && rows[0] === "My Test Video", `popup lists the page and its videos: ${rows.join(" | ")}`);
    if (shots) {
      await send("Emulation.setDeviceMetricsOverride", { width: 400, height: 640, deviceScaleFactor: 1, mobile: false }, pop);
      await shoot(pop, "popup.png");
    }
    const folders = await until(() => evaluate(pop, `(() => { const m = document.getElementById('dir-main'); return m?.value ? [m.value, document.querySelectorAll('#folders input').length, document.getElementById('dir-video').placeholder] : null; })()`), 5000, "popup folders");
    check(folders[0].endsWith("/downloads") && folders[1] === 7 && /^Main \(/.test(folders[2]), `popup shows the main folder and 6 type folders: ${folders[0]}`);
    if (shots) {
      await evaluate(pop, `document.getElementById('folders').scrollIntoView(); true`);
      await shoot(pop, "popup-folders.png");
    }
    // window.close() would end the target before we can look; keep it open.
    await evaluate(pop, `window.close = () => (window.__closed = true); document.querySelector('#media .item').click(); true`);
    const dp = await dialogFor(seen);
    seen.add(dp.targetId);
    const sp = await attach(dp.targetId);
    const dpUrl = await until(() => evaluate(sp, `document.getElementById('url').value || null`), 5000, "dialog from popup");
    check(dpUrl === `${base}/page` && (await evaluate(pop, `!!window.__closed`)), "clicking the popup's page row opens the dialog and closes the popup");
    const hint = await until(() => evaluate(sp, `document.getElementById('quality').disabled ? null : document.getElementById('audio').selectedOptions[0]?.text`), 30000, "dialog from popup probe");
    check(hint === "Only one audio track", `single-track page says so: "${hint}"`);
    await evaluate(sp, `document.getElementById('cancel').click(); true`);
    await send("Target.closeTarget", { targetId: popId });
  }
  await evaluate(page, `document.getElementById('v').pause(); true`);

  // 2c. The stream's "720p" chip → dialog with 720p already picked.
  const chip = await (async () => {
    const { root: doc } = await send("DOM.getDocument", { depth: -1, pierce: true }, page);
    let found = null;
    const walk = (n) => {
      if (found) return;
      const cls = n.attributes?.[n.attributes.indexOf("class") + 1] || "";
      if (n.attributes?.includes("class") && cls.split(" ").includes("chip") && n.children?.[0]?.nodeValue === "720p") found = n;
      for (const c of [...(n.shadowRoots || []), ...(n.children || [])]) walk(c);
    };
    walk(doc);
    return found;
  })();
  const box = await send("DOM.getBoxModel", { backendNodeId: chip.backendNodeId }, page);
  const [qx, qy] = [(box.model.content[0] + box.model.content[2]) / 2, (box.model.content[1] + box.model.content[5]) / 2];
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x: qx, y: qy, button: "left", clickCount: 1 }, page);
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x: qx, y: qy, button: "left", clickCount: 1 }, page);
  const dq = await dialogFor(seen);
  seen.add(dq.targetId);
  const sq = await attach(dq.targetId);
  const picked = await until(
    () => evaluate(sq, `(() => { const q = document.getElementById('quality'); const err = document.getElementById('error').textContent; return err ? 'ERROR ' + err : q.disabled ? null : q.selectedOptions[0].text; })()`),
    30000,
    "quality list for the chip",
  );
  check(picked.startsWith("720p"), `chip opened the dialog with "${picked}" picked`);
  check((await evaluate(sq, `document.getElementById('url').value`)).endsWith("/hls/master.m3u8"), "a stream on a plain <video> page stays the stream");
  await evaluate(sq, `document.getElementById('cancel').click(); true`);

  // 3. A plain browser download is intercepted → dialog → "Download later".
  await evaluate(swSession, `chrome.downloads.download({ url: '${base}/files/archive.zip' }).then(() => true)`);
  const d2 = await dialogFor(seen);
  const s2 = await attach(d2.targetId);
  await until(() => evaluate(s2, `document.getElementById('url').value || null`), 5000, "second dialog");
  await evaluate(s2, `document.getElementById('later').click(); true`);
  const later = await until(() => {
    const list = JSON.parse(readFileSync(store, "utf8"));
    return list.find((d) => d.url.endsWith("/files/archive.zip"));
  }, 10000, "intercepted download in Hurtle");
  check(later.status?.state === "paused", `intercepted browser download queued paused in Hurtle (status ${JSON.stringify(later.status)})`);
  const browserItems = await evaluate(swSession, `chrome.downloads.search({ urlRegex: 'archive' }).then(r => r.length)`);
  check(browserItems === 0, "browser's own copy was cancelled and erased");

  // 4. Global settings round-trip through the host.
  const r = await evaluate(swSession, `chrome.runtime.sendNativeMessage('io.github.arafays.hurtle', { type: 'settings', speedLimit: 1048576 })`);
  check(r.ok && r.settings.speed_limit === 1048576, "global speed limit set via native host");

  // Folders by type, set the way the popup does: videos get their own.
  const videos = join(work, "videos");
  const f = await evaluate(swSession, `chrome.runtime.sendNativeMessage('io.github.arafays.hurtle', { type: 'settings', typeDirs: { video: ${JSON.stringify(videos)}, image: '' } })`);
  check(f.ok && f.settings.type_dirs.video === videos && !f.settings.type_dirs.image, "video folder set via native host");
  const bad = await evaluate(swSession, `chrome.runtime.sendNativeMessage('io.github.arafays.hurtle', { type: 'settings', typeDirs: { video: 'relative/dir' } })`);
  check(!bad.ok && /full path/.test(bad.error), `a relative folder is refused: ${bad.error}`);

  // 5. HLS stream → dialog in video mode: yt-dlp lists qualities, Hurtle fetches and muxes it.
  await evaluate(
    swSession,
    `openDialog({ url: '${base}/hls/stream.m3u8', media: true, filename: 'HLS Clip', referrer: '${base}/page' }).then(() => true)`,
  );
  const d3 = await dialogFor(seen);
  const s3 = await attach(d3.targetId);
  const labels = await until(
    () => evaluate(s3, `(() => { const q = document.getElementById('quality'); const err = document.getElementById('error').textContent; return err ? ['ERROR ' + err] : q.disabled ? null : [...q.options].map(o => o.text); })()`),
    30000,
    "quality list",
  );
  check(labels.length >= 1 && labels[0].startsWith("Best"), `dialog lists qualities via yt-dlp: ${labels.join(" | ")}`);
  check((await evaluate(s3, `document.getElementById('browser').hidden`)) === true, "'Use browser' hidden for video pages");
  const saveTo = await until(() => evaluate(s3, `document.getElementById('dir').value && [document.getElementById('dir').value, document.getElementById('dirHint').textContent]`), 5000, "Save to folder");
  check(saveTo[0].endsWith("/videos") && /Videos folder/.test(saveTo[1]), `dialog shows where it goes: ${saveTo.join(" ")}`);
  if (shots) {
    await send("Emulation.setDeviceMetricsOverride", { width: 540, height: 560, deviceScaleFactor: 1, mobile: false }, s3);
    await sleep(300);
    await shoot(s3, "dialog-video.png");
  }
  await evaluate(s3, `document.getElementById('start').click(); true`);
  const vid = await until(() => {
    const list = JSON.parse(readFileSync(store, "utf8"));
    const d = list.find((d) => d.kind === "media");
    return d?.status?.state === "completed" || d?.status?.state === "failed" ? d : null;
  }, 60000, "HLS download to finish");
  check(vid.status.state === "completed" && vid.filename === "HLS Clip.mp4", `HLS saved as "${vid.filename}" (${JSON.stringify(vid.status)})`);
  check(vid.path?.startsWith(videos + "/"), `video saved in the Videos folder: ${vid.path}`);
  const dur = Number(execFileSync("ffprobe", ["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", vid.path]).toString());
  check(Math.abs(dur - 6) < 0.5, `merged file plays: ${dur.toFixed(2)}s`);
  // 7. Torrents. A magnet link click opens the dialog, which lists the files
  //    (the metadata comes from the peer); only the video is downloaded.
  const { targetId: tId } = await send("Target.createTarget", { url: `${base}/torrent` });
  const tp = await attach(tId);
  await until(() => evaluate(tp, `document.readyState === 'complete' && document.getElementById('m') ? true : null`), 10000, "torrent page");
  await sleep(500); // content scripts run at document_idle
  // A script's click must not start anything; only the user's own does.
  // (The page stops the link's own navigation afterwards: a pending "open
  // the torrent app?" prompt would swallow the real click below.)
  await evaluate(tp, `(() => { addEventListener('click', (e) => e.isTrusted || e.preventDefault()); document.getElementById('m').click(); return true; })()`);
  await sleep(1500);
  const sneaky = (await targets()).some((t) => t.url.includes("/dialog.html") && !seen.has(t.targetId));
  check(!sneaky, "a page's scripted click on a magnet link is ignored");
  const ml = await evaluate(tp, `(() => { const r = document.getElementById('m').getBoundingClientRect(); return { x: r.left + 10, y: r.top + r.height / 2 }; })()`);
  await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: ml.x, y: ml.y }, tp);
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x: ml.x, y: ml.y, button: "left", clickCount: 1 }, tp);
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x: ml.x, y: ml.y, button: "left", clickCount: 1 }, tp);
  const d4 = await dialogFor(seen);
  seen.add(d4.targetId);
  const s4 = await attach(d4.targetId);
  const files = await until(
    () => evaluate(s4, `(() => { const b = document.querySelectorAll('#files input[type=checkbox]'); return b.length ? [...document.querySelectorAll('#files .path')].map(p => p.title) : null; })()`),
    60000,
    "torrent file list",
  );
  check(files.length === 2 && files.some((f) => f.endsWith("clip.mp4")), `magnet click → dialog lists the torrent's files: ${files.join(", ")}`);
  const tinfo = await evaluate(s4, `(() => { document.getElementById('mediaFiles').click(); return { name: document.getElementById('filename').value, hint: document.getElementById('torrentHint').textContent, play: !document.getElementById('streamNow').disabled, conns: document.getElementById('connectionsCol').hidden }; })()`);
  check(tinfo.name === "Pack" && /1 of 2/.test(tinfo.hint) && tinfo.play && tinfo.conns, `torrent mode: ${JSON.stringify(tinfo)}`);
  if (shots) {
    await send("Emulation.setDeviceMetricsOverride", { width: 520, height: 620, deviceScaleFactor: 1, mobile: false }, s4);
    await sleep(300);
    await shoot(s4, "dialog-torrent.png");
  }
  await evaluate(s4, `document.getElementById('start').click(); true`);
  const tor = await until(() => {
    const list = JSON.parse(readFileSync(store, "utf8"));
    const d = list.find((d) => d.kind === "torrent");
    return d?.status?.state === "completed" || d?.status?.state === "failed" ? d : null;
  }, 60000, "torrent download to finish");
  const got = tor.path && existsSync(join(tor.path, "clip.mp4")) ? readFileSync(join(tor.path, "clip.mp4")) : null;
  check(tor.status.state === "completed" && got?.equals(clip), `torrent downloaded the chosen file byte for byte into ${tor.path} (${JSON.stringify(tor.status)})`);
  check(tor.torrent?.files?.filter((f) => f.selected).length === 1, "only the chosen file is wanted");

  //    A .torrent download is caught too: the dialog reads the file the browser fetched.
  // A minimum size (meant for the content) must not let small .torrent files through.
  await evaluate(swSession, `chrome.storage.local.set({ minSizeMB: 5 }).then(() => true)`);
  await evaluate(swSession, `chrome.downloads.download({ url: '${base}/files/pack.torrent' }).then(() => true)`);
  const d5 = await dialogFor(seen);
  seen.add(d5.targetId);
  const s5 = await attach(d5.targetId);
  const files2 = await until(() => evaluate(s5, `document.querySelectorAll('#files input[type=checkbox]').length || null`), 20000, ".torrent file list");
  check(files2 === 2, ".torrent download → dialog lists its files (with a 5 MB minimum size set)");
  await evaluate(swSession, `chrome.storage.local.set({ minSizeMB: 0 }).then(() => true)`);
  await evaluate(s5, `document.getElementById('cancel').click(); true`);
  const tItems = await until(async () => ((await evaluate(swSession, `chrome.downloads.search({ urlRegex: 'pack\\.torrent' }).then(r => r.length)`)) === 0 ? true : null), 5000, "browser copy dropped").catch(() => false);
  check(tItems, "cancelling drops the browser's .torrent download");

  // 6. Debug logs: the extension's own and Hurtle's file (host, yt-dlp, app).
  const extLog = (await evaluate(swSession, `chrome.storage.local.get('debugLog').then(r => r.debugLog || [])`)).join("\n");
  check(/native probe .*ok=true/.test(extLog) && /dialog start/.test(extLog), "extension debug log records probes and dialog actions");
  const fileLog = readFileSync(join(data, "logs/hurtle.log"), "utf8");
  if (process.env.E2E_DEBUG) console.log(fileLog);
  check(/host\[\d+\] probe .* ok in .*delivered/.test(fileLog), "host logs each request and whether the reply was delivered");
  check(/yt-dlp\[\d+\] probe exited/.test(fileLog) && /app\[\d+\] #\d+ \S+ completed/.test(fileLog), "yt-dlp runs and finished downloads are in hurtle.log");
  check(!/Cookie:|PHPSESSID/.test(fileLog + extLog), "no cookie values in the logs");
  const { targetId: logId } = await send("Target.createTarget", { url: `chrome-extension://${chromeId}/log.html` });
  const ls = await attach(logId);
  const logPage = await until(() => evaluate(ls, `(() => { const t = document.getElementById('host').textContent; return t.includes('probe') ? { env: document.getElementById('env').textContent, ext: document.getElementById('ext').textContent } : null; })()`), 10000, "debug log page");
  check(/Native host +reachable/.test(logPage.env) && /Site access +granted/.test(logPage.env) && logPage.ext.includes("native probe"), "debug log page shows setup, extension and Hurtle logs");
  if (shots) await shoot(ls, "log.png");
} catch (e) {
  failures++;
  console.log(`FAIL  ${e.message}`);
} finally {
  ws.close();
  chrome.kill();
  seeder.kill();
  server.close();
  // Stop the Hurtle instance this test started (matched by its private socket dir).
  spawn("pkill", ["-f", `^${join(root, "target/debug/hurtle")}$`]).on("exit", () => {
    setTimeout(() => rmSync(work, { recursive: true, force: true }), 500);
  });
}
console.log(failures ? `\n${failures} check(s) failed` : "\nall checks passed");
process.exitCode = failures ? 1 : 0;
