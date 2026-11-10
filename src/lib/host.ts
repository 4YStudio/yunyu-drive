/**
 * Host bridge.
 *
 * The UI is also served in a plain browser for design review (mock backend),
 * where none of the Tauri IPC plugins exist. Every native call goes through
 * this module so the components stay free of `try { ... } catch {}` noise and
 * a browser session never crashes on `undefined.metadata`.
 */
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { invoke } from "@tauri-apps/api/core";

export const IS_TAURI =
  typeof window !== "undefined" &&
  typeof (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ === "object";

export interface DropPayload {
  type: "over" | "enter" | "drop" | "leave";
  paths: string[];
}

/**
 * Keep the native window chrome in sync with the in-app theme.
 * Pass `null` to hand control back to the desktop environment.
 */
export async function hostSetTheme(theme: "light" | "dark" | null) {
  if (!IS_TAURI) {
    document.documentElement.style.colorScheme = theme ?? "light dark";
    return;
  }
  try {
    await getCurrentWindow().setTheme(theme);
  } catch {
    /* some window managers ignore the request */
  }
}

/**
 * The desktop's own colour scheme. WebKitGTK does not always resolve
 * `prefers-color-scheme` from the KDE/GNOME setting, so we ask the window
 * manager first and only fall back to the media query.
 */
export async function hostSystemTheme(): Promise<"light" | "dark" | null> {
  if (IS_TAURI) {
    // WebKitGTK reports `light` on a dark KDE desktop, so trust our own
    // detection of kdeglobals / GTK settings first.
    try {
      const detected = await invoke<"light" | "dark">("system_theme");
      if (detected === "light" || detected === "dark") return detected;
    } catch {
      /* fall through to the window theme */
    }
    try {
      const theme = await getCurrentWindow().theme();
      if (theme === "light" || theme === "dark") return theme;
    } catch {
      /* fall through to the media query */
    }
  }
  if (typeof window !== "undefined" && typeof window.matchMedia === "function") {
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  return null;
}

/** Fires whenever the desktop colour scheme changes. */
export async function onSystemThemeChange(handler: (theme: "light" | "dark") => void) {
  if (!IS_TAURI) return () => {};
  try {
    return await getCurrentWindow().onThemeChanged((event) => {
      handler(event.payload as "light" | "dark");
    });
  } catch {
    return () => {};
  }
}

export async function pickFiles(options?: {
  multiple?: boolean;
  directory?: boolean;
  title?: string;
  defaultPath?: string;
}): Promise<string[]> {
  if (!IS_TAURI) return [];
  try {
    const picked = await openDialog({
      multiple: options?.multiple ?? true,
      directory: options?.directory ?? false,
      title: options?.title,
      defaultPath: options?.defaultPath,
    });
    if (!picked) return [];
    return Array.isArray(picked) ? picked : [picked];
  } catch {
    return [];
  }
}

export async function revealPath(path: string | null | undefined) {
  if (!IS_TAURI || !path) return false;
  try {
    await revealItemInDir(path);
    return true;
  } catch {
    return false;
  }
}

/** Drag & drop is a webview feature; browsers fall back to the file picker. */
export async function onFileDrop(handler: (payload: DropPayload) => void) {
  if (!IS_TAURI) return () => {};
  try {
    return await getCurrentWebview().onDragDropEvent((event) => {
      const payload = event.payload as { type: string; paths?: string[] };
      handler({ type: payload.type as DropPayload["type"], paths: payload.paths ?? [] });
    });
  } catch {
    return () => {};
  }
}
