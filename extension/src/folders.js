// Download folders, shared by the popup and the download dialog. Surge keeps
// one main folder plus optional folders per file type (settings.json:
// download_dir, type_dirs); this mirrors surge-core's util::category so the
// dialog can say where a file will go before it is sent.
globalThis.SurgeFolders ??= (() => {
  const CATEGORIES = [
    ["video", "Videos"],
    ["audio", "Music"],
    ["image", "Pictures"],
    ["document", "Documents"],
    ["archive", "Archives"],
    ["program", "Programs"],
  ];
  const EXT = {
    video: "mp4 m4v mkv webm mov avi wmv flv mpg mpeg 3gp ts m2ts ogv",
    audio: "mp3 m4a aac flac wav ogg oga opus wma alac aiff",
    image: "jpg jpeg png gif webp avif heic heif bmp tif tiff svg ico raw cr2 nef dng",
    document: "pdf doc docx odt rtf txt md epub mobi xls xlsx ods csv ppt pptx odp",
    archive: "zip rar 7z tar gz tgz bz2 xz zst iso",
    program: "exe msi dmg pkg deb rpm appimage apk flatpak snap sh run bin",
  };
  const BY_EXT = new Map(Object.entries(EXT).flatMap(([cat, list]) => list.split(" ").map((e) => [e, cat])));
  const AUDIO_TYPES = new Set(["mp3", "m4a", "opus", "flac", "wav"]);

  /** "video", "image"… from a file name's extension, or null. */
  function category(name) {
    const ext = /\.([a-z0-9]+)$/i.exec(name || "")?.[1]?.toLowerCase();
    return (ext && BY_EXT.get(ext)) || null;
  }

  /** A yt-dlp download: audio for an audio type or audio-only format, else video. */
  function mediaCategory(format, container) {
    const f = format || "bv*+ba/b";
    return AUDIO_TYPES.has(container) || (!f.includes("bv") && f.startsWith("ba")) ? "audio" : "video";
  }

  function dirFor(settings, cat) {
    return (cat && settings?.type_dirs?.[cat]) || settings?.download_dir || "";
  }

  /** "~/Videos" for "/home/me/Videos". */
  function short(path) {
    return (path || "").replace(/^\/home\/[^/]+(?=\/|$)/, "~").replace(/^\/Users\/[^/]+(?=\/|$)/, "~");
  }

  return { CATEGORIES, category, mediaCategory, dirFor, short };
})();
