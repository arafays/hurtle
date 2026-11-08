# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack

Landing page: plain static HTML/CSS in `docs/`, served by GitHub Pages from `main` `/docs`. No build step.

## Users

Power users on Windows, macOS and Linux who want IDM-style download acceleration: people who miss IDM, download large files from servers that cap each connection, or save videos from the browser and want yt-dlp without the command line.

## Product Purpose

Surge downloads files faster by splitting each one into byte-range segments over parallel HTTP/1.1 connections, with IDM-style dynamic segmentation. A browser extension catches downloads and videos and hands them to the desktop app. Success: a visitor installs the app and the extension and their next big download or video goes through Surge.

## Positioning

Free, open-source and cross-platform, with IDM's dynamic segmentation (idle connections split the slowest segment, so no straggler finishes the download), plus yt-dlp built in for video sites and HLS/DASH streams, chosen from a dialog in the browser.

## Capabilities and Constraints

- Segmented downloads, per-file and global speed limits, pause/resume across restarts, per-type download folders.
- Browser extension for Chrome-family browsers and Firefox: download interception, media sniffing, video overlay button, quality/audio-language/container choice.
- Desktop app with themes (system theme detection on Linux, plus presets) and a Linux tray icon.
- macOS builds are ad-hoc signed and not notarized. The Firefox add-on is unsigned unless AMO keys are set. No tray on Windows/macOS yet.

## Evidence on Hand

- Benchmark from the README: 24 MB file, server capped at 512 KB/s per connection: 1 connection 48.2 s (510 KB/s), 8 connections 6.6 s (3.7 MB/s).
- Real screenshots of the app and extension (captured from test instances with synthetic local downloads).
- No testimonials, user counts or press exist; never invent them.

## Product Principles

- Prove speed with measurement, not adjectives.
- The browser flow should feel native: one dialog, sensible defaults, no command line.
- Honest about platform gaps (notarization, tray, signing).
