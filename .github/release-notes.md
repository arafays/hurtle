**Torrents and streaming.** Hurtle now downloads magnet links and `.torrent` files with its own BitTorrent client: pick which files you want, seed to a ratio, and play videos in VLC (or mpv and others) while they download, with seeking. The extension catches magnet clicks and `.torrent` downloads and shows the torrent's file list before you start. Hurtle registers itself as an app for magnet links and `.torrent` files, and Settings → *Make default* makes it the default. The app now shows its version (footer and Settings → About), and `hurtle --version` prints it.

## Install

| System | Download | Then |
| --- | --- | --- |
| Windows (x64) | `hurtle-*-windows-x86_64-setup.exe` | Run it (per-user, no admin rights). `hurtle-*-windows-x86_64.zip` is the portable build |
| macOS (Apple Silicon) | `hurtle-*-macos-aarch64.dmg` | Drag `Hurtle.app` to Applications, open it (the `.zip` works too) |
| macOS (Intel) | `hurtle-*-macos-x86_64.dmg` | Same as above |
| Debian / Ubuntu (x64, arm64) | `hurtle_*_amd64.deb` / `hurtle_*_arm64.deb` | `sudo apt install ./hurtle_*.deb` |
| Linux (x64) | `hurtle-*-linux-x86_64.tar.gz` | Extract, run `./install.sh` (or just `./hurtle`) |
| Linux (arm64) | `hurtle-*-linux-aarch64.tar.gz` | Same as above |

Or install from a terminal: `curl -fsSL https://arafays.github.io/hurtle/install.sh | sh` (Linux, macOS) or `irm https://arafays.github.io/hurtle/install.ps1 | iex` (Windows). Every download is checked against `SHA256SUMS.txt`.

Running Hurtle once registers it with Chrome, Chromium, Brave, Edge, Vivaldi and Firefox, so the extension can reach it. If you move the files, run `hurtle-host --register` again (and `hurtle-host --unregister` to remove it).

**Browser extension**
- Chrome / Brave / Edge: unzip `hurtle-chrome-*.zip`, open `chrome://extensions`, turn on *Developer mode*, click *Load unpacked*, and pick the unzipped folder.
- Firefox, Zen, LibreWolf: open `hurtle-firefox-<version>.xpi` (signed by Mozilla) and confirm. Once installed, it updates itself about once a day; `about:addons` → ⚙ → *Check for Updates* checks now. If you loaded it as a temporary add-on before, remove that first.

**Video sites (YouTube etc.) and HLS/DASH streams** use yt-dlp, ffmpeg and deno. Installed copies are used if you have them; otherwise Hurtle fetches them from their official releases the first time you save a video, checks their SHA-256 sums, and keeps yt-dlp up to date.

**macOS:** the app is not notarized. If macOS says it is damaged or can't be opened, run `xattr -dr com.apple.quarantine /Applications/Hurtle.app`.

**Windows:** builds are unsigned, so SmartScreen may say "Windows protected your PC". Choose More info, then Run anyway.

**Windows and macOS:** there is no tray icon yet, so closing the window quits Hurtle. It starts again on the next browser download.
