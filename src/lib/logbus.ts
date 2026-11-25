//! The app's own "devtools console" plumbing.
//!
//! Everything the backend logs through `ylog!` lands in a ring buffer that the
//! 日志 view polls; this module feeds the same buffer from the frontend side:
//! `console.*`, uncaught errors, unhandled rejections and (via `api.ts`) every
//! Tauri command with its duration. The point is that a user can open one
//! screen, reproduce a bug, and hand over something diagnostics-grade instead
//! of "it didn't work".
//!
//! Lines are batched (one IPC call per ~300 ms) so a chatty console cannot
//! slow the app down, and a failed send is dropped rather than retried — this
//! is diagnostics, not data.

import { invoke as rawInvoke } from "@tauri-apps/api/core";

import { IS_TAURI } from "@/lib/host";

export type LogLevel = "log" | "ok" | "warn" | "error" | "invoke";

interface Entry {
  level: LogLevel;
  target: string;
  message: string;
}

let queue: Entry[] = [];
let timer: number | null = null;
/** Guards against a log line that itself throws while flushing. */
let sending = false;

/** Longest string we put on one line, so a giant payload cannot flood the ring. */
const CLIP = 4000;

function clip(text: string): string {
  return text.length > CLIP ? `${text.slice(0, CLIP)}… (${text.length} 字符)` : text;
}

function describe(value: unknown): string {
  if (typeof value === "string") return value;
  if (value instanceof Error) return `${value.name}: ${value.message}`;
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

/** Queues one line for the in-app console. */
export function logLine(level: LogLevel, target: string, message: string): void {
  queue.push({ level, target, message: clip(message) });
  if (timer !== null) return;
  timer = window.setTimeout(() => {
    void flush();
  }, 300);
}

async function flush(): Promise<void> {
  timer = null;
  // Outside the desktop shell there is no ring buffer to feed.
  if (!IS_TAURI) {
    queue = [];
    return;
  }
  if (sending || queue.length === 0) return;
  sending = true;
  const batch = queue;
  queue = [];
  try {
    for (const entry of batch) {
      await rawInvoke<void>("log_emit", {
        level: entry.level,
        target: entry.target,
        message: entry.message,
      });
    }
  } catch {
    // The backend is gone (app shutting down) — drop the rest silently.
  } finally {
    sending = false;
    if (queue.length > 0 && timer === null) {
      timer = window.setTimeout(() => {
        void flush();
      }, 300);
    }
  }
}

/**
 * Mirrors `console.*` and the global error hooks into the log buffer.
 *
 * Called once from `main.ts`; the original console methods still run, so a dev
 * session still sees everything in the terminal.
 */
export function installConsoleCapture(): void {
  const mirror = (level: LogLevel, original: (...args: unknown[]) => void) => {
    return (...args: unknown[]): void => {
      logLine(level, "console", args.map(describe).join(" "));
      original(...args);
    };
  };
  // eslint-disable-next-line no-console
  console.log = mirror("log", console.log.bind(console));
  // eslint-disable-next-line no-console
  console.info = mirror("log", console.info.bind(console));
  // eslint-disable-next-line no-console
  console.warn = mirror("warn", console.warn.bind(console));
  // eslint-disable-next-line no-console
  console.error = mirror("error", console.error.bind(console));
  // eslint-disable-next-line no-console
  console.debug = mirror("log", console.debug.bind(console));

  window.addEventListener("error", (event) => {
    const where = event.filename ? ` (${event.filename}:${event.lineno})` : "";
    logLine("error", "window", `${describe(event.error ?? event.message)}${where}`);
  });
  window.addEventListener("unhandledrejection", (event) => {
    logLine("error", "promise", describe(event.reason));
  });
}
