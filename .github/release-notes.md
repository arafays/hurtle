**Installers, notifications and auto-retry.** Hurtle now ships a per-user `setup.exe` for Windows, `.dmg`s for macOS, `.deb`s for Debian/Ubuntu, and one-line install scripts. Finished and failed downloads raise a desktop notification, and failed downloads retry on their own (both can be turned off in the app or the extension popup). Downloads also survive more server quirks, and the list keeps working when one saved entry is corrupt.

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
