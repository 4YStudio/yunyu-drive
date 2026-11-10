#!/usr/bin/env node
/**
 * Thin wrapper around the Tauri CLI so shortcuts like
 * `pnpm tauri build:deb` work (the CLI itself only knows `build`).
 *
 * Everything not listed in ALIASES is forwarded untouched, so
 * `pnpm tauri dev`, `pnpm tauri build`, `pnpm tauri icon …` keep working.
 */
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { existsSync } from "node:fs";

/** first argument -> extra argv prepended to the real CLI call */
const ALIASES = {
  "build:deb": ["build", "--bundles", "deb"],
  "build:rpm": ["build", "--bundles", "rpm"],
  "build:appimage": ["build", "--bundles", "appimage"],
  "build:fast": ["build", "--no-bundle"],
};

const here = dirname(fileURLToPath(import.meta.url));
const projectRoot = join(here, "..");

/** Prefer the locally installed CLI so we never depend on $PATH. */
function cliCommand() {
  const local = join(projectRoot, "node_modules", "@tauri-apps", "cli", "tauri.js");
  if (existsSync(local)) return { cmd: process.execPath, prepend: [local] };
  try {
    const require = createRequire(join(projectRoot, "package.json"));
    return { cmd: process.execPath, prepend: [require.resolve("@tauri-apps/cli/tauri.js")] };
  } catch {
    return { cmd: "tauri", prepend: [] };
  }
}

const argv = process.argv.slice(2);
const alias = ALIASES[argv[0]];
const args = alias ? [...alias, ...argv.slice(1)] : argv;

const { cmd, prepend } = cliCommand();
const child = spawn(cmd, [...prepend, ...args], {
  stdio: "inherit",
  cwd: process.cwd(),
  shell: process.platform === "win32",
});

child.on("error", (err) => {
  console.error(`[tauri] failed to launch the Tauri CLI: ${err.message}`);
  process.exit(1);
});

child.on("exit", (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 0);
});
