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

const draft = ref<Settings | null>(app.settings ? { ...app.settings } : null);
const info = ref<AppInfo | null>(null);
const dirty = computed(
  () => draft.value !== null && JSON.stringify(draft.value) !== JSON.stringify(app.settings),
);
const saving = ref(false);
/** Clearing the webview store takes a moment: WebKit closes its files first. */
const clearingWeb = ref(false);

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

onMounted(async () => {
  try {
    info.value = await api.appInfo();
  } catch {
    /* ignore */
  }
  if (!draft.value && app.settings) draft.value = { ...app.settings };
  if (app.authenticated && !app.account) void loadAccount();
  // The account card shows every drive's capacity, not just the active one.
  await loadProviderStates();
  for (const drive of app.providers) {
    if (drive.authenticated) {
      void loadQuota(drive.provider);
      void loadTraffic(drive.provider);
    }
  }
});

watch(
  () => app.settings,
  (value) => {
    if (value && !dirty.value) draft.value = { ...value };
  },
  { deep: true },
);

/** Real exit: the tray menu is the normal path, this is the explicit one. */
async function quitApp() {
  try {
    await api.quitApp();
  } catch (err) {
    toast(errText(err), "error");
  }
}

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

/** Opens the app log, so a failing drive login can be reported with evidence. */
async function revealLog() {
  const ok = await revealPath(info.value?.logPath);
  if (!ok) toast("找不到日志文件", "error");
}

async function revealCookie() {
  const ok = await revealPath(info.value?.cookiePath);
  if (!ok) toast("无法打开文件管理器", "error");
}

/** Logging out of one drive while the request is in flight. */
const busyDrive = ref<ProviderId | null>(null);

/**
 * One row per drive for the account card.
 *
 * Quark's own state lives on `app` (cookie), the other three in `app.providers`;
 * this folds both into the same shape so the template stays dumb.
 */
const accountRows = computed(() =>
  PROVIDERS.map((drive) => {
    const state = app.providers.find((p) => p.provider === drive.id);
    const quark = drive.id === "quark";
    return {
      id: drive.id,
      name: drive.name,
      active: drive.id === app.activeProvider,
      authenticated: quark ? app.authenticated : (state?.authenticated ?? false),
      nickname: quark ? (app.account?.nickname ?? null) : (state?.nickname ?? null),
      masked: quark ? app.maskedCookie : (state?.masked ?? null),
      quota: app.quotas[drive.id] ?? null,
      traffic: app.traffics[drive.id] ?? null,
    };
  }),
);

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

const themeOptions = [
  { value: "light", label: "浅色" },
  { value: "dark", label: "深色" },
  { value: "system", label: "跟随系统" },
];

const theme = computed({
  get: () => app.themeMode,
  set: (value: string) => setThemeMode(value as "light" | "dark" | "system"),
});
</script>

<template>
  <section class="settings">
    <header class="head">
      <div>
        <h1 class="title">设置</h1>
        <p class="sub">外观、传输与账户配置</p>
      </div>
      <button class="btn btn--solid" :disabled="!dirty || saving" @click="persist">
        保存更改
      </button>
    </header>

    <div v-if="draft" class="body">
      <!-- appearance -->
      <section class="group">
        <h2 class="group-title">外观</h2>
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
        </div>
      </section>

      <!-- transfers -->
      <section class="group">
        <h2 class="group-title">传输</h2>
        <div class="card">
          <div class="row">
            <div class="row-copy">
              <span class="row-label">下载目录</span>
              <span class="row-hint mono truncate">{{ draft.downloadDir }}</span>
            </div>
            <button class="btn btn--ghost" @click="chooseDownloadDir">选择目录</button>
          </div>
          <div class="divider" />
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
          <div class="row">
            <div class="row-copy">
              <span class="row-label">同时传输数</span>
              <span class="row-hint">超出的任务会排队，完成一个后自动开始下一个</span>
            </div>
            <Segmented
              :model-value="String(draft.concurrentTransfers)"
              :options="[
                { value: '1', label: '1' },
                { value: '3', label: '3' },
                { value: '5', label: '5' },
                { value: '8', label: '8' },
              ]"
              @update:model-value="(v: string) => draft && (draft.concurrentTransfers = Number(v))"
            />
          </div>
          <div class="divider" />
          <div class="row">
            <div class="row-copy">
              <span class="row-label">速度限制</span>
              <span class="row-hint">所有传输共享这个上限，超出的部分靠等待补回来</span>
            </div>
            <Segmented
              :model-value="String(draft.speedLimitKbps)"
              :options="[
                { value: '0', label: '不限' },
                { value: '512', label: '512K' },
                { value: '2048', label: '2M' },
                { value: '5120', label: '5M' },
                { value: '10240', label: '10M' },
              ]"
              @update:model-value="(v: string) => draft && (draft.speedLimitKbps = Number(v))"
            />
          </div>
          <div class="divider" />
          <Switch
            :model-value="draft.confirmDelete"
            label="删除前确认"
            hint="删除文件时弹出确认对话框"
            @update:model-value="(v) => draft && (draft.confirmDelete = v)"
          />
        </div>
      </section>

      <!-- background & notifications -->
      <section class="group">
        <h2 class="group-title">后台与通知</h2>
        <div class="card">
          <Switch
            :model-value="draft.closeToTray"
            label="关闭窗口时最小化到托盘"
            hint="上传 / 下载会继续在后台进行；从托盘菜单可以退出"
            @update:model-value="(v) => draft && (draft.closeToTray = v)"
          />
          <div class="divider" />
          <Switch
            :model-value="draft.notifications"
            label="系统通知"
            hint="传输完成、下载失败或收到分享链接时发送桌面通知"
            @update:model-value="(v) => draft && (draft.notifications = v)"
          />
          <div class="divider" />
          <div class="row">
            <div class="row-copy">
              <span class="row-label">退出应用</span>
              <span class="row-hint">关闭窗口只会隐藏，需要真正退出时用托盘菜单</span>
            </div>
            <button class="btn btn--ghost" @click="quitApp">退出</button>
          </div>
        </div>
      </section>

      <!-- accounts: one row per drive, since each authenticates separately -->
      <section class="group">
        <h2 class="group-title">账户</h2>
        <div class="card">
          <template v-for="(drive, index) in accountRows" :key="drive.id">
          <div class="divider" v-if="index > 0" />
          <div class="row">
            <div class="row-copy">
              <span class="row-label">
                {{ drive.name }}
                <span v-if="drive.active" class="row-badge">当前</span>
              </span>
              <span class="row-hint truncate">
                {{ drive.authenticated ? (drive.nickname ?? "已连接") : "未登录" }}
                <template v-if="drive.quota">
                  · 已用 {{ formatSize(drive.quota.used) }} / {{ formatSize(drive.quota.total) }}
                </template>
                <template v-if="drive.traffic">
                  · 下载流量剩余 {{ formatSize(drive.traffic.left) }}
                </template>
              </span>
              <span v-if="drive.masked" class="row-hint mono truncate">{{ drive.masked }}</span>
            </div>
            <div class="row-actions">
              <button
                v-if="drive.id === 'quark' && drive.authenticated"
                class="btn btn--ghost"
                @click="revealCookie"
              >
                查看文件
              </button>
              <button
                v-if="drive.authenticated"
                class="btn btn--ghost"
                :disabled="busyDrive === drive.id"
                @click="logoutDrive(drive.id)"
              >
                退出
              </button>
              <button v-else class="btn btn--solid" @click="goLoginFor(drive.id)">去登录</button>
            </div>
          </div>
          </template>
          <div class="divider" />
          <div class="row">
            <div class="row-copy">
              <span class="row-label">API 地址</span>
              <span class="row-hint">夸克网盘的接口地址，其它网盘不使用这一项</span>
            </div>
            <input v-model="draft.apiBaseUrl" class="input api-input" spellcheck="false" />
          </div>
          <div class="divider" />
          <Switch
            :model-value="draft.showHidden"
            label="显示隐藏文件"
            hint="显示以 . 开头的文件夹"
            @update:model-value="(v) => draft && (draft.showHidden = v)"
          />
        </div>
      </section>

      <!-- browser data: the escape hatch for a login page stuck in a loop -->
      <section class="group">
        <h2 class="group-title">登录与缓存</h2>
        <div class="card">
          <div class="row">
            <div class="row-copy">
              <span class="row-label">清除内置浏览器数据</span>
              <span class="row-hint">
                清掉网页登录窗口留下的 Cookie、缓存与站点存储。某个网盘的登录页一直来回跳、
                提示已登录却进不去时点这里。各网盘已保存的登录状态在本地配置文件里，不受影响。
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

      <!-- about -->
      <section class="group">
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
            <span class="about-meta">
              Tauri 2 · Vue 3 · {{ info?.platform ?? "linux" }}
            </span>
            <span class="about-path mono truncate" :title="info?.configDir">
              配置目录：{{ info?.configDir ?? "—" }}
            </span>
            <button
              v-if="info?.logPath"
              class="about-log"
              :title="info?.logPath"
              @click="revealLog"
            >
              <Icon name="list" :size="11.5" />
              <span>打开日志</span>
            </button>
          </div>
        </div>
      </section>

      <!-- diagnostics: deliberately tucked away, these are not user features -->
      <section class="group">
        <h2 class="group-title">诊断</h2>
        <div class="card">
          <div class="row">
            <div class="row-copy">
              <span class="row-label">运行日志</span>
              <span class="row-hint">控制台输出、每次前后端调用与错误，排查问题时复制给开发者</span>
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
              <span class="row-hint">网盘 HTTP 调用的请求头/体与响应头/体，查看网盘拒绝上传的原因</span>
            </div>
            <button class="btn btn--ghost" @click="app.view = 'net'">
              <Icon name="globe" :size="13" />
              <span>查看</span>
            </button>
          </div>
        </div>
      </section>
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
  max-width: 820px;
  margin: 0 auto;
  padding: 22px 24px 10px;
  box-sizing: border-box;
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
  padding: 6px 24px 30px;
  display: flex;
  flex-direction: column;
  gap: 22px;
  width: 100%;
  max-width: 820px;
  margin: 0 auto;
  box-sizing: border-box;
}
.group-title {
  margin: 0 0 8px 2px;
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 0.06em;
  text-transform: uppercase;
  color: var(--text-3);
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
.row-copy {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
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
  max-width: 46ch;
}
.row-actions {
  display: flex;
  gap: 8px;
}
.divider {
  height: 1px;
  background: var(--line);
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
}
.about-name {
  font-size: 13px;
  font-weight: 540;
}
.about-meta {
  font-size: 11.5px;
  color: var(--text-2);
}
.about-log {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  margin-top: 2px;
  padding: 0;
  border: none;
  background: none;
  color: var(--text-3);
  font-size: 10.5px;
  cursor: pointer;
}
.about-log:hover {
  color: var(--text-1);
}
.about-path {
  color: var(--text-3);
  max-width: 54ch;
}
</style>
