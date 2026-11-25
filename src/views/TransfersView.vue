<script setup lang="ts">
import { computed, onActivated, onDeactivated, onMounted, ref } from "vue";

import { motion } from "motion-v";
import { barTransition, rowEnter } from "@/lib/motion";
import Icon from "@/components/Icon.vue";
import ChunkFooter from "@/components/ChunkFooter.vue";
import EmptyState from "@/components/EmptyState.vue";
import Segmented from "@/components/Segmented.vue";
import Modal from "@/components/Modal.vue";
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
    else if (kindFilter.value === "download" && !isDownload(task)) return false;
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
    if (action === "remove") {
      await runRemove(list);
      return;
    }
    const ids = list.map((t) => t.id);
    const n = await api.transferBulk(ids, action);
    if (action === "pause") {
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
  download: all.value.filter((t) => isDownload(t)).length,
  cross: all.value.filter(isCross).length,
}));

/** Cross-drive jobs have their own kind: they download *and* upload. */
function isCross(task: TransferTask) {
  return task.kind === "copy" || task.kind === "move";
}

function isDownload(task: TransferTask) {
  return task.kind === "download" || task.kind === "folder";
}

/** A whole folder downloaded as one row. */
function isFolder(task: TransferTask) {
  return task.kind === "folder";
}

function kindLabel(task: TransferTask) {
  if (isCross(task)) return task.kind === "copy" ? "跨盘复制" : "跨盘移动";
  if (isFolder(task)) return "文件夹";
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
      // A folder is walking and downloading at the same time; before the first
      // file shows up there is nothing to count, so say what it is doing.
      if (isFolder(task)) return task.listing && !task.size ? "读取中" : "下载中";
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

/**
 * Removal has two meanings and only one of them is safe to assume.
 *
 * An unfinished task owns nothing but a `.part` placeholder, so dropping it is
 * free. A finished one owns a real file on disk — the user may still want it,
 * and the app cannot know that for them. So: ask, and let a batch remember the
 * answer, or selecting a hundred rows would mean a hundred dialogs.
 */
const removeAsk = ref<{ task: TransferTask; queue: TransferTask[] } | null>(null);
const removeRest = ref(true);
let removeMemory: boolean | null = null;

async function runRemove(tasks: TransferTask[]) {
  removeMemory = null;
  removeAsk.value = null;
  await nextRemove([...tasks]);
}

async function nextRemove(queue: TransferTask[]) {
  const head = queue.shift();
  if (!head) return;
  if (removeMemory !== null) {
    await applyRemove([head], removeMemory);
    return nextRemove(queue);
  }
  if (head.status !== "done") {
    // Nothing finished: the placeholders and partial trees go without asking.
    await applyRemove([head], true);
    return nextRemove(queue);
  }
  removeAsk.value = { task: head, queue };
}

async function confirmRemove(purgeLocal: boolean) {
  const ask = removeAsk.value;
  if (!ask) return;
  if (removeRest.value) removeMemory = purgeLocal;
  removeAsk.value = null;
  await applyRemove([ask.task], purgeLocal);
  await nextRemove(ask.queue);
}

function cancelRemove() {
  removeAsk.value = null;
}

async function applyRemove(tasks: TransferTask[], purgeLocal: boolean) {
  const ids = tasks.map((task) => task.id);
  try {
    if (ids.length === 1) await api.transferRemove(ids[0], purgeLocal);
    else await api.transferBulk(ids, "remove", purgeLocal);
    forgetTransfers(ids);
    clearSelection();
  } catch (err) {
    toast(errText(err), "error");
  }
}

/** Path shown in the confirm dialog: a folder task owns the whole tree. */
const removeTarget = computed(() => removeAsk.value?.task.localPath ?? "");

// ------------------------------------------------------------ expand a folder ---

/** Which folder row is unfolded, and the files it is working on right now. */
const expanded = ref<string | null>(null);
const children = ref<TransferTask[]>([]);
let childTimer: ReturnType<typeof setInterval> | undefined;

async function loadChildren() {
  const id = expanded.value;
  if (!id) return;
  try {
    children.value = await api.transferChildren(id);
  } catch {
    children.value = [];
  }
}

function stopChildren() {
  if (childTimer !== undefined) clearInterval(childTimer);
  childTimer = undefined;
  children.value = [];
}

function collapse() {
  expanded.value = null;
  stopChildren();
}

async function toggleExpand(task: TransferTask) {
  if (expanded.value === task.id) {
    collapse();
    return;
  }
  expanded.value = task.id;
  children.value = [];
  await loadChildren();
  stopChildren();
  expanded.value = task.id;
  // Children are hidden from the event stream on purpose — a big folder has
  // thousands of them and each event would re-render this list. The open row
  // polls instead, and only while it is open.
  childTimer = setInterval(loadChildren, 900);
}

onDeactivated(collapse);

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
              :name="
                isCross(task)
                  ? 'arrow-up-down'
                  : isFolder(task)
                    ? 'folder'
                    : task.kind === 'download'
                      ? 'download'
                      : 'upload'
              "
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
              <button
                v-if="isFolder(task)"
                class="kid-toggle"
                :aria-expanded="expanded === task.id"
                :title="expanded === task.id ? '收起文件列表' : '展开查看正在下载的文件'"
                @click.stop="toggleExpand(task)"
              >
                <Icon :name="expanded === task.id ? 'chevron-down' : 'chevron-right'" :size="11" />
              </button>
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
              <template v-if="isFolder(task)">
                <span class="dot">·</span>
                <span class="mono">
                  {{ task.childrenDone ?? 0 }} / {{ task.childrenTotal ?? 0 }} 个文件
                </span>
                <span v-if="task.listing" class="dot">·</span>
                <span v-if="task.listing" class="task-pool">正在读取</span>
                <template v-if="task.childrenFailed">
                  <span class="dot">·</span>
                  <span class="task-error">{{ task.childrenFailed }} 个文件失败</span>
                </template>
              </template>
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
              v-if="task.status === 'done' && task.kind === 'download'"
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
              :title="
                isCross(task)
                  ? '继续传输'
                  : isFolder(task)
                    ? '继续下载（已下好的文件会跳过）'
                    : '继续下载（断点续传）'
              "
              @click="retry(task)"
            >
              <Icon name="play" :size="14" />
            </button>
            <button
              v-if="['done', 'failed', 'canceled'].includes(task.status)"
              class="icon-btn"
              title="移除任务"
              @click="runRemove([task])"
            >
              <Icon name="trash" :size="14" />
            </button>
          </div>

          <div v-if="expanded === task.id" class="kids">
            <p v-if="children.length === 0" class="kids-empty">
              {{ task.listing ? "正在读取文件夹…" : "没有正在下载的文件" }}
            </p>
            <div v-for="kid in children" :key="kid.id" class="kid">
              <span class="kid-name truncate" :title="kid.name">{{ kid.name }}</span>
              <div class="kid-bar">
                <div
                  class="kid-bar-fill"
                  :style="{ width: `${kid.size ? Math.min(100, (kid.transferred / kid.size) * 100) : 0}%` }"
                />
              </div>
              <span class="kid-size mono">
                {{ formatSize(kid.transferred) }} / {{ formatSize(kid.size) }}
              </span>
            </div>
          </div>
        </motion.article>

        <ChunkFooter :mounted="pagedRows.length" :rest="rest" noun="传输" @more="more()" />
      </div>

      <Modal
        :open="removeAsk !== null"
        title="移除任务"
        :width="430"
        @close="cancelRemove"
      >
        <p class="rm-text">
          <b>{{ removeAsk?.task.name }}</b> 已经下载完成，本地文件还在：
        </p>
        <p class="rm-path mono" :title="removeTarget">{{ removeTarget }}</p>
        <p class="rm-hint">
          只移除记录不会动这个文件；移除并删除会把它从磁盘上删掉（无法恢复）。
        </p>
        <label v-if="(removeAsk?.queue.length ?? 0) > 0" class="rm-remember">
          <input v-model="removeRest" type="checkbox" />
          <span>对剩下的 {{ removeAsk?.queue.length }} 项都这样处理</span>
        </label>
        <template #footer>
          <button class="btn" @click="cancelRemove">取消</button>
          <button class="btn btn--danger" @click="confirmRemove(true)">移除并删除文件</button>
          <button class="btn btn--solid" @click="confirmRemove(false)">只移除记录</button>
        </template>
      </Modal>
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

/* Fold/unfold a folder row. Small targets on purpose: it sits inside the name
   column, not in the action strip. */
.kid-toggle {
  flex: none;
  display: grid;
  place-items: center;
  width: 16px;
  height: 16px;
  margin-right: -3px;
  border: none;
  border-radius: 5px;
  background: transparent;
  color: var(--text-3);
  cursor: pointer;
  transition: background 0.14s ease, color 0.14s ease;
}
.kid-toggle:hover {
  background: var(--bg-3);
  color: var(--text-2);
}

.kids {
  grid-column: 1 / -1;
  margin: 2px 0 2px 30px;
  padding: 4px 0 6px 8px;
  border-left: 1px solid var(--line);
  display: flex;
  flex-direction: column;
  gap: 5px;
}
.kids-empty {
  margin: 0;
  color: var(--text-3);
  font-size: 11.8px;
}
.kid {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 90px 118px;
  align-items: center;
  gap: 10px;
  font-size: 11.8px;
  color: var(--text-2);
}
.kid-name {
  min-width: 0;
}
.kid-bar {
  height: 3px;
  border-radius: 999px;
  background: var(--bg-3);
  overflow: hidden;
}
.kid-bar-fill {
  height: 100%;
  border-radius: inherit;
  background: var(--accent);
  transition: width 0.24s linear;
}
.kid-size {
  text-align: right;
  color: var(--text-3);
  font-size: 11px;
}

.rm-text {
  margin: 0 0 6px;
  font-size: 13px;
  color: var(--text-2);
}
.rm-path {
  margin: 0 0 10px;
  padding: 7px 9px;
  border-radius: 8px;
  background: var(--bg-3);
  font-size: 11.5px;
  color: var(--text-2);
  word-break: break-all;
}
.rm-hint {
  margin: 0 0 12px;
  font-size: 12px;
  color: var(--text-3);
  line-height: 1.55;
}
.rm-remember {
  display: flex;
  align-items: center;
  gap: 7px;
  font-size: 12.2px;
  color: var(--text-2);
}
</style>
