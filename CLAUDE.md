# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Hurtle is an IDM-style download accelerator for Linux, Windows and macOS. It has three parts:
- A Rust engine that downloads a file as byte-range segments over parallel HTTP/1.1 connections.
- A GPUI desktop app with a tray icon.
- An MV3 browser extension for Chrome-family browsers and Firefox. It reaches the app through a native-messaging host.

Video sites and HLS/DASH streams are handed to yt-dlp. Magnet links and `.torrent` files go through librqbit, and any download can be streamed to a player (VLC first) over local HTTP.

## Commands

Everything runs through mise. The extension's JS dev dependencies use aube (`aube install`, `aube exec …`), never npm/npx.

```bash
mise run dev [url]        # app, debug build; HURTLE_DATA=./.dev-data keeps your real list untouched
mise run test             # cargo test -p hurtle-core
cargo test -p hurtle-core media::tests::limits   # a single test
mise run lint             # clippy -D warnings + web-ext lint of dist/firefox
mise run fmt
mise run ext:build        # extension/dist/{chrome,firefox} + zips (generates both manifests)
mise run ext:e2e          # headless browser e2e: extension + hurtle-host + app + yt-dlp HLS
HURTLE_BROWSER=/usr/bin/brave node extension/test/e2e.mjs   # e2e in another Chromium-family browser
mise run ext:e2e-firefox  # headless Firefox-family e2e over WebDriver BiDi (installed host; HURTLE_FIREFOX=/opt/zen-browser-bin/zen-bin, HURTLE_VIDEO=<real video page>)
mise run serve <file> [KiB/s] / mise run bench   # throttled local server, 1 vs 8 connections
cargo run -p hurtle-core --example media_ops -- <url> <out-dir>   # yt-dlp limit-restart/pause/resume check
cargo run -p hurtle-core --example pause_resume                   # segment resume across engine restarts
cargo run -p hurtle-core --example torrent                        # BitTorrent over loopback: magnet+x.pe, file choice, mid-download range stream, restart/resume
cargo run -p hurtle-core --example seed -- <path> [KiB/s]         # seed a path on 127.0.0.1, print a magnet (and write <name>.torrent to $TMPDIR)
```

`mise run install` builds a release, copies the binaries to `~/.local/bin`, and writes the native-messaging manifests into every existing Chrome-family config dir and into `~/.mozilla`. Zen and other Firefox forks also read `~/.mozilla`. This changes the user's browser config, so leave it to the user unless they ask. With the AMO keys in `.env.local` (only those two lines are read) it also submits the Firefox build as a new unlisted AMO version (`ext:submit`, no waiting) and checks for ~90s; `mise run ext:fetch-signed <version>` (`scripts/amo-signed.py`) gets the xpi once approved. `HURTLE_SIGN=0` skips it, so don't run it just to test. CI's `ext:sign` still waits, since the release needs the xpi. A running `hurtle` keeps its old binary until it is restarted.

The e2e test runs in isolation: a throwaway profile with a profile-local `NativeMessagingHosts`, and its own `HURTLE_DATA`, `HURTLE_SOCKET` and `HURTLE_APP`. Brave is the exception: it prefers the user-level manifest, so it runs the *installed* `~/.local/bin/hurtle-host`. A Brave failure can therefore just mean the install is stale.

**Cross-platform checks.** `mise run check:cross` (`scripts/cross-check.sh`) runs clippy for Windows (all crates) and macOS (core, host and CLI) through zig. Tool versions are pinned in `mise.lock`, and CI sets `MISE_LOCKED=1`. Linux is the only OS that can be run here. To type-check Windows and macOS, cross-compile with zig as the C compiler: it is needed for `aws-lc-sys`, and the Windows app also needs a stub `x86_64-w64-mingw32-windres`. The macOS GUI can't be checked from Linux, because gpui's Metal shaders need Xcode. CI (`.github/workflows/ci.yml`) runs clippy and tests on all three OSes.

**Releases.** Pushing a tag `v<version>` runs `.github/workflows/release.yml`:
- The tag must equal `Cargo.toml`'s version.
- `scripts/package.sh <triple>` builds the per-OS archives. macOS gets an ad-hoc-signed `Hurtle.app` with the host inside `Contents/MacOS`.
- The extension zips are built too, plus a signed `.xpi` if AMO secrets exist.
- The add-on is unlisted on AMO, so installed copies update from `docs/updates.json` (the gecko `update_url`, on GitHub Pages). After a stable release, the `firefox-update` job adds the signed xpi there with `scripts/firefox-update.sh` and pushes to main. Run that script by hand if a release was signed outside CI.
- Linux build dependencies for GPUI are in `scripts/ci-linux-deps.sh`.
- Workflows are linted with `mise run ci:lint` (actionlint).

**Installers and CI.**
- `docs/install.sh` and `docs/install.ps1` are the one-line installers. `scripts/package-deb.sh` (also writes the system-wide native-messaging manifests) and `package-dmg.sh` build packages; `packaging/{linux/aur,windows,macos}` hold the AUR, winget/Scoop and Homebrew templates, filled by `scripts/update-aur.sh` and `scripts/update-manifests.sh`.
- CI has a `verify` gate that the release needs, an `e2e` job, and cargo-deny (`mise run ci:deny`, config in `deny.toml`). Actions are pinned by SHA.

Unix socket paths must stay under about 108 bytes. Put test sockets in `$XDG_RUNTIME_DIR`, not in deep temp dirs.

## Architecture

```
browser ext ──native messaging──► hurtle-host ──Unix socket (JSON lines)──► hurtle (app) ─► Engine (hurtle-core)
                                     │ launches app if not running                         ├─ job.rs   segmented HTTP
                                     └ answers `probe` itself via yt-dlp                   └─ media.rs yt-dlp child
hurtle-cli ── uses Engine directly (data in $HURTLE_DATA/cli), or --send over the socket
```

### hurtle-core
- `Engine` owns its own tokio runtime. The GUI never runs tokio: it polls `engine.snapshot()` every 250ms and re-renders only when a fingerprint in `view.rs` changes. If you add a field the UI shows, add it to that fingerprint.
- Lock order is always `path_claim` → `jobs` → a job's `rec`. Never lock `jobs` while holding a record lock. `path_claim` serialises choosing an output path, so two downloads can't take the same name (media names are uniqued the same way).
- **Plain files (`job.rs`):**
  - The probe is `GET` with `Range: bytes=0-0`. A 206 with a total in Content-Range means resumable, multi-connection. A 200 means the response is streamed as a single connection.
  - Probe fallbacks: 503/429 are retried; a 400/405/416/501, or a 206 with no total, repeats the probe without `Range`.
  - Segment requests send `If-Range` with a strong ETag only (never Last-Modified, which some servers change per request). A changed file answers 200, and the download restarts from zero. On resume, `same_validators` compares ETags, and Last-Modified only when neither side has an ETag.
  - Cookie and auth headers are dropped when a redirect leaves the host (`util::headers_for_redirect`).
  - Dynamic segmentation as in IDM. The pure scheduling logic is in `sched.rs`, with unit tests:
    - An idle worker first takes an orphaned segment.
    - Next, it takes over the whole remainder of a stalled owner (no data for 10s).
    - Otherwise it splits the segment with the longest ETA (remaining ÷ EWMA speed). The cut is weighted so both halves finish together.
    - A finished worker requests its next range over the same keep-alive connection. Leftover bodies are drained so the connection goes back to the pool.
  - Failed downloads retry on their own when `Settings.auto_retry` (default on): up to 5 times, pauses growing from 30s to 5 min (`auto_retry_delay`), plain files only (yt-dlp has its own), transient errors only (`is_transient`); the count resets when bytes arrived.
  - `connections` is the user's maximum. A download starts with up to 4 workers and the `Ramp` adds more while throughput keeps rising. A 429, 503 or connect error halves the cap.
  - `Segment.speed` and `last_data` are `#[serde(skip)]`. The app's expanded row shows them per connection.
  - Data is written in place into a preallocated `name.part`, then renamed.
  - The reqwest client is `http1_only` on purpose. HTTP/2 would multiplex every segment over one TCP connection and defeat per-connection server caps.
- **Media (`media.rs`):**
  - `Kind::Media` downloads run `yt-dlp` as a child process in its own process group.
  - A download is Media when:
    - `AddRequest.media` is set,
    - the URL ends in `.m3u8`/`.mpd`,
    - or the probe returns `text/html` (not an attachment) or a stream content-type. This is how pasting a YouTube link just works.
  - Progress comes from custom `--progress-template`/`--print` lines (`HURTLE`, `HURTLEINFO`, `HURTLEPP`, `HURTLEFILE`).
  - Partial files live in `$HURTLE_DATA/media/<id>/`.
  - Pause/remove/shutdown call `Job::kill_child()` **synchronously**, because the app can exit before async cleanup runs. That is `killpg` on Unix and `taskkill /T /F` on Windows.
  - Every yt-dlp process goes through `prepare()`:
    - It gets its own process group, so killing it also stops its ffmpeg.
    - On Windows it gets `CREATE_NO_WINDOW`.
    - Its `PATH` gets the tool dirs added, so it finds ffmpeg and deno even when the browser started us.
  - Format choice:
    - Format sort is `lang,res,…`, so the original audio wins over YouTube auto-dubs.
    - The dialog's audio language goes through `with_lang`, and its container through `container_args`.
    - An audio type (mp3/m4a/…) turns the download into audio-only.
    - Without ffmpeg, `without_merge` drops the `bv+ba` pairs.
  - Progress keeps one `Stream` per yt-dlp format_id. They become `rec.segments`, one block per stream, and `rec.note` (e.g. "Video (stream 1) · fragment 3/40, 8 at a time"). The total only grows while downloading.
  - Headers (Cookie, Referer) reach yt-dlp through a 0600 `--config-locations` file, never the command line.
  - yt-dlp can't share the token buckets. It gets `--limit-rate` = the smaller of the per-file and global limits, and the job restarts (`Job::restart`) when either changes.
  - For extractor sites, header forwarding is limited to Cookie and Referer: overriding the User-Agent breaks yt-dlp's extraction.
  - yt-dlp is found at `HURTLE_YTDLP`, else in `tools::search_dirs()`: next to our executable, on `PATH`, Homebrew, `/usr/bin`, `~/.local/bin`, WinGet `Links` and Scoop `shims`, and last `$HURTLE_DATA/tools`. The browser launches the host without the user's shell PATH, so mise shims are invisible to it.
- **Tool fetching (`tools.rs`):**
  - A missing yt-dlp, ffmpeg (+ffprobe) or deno is fetched from its official release into `tools::dir()` (the global `data_dir()/tools`, shared by app, CLI and host) and checked against the published SHA-256 sums. The per-OS/arch asset table is `sources_for`.
  - `probe()` fetches yt-dlp (and deno for YouTube) synchronously. `drive()` also fetches ffmpeg, through `spawn_blocking`, with progress in `rec.note`. Only a missing yt-dlp is an error.
  - `job.rs` treats pages as media when yt-dlp is installed *or fetchable* (`media::ytdlp_available`).
  - A fetched yt-dlp runs `-U` at most daily (`update_ytdlp_in_background`); installed copies are never touched.
  - `HURTLE_TOOLS_ONLY=1` ignores installed tools, to test fetching: `HURTLE_TOOLS_ONLY=1 cargo run -p hurtle-core --example tools` (into `.dev-data/tools` under mise; delete them there to refetch).
- **Expired links (relink):** `Download.page_url` is where a download was found (`AddRequest.pageUrl`, else the referrer). The app shows it with an "Open page" button.
  - `Engine::add_or_relink` checks `same_download`. When a paused, failed or queued entry matches, it gets the new URL and fresh headers, its ETag is cleared, and it is queued again instead of being added twice. The IPC reply then says `resumed: true`.
  - A file matches on the same link ignoring the query string, or on the same `size` (at least 1 MiB) plus the same name or page. Media matches on the same options plus the same manifest path, or the same page and name.
  - `job.rs`'s `same_file` probe still decides whether the segments are kept. Check it with `cargo run -p hurtle-core --example relink -- http://127.0.0.1:8090 <size> <dir>`.
- **Torrents (`torrent.rs`, librqbit 9.1 with `rust-tls`, no default features):**
  - `Kind::Torrent`: a magnet link, a `.torrent` URL or `application/x-bittorrent` probe, a local `.torrent` path (app/CLI only, never IPC), or `AddRequest.torrentData` (base64 of the file, sent by the extension so logged-in trackers work). `Download.torrent: Option<TorrentState>` holds the file list, the choice (`wanted`), uploaded bytes and the per-download seed ratio.
  - One librqbit `Session` per engine, started lazily on the engine runtime (`torrents.session`), with `Settings.torrent_port` (default 42069, falls back to a free port; `HURTLE_TORRENT_PORT` overrides), DHT (state in `$HURTLE_DATA/torrent/dht.json`) and UPnP. Port/DHT/UPnP changes apply after a restart.
  - librqbit ignores magnet `x.pe=` peers; `magnet_peers` passes them as `initial_peers`. Resolved metadata is cached as `$HURTLE_DATA/torrent/<hash>.torrent`, so resume never needs peers for metadata.
  - `drive()` adds with `only_files` and `overwrite: true` (existing pieces are re-checked), polls stats every 500 ms into `downloaded`/`total` (the chosen files' pieces, so a little more than the files), peers and the note. Finishing seeds (`spawn_seeding`) until the ratio, else the torrent is removed from the session. Per-torrent limits are fixed at add time, so a limit change restarts the job like yt-dlp. No auto-retry.
  - A single-file torrent goes into the folder chosen by its type; multi-file ones get `<dir>/<name>` (uniqued). `IpcRequest::TorrentInfo` lists files (truncated to 4000 for the 1 MB native-messaging cap); the host relays it to the app.
  - `cargo-deny` ignores RUSTSEC-2026-0293 (ringbuf via librqbit-utp; only `u8` elements). Re-check when librqbit updates.
- **Streaming (`stream.rs`, `player.rs`):**
  - A hand-rolled HTTP/1.1 server (Range/206/416, HEAD) bound lazily on 127.0.0.1, or 0.0.0.0 when `Settings.stream_lan`. Every path starts with a random 32-hex token: `/<token>/<id>/<file>/<name>` and `/<token>/<id>.m3u`.
  - A finished file is served from disk; an unfinished torrent file through librqbit's `FileStream`, which prioritises the pieces being read (that's how seeking works mid-download). Plain downloads stream once completed.
  - `IpcRequest::Stream { id, file, play }` selects the file, resumes a paused torrent and returns the URL; `play` waits for a live handle and opens it with `player::open` (http URLs only). `player::detect` finds VLC first, then mpv & co. on `tools::search_dirs()`, Flatpak, macOS `/Applications` and Program Files; `Settings.player` is an id from that list (IPC refuses anything else) or a program path set in `settings.json`.
- **Magnet/.torrent handler (`assoc.rs`):** `ensure_registered` (app start, release builds only, never changes a default) makes the OS offer Hurtle: Linux writes or patches `hurtle.desktop` (`MimeType=x-scheme-handler/magnet;application/x-bittorrent;`, a user copy shadows a package entry without it); Windows adds `Hurtle.Magnet`/`Hurtle.Torrent` ProgIDs, `.torrent\OpenWithProgids`, `Software\Hurtle\Capabilities` + `RegisteredApplications`, and claims `magnet` only when nothing handles it; macOS relies on `Info.plist` (`scripts/package.sh`) and the app's `on_open_urls`, since macOS sends links and files as Apple Events, not arguments. `make_default` uses `xdg-mime` (else edits `mimeapps.list`), Launch Services on macOS, and opens `ms-settings:defaultapps?registeredAppUser=Hurtle` on Windows. `status()` shells out, so the view caches it per panel opening.
- **Speed limits:** `rate::RateLimiter` token buckets. There is one per job, shared by its connections, plus one global. Both change at runtime.
- **Download folders:** `Settings.download_dir` is the main folder, and `type_dirs` maps a `util::CATEGORIES` id (video, audio, image, document, archive, program) to its own folder.
  - `Settings::dir_for(util::category(name))` picks the folder. Media uses `media::category(format, container)`, so audio-only goes to the audio folder.
  - A download without `AddRequest.dir` is `auto_dir`. Its folder is chosen again in `job.rs` once the server names the file, and in `media::drive` before yt-dlp starts.
  - IPC `settings` takes a `SettingsChange` (`downloadDir`, `typeDirs`, where an empty path means the main folder). Paths must be absolute; `~` is expanded.
  - With the app closed, hurtle-host reads and writes `settings.json` itself.
  - IPC `pickFolder` queues a `FolderPick`. The app's main loop shows GPUI's system picker for it, which is how the extension's "Change…" buttons work.
  - `extension/src/folders.js` mirrors the category table for the popup and the dialog's "Save to" row. Keep the two in sync.
- **Persistence:**
  - `$HURTLE_DATA` (default `~/.local/share/hurtle`) holds `downloads.json` and `settings.json`. They are saved every 2s and on `shutdown()`.
  - Active jobs come back Paused.
  - The data dir is 0700 and its files 0600 (`util::create_private_dir`, `write_private`).
  - An unreadable store is kept as `*.json.bak`; bad records in `downloads.json` are skipped individually and backed up, so one corrupt entry doesn't drop the list.
  - `Settings.notify` (default on) and `auto_retry` are settable over IPC (`notify`, `autoRetry`), in the app's folders panel and in the popup.
  - New `Download` fields need `#[serde(default)]`, or old stores stop loading.
- **IPC (`ipc.rs`):**
  - `IpcRequest` is tagged by `type`, camelCase: `add | probe | ping | show | settings | torrentInfo | stream | players | …`.
  - `AddRequest` is also the extension's wire format.
  - The socket is `$HURTLE_SOCKET`, else `$XDG_RUNTIME_DIR/hurtle.sock`.
  - On Windows it is a named pipe instead: `ipc::pipe_name()` gives `\\.\pipe\hurtle-<user>`, and `HURTLE_SOCKET` names the pipe.
    - The server creates the next pipe instance before handing off the connected one.
    - The client opens the pipe as a file and waits for the reply on a thread, since pipe handles have no read timeout.
  - Request handling (`handle_request`) is shared; only the listener is per-OS.
  - A request line is capped at 4 MiB.
  - IPC `downloadDir`/`typeDirs` go through `check_ipc_dir`, which refuses `..` and places where a dropped file runs later (`~/.ssh`, autostart, systemd, LaunchAgents…). The app's picker and the CLI are unrestricted.
- **Native-host registration (`nmh.rs`):**
  - It is used by `hurtle-host --register/--unregister`, by `install.sh`, and on app start (`ensure_registered`).
  - App start only registers when no working registration exists, so a dev build never takes over an installed host.
  - Linux and macOS: it writes manifests only into installed browsers' dirs, plus Firefox's dir always.
  - `registered_host` also accepts the .deb's system manifests on Linux (`/etc/opt/chrome`, `/etc/chromium`, `/etc/opt/edge`, `/usr/lib/mozilla`), so a package install is not shadowed by per-user ones.
  - Windows: it writes manifests to the data dir and adds `HKCU\Software\<browser>\NativeMessagingHosts` keys via `reg`.

### hurtle-host
- It uses native-messaging framing (4-byte native-endian length + JSON). stdout is the protocol channel, so never print anything else there.
- It relays requests to the app's socket and launches the app if it isn't running (`HURTLE_APP`, else the sibling binary, else PATH). Exceptions:
  - `ping` and settings reads never launch the app.
  - `probe` runs yt-dlp in-process, so filling the dialog doesn't start the app.
- `log` replies carry the tail of `hurtle.log` plus its absolute `path`; the extension's log page shows it.
- Native-messaging replies are capped at 1 MB, so `MediaInfo` is a trimmed summary, never raw `yt-dlp -J` output.

### hurtle-app (binary `hurtle`)
- GPUI comes from `gpui-kit` 0.9, which re-exports `gpui-pre`. Don't add a separate `gpui` dependency, or you'll get two GPUI stacks.
- Required setup:
  - Call `.with_assets(gpui_kit::assets::Assets)`, or icons don't render.
  - View helpers return `AnyElement`. Rust 2024 `impl IntoElement` captures the `cx` lifetime.
- It is single-instance: a second launch forwards its URLs plus `show` over the socket, then exits.
- `#![windows_subsystem = "windows"]` applies to release builds of the app only. The host must stay a console program, because stdio is its protocol.
- **Tray (`tray.rs`, ksni StatusNotifierItem, Linux only):**
  - Other OSes compile `tray_stub.rs` instead, whose `start()` returns false. Closing the window then quits.
  - The tray runs on the engine's runtime and talks to GPUI only through the `Flags { show, quit }` atomics. An app-level loop in `main.rs` polls them, then raises or reopens the window, or quits.
  - When the tray registers, the quit mode is `QuitMode::Explicit`: closing the window keeps Hurtle running.
- **Theme (`theme.rs`):**
  - `Settings.theme` is one of "system" (the default), light, dark, or a preset name.
  - "system" reads, in order: Omarchy, Noctalia `colors.json`, pywal/wallust/hellwal, Stylix, GTK css (`@import`s are followed, which covers matugen and Noctalia's templates), and KDE `kdeglobals`. Otherwise it uses the OS light/dark preference.
  - Missing roles are derived and pushed to a minimum contrast.
  - `theme::poll` re-hashes the source files about once a second, so a generator's rewrite or a symlink swap re-themes the app live.
  - `HURTLE_THEME=<name|path>` overrides the setting, for screenshots.
- **Torrent UI (`view.rs`):** paste a magnet or use "Open .torrent"; rows get Play (stream) and a Seed toggle; the details list files with checkboxes and per-file progress (first 300). The folders panel has a "BitTorrent and streaming" section. Extra Lucide icons come from `icon_assets!(ExtraIcons, …)` behind `AppAssets`. `hurtle magnet:…` / `hurtle file.torrent` arguments are forwarded like URLs; the Linux desktop entry declares `x-scheme-handler/magnet` and `application/x-bittorrent`.
- **Notifications (`notify.rs`):** a watcher on the engine's runtime announces completed/failed downloads (not ones already in the list at start) while `Settings.notify` is on. Rows also offer remove-with-file and retry.
- **Shutdown:** `on_app_quit` and a SIGTERM/SIGINT/SIGHUP handler both call `engine.shutdown()`. It saves state, kills yt-dlp and removes the socket.

### extension/
- `src/` is shared. `build.sh` writes two manifests:
  - **Chrome:** uses `background.service_worker`. A fixed `key` pins the ID `llabdeoinnakkcncicakgafkjkkjfdgh`; the ID is also in `chrome-id.txt`.
  - **Firefox:** uses `background.scripts`, with gecko id `hurtle@arafays.github.io` and `strict_min_version` 142.
- `background.js` injects content scripts only into an allowlist of pages (no browser-internal or store pages).
- `folders.js` also holds `LIMITS`, `CONNECTIONS` and `INSTALL_URL` (the popup and dialog use them; a test checks it against the core table).
- The dialog has a host-missing mode (install link) and the popup shows an install notice when the native host doesn't answer.
- `background.js` keeps all state in `storage.session`, because Chrome unloads the service worker.
- **Download interception:** `downloads.onCreated` pauses the browser's own download, then opens `dialog.html` as a popup window. Closing that window means cancel. `passThrough` stops Hurtle from re-catching downloads it handed back to the browser.
- **Media sniffing:** `webRequest.onHeadersReceived` sniffs media per tab.
  - The `SEGMENT` regex drops stream pieces (`.ts`, `.m4s`, `videoplayback`).
  - `PAGE_ONLY` drops YouTube/googlevideo manifests. Downloaded raw, they pick a random dub and get named after the manifest.
  - For a sniffed stream, `dialogProbe` first probes the page URL. If yt-dlp has a real extractor for it (anything but `Generic`/`HTML5MediaEmbed`, which just mean "a page with a `<video>`"), the download switches to the page.
  - Each sniffed HLS/DASH manifest is fetched once and parsed (`manifestInfo`) for its qualities, length and live flag.
- **Video lists:** `mediaui.js` is shared by the overlay (loaded as a content script before `content.js`) and the popup. Rows show a thumbnail, resolution, length and size:
  - The thumbnail is the playing video's frame, else a poster/og:image, else a metadata-only `<video>` preview.
  - Quality chips pass `quality` through to the dialog, which preselects the closest choice at or below it.
  - The popup gets the page's title and `<video>` details by sending `page-info` to the top frame.
- **Sizes:** the first time the button shows on a page, `page-sizes` asks yt-dlp (via the host's `probe`) which sizes the page offers. The button then reads "Download video [N sizes] ▾", and the menu's page row gets one chip per size ("1080p · 45 MB"). `probeMedia` caches answers per URL for 10 minutes, so the dialog opens with the list already there. `MediaChoice` carries `width`/`height`, shown in the dialog as "720p · 1280×720 · ≈ 1.4 MB".
- **Preferred resolution:** `storage.local.preferredQuality` (popup Settings → Video, default 1080, 0 = best). `HurtleUI.pickHeight` picks the highest size at or below it, else the lowest above; that chip is highlighted (`.chip.pick`) in the overlay menu and popup, and `dialog.js` preselects the same choice when no chip was clicked.
- `build.sh` builds into `dist/.build` and swaps `dist/chrome`/`dist/firefox` in with `mv --exchange`. Brave/Chromium load `dist/chrome` unpacked and Zen may have `dist/firefox` as a temporary add-on, so the folders must never be missing, even mid-build.
- Local builds get a 4th manifest version part (`build.sh`), so reinstalling a rebuilt `.xpi` is an update. Firefox/Zen kept serving the old files after a same-version reinstall.
- **Torrents:** `content.js` catches left-clicks on `magnet:` links while catching is on (`download-magnet`); `downloads.onCreated` catches `.torrent`/`application/x-bittorrent` downloads and re-fetches the file with cookies (`fetchTorrent`, ≤ 4 MiB, must start with `d`); the context menu handles both. The dialog's torrent mode (`loadTorrent`) gets the file list over `dialog-torrent` → IPC `torrentInfo` and sends `files` (null = all), `seedRatio` and `stream` ("Play while downloading", Start only).
- **Overlay (`content.js`):** a closed shadow-root button that sits above the hovered video.
  - With no direct file, it opens the dialog in video mode (`download-page`).
  - `dialog.js` then fetches the quality list over `dialog-probe`.
  - It replaces the filename with yt-dlp's title, except for the `Generic` extractor, where the page title is kept.

## Desktop constraints
- Never drive the user's desktop with synthetic input (wtype/ydotool). To see the app, launch it with a private `HURTLE_DATA`/`HURTLE_SOCKET` and screenshot it with `grim`.
- `HURTLE_VIEW=folders` opens the app on its folders panel. Launch test/dev app instances with `HURTLE_APP_ID=hurtle-test` (the e2e tests and `mise run dev` set it, as `hurtle-test`/`hurtle-dev`). A niri rule opens those windows unfocused on the small side monitor (HDMI-A-1), so they don't interrupt the user on DP-3. Screenshot that output with `grim -o HDMI-A-1`.
- Window management is niri. Its rules live in `~/.config/niri/cfg/rules.kdl`.
  - Chromium/Brave give the Hurtle dialog the app-id `chrome-…`/`brave-llabdeoinnakkcncicakgafkjkkjfdgh-Default`, and they set the title only after the window maps. Match these windows by app-id, not title.
- When using `pkill -f`, use a pattern that can't match your own shell command line.
