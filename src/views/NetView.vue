<script setup lang="ts">
import { computed, nextTick, onActivated, onDeactivated, ref } from "vue";

import Icon from "@/components/Icon.vue";
import { api } from "@/lib/backend";
import { copyText } from "@/lib/clipboard";
import { errText } from "@/lib/format";
import type { NetRecord } from "@/lib/types";
import { app, toast } from "@/store/app";

/**
 * Chrome-style "Network" panel.
 *
 * Every drive HTTP call is recorded by the backend (`src-tauri/src/net.rs`):
 * method, redacted url, status, duration, request/response headers and — when
 * the body is text and small enough — the body itself. That is how a refused
 * upload becomes visible: the drive answers `200` with
 * `{"code":23018,"message":"该类型文件不允许分享和保存"}`, and without this
 * panel the user only sees "上传失败".
 *
 * Bodies are capped at 12 KiB and secrets (`cookie`, `authorization`,
 * `sessionKey`, …) are replaced with `abcd…yz（N 字节）` on the way in, so the
 * panel can be copied and pasted into a bug report.
 */

const rows = ref<NetRecord[]>([]);
const lastSeq = ref(0);
const loading = ref(false);
const error = ref<string | null>(null);
const query = ref("");
const failedOnly = ref(false);
const paused = ref(false);
const autoScroll = ref(true);
const open = ref<Set<number>>(new Set());
const board = ref<HTMLElement | null>(null);

let timer: number | null = null;

/** The backend ring holds 400 records; this caps what the DOM mounts. */
const MAX_ROWS = 400;
const POLL_MS = 1000;

const failures = computed(() => rows.value.filter((row) => isFailure(row)).length);

const shown = computed(() => {
  const needle = query.value.trim().toLowerCase();
  return rows.value.filter((row) => {
    if (failedOnly.value && !isFailure(row)) return false;
    if (!needle) return true;
    return (
      row.url.toLowerCase().includes(needle) ||
      row.tag.toLowerCase().includes(needle) ||
      row.respBody.toLowerCase().includes(needle)
    );
  });
});

/**
 * Same rules as `Record::is_failure` in `src-tauri/src/net.rs`: a drive answers
 * `200` with `{"code":23018,"message":"该类型文件不允许分享和保存"}` when it
 * refuses an upload, and that has to be painted red.
 */
function isFailure(row: NetRecord): boolean {
  if (row.error) return true;
  if (row.status !== null && row.status >= 400) return true;
  const body = row.respBody;
  if (!body) return false;
  try {
    const value = JSON.parse(body) as Record<string, unknown>;
    if (value.state === false || value.success === false) return true;
    const bad = (key: string): boolean => {
      const code = value[key];
      if (typeof code === "number") return code !== 0 && code !== 200;
      if (typeof code === "string") {
        const text = code.trim();
        return text !== "" && text !== "0" && text !== "200";
      }
      return false;
    };
    return bad("code") || bad("res_code") || bad("errno") || bad("error_code");
  } catch {
    return (
      body.includes('"state":false') ||
      body.includes('"success":false') ||
      body.includes('"code":-') ||
      body.includes('"res_code":-')
    );
  }
}

/** `https://host/path?query` → `/path?query`, keeping the host when it differs. */
function shortUrl(url: string): string {
  try {
    const parsed = new URL(url);
    return `${parsed.pathname}${parsed.search}`;
  } catch {
    return url;
  }
}

function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return "";
  }
}

function size(bytes: number): string {
  if (!bytes) return "—";
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 ? value.toFixed(2) : value.toFixed(1)} ${units[unit]}`;
}

function toggle(seq: number): void {
  const next = new Set(open.value);
  if (next.has(seq)) next.delete(seq);
  else next.add(seq);
  open.value = next;
}

function headerText(rows: [string, string][]): string {
  return rows.map(([key, value]) => `${key}: ${value}`).join("\n");
}

async function scrollToEnd(): Promise<void> {
  await nextTick();
  const el = board.value;
  if (el) el.scrollTop = el.scrollHeight;
}

async function pull(): Promise<void> {
  if (paused.value) return;
  try {
    const batch = await api.netTail(lastSeq.value, 200);
    error.value = null;
    if (batch.length === 0) return;
    lastSeq.value = batch[batch.length - 1].seq;
    rows.value.push(...batch);
    if (rows.value.length > MAX_ROWS) {
      rows.value.splice(0, rows.value.length - MAX_ROWS);
    }
    if (autoScroll.value) void scrollToEnd();
  } catch (err) {
    error.value = errText(err);
  }
}

async function load(): Promise<void> {
  loading.value = true;
  try {
    rows.value = await api.netTail(0, MAX_ROWS);
    lastSeq.value = rows.value.length ? rows.value[rows.value.length - 1].seq : 0;
    error.value = null;
    if (autoScroll.value) void scrollToEnd();
  } catch (err) {
    error.value = errText(err);
  } finally {
    loading.value = false;
  }
}

function start(): void {
  if (timer !== null) return;
  timer = window.setInterval(() => void pull(), POLL_MS);
}

function stop(): void {
  if (timer !== null) {
    window.clearInterval(timer);
    timer = null;
  }
}

async function clear(): Promise<void> {
  try {
    await api.netClear();
    rows.value = [];
    lastSeq.value = 0;
    open.value = new Set();
    toast("已清空网络记录", "info");
  } catch (err) {
    toast(errText(err), "error");
  }
}

async function copyAll(): Promise<void> {
  const text = shown.value
    .map((row) => {
      const head = `${row.at} ${row.method} ${row.url} → ${row.status ?? row.error ?? "ERR"} · ${row.ms} ms · ${size(row.bytes)}`;
      const parts = [head, `[请求头]\n${headerText(row.reqHeaders)}`];
      if (row.reqBody) parts.push(`[请求体]\n${row.reqBody}`);
      parts.push(`[响应头]\n${headerText(row.respHeaders)}`);
      if (row.respBody) parts.push(`[响应体]\n${row.respBody}`);
      return parts.join("\n");
    })
    .join("\n\n");
  if (!text) {
    toast("没有可复制的内容", "info");
    return;
  }
  try {
    await copyText(text);
    toast(`已复制 ${shown.value.length} 条请求`, "success");
  } catch (err) {
    toast(errText(err), "error");
  }
}

onActivated(() => {
  void load();
  start();
});

// Keep-alive: stop polling while another view is on screen.
onDeactivated(stop);
</script>

<template>
  <section class="net">
    <header class="head">
      <div class="title">
        <h2>网络请求</h2>
        <p class="sub">
          网盘的每一次 HTTP 调用：方法、状态、耗时，以及请求头/体与响应头/体（上限 12 KiB，密钥类字段已脱敏）。
        </p>
      </div>
      <div class="actions">
        <button class="btn btn--ghost" @click="app.view = 'settings'">
          <Icon name="chevron-left" :size="13" />
          <span>返回设置</span>
        </button>
        <button class="btn btn--ghost" :disabled="loading" @click="load">
          <Icon name="refresh" :size="13" />
          <span>刷新</span>
        </button>
        <button class="btn btn--ghost" @click="copyAll">
          <Icon name="copy" :size="13" />
          <span>复制</span>
        </button>
        <button class="btn btn--ghost" @click="clear">
          <Icon name="trash" :size="13" />
          <span>清空</span>
        </button>
      </div>
    </header>

    <div class="bar">
      <button
        class="btn btn--ghost btn--small"
        :class="{ 'is-on': failedOnly }"
        @click="failedOnly = !failedOnly"
      >
        <Icon name="filter" :size="12.5" />
        <span>只看失败</span>
      </button>
      <input v-model="query" class="search" type="text" placeholder="搜索地址、网盘或响应内容…" />
      <button class="btn btn--ghost btn--small" @click="autoScroll = !autoScroll">
        <Icon :name="autoScroll ? 'check' : 'list'" :size="12.5" />
        <span>自动滚动</span>
      </button>
      <button class="btn btn--ghost btn--small" @click="paused = !paused">
        <Icon :name="paused ? 'play' : 'pause'" :size="12.5" />
        <span>{{ paused ? "已暂停" : "暂停" }}</span>
      </button>
    </div>

    <p v-if="error" class="error">
      <Icon name="alert" :size="13" />
      <span>{{ error }}</span>
    </p>

    <div ref="board" class="board">
      <p v-if="shown.length === 0" class="empty">
        <Icon name="info" :size="14" />
        <span>
          {{
            rows.length === 0
              ? "还没有请求记录。刷新一下文件列表或传一个文件，这里就会出现。"
              : "当前筛选下没有匹配的请求。"
          }}
        </span>
      </p>
      <div v-for="row in shown" :key="row.seq" class="req" :class="{ 'req--bad': isFailure(row) }">
        <button class="req-head" @click="toggle(row.seq)">
          <span class="time">{{ row.at }}</span>
          <span class="method">{{ row.method }}</span>
          <span class="status" :class="{ 'status--bad': isFailure(row) }">
            {{ row.status ?? "ERR" }}
          </span>
          <span class="url" :title="row.url">
            <span class="host">{{ hostOf(row.url) }}</span>
            <span class="path">{{ shortUrl(row.url) }}</span>
          </span>
          <span class="tag">{{ row.tag }}</span>
          <span class="ms">{{ row.ms }} ms</span>
          <span class="bytes">{{ size(row.bytes) }}</span>
          <Icon :name="open.has(row.seq) ? 'chevron-down' : 'chevron-right'" :size="12" />
        </button>
        <p v-if="row.error" class="req-error">{{ row.error }}</p>
        <div v-if="open.has(row.seq)" class="req-body">
          <div class="col">
            <h4>请求头</h4>
            <pre>{{ headerText(row.reqHeaders) || "—" }}</pre>
            <h4 v-if="row.reqBody">请求体</h4>
            <pre v-if="row.reqBody">{{ row.reqBody }}</pre>
          </div>
          <div class="col">
            <h4>响应头</h4>
            <pre>{{ headerText(row.respHeaders) || "—" }}</pre>
            <h4 v-if="row.respBody">响应体</h4>
            <pre v-if="row.respBody">{{ row.respBody }}</pre>
          </div>
        </div>
      </div>
    </div>

    <footer class="foot">
      <span>共 {{ shown.length }} 条{{ failures ? ` · ${failures} 条失败` : "" }}</span>
      <span class="hint">流式传输（上传/下载主体）不缓冲响应体，以免占内存</span>
    </footer>
  </section>
</template>

<style scoped>
.net {
  display: flex;
  flex-direction: column;
  gap: 10px;
  min-height: 0;
  height: 100%;
  padding: 18px 20px 14px;
}

.head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 12px;
}

.title h2 {
  margin: 0;
  font-size: 17px;
  font-weight: 600;
  color: var(--text);
}

.sub {
  margin: 3px 0 0;
  font-size: 12.2px;
  color: var(--text-dim);
}

.actions {
  display: flex;
  gap: 6px;
  flex-shrink: 0;
}

.bar {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

.bar .is-on {
  color: var(--text);
  border-color: var(--line-strong);
}

.search {
  flex: 1;
  min-width: 160px;
  height: 28px;
  padding: 0 10px;
  border-radius: 8px;
  border: 1px solid var(--line);
  background: var(--surface);
  color: var(--text);
  font-size: 12.5px;
}

.search:focus {
  outline: none;
  border-color: var(--line-strong);
}

.error {
  display: flex;
  align-items: center;
  gap: 6px;
  margin: 0;
  padding: 6px 10px;
  border-radius: 8px;
  background: color-mix(in oklab, #d9534f 12%, transparent);
  color: #d9534f;
  font-size: 12.4px;
}

.board {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  border: 1px solid var(--line);
  border-radius: 10px;
  background: var(--surface);
}

.empty {
  display: flex;
  align-items: center;
  gap: 6px;
  margin: 0;
  padding: 14px;
  color: var(--text-dim);
  font-size: 12.5px;
}

.req + .req {
  border-top: 1px solid var(--line);
}

.req--bad {
  background: color-mix(in oklab, #d9534f 7%, transparent);
}

.req-head {
  display: grid;
  grid-template-columns: 72px 46px 44px minmax(120px, 1fr) 64px 62px 68px 14px;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 5px 10px;
  border: 0;
  background: none;
  color: var(--text);
  font-size: 12.2px;
  font-family: var(--mono, ui-monospace, monospace);
  text-align: left;
  cursor: pointer;
}

.req-head:hover {
  background: var(--hover);
}

.time {
  color: var(--text-dim);
}

.method {
  color: var(--text-dim);
}

.status {
  color: var(--text-dim);
}

.status--bad {
  color: #d9534f;
  font-weight: 600;
}

.url {
  display: flex;
  gap: 4px;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.host {
  color: var(--text-dim);
}

.path {
  overflow: hidden;
  text-overflow: ellipsis;
}

.tag,
.ms,
.bytes {
  color: var(--text-dim);
  text-align: right;
}

.req-error {
  margin: 0;
  padding: 0 10px 6px 10px;
  color: #d9534f;
  font-size: 12px;
  font-family: var(--mono, ui-monospace, monospace);
  word-break: break-all;
}

.req-body {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 12px;
  padding: 2px 12px 12px;
}

@media (max-width: 900px) {
  .req-body {
    grid-template-columns: 1fr;
  }
}

.col {
  min-width: 0;
}

.col h4 {
  margin: 6px 0 4px;
  font-size: 11.5px;
  font-weight: 600;
  color: var(--text-dim);
  text-transform: none;
}

.col pre {
  margin: 0;
  padding: 8px 10px;
  max-height: 220px;
  overflow: auto;
  border-radius: 8px;
  border: 1px solid var(--line);
  background: var(--surface-2, var(--surface));
  color: var(--text);
  font-size: 11.8px;
  line-height: 1.5;
  font-family: var(--mono, ui-monospace, monospace);
  white-space: pre-wrap;
  word-break: break-all;
}

.foot {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  font-size: 11.8px;
  color: var(--text-dim);
}

.hint {
  opacity: 0.85;
}
</style>
