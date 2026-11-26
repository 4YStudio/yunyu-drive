<script setup lang="ts">
import {
  computed,
  nextTick,
  onActivated,
  onBeforeUnmount,
  onDeactivated,
  onMounted,
  ref,
  watch,
} from "vue";
import { onFileDrop, pickFiles } from "@/lib/host";

import { AnimatePresence, motion } from "motion-v";
import { rowEnter } from "@/lib/motion";
import Icon from "@/components/Icon.vue";
import FileIcon from "@/components/FileIcon.vue";
import FileThumb from "@/components/FileThumb.vue";
import EmptyState from "@/components/EmptyState.vue";
import ContextMenu from "@/components/ContextMenu.vue";
import type { MenuItem } from "@/lib/menu";
import Modal from "@/components/Modal.vue";
import { type TrafficWarning, trafficWarning } from "@/lib/traffic";
import Segmented from "@/components/Segmented.vue";

import { api } from "@/lib/backend";
import { copyText } from "@/lib/clipboard";
import { storageKey } from "@/lib/brand";
import type { DirListing, EntryDto, PathNode, SearchHit, ShareCreated } from "@/lib/types";
import { errText, formatDate, formatFullDate, formatSize, fileKind, kindLabel } from "@/lib/format";
import {
  app,
  activeProviderState,
  goLoginFor,
  loadProviderStates,
  loadQuota,
  refreshQuota,
  rootFid,
  saveSettings,
  toast,
} from "@/store/app";
import { PROVIDERS, type ProviderId } from "@/lib/types";
import DriveLoader from "@/components/DriveLoader.vue";

type SortKey = "name" | "size" | "updated";

/** Display name of the drive currently shown. */
const driveName = computed(
  () => PROVIDERS.find((p) => p.id === app.activeProvider)?.name ?? "当前网盘",
);

/** False when the active drive has no usable credential yet. */
const driveLoggedIn = computed(() => activeProviderState.value?.authenticated !== false);

/** Sends the user to the login screen for the active drive, skipping the picker. */
function goLogin() {
  goLoginFor(app.activeProvider);
}

/** Root of the active drive; the id is qualified (`"quark:0"`). */
function rootNode(): PathNode {
  return { fid: rootFid.value, name: "全部文件" };
}

const listing = ref<DirListing | null>(null);
const crumbs = ref<PathNode[]>([rootNode()]);
const currentFid = computed(() => crumbs.value[crumbs.value.length - 1].fid);
const loading = ref(false);
const error = ref<string | null>(null);

const query = ref("");
const searching = ref(false);
const hits = ref<SearchHit[]>([]);

const selected = ref<Set<string>>(new Set());
const anchor = ref<string | null>(null);

/** Sort survives restarts: re-picking "name, ascending" on every launch is noise. */
function initialSort(): { key: SortKey; asc: boolean } {
  const stored = localStorage.getItem(storageKey("sort"));
  if (stored === "size" || stored === "updated" || stored === "name") {
    return { key: stored, asc: localStorage.getItem(storageKey("sortAsc")) !== "0" };
  }
  return { key: "name", asc: true };
}
const initialSortState = initialSort();
const sortKey = ref<SortKey>(initialSortState.key);
const sortAsc = ref(initialSortState.asc);
function initialDensity(): "comfortable" | "compact" {
  const stored = localStorage.getItem(storageKey("density"));
  return stored === "compact" ? "compact" : "comfortable";
}
const density = ref<"comfortable" | "compact">(initialDensity());
function initialViewMode(): "list" | "grid" {
  return localStorage.getItem(storageKey("view")) === "grid" ? "grid" : "list";
}
const viewMode = ref<"list" | "grid">(initialViewMode());

const menu = ref({ open: false, x: 0, y: 0, target: null as EntryDto | null });
const prompt = ref<null | { mode: "mkdir" | "rename"; value: string; target?: EntryDto }>(null);
const confirmDelete = ref<EntryDto[] | null>(null);
const busy = ref(false);
const downloading = ref(false);
/** Pending download: ask where to put it instead of dropping into the default. */
const downloadPrompt = ref<null | {
  items: { fid: string; name: string; size: number; isDir: boolean }[];
  dir: string;
  remember: boolean;
}>(null);
const dragOver = ref(false);
const uploading = ref(false);
/** Move / copy dialog: which entries and into which folder. */
const relocate = ref<null | {
  mode: "move" | "copy";
  entries: EntryDto[];
  /** Folder the browser is showing; `null` while the root is loading. */
  fid: string;
  path: PathNode[];
  dirs: EntryDto[];
  loading: boolean;
  busy: boolean;
  /** Drive the picker is browsing; starts on the active one. */
  provider: ProviderId;
}>(null);

/** Drives the picker can switch to: logged in, and not the source drive. */
const relocateTargets = computed(() =>
  app.providers.filter((state) => state.authenticated),
);

/** True when the picker sits on a drive other than the files' own. */
const relocateForeign = computed(
  () => relocate.value !== null && relocate.value.provider !== app.activeProvider,
);

function relocateRoot(provider: ProviderId): string {
  return `${provider}:0`;
}

/** Switches the picker to another drive and starts at its root. */
async function pickDrive(provider: ProviderId) {
  const state = relocate.value;
  if (!state || state.provider === provider) return;
  const node: PathNode = {
    fid: relocateRoot(provider),
    name: PROVIDERS.find((p) => p.id === provider)?.name ?? "全部文件",
  };
  state.provider = provider;
  await loadRelocate(node.fid, [node]);
}

// ---------------------------------------------------------------- loading ---

/** The drive the on-screen listing belongs to; `null` before the first load. */
let loadedProvider: string | null = null;

/**
 * Guards against out-of-order responses.
 *
 * At startup this view mounts before `bootstrap()` has restored the remembered
 * drive, so it fires a request for the default (`quark:0`) and then one for the
 * restored drive. Whichever answered last won — usually the default, which is
 * why a 天翼云盘 session opened on 夸克's files until the user toggled drives.
 * Only the newest call may touch `listing`.
 */
let loadSeq = 0;

/** The request currently on the wire, so a switch that trips two watchers at
 *  once only fetches once. See the note at the top of `load`. */
let pending: { key: string; seq: number } | null = null;

async function load(fid: string, crumbs2?: PathNode[]) {
  loadedProvider = app.activeProvider;
  // An unlogged drive cannot be listed: asking anyway produces a "登录已过期"
  // style error that reads as a bug. Show the sign-in prompt instead.
  if (activeProviderState.value?.authenticated === false) {
    listing.value = null;
    error.value = null;
    loading.value = false;
    return;
  }
  // A qualified id from another drive can never be listed on this one. This
  // happens after logging in on a different drive, or when the remembered
  // directory came from the previous session's drive (the "123 显示夸克文件"
  // bug): fall back to the active drive's root instead of showing stale rows.
  if (!fid.startsWith(`${app.activeProvider}:`)) {
    fid = rootFid.value;
    crumbs.value = [rootNode()];
    crumbs2 = crumbs.value;
    listing.value = null;
  }
  // Same directory => keep the rows on screen while refetching, so refresh,
  // rename and delete never flash the loading spinner over existing content.
  const sameDir = listing.value?.pdirFid === fid;
  if (!sameDir) listing.value = null;
  // Switching drives trips two watchers at once — `filesToken` (bumped by
  // `setActiveProvider`) and `activeProvider` itself — and both used to issue a
  // request. The sequence guard threw the first answer away, but the two calls
  // also cleared `listing` twice, so the rows unmounted, remounted and
  // staggered in again: exactly what reads as "loaded twice". A request already
  // in flight for this exact target is the answer we would get anyway, so let
  // it finish and drop the duplicate.
  const key = `${app.activeProvider}|${fid}`;
  if (pending?.key === key) return;
  // Bump the sequence *after* the dedupe check: incrementing first would make
  // the request already in flight look stale, so its answer would be dropped
  // and its `finally` would refuse to clear `loading` — an endless spinner.
  const seq = ++loadSeq;
  pending = { key, seq };
  loading.value = true;
  error.value = null;
  try {
    const data = await api.listDir(fid);
    // A newer load (drive switch, navigation) owns the view now; drop this one.
    if (seq !== loadSeq) return;
    listing.value = data;
    selected.value = new Set();
    anchor.value = null;
    if (crumbs2) crumbs.value = crumbs2;
    else if (fid === rootFid.value) crumbs.value = [rootNode()];
    const remembered = app.settings?.lastDirFid ?? rootFid.value;
    if (fid !== remembered) {
      persistDir(fid);
    }
  } catch (err) {
    if (seq !== loadSeq) return;
    error.value = errText(err);
    listing.value = null;
  } finally {
    if (seq === loadSeq) loading.value = false;
    if (pending?.seq === seq) pending = null;
  }
}

let persistTimer: number | undefined;
function persistDir(fid: string) {
  window.clearTimeout(persistTimer);
  persistTimer = window.setTimeout(async () => {
    if (!app.settings) return;
    try {
      await api.settingsSet({ ...app.settings, lastDirFid: fid });
    } catch {
      /* non-critical */
    }
  }, 800);
}

async function enter(entry: EntryDto) {
  const next = [...crumbs.value, { fid: entry.fid, name: entry.name }];
  await load(entry.fid, next);
}

async function goToCrumb(index: number) {
  const next = crumbs.value.slice(0, index + 1);
  if (next.length === 0) next.push(rootNode());
  await load(next[next.length - 1].fid, next);
}

async function goUp() {
  if (crumbs.value.length <= 1) return;
  await goToCrumb(crumbs.value.length - 2);
}

async function refresh() {
  if (query.value.trim()) {
    await runSearch();
    return;
  }
  await load(currentFid.value);
}

// ----------------------------------------------------------------- search ---

let searchTimer: number | undefined;
watch(query, (value) => {
  window.clearTimeout(searchTimer);
  const trimmed = value.trim();
  if (!trimmed) {
    hits.value = [];
    searching.value = false;
    return;
  }
  searching.value = true;
  searchTimer = window.setTimeout(runSearch, 320);
});

async function runSearch() {
  const keyword = query.value.trim();
  if (!keyword) return;
  searching.value = true;
  try {
    hits.value = await api.search(currentFid.value, keyword, 120);
  } catch (err) {
    toast(errText(err), "error");
    hits.value = [];
  } finally {
    searching.value = false;
  }
}

// ------------------------------------------------------------------- rows ---

const rows = computed(() => {
  if (query.value.trim()) {
    return hits.value.map((hit) => ({ entry: hit.entry, path: hit.path }));
  }
  const entries = (listing.value?.entries ?? []).filter((e) => {
    // Hidden entries are dot-files on the Quark side too.
    if (app.settings?.showHidden) return true;
    return !e.name.startsWith(".");
  });
  const list = [...entries];
  const dir = sortAsc.value ? 1 : -1;
  list.sort((a, b) => {
    if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
    switch (sortKey.value) {
      case "size":
        return (a.size - b.size) * dir;
      case "updated":
        return (a.updatedAt - b.updatedAt) * dir;
      default:
        return a.name.localeCompare(b.name, "zh-CN", { numeric: true }) * dir;
    }
  });
  return list.map((entry) => ({ entry, path: [] as PathNode[] }));
});

/**
 * Rendering budget.
 *
 * A folder with a few thousand entries used to mount every row at once, which
 * stalls WebKitGTK for seconds. Only the first `shown` rows are mounted; the
 * list body grows the window as you scroll near the bottom (see `onBodyScroll`).
 */
// `VITE_LIST_CHUNK` only exists so the mock build can exercise the paging
// behaviour on a 12-row fixture.
const CHUNK = Number(import.meta.env.VITE_LIST_CHUNK) || 200;
const shown = ref(CHUNK);
const bodyRef = ref<HTMLElement | null>(null);
const visibleRows = computed(() => rows.value.slice(0, shown.value));

function onBodyScroll() {
  const el = bodyRef.value;
  if (!el) return;
  if (shown.value >= rows.value.length) return;
  if (el.scrollHeight - el.scrollTop - el.clientHeight > 600) return;
  shown.value = Math.min(rows.value.length, shown.value + CHUNK);
}

// A new listing (other folder, other drive, other search) starts from the top.
watch(
  () => [rows.value.length, currentFid.value, query.value].join("|"),
  () => {
    shown.value = CHUNK;
    if (bodyRef.value) bodyRef.value.scrollTop = 0;
  },
);

const selectedEntries = computed(() =>
  rows.value.filter((r) => selected.value.has(r.entry.fid)).map((r) => r.entry),
);

const totalSize = computed(() =>
  rows.value.reduce((sum, r) => sum + (r.entry.isDir ? 0 : r.entry.size), 0),
);

const folderCount = computed(() => rows.value.filter((r) => r.entry.isDir).length);

function toggleSort(key: SortKey) {
  if (sortKey.value === key) sortAsc.value = !sortAsc.value;
  else {
    sortKey.value = key;
    sortAsc.value = key === "name";
  }
  localStorage.setItem(storageKey("sort"), sortKey.value);
  localStorage.setItem(storageKey("sortAsc"), sortAsc.value ? "1" : "0");
}

function setDensity(value: string) {
  density.value = value as "comfortable" | "compact";
  localStorage.setItem(storageKey("density"), value);
}

function setViewMode(mode: "list" | "grid") {
  if (viewMode.value === mode) return;
  viewMode.value = mode;
  localStorage.setItem(storageKey("view"), mode);
}

// --------------------------------------------------------------- selection ---

function isSelected(fid: string) {
  return selected.value.has(fid);
}

function selectOne(entry: EntryDto) {
  selected.value = new Set([entry.fid]);
  anchor.value = entry.fid;
}

function onRowClick(event: MouseEvent, entry: EntryDto) {
  if (event.shiftKey && anchor.value) {
    const ids = rows.value.map((r) => r.entry.fid);
    const from = ids.indexOf(anchor.value);
    const to = ids.indexOf(entry.fid);
    if (from >= 0 && to >= 0) {
      const [lo, hi] = from < to ? [from, to] : [to, from];
      const next = new Set(event.ctrlKey || event.metaKey ? selected.value : []);
      for (let i = lo; i <= hi; i += 1) next.add(ids[i]);
      selected.value = next;
      return;
    }
  }
  if (event.ctrlKey || event.metaKey) {
    const next = new Set(selected.value);
    if (next.has(entry.fid)) next.delete(entry.fid);
    else next.add(entry.fid);
    selected.value = next;
    anchor.value = entry.fid;
    return;
  }
  selectOne(entry);
}

function selectAll() {
  selected.value = new Set(rows.value.map((r) => r.entry.fid));
}

/**
 * Arrow-key navigation.
 *
 * `anchor` is the cursor: it survives Ctrl-click multi-selection, so holding
 * Shift extends the range from wherever you last landed. The row has to be
 * scrolled into view by hand because the browser does not know about it.
 */
function moveSelection(step: number, extend: boolean) {
  const list = rows.value;
  if (!list.length) return;
  const ids = list.map((r) => r.entry.fid);
  const from = anchor.value ? ids.indexOf(anchor.value) : -1;
  const last = [...selected.value].map((id) => ids.indexOf(id)).filter((i) => i >= 0);
  const base = from >= 0 ? from : last.length ? Math.max(...last) : -1;
  const next = Math.min(list.length - 1, Math.max(0, base + step));
  const entry = list[next].entry;
  if (extend && anchor.value) {
    const lo = Math.min(base, next);
    const hi = Math.max(base, next);
    const set = new Set(selected.value);
    for (let i = lo; i <= hi; i += 1) set.add(ids[i]);
    selected.value = set;
  } else {
    selected.value = new Set([entry.fid]);
  }
  anchor.value = entry.fid;
  // Paging only mounts the first `shown` rows, so the cursor can outrun it.
  if (next >= shown.value) shown.value = Math.min(list.length, next + 1);
  void nextTick(() => {
    const el = bodyRef.value?.querySelectorAll<HTMLElement>(".row, .card")[next];
    el?.scrollIntoView({ block: "nearest" });
  });
}

// ----------------------------------------------------------------- actions ---

function openMenu(event: MouseEvent, entry: EntryDto | null) {
  if (entry && !isSelected(entry.fid)) selectOne(entry);
  if (!entry) selected.value = new Set();
  menu.value = { open: true, x: event.clientX, y: event.clientY, target: entry };
}

/** What the drive on screen can do; drives differ, so the menu follows. */
/**
 * Delete goes to the drive's recycle bin where there is one, so the wording has
 * to say "可恢复" instead of "不可撤销" — 百度 and 139 delete for good.
 */
const deleteHint = computed(() => {
  const count = confirmDelete.value?.length ?? 0;
  if (caps.value.recycle) {
    return `将把 ${count} 项移入${driveName.value}的回收站，可以再恢复。`;
  }
  return `将从云端永久删除 ${count} 项，此操作不可撤销。`;
});

const caps = computed(
  () =>
    app.providers.find((p) => p.provider === app.activeProvider)?.capabilities ?? {
      shareOpen: false,
      shareCreate: false,
      move: true,
      copy: true,
      search: true,
      rename: true,
      folders: true,
      recycle: false,
    },
);

const menuItems = computed<MenuItem[]>(() => {
  const target = menu.value.target;
  const many = selectedEntries.value.length > 1;
  if (!target) {
    return [
      { id: "mkdir", label: "新建文件夹", icon: "folder-plus" },
      { id: "upload", label: "上传文件", icon: "upload" },
      { id: "upload-dir", label: "上传文件夹", icon: "folder" },
      { id: "refresh", label: "刷新", icon: "refresh", separatorBefore: true },
    ];
  }
  const items: MenuItem[] = [];
  if (target.isDir) {
    items.push({ id: "open", label: "打开", icon: "folder" });
    items.push({ id: "download", label: many ? "下载所选" : "下载文件夹", icon: "download" });
  } else {
    items.push({ id: "download", label: many ? "下载所选" : "下载", icon: "download" });
  }
  items.push({
    id: "rename",
    label: "重命名",
    icon: "rename",
    disabled: many || !caps.value.rename,
    separatorBefore: true,
  });
  // A drive that cannot move or copy says so up front instead of after the
  // folder picker. 123 云盘's Android protocol has no such endpoint today.
  items.push({
    id: "move",
    label: many ? `移动 ${selectedEntries.value.length} 项` : "移动到…",
    icon: "move",
    disabled: !caps.value.move,
  });
  items.push({
    id: "copy",
    label: many ? `复制 ${selectedEntries.value.length} 项` : "复制到…",
    icon: "copy",
    disabled: !caps.value.copy,
  });
  if (!target.isDir) {
    items.push({ id: "copy-name", label: "复制文件名", icon: "copy" });
  }
  items.push({
    id: "share",
    label: many ? `分享 ${selectedEntries.value.length} 项` : "创建分享链接",
    icon: "share",
    disabled: !caps.value.shareCreate,
  });
  items.push({ id: "delete", label: many ? `删除 ${selectedEntries.value.length} 项` : "删除", icon: "trash", danger: true, separatorBefore: true });
  return items;
});

async function onMenuSelect(id: string) {
  const target = menu.value.target;
  switch (id) {
    case "mkdir":
      prompt.value = { mode: "mkdir", value: "" };
      break;
    case "upload":
      await pickUpload();
      break;
    case "upload-dir":
      await pickUploadDir();
      break;
    case "refresh":
      await refresh();
      break;
    case "open":
      if (target) await enter(target);
      break;
    case "download":
      await downloadSelected();
      break;
    case "rename":
      if (target) prompt.value = { mode: "rename", value: target.name, target };
      break;
    case "move":
      if (target) await openRelocate("move");
      break;
    case "copy":
      if (target) await openRelocate("copy");
      break;
    case "copy-name":
      if (target) {
        const ok = await copyText(target.name);
        toast(ok ? "已复制文件名" : "复制失败", ok ? "success" : "error");
      }
      break;
    case "share":
      openShare();
      break;
    case "delete":
      requestDelete(selectedEntries.value);
      break;
  }
}

/** Opens the folder picker for a move or copy of the current selection. */
/** Same allowance check, for a cross-drive copy that has to download first. */
const relocateTraffic = ref<TrafficWarning | null>(null);

async function openRelocate(mode: "move" | "copy") {
  const entries = selectedEntries.value;
  if (entries.length === 0) return;
  relocateTraffic.value = null;
  relocate.value = {
    mode,
    entries,
    fid: rootFid.value,
    path: [rootNode()],
    dirs: [],
    loading: true,
    busy: false,
    provider: app.activeProvider,
  };
  await loadRelocate(rootFid.value, [rootNode()]);
  if (mode === "copy") {
    const bytes = entries.reduce((sum, entry) => sum + (entry.isDir ? 0 : entry.size), 0);
    relocateTraffic.value = await trafficWarning(
      app.activeProvider,
      bytes,
      entries.some((entry) => entry.isDir),
    );
  }
}

async function loadRelocate(fid: string, path: PathNode[]) {
  const state = relocate.value;
  if (!state) return;
  state.loading = true;
  try {
    const data = await api.listDir(fid);
    state.fid = fid;
    state.path = path;
    // Only folders can be a destination.
    state.dirs = data.entries.filter((entry) => entry.isDir);
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    state.loading = false;
  }
}

async function relocateInto(entry: EntryDto) {
  const state = relocate.value;
  if (!state) return;
  await loadRelocate(entry.fid, [...state.path, { fid: entry.fid, name: entry.name }]);
}

async function relocateUp() {
  const state = relocate.value;
  if (!state || state.path.length <= 1) return;
  const parent = state.path[state.path.length - 2];
  await loadRelocate(parent.fid, state.path.slice(0, -1));
}

/** Label for the confirm button, including the cross-drive case. */
const relocateLabel = computed(() => {
  const state = relocate.value;
  if (!state) return "";
  if (state.provider !== app.activeProvider) {
    return state.mode === "move" ? "移动到这里（跨网盘）" : "复制到这里（跨网盘）";
  }
  return state.mode === "move" ? "移动到这里" : "复制到这里";
});

async function confirmRelocate() {
  const state = relocate.value;
  if (!state) return;
  // Moving entries into the folder they already live in does nothing; the
  // source folder is the current one, so compare against it.
  const foreign = state.provider !== app.activeProvider;
  if (!foreign && state.mode === "move" && state.fid === currentFid.value) {
    toast("已经是当前文件夹", "info");
    return;
  }
  state.busy = true;
  try {
    const count = await api.transferEntries(
      state.entries.map((entry) => entry.fid),
      state.fid,
      state.mode === "copy",
    );
    if (foreign) {
      // The drive cannot do this server-side: the app streams every file down
      // and back up, which shows up in the transfer list.
      toast(`已加入传输列表：${count} 个文件将下载后再上传`, "success");
    } else {
      toast(state.mode === "move" ? `已移动 ${count} 项` : `已复制 ${count} 项`, "success");
    }
    relocate.value = null;
    await load(currentFid.value);
    // 复制/移动都改容量（跨盘的还是先下载再上传），缓存里的读数不能留。
    refreshQuota();
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    state.busy = false;
  }
}

async function submitPrompt() {
  const state = prompt.value;
  if (!state) return;
  const name = state.value.trim();
  if (!name) return;
  busy.value = true;
  try {
    if (state.mode === "mkdir") {
      await api.makeDir(currentFid.value, name);
      toast("文件夹已创建", "success");
    } else if (state.target) {
      await api.renameEntry(state.target.fid, name);
      toast("已重命名", "success");
    }
    prompt.value = null;
    await load(currentFid.value);
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    busy.value = false;
  }
}

function requestDelete(entries: EntryDto[]) {
  if (entries.length === 0) return;
  if (app.settings?.confirmDelete === false) {
    void doDelete(entries);
    return;
  }
  confirmDelete.value = entries;
}

async function doDelete(entries: EntryDto[]) {
  busy.value = true;
  try {
    await api.deleteEntries(entries.map((e) => e.fid));
    toast(`已删除 ${entries.length} 项`, "success");
    confirmDelete.value = null;
    await load(currentFid.value);
    // Deleted bytes free capacity; refetch instead of leaving a stale bar.
    void loadQuota(app.activeProvider, true);
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    busy.value = false;
  }
}

function defaultDownloadDir() {
  return app.settings?.downloadDir ?? "";
}

/**
 * A folder has no size in the listing, so summing the selection showed "0 B"
 * for a folder of 30 GB. Count them separately and say so, instead of printing
 * a number that is simply wrong.
 */
const downloadSummary = computed(() => {
  const items = downloadPrompt.value?.items ?? [];
  const dirs = items.filter((item) => item.isDir).length;
  const files = items.length - dirs;
  const bytes = items.reduce((sum, item) => sum + (item.isDir ? 0 : item.size), 0);
  const parts: string[] = [];
  if (files > 0) parts.push(`${files} 个文件`);
  if (dirs > 0) parts.push(`${dirs} 个文件夹`);
  const head = parts.length ? parts.join("、") : `${items.length} 项`;
  if (dirs === 0) return `${head}，共 ${formatSize(bytes)}`;
  return `${head}${bytes > 0 ? `，另有 ${formatSize(bytes)} 文件` : ""}（文件夹的实际大小在读取后显示）`;
});

/**
 * Why this download may not fit the drive's monthly allowance, if it may not.
 *
 * 123 lets a transfer start and then cuts it off when the allowance runs out,
 * which leaves an incomplete file — so say it before the queue starts, not
 * after the failure.
 */
const downloadTraffic = ref<TrafficWarning | null>(null);

async function askDownload(items: { fid: string; name: string; size: number; isDir: boolean }[]) {
  if (items.length === 0) return;
  downloadPrompt.value = { items, dir: defaultDownloadDir(), remember: false };
  downloadTraffic.value = null;
  const bytes = items.reduce((sum, item) => sum + (item.isDir ? 0 : item.size), 0);
  downloadTraffic.value = await trafficWarning(
    app.activeProvider,
    bytes,
    items.some((item) => item.isDir),
  );
}

async function chooseDownloadDir() {
  const picked = await pickFiles({
    multiple: false,
    directory: true,
    title: "选择下载到哪个文件夹",
  });
  if (picked.length && downloadPrompt.value) downloadPrompt.value.dir = picked[0];
}

async function confirmDownload() {
  const prompt2 = downloadPrompt.value;
  if (!prompt2) return;
  downloading.value = true;
  try {
    if (prompt2.remember && prompt2.dir && app.settings) {
      await saveSettings({ ...app.settings, downloadDir: prompt2.dir });
    }
    const tasks = await api.downloadEntries(prompt2.items, prompt2.dir);
    toast(`已加入下载队列 ${tasks.length} 项`, "success");
    downloadPrompt.value = null;
    app.view = "transfers";
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    downloading.value = false;
  }
}

async function downloadSelected() {
  const picked = selectedEntries.value;
  if (picked.length === 0) {
    toast("请先选择要下载的内容", "error");
    return;
  }
  askDownload(
    picked.map((f) => ({ fid: f.fid, name: f.name, size: f.size, isDir: f.isDir })),
  );
}

function downloadOne(entry: EntryDto) {
  askDownload([{ fid: entry.fid, name: entry.name, size: entry.size, isDir: entry.isDir }]);
}

// ------------------------------------------------------------- create share ---

/**
 * Share dialog state.
 *
 * `days` is a string because `Segmented` works on strings; `result` stays set
 * after a successful create so the user can copy the link without the dialog
 * closing on them.
 */
const shareDialog = ref<null | {
  entries: EntryDto[];
  title: string;
  days: string;
  passcode: string;
  busy: boolean;
  result: ShareCreated | null;
}>(null);

/** 百度 rejects an empty code, so it is the one drive that cannot go open. */
const shareNeedsCode = computed(() => app.activeProvider === "baidu");

/** 139 accepts the field but mints its own code; the value typed here is not used. */
const shareCodeAuto = computed(() => app.activeProvider === "yun139");

/** 天翼云盘 ignores the field too, and always hands out a 4-character code. */
const shareCodeForced = computed(
  () => app.activeProvider === "cloud189" || app.activeProvider === "pan115",
);

/** 天翼云盘 has no working multi-file endpoint, so it shares one entry at a time. */
const shareSingleOnly = computed(() => app.activeProvider === "cloud189");

const shareCodeHint = computed(() => {
  if (shareNeedsCode.value) return `${driveName.value}必须设置 4 位提取码`;
  if (shareCodeAuto.value) return "填 4 位则由 139 生成提取码（不能自定义），留空则无提取码";
  if (shareCodeForced.value) return `${driveName.value}总是自己生成 4 位访问码，这里填什么都不会生效`;
  return "留空则生成不需要提取码的链接";
});

const shareDays = computed(() => Number(shareDialog.value?.days ?? 7));

function openShare() {
  let entries = selectedEntries.value;
  if (entries.length === 0) return;
  if (shareSingleOnly.value && entries.length > 1) {
    toast(`${driveName.value}一次只能分享一个文件或文件夹，已选中第一项`, "info");
    entries = entries.slice(0, 1);
  }
  shareDialog.value = {
    entries,
    title:
      entries.length === 1
        ? entries[0].name.replace(/\.[^.]+$/, "")
        : `${driveName.value}分享 ${entries.length} 项`,
    days: "7",
    passcode: "",
    busy: false,
    result: null,
  };
}

async function submitShare() {
  const dialog = shareDialog.value;
  if (!dialog || dialog.busy) return;
  // 天翼云盘 mints its own 4-character code and ignores anything sent here.
  const code = shareCodeForced.value ? "" : dialog.passcode.trim();
  if (code && code.length !== 4) {
    toast("提取码需要 4 位，留空则由网盘决定", "error");
    return;
  }
  dialog.busy = true;
  try {
    dialog.result = await api.shareCreate(
      app.activeProvider,
      dialog.entries.map((entry) => entry.fid),
      dialog.title.trim() || undefined,
      Number(dialog.days),
      code || undefined,
    );
    toast("已创建分享链接", "success");
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    dialog.busy = false;
  }
}

/** The full line people paste into a chat: link plus code when there is one. */
const shareLine = computed(() => {
  const result = shareDialog.value?.result;
  if (!result) return "";
  return result.passcode ? `${result.url} 提取码：${result.passcode}` : result.url;
});

async function copyShareLink() {
  if (!shareLine.value) return;
  const ok = await copyText(shareLine.value);
  toast(ok ? "已复制分享链接" : "复制失败，请手动选中链接", ok ? "success" : "error");
}

/** Revokes the share the dialog just created. */
async function revokeShare() {
  const dialog = shareDialog.value;
  if (!dialog?.result || dialog.busy) return;
  dialog.busy = true;
  try {
    await api.shareRevoke(
      app.activeProvider,
      dialog.result.shareId,
      dialog.entries.map((entry) => entry.fid),
    );
    toast("已取消分享", "success");
    shareDialog.value = null;
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    if (shareDialog.value) shareDialog.value.busy = false;
  }
}

async function pickUpload() {
  const paths = await pickFiles({ multiple: true, title: "选择要上传的文件" });
  await upload(paths);
}

async function pickUploadDir() {
  const paths = await pickFiles({
    directory: true,
    multiple: false,
    title: "选择要上传的文件夹",
  });
  await upload(paths);
}

async function upload(paths: string[]) {
  if (paths.length === 0) return;
  uploading.value = true;
  try {
    const tasks = await api.uploadFiles(paths, currentFid.value);
    toast(`已加入上传队列 ${tasks.length} 项`, "success");
    await load(currentFid.value);
    void loadQuota(app.activeProvider, true);
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    uploading.value = false;
  }
}

// -------------------------------------------------------------- drag & drop ---

let unlistenDrop: (() => void) | null = null;

onMounted(async () => {
  if (app.settings?.listDensity === "compact" || app.settings?.listDensity === "comfortable") {
    density.value = app.settings.listDensity;
  }
  // A remembered id may belong to another drive (or predate the multi-drive
  // refactor), so fall back to the active drive's root in that case.
  const remembered = app.settings?.lastDirFid ?? rootFid.value;
  const usable = remembered.startsWith(`${app.activeProvider}:`) ? remembered : rootFid.value;
  await load(usable, usable === rootFid.value ? [rootNode()] : undefined);
  if (usable !== rootFid.value) {
    try {
      const path = await api.resolvePath(usable);
      if (path.length) crumbs.value = [rootNode(), ...path];
    } catch {
      /* stay at root */
    }
  }

  unlistenDrop = await onFileDrop((payload) => {
    if (payload.type === "over" || payload.type === "enter") {
      dragOver.value = true;
    } else if (payload.type === "drop") {
      dragOver.value = false;
      if (payload.paths.length) void upload(payload.paths);
    } else {
      dragOver.value = false;
    }
  });
});

// The view stays alive between sidebar switches, so keyboard handling follows
// activation rather than mount. The list itself also refreshes on activation:
// signing in or saving a share elsewhere bumps `filesToken` while this view is
// parked, and a stale (or empty) listing was the m03508 complaint.
let seenToken = app.filesToken;
let activated = false;
/**
 * Set when the drive changes while this view is parked.
 *
 * The listing is cleared in that case, so `loadedProvider` alone cannot tell
 * "nothing loaded yet" from "loaded for a drive that is no longer active" —
 * and the screen came back showing the previous drive's rows.
 */
let driveChangedWhileParked = false;

function syncIfStale() {
  // Switching to a drive (or finishing its login) does not always bump the
  // refresh token — `setActiveProvider` early-returns when the drive was
  // already selected — so compare the listing's own drive too. Without this the
  // screen kept showing the previous drive's rows, which read as "logged into
  // 123 云盘 but the file list is empty".
  const staleDrive =
    driveChangedWhileParked || (loadedProvider !== null && loadedProvider !== app.activeProvider);
  if (seenToken === app.filesToken && !staleDrive) return;
  seenToken = app.filesToken;
  driveChangedWhileParked = false;
  if (staleDrive) {
    crumbs.value = [rootNode()];
    query.value = "";
    hits.value = [];
    selected.value = new Set();
    void load(rootFid.value, crumbs.value);
    return;
  }
  void refresh();
}

watch(
  () => app.filesToken,
  () => {
    if (activated) syncIfStale();
  },
);

// Switching drives must show the NEW drive's files. Without this the listing
// just refreshed the previous drive's directory (the "123 显示夸克文件" bug).
watch(
  () => app.activeProvider,
  async (next, prev) => {
    if (next === prev) return;
    // While parked, `syncIfStale` handles it on the next activation.
    if (!activated) {
      loadedProvider = null;
      driveChangedWhileParked = true;
      return;
    }
    // Reset the view state: crumbs, selection, and the search field belong to
    // the previous drive.
    selected.value = new Set();
    anchor.value = null;
    query.value = "";
    hits.value = [];
    crumbs.value = [{ fid: rootFid.value, name: "全部文件" }];
    await load(rootFid.value, crumbs.value);
  },
);

onActivated(() => {
  activated = true;
  window.addEventListener("keydown", onKey);
  syncIfStale();
});
onDeactivated(() => {
  activated = false;
  window.removeEventListener("keydown", onKey);
});

onBeforeUnmount(() => {
  unlistenDrop?.();
  window.removeEventListener("keydown", onKey);
  window.clearTimeout(searchTimer);
  window.clearTimeout(persistTimer);
});

function onKey(event: KeyboardEvent) {
  const target = event.target as HTMLElement | null;
  const typing =
    target &&
    (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable);
  if (typing) return;
  if (prompt.value || confirmDelete.value || menu.value.open) return;

  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "a") {
    event.preventDefault();
    selectAll();
  } else if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "r") {
    event.preventDefault();
    void refresh();
  } else if (event.key === "Delete") {
    event.preventDefault();
    requestDelete(selectedEntries.value);
  } else if (event.key === "F2" && selectedEntries.value.length === 1) {
    event.preventDefault();
    const target2 = selectedEntries.value[0];
    prompt.value = { mode: "rename", value: target2.name, target: target2 };
  } else if (event.key === "Escape") {
    selected.value = new Set();
  } else if (event.key === "Backspace") {
    event.preventDefault();
    void goUp();
  } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    moveSelection(event.key === "ArrowDown" ? 1 : -1, event.shiftKey);
  } else if (event.key === "Enter" && selectedEntries.value.length === 1) {
    const entry = selectedEntries.value[0];
    if (entry.isDir) void enter(entry);
    else void downloadOne(entry);
  }
}

defineExpose({ refresh });
</script>

<template>
  <section class="files" @contextmenu.self.prevent="openMenu($event, null)">
    <!-- toolbar -->
    <header class="toolbar">
      <div class="crumbs-wrap">
        <button class="icon-btn" :disabled="crumbs.length <= 1" title="返回上级" @click="goUp">
          <Icon name="chevron-left" :size="16" />
        </button>
        <nav class="crumbs">
          <template v-for="(crumb, index) in crumbs" :key="crumb.fid">
            <button
              class="crumb"
              :class="{ 'crumb--current': index === crumbs.length - 1 }"
              :title="crumb.name"
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

      <div class="toolbar-actions">
        <div class="search-box">
          <Icon name="search" :size="14" class="search-icon" />
          <input
            v-model="query"
            class="search-input"
            type="text"
            placeholder="搜索当前目录"
            spellcheck="false"
          />
          <button v-if="query" class="search-clear" @click="query = ''">
            <Icon name="x" :size="12" />
          </button>
        </div>
        <button class="icon-btn" title="刷新 (Ctrl+R)" @click="refresh">
          <Icon name="refresh" :size="15" :spin="loading" />
        </button>
      </div>
    </header>

    <!-- action bar -->
    <div class="actions">
      <!-- Narrow windows drop the button labels (see the `max-width: 1179px`
           block below), so every action keeps a `title` for its tooltip. -->
      <div class="actions-left">
        <button
          class="btn btn--solid"
          title="新建文件夹"
          @click="prompt = { mode: 'mkdir', value: '' }"
        >
          <Icon name="folder-plus" :size="14.5" />
          <span>新建文件夹</span>
        </button>
        <button class="btn btn--ghost" title="上传文件" :disabled="uploading" @click="pickUpload">
          <Icon name="upload" :size="14.5" :spin="uploading" />
          <span>上传</span>
        </button>
        <button
          class="btn btn--ghost"
          title="上传整个文件夹"
          :disabled="uploading"
          @click="pickUploadDir"
        >
          <Icon name="folder-plus" :size="14.5" />
          <span>文件夹</span>
        </button>
        <template v-if="selectedEntries.length">
          <span class="chip">已选 {{ selectedEntries.length }} 项</span>
          <button class="btn btn--ghost" title="下载" @click="downloadSelected">
            <Icon name="download" :size="14.5" />
            <span>下载</span>
          </button>
          <button class="btn btn--ghost" title="移动到…" @click="openRelocate('move')">
            <Icon name="move" :size="14.5" />
            <span>移动</span>
          </button>
          <button class="btn btn--ghost" title="复制到…" @click="openRelocate('copy')">
            <Icon name="copy" :size="14.5" />
            <span>复制</span>
          </button>
          <button
            v-if="selectedEntries.length === 1"
            class="btn btn--ghost"
            title="重命名 (F2)"
            @click="prompt = { mode: 'rename', value: selectedEntries[0].name, target: selectedEntries[0] }"
          >
            <Icon name="rename" :size="14.5" />
            <span>重命名</span>
          </button>
          <button class="btn btn--ghost" title="删除 (Delete)" @click="requestDelete(selectedEntries)">
            <Icon name="trash" :size="14.5" />
            <span>删除</span>
          </button>
        </template>
      </div>
      <div class="actions-right">
        <Segmented
          v-if="viewMode === 'list'"
          :model-value="density"
          compact
          :options="[
            { value: 'comfortable', label: '标准' },
            { value: 'compact', label: '紧凑' },
          ]"
          @update:model-value="setDensity"
        />
        <div class="view-toggle">
          <button
            class="view-btn"
            :class="{ 'view-btn--on': viewMode === 'list' }"
            title="列表视图"
            @click="setViewMode('list')"
          >
            <Icon name="list" :size="14" />
          </button>
          <button
            class="view-btn"
            :class="{ 'view-btn--on': viewMode === 'grid' }"
            title="网格视图"
            @click="setViewMode('grid')"
          >
            <Icon name="grid" :size="14" />
          </button>
        </div>
      </div>
    </div>

    <!-- list -->
    <div class="list-wrap" :class="{ 'list-wrap--drop': dragOver }">
      <div v-if="viewMode === 'list'" class="list-head" :class="density">
        <button class="col col--name" @click="toggleSort('name')">
          名称
          <Icon v-if="sortKey === 'name'" :name="sortAsc ? 'arrow-up' : 'arrow-down'" :size="11" />
        </button>
        <button class="col col--size" @click="toggleSort('size')">
          大小
          <Icon v-if="sortKey === 'size'" :name="sortAsc ? 'arrow-up' : 'arrow-down'" :size="11" />
        </button>
        <button class="col col--date" @click="toggleSort('updated')">
          修改时间
          <Icon v-if="sortKey === 'updated'" :name="sortAsc ? 'arrow-up' : 'arrow-down'" :size="11" />
        </button>
      </div>

      <div ref="bodyRef" class="list-body" @scroll.passive="onBodyScroll">
        <!-- An unlogged drive must not look like an empty one. -->
        <div v-if="!driveLoggedIn" class="state">
          <EmptyState
            icon="user"
            :title="`${driveName}未登录，请先登录`"
            description="登录后即可浏览、上传和下载这个网盘里的文件。"
          >
            <button class="btn btn--solid" @click="goLogin">去登录</button>
          </EmptyState>
        </div>

        <div v-else-if="error" class="state">
          <EmptyState icon="alert" title="加载失败" :description="error">
            <button class="btn btn--ghost" @click="refresh">重试</button>
          </EmptyState>
        </div>

        <div v-else-if="loading && !rows.length" class="state">
          <DriveLoader :provider="app.activeProvider" :caption="`正在读取${driveName}`" />
        </div>

        <div v-else-if="!rows.length" class="state">
          <EmptyState
            v-if="query"
            icon="search"
            title="没有找到匹配项"
            :description="`在当前目录下未搜索到「${query}」`"
          />
          <EmptyState
            v-else
            icon="folder"
            title="这里还空着"
            description="拖拽文件到此处即可上传，或使用上方按钮新建文件夹。"
          />
        </div>

        <div v-else-if="viewMode === 'grid'" class="grid">
          <motion.div
            v-for="(row, i) in visibleRows"
            :key="row.entry.fid"
            class="card"
            v-bind="rowEnter(i)"
            :class="{ 'card--selected': isSelected(row.entry.fid) }"
            @click="onRowClick($event, row.entry)"
            @dblclick="row.entry.isDir ? enter(row.entry) : downloadOne(row.entry)"
            @contextmenu.prevent.stop="openMenu($event, row.entry)"
          >
            <div class="card-art">
              <FileThumb
                :thumb="row.entry.thumb"
                :is-dir="row.entry.isDir"
                :ext="row.entry.ext"
                :name="row.entry.name"
                :size="44"
              />
            </div>
            <span class="card-name truncate" :title="row.entry.name">{{ row.entry.name }}</span>
            <span class="card-meta">
              {{ row.entry.isDir ? "文件夹" : formatSize(row.entry.size) }}
            </span>
            <button class="card-more mini-btn" title="更多" @click.stop="openMenu($event, row.entry)">
              <Icon name="more" :size="14" />
            </button>
          </motion.div>
        </div>

        <div v-else class="rows">
          <motion.div
            v-for="(row, i) in visibleRows"
            :key="row.entry.fid"
            class="row"
            v-bind="rowEnter(i)"
            :class="{
              [density]: true,
              'row--selected': isSelected(row.entry.fid),
            }"
            @click="onRowClick($event, row.entry)"
            @dblclick="row.entry.isDir ? enter(row.entry) : downloadOne(row.entry)"
            @contextmenu.prevent.stop="openMenu($event, row.entry)"
          >
            <div class="cell cell--name">
              <FileIcon
                :is-dir="row.entry.isDir"
                :ext="row.entry.ext"
                :name="row.entry.name"
                :size="density === 'compact' ? 26 : 32"
              />
              <div class="name-copy">
                <span class="name truncate" :title="row.entry.name">{{ row.entry.name }}</span>
                <span v-if="query && row.path.length > 1" class="name-path truncate">
                  {{ row.path.slice(0, -1).map((p) => p.name).join(" / ") }}
                </span>
                <span v-else class="name-kind">
                  {{ row.entry.isDir ? "文件夹" : kindLabel(fileKind(row.entry)) }}
                </span>
              </div>
              <div class="row-hover-actions">
                <button
                  v-if="!row.entry.isDir"
                  class="mini-btn"
                  title="下载"
                  @click.stop="downloadOne(row.entry)"
                >
                  <Icon name="download" :size="14" />
                </button>
                <button class="mini-btn" title="更多" @click.stop="openMenu($event, row.entry)">
                  <Icon name="more" :size="14" />
                </button>
              </div>
            </div>
            <div class="cell cell--size mono">
              {{ row.entry.isDir ? "—" : formatSize(row.entry.size) }}
            </div>
            <div class="cell cell--date" :title="formatFullDate(row.entry.updatedAt)">
              {{ formatDate(row.entry.updatedAt) }}
            </div>
          </motion.div>
        </div>
      </div>

      <footer class="list-foot">
        <span>{{ folderCount }} 个文件夹</span>
        <span class="dot">·</span>
        <span>{{ rows.length - folderCount }} 个文件</span>
        <span class="dot">·</span>
        <span class="mono">{{ formatSize(totalSize) }}</span>
        <span v-if="visibleRows.length < rows.length" class="foot-more">
          已显示 {{ visibleRows.length }} / {{ rows.length }}
        </span>
      </footer>

      <AnimatePresence>
        <motion.div
          v-if="dragOver"
          class="drop-hint"
          :initial="{ opacity: 0 }"
          :animate="{ opacity: 1 }"
          :exit="{ opacity: 0 }"
          :transition="{ duration: 0.12 }"
        >
          <div class="drop-card">
            <Icon name="upload" :size="22" />
            <span>松开以上传文件</span>
          </div>
        </motion.div>
      </AnimatePresence>
    </div>

    <ContextMenu
      :open="menu.open"
      :x="menu.x"
      :y="menu.y"
      :items="menuItems"
      @select="onMenuSelect"
      @close="menu.open = false"
    />

    <!-- new folder / rename -->
    <Modal
      :open="prompt !== null"
      :title="prompt?.mode === 'mkdir' ? '新建文件夹' : '重命名'"
      :subtitle="prompt?.mode === 'mkdir' ? `位置：${crumbs[crumbs.length - 1].name}` : undefined"
      :width="420"
      @close="prompt = null"
    >
      <input
        v-if="prompt"
        v-model="prompt.value"
        class="input prompt-input"
        autofocus
        spellcheck="false"
        :placeholder="prompt.mode === 'mkdir' ? '文件夹名称' : '新名称'"
        @keydown.enter.prevent="submitPrompt"
      />
      <template #footer>
        <button class="btn" @click="prompt = null">取消</button>
        <button class="btn btn--solid" :disabled="busy || !prompt?.value.trim()" @click="submitPrompt">
          确定
        </button>
      </template>
    </Modal>

    <!-- download destination -->
    <Modal
      :open="downloadPrompt !== null"
      title="选择下载位置"
      :subtitle="downloadSummary"
      :width="470"
      @close="downloadPrompt = null"
    >
      <div class="dl-dir">
        <Icon name="folder" :size="15" class="dl-dir-icon" />
        <span class="mono truncate" :title="downloadPrompt?.dir">{{ downloadPrompt?.dir }}</span>
        <button class="btn btn--ghost" @click="chooseDownloadDir">更改</button>
      </div>
      <label class="dl-remember">
        <input
          type="checkbox"
          :checked="downloadPrompt?.remember ?? false"
          @change="
            downloadPrompt &&
              (downloadPrompt.remember = ($event.target as HTMLInputElement).checked)
          "
        />
        <span>同时把这个文件夹设为默认下载目录</span>
      </label>
      <p v-if="downloadTraffic" class="dl-warn">
        <strong>{{ downloadTraffic.short }}</strong>
        {{ downloadTraffic.detail }}
      </p>
      <p class="dl-hint">
        文件夹会以同名子目录保存；同名文件自动追加 (2)、(3) 序号，不会覆盖已有内容。
      </p>
      <template #footer>
        <button class="btn" @click="downloadPrompt = null">取消</button>
        <button
          class="btn btn--solid"
          :disabled="downloading || !downloadPrompt?.dir"
          @click="confirmDownload"
        >
          {{ downloading ? "正在加入队列…" : "开始下载" }}
        </button>
      </template>
    </Modal>

    <!-- move / copy target folder -->
    <Modal
      :open="relocate !== null"
      :title="relocate?.mode === 'move' ? '移动到' : '复制到'"
      :subtitle="`${relocate?.entries.length ?? 0} 项 · 选择目标文件夹`"
      :width="520"
      @close="relocate = null"
    >
      <div class="picker">
        <div v-if="relocateTargets.length > 1" class="picker-drives">
          <button
            v-for="state in relocateTargets"
            :key="state.provider"
            class="picker-drive"
            :class="{ 'picker-drive--on': relocate?.provider === state.provider }"
            @click="pickDrive(state.provider)"
          >
            <img
              v-if="PROVIDERS.find((p) => p.id === state.provider)?.logo"
              class="picker-drive-logo"
              :src="PROVIDERS.find((p) => p.id === state.provider)?.logo"
              alt=""
            />
            <span class="truncate">
              {{ PROVIDERS.find((p) => p.id === state.provider)?.short ?? state.provider }}
            </span>
          </button>
        </div>
        <p v-if="relocateForeign" class="picker-hint">
          目标在另一个网盘：会先下载到本地，再上传过去，任务可以在传输列表里查看。
        </p>
        <p v-if="relocateForeign && relocateTraffic" class="dl-warn">
          <strong>{{ relocateTraffic.short }}</strong>
          {{ relocateTraffic.detail }}
        </p>
        <div class="picker-bar">
          <button
            class="icon-btn"
            title="上一级"
            :disabled="(relocate?.path.length ?? 1) <= 1"
            @click="relocateUp"
          >
            <Icon name="chevron-left" :size="15" />
          </button>
          <span class="picker-path truncate">
            {{ (relocate?.path ?? []).map((node) => node.name).join(" / ") }}
          </span>
          <span v-if="relocate?.loading" class="chip">读取中…</span>
        </div>
        <div class="picker-list">
          <button
            v-for="dir in relocate?.dirs ?? []"
            :key="dir.fid"
            class="picker-row"
            @click="relocateInto(dir)"
          >
            <Icon name="folder" :size="15" />
            <span class="truncate">{{ dir.name }}</span>
            <Icon name="chevron-right" :size="14" class="picker-chevron" />
          </button>
          <p v-if="!(relocate?.dirs ?? []).length && !relocate?.loading" class="picker-empty">
            这里没有子文件夹，可以直接放到当前目录
          </p>
        </div>
      </div>
      <template #footer>
        <button class="btn" @click="relocate = null">取消</button>
        <button class="btn btn--solid" :disabled="relocate?.busy" @click="confirmRelocate">
          {{ relocateLabel }}
        </button>
      </template>
    </Modal>

    <!-- create share -->
    <Modal
      :open="shareDialog !== null"
      title="创建分享链接"
      :subtitle="`${driveName} · ${shareDialog?.entries.length ?? 0} 项`"
      :width="480"
      @close="shareDialog = null"
    >
      <div v-if="shareDialog" class="share-form">
        <template v-if="!shareDialog.result">
          <label class="field">
            <span class="field-label">分享名称</span>
            <input v-model="shareDialog.title" class="input" placeholder="分享名称" />
          </label>
          <div class="field">
            <span class="field-label">有效期</span>
            <Segmented
              v-model="shareDialog.days"
              :options="[
                { value: '0', label: '永久' },
                { value: '7', label: '7 天' },
                { value: '30', label: '30 天' },
              ]"
            />
          </div>
          <label class="field">
            <span class="field-label">提取码</span>
            <input
              v-model="shareDialog.passcode"
              class="input mono"
              maxlength="4"
              :placeholder="
                shareNeedsCode
                  ? '必填，4 位'
                  : shareCodeForced
                    ? '由网盘自动生成'
                    : '留空则不需要提取码'
              "
              :disabled="shareCodeForced"
            />
            <span class="field-hint">{{ shareCodeHint }}</span>
          </label>
          <p class="share-note">
            创建的分享会出现在网盘自己的分享列表里，也可以在这里随时取消。
          </p>
        </template>
        <template v-else>
          <div class="share-result">
            <div class="share-link-row">
              <input class="input mono" readonly :value="shareDialog.result.url" />
              <button class="btn" @click="copyShareLink">
                <Icon name="copy" :size="14" />
                复制
              </button>
            </div>
            <p v-if="shareDialog.result.passcode" class="share-code">
              提取码 <span class="mono">{{ shareDialog.result.passcode }}</span>
            </p>
            <p class="field-hint">
              {{ shareDialog.result.expiredAt > 0
                ? `有效期至 ${formatFullDate(shareDialog.result.expiredAt)}`
                : "永久有效" }}
            </p>
          </div>
        </template>
      </div>
      <template #footer>
        <template v-if="shareDialog?.result">
          <button class="btn" :disabled="shareDialog?.busy" @click="revokeShare">
            取消分享
          </button>
          <button class="btn" @click="shareDialog = null">关闭</button>
          <button class="btn btn--solid" @click="copyShareLink">复制链接</button>
        </template>
        <template v-else>
          <button class="btn" @click="shareDialog = null">取消</button>
          <button
            class="btn btn--solid"
            :disabled="shareDialog?.busy || (shareNeedsCode && shareDialog?.passcode.trim().length !== 4)"
            @click="submitShare"
          >
            {{ shareDialog?.busy ? "创建中…" : "创建链接" }}
          </button>
        </template>
      </template>
    </Modal>

    <!-- delete confirm -->
    <Modal
      :open="confirmDelete !== null"
      title="删除确认"
      :subtitle="deleteHint"
      :width="440"
      @close="confirmDelete = null"
    >
      <ul class="delete-list">
        <li v-for="entry in (confirmDelete ?? []).slice(0, 6)" :key="entry.fid" class="truncate">
          {{ entry.name }}
        </li>
        <li v-if="(confirmDelete?.length ?? 0) > 6" class="dim">
          以及另外 {{ (confirmDelete?.length ?? 0) - 6 }} 项…
        </li>
      </ul>
      <template #footer>
        <button class="btn" @click="confirmDelete = null">取消</button>
        <button class="btn btn--solid" :disabled="busy" @click="doDelete(confirmDelete ?? [])">
          删除
        </button>
      </template>
    </Modal>
  </section>
</template>

<style scoped>
.files {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-width: 0;
  background: var(--bg);
}

/* ------------------------------- toolbar ------------------------------- */
.toolbar {
  display: flex;
  align-items: center;
  gap: 12px;
  height: 52px;
  flex: none;
  padding: 0 16px 0 10px;
  border-bottom: 1px solid var(--line);
  user-select: none;
}
.crumbs-wrap {
  display: flex;
  align-items: center;
  gap: 2px;
  min-width: 0;
  flex: 1;
}
.crumbs {
  display: flex;
  align-items: center;
  gap: 1px;
  min-width: 0;
  overflow: hidden;
}
.crumb {
  max-width: 180px;
  padding: 4px 7px;
  border-radius: 6px;
  font-size: 12.5px;
  font-weight: 480;
  color: var(--text-2);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  transition: background-color var(--dur-1) var(--ease-out),
    color var(--dur-1) var(--ease-out);
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
.toolbar-actions {
  display: flex;
  align-items: center;
  gap: 4px;
}
.search-box {
  position: relative;
  display: flex;
  align-items: center;
  /* Shrinks before the breadcrumbs do: the trail is the only thing telling the
     user where they are, while the search box keeps working when narrow. */
  flex: 0 1 210px;
  min-width: 120px;
  height: 30px;
  padding: 0 9px 0 28px;
  border-radius: var(--radius-sm);
  background: var(--bg-sunken);
  box-shadow: inset 0 0 0 1px var(--line);
  transition: box-shadow var(--dur-2) var(--ease-out),
    background-color var(--dur-2) var(--ease-out);
}
:root[data-theme="dark"] .search-box {
  background: rgba(255, 255, 255, 0.045);
}
.search-box:focus-within {
  box-shadow: inset 0 0 0 1px var(--line-strong), 0 0 0 3px var(--bg-active);
}
.search-icon {
  position: absolute;
  left: 9px;
  color: var(--text-3);
}
.search-input {
  width: 100%;
  border: 0;
  background: none;
  outline: none;
  font-size: 12.5px;
}
.search-input::placeholder {
  color: var(--text-3);
}
.search-clear {
  display: grid;
  place-items: center;
  width: 18px;
  height: 18px;
  border-radius: 50%;
  color: var(--text-3);
  background: var(--bg-active);
}
.search-clear:hover {
  color: var(--text);
}

/* ------------------------------- actions ------------------------------- */
/* The action bar is the one row that genuinely does not fit at the minimum
   window width: nine buttons plus the selected-count chip and the view toggle
   need ~920px, but a 940px window leaves 716px of content. Wrapping is the
   safety net; the two `max-width` blocks below shrink the bar instead, so it
   normally stays on one line rather than pushing the list down. */
.actions {
  display: flex;
  align-items: center;
  justify-content: space-between;
  flex-wrap: wrap;
  gap: 8px 12px;
  padding: 12px 16px 10px;
  flex: none;
}
.actions-left,
.actions-right {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
  min-width: 0;
}
/* The toggle keeps its two buttons side by side even when the bar wraps. */
.actions-right {
  flex-wrap: nowrap;
}

/* Nine action buttons plus the selected-count chip and the view toggle need
   ~987px of content width on one line. Content is `window - 224px sidebar -
   32px padding`, so the labels fit from 1243px up. Tightening the padding and
   gaps brings that down to ~907px, which covers the default 1220px window.
   Below that the bar would wrap into a second row and push the file list down,
   so the labels go away and the bar shrinks to ~389px. Every button carries a
   `title`, so the tooltip still names it. */
@media (max-width: 1279px) {
  .actions-left {
    gap: 6px;
  }
  .actions-left .btn {
    padding: 0 10px;
  }
}
@media (max-width: 1179px) {
  .actions-left .btn {
    padding: 0 9px;
  }
  .actions-left .btn span {
    display: none;
  }
}

/* List / grid switch. Two icon buttons in one sunken pill. */
.view-toggle {
  display: flex;
  align-items: center;
  gap: 2px;
  height: 26px;
  padding: 2px;
  border-radius: var(--radius-sm);
  background: var(--bg-sunken);
  box-shadow: inset 0 0 0 1px var(--line);
}
.view-btn {
  display: grid;
  place-items: center;
  width: 28px;
  height: 22px;
  border-radius: var(--radius-xs);
  color: var(--text-3);
  transition: background-color var(--dur-1) var(--ease-out),
    color var(--dur-1) var(--ease-out);
}
.view-btn:hover {
  color: var(--text);
}
.view-btn--on {
  color: var(--text);
  background: var(--bg);
  box-shadow: var(--shadow-sm);
}
:root[data-theme="dark"] .view-btn--on {
  background: rgba(255, 255, 255, 0.09);
  box-shadow: none;
}

/* -------------------------------- list -------------------------------- */
.list-wrap {
  position: relative;
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
  margin: 0 16px 14px;
  border-radius: var(--radius-md);
  border: 1px solid var(--line);
  overflow: hidden;
  transition: border-color var(--dur-2) var(--ease-out),
    box-shadow var(--dur-2) var(--ease-out);
}
.list-wrap--drop {
  border-color: var(--line-strong);
  box-shadow: 0 0 0 3px var(--bg-active);
}
.list-head {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 96px 148px;
  align-items: center;
  height: 32px;
  flex: none;
  padding: 0 14px;
  border-bottom: 1px solid var(--line);
  background: var(--bg-sunken);
  user-select: none;
}
:root[data-theme="dark"] .list-head {
  background: rgba(255, 255, 255, 0.025);
}
.col {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-size: 11px;
  font-weight: 560;
  letter-spacing: 0.04em;
  text-transform: uppercase;
  color: var(--text-3);
  text-align: left;
  transition: color var(--dur-1) var(--ease-out);
}
.col:hover {
  color: var(--text-2);
}
.col--size,
.col--date {
  justify-content: flex-end;
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
  position: relative;
  display: grid;
  grid-template-columns: minmax(0, 1fr) 96px 148px;
  align-items: center;
  border-radius: var(--radius-sm);
  cursor: default;
  user-select: none;
  transition: background-color var(--dur-1) var(--ease-out),
    box-shadow var(--dur-1) var(--ease-out);
}
.row.comfortable {
  height: 48px;
  padding: 0 9px;
}
.row.compact {
  height: 38px;
  padding: 0 9px;
}
.row:hover {
  background: var(--bg-hover);
}
.row--selected {
  background: var(--bg-active);
  box-shadow: inset 0 0 0 1px var(--line-strong);
}
.row--selected:hover {
  background: var(--bg-active);
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
  flex: 1;
}
.name {
  font-size: 13px;
  font-weight: 490;
  letter-spacing: -0.005em;
}
.row.compact .name {
  font-size: 12.5px;
}
.name-kind,
.name-path {
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
.row-hover-actions {
  display: flex;
  gap: 2px;
  opacity: 0;
  transition: opacity var(--dur-1) var(--ease-out);
}
.row:hover .row-hover-actions {
  opacity: 1;
}
.mini-btn {
  display: grid;
  place-items: center;
  width: 26px;
  height: 26px;
  border-radius: 6px;
  color: var(--text-2);
  transition: background-color var(--dur-1) var(--ease-out),
    color var(--dur-1) var(--ease-out);
}
.mini-btn:hover {
  background: var(--bg-active);
  color: var(--text);
}

/* ------------------------------- grid -------------------------------- */
.grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(132px, 1fr));
  gap: 6px;
  padding: 10px;
}
.card {
  position: relative;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  padding: 14px 10px 10px;
  border-radius: var(--radius-sm);
  cursor: default;
  user-select: none;
  transition: background-color var(--dur-1) var(--ease-out),
    box-shadow var(--dur-1) var(--ease-out);
}
.card:hover {
  background: var(--bg-hover);
}
.card--selected {
  background: var(--bg-active);
  box-shadow: inset 0 0 0 1px var(--line-strong);
}
.card-art {
  display: grid;
  place-items: center;
  height: 48px;
  margin-bottom: 2px;
}
.card-name {
  max-width: 100%;
  font-size: 12.4px;
  font-weight: 490;
  text-align: center;
}
.card-meta {
  font-size: 10.5px;
  color: var(--text-3);
}
.card-more {
  position: absolute;
  top: 4px;
  right: 4px;
  opacity: 0;
  transition: opacity var(--dur-1) var(--ease-out);
}
.card:hover .card-more {
  opacity: 1;
}

/* -------------------------------- footer -------------------------------- */
.foot-more {
  margin-left: auto;
  color: var(--text-3);
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
.dot {
  opacity: 0.5;
}

/* --------------------------------- drop --------------------------------- */
.drop-hint {
  position: absolute;
  inset: 0;
  display: grid;
  place-items: center;
  background: color-mix(in srgb, var(--bg) 78%, transparent);
  backdrop-filter: blur(3px);
  pointer-events: none;
}
.drop-card {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 16px 26px;
  border-radius: var(--radius-md);
  font-size: 13px;
  font-weight: 520;
  color: var(--text);
  background: var(--bg);
  border: 1px dashed var(--line-strong);
  box-shadow: var(--shadow-md);
}

/* ---------------------------- move/copy picker ---------------------------- */
.picker {
  border: 1px solid var(--line);
  border-radius: var(--radius-sm);
  overflow: hidden;
}
.picker-bar {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 8px;
  border-bottom: 1px solid var(--line);
  background: var(--bg-sunken);
}
.picker-path {
  flex: 1;
  min-width: 0;
  font-size: 12.2px;
  color: var(--text-2);
}
.picker-drives {
  display: flex;
  gap: 6px;
  padding: 8px;
  overflow-x: auto;
  border-bottom: 1px solid var(--line);
}
.picker-drive {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  flex: none;
  height: 26px;
  padding: 0 10px 0 6px;
  border-radius: var(--radius-pill, 999px);
  box-shadow: inset 0 0 0 1px var(--line);
  font-size: 12.2px;
  color: var(--text-2);
  transition:
    background-color var(--dur-1) var(--ease-out),
    color var(--dur-1) var(--ease-out);
}
.picker-drive:hover {
  background: var(--bg-hover);
  color: var(--text);
}
.picker-drive--on {
  background: var(--bg-active);
  color: var(--text);
  box-shadow: inset 0 0 0 1px var(--line-strong);
}
.picker-drive-logo {
  width: 15px;
  height: 15px;
  border-radius: 4px;
  object-fit: contain;
}
.picker-hint {
  margin: 0;
  padding: 8px 10px;
  border-bottom: 1px solid var(--line);
  font-size: 11.8px;
  line-height: 1.5;
  color: var(--text-3);
}
.picker-list {
  max-height: 300px;
  overflow: auto;
  padding: 4px;
}
.picker-row {
  display: flex;
  align-items: center;
  gap: 9px;
  width: 100%;
  height: 34px;
  padding: 0 9px;
  border-radius: var(--radius-xs);
  font-size: 12.8px;
  color: var(--text);
  text-align: left;
  transition: background-color var(--dur-1) var(--ease-out);
}
.picker-row:hover {
  background: var(--bg-hover);
}
.picker-row > span {
  flex: 1;
  min-width: 0;
}
.picker-chevron {
  color: var(--text-3);
}
.picker-empty {
  margin: 0;
  padding: 14px 10px;
  font-size: 12px;
  color: var(--text-3);
  text-align: center;
}

/* ------------------------------- misc bits ------------------------------- */
.prompt-input {
  width: 100%;
  height: 36px;
}
.delete-list {
  margin: 0;
  padding: 0 0 8px;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 4px;
  max-height: 190px;
  overflow: auto;
}
.delete-list li {
  padding: 7px 10px;
  border-radius: var(--radius-xs);
  font-size: 12.5px;
  background: var(--bg-sunken);
}

/* --------------------------- download dialog --------------------------- */
.dl-dir {
  display: flex;
  align-items: center;
  gap: 9px;
  padding: 9px 11px;
  border-radius: var(--radius-sm);
  border: 1px solid var(--line);
  background: var(--bg-sunken);
}
.dl-dir .mono {
  flex: 1;
  color: var(--text-2);
}
.dl-dir-icon {
  color: var(--text-3);
  flex: none;
}
.dl-remember {
  display: flex;
  align-items: center;
  gap: 8px;
  margin: 12px 0 0;
  font-size: 12.2px;
  color: var(--text-2);
  cursor: pointer;
  user-select: none;
}
.dl-remember input {
  accent-color: var(--fill);
}
/* Allowance warning: the file may arrive truncated, so it reads as a caution. */
.dl-warn {
  margin: 8px 0 0;
  padding: 8px 10px;
  border-radius: 8px;
  background: color-mix(in srgb, #e0554f 12%, transparent);
  border: 1px solid color-mix(in srgb, #e0554f 30%, transparent);
  font-size: 11.5px;
  line-height: 1.6;
  color: var(--text-2);
}
.dl-warn strong {
  display: block;
  margin-bottom: 2px;
  font-weight: 600;
  color: #e0554f;
}
.dl-hint {
  margin: 8px 0 0;
  font-size: 11.5px;
  line-height: 1.6;
  color: var(--text-3);
}

/* --------------------------- create-share dialog --------------------------- */
.share-form {
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.share-form .field {
  display: flex;
  flex-direction: column;
  gap: 7px;
  margin: 0;
}
.field-label {
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 0.05em;
  text-transform: uppercase;
  color: var(--text-3);
}
.field-hint {
  font-size: 11.5px;
  line-height: 1.6;
  color: var(--text-3);
}
.share-form .input {
  width: 100%;
}
.share-note {
  margin: 0;
  font-size: 11.5px;
  line-height: 1.6;
  color: var(--text-3);
}
.share-result {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.share-link-row {
  display: flex;
  gap: 8px;
}
.share-link-row .input {
  flex: 1;
  min-width: 0;
}
.share-code {
  margin: 0;
  font-size: 12.5px;
  color: var(--text-2);
}
.share-code .mono {
  padding: 2px 7px;
  border-radius: var(--radius-xs);
  background: var(--bg-sunken);
  color: var(--text);
  letter-spacing: 0.08em;
}

/* ------------------------------ row motion ------------------------------ */
/* Rows are `motion.div`s now — the enter animation and the layout move are both
   driven by motion-v (see `rowHidden`/`rowShown` in src/lib/motion.ts). No CSS
   transition classes are needed, and no leave animation: a row that is removed
   should be gone immediately, because keeping it around while it fades means the
   list is briefly taller than it should be. */
:where(.row, .card) {
  /* Rest on an identity transform, not `none`: WebKitGTK tears the compositing
     layer down when the animated transform disappears. */
  transform: translateY(0);
}
</style>
