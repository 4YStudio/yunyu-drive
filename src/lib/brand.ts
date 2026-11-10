/**
 * Product identity in one place.
 *
 * The app aggregates several cloud drives, so anything that used to say
 * "夸克网盘" now has to say "云语聚合网盘" — keeping the strings here means a
 * rename is one edit instead of a grep across views, the tray and the bundle.
 */
export const BRAND = {
  /** Chinese product name, used in UI copy and window titles. */
  name: "云语聚合网盘",
  /** Latin name shown next to the mark in the title bar. */
  latin: "Yunyu Drive",
  /** Shown in the About row. */
  product: "云语聚合网盘",
  /** `localStorage` namespace; bumping it isolates old settings. */
  storagePrefix: "yunyu",
  /** Custom protocol the app answers. */
  scheme: "yunyu-drive",
} as const;

/** `localStorage` key inside the brand namespace. */
export function storageKey(name: string): string {
  return `${BRAND.storagePrefix}.${name}`;
}
