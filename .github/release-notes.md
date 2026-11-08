## Install

| System | Download | Then |
| --- | --- | --- |
| Windows (x64) | `surge-*-windows-x86_64.zip` | Unzip anywhere, run `surge.exe` |
| macOS (Apple Silicon) | `surge-*-macos-aarch64.zip` | Unzip, move `Surge.app` to Applications, open it |
| macOS (Intel) | `surge-*-macos-x86_64.zip` | Same as above |
| Linux (x64) | `surge-*-linux-x86_64.tar.gz` | Extract, run `./surge` |
| Linux (arm64) | `surge-*-linux-aarch64.tar.gz` | Extract, run `./surge` |

Running Surge once registers it with Chrome, Chromium, Brave, Edge, Vivaldi and Firefox, so the extension can reach it. If you move the files, run `surge-host --register` again (and `surge-host --unregister` to remove it).

**Browser extension**
- Chrome / Brave / Edge: unzip `surge-chrome-*.zip`, open `chrome://extensions`, turn on *Developer mode*, click *Load unpacked*, and pick the unzipped folder.
- Firefox: open `surge-firefox-*.xpi` if this release has one (signed). Otherwise use `about:debugging` → *Load Temporary Add-on* → `manifest.json` from `surge-firefox-*.zip`. A temporary add-on is removed when Firefox restarts.
- Zen / LibreWolf: set `xpinstall.signatures.required` to `false` in `about:config`, then `about:addons` → ⚙ → *Install Add-on From File…* → `surge-firefox-*-unsigned.xpi`.

**Video sites (YouTube etc.) and HLS/DASH streams** use yt-dlp, ffmpeg and deno. Installed copies are used if you have them; otherwise Surge fetches them from their official releases the first time you save a video, checks their SHA-256 sums, and keeps yt-dlp up to date.

**macOS:** the app is not notarized. If macOS says it is damaged or can't be opened, run `xattr -dr com.apple.quarantine /Applications/Surge.app`.

**Windows and macOS:** there is no tray icon yet, so closing the window quits Surge. It starts again on the next browser download.
