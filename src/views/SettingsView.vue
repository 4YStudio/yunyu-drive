<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { pickFiles, revealPath } from "@/lib/host";

import Icon from "@/components/Icon.vue";
import Switch from "@/components/Switch.vue";
import Segmented from "@/components/Segmented.vue";
import { api } from "@/lib/backend";
import { BRAND } from "@/lib/brand";
import { PROVIDERS, type AppInfo, type ProviderId, type Settings } from "@/lib/types";
import { errText, formatSize } from "@/lib/format";
import {
  app,
  cycleTheme,
  goLoginFor,
  loadAccount,
  loadProviderStates,
  loadQuota,
  loadTraffic,
  logoutProvider,
  saveSettings,
  setThemeMode,
  toast,
} from "@/store/app";

/**
 * Settings are split in two: what belongs to the app (theme, download folder,
 * how many transfers run at once) and what belongs to one drive (its login,
 * its capacity). Mixing them in one long page was the complaint that led to
 * this layout, so the two live in separate sections and the drive list has a
 * page of its own.
 */
type SectionId = "general" | "download" | "upload" | "transfers" | "drives" | "about";

const SECTIONS: { id: SectionId; label: string; icon: string; hint: string }[] = [
  { id: "general", label: "通用", icon: "settings", hint: "主题与列表外观" },
  { id: "download", label: "下载", icon: "download", hint: "目录、并发与限速" },
  { id: "upload", label: "上传", icon: "upload", hint: "重名、并发与限速" },
  { id: "transfers", label: "传输列表", icon: "list", hint: "布局与提醒" },
  { id: "drives", label: "网盘", icon: "cloud", hint: "每个盘的登录与容量" },
  { id: "about", label: "关于与诊断", icon: "info", hint: "版本、日志与网络" },
];

const section = ref<SectionId>("general");
const draft = ref<Settings | null>(app.settings ? { ...app.settings } : null);
const info = ref<AppInfo | null>(null);
const dirty = computed(
  () => draft.value !== null && JSON.stringify(draft.value) !== JSON.stringify(app.settings),
);
const saving = ref(false);
/** Clearing the webview store takes a moment: WebKit closes its files first. */
const clearingWeb = ref(false);

/* ------------------------------------------------------------------ saving */

async function persist() {
  if (!draft.value) return;
  saving.value = true;
  try {
    await saveSettings({ ...draft.value });
    toast("设置已保存", "success");
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    saving.value = false;
  }
}

watch(
  () => app.settings,
  (value) => {
    if (value && !dirty.value) draft.value = { ...value };
  },
  { deep: true },
);

/* ------------------------------------------------------- numbers & speeds */

/** Parallel transfers: the manager clamps to 1..16, these cover the usual. */
const COUNT_PRESETS = [1, 2, 3, 5, 8, 10, 16];
const SPEED_PRESETS = [0, 512, 2048, 5120, 10240, 20480];

/** What a preset list means as a label: `0` is "no cap", the rest is KB/s. */
function speedOption(kbps: number): string {
  return kbps === 0 ? "不限速" : speedLabel(kbps);
}

/**
 * The select offers presets and one "自定义" entry. Picking that entry flips
 * the matching flag so the number box appears next to it without changing the
 * value the user already had.
 */
const custom = ref({ dlCount: false, ulCount: false, dlCap: false, ulCap: false });

function pickCount(kind: "download" | "upload", raw: string) {
  if (!draft.value) return;
  if (kind === "download") custom.value.dlCount = raw === "custom";
  else custom.value.ulCount = raw === "custom";
  if (raw === "custom") return;
  if (kind === "download") setDownloadCount(raw);
  else setUploadCount(raw);
}

function pickCap(kind: "download" | "upload", raw: string) {
  if (!draft.value) return;
  if (kind === "download") custom.value.dlCap = raw === "custom";
  else custom.value.ulCap = raw === "custom";
  if (raw === "custom") return;
  if (kind === "download") setDownloadCap(raw);
  else setUploadCap(raw);
}

/** True when the current value is not one of the presets (the input shows). */
function countIsCustom(value: number): boolean {
  return !COUNT_PRESETS.includes(value);
}

function capIsCustom(value: number): boolean {
  return !SPEED_PRESETS.includes(value);
}

function setCounts(patch: Partial<Settings>) {
  if (draft.value) Object.assign(draft.value, patch);
}

function setDownloadCount(raw: number | string) {
  const n = Math.round(Number(raw));
  if (!Number.isFinite(n)) return;
  setCounts({ concurrentTransfers: Math.min(16, Math.max(1, n)) });
}

function setUploadCount(raw: number | string) {
  const n = Math.round(Number(raw));
  if (!Number.isFinite(n)) return;
  setCounts({ concurrentUploads: Math.min(16, Math.max(1, n)) });
}

function setDownloadCap(raw: number | string) {
  const n = Math.round(Number(raw));
  if (!Number.isFinite(n)) return;
  setCounts({ speedLimitKbps: Math.max(0, n) });
}

function setUploadCap(raw: number | string) {
  const n = Math.round(Number(raw));
  if (!Number.isFinite(n)) return;
  setCounts({ speedLimitUploadKbps: Math.max(0, n) });
}

/** `10240` → `10 MB/s`, `0` → `不限速`. Used by chips and the summary line. */
function speedLabel(kbps: number): string {
  if (kbps <= 0) return "不限速";
  if (kbps >= 1024) {
    const mb = kbps / 1024;
    return `${Number.isInteger(mb) ? mb : mb.toFixed(1)} MB/s`;
  }
  return `${kbps} KB/s`;
}

/* -------------------------------------------------------------------- paths */

async function chooseDownloadDir() {
  const [picked] = await pickFiles({
    directory: true,
    multiple: false,
    title: "选择下载目录",
  });
  if (picked && draft.value) {
    draft.value.downloadDir = picked;
    await persist();
  }
}

async function clearWebData() {
  if (clearingWeb.value) return;
  clearingWeb.value = true;
  try {
    const res = await api.clearLoginWebData();
    const closed = res.windows > 0 ? `，关闭了 ${res.windows} 个登录窗口` : "";
    toast(
      res.files > 0
        ? `已清除 ${res.files} 个文件（${formatSize(res.bytes)}）${closed}`
        : `没有需要清理的数据${closed}`,
      "success",
    );
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    clearingWeb.value = false;
  }
}

/** Opens the app log, so a failing drive login can be reported with evidence. */
async function revealLog() {
  const ok = await revealPath(info.value?.logPath);
  if (!ok) toast("找不到日志文件", "error");
}

async function revealCookie() {
  const ok = await revealPath(info.value?.cookiePath);
  if (!ok) toast("无法打开文件管理器", "error");
}

/** Real exit: the tray menu is the normal path, this is the explicit one. */
async function quitApp() {
  try {
    await api.quitApp();
  } catch (err) {
    toast(errText(err), "error");
  }
}

/* ------------------------------------------------------------------- drives */

/** Logging out of one drive while the request is in flight. */
const busyDrive = ref<ProviderId | null>(null);

/**
 * One row per drive: every drive authenticates on its own, so the app cannot
 * have a single "account" setting here.
 */
const accountRows = computed(() =>
  PROVIDERS.map((drive) => {
    const state = app.providers.find((p) => p.provider === drive.id);
    const quark = drive.id === "quark";
    return {
      id: drive.id,
      name: drive.name,
      logo: drive.logo,
      active: drive.id === app.activeProvider,
      authenticated: quark ? app.authenticated : (state?.authenticated ?? false),
      nickname: quark ? (app.account?.nickname ?? null) : (state?.nickname ?? null),
      masked: quark ? app.maskedCookie : (state?.masked ?? null),
      quota: app.quotas[drive.id] ?? null,
      traffic: app.traffics[drive.id] ?? null,
    };
  }),
);

const signedIn = computed(() => accountRows.value.filter((row) => row.authenticated).length);

async function logoutDrive(id: ProviderId) {
  busyDrive.value = id;
  try {
    await logoutProvider(id);
    toast(`已退出${PROVIDERS.find((p) => p.id === id)?.name ?? ""}`, "success");
  } catch (err) {
    toast(errText(err), "error");
  } finally {
    busyDrive.value = null;
  }
}

/* -------------------------------------------------------------------- theme */

const themeOptions = [
  { value: "light", label: "浅色" },
  { value: "dark", label: "深色" },
  { value: "system", label: "跟随系统" },
];

const theme = computed({
  get: () => app.themeMode,
  set: (value: string) => setThemeMode(value as "light" | "dark" | "system"),
});

onMounted(async () => {
  try {
    info.value = await api.appInfo();
  } catch {
    /* ignore */
  }
  if (!draft.value && app.settings) draft.value = { ...app.settings };
  if (app.authenticated && !app.account) void loadAccount();
  // The drive list shows every capacity, not just the active drive's.
  await loadProviderStates();
  for (const drive of app.providers) {
    if (drive.authenticated) {
      void loadQuota(drive.provider);
      void loadTraffic(drive.provider);
    }
  }
});
</script>

<template>
  <section class="settings">
    <header class="head">
      <div class="head-copy">
        <h1 class="title">设置</h1>
        <p class="sub">软件设置与各个网盘的账号分开管理</p>
      </div>
      <button class="btn btn--solid" :disabled="!dirty || saving" @click="persist">
        {{ dirty ? "保存更改" : "已保存" }}
      </button>
    </header>

    <div v-if="draft" class="panes">
      <!-- section nav: the page is long, so the sections are addressable -->
      <nav class="nav" aria-label="设置分区">
        <button
          v-for="item in SECTIONS"
          :key="item.id"
          class="nav-item"
          :class="{ 'nav-item--on': section === item.id }"
          @click="section = item.id"
        >
          <Icon :name="item.icon as never" :size="14" />
          <span class="nav-label">{{ item.label }}</span>
          <span v-if="item.id === 'drives'" class="nav-count">{{ signedIn }}</span>
        </button>
        <p class="nav-foot">
          修改后点右上角保存。传输相关的改动立即生效，不会打断正在跑的任务。
        </p>
      </nav>

      <div class="pane">
        <!-- ------------------------------------------------------------ 通用 -->
        <section v-if="section === 'general'" class="group">
          <h2 class="group-title">通用</h2>
          <div class="card">
            <div class="row">
              <div class="row-copy">
                <span class="row-label">主题</span>
                <span class="row-hint">界面黑白双色调，可跟随系统自动切换</span>
              </div>
              <Segmented v-model="theme" :options="themeOptions" />
            </div>
            <div class="divider" />
            <div class="row">
              <div class="row-copy">
                <span class="row-label">快捷切换</span>
                <span class="row-hint">在侧边栏底部快速切换主题</span>
              </div>
              <button class="btn btn--ghost" @click="cycleTheme">
                <Icon name="palette" :size="14" />
                <span>切换主题</span>
              </button>
            </div>
            <div class="divider" />
            <div class="row">
              <div class="row-copy">
                <span class="row-label">列表密度</span>
                <span class="row-hint">文件列表的行高与信息密度</span>
              </div>
              <Segmented
                :model-value="draft.listDensity"
                :options="[
                  { value: 'comfortable', label: '标准' },
                  { value: 'compact', label: '紧凑' },
                ]"
                @update:model-value="(v: string) => draft && (draft.listDensity = v)"
              />
            </div>
            <div class="divider" />
            <Switch
              :model-value="draft.showHidden"
              label="显示隐藏文件"
              hint="显示以 . 开头的文件和文件夹"
              @update:model-value="(v) => draft && (draft.showHidden = v)"
            />
            <div class="divider" />
            <Switch
              :model-value="draft.closeToTray"
              label="关闭窗口时最小化到托盘"
              hint="上传 / 下载会继续在后台进行，从托盘菜单可以退出"
              @update:model-value="(v) => draft && (draft.closeToTray = v)"
            />
          </div>
        </section>

        <!-- ------------------------------------------------------------ 下载 -->
        <section v-else-if="section === 'download'" class="group">
          <h2 class="group-title">下载</h2>
          <div class="card">
            <div class="row">
              <div class="row-copy">
                <span class="row-label">下载目录</span>
                <span class="row-hint mono truncate">{{ draft.downloadDir }}</span>
              </div>
              <button class="btn btn--ghost" @click="chooseDownloadDir">选择目录</button>
            </div>
            <div class="divider" />
            <div class="row row--stack">
              <div class="row-copy">
                <span class="row-label">同时下载数</span>
                <span class="row-hint">
                  超出的任务排队，完成一个后自动开始下一个。文件夹里的文件共用这个名额。
                </span>
              </div>
              <div class="field">
                <select
                  class="select"
                  :value="custom.dlCount || countIsCustom(draft.concurrentTransfers) ? 'custom' : String(draft.concurrentTransfers)"
                  @change="pickCount('download', ($event.target as HTMLSelectElement).value)"
                >
                  <option v-for="n in COUNT_PRESETS" :key="`dl-${n}`" :value="String(n)">{{ n }} 个</option>
                  <option value="custom">自定义…</option>
                </select>
                <label v-if="custom.dlCount || countIsCustom(draft.concurrentTransfers)" class="num">
                  <input
                    :value="draft.concurrentTransfers"
                    type="number"
                    min="1"
                    max="16"
                    @change="setDownloadCount(($event.target as HTMLInputElement).value)"
                  />
                  <span>个</span>
                </label>
              </div>
            </div>
            <div class="divider" />
            <div class="row row--stack">
              <div class="row-copy">
                <span class="row-label">下载限速</span>
                <span class="row-hint">
                  所有下载共享这个上限，当前 {{ speedLabel(draft.speedLimitKbps) }}。
                </span>
              </div>
              <div class="field">
                <select
                  class="select"
                  :value="custom.dlCap || capIsCustom(draft.speedLimitKbps) ? 'custom' : String(draft.speedLimitKbps)"
                  @change="pickCap('download', ($event.target as HTMLSelectElement).value)"
                >
                  <option v-for="kbps in SPEED_PRESETS" :key="`dl-cap-${kbps}`" :value="String(kbps)">
                    {{ speedOption(kbps) }}
                  </option>
                  <option value="custom">自定义…</option>
                </select>
                <label v-if="custom.dlCap || capIsCustom(draft.speedLimitKbps)" class="num">
                  <input
                    :value="draft.speedLimitKbps"
                    type="number"
                    min="0"
                    step="128"
                    @change="setDownloadCap(($event.target as HTMLInputElement).value)"
                  />
                  <span>KB/s</span>
                </label>
              </div>
            </div>
          </div>
        </section>

        <!-- ------------------------------------------------------------ 上传 -->
        <section v-else-if="section === 'upload'" class="group">
          <h2 class="group-title">上传</h2>
          <div class="card">
            <div class="row">
              <div class="row-copy">
                <span class="row-label">上传重名处理</span>
                <span class="row-hint">同名文件默认追加 (2)、(3) 序号；「跳过」则不上传</span>
              </div>
              <Segmented
                :model-value="draft.uploadConflict"
                :options="[
                  { value: 'rename', label: '自动重命名' },
                  { value: 'skip', label: '跳过' },
                ]"
                @update:model-value="(v: string) => draft && (draft.uploadConflict = v)"
              />
            </div>
            <div class="divider" />
            <div class="row row--stack">
              <div class="row-copy">
                <span class="row-label">同时上传数</span>
                <span class="row-hint">
                  和下载分开计数：上传再多也不会占掉下载的名额。文件夹上传按文件逐个占用。
                </span>
              </div>
              <div class="field">
                <select
                  class="select"
                  :value="custom.ulCount || countIsCustom(draft.concurrentUploads) ? 'custom' : String(draft.concurrentUploads)"
                  @change="pickCount('upload', ($event.target as HTMLSelectElement).value)"
                >
                  <option v-for="n in COUNT_PRESETS" :key="`ul-${n}`" :value="String(n)">{{ n }} 个</option>
                  <option value="custom">自定义…</option>
                </select>
                <label v-if="custom.ulCount || countIsCustom(draft.concurrentUploads)" class="num">
                  <input
                    :value="draft.concurrentUploads"
                    type="number"
                    min="1"
                    max="16"
                    @change="setUploadCount(($event.target as HTMLInputElement).value)"
                  />
                  <span>个</span>
                </label>
              </div>
            </div>
            <div class="divider" />
            <div class="row row--stack">
              <div class="row-copy">
                <span class="row-label">上传限速</span>
                <span class="row-hint">
                  只用在上传上，当前 {{ speedLabel(draft.speedLimitUploadKbps) }}；下载不受影响。
                </span>
              </div>
              <div class="field">
                <select
                  class="select"
                  :value="custom.ulCap || capIsCustom(draft.speedLimitUploadKbps) ? 'custom' : String(draft.speedLimitUploadKbps)"
                  @change="pickCap('upload', ($event.target as HTMLSelectElement).value)"
                >
                  <option v-for="kbps in SPEED_PRESETS" :key="`ul-cap-${kbps}`" :value="String(kbps)">
                    {{ speedOption(kbps) }}
                  </option>
                  <option value="custom">自定义…</option>
                </select>
                <label v-if="custom.ulCap || capIsCustom(draft.speedLimitUploadKbps)" class="num">
                  <input
                    :value="draft.speedLimitUploadKbps"
                    type="number"
                    min="0"
                    step="128"
                    @change="setUploadCap(($event.target as HTMLInputElement).value)"
                  />
                  <span>KB/s</span>
                </label>
              </div>
            </div>
          </div>
        </section>

        <!-- ------------------------------------------------------ 传输列表 -->
        <section v-else-if="section === 'transfers'" class="group">
          <h2 class="group-title">传输列表</h2>
          <div class="card">
            <div class="row">
              <div class="row-copy">
                <span class="row-label">列表布局</span>
                <span class="row-hint">「紧凑」一行一条任务，一屏能看十几条</span>
              </div>
              <Segmented
                :model-value="draft.transferDensity"
                :options="[
                  { value: 'card', label: '卡片' },
                  { value: 'compact', label: '紧凑' },
                ]"
                @update:model-value="(v: string) => draft && (draft.transferDensity = v)"
              />
            </div>
            <div class="divider" />
            <Switch
              :model-value="draft.confirmDelete"
              label="删除前确认"
              hint="在文件列表里删除文件时弹出确认对话框"
              @update:model-value="(v) => draft && (draft.confirmDelete = v)"
            />
            <div class="divider" />
            <Switch
              :model-value="draft.notifications"
              label="传输提醒"
              hint="传输完成、失败或收到分享链接时提醒"
              @update:model-value="(v) => draft && (draft.notifications = v)"
            />
            <div class="divider" />
            <div class="row">
              <div class="row-copy">
                <span class="row-label">提醒方式</span>
                <span class="row-hint">系统通知走桌面角标，应用内只在窗口里提示，静默则都不出现</span>
              </div>
              <Segmented
                :model-value="draft.notifyMode"
                :options="[
                  { value: 'system', label: '系统通知' },
                  { value: 'inapp', label: '应用内' },
                  { value: 'silent', label: '静默' },
                ]"
                @update:model-value="(v: string) => draft && (draft.notifyMode = v)"
              />
            </div>
          </div>
        </section>

        <!-- ------------------------------------------------------------ 网盘 -->
        <section v-else-if="section === 'drives'" class="group">
          <h2 class="group-title">网盘账号</h2>
          <p class="group-hint">
            每个网盘单独登录，容量与流量只有登录后才显示。这里只放网盘自己的东西，
            下载目录、并发这类偏好不在这里。
          </p>
          <div class="card">
            <template v-for="(drive, index) in accountRows" :key="drive.id">
              <div v-if="index > 0" class="divider" />
              <div class="drive">
                <img class="drive-logo" :src="drive.logo" :alt="drive.name" width="26" height="26" />
                <div class="drive-copy">
                  <span class="row-label">
                    {{ drive.name }}
                    <span v-if="drive.active" class="row-badge">当前</span>
                    <span v-if="!drive.authenticated" class="row-badge">未登录</span>
                  </span>
                  <span class="row-hint truncate">
                    {{ drive.authenticated ? (drive.nickname ?? "已连接") : "登录后可以浏览这个网盘的文件" }}
                  </span>
                  <span v-if="drive.quota || drive.traffic" class="drive-stats">
                    <span v-if="drive.quota">
                      已用 {{ formatSize(drive.quota.used) }} / {{ formatSize(drive.quota.total) }}
                    </span>
                    <span v-if="drive.traffic">
                      下载流量剩余 {{ formatSize(drive.traffic.left) }}
                    </span>
                  </span>
                </div>
                <div class="row-actions">
                  <button
                    v-if="drive.id === 'quark' && drive.authenticated"
                    class="btn btn--ghost"
                    @click="revealCookie"
                  >
                    查看凭据
                  </button>
                  <button
                    v-if="drive.authenticated"
                    class="btn btn--ghost"
                    :disabled="busyDrive === drive.id"
                    @click="logoutDrive(drive.id)"
                  >
                    退出登录
                  </button>
                  <button v-else class="btn btn--solid" @click="goLoginFor(drive.id)">去登录</button>
                </div>
              </div>
            </template>
          </div>

          <h2 class="group-title group-title--spaced">网盘专用选项</h2>
          <div class="card">
            <div class="row">
              <div class="row-copy">
                <span class="row-label">夸克 API 地址</span>
                <span class="row-hint">只有夸克网盘使用这一项，其它网盘用的是官方地址</span>
              </div>
              <input v-model="draft.apiBaseUrl" class="input api-input" spellcheck="false" />
            </div>
          </div>

          <h2 class="group-title group-title--spaced">登录与缓存</h2>
          <div class="card">
            <div class="row">
              <div class="row-copy">
                <span class="row-label">清除内置浏览器数据</span>
                <span class="row-hint">
                  清掉网页登录窗口留下的 Cookie、缓存与站点存储。某个网盘的登录页一直来回跳、
                  提示已登录却进不去时点这里。已保存的登录状态在本地配置文件里，不受影响。
                </span>
              </div>
              <div class="row-actions">
                <button class="btn btn--ghost" :disabled="clearingWeb" @click="clearWebData">
                  <Icon v-if="clearingWeb" name="loader" :size="13" spin />
                  <span>{{ clearingWeb ? "正在清理…" : "立即清理" }}</span>
                </button>
              </div>
            </div>
          </div>
        </section>

        <!-- ------------------------------------------------------ 关于与诊断 -->
        <section v-else class="group">
          <h2 class="group-title">关于</h2>
          <div class="card about">
            <div class="about-mark">
              <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
                <circle cx="10.5" cy="10.5" r="6.2" />
                <path d="M15.2 15.2 20 20" />
              </svg>
            </div>
            <div class="about-copy">
              <span class="about-name">{{ BRAND.product }} · {{ info?.version ?? "0.1.0" }}</span>
              <span class="about-meta">Tauri 2 · Vue 3 · {{ info?.platform ?? "linux" }}</span>
              <span class="about-path mono truncate" :title="info?.configDir">
                配置目录：{{ info?.configDir ?? "—" }}
              </span>
            </div>
            <div class="row-actions">
              <button v-if="info?.logPath" class="btn btn--ghost" @click="revealLog">
                <Icon name="list" :size="13" />
                <span>日志文件</span>
              </button>
              <button class="btn btn--ghost" @click="quitApp">
                <Icon name="logout" :size="13" />
                <span>退出程序</span>
              </button>
            </div>
          </div>

          <h2 class="group-title group-title--spaced">诊断</h2>
          <p class="group-hint">出问题时把这里的内容复制给开发者，不用截图。</p>
          <div class="card">
            <div class="row">
              <div class="row-copy">
                <span class="row-label">运行日志</span>
                <span class="row-hint">控制台输出、每次前后端调用与错误</span>
              </div>
              <button class="btn btn--ghost" @click="app.view = 'logs'">
                <Icon name="list" :size="13" />
                <span>查看</span>
              </button>
            </div>
            <div class="divider" />
            <div class="row">
              <div class="row-copy">
                <span class="row-label">网络请求</span>
                <span class="row-hint">网盘 HTTP 调用的请求头/体与响应头/体，看网盘为什么拒绝上传</span>
              </div>
              <button class="btn btn--ghost" @click="app.view = 'net'">
                <Icon name="globe" :size="13" />
                <span>查看</span>
              </button>
            </div>
          </div>
        </section>
      </div>
    </div>
  </section>
</template>

<style scoped>
.settings {
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
  width: 100%;
  max-width: 980px;
  margin: 0 auto;
  padding: 22px 24px 12px;
  box-sizing: border-box;
}
.head-copy {
  display: flex;
  flex-direction: column;
  min-width: 0;
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

/* nav on the left, one section at a time on the right: the page used to be one
   long scroll where drive settings sat between the theme and the log buttons */
.panes {
  flex: 1;
  min-height: 0;
  display: grid;
  grid-template-columns: 168px minmax(0, 1fr);
  gap: 22px;
  width: 100%;
  max-width: 980px;
  margin: 0 auto;
  padding: 0 24px 26px;
  box-sizing: border-box;
}
.nav {
  position: sticky;
  top: 0;
  align-self: start;
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding-top: 2px;
}
.nav-item {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 7px 9px;
  border: none;
  border-radius: 9px;
  background: transparent;
  color: var(--text-2);
  font-size: 12.6px;
  text-align: left;
  cursor: pointer;
}
.nav-item:hover {
  color: var(--text);
  background: var(--hover);
}
.nav-item--on {
  color: var(--text);
  background: var(--hover);
  font-weight: 540;
}
.nav-label {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.nav-count {
  padding: 0 5px;
  border-radius: 999px;
  border: 1px solid var(--line);
  font-size: 10px;
  color: var(--text-3);
}
.nav-foot {
  margin: 14px 2px 0;
  font-size: 10.8px;
  line-height: 1.55;
  color: var(--text-3);
}

.pane {
  min-width: 0;
  overflow-y: auto;
  padding-bottom: 8px;
}
.group + .group {
  margin-top: 4px;
}
.group-title {
  margin: 0 0 8px 2px;
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 0.06em;
  text-transform: uppercase;
  color: var(--text-3);
}
.group-title--spaced {
  margin-top: 20px;
}
.group-hint {
  margin: -2px 2px 10px;
  font-size: 11.5px;
  line-height: 1.6;
  color: var(--text-3);
  max-width: 62ch;
}
.card {
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 6px 14px;
  border-radius: var(--radius-md);
  border: 1px solid var(--line);
  background: var(--bg);
}
.row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 20px;
  padding: 11px 0;
}
/* presets plus a custom box do not fit next to the label on narrow windows */
.row--stack {
  flex-wrap: wrap;
  align-items: flex-start;
  row-gap: 10px;
}
.row-copy {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
  flex: 1 1 260px;
}
.row-label {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12.8px;
  font-weight: 510;
}
.row-badge {
  padding: 1px 5px;
  border-radius: 999px;
  border: 1px solid var(--line);
  font-size: 9.5px;
  font-weight: 500;
  letter-spacing: 0.02em;
  color: var(--text-3);
}
.row-hint {
  font-size: 11.5px;
  color: var(--text-3);
  max-width: 52ch;
}
.row-actions {
  display: flex;
  gap: 8px;
  flex: none;
}
.divider {
  height: 1px;
  background: var(--line);
}

/* one preset select, plus a number box only when the value is off-list */
.field {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: none;
}
.select {
  min-width: 116px;
  height: 30px;
  padding: 0 8px;
  border: 1px solid var(--line);
  border-radius: 8px;
  background: var(--bg);
  color: var(--text);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
}
.select:focus {
  outline: none;
  border-color: var(--text-3);
}
.num {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  height: 30px;
  padding: 0 9px;
  border: 1px solid var(--line);
  border-radius: 8px;
}
.num input {
  width: 58px;
  border: none;
  background: transparent;
  color: var(--text);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
  text-align: right;
}
.num input:focus {
  outline: none;
}
.num span {
  font-size: 10.5px;
  color: var(--text-3);
}

/* drive rows: logo, identity, numbers, actions */
.drive {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 11px 0;
}
.drive-logo {
  flex: none;
  border-radius: 7px;
}
.drive-copy {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
  flex: 1 1 auto;
}
.drive-stats {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 12px;
  font-size: 11px;
  color: var(--text-2);
  font-variant-numeric: tabular-nums;
}

.api-input {
  width: 250px;
  font-family: var(--font-mono);
  font-size: 11.5px;
}
.about {
  flex-direction: row;
  align-items: center;
  gap: 14px;
  padding: 15px 16px;
}
.about-mark {
  display: grid;
  place-items: center;
  width: 42px;
  height: 42px;
  flex: none;
  border-radius: 13px;
  color: var(--on-fill);
  background: var(--fill);
}
.about-copy {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
  flex: 1 1 auto;
}
.about-name {
  font-size: 13px;
  font-weight: 540;
}
.about-meta {
  font-size: 11.5px;
  color: var(--text-2);
}
.about-path {
  color: var(--text-3);
  max-width: 54ch;
}

@media (max-width: 760px) {
  .panes {
    grid-template-columns: minmax(0, 1fr);
    gap: 12px;
  }
  .nav {
    position: static;
    flex-direction: row;
    flex-wrap: wrap;
  }
  .nav-foot {
    display: none;
  }
}
</style>
