<script setup lang="ts">
import { computed, onActivated, onDeactivated, ref, watch } from "vue";
import { pickFiles } from "@/lib/host";

import { motion } from "motion-v";
import { rowEnter, rowTransition } from "@/lib/motion";
import Icon from "@/components/Icon.vue";
import Avatar from "@/components/Avatar.vue";
import FileIcon from "@/components/FileIcon.vue";
import ChunkFooter from "@/components/ChunkFooter.vue";
import EmptyState from "@/components/EmptyState.vue";
import { api } from "@/lib/backend";
import { errText } from "@/lib/format";
import { PROVIDERS } from "@/lib/types";
import type { ShareEntry, ShareListing, ShareSession } from "@/lib/types";
import { formatDate, formatSize, fileKind, kindLabel } from "@/lib/format";
import { useChunkedRows } from "@/lib/progressive";
import { app, refreshQuota, requestFilesRefresh, toast } from "@/store/app";

const link = ref("");
/** Only needed when the link itself carries no `?pwd=`; some drives require it. */
const passcode = ref("");
const busy = ref(false);
const error = ref<string | null>(null);

const session = ref<ShareSession | null>(null);
const listing = ref<ShareListing | null>(null);
const crumbs = ref<{ fid: string; name: string }[]>([]);

const selected = ref<Set<string>>(new Set());
const saving = ref(false);
const downloading = ref(false);

const provider = computed(
  () => PROVIDERS.find((p) => p.id === session.value?.provider) ?? null,
);
const rows = computed(() => listing.value?.entries ?? []);
/**
 * A share can hold the same thousands of entries as a drive folder, and the
 * listing is read all at once, so the rows are mounted in chunks too.
 */
const {
  rows: pagedRows,
  rest,
  more,
  onScroll,
} = useChunkedRows(rows, { resetKey: () => crumbs.value.map((c) => c.fid).join(">") });
const selectedEntries = computed(() =>
  rows.value.filter((entry) => selected.value.has(entry.fid)),
);
const totalSize = computed(() =>
  rows.value.reduce((sum, entry) => sum + (entry.isDir ? 0 : entry.size), 0),
);
const expires = computed(() => {
  const at = session.value?.expiredAt ?? 0;
  if (!at) return null;
  // Quark reports far-future timestamps for permanent shares.
  return at > Date.now() + 10 * 365 * 86_400_000 ? null : at;
});

async function open(target?: string) {
  const raw = (target ?? link.value).trim();
  if (!raw) return;
  let value = normalizeLink(raw);
  const typed = passcode.value.trim();
  // A passcode typed into the box wins over whatever the link carried.
  if (typed && !/[?&]pwd=/.test(value)) {
    value += `${value.includes("?") ? "&" : "?"}pwd=${encodeURIComponent(typed)}`;
  }
  link.value = value;
  busy.value = true;
  error.value = null;
  session.value = null;
  listing.value = null;
  selected.value = new Set();
  try {
    const opened = await api.shareOpen(value);
    session.value = opened;
    const root = rootFid(opened.provider);
    crumbs.value = [{ fid: root, name: opened.title || "分享内容" }];
    await listDir(root);
  } catch (err) {
    error.value = errText(err);
  } finally {
    busy.value = false;
  }
}

/// Root folder id, which every drive spells differently.
function rootFid(provider: string) {
  if (provider === "yun139" || provider === "aliyun") return "root";
  if (provider === "baidu") return "/";
  return "0";
}

/// Turns `yunyu-drive://share?url=…` / scheme-less links into the canonical
/// `https://pan.quark.cn/s/<id>?pwd=<code>` form so the input stays readable.
function normalizeLink(input: string) {
  let text = input.trim();
  if (text.startsWith("yunyu-drive://")) {
    const query = text.split("?")[1] ?? "";
    const params = new URLSearchParams(query);
    const inner = params.get("url") || params.get("link");
    if (inner) return normalizeLink(decodeURIComponent(inner));
    const pwdId = params.get("pwd_id") || params.get("pwdid") || "";
    const passcode = params.get("passcode") || params.get("pwd") || "";
    if (!pwdId) return text;
    text = `https://pan.quark.cn/s/${pwdId}`;
    if (passcode) text += `?pwd=${passcode}`;
    return text;
  }
  if (!text.includes("://")) text = `https://${text}`;
  return text;
}

async function listDir(pdirFid: string) {
  if (!session.value) return;
  try {
    listing.value = await api.shareList(
      session.value.pwdId,
      session.value.stoken,
      pdirFid,
      session.value.provider,
      session.value.passcode,
    );
    selected.value = new Set();
  } catch (err) {
    toast(errText(err), "error");
  }
}

async function enter(entry: ShareEntry) {
  if (!entry.isDir) return;
  crumbs.value = [...crumbs.value, { fid: entry.fid, name: entry.name }];
  await listDir(entry.fid);
}

async function goToCrumb(index: number) {
  const next = crumbs.value.slice(0, index + 1);
  if (next.length === 0) {
    next.push({
      fid: rootFid(session.value?.provider ?? "quark"),
      name: session.value?.title ?? "分享内容",
    });
  }
  crumbs.value = next;
  await listDir(next[next.length - 1].fid);
}

async function goUp() {
  if (crumbs.value.length <= 1) return;
  await goToCrumb(crumbs.value.length - 2);
}

function toggle(entry: ShareEntry) {
  const next = new Set(selected.value);
  if (next.has(entry.fid)) next.delete(entry.fid);
  else next.add(entry.fid);
  selected.value = next;
}

function toggleAll() {
  selected.value =
    selected.value.size === rows.value.length
      ? new Set()
      : new Set(rows.value.map((entry) => entry.fid));
}

function targetEntries() {
  return selectedEntries.value.length ? selectedEntries.value : rows.value;
}

async function save() {
  const current = session.value;
  const items = targetEntries();
  if (!current || items.length === 0) {
    toast("这个位置没有可转存的内容", "error");
    return;
  }
  if (!current.canSave) {
    toast("这个网盘不支持转存分享内容", "error");
    return;
  }
  saving.value = true;
  try {
    const count = await api.shareSave(
      current.pwdId,
      current.stoken,
      items.map((entry) => ({ fid: entry.fid, token: entry.token })),
      undefined,
      current.provider,
      current.passcode,
    );
    toast(`已转存 ${count} 项到「我的文件」`, "success");
    requestFilesRefresh();
    // 转存落到当前网盘，占用立刻变了。
    refreshQuota();
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    saving.value = false;
  }
}

async function download() {
  const current = session.value;
  const items = targetEntries();
  if (!current || items.length === 0) {
    toast("这个位置没有可下载的内容", "error");
    return;
  }
  const picked = await pickFiles({
    multiple: false,
    directory: true,
    title: "选择下载到哪个文件夹",
    defaultPath: app.settings?.downloadDir,
  });
  if (picked.length === 0) return;
  downloading.value = true;
  try {
    const tasks = await api.shareDownload(
      current.pwdId,
      current.stoken,
      items.map((entry) => ({
        fid: entry.fid,
        name: entry.name,
        size: entry.size,
        isDir: entry.isDir,
        token: entry.token,
      })),
      picked[0],
      current.provider,
      current.passcode,
    );
    toast(`已加入下载队列 ${tasks.length} 项`, "success");
    app.view = "transfers";
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    downloading.value = false;
  }
}

function onKeydown(event: KeyboardEvent) {
  const target = event.target as HTMLElement | null;
  if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA")) return;
  if (event.key === "Backspace") {
    event.preventDefault();
    void goUp();
  } else if (event.key === "Escape") {
    selected.value = new Set();
  }
}

onActivated(() => window.addEventListener("keydown", onKeydown));
onDeactivated(() => window.removeEventListener("keydown", onKeydown));
watch(
  () => app.shareNonce,
  () => {
    if (app.shareLink) void open(app.shareLink);
  },
  { immediate: true },
);
</script>

<template>
  <section class="share">
    <header class="head">
      <div>
        <h1 class="title">分享链接</h1>
        <p class="sub">粘贴分享链接，可直接转存到自己的网盘或下载到本地</p>
      </div>
      <a
        v-if="session?.webUrl"
        class="btn btn--ghost"
        :href="session.webUrl"
        target="_blank"
        rel="noreferrer"
      >
        <Icon name="external" :size="14" />
        <span>在浏览器打开</span>
      </a>
    </header>

    <div class="bar">
      <div class="link-box">
        <Icon name="link" :size="15" class="link-icon" />
        <input
          v-model="link"
          class="link-input"
          type="text"
          spellcheck="false"
          placeholder="粘贴夸克 / 139 / 阿里 / 123 / 百度 / 天翼 分享链接"
          @keydown.enter.prevent="open()"
        />
        <button v-if="link" class="clear" @click="link = ''">
          <Icon name="x" :size="12" />
        </button>
      </div>
      <input
        v-model="passcode"
        class="pass-input"
        type="text"
        spellcheck="false"
        placeholder="提取码"
        @keydown.enter.prevent="open()"
      />
      <button class="btn btn--solid" :disabled="busy || !link.trim()" @click="open()">
        {{ busy ? "正在打开…" : "打开" }}
      </button>
    </div>

    <AnimatePresence>
      <motion.div
        v-if="error"
        class="alert"
        :initial="{ opacity: 0, y: -4 }"
        :animate="{ opacity: 1, y: 0 }"
        :exit="{ opacity: 0, transition: { duration: 0.1 } }"
        :transition="rowTransition"
      >
        <Icon name="alert" :size="15" />
        <span>{{ error }}</span>
      </motion.div>
    </AnimatePresence>

    <template v-if="session">
      <AnimatePresence>
        <motion.div
          class="session"
        :initial="{ opacity: 0, y: 6 }"
        :animate="{ opacity: 1, y: 0 }"
        :exit="{ opacity: 0, transition: { duration: 0.1 } }"
        :transition="rowTransition"
      >
        <Avatar :src="session.ownerAvatar" :name="session.owner" :size="34" />
        <div class="session-copy">
          <div class="session-title truncate">
            <img v-if="provider" class="session-logo" :src="provider.logo" alt="" />
            {{ session.title || "未命名分享" }}
          </div>
          <div class="session-meta">
            <span v-if="provider">{{ provider.short }}</span>
            <span v-if="provider" class="dot">·</span>
            <span>{{ session.owner || "未知分享者" }}</span>
            <span class="dot">·</span>
            <span>{{ session.fileNum }} 项</span>
            <span class="dot">·</span>
            <span class="mono">{{ formatSize(session.totalSize) }}</span>
            <template v-if="expires">
              <span class="dot">·</span>
              <span>{{ formatDate(expires) }} 过期</span>
            </template>
          </div>
        </div>
        <div class="session-actions">
          <button
            v-if="session.canSave"
            class="btn btn--ghost"
            :disabled="saving || !rows.length"
            @click="save"
          >
            <Icon name="save" :size="14" :spin="saving" />
            <span>{{ selected.size ? `转存 ${selected.size} 项` : "全部转存" }}</span>
          </button>
          <button
            class="btn btn--solid"
            :disabled="downloading || !rows.length"
            @click="download"
          >
            <Icon name="download" :size="14" :spin="downloading" />
            <span>{{ selected.size ? `下载 ${selected.size} 项` : "全部下载" }}</span>
          </button>
        </div>
        </motion.div>
      </AnimatePresence>

      <div class="crumbs-wrap">
        <button class="icon-btn" :disabled="crumbs.length <= 1" title="返回上级" @click="goUp">
          <Icon name="chevron-left" :size="16" />
        </button>
        <nav class="crumbs">
          <template v-for="(crumb, index) in crumbs" :key="crumb.fid">
            <button
              class="crumb"
              :class="{ 'crumb--current': index === crumbs.length - 1 }"
              @click="goToCrumb(index)"
            >
              {{ crumb.name }}
            </button>
            <Icon
              v-if="index < crumbs.length - 1"
              name="chevron-right"
              :size="13"
              class="crumb-sep"
            />
          </template>
        </nav>
      </div>

      <div class="list-wrap">
        <div class="list-head">
          <button class="check" @click="toggleAll">
            <span class="box" :class="{ 'box--on': selected.size > 0 }">
              <Icon v-if="selected.size" name="check" :size="11" :stroke="2.4" />
            </span>
          </button>
          <span class="col">名称</span>
          <span class="col col--size">大小</span>
          <span class="col col--date">修改时间</span>
        </div>

        <div class="list-body" @scroll.passive="onScroll">
          <div v-if="!rows.length" class="state">
            <EmptyState icon="folder" title="这个文件夹是空的" description="分享内容可能还在上传中。" />
          </div>
          <div v-else class="rows">
            <motion.div
              v-for="(entry, i) in pagedRows"
              :key="entry.fid"
              class="row"
              :class="{ 'row--selected': selected.has(entry.fid) }"
              v-bind="rowEnter(i)"
              @click="toggle(entry)"
              @dblclick="enter(entry)"
            >
              <button class="check" @click.stop="toggle(entry)">
                <span class="box" :class="{ 'box--on': selected.has(entry.fid) }">
                  <Icon v-if="selected.has(entry.fid)" name="check" :size="11" :stroke="2.4" />
                </span>
              </button>
              <div class="cell cell--name">
                <FileIcon
                  :is-dir="entry.isDir"
                  :ext="entry.name.split('.').pop()?.toLowerCase() ?? ''"
                  :name="entry.name"
                  :size="30"
                />
                <div class="name-copy">
                  <span class="name truncate" :title="entry.name">{{ entry.name }}</span>
                  <span class="name-kind">
                    {{ entry.isDir ? "文件夹" : kindLabel(fileKind({ isDir: entry.isDir, ext: entry.name.split(".").pop() ?? "" })) }}
                  </span>
                </div>
              </div>
              <div class="cell cell--size mono">
                {{ entry.isDir ? "—" : formatSize(entry.size) }}
              </div>
              <div class="cell cell--date">{{ formatDate(entry.updatedAt) }}</div>
            </motion.div>

            <ChunkFooter :mounted="pagedRows.length" :rest="rest" noun="文件" @more="more()" />
          </div>
        </div>

        <footer class="list-foot">
          <span>{{ rows.length }} 项</span>
          <span class="dot">·</span>
          <span class="mono">{{ formatSize(totalSize) }}</span>
          <span v-if="selected.size" class="foot-selected">已选 {{ selected.size }} 项</span>
        </footer>
      </div>
    </template>

    <div v-else-if="!busy && !error" class="intro">
      <EmptyState
        icon="link"
        title="尚未打开分享"
        description="把浏览器里的分享链接复制过来，或直接点击系统里的分享链接。"
      >
        <button class="btn btn--ghost" @click="app.view = 'files'">
          <Icon name="folder" :size="14" />
          <span>返回我的文件</span>
        </button>
      </EmptyState>
    </div>
  </section>
</template>



<style scoped>
.share {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-width: 0;
  background: var(--bg);
  padding: 0 0 14px;
}
.head {
  display: flex;
  align-items: flex-end;
  justify-content: space-between;
  gap: 16px;
  padding: 22px 20px 14px;
}
.title {
  margin: 0;
  font-size: 19px;
  font-weight: 600;
  letter-spacing: -0.02em;
}
.sub {
  margin: 4px 0 0;
  font-size: 12.2px;
  color: var(--text-3);
}
.bar {
  display: flex;
  gap: 8px;
  padding: 0 20px 14px;
}
.link-box {
  position: relative;
  flex: 1;
  display: flex;
  align-items: center;
  height: 36px;
  padding: 0 30px 0 32px;
  border-radius: var(--radius-sm);
  background: var(--bg-sunken);
  box-shadow: inset 0 0 0 1px var(--line);
  transition: box-shadow var(--dur-2) var(--ease-out);
}
.link-box:focus-within {
  box-shadow: inset 0 0 0 1px var(--line-strong), 0 0 0 3px var(--bg-active);
}
.link-icon {
  position: absolute;
  left: 10px;
  color: var(--text-3);
}
.link-input {
  width: 100%;
  border: 0;
  background: none;
  outline: none;
  font-size: 12.8px;
  font-family: var(--font-mono);
}
.link-input::placeholder {
  color: var(--text-3);
  font-family: var(--font-sans);
}
.pass-input {
  width: 92px;
  height: 30px;
  padding: 0 9px;
  border: 0;
  box-shadow: inset 0 0 0 1px var(--line);
  border-radius: 7px;
  background: var(--bg-sunken);
  color: var(--text);
  font-size: 12.5px;
  font-family: var(--font-mono);
  outline: none;
}
.pass-input:focus {
  box-shadow: inset 0 0 0 1px var(--accent);
}
.pass-input::placeholder {
  color: var(--text-3);
  font-family: var(--font-sans);
}
.clear {
  position: absolute;
  right: 8px;
  display: grid;
  place-items: center;
  width: 18px;
  height: 18px;
  border-radius: 50%;
  color: var(--text-3);
  background: var(--bg-active);
}
.clear:hover {
  color: var(--text);
}
.alert {
  display: flex;
  align-items: center;
  gap: 8px;
  margin: 0 20px 12px;
  padding: 10px 12px;
  border-radius: var(--radius-sm);
  font-size: 12.2px;
  color: var(--text);
  background: var(--bg-sunken);
  box-shadow: inset 0 0 0 1px var(--line);
  /* Rest on an identity transform: an element whose final computed transform
     is `none` has its compositing layer destroyed at the end of the animation,
     and WebKitGTK repaints one stale frame — the visible "blink". */
  /* Animated by motion-v on the element itself (see the template). */
  transform: translateY(0);
}
.session {
  display: flex;
  align-items: center;
  gap: 12px;
  margin: 0 20px 12px;
  padding: 12px 14px;
  border-radius: var(--radius-md);
  border: 1px solid var(--line);
  background: var(--bg-sunken);
  /* Animated by motion-v on the element itself (see the template). */
  transform: translateY(0);
}
.session-copy {
  min-width: 0;
  flex: 1;
}
.session-title {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 13.5px;
  font-weight: 560;
  letter-spacing: -0.01em;
}
.session-logo {
  width: 15px;
  height: 15px;
  border-radius: 4px;
  object-fit: contain;
  flex: none;
}
.session-meta {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-top: 3px;
  font-size: 11.5px;
  color: var(--text-3);
}
.session-actions {
  display: flex;
  gap: 8px;
  flex: none;
}
.crumbs-wrap {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 0 20px 10px;
  min-width: 0;
}
.crumbs {
  display: flex;
  align-items: center;
  gap: 1px;
  min-width: 0;
  overflow: hidden;
}
.crumb {
  max-width: 200px;
  padding: 4px 7px;
  border-radius: 6px;
  font-size: 12.5px;
  color: var(--text-2);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  transition: background-color var(--dur-1) var(--ease-out), color var(--dur-1) var(--ease-out);
}
.crumb:hover {
  background: var(--bg-hover);
  color: var(--text);
}
.crumb--current {
  color: var(--text);
  font-weight: 560;
}
.crumb-sep {
  color: var(--text-3);
  flex: none;
  opacity: 0.7;
}
.list-wrap {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
  margin: 0 20px;
  border-radius: var(--radius-md);
  border: 1px solid var(--line);
  overflow: hidden;
}
.list-head {
  display: grid;
  grid-template-columns: 34px minmax(0, 1fr) 96px 148px;
  align-items: center;
  height: 32px;
  flex: none;
  padding: 0 10px;
  border-bottom: 1px solid var(--line);
  background: var(--bg-sunken);
  user-select: none;
}
:root[data-theme="dark"] .list-head {
  background: rgba(255, 255, 255, 0.025);
}
.col {
  font-size: 11px;
  font-weight: 560;
  letter-spacing: 0.04em;
  text-transform: uppercase;
  color: var(--text-3);
}
.col--size,
.col--date {
  text-align: right;
}
.list-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  overflow-x: hidden;
}
.state {
  display: grid;
  place-items: center;
  min-height: 100%;
}
.rows {
  padding: 5px;
}
.row {
  display: grid;
  grid-template-columns: 34px minmax(0, 1fr) 96px 148px;
  align-items: center;
  height: 48px;
  padding: 0 10px;
  border-radius: var(--radius-sm);
  cursor: default;
  user-select: none;
  transition: background-color var(--dur-1) var(--ease-out),
    box-shadow var(--dur-1) var(--ease-out);
}
.row:hover {
  background: var(--bg-hover);
}
.row--selected {
  background: var(--bg-active);
  box-shadow: inset 0 0 0 1px var(--line-strong);
}
.check {
  display: grid;
  place-items: center;
  width: 26px;
  height: 26px;
  border-radius: 6px;
  color: var(--text-2);
}
.box {
  display: grid;
  place-items: center;
  width: 15px;
  height: 15px;
  border-radius: 4px;
  box-shadow: inset 0 0 0 1px var(--line-strong);
  transition: background-color var(--dur-1) var(--ease-out),
    box-shadow var(--dur-1) var(--ease-out), color var(--dur-1) var(--ease-out);
}
.box--on {
  color: var(--on-fill);
  background: var(--fill);
  box-shadow: inset 0 0 0 1px var(--fill);
}
.cell {
  min-width: 0;
}
.cell--name {
  display: flex;
  align-items: center;
  gap: 10px;
}
.name-copy {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
}
.name {
  font-size: 13px;
  font-weight: 490;
}
.name-kind {
  font-size: 10.5px;
  color: var(--text-3);
}
.cell--size {
  text-align: right;
  color: var(--text-2);
  font-size: 11.5px;
}
.cell--date {
  text-align: right;
  color: var(--text-3);
  font-size: 11.8px;
}
.list-foot {
  display: flex;
  align-items: center;
  gap: 7px;
  height: 30px;
  flex: none;
  padding: 0 14px;
  border-top: 1px solid var(--line);
  font-size: 11px;
  color: var(--text-3);
  background: var(--bg-sunken);
  user-select: none;
}
:root[data-theme="dark"] .list-foot {
  background: rgba(255, 255, 255, 0.025);
}
.foot-selected {
  margin-left: auto;
  color: var(--text-2);
}
.dot {
  opacity: 0.5;
}
.intro {
  flex: 1;
  display: grid;
  place-items: center;
}
:where(.row) {
  /* Rows are `motion.div`s — enter and layout animations come from motion-v
     (`rowHidden`/`rowShown` in src/lib/motion.ts). Resting identity transform
     keeps the compositing layer from being torn down after the animation. */
  transform: translateY(0);
}
</style>
