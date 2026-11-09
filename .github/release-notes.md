**Surge is now Hurtle.** The app, binaries, data folder and browser add-on all have the new name. Your downloads list and settings move over on first start. Firefox users: remove the old Surge add-on and install the Hurtle one from this release (it has a new add-on ID). Chrome-family users: reload the unpacked extension from the new zip.

## Install

| System | Download | Then |
| --- | --- | --- |
| Windows (x64) | `hurtle-*-windows-x86_64.zip` | Unzip anywhere, run `hurtle.exe` |
| macOS (Apple Silicon) | `hurtle-*-macos-aarch64.zip` | Unzip, move `Hurtle.app` to Applications, open it |
| macOS (Intel) | `hurtle-*-macos-x86_64.zip` | Same as above |
| Linux (x64) | `hurtle-*-linux-x86_64.tar.gz` | Extract, run `./hurtle` |
| Linux (arm64) | `hurtle-*-linux-aarch64.tar.gz` | Extract, run `./hurtle` |

Running Hurtle once registers it with Chrome, Chromium, Brave, Edge, Vivaldi and Firefox, so the extension can reach it. If you move the files, run `hurtle-host --register` again (and `hurtle-host --unregister` to remove it).

**Browser extension**
- Chrome / Brave / Edge: unzip `hurtle-chrome-*.zip`, open `chrome://extensions`, turn on *Developer mode*, click *Load unpacked*, and pick the unzipped folder.
- Firefox, Zen, LibreWolf: open `hurtle-firefox-<version>.xpi` (signed by Mozilla) and confirm. Once installed, it updates itself about once a day; `about:addons` → ⚙ → *Check for Updates* checks now. If you loaded it as a temporary add-on before, remove that first.

**Video sites (YouTube etc.) and HLS/DASH streams** use yt-dlp, ffmpeg and deno. Installed copies are used if you have them; otherwise Hurtle fetches them from their official releases the first time you save a video, checks their SHA-256 sums, and keeps yt-dlp up to date.

**macOS:** the app is not notarized. If macOS says it is damaged or can't be opened, run `xattr -dr com.apple.quarantine /Applications/Hurtle.app`.

**Windows and macOS:** there is no tray icon yet, so closing the window quits Hurtle. It starts again on the next browser download.
