// The popup's "Open the debug log": the extension's own log, Hurtle's log
// file (app, native host, yt-dlp) and the setup facts that matter most.
const api = globalThis.browser ?? globalThis.chrome;
const $ = (id) => document.getElementById(id);

function show(el, text) {
  el.replaceChildren();
  for (const line of (text || "(empty)").split("\n")) {
    const span = document.createElement("span");
    if (/\bERROR\b|FAILED|NOT delivered|error "/.test(line)) span.className = "error";
    span.textContent = line + "\n";
    el.append(span);
  }
  el.scrollTop = el.scrollHeight;
}

async function env() {
  const m = api.runtime.getManifest();
  const access = await api.permissions.contains({ origins: ["<all_urls>"] }).catch(() => false);
  const ping = await api.runtime.sendMessage({ type: "ping" }).catch((e) => ({ ok: false, error: String(e?.message || e) }));
  const settings = await api.storage.local.get(null);
  const lines = [
    `Extension      ${m.name} ${m.version} (${api.runtime.id})`,
    `Browser        ${navigator.userAgent}`,
    `Site access    ${access ? "granted on all sites" : "NOT GRANTED: the button over videos, media detection and cookies don't work. Grant it from the toolbar popup."}`,
    `Native host    ${ping?.ok ? `reachable, hurtle-host ${ping.version}${ping.error ? ` (${ping.error})` : ", app running"}` : `NOT reachable: ${ping?.error || "no answer"}`}`,
    `Settings       ${JSON.stringify(Object.fromEntries(Object.entries(settings).filter(([k]) => !["debugLog", "status", "history"].includes(k))))}`,
  ];
  if (settings.status) lines.push(`Last status    ${settings.status.ok ? "ok" : "error"}: ${settings.status.message} (${new Date(settings.status.at).toLocaleString()})`);
  return lines.join("\n");
}

async function load() {
  const [e, ext, host] = await Promise.all([
    env(),
    api.runtime.sendMessage({ type: "debug-read" }).catch(() => null),
    api.runtime.sendMessage({ type: "debug-host-log" }).catch((err) => ({ ok: false, error: String(err?.message || err) })),
  ]);
  show($("env"), e);
  $("debug").checked = !!ext?.debug;
  show($("ext"), (ext?.debugLog || []).join("\n"));
  if (host?.ok) {
    show($("host"), host.log?.trimEnd());
    $("hostPath").textContent = "(from $HURTLE_DATA/logs/hurtle.log, e.g. ~/.local/share/hurtle/logs/hurtle.log)";
  } else {
    show($("host"), `Couldn't read it through the native host: ${host?.error || "no answer"}. The file is $HURTLE_DATA/logs/hurtle.log (usually ~/.local/share/hurtle/logs/hurtle.log).`);
  }
}

const all = () => ["== Setup", $("env").textContent, "== Browser extension", $("ext").textContent, "== Hurtle", $("host").textContent].join("\n");

$("refresh").onclick = load;
$("copy").onclick = async () => {
  await navigator.clipboard.writeText(all());
  $("copy").textContent = "Copied";
  setTimeout(() => ($("copy").textContent = "Copy all"), 1500);
};
$("save").onclick = () => {
  const a = document.createElement("a");
  a.href = URL.createObjectURL(new Blob([all()], { type: "text/plain" }));
  a.download = `hurtle-debug-${new Date().toISOString().replace(/[:.]/g, "-")}.txt`;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 5000);
};
$("clear").onclick = async () => {
  await api.runtime.sendMessage({ type: "debug-clear" });
  load();
};
$("debug").onchange = () => api.storage.local.set({ debug: $("debug").checked });

load();
