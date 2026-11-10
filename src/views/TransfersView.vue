<script setup lang="ts">
import { computed, onActivated, onMounted, ref } from "vue";

import Icon from "@/components/Icon.vue";
import EmptyState from "@/components/EmptyState.vue";
import Segmented from "@/components/Segmented.vue";
import { api } from "@/lib/backend";
import { revealPath } from "@/lib/host";
import { PROVIDERS, type TransferTask } from "@/lib/types";
import { formatSize, formatSpeed, formatEta } from "@/lib/format";
import { app, refreshTransfers, toast } from "@/store/app";

/**
 * Two independent axes, as two rows of tabs:
 *   kind   — all / uploads / downloads
 *   status — all / in progress / finished (done, failed and canceled alike)
 */
const kindFilter = ref<"all" | "upload" | "download">("all");
const statusFilter = ref<"all" | "active" | "finished">("all");

const ACTIVE = ["running", "hashing", "queued", "paused"];
const FINISHED = ["done", "failed", "canceled"];

const all = computed(() => app.transfers);

const visible = computed(() => {
  const list = [...all.value];
  list.sort((a, b) => b.createdAt - a.createdAt);
  return list.filter((task) => {
    if (kindFilter.value !== "all" && task.kind !== kindFilter.value) return false;
    if (statusFilter.value === "active") return ACTIVE.includes(task.status);
    if (statusFilter.value === "finished") return FINISHED.includes(task.status);
    return true;
  });
});

const counts = computed(() => ({
  active: all.value.filter((t) => ACTIVE.includes(t.status)).length,
  upload: all.value.filter((t) => t.kind === "upload").length,
  download: all.value.filter((t) => t.kind === "download").length,
}));

/** Logo + name of the drive a task belongs to (empty for share downloads). */
function providerLogo(task: TransferTask): string | undefined {
  return PROVIDERS.find((p) => p.id === task.provider)?.logo;
}

function providerName(task: TransferTask): string {
  return PROVIDERS.find((p) => p.id === task.provider)?.name ?? "";
}

function percent(task: TransferTask) {
  if (!task.size) return task.status === "done" ? 100 : 0;
  return Math.min(100, Math.round((task.transferred / task.size) * 100));
}

function statusLabel(task: TransferTask) {
  switch (task.status) {
    case "queued":
      return "排队中";
    case "paused":
      return "已暂停";
    case "hashing":
      return "校验中";
    case "running":
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
    toast(String(err), "error");
  }
}

async function cancel(task: TransferTask) {
  try {
    await api.transferCancel(task.id);
  } catch (err) {
    toast(String(err), "error");
  }
}

async function remove(task: TransferTask) {
  try {
    await api.transferRemove(task.id);
  } catch (err) {
    toast(String(err), "error");
  }
}

async function retry(task: TransferTask) {
  try {
    await api.transferRetry(task.id);
    toast("已重新开始，接着上次的进度继续", "success");
  } catch (err) {
    toast(String(err), "error");
  }
}

async function clearFinished() {
  try {
    await api.transferClear();
    toast("已清除完成的传输", "success");
  } catch (err) {
    toast(String(err), "error");
  }
}

async function reveal(task: TransferTask) {
  const ok = await revealPath(task.localPath);
  if (!ok) toast("无法打开文件管理器", "error");
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
    </div>

    <div class="body">
      <div v-if="!visible.length" class="state">
        <EmptyState
          icon="arrow-down"
          title="还没有传输任务"
          description="在文件页双击文件即可开始下载，拖拽本地文件到窗口即可上传。"
        />
      </div>

      <TransitionGroup v-else name="task" tag="div" class="tasks">
        <article v-for="task in visible" :key="task.id" class="task" :class="`task--${task.status}`">
          <div class="task-glyph">
            <Icon
              :name="task.kind === 'download' ? 'download' : 'upload'"
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
            </div>

            <div class="task-bar">
              <div
                class="task-bar-fill"
                :class="{ 'task-bar-fill--indeterminate': task.status === 'hashing' }"
                :style="{ width: task.status === 'hashing' ? '100%' : `${percent(task)}%` }"
              />
            </div>

            <div class="task-meta">
              <span class="mono">{{ formatSize(task.transferred) }} / {{ formatSize(task.size) }}</span>
              <template v-if="task.status === 'running' || task.status === 'paused'">
                <span class="dot">·</span>
                <template v-if="task.status === 'running'">
                  <span class="dot">·</span>
                  <span class="mono">{{ formatSpeed(task.speed) }}</span>
                  <span class="dot">·</span>
                  <span class="mono">{{ formatEta(task.size - task.transferred, task.speed) || "—" }}</span>
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
              v-if="task.status === 'done' && task.kind === 'download'"
              class="icon-btn"
              title="在文件夹中显示"
              @click="reveal(task)"
            >
              <Icon name="external" :size="14" />
            </button>
            <button
              v-if="task.kind === 'download' && ['failed', 'canceled', 'paused'].includes(task.status)"
              class="icon-btn"
              title="继续下载（断点续传）"
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
        </article>
      </TransitionGroup>
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
  transition: width var(--dur-3) var(--ease-out);
}
.task-bar-fill--indeterminate {
  background: linear-gradient(
    90deg,
    transparent 0%,
    var(--fill) 40%,
    var(--fill) 60%,
    transparent 100%
  );
  background-size: 40% 100%;
  background-repeat: no-repeat;
  animation: indeterminate 1.4s var(--ease-in-out) infinite;
}
@keyframes indeterminate {
  from {
    background-position: -45% 0;
  }
  to {
    background-position: 145% 0;
  }
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

.task-move,
.task-enter-active,
.task-leave-active {
  transition: opacity var(--dur-2) var(--ease-out),
    transform var(--dur-2) var(--ease-out);
}
.task-enter-from {
  opacity: 0;
  transform: translateY(-6px);
}
.task-leave-to {
  opacity: 0;
  transform: translateY(-6px);
}
.task-leave-active {
  position: absolute;
  width: 100%;
}
</style>
