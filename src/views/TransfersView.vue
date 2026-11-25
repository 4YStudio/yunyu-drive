<script setup lang="ts">
import { computed, onActivated, onMounted, ref } from "vue";

import { motion } from "motion-v";
import { barTransition, rowEnter } from "@/lib/motion";
import Icon from "@/components/Icon.vue";
import ChunkFooter from "@/components/ChunkFooter.vue";
import EmptyState from "@/components/EmptyState.vue";
import Segmented from "@/components/Segmented.vue";
import { api } from "@/lib/backend";
import { openFile, revealPath } from "@/lib/host";
import { PROVIDERS, type TransferTask } from "@/lib/types";
import { errText, formatSize, formatSpeed, formatEta } from "@/lib/format";
import { useChunkedRows } from "@/lib/progressive";
import { app, forgetTransfers, refreshTransfers, toast } from "@/store/app";

/**
 * Two independent axes, as two rows of tabs:
 *   kind   — all / uploads / downloads
 *   status — all / in progress / finished (done, failed and canceled alike)
 */
const kindFilter = ref<"all" | "upload" | "download" | "cross">("all");
const statusFilter = ref<"all" | "active" | "finished">("all");

const ACTIVE = ["running", "hashing", "queued", "paused"];
const FINISHED = ["done", "failed", "canceled"];

const all = computed(() => app.transfers);

const visible = computed(() => {
  const list = [...all.value];
  list.sort((a, b) => b.createdAt - a.createdAt);
  return list.filter((task) => {
    if (kindFilter.value === "cross" && !isCross(task)) return false;
    else if (kindFilter.value === "upload" && task.kind !== "upload") return false;
    else if (kindFilter.value === "download" && task.kind !== "download") return false;
    if (statusFilter.value === "active") return ACTIVE.includes(task.status);
    if (statusFilter.value === "finished") return FINISHED.includes(task.status);
    return true;
  });
});

/**
 * Only the first chunk is mounted; a folder download can add thousands of rows
 * in one go and WebKitGTK stalls laying them all out at once.
 */
const {
  rows: pagedRows,
  rest,
  more,
  onScroll,
} = useChunkedRows(visible, {
  resetKey: () => `${kindFilter.value}|${statusFilter.value}`,
});

/** Ids ticked for a batch action. */
const selected = ref<Set<string>>(new Set());
const picked = computed(() => visible.value.filter((task) => selected.value.has(task.id)));
const selectedAll = computed(
  () => visible.value.length > 0 && picked.value.length === visible.value.length,
);

function toggleSelect(task: TransferTask) {
  const next = new Set(selected.value);
  if (next.has(task.id)) next.delete(task.id);
  else next.add(task.id);
  selected.value = next;
}

function toggleSelectAll() {
  selected.value = selectedAll.value ? new Set() : new Set(visible.value.map((t) => t.id));
}

function clearSelection() {
  selected.value = new Set();
}

/** Only downloads and cross-drive jobs can pick up where they stopped. */
function canResume(task: TransferTask) {
  return isDownload(task) || isCross(task);
}

const bulkBusy = ref(false);

/**
 * One IPC call for the whole selection.
 *
 * Doing it per task meant thousands of round trips and thousands of events; the
 * backend pauses/cancels in one pass and announces the batch once.
 */
async function bulk(action: "pause" | "cancel" | "remove" | "resume") {
  const list = picked.value;
  if (!list.length || bulkBusy.value) return;
  bulkBusy.value = true;
  try {
    if (action === "resume") {
      const runnable = list.filter(canResume).map((t) => t.id);
      const skipped = list.length - runnable.length;
      if (!runnable.length) {
        toast("选中的任务都不能续传，上传任务需要重新上传", "error");
        return;
      }
      const n = await api.transferBulk(runnable, "resume");
      toast(
        `已继续 ${n} 项${skipped ? `，另有 ${skipped} 项不支持续传` : ""}`,
        "success",
      );
      return;
    }
    const ids = list.map((t) => t.id);
    const n = await api.transferBulk(ids, action);
    if (action === "remove") {
      forgetTransfers(ids);
      clearSelection();
      toast(`已移除 ${n} 条记录`, "success");
    } else if (action === "pause") {
      toast(n ? `已暂停 ${n} 项` : "选中的任务当前都不在运行", "success");
    } else {
      toast(n ? `已取消 ${n} 项` : "选中的任务当前都不在运行", "success");
    }
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    bulkBusy.value = false;
  }
}

const counts = computed(() => ({
  active: all.value.filter((t) => ACTIVE.includes(t.status)).length,
  upload: all.value.filter((t) => t.kind === "upload").length,
  download: all.value.filter((t) => t.kind === "download").length,
  cross: all.value.filter(isCross).length,
}));

/** Cross-drive jobs have their own kind: they download *and* upload. */
function isCross(task: TransferTask) {
  return task.kind === "copy" || task.kind === "move";
}

function isDownload(task: TransferTask) {
  return task.kind === "download";
}

function kindLabel(task: TransferTask) {
  if (isCross(task)) return task.kind === "copy" ? "跨盘复制" : "跨盘移动";
  return task.kind === "download" ? "下载" : "上传";
}

/** Logo + name of the drive a task belongs to (empty for share downloads). */
function providerLogo(task: TransferTask): string | undefined {
  return PROVIDERS.find((p) => p.id === task.provider)?.logo;
}

function providerName(task: TransferTask): string {
  return PROVIDERS.find((p) => p.id === task.provider)?.name ?? "";
}

function percent(task: TransferTask) {
  // While the file is being read (hashing) the upload itself has not moved any
  // bytes yet, so `transferred` is still 0 and the bar used to sit at full with
  // an indeterminate shimmer. Show what has actually been read instead.
  const prepared = task.prepared ?? 0;
  if (task.status === "hashing" && prepared > 0 && task.size > 0) {
    return Math.min(100, Math.round((prepared / task.size) * 100));
  }
  if (!task.size) return task.status === "done" ? 100 : 0;
  return Math.min(100, Math.round((task.transferred / task.size) * 100));
}

/** Bytes read so far while the file is still being prepared. */
function preparedSize(task: TransferTask) {
  return task.status === "hashing" ? (task.prepared ?? 0) : 0;
}

/**
 * "256 线程 · 38/183 片", or empty when the transfer is not segmented.
 *
 * The connection count is the number the engine actually has open, which is the
 * per-account budget (or however many pieces are left, whichever is smaller) —
 * not what the drive would allow in theory.
 */
function poolLabel(task: TransferTask): string {
  const connections = task.connections ?? 0;
  const segments = task.segments ?? 0;
  if (connections <= 0 && segments <= 0) return "";
  const parts: string[] = [];
  if (connections > 0) parts.push(`${connections} 线程`);
  if (segments > 0) parts.push(`${task.segmentsDone ?? 0}/${segments} 片`);
  return parts.join(" · ");
}

function statusLabel(task: TransferTask) {
  switch (task.status) {
    case "queued":
      return "排队中";
    case "paused":
      return "已暂停";
    case "hashing":
      // Baidu hashes the whole file before the first byte goes up, which can
      // take as long as the upload itself on a big file. "校验中" made that
      // look like the transfer was stuck checking integrity.
      return task.kind === "download" ? "校验中" : "预处理中";
    case "running":
      if (isCross(task)) return task.kind === "copy" ? "复制中" : "移动中";
      return task.kind === "download" ? "下载中" : "上传中";
    case "done":
      return task.rapid ? "秒传完成" : "已完成";
    case "failed":
      return "失败";
    case "canceled":
      return "已取消";
    default:
      return task.status;
  }
}

async function pause(task: TransferTask) {
  try {
    await api.transferPause(task.id);
  } catch (err) {
    toast(errText(err), "error");
  }
}

async function cancel(task: TransferTask) {
  try {
    await api.transferCancel(task.id);
  } catch (err) {
    toast(errText(err), "error");
  }
}

async function remove(task: TransferTask) {
  try {
    await api.transferRemove(task.id);
  } catch (err) {
    toast(errText(err), "error");
  }
}

async function retry(task: TransferTask) {
  try {
    await api.transferRetry(task.id);
    toast("已重新开始，接着上次的进度继续", "success");
  } catch (err) {
    toast(errText(err), "error");
  }
}

async function clearFinished() {
  try {
    await api.transferClear();
    toast("已清除完成的传输", "success");
  } catch (err) {
    toast(errText(err), "error");
  }
}

async function reveal(task: TransferTask) {
  const ok = await revealPath(task.localPath);
  if (!ok) toast("无法打开文件管理器", "error");
}

async function open(task: TransferTask) {
  const ok = await openFile(task.localPath);
  if (!ok) toast("无法打开文件", "error");
}

const anyFinished = computed(() =>
  all.value.some((t) => ["done", "failed", "canceled"].includes(t.status)),
);

onMounted(() => {
  void refreshTransfers().catch(() => {});
});

// Kept alive between sidebar switches, so re-sync whenever the tab regains focus.
onActivated(() => {
  void refreshTransfers().catch(() => {});
});
</script>

<template>
  <section class="transfers">
    <header class="head">
      <div>
        <h1 class="title">传输列表</h1>
        <p class="sub">
          {{ counts.active ? `${counts.active} 项进行中` : "暂无进行中的传输" }}
        </p>
      </div>
      <button class="btn btn--ghost" :disabled="!anyFinished" @click="clearFinished">
        <Icon name="trash" :size="14" />
        <span>清除已完成</span>
      </button>
    </header>

    <div class="filters">
      <Segmented
        v-model="kindFilter"
        :options="[
          { value: 'all', label: '全部' },
          { value: 'upload', label: '上传' },
          { value: 'download', label: '下载' },
          ...(counts.cross ? [{ value: 'cross', label: '跨盘' }] : []),
        ]"
      />
      <Segmented
        v-model="statusFilter"
        :options="[
          { value: 'all', label: '全部' },
          { value: 'active', label: '进行中' },
          { value: 'finished', label: '已完成' },
        ]"
      />
      <label v-if="visible.length" class="pick pick--all">
        <input type="checkbox" :checked="selectedAll" @change="toggleSelectAll" />
        <span>全选 {{ visible.length }} 项</span>
      </label>
    </div>

    <div v-if="picked.length" class="bulk">
      <span class="bulk-count">已选 {{ picked.length }} 项</span>
      <button class="btn btn--ghost bulk-btn" :disabled="bulkBusy" @click="bulk('pause')">
        暂停
      </button>
      <button class="btn btn--ghost bulk-btn" :disabled="bulkBusy" @click="bulk('resume')">
        继续
      </button>
      <button class="btn btn--ghost bulk-btn" :disabled="bulkBusy" @click="bulk('cancel')">
        取消
      </button>
      <button class="btn btn--ghost bulk-btn" :disabled="bulkBusy" @click="bulk('remove')">
        移除记录
      </button>
      <button class="btn btn--ghost bulk-btn" @click="clearSelection">取消选择</button>
    </div>

    <div class="body" @scroll.passive="onScroll">
      <div v-if="!visible.length" class="state">
        <EmptyState
          icon="arrow-down"
          title="还没有传输任务"
          description="在文件页双击文件即可开始下载，拖拽本地文件到窗口即可上传。"
        />
      </div>

      <div v-else class="tasks">
        <motion.article
          v-for="(task, i) in pagedRows"
          :key="task.id"
          class="task"
          :class="`task--${task.status}`"
          v-bind="rowEnter(i)"
        >
          <label class="pick" @click.stop>
            <input
              type="checkbox"
              :checked="selected.has(task.id)"
              @change="toggleSelect(task)"
            />
          </label>
          <div class="task-glyph">
            <Icon
              :name="isCross(task) ? 'arrow-up-down' : task.kind === 'download' ? 'download' : 'upload'"
              :size="15"
              :stroke="1.7"
            />
          </div>

          <div class="task-main">
            <div class="task-line">
              <img
                v-if="providerLogo(task)"
                class="task-badge"
                :src="providerLogo(task)"
                :alt="providerName(task)"
                :title="providerName(task)"
                draggable="false"
              />
              <span class="task-name truncate" :title="task.name">{{ task.name }}</span>
              <span class="task-state">{{ statusLabel(task) }}</span>
              <span v-if="isCross(task)" class="task-kind">{{ kindLabel(task) }}</span>
            </div>

            <div class="task-bar">
              <motion.div
                class="task-bar-fill"
                :class="{ 'task-bar-fill--prep': task.status === 'hashing' }"
                :animate="{ width: `${percent(task)}%` }"
                :transition="barTransition"
              />
            </div>

            <div class="task-meta">
              <span class="mono">
                {{ formatSize(task.transferred + preparedSize(task)) }} /
                {{ formatSize(task.size) }}
              </span>
              <template v-if="task.status === 'running'">
                <span class="dot">·</span>
                <span class="mono">{{ formatSpeed(task.speed) }}</span>
                <span class="dot">·</span>
                <span class="mono">{{ formatEta(task.size - task.transferred, task.speed) || "—" }}</span>
                <template v-if="poolLabel(task)">
                  <span class="dot">·</span>
                  <span class="mono task-pool">{{ poolLabel(task) }}</span>
                </template>
              </template>
              <template v-else-if="task.status === 'failed' && task.error">
                <span class="dot">·</span>
                <span class="task-error truncate" :title="task.error">{{ task.error }}</span>
              </template>
            </div>
          </div>

          <div class="task-actions">
            <button
              v-if="['running', 'hashing', 'queued'].includes(task.status)"
              class="icon-btn"
              title="暂停"
              @click="pause(task)"
            >
              <Icon name="pause" :size="14" />
            </button>
            <button
              v-if="['running', 'hashing', 'queued', 'paused'].includes(task.status)"
              class="icon-btn"
              title="取消"
              @click="cancel(task)"
            >
              <Icon name="x" :size="14" />
            </button>
            <button
              v-if="task.status === 'done' && isDownload(task)"
              class="icon-btn"
              title="打开文件"
              @click="open(task)"
            >
              <Icon name="file" :size="14" />
            </button>
            <button
              v-if="task.status === 'done' && isDownload(task)"
              class="icon-btn"
              title="在文件夹中显示"
              @click="reveal(task)"
            >
              <Icon name="folder" :size="14" />
            </button>
            <button
              v-if="(isDownload(task) || isCross(task)) && ['failed', 'canceled', 'paused'].includes(task.status)"
              class="icon-btn"
              :title="isCross(task) ? '继续传输' : '继续下载（断点续传）'"
              @click="retry(task)"
            >
              <Icon name="play" :size="14" />
            </button>
            <button
              v-if="['done', 'failed', 'canceled'].includes(task.status)"
              class="icon-btn"
              title="移除记录"
              @click="remove(task)"
            >
              <Icon name="trash" :size="14" />
            </button>
          </div>
        </motion.article>

        <ChunkFooter :mounted="pagedRows.length" :rest="rest" noun="传输" @more="more()" />
      </div>
    </div>
  </section>
</template>

<style scoped>
.transfers {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-width: 0;
  background: var(--bg);
}
.head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 16px;
  padding: 22px 24px 6px;
}
.title {
  margin: 0;
  font-size: 17px;
  font-weight: 580;
  letter-spacing: -0.015em;
}
.sub {
  margin: 3px 0 0;
  font-size: 12px;
  color: var(--text-3);
}
.filters {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 12px 24px 14px;
  flex-wrap: wrap;
}
.body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 0 24px 24px;
}
/* Selection is a mode, so it gets its own strip above the list instead of
   living in a menu: the actions have to be one click away when a batch of a
   thousand rows is selected. */
.bulk {
  display: flex;
  align-items: center;
  gap: 4px;
  flex-wrap: wrap;
  margin: 0 24px 10px;
  padding: 6px 8px 6px 12px;
  border-radius: var(--radius-md);
  background: var(--bg-sunken);
  box-shadow: inset 0 0 0 1px var(--line);
}
.bulk-count {
  margin-right: 6px;
  font-size: 11.5px;
  font-weight: 560;
  color: var(--text-2);
}
.bulk-btn {
  height: 26px;
  padding: 0 10px;
  font-size: 11.5px;
}
.pick {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  flex: none;
  cursor: pointer;
}
.pick input {
  width: 13px;
  height: 13px;
  accent-color: var(--fill);
  cursor: pointer;
}
.pick--all {
  margin-left: auto;
  font-size: 11.5px;
  color: var(--text-3);
}
.state {
  display: grid;
  place-items: center;
  min-height: 60%;
}
.tasks {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.task {
  display: flex;
  align-items: center;
  gap: 13px;
  padding: 12px 12px 12px 14px;
  border-radius: var(--radius-md);
  border: 1px solid var(--line);
  background: var(--bg);
  transition: border-color var(--dur-2) var(--ease-out),
    background-color var(--dur-2) var(--ease-out);
}
.task:hover {
  border-color: var(--line-strong);
}
.task-glyph {
  display: grid;
  place-items: center;
  width: 32px;
  height: 32px;
  flex: none;
  border-radius: 10px;
  color: var(--text-2);
  background: var(--bg-sunken);
  box-shadow: inset 0 0 0 1px var(--line);
}
.task-main {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.task-line {
  display: flex;
  align-items: baseline;
  gap: 10px;
}
.task-name {
  flex: 1;
  font-size: 12.8px;
  font-weight: 500;
}
.task-state {
  flex: none;
  font-size: 10.5px;
  font-weight: 560;
  letter-spacing: 0.03em;
  text-transform: uppercase;
  color: var(--text-3);
}
.task-kind {
  flex: none;
  height: 16px;
  padding: 0 6px;
  border-radius: 999px;
  font-size: 10px;
  line-height: 16px;
  color: var(--text-3);
  box-shadow: inset 0 0 0 1px var(--line);
}
.task-bar {
  position: relative;
  height: 3px;
  border-radius: 999px;
  background: var(--bg-active);
  overflow: hidden;
}
.task-bar-fill {
  height: 100%;
  border-radius: inherit;
  background: var(--fill);
  /* Width is animated by motion-v (`barTransition`). */
}
/* Reading the file (hashing) is a separate stage: the bar refills from 0 once
   bytes really go up, so the dimmer fill makes that restart read as a new phase
   instead of a glitch. */
.task-bar-fill--prep {
  opacity: 0.55;
}
.task-meta {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 10.8px;
  color: var(--text-3);
  min-width: 0;
}
.task-error {
  color: var(--text-2);
}
.task-pool {
  color: var(--text-2);
}
.dot {
  opacity: 0.55;
}
.task-actions {
  display: flex;
  align-items: center;
  gap: 2px;
  flex: none;
}
.task-badge {
  flex: none;
  width: 14px;
  height: 14px;
  object-fit: contain;
  opacity: 0.9;
}
.task--done .task-state {
  color: var(--text-2);
}
.task--failed .task-bar-fill {
  background: var(--text-3);
}
/* Paused keeps its progress visible but muted, so it reads as "waiting",
   not as "broken". */
.task--paused .task-bar-fill {
  background: var(--text-3);
}
.task--paused .task-state {
  color: var(--text-2);
}

:where(.task) {
  /* `motion.article` — see src/lib/motion.ts. */
  transform: translateY(0);
}
</style>
