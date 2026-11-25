<script setup lang="ts">
import { computed, nextTick, onActivated, onDeactivated, reactive, ref } from "vue";

import Icon from "@/components/Icon.vue";
import { api } from "@/lib/backend";
import { copyText } from "@/lib/clipboard";
import { errText } from "@/lib/format";
import type { LogEntry } from "@/lib/types";
import { toast } from "@/store/app";

/**
 * In-app console, the app's answer to "open DevTools".
 *
 * The backend already writes `ylog!` lines to `<data>/logs/app.log`, but a user
 * on a packaged build has nowhere to see them while reproducing a bug — so the
 * same lines are mirrored into a ring buffer and shown here, together with the
 * frontend half: `console.*`, uncaught errors, and every Tauri command with its
 * duration (that is the "network requests" equivalent for this app, since the
 * UI talks to the backend over IPC rather than HTTP).
 *
 * Polling instead of events on purpose: the log can be chatty, and one small
 * invoke per second is cheaper than a stream the UI has to coalesce.
 */

const rows = ref<LogEntry[]>([]);
const lastSeq = ref(0);
const loading = ref(false);
const error = ref<string | null>(null);
const logFile = ref<string | null>(null);
const query = ref("");
const autoScroll = ref(true);
const paused = ref(false);
const board = ref<HTMLElement | null>(null);
const filter = reactive<Record<string, boolean>>({
  log: true,
  invoke: true,
  ok: true,
  warn: true,
  error: true,
});

let timer: number | null = null;

/** Hard cap on what the view keeps mounted; the backend ring is the real limit. */
const MAX_ROWS = 2000;
const POLL_MS = 800;

const levels: { key: keyof typeof filter; label: string }[] = [
  { key: "log", label: "日志" },
  { key: "invoke", label: "调用" },
  { key: "ok", label: "成功" },
  { key: "warn", label: "警告" },
  { key: "error", label: "错误" },
];

const shown = computed(() => {
  const needle = query.value.trim().toLowerCase();
  return rows.value.filter((row) => {
    if (!filter[row.level as keyof typeof filter]) return false;
    if (!needle) return true;
    return (
      row.message.toLowerCase().includes(needle) ||
      row.target.toLowerCase().includes(needle)
    );
  });
});

const errorCount = computed(
  () => rows.value.filter((row) => row.level === "error").length,
);

async function pull(): Promise<void> {
  try {
    const batch = await api.logsTail(lastSeq.value, 500);
    if (batch.length === 0) return;
    lastSeq.value = batch[batch.length - 1].seq;
    rows.value.push(...batch);
    if (rows.value.length > MAX_ROWS) {
      rows.value.splice(0, rows.value.length - MAX_ROWS);
    }
    error.value = null;
    if (autoScroll.value) await scrollToEnd();
  } catch (err) {
    error.value = errText(err);
  }
}

async function scrollToEnd(): Promise<void> {
  await nextTick();
  const el = board.value;
  if (el) el.scrollTop = el.scrollHeight;
}

function start(): void {
  if (timer !== null) return;
  timer = window.setInterval(() => {
    if (!paused.value) void pull();
  }, POLL_MS);
}

function stop(): void {
  if (timer === null) return;
  window.clearInterval(timer);
  timer = null;
}

async function load(): Promise<void> {
  loading.value = true;
  try {
    rows.value = [];
    lastSeq.value = 0;
    logFile.value = await api.logsFile();
    await pull();
  } catch (err) {
    error.value = errText(err);
  } finally {
    loading.value = false;
  }
}

async function clear(): Promise<void> {
  try {
    await api.logsClear();
    rows.value = [];
    lastSeq.value = 0;
    toast("已清空当前日志（磁盘上的 app.log 保留）", "success");
  } catch (err) {
    toast(errText(err), "error");
  }
}

async function copyAll(): Promise<void> {
  const text = shown.value
    .map((row) => `${row.at} [${row.level}] ${row.target} — ${row.message}`)
    .join("\n");
  if (!text) {
    toast("没有可复制的内容", "info");
    return;
  }
  try {
    await copyText(text);
    toast(`已复制 ${shown.value.length} 行`, "success");
  } catch (err) {
    toast(errText(err), "error");
  }
}

function toggleScroll(): void {
  autoScroll.value = !autoScroll.value;
  if (autoScroll.value) void scrollToEnd();
}

onActivated(() => {
  void load();
  start();
});

// Keep-alive keeps the component alive; stop polling while another view is up.
onDeactivated(stop);
</script>

<template>
  <section class="logs">
    <header class="head">
      <div class="title">
        <h2>运行日志</h2>
        <p class="sub">
          控制台输出、前端调用与后端请求都记在这里；出现问题时把这里的内容复制给开发者。
        </p>
      </div>
      <div class="actions">
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
      <div class="chips">
        <label v-for="item in levels" :key="item.key" class="chip" :class="`chip--${item.key}`">
          <input v-model="filter[item.key]" type="checkbox" />
          <span>{{ item.label }}</span>
        </label>
      </div>
      <input v-model="query" class="search" type="text" placeholder="搜索日志…" />
      <button class="btn btn--ghost btn--small" @click="toggleScroll">
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
        <span>{{ rows.length === 0 ? "还没有日志。操作一下需要排查的功能，这里会实时出现。" : "当前筛选下没有匹配的日志。" }}</span>
      </p>
      <div v-for="row in shown" :key="row.seq" class="line" :class="`line--${row.level}`">
        <span class="time">{{ row.at }}</span>
        <span class="lvl">{{ row.level }}</span>
        <span class="target">{{ row.target }}</span>
        <span class="msg">{{ row.message }}</span>
      </div>
    </div>

    <footer class="foot">
      <span>共 {{ shown.length }} 行{{ errorCount ? ` · ${errorCount} 条错误` : "" }}</span>
      <span v-if="logFile" class="path">{{ logFile }}</span>
    </footer>
  </section>
</template>

<style scoped>
.logs {
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

.chips {
  display: flex;
  gap: 5px;
  flex-wrap: wrap;
}

.chip {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 2px 7px;
  border-radius: 999px;
  border: 1px solid var(--line);
  font-size: 11.5px;
  color: var(--text-dim);
  cursor: pointer;
  user-select: none;
}

.chip input {
  accent-color: var(--fill);
}

.chip--error {
  color: #d9534f;
}

.chip--warn {
  color: #c08a26;
}

.search {
  flex: 1 1 160px;
  min-width: 120px;
  padding: 5px 9px;
  border-radius: 7px;
  border: 1px solid var(--line);
  background: var(--surface);
  color: var(--text);
  font-size: 12.2px;
}

.search:focus {
  outline: none;
  border-color: var(--text-dim);
}

.error {
  display: flex;
  align-items: center;
  gap: 6px;
  margin: 0;
  font-size: 12.2px;
  color: #d9534f;
}

.board {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  border: 1px solid var(--line);
  border-radius: 9px;
  background: var(--surface);
  padding: 6px 8px;
  font-family: "JetBrains Mono", "Fira Code", ui-monospace, monospace;
  font-size: 11.6px;
  line-height: 1.55;
}

.empty {
  display: flex;
  align-items: center;
  gap: 6px;
  margin: 10px 4px;
  color: var(--text-dim);
  font-family: inherit;
  font-size: 12px;
}

.line {
  display: grid;
  grid-template-columns: 82px 52px 96px 1fr;
  gap: 8px;
  padding: 1px 2px;
  border-radius: 4px;
}

.line:hover {
  background: color-mix(in srgb, var(--fill) 8%, transparent);
}

.time {
  color: var(--text-dim);
  font-variant-numeric: tabular-nums;
}

.lvl {
  color: var(--text-dim);
  text-transform: uppercase;
  font-size: 10.4px;
  letter-spacing: 0.3px;
  padding-top: 1px;
}

.target {
  color: var(--text-dim);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.msg {
  color: var(--text);
  white-space: pre-wrap;
  word-break: break-word;
}

.line--error .msg,
.line--error .lvl {
  color: #d9534f;
}

.line--warn .msg,
.line--warn .lvl {
  color: #c08a26;
}

.line--ok .lvl {
  color: #4a9a5c;
}

.line--invoke .msg {
  color: var(--text-dim);
}

.foot {
  display: flex;
  justify-content: space-between;
  gap: 10px;
  font-size: 11.4px;
  color: var(--text-dim);
}

.path {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.btn--small {
  font-size: 11.6px;
  padding: 3px 8px;
}
</style>
