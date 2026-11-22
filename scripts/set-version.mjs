#!/usr/bin/env node
/**
 * 改版本号：只改 `package.json` 的 `version` 一个地方。
 *
 * 其它需要版本号的地方都从它派生：
 *
 *   - 安装包版本（deb / rpm / NSIS / dmg 的文件名与元数据）
 *       ← `src-tauri/tauri.conf.json` 里写的是 `"version": "../package.json"`，
 *         Tauri 允许 version 填一个「含 version 字段的 JSON 路径」。
 *   - 应用内显示的版本（「关于」页、日志首行、夸克网页桥接的 /desktop_info）
 *       ← `src-tauri/build.rs` 读 package.json 并注入 `YUNYU_VERSION`，
 *         Rust 侧统一读 `crate::version::VERSION`。
 *   - `src-tauri/Cargo.toml` 里故意不写 version（缺省 0.0.0），
 *     所以升版本不会牵动 `Cargo.lock`。
 *
 * 用法：
 *   pnpm version:set 0.2.0
 *   node scripts/set-version.mjs v0.2.0     # 开头的 v 会被去掉
 *   node scripts/set-version.mjs            # 不带参数 = 只打印当前版本
 *
 * CI 里由 tag 推导版本（见 .github/workflows/release.yml），本地发版前跑一次。
 */
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const projectRoot = join(here, "..");
const packageJsonPath = join(projectRoot, "package.json");

const raw = process.argv[2];

if (!raw) {
  console.log(JSON.parse(readFileSync(packageJsonPath, "utf8")).version);
  process.exit(0);
}

const version = raw.replace(/^v/, "").trim();
// 允许 1.2.3 / 1.2.3-beta.1 / 1.2.3+build.5 —— 与 semver 以及 Tauri 的要求一致。
if (!/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(version)) {
  console.error(`[set-version] "${raw}" 不是合法的 semver 版本号（应形如 1.2.3 或 v1.2.3）`);
  process.exit(1);
}

const before = readFileSync(packageJsonPath, "utf8");
const json = JSON.parse(before);

if (json.version === version) {
  console.log(`[set-version] package.json 已是 ${version}，未改动`);
  process.exit(0);
}

// 重新序列化时保持原有缩进（2 空格）与末尾换行，避免制造无关 diff。
json.version = version;
writeFileSync(packageJsonPath, `${JSON.stringify(json, null, 2)}\n`);

console.log(`[set-version] ${json.name} ${version}`);
