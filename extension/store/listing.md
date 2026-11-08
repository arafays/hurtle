# addons.mozilla.org listing

Paste these into the Developer Hub (Edit Product Page). Screenshots and the
icon are the PNGs next to this file (1280×800 and 128×128).

## Name
Surge Download Manager

## Summary (250 characters max)
Send downloads to Surge: multi-connection, resumable downloads and video sites.

## Description
Surge is a free, open-source download accelerator for Windows, macOS and Linux. This add-on hands your downloads to the Surge desktop app, which splits each file across parallel connections so it finishes faster, and resumes after a pause, a quit or a crash.

What the add-on does:
- Catches browser downloads and opens one dialog: rename the file, choose the folder, connections and speed limit, then start, save for later, or hand it back to Firefox.
- Puts a "Download video" button above videos and finds HLS/DASH streams on the page. Pick the quality, audio language and format (MP4, MKV or audio only), and Surge saves it through yt-dlp.
- Keeps the original audio track on YouTube instead of an auto-dub.
- Right-click any link or video and choose "Download with Surge".
- Forwards the page's cookies and Referer to Surge, so downloads behind a login still work.

The add-on needs the Surge app installed, which it talks to through native messaging: https://arafays.github.io/surge/
For video sites the app uses yt-dlp, ffmpeg and deno, and fetches them itself the first time if they are not installed.

Nothing is sent anywhere except to the Surge app on your own computer.

## Homepage
https://arafays.github.io/surge/

## Support site
https://github.com/arafays/surge/issues

## Categories
Download Management

## Tags
download manager, video downloader, download accelerator

## License
MIT

## Privacy policy
Surge Download Manager collects no data. It passes download URLs, the page's
cookies and Referer for those URLs, and page titles only to the Surge desktop
app on the same computer, through native messaging. Nothing is sent to the
developer or any third party.

## Notes to reviewer
The add-on talks to a native-messaging host (surge-host) that ships with the
Surge desktop app: https://github.com/arafays/surge/releases. Without the app
the popup shows "Surge not found". The source is not minified or bundled; the
files in the package are the files in extension/src of the repository.

## Screenshot captions
1-video-button.png  A "Download video" button above any video lists the page, its files and stream qualities.
2-dialog.png        One dialog: name, folder, quality, audio language, format, connections and speed limit.
3-popup.png         The popup lists media found on the page and your download folders.
4-app.png           The Surge app: plain files and streams in one list, each split across connections.
5-connections.png   An expanded download shows every connection's byte range and speed.
