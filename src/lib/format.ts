export function formatSize(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined) return "—";
  if (bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / Math.pow(1024, i);
  const digits = value >= 100 || i === 0 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(digits)} ${units[i]}`;
}

export function formatSpeed(bytesPerSecond: number): string {
  if (!bytesPerSecond || bytesPerSecond < 1) return "—";
  return `${formatSize(bytesPerSecond)}/s`;
}

/** Quark timestamps are either seconds or milliseconds. */
export function toDate(value: number): Date | null {
  if (!value) return null;
  const ms = value < 1e12 ? value * 1000 : value;
  const date = new Date(ms);
  return Number.isNaN(date.getTime()) ? null : date;
}

export function formatDate(value: number): string {
  const date = toDate(value);
  if (!date) return "—";
  const now = new Date();
  const sameDay =
    date.getFullYear() === now.getFullYear() &&
    date.getMonth() === now.getMonth() &&
    date.getDate() === now.getDate();
  const pad = (n: number) => String(n).padStart(2, "0");
  if (sameDay) return `今天 ${pad(date.getHours())}:${pad(date.getMinutes())}`;
  if (date.getFullYear() === now.getFullYear()) {
    return `${date.getMonth() + 1}月${date.getDate()}日 ${pad(date.getHours())}:${pad(
      date.getMinutes(),
    )}`;
  }
  return `${date.getFullYear()}/${pad(date.getMonth() + 1)}/${pad(date.getDate())}`;
}

export function formatFullDate(value: number): string {
  const date = toDate(value);
  if (!date) return "—";
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}`;
}

export function formatEta(remaining: number, speed: number): string {
  if (!speed || speed <= 0 || remaining <= 0) return "";
  const seconds = Math.round(remaining / speed);
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
}

export type FileKind =
  | "folder"
  | "image"
  | "video"
  | "audio"
  | "pdf"
  | "archive"
  | "code"
  | "doc"
  | "sheet"
  | "slide"
  | "text"
  | "file";

const KIND_MAP: Record<string, FileKind> = {
  jpg: "image", jpeg: "image", png: "image", gif: "image", webp: "image",
  bmp: "image", svg: "image", heic: "image", avif: "image", tiff: "image",
  mp4: "video", mkv: "video", avi: "video", mov: "video", webm: "video",
  flv: "video", wmv: "video", m4v: "video", m2ts: "video",
  mp3: "audio", flac: "audio", wav: "audio", aac: "audio", ogg: "audio",
  m4a: "audio", wma: "audio", opus: "audio",
  pdf: "pdf",
  zip: "archive", rar: "archive", "7z": "archive", tar: "archive",
  apk: "archive", deb: "archive", rpm: "archive", dmg: "archive",
  gz: "archive", bz2: "archive", xz: "archive", zst: "archive", iso: "archive",
  js: "code", ts: "code", jsx: "code", tsx: "code", vue: "code", rs: "code",
  py: "code", go: "code", c: "code", h: "code", cpp: "code", java: "code",
  json: "code", toml: "code", yaml: "code", yml: "code", sh: "code",
  sql: "code", css: "code", html: "code", xml: "code",
  doc: "doc", docx: "doc", rtf: "doc", odt: "doc",
  xls: "sheet", xlsx: "sheet", csv: "sheet", ods: "sheet",
  ppt: "slide", pptx: "slide", odp: "slide", key: "slide",
  psd: "image", ai: "image", sketch: "image", xd: "image", fig: "image",
  nef: "image", cr2: "image", cr3: "image", arw: "image", dng: "image",
  raw: "image", eps: "image", indd: "image",
  txt: "text", md: "text", log: "text",
};

export function fileKind(entry: { isDir: boolean; ext: string }): FileKind {
  if (entry.isDir) return "folder";
  return KIND_MAP[entry.ext] ?? "file";
}

export function kindLabel(kind: FileKind): string {
  const map: Record<FileKind, string> = {
    folder: "文件夹", image: "图片", video: "视频", audio: "音频",
    pdf: "PDF", archive: "压缩包", code: "代码", doc: "文档",
    sheet: "表格", slide: "演示", text: "文本", file: "文件",
  };
  return map[kind];
}

/**
 * Turns anything a rejected IPC call can hand back into one readable line.
 *
 * Tauri rejects with the serialised `AppError`, i.e. a plain string, but a
 * transport failure arrives as `Error("...")` or even as a bare object, and a
 * raw `reqwest` error would surface as English ("error sending request for
 * url (...)") which reads as nonsense in the UI. Everything that is not a
 * clean Chinese message gets a short Chinese prefix instead of leaking that.
 */
export function errText(err: unknown): string {
  const raw =
    typeof err === "string"
      ? err
      : err instanceof Error
        ? err.message
        : typeof err === "object" && err !== null && "message" in err
          ? String((err as { message: unknown }).message)
          : String(err ?? "");
  const text = shorten(raw.trim().replace(/^(Error:\s*)+/i, ""), 120);
  if (!text) return "操作失败，请稍后重试";
  // A backend message we wrote ourselves is always Chinese; anything with an
  // ASCII letter in the first few words came from a library, not from us.
  if (/^[\x00-\x7f]*$/.test(text.slice(0, 24))) {
    if (/timeout|timed out/i.test(text)) return `网络超时，请检查网络后重试（${text}）`;
    if (/connect|dns|network|request/i.test(text)) return `网络请求失败，请检查网络后重试（${text}）`;
    return `操作失败（${text}）`;
  }
  return text;
}

/** Clips a long raw message so a toast stays one readable line. */
function shorten(text: string, max: number): string {
  return text.length <= max ? text : `${text.slice(0, max - 1)}…`;
}
