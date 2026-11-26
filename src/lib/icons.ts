/*
 * Minimal stroke icon set (24×24, 1.7px, round caps) rendered inline so the
 * app needs no icon font and inherits currentColor.
 */
export type IconName =
  | "search" | "x" | "plus" | "upload" | "download" | "folder"
  | "folder-plus" | "refresh" | "more" | "chevron-right" | "chevron-down"
  | "chevron-left" | "chevron-up" | "check" | "sun" | "moon" | "settings" | "user"
  | "rename" | "trash" | "list" | "grid" | "clock" | "alert" | "info"
  | "loader" | "cloud" | "drive" | "external" | "logout" | "sparkles"
  | "copy" | "filter" | "check-circle" | "pause" | "play" | "arrow-up"
  | "arrow-down" | "arrow-up-down" | "keyboard" | "palette" | "database" | "shield"
  | "window-minimize" | "window-maximize" | "window-restore"
  | "window-close" | "grip" | "share" | "link" | "save" | "user-circle"
  | "move" | "retry" | "bell" | "tray" | "file" | "globe" | "gauge";

export const ICON_PATHS: Record<IconName, string> = {
  search: "M11 4a7 7 0 1 0 0 14 7 7 0 0 0 0-14ZM20 20l-4.2-4.2",
  x: "M6 6l12 12M18 6L6 18",
  plus: "M12 5v14M5 12h14",
  upload: "M12 16V4M8 8l4-4 4 4M4 16v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2",
  download: "M12 4v12M8 12l4 4 4-4M4 18v1a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-1",
  folder: "M3 7.5A2.5 2.5 0 0 1 5.5 5h3.2c.6 0 1.2.24 1.6.7L11.5 7h7A2.5 2.5 0 0 1 21 9.5v7A2.5 2.5 0 0 1 18.5 19h-13A2.5 2.5 0 0 1 3 16.5v-9Z",
  "folder-plus": "M3 7.5A2.5 2.5 0 0 1 5.5 5h3.2c.6 0 1.2.24 1.6.7L11.5 7h7A2.5 2.5 0 0 1 21 9.5v7A2.5 2.5 0 0 1 18.5 19h-13A2.5 2.5 0 0 1 3 16.5v-9ZM12 11v5M9.5 13.5h5",
  refresh: "M20 12a8 8 0 1 1-2.35-5.65M20 4v4.5h-4.5",
  more: "M6 12h.01M12 12h.01M18 12h.01",
  "chevron-right": "M9.5 6l6 6-6 6",
  "chevron-down": "M6 9.5l6 6 6-6",
  "chevron-left": "M14.5 6l-6 6 6 6",
  "chevron-up": "M6 14.5l6-6 6 6",
  check: "M5 12.5l4.5 4.5L19 7.5",
  "check-circle": "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18ZM8.5 12l2.5 2.5 4.5-4.5",
  sun: "M12 7.5a4.5 4.5 0 1 0 0 9 4.5 4.5 0 0 0 0-9ZM12 2v2M12 20v2M4.2 4.2l1.4 1.4M18.4 18.4l1.4 1.4M2 12h2M20 12h2M4.2 19.8l1.4-1.4M18.4 5.6l1.4-1.4",
  moon: "M20 14.5A8.5 8.5 0 0 1 9.5 4a8.5 8.5 0 1 0 10.5 10.5Z",
  settings: "M12 15.2a3.2 3.2 0 1 0 0-6.4 3.2 3.2 0 0 0 0 6.4Z M19.4 15a1.6 1.6 0 0 0 .32 1.77l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06A1.6 1.6 0 0 0 15 19.4a1.6 1.6 0 0 0-.97 1.47V21a2 2 0 1 1-4 0v-.09A1.6 1.6 0 0 0 9 19.4a1.6 1.6 0 0 0-1.77.32l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.6 1.6 0 0 0 4.6 15a1.6 1.6 0 0 0-1.47-.97H3a2 2 0 1 1 0-4h.09A1.6 1.6 0 0 0 4.6 9a1.6 1.6 0 0 0-.32-1.77l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.6 1.6 0 0 0 9 4.6h.09A1.6 1.6 0 0 0 10.56 3.13V3a2 2 0 1 1 4 0v.09a1.6 1.6 0 0 0 .97 1.47 1.6 1.6 0 0 0 1.77-.32l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.6 1.6 0 0 0 19.4 9v.09a1.6 1.6 0 0 0 1.47.97H21a2 2 0 1 1 0 4h-.09a1.6 1.6 0 0 0-1.51.94Z",
  user: "M12 12.5a4 4 0 1 0 0-8 4 4 0 0 0 0 8ZM4.5 20a7.5 7.5 0 0 1 15 0",
  rename: "M4 16.5V20h3.5L19 8.5 15.5 5 4 16.5Z M14 6.5l3.5 3.5",
  trash: "M4 7h16M9.5 7V5.5A1.5 1.5 0 0 1 11 4h2a1.5 1.5 0 0 1 1.5 1.5V7M6.5 7l.8 12a2 2 0 0 0 2 1.9h5.4a2 2 0 0 0 2-1.9l.8-12M10 11v6M14 11v6",
  list: "M8 7h12M8 12h12M8 17h12M4 7h.01M4 12h.01M4 17h.01",
  grid: "M4 4h6v6H4V4ZM14 4h6v6h-6V4ZM4 14h6v6H4v-6ZM14 14h6v6h-6v-6Z",
  clock: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18ZM12 7.5V12l3 2",
  alert: "M12 8v5M12 16.5h.01M10.3 4.3 2.9 17a2 2 0 0 0 1.7 3h14.8a2 2 0 0 0 1.7-3L13.7 4.3a2 2 0 0 0-3.4 0Z",
  info: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18ZM12 11v5M12 8h.01",
  loader: "M12 3v3M12 18v3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M3 12h3M18 12h3M5.6 18.4l2.1-2.1M16.3 7.7l2.1-2.1",
  cloud: "M7 18h10a4 4 0 0 0 .4-7.98A5.5 5.5 0 0 0 7.1 8.5 4.75 4.75 0 0 0 7 18Z",
  drive: "M4 15h16M6.5 15 9 7.5A2 2 0 0 1 10.9 6h2.2a2 2 0 0 1 1.9 1.5L17.5 15M6 19h12",
  external: "M14 4h6v6M20 4l-8.5 8.5M18 14v4a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4",
  logout: "M15 17l5-5-5-5M20 12H9M12 4H6a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h6",
  sparkles: "M12 3l1.6 4.4L18 9l-4.4 1.6L12 15l-1.6-4.4L6 9l4.4-1.6L12 3ZM18.5 15l.9 2.1 2.1.9-2.1.9-.9 2.1-.9-2.1-2.1-.9 2.1-.9.9-2.1Z",
  copy: "M9 9h9a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H9a2 2 0 0 1-2-2v-9a2 2 0 0 1 2-2ZM5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1",
  gauge: "M12 14l3.5-3.5M4.5 19a9 9 0 1 1 15 0",
  globe: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18ZM3 12h18M12 3c2.5 2.4 4 5.6 4 9s-1.5 6.6-4 9c-2.5-2.4-4-5.6-4-9s1.5-6.6 4-9Z",
  filter: "M4 6h16M7 12h10M10 18h4",
  pause: "M9 5v14M15 5v14",
  play: "M7 4.5v15l13-7.5-13-7.5Z",
  "arrow-up": "M12 19V5M6 11l6-6 6 6",
  "arrow-down": "M12 5v14M6 13l6 6 6-6",
  // A document with a folded corner: "open the finished file".
  file: "M13.5 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8.5L13.5 3ZM13.5 3v5.5H19",
  // Two shafts, one head up and one down: "transfer" in both directions.
  "arrow-up-down": "M8 19.5V5M4.5 8.5L8 5l3.5 3.5M16 4.5V19M12.5 15.5L16 19l3.5-3.5",
  keyboard: "M3 6h18a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1ZM7 10h.01M11 10h.01M15 10h.01M17 10h.01M7 14h10",
  palette: "M12 21a9 9 0 1 1 0-18c4.5 0 8 3 8 7 0 2.5-2 4-4 4h-1.5a1.8 1.8 0 0 0-1.3 3 1.6 1.6 0 0 1-1.2 2H12ZM7.5 10.5h.01M10.5 7.5h.01M14.5 7.5h.01M17 10.5h.01",
  database: "M12 8c4.4 0 8-1.34 8-3s-3.6-3-8-3-8 1.34-8 3 3.6 3 8 3ZM4 5v14c0 1.66 3.6 3 8 3s8-1.34 8-3V5M4 12c0 1.66 3.6 3 8 3s8-1.34 8-3",
  shield: "M12 21s7-3.2 7-9V6l-7-3-7 3v6c0 5.8 7 9 7 9Z",
  "window-minimize": "M5 12h14",
  "window-maximize": "M6.5 6.5h11v11h-11z",
  "window-restore": "M9 9h9v9H9zM6 15V6h9",
  "window-close": "M6.5 6.5l11 11M17.5 6.5l-11 11",
  "grip": "M9 6h.01M15 6h.01M9 12h.01M15 12h.01M9 18h.01M15 18h.01",
  share: "M18 8a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5ZM6 14.5a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5ZM18 21a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5ZM8.2 11.1l7.6-4.2M8.2 12.9l7.6 4.2",
  link: "M10.5 13.5a4 4 0 0 0 5.66 0l2.34-2.34a4 4 0 1 0-5.66-5.66l-1.2 1.2M13.5 10.5a4 4 0 0 0-5.66 0L5.5 12.84a4 4 0 1 0 5.66 5.66l1.2-1.2",
  move: "M12 3v18M3 12h18M12 3l-3 3M12 3l3 3M12 21l-3-3M12 21l3-3M3 12l3-3M3 12l3 3M21 12l-3-3M21 12l-3 3",
  retry: "M4 12a8 8 0 1 0 3.05-6.3M4 5v4.5h4.5",
  bell: "M18 8a6 6 0 1 0-12 0c0 5-2 6-2 6h16s-2-1-2-6ZM10.3 20a2 2 0 0 0 3.4 0",
  tray: "M3 13h5l1.5 2.5h5L16 13h5M3 13l2.2-6.1A2 2 0 0 1 7.1 5.5h9.8a2 2 0 0 1 1.9 1.4L21 13v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4Z",
  save: "M12 3v11M8 10.5l4 4 4-4M4 17v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2",
  "user-circle": "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18ZM12 12.2a3.1 3.1 0 1 0 0-6.2 3.1 3.1 0 0 0 0 6.2ZM5.6 18.4a6.6 6.6 0 0 1 12.8 0",
};
