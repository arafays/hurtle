// Firefox-family e2e (Firefox, Zen, LibreWolf…): a throwaway headless
// profile driven over WebDriver BiDi, with the unsigned add-on installed.
//
//   HURTLE_FIREFOX=/opt/zen-browser-bin/zen-bin node extension/test/e2e-firefox.mjs
//
// The native host is the one registered for the user (~/.mozilla), so run
// `mise run install` first; Hurtle itself runs with a private HURTLE_DATA and
// HURTLE_SOCKET, never the user's.
//
// Checks: a video page → dialog → yt-dlp's qualities and audio row show up.

import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dirname, "../..");
const xpi = join(root, "extension/dist", readdirSync(join(root, "extension/dist")).find((f) => f.endsWith("-unsigned.xpi")));
const work = mkdtempSync(join(tmpdir(), "hurtle-ff-"));
const profile = join(work, "profile");
const data = join(work, "data");
mkdirSync(profile, { recursive: true });
mkdirSync(join(data, "downloads"), { recursive: true });
writeFileSync(join(data, "settings.json"), JSON.stringify({ download_dir: join(data, "downloads"), connections: 8 }));
const UUID = "5a9e0000-0000-4000-8000-00000000e2e0";
const prefs = {
  "xpinstall.signatures.required": false,
  "extensions.webextensions.uuids": JSON.stringify({ "hurtle@arafays.github.io": UUID }),
  "extensions.autoDisableScopes": 0,
  "browser.shell.checkDefaultBrowser": false,
  "datareporting.policy.dataSubmissionEnabled": false,
  "toolkit.telemetry.reportingpolicy.firstRun": false,
  "browser.aboutwelcome.enabled": false,
  "zen.welcome-screen.seen": true,
};
writeFileSync(join(profile, "user.js"), Object.entries(prefs).map(([k, v]) => `user_pref(${JSON.stringify(k)}, ${JSON.stringify(v)});`).join("\n"));

// A local page with a <video> that has no plain file (like YouTube's).
const server = createServer((req, res) => {
  res.writeHead(200, { "content-type": "text/html" });
  res.end(`<!doctype html><title>Local Test Clip - FFTube</title><body style="margin:0;padding-top:60px">
    <video width="640" height="360" style="background:#000"></video></body>`);
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const pageUrl = process.env.HURTLE_VIDEO || `http://127.0.0.1:${server.address().port}/watch`;
const browser = process.env.HURTLE_FIREFOX || "firefox";
const port = 9800 + Math.floor(Math.random() * 150);
const env = { ...process.env, HURTLE_DATA: data, HURTLE_SOCKET: join(work, "s.sock"), MOZ_NO_REMOTE: "1", MOZ_REMOTE_ALLOW_SYSTEM_ACCESS: "1", HURTLE_APP_ID: "hurtle-test" };
const ff = spawn(browser, ["--headless", "--profile", profile, "--no-remote", `--remote-debugging-port=${port}`], { env, stdio: ["ignore", "ignore", "pipe"] });
let stderr = "";
ff.stderr.on("data", (d) => (stderr += d));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;
const check = (ok, what) => {
  console.log(`${ok ? "PASS" : "FAIL"}  ${what}`);
  if (!ok) failures++;
};
async function until(fn, ms, what) {
  const end = Date.now() + ms;
  for (;;) {
    const v = await Promise.resolve().then(fn).catch(() => undefined);
    if (v) return v;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(300);
  }
}

// ---- minimal WebDriver BiDi client
await until(() => /WebDriver BiDi listening/.test(stderr), 20000, `${browser} to start`);
const ws = new WebSocket(`ws://127.0.0.1:${port}/session`);
await new Promise((r, j) => ((ws.onopen = r), (ws.onerror = j)));
let nextId = 1;
const pending = new Map();
ws.onmessage = (ev) => {
  const m = JSON.parse(ev.data);
  if (m.id && pending.has(m.id)) {
    const { resolve, reject } = pending.get(m.id);
    pending.delete(m.id);
    m.type === "error" ? reject(new Error(`${m.error}: ${m.message}`)) : resolve(m.result);
  }
};
const send = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params }));
  });
const evaluate = async (context, expression) => {
  const r = await send("script.evaluate", { expression, target: { context }, awaitPromise: true, resultOwnership: "none", serializationOptions: { maxObjectDepth: 4 } });
  if (r.type === "exception") throw new Error(r.exceptionDetails.text);
  return unwrap(r.result);
};
// BiDi's remote values → plain JS.
function unwrap(v) {
  if (!v) return v;
  switch (v.type) {
    case "undefined": case "null": return null;
    case "string": case "number": case "boolean": return v.value;
    case "array": return v.value.map(unwrap);
    case "object": return Object.fromEntries(v.value.map(([k, x]) => [typeof k === "string" ? k : unwrap(k), unwrap(x)]));
    default: return v.value ?? null;
  }
}
const contexts = async () => (await send("browsingContext.getTree", {})).contexts;

let dialog;
try {
  await send("session.new", { capabilities: { alwaysMatch: { webSocketUrl: true } } });
  const { extension } = await send("webExtension.install", { extensionData: { type: "archivePath", path: xpi } });
  check(extension === "hurtle@arafays.github.io", `add-on installed from ${xpi.split("/").pop()}`);

  // A page with a <video> but no direct file: the button asks yt-dlp to read
  // the page. HURTLE_VIDEO (a real site) skips the button and asks from an
  // extension page instead, since every site places its player differently.
  const { context: tab } = await send("browsingContext.create", { type: "tab" });
  const before = new Set((await contexts()).map((c) => c.context));
  if (process.env.HURTLE_VIDEO) {
    await send("browsingContext.navigate", { context: tab, url: `moz-extension://${UUID}/log.html`, wait: "complete" });
    await evaluate(tab, `browser.runtime.sendMessage({ type: "download-page", pageUrl: ${JSON.stringify(pageUrl)}, title: "Placeholder title" })`);
    before.add(tab);
  } else {
    await send("browsingContext.navigate", { context: tab, url: pageUrl, wait: "complete" });
    let box = await until(async () => {
      await send("input.performActions", { context: tab, actions: [{ type: "pointer", id: "m", actions: [{ type: "pointerMove", x: 300, y: 200 }, { type: "pointerMove", x: 310, y: 210 }] }] });
      return evaluate(tab, `(() => { const h = document.querySelector('hurtle-overlay'); if (!h || h.style.display !== 'block') return null; const r = h.getBoundingClientRect(); return { x: r.left, y: r.top }; })()`);
    }, 15000, "the button over the video");
    check(true, "button shown over the video (content script runs)");
    // While yt-dlp reads the page the button opens a menu ("…"); this page
    // has no real video, so once that answer is in, a click goes to the dialog.
    await sleep(4000);
    // The button is right-aligned and its width changed with the count.
    Object.assign(box, await evaluate(tab, `(() => { const r = document.querySelector('hurtle-overlay').getBoundingClientRect(); return { x: r.left, y: r.top }; })()`));
    await send("input.performActions", {
      context: tab,
      actions: [{ type: "pointer", id: "m", actions: [{ type: "pointerMove", x: Math.round(box.x + 30), y: Math.round(box.y + 14) }, { type: "pointerDown", button: 0 }, { type: "pointerUp", button: 0 }] }],
    });
  }
  dialog = await until(async () => (await contexts()).find((c) => !before.has(c.context) && c.url.includes("/dialog.html")), 10000, "dialog window");
  check(true, "dialog opened");
  const started = Date.now();
  const state = await until(
    () => evaluate(dialog.context, `(() => {
      const q = document.getElementById('quality');
      const err = document.getElementById('error').textContent;
      if (q.disabled && !err) return null;
      return { options: [...q.options].map(o => o.text), err, audio: document.getElementById('audioRow').hidden ? null : [...document.getElementById('audio').options].map(o => o.text), name: document.getElementById('filename').value, thumb: !document.getElementById('thumb').hidden };
    })()`),
    120000,
    "the dialog's quality list",
  );
  console.log(`      after ${((Date.now() - started) / 1000).toFixed(1)}s: ${JSON.stringify(state)}`);
  check(state.audio?.length > 0, `audio row shown: ${state.audio?.join(" | ")}`);
  if (process.env.HURTLE_VIDEO) {
    check(!state.err && state.options.length > 2, `qualities listed: ${state.options.join(" | ")}`);
    check(state.name && state.name !== "Placeholder title", `named after the video: "${state.name}"`);
    // What the page button and popup show: sizes with their frame size.
    const sizes = await evaluate(tab, `browser.runtime.sendMessage({ type: "page-sizes", pageUrl: ${JSON.stringify(pageUrl)} })`);
    check(sizes?.ok && sizes.sizes.length > 0 && sizes.sizes.every((z) => z.height), `page sizes: ${sizes?.sizes?.map((z) => `${z.label} ${z.width}×${z.height}`).join(", ")}`);
  } else {
    // The local page has no real video: yt-dlp's answer must still arrive.
    check(/no downloadable video/.test(state.err), `probe answer reached the dialog: ${state.err}`);
  }
} catch (e) {
  failures++;
  console.log(`FAIL  ${e.message}`);
}
if (failures && !dialog) {
  const { context: logTab } = await send("browsingContext.create", { type: "tab" });
  await send("browsingContext.navigate", { context: logTab, url: `moz-extension://${UUID}/log.html`, wait: "complete" }).catch(() => {});
  console.log(await evaluate(logTab, `browser.runtime.sendMessage({ type: "debug-read" }).then(r => r.debugLog.join("\\n"))`).catch((e) => e.message));
}
if (failures && dialog) {
  // What the dialog shows, and the extension's own log.
  const seen = await evaluate(dialog.context, `({ quality: document.getElementById('quality').options[0]?.text, error: document.getElementById('error').textContent, name: document.getElementById('filename').value })`).catch((e) => e.message);
  console.log("dialog:", JSON.stringify(seen));
  const log = await evaluate(dialog.context, `browser.runtime.sendMessage({ type: "debug-read" }).then(r => r.debugLog.join("\\n"))`).catch((e) => e.message);
  console.log(log);
}
ws.close();
ff.kill();
server.close();
setTimeout(() => rmSync(work, { recursive: true, force: true }), 1000);
console.log(failures ? `\n${failures} check(s) failed` : "\nall checks passed");
process.exitCode = failures ? 1 : 0;
