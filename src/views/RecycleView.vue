<script setup lang="ts">
import { computed, onActivated, ref, watch } from "vue";

import { motion } from "motion-v";
import { rowEnter } from "@/lib/motion";
import Icon from "@/components/Icon.vue";
import EmptyState from "@/components/EmptyState.vue";
import DriveLoader from "@/components/DriveLoader.vue";
import Modal from "@/components/Modal.vue";
import { api } from "@/lib/backend";
import { errText, formatFullDate, formatSize } from "@/lib/format";
import { PROVIDERS, type RecycleEntry } from "@/lib/types";
import { app, activeProviderState, toast } from "@/store/app";

/**
 * Recycle bin of the active drive.
 *
 * Only three drives expose one: 夸克 (list/recover/remove), 123 云盘
 * (list/restore — no permanent delete) and 阿里云盘 (list/restore/clear — no
 * per-item delete). 百度网盘 and 中国移动云盘 reject the API calls outright
 * (百度 answers `errno:2` to every restore, 139 needs an `mcloud-sign` header we
 * cannot produce), so their bins are not offered at all.
 */
const rows = ref<RecycleEntry[]>([]);
const loading = ref(false);
const busy = ref(false);
const error = ref<string | null>(null);
const selected = ref<Set<string>>(new Set());
const confirmClear = ref(false);

const provider = computed(
  () => PROVIDERS.find((p) => p.id === app.activeProvider) ?? PROVIDERS[0],
);
const state = activeProviderState;
const supported = computed(() => state.value?.capabilities.recycle !== false);
const signedIn = computed(() => state.value?.authenticated === true);

const allSelected = computed(
  () => rows.value.length > 0 && selected.value.size === rows.value.length,
);

const totalSize = computed(() =>
  rows.value.reduce((sum, row) => sum + (selected.value.has(row.fid) ? row.size : 0), 0),
);

const picked = computed(() => rows.value.filter((row) => selected.value.has(row.fid)));

function reset() {
  rows.value = [];
  selected.value = new Set();
  error.value = null;
}

async function load() {
  if (!supported.value || !signedIn.value) {
    reset();
    return;
  }
  loading.value = true;
  error.value = null;
  try {
    rows.value = await api.recycleList(app.activeProvider, 0, 200);
    selected.value = new Set();
  } catch (err) {
    error.value = errText(err);
    rows.value = [];
  } finally {
    loading.value = false;
  }
}

function toggle(fid: string) {
  const next = new Set(selected.value);
  if (next.has(fid)) next.delete(fid);
  else next.add(fid);
  selected.value = next;
}

function toggleAll() {
  selected.value = allSelected.value ? new Set() : new Set(rows.value.map((row) => row.fid));
}

async function restore() {
  if (!picked.value.length) return;
  busy.value = true;
  try {
    const count = await api.recycleRestore(app.activeProvider, picked.value);
    toast(`已恢复 ${count} 项到${provider.value.name}`, "success");
    await load();
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    busy.value = false;
  }
}

async function purge() {
  if (!picked.value.length) return;
  busy.value = true;
  try {
    const count = await api.recyclePurge(app.activeProvider, picked.value);
    toast(`已彻底删除 ${count} 项`, "success");
    await load();
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    busy.value = false;
  }
}

async function clearAll() {
  busy.value = true;
  try {
    await api.recycleClear(app.activeProvider);
    toast(`已清空${provider.value.name}的回收站`, "success");
    confirmClear.value = false;
    await load();
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    busy.value = false;
  }
}

// `<KeepAlive>` keeps this instance across drive switches, so reload on every
// activation instead of only on mount (same trap as the login screen).
onActivated(load);

// Switching drives while the bin is open must not leave the old rows on screen.
watch(
  () => app.activeProvider,
  () => {
    reset();
    void load();
  },
);
</script>

<template>
  <section class="recycle">
    <header class="head">
      <div class="head-copy">
        <h1 class="title">回收站</h1>
        <p class="sub">
          <template v-if="!signedIn">{{ provider.name }}未登录</template>
          <template v-else-if="!supported">{{ provider.name }}不提供回收站</template>
          <template v-else>
            {{ provider.name }}里删除的内容会先留在这里，可以恢复到原位置
          </template>
        </p>
      </div>
      <button
        v-if="supported && signedIn"
        class="icon-btn"
        title="刷新"
        :disabled="loading"
        @click="load"
      >
        <Icon name="refresh" :size="15" :spin="loading" />
      </button>
    </header>

    <div v-if="supported && signedIn" class="actions">
      <div class="actions-left">
        <button class="btn" :disabled="!rows.length" @click="toggleAll">
          <Icon :name="allSelected ? 'check-circle' : 'list'" :size="14" />
          <span>{{ allSelected ? "取消全选" : "全选" }}</span>
        </button>
        <span v-if="selected.size" class="chip">
          已选 {{ selected.size }} 项 · {{ formatSize(totalSize) }}
        </span>
      </div>
      <div class="actions-right">
        <button class="btn" :disabled="busy || !selected.size" title="恢复到删除前的位置" @click="restore">
          <Icon name="retry" :size="14" />
          <span>恢复</span>
        </button>
        <button
          class="btn btn--danger"
          :disabled="busy || !selected.size"
          title="永久删除，不可撤销"
          @click="purge"
        >
          <Icon name="trash" :size="14" />
          <span>彻底删除</span>
        </button>
        <button
          v-if="app.activeProvider === 'aliyun'"
          class="btn btn--danger"
          :disabled="busy || !rows.length"
          title="清空整个回收站"
          @click="confirmClear = true"
        >
          <span>清空</span>
        </button>
      </div>
    </div>

    <div class="body">
      <div v-if="loading && !rows.length" class="state">
        <DriveLoader :provider="app.activeProvider" caption="正在读取回收站" />
      </div>

      <EmptyState
        v-else-if="!signedIn"
        icon="user"
        :title="`${provider.name}还没有登录`"
        description="登录之后才能查看它的回收站。"
      />

      <EmptyState
        v-else-if="!supported"
        icon="trash"
        :title="`${provider.name}不提供回收站`"
        description="这个网盘的接口没有开放回收站，删除的文件只能去网页版找回。"
      />

      <EmptyState
        v-else-if="error"
        icon="alert"
        title="读取回收站失败"
        :description="error"
      >
        <button class="btn" @click="load">重试</button>
      </EmptyState>

      <EmptyState
        v-else-if="!rows.length"
        icon="trash"
        title="回收站是空的"
        description="删除的文件会在这里保留一段时间。"
      />

      <div v-else class="rows">
        <motion.button
          v-for="(row, i) in rows"
          :key="row.fid"
          class="row"
          :class="{ 'row--on': selected.has(row.fid) }"
          v-bind="rowEnter(i)"
          @click="toggle(row.fid)"
        >
          <span class="check" :class="{ 'check--on': selected.has(row.fid) }">
            <Icon v-if="selected.has(row.fid)" name="check" :size="11" :stroke="2.2" />
          </span>
          <Icon :name="row.isDir ? 'folder' : 'file'" :size="15" class="row-glyph" />
          <span class="row-name truncate" :title="row.name">{{ row.name }}</span>
          <span class="row-size mono">{{ row.isDir ? "—" : formatSize(row.size) }}</span>
          <span class="row-time mono">{{ formatFullDate(row.deletedAt) }}</span>
        </motion.button>
      </div>
    </div>

    <Modal
      :open="confirmClear"
      title="清空回收站"
      subtitle="阿里云盘只支持一次性清空整个回收站，此操作不可撤销。"
      :width="420"
      @close="confirmClear = false"
    >
      <p class="clear-hint">回收站里的 {{ rows.length }} 项内容都会被永久删除。</p>
      <template #footer>
        <button class="btn" @click="confirmClear = false">取消</button>
        <button class="btn btn--solid" :disabled="busy" @click="clearAll">
          {{ busy ? "正在清空…" : "清空" }}
        </button>
      </template>
    </Modal>
  </section>
</template>

<style scoped>
.recycle {
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
.actions {
  display: flex;
  align-items: center;
  justify-content: space-between;
  flex-wrap: wrap;
  gap: 8px 12px;
  padding: 12px 24px 14px;
}
.actions-left,
.actions-right {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
  min-width: 0;
}
.actions-right {
  flex-wrap: nowrap;
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
.rows {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.row {
  display: grid;
  grid-template-columns: 18px 16px minmax(0, 1fr) 92px 148px;
  align-items: center;
  gap: 10px;
  width: 100%;
  height: 40px;
  padding: 0 12px;
  border-radius: var(--radius-sm);
  border: 1px solid transparent;
  background: transparent;
  color: var(--text-1);
  text-align: left;
  cursor: pointer;
  transition: background-color var(--dur-2) var(--ease-out),
    border-color var(--dur-2) var(--ease-out);
}
.row:hover {
  background: var(--bg-sunken);
}
.row--on {
  border-color: var(--line-strong);
  background: var(--bg-sunken);
}
.check {
  display: grid;
  place-items: center;
  width: 15px;
  height: 15px;
  border-radius: 5px;
  box-shadow: inset 0 0 0 1px var(--line-strong);
  color: var(--bg);
}
.check--on {
  background: var(--text-1);
  box-shadow: none;
}
.row-glyph {
  color: var(--text-3);
}
.row-name {
  font-size: 12.8px;
}
.row-size,
.row-time {
  font-size: 11.5px;
  color: var(--text-3);
  text-align: right;
}
.row-time {
  text-align: right;
}
.clear-hint {
  margin: 0;
  font-size: 12.5px;
  color: var(--text-2);
}

@media (max-width: 1179px) {
  .actions-left .btn span,
  .actions-right .btn span {
    display: none;
  }
  .actions-left .btn,
  .actions-right .btn {
    padding: 0 10px;
  }
}
</style>
