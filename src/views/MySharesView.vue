<script setup lang="ts">
import { computed, onActivated, ref, watch } from "vue";

import Icon from "@/components/Icon.vue";
import EmptyState from "@/components/EmptyState.vue";
import DriveLoader from "@/components/DriveLoader.vue";
import Modal from "@/components/Modal.vue";
import { api } from "@/lib/backend";
import { copyText } from "@/lib/clipboard";
import { formatFullDate } from "@/lib/format";
import { PROVIDERS, type ShareRecord } from "@/lib/types";
import { app, activeProviderState, toast } from "@/store/app";

/**
 * Shares the account has already created on the active drive.
 *
 * Four drives can list them (夸克 / 123 / 阿里 / 百度); 139 has no such endpoint
 * and its `shareCreate` capability is already false, so the sidebar entry is
 * hidden there rather than showing an empty page.
 *
 * What each drive reports differs a lot, and the backend normalises the gaps:
 * only 夸克 and 阿里 give a title the user recognises, 百度 only has the shared
 * path, 123 falls back to its own placeholder ("分享"). 百度 and 123 never echo
 * a passcode, so the code column stays empty for them even on coded shares.
 */
const rows = ref<ShareRecord[]>([]);
const loading = ref(false);
const busy = ref(false);
const error = ref<string | null>(null);
const revoking = ref<ShareRecord | null>(null);

const provider = computed(
  () => PROVIDERS.find((p) => p.id === app.activeProvider) ?? PROVIDERS[0],
);
const state = activeProviderState;
const signedIn = computed(() => state.value?.authenticated === true);
const supported = computed(() => state.value?.capabilities.shareCreate !== false);

/** Shares that can still be opened, newest first. */
const live = computed(() => rows.value.filter((row) => !row.expired));
const dead = computed(() => rows.value.filter((row) => row.expired));

function reset() {
  rows.value = [];
  error.value = null;
  revoking.value = null;
}

async function load() {
  if (!supported.value || !signedIn.value) {
    reset();
    return;
  }
  loading.value = true;
  error.value = null;
  try {
    rows.value = await api.shareRecords(app.activeProvider);
  } catch (err) {
    error.value = String(err);
    rows.value = [];
  } finally {
    loading.value = false;
  }
}

/** A share is only worth copying when the drive told us its address. */
function canCopy(row: ShareRecord) {
  return row.url.length > 0;
}

function line(row: ShareRecord) {
  return row.passcode ? `${row.url} 提取码：${row.passcode}` : row.url;
}

async function copy(row: ShareRecord) {
  if (!canCopy(row)) return;
  if (!(await copyText(line(row)))) {
    toast("复制失败，请手动选中链接", "error");
    return;
  }
  toast(row.passcode ? "已复制链接和提取码" : "已复制分享链接", "success");
}

async function confirmRevoke() {
  const row = revoking.value;
  if (!row || busy.value) return;
  busy.value = true;
  try {
    await api.shareRevoke(app.activeProvider, row.shareId, row.fids);
    toast("已取消分享", "success");
    revoking.value = null;
    await load();
  } catch (err) {
    toast(String(err), "error");
  } finally {
    busy.value = false;
  }
}

function expiry(row: ShareRecord) {
  if (row.expired) return "已过期";
  if (!row.expiredAt) return "永久有效";
  return `${formatFullDate(row.expiredAt)} 到期`;
}

// `<KeepAlive>` keeps this instance across drive switches, so reload on every
// activation instead of only on mount.
onActivated(load);

watch(
  () => app.activeProvider,
  () => {
    reset();
    void load();
  },
);
</script>

<template>
  <section class="mine">
    <header class="head">
      <div class="head-copy">
        <h1 class="title">我的分享</h1>
        <p class="sub">
          <template v-if="!signedIn">{{ provider.name }}未登录</template>
          <template v-else-if="!supported">{{ provider.name }}不支持创建分享</template>
          <template v-else>
            {{ provider.name }}里已经创建 {{ rows.length }} 个分享链接，可以随时取消
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

    <div class="body">
      <div v-if="loading && !rows.length" class="state">
        <DriveLoader :provider="app.activeProvider" caption="正在读取分享列表" />
      </div>

      <EmptyState
        v-else-if="!signedIn"
        icon="user"
        :title="`${provider.name}还没有登录`"
        description="登录之后才能查看已经创建的分享。"
      />

      <EmptyState
        v-else-if="!supported"
        icon="share"
        :title="`${provider.name}不支持分享`"
        description="这个网盘的接口没有开放创建分享，也就没有分享列表可以管理。"
      />

      <EmptyState
        v-else-if="error"
        icon="alert"
        title="读取分享列表失败"
        :description="error"
      >
        <button class="btn" @click="load">重试</button>
      </EmptyState>

      <EmptyState
        v-else-if="!rows.length"
        icon="share"
        title="还没有创建过分享"
        description="在文件上点右键选择「创建分享链接」，建好的分享会出现在这里。"
      />

      <template v-else>
        <div class="list">
          <article
            v-for="row in live"
            :key="row.shareId"
            class="card"
          >
            <div class="card-main">
              <div class="card-head">
                <span class="card-title truncate" :title="row.title">{{ row.title }}</span>
                <span v-if="row.passcode" class="tag">{{ row.passcode }}</span>
                <span class="tag tag--muted">{{ expiry(row) }}</span>
              </div>
              <p v-if="row.url" class="card-url mono truncate" :title="row.url">{{ row.url }}</p>
              <p v-else class="card-url dim">这个网盘没有返回链接地址</p>
              <div class="card-meta">
                <span>{{ formatFullDate(row.createdAt) }}</span>
                <span class="dot">·</span>
                <span>{{ row.fileNum }} 个文件</span>
                <template v-if="row.visitCount || row.saveCount || row.downloadCount">
                  <span class="dot">·</span>
                  <span>浏览 {{ row.visitCount }}</span>
                  <span class="dot">·</span>
                  <span>转存 {{ row.saveCount }}</span>
                  <span class="dot">·</span>
                  <span>下载 {{ row.downloadCount }}</span>
                </template>
              </div>
            </div>
            <div class="card-actions">
              <button class="btn" :disabled="!canCopy(row)" title="复制链接" @click="copy(row)">
                <Icon name="copy" :size="14" />
                <span>复制</span>
              </button>
              <button
                class="btn btn--danger"
                title="取消这个分享，链接会立刻失效"
                @click="revoking = row"
              >
                <Icon name="trash" :size="14" />
                <span>取消分享</span>
              </button>
            </div>
          </article>
        </div>

        <details v-if="dead.length" class="expired">
          <summary>已过期的分享（{{ dead.length }}）</summary>
          <div class="list">
            <article v-for="row in dead" :key="row.shareId" class="card card--dead">
              <div class="card-main">
                <div class="card-head">
                  <span class="card-title truncate" :title="row.title">{{ row.title }}</span>
                  <span class="tag tag--muted">已过期</span>
                </div>
                <p class="card-meta">
                  {{ formatFullDate(row.createdAt) }} 创建 · {{ row.fileNum }} 个文件
                </p>
              </div>
              <div class="card-actions">
                <button class="btn btn--danger" @click="revoking = row">
                  <Icon name="trash" :size="14" />
                  <span>删除记录</span>
                </button>
              </div>
            </article>
          </div>
        </details>
      </template>
    </div>

    <Modal
      :open="revoking !== null"
      title="取消分享"
      subtitle="取消之后链接会立刻失效，已经保存到别人网盘里的文件不受影响。"
      :width="420"
      @close="revoking = null"
    >
      <p class="revoke-hint">
        「{{ revoking?.title }}」的分享链接将无法再打开。
      </p>
      <template #footer>
        <button class="btn" @click="revoking = null">保留</button>
        <button class="btn btn--solid" :disabled="busy" @click="confirmRevoke">
          {{ busy ? "正在取消…" : "取消分享" }}
        </button>
      </template>
    </Modal>
  </section>
</template>

<style scoped>
.mine {
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
  padding: 22px 24px 10px;
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
.list {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.card {
  display: flex;
  align-items: center;
  gap: 14px;
  padding: 12px 14px;
  border: 1px solid var(--line);
  border-radius: var(--radius-md);
  background: var(--bg-raised);
}
.card--dead {
  opacity: 0.72;
}
.card-main {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.card-head {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.card-title {
  font-size: 13px;
  font-weight: 520;
  color: var(--text-1);
  min-width: 0;
}
.tag {
  flex: none;
  padding: 1px 6px;
  border-radius: 5px;
  background: var(--bg-sunken);
  box-shadow: inset 0 0 0 1px var(--line);
  font-size: 11px;
  color: var(--text-2);
  font-variant-numeric: tabular-nums;
}
.tag--muted {
  color: var(--text-3);
}
.card-url {
  font-size: 11.5px;
  color: var(--text-2);
}
.card-url.dim {
  color: var(--text-3);
}
.card-meta {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px 6px;
  font-size: 11px;
  color: var(--text-3);
}
.dot {
  opacity: 0.6;
}
.card-actions {
  display: flex;
  align-items: center;
  gap: 6px;
  flex: none;
}
.expired {
  margin-top: 16px;
}
.expired summary {
  cursor: pointer;
  padding: 4px 2px;
  font-size: 12px;
  color: var(--text-3);
  user-select: none;
}
.expired summary:hover {
  color: var(--text-2);
}
.expired .list {
  margin-top: 8px;
}
.revoke-hint {
  margin: 0;
  font-size: 12.5px;
  color: var(--text-2);
}

@media (max-width: 1179px) {
  .card-actions .btn span {
    display: none;
  }
  .card-actions .btn {
    padding: 0 10px;
  }
}
</style>
