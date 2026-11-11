<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from "vue";
import Icon from "@/components/Icon.vue";
import {
  app,
  activeProviderState,
  activeTransfers,
  goLoginFor,
  loadQuota,
  logoutProvider,
  setActiveProvider,
  setThemeMode,
  toast,
  type View,
} from "@/store/app";
import { formatSize } from "@/lib/format";
import { pop } from "@/lib/motion";
import { PROVIDERS, type ProviderId } from "@/lib/types";
import type { IconName } from "@/lib/icons";

/**
 * Sidebar entries. "分享链接" only makes sense for a drive whose API can open
 * someone else's share, so it is dropped when the active drive cannot.
 */
const items = computed<{ view: View; label: string; icon: IconName }[]>(() => {
  const list: { view: View; label: string; icon: IconName }[] = [
    { view: "files", label: "我的文件", icon: "folder" },
  ];
  if (activeProviderState.value?.capabilities.shareOpen !== false) {
    list.push({ view: "share", label: "分享链接", icon: "link" });
  }
  // The recycle bin is only offered where the drive exposes one; 百度 and 139
  // reject every restore call, so the entry would be a dead end.
  if (activeProviderState.value?.capabilities.recycle) {
    list.push({ view: "recycle", label: "回收站", icon: "trash" });
  }
  list.push(
    { view: "transfers", label: "传输列表", icon: "arrow-up-down" },
    { view: "settings", label: "设置", icon: "settings" },
  );
  return list;
});

/**
 * Account row of the *active* drive.
 *
 * This used to read `app.authenticated` / `app.account`, which only ever
 * describe 夸克 — so the footer kept showing the Quark account after switching
 * to another drive. `activeProviderState` is the per-drive source of truth.
 */
const signedIn = computed(() => activeProviderState.value?.authenticated === true);

const accountName = computed(() => {
  const state = activeProviderState.value;
  if (!state?.authenticated) return "未登录";
  return state.nickname?.trim() || currentProvider.value.name;
});

const accountSub = computed(() => {
  if (!signedIn.value) return "点击登录";
  return activeProviderState.value?.source === "env" ? "环境变量 Cookie" : "已连接";
});

/**
 * Every drive reports a profile through `auth_states`; the avatar arrives as an
 * inlined `data:` URL because the CSP blocks remote image hosts. Quark is the
 * one exception that can also come from the cached `account` call.
 */
const avatarSrc = computed(
  () =>
    activeProviderState.value?.avatar ??
    (app.activeProvider === "quark" ? (app.account?.avatar ?? null) : null),
);

function go(view: View) {
  app.view = view;
}

/** Drive switcher: open state + the label of the current drive. */
const switcherOpen = ref(false);

const currentProvider = computed(
  () => PROVIDERS.find((p) => p.id === app.activeProvider) ?? PROVIDERS[0],
);

/**
 * Capacity bar of the active drive.
 *
 * `null` from the store means the drive has no quota endpoint (123 today), so
 * the row is hidden instead of showing a permanently empty bar.
 */
const quota = computed(() => app.quotas[app.activeProvider] ?? null);
const quotaPercent = computed(() => {
  const q = quota.value;
  if (!q || q.total <= 0) return 0;
  return Math.min(100, Math.max(0, (q.used / q.total) * 100));
});
/** Used space first: that is the number a user acts on when a drive is full. */
const quotaLabel = computed(() => {
  const q = quota.value;
  if (!q) return "";
  return `${formatSize(q.used)} / ${formatSize(q.total)}`;
});

// The switcher is the only way to change drive, so refetch when it changes.
watch(
  () => [app.activeProvider, activeProviderState.value?.authenticated],
  () => {
    if (activeProviderState.value?.authenticated) void loadQuota();
  },
  { immediate: true },
);

/**
 * Account bubble: a small popover with the account details and the actions a
 * user actually needs there (settings, sign out). Clicking the row used to jump
 * straight to Settings, which made signing out a three-click detour.
 */
const accountOpen = ref(false);
const accountBusy = ref(false);

function toggleAccount() {
  accountOpen.value = !accountOpen.value;
}

function closeAccount() {
  accountOpen.value = false;
}

function onAccountOutside(event: PointerEvent) {
  const target = event.target as Node | null;
  if (target && !(target as HTMLElement).closest?.(".account-area")) closeAccount();
}
function onAccountKey(event: KeyboardEvent) {
  if (event.key === "Escape") closeAccount();
}

watch(accountOpen, (open) => {
  if (open) {
    window.addEventListener("pointerdown", onAccountOutside, true);
    window.addEventListener("keydown", onAccountKey);
  } else {
    window.removeEventListener("pointerdown", onAccountOutside, true);
    window.removeEventListener("keydown", onAccountKey);
  }
});

onBeforeUnmount(() => {
  window.removeEventListener("pointerdown", onAccountOutside, true);
  window.removeEventListener("keydown", onAccountKey);
});

/** The drive's capacity, shown inside the bubble as well as on the bar. */
const quotaText = computed(() => (quota.value ? quotaLabel.value : "未提供"));

async function signOut() {
  const id = app.activeProvider;
  accountBusy.value = true;
  try {
    await logoutProvider(id);
    toast(`已退出${currentProvider.value.name}`, "success");
    closeAccount();
  } catch (err) {
    toast(String(err), "error");
  } finally {
    accountBusy.value = false;
  }
}

function openSettings() {
  closeAccount();
  app.view = "settings";
}

function openLogin() {
  closeAccount();
  goLoginFor(app.activeProvider);
}

/** Per-drive login dot: green when connected, hollow when not. */
function providerState(id: ProviderId) {
  return app.providers.find((p) => p.provider === id) ?? null;
}

function pick(id: ProviderId) {
  switcherOpen.value = false;
  setActiveProvider(id);
  // Switching drives always lands on the file list, even when that drive is not
  // signed in: the list then shows "未登录，请先登录" with its own 去登录 button.
  // Jumping straight to the login screen made the flow feel unpredictable.
  if (app.view !== "files") app.view = "files";
}
</script>

<template>
  <aside class="sidebar">
    <!-- Drive switcher. The brand lives in the title bar, so this is purely
         "which cloud am I looking at", with one dot per drive. -->
    <div class="switcher">
      <button
        class="switcher-btn"
        :aria-expanded="switcherOpen"
        @click="switcherOpen = !switcherOpen"
      >
        <img class="switcher-logo" :src="currentProvider.logo" alt="" draggable="false" />
        <span class="switcher-copy">
          <span class="switcher-name truncate">{{ currentProvider.name }}</span>
          <span class="switcher-sub truncate">
            {{
              activeProviderState?.authenticated
                ? (activeProviderState.nickname ?? "已连接")
                : "未登录"
            }}
          </span>
        </span>
        <Icon name="chevron-down" :size="14" class="switcher-chevron" />
      </button>

      <Transition :css="false" v-bind="pop">
        <div v-if="switcherOpen" class="switcher-menu">
          <button
            v-for="p in PROVIDERS"
            :key="p.id"
            class="switcher-item"
            :class="{ 'switcher-item--active': p.id === app.activeProvider }"
            @click="pick(p.id)"
          >
            <img class="switcher-logo switcher-logo--sm" :src="p.logo" alt="" draggable="false" />
            <span class="switcher-item-copy">
              <span class="truncate">{{ p.name }}</span>
              <span class="switcher-item-sub truncate">
                {{
                  providerState(p.id)?.authenticated
                    ? (providerState(p.id)?.nickname ?? "已连接")
                    : "未登录"
                }}
              </span>
            </span>
            <span
              class="switcher-dot"
              :class="{ 'switcher-dot--on': providerState(p.id)?.authenticated }"
            />
          </button>
        </div>
      </Transition>
    </div>

    <nav class="nav">
      <button
        v-for="item in items"
        :key="item.view"
        class="nav-item"
        :class="{ 'nav-item--active': app.view === item.view }"
        @click="go(item.view)"
      >
        <Icon :name="item.icon" :size="16.5" :stroke="1.6" />
        <span class="nav-label">{{ item.label }}</span>
        <span v-if="item.view === 'transfers' && activeTransfers" class="nav-badge">
          {{ activeTransfers }}
        </span>
      </button>
    </nav>

    <div class="sidebar-foot">
      <!-- Capacity of the drive on screen. Hidden for drives without a quota
           endpoint and while the request is in flight. -->
      <div v-if="quota" class="quota" :title="quotaLabel">
        <div class="quota-track">
          <div
            class="quota-fill"
            :class="{ 'quota-fill--full': quotaPercent >= 90 }"
            :style="{ width: `${quotaPercent}%` }"
          />
        </div>
        <span class="quota-label truncate">{{ quotaLabel }}</span>
      </div>

      <div class="account-area">
        <Transition :css="false" v-bind="pop">
          <div v-if="accountOpen" class="account-pop" role="dialog" aria-label="账号">
            <div class="account-pop-head">
              <img class="account-pop-logo" :src="currentProvider.logo" alt="" draggable="false" />
              <span class="account-pop-copy">
                <span class="account-pop-drive truncate">{{ currentProvider.name }}</span>
                <span class="account-pop-name truncate">{{ accountName }}</span>
              </span>
            </div>
            <dl class="account-pop-facts">
              <div class="account-pop-fact">
                <dt>状态</dt>
                <dd>{{ signedIn ? "已登录" : "未登录" }}</dd>
              </div>
              <div class="account-pop-fact">
                <dt>容量</dt>
                <dd>{{ signedIn ? quotaText : "—" }}</dd>
              </div>
            </dl>
            <div class="account-pop-actions">
              <button class="account-pop-btn" @click="openSettings">
                <Icon name="settings" :size="14" />
                <span>设置</span>
              </button>
              <button
                v-if="signedIn"
                class="account-pop-btn account-pop-btn--danger"
                :disabled="accountBusy"
                @click="signOut"
              >
                <Icon name="logout" :size="14" />
                <span>退出账号</span>
              </button>
              <button v-else class="account-pop-btn" @click="openLogin">
                <Icon name="user" :size="14" />
                <span>去登录</span>
              </button>
            </div>
          </div>
        </Transition>

        <button
          class="account"
          :aria-expanded="accountOpen"
          @click="toggleAccount"
        >
          <span class="avatar" :class="{ 'avatar--on': signedIn }">
            <img v-if="avatarSrc" :src="avatarSrc" alt="" draggable="false" />
            <span v-else class="avatar-initial">{{ signedIn ? accountName.slice(0, 1) : "?" }}</span>
          </span>
          <span class="account-copy">
            <span class="account-title truncate">{{ accountName }}</span>
            <span class="account-sub truncate">{{ accountSub }}</span>
          </span>
          <Icon name="chevron-up" :size="14" class="account-chevron" />
        </button>
      </div>

      <div class="theme-row">
        <button
          class="theme-btn"
          :class="{ 'theme-btn--active': app.themeMode === 'light' }"
          title="浅色"
          @click="setThemeMode('light')"
        >
          <Icon name="sun" :size="15" />
        </button>
        <button
          class="theme-btn"
          :class="{ 'theme-btn--active': app.themeMode === 'system' }"
          title="跟随系统"
          @click="setThemeMode('system')"
        >
          <Icon name="palette" :size="15" />
        </button>
        <button
          class="theme-btn"
          :class="{ 'theme-btn--active': app.themeMode === 'dark' }"
          title="深色"
          @click="setThemeMode('dark')"
        >
          <Icon name="moon" :size="15" />
        </button>
      </div>
    </div>
  </aside>
</template>

<style scoped>
.sidebar {
  display: flex;
  flex-direction: column;
  width: var(--sidebar-w);
  flex: none;
  padding: 10px 12px 12px;
  background: var(--bg-sunken);
  border-right: 1px solid var(--line);
  user-select: none;
  transition: background-color var(--dur-3) var(--ease-out),
    border-color var(--dur-3) var(--ease-out);
}
/* ---------------------------- drive switcher ---------------------------- */
.switcher {
  position: relative;
  margin-bottom: 10px;
}
.switcher-btn {
  display: flex;
  align-items: center;
  gap: 9px;
  width: 100%;
  padding: 7px 8px;
  border-radius: var(--radius-sm);
  border: 1px solid var(--line);
  background: var(--bg);
  text-align: left;
  transition: background-color var(--dur-1) var(--ease-out),
    border-color var(--dur-1) var(--ease-out);
}
.switcher-btn:hover {
  background: var(--bg-hover);
  border-color: var(--line-strong);
}
.switcher-logo {
  flex: none;
  width: 22px;
  height: 22px;
  object-fit: contain;
}
.switcher-logo--sm {
  width: 18px;
  height: 18px;
}
.switcher-copy {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-width: 0;
  line-height: 1.25;
}
.switcher-name {
  font-size: 12.6px;
  font-weight: 560;
  color: var(--text);
}
.switcher-sub {
  font-size: 10.8px;
  color: var(--text-3);
}
.switcher-chevron {
  flex: none;
  color: var(--text-3);
}
.switcher-menu {
  position: absolute;
  z-index: 30;
  top: calc(100% + 4px);
  left: 0;
  right: 0;
  padding: 4px;
  border-radius: var(--radius-md);
  border: 1px solid var(--line);
  background: var(--bg-raised);
  box-shadow: var(--shadow-lg);
  /* WebKitGTK re-rasterises a scaled, shadowed layer on every frame, which
     reads as the menu (and the sidebar under it) flickering. Keeping a stable
     identity transform and asking for the layer up front stops that. */
  transform: translateY(0);
  will-change: opacity, transform;
  /* Keep the popup's paint work out of the sidebar's layer. */
  contain: layout paint;
}
.switcher-item {
  display: flex;
  align-items: center;
  gap: 9px;
  width: 100%;
  padding: 6px 7px;
  border-radius: var(--radius-xs);
  text-align: left;
  transition: background-color var(--dur-1) var(--ease-out);
}
.switcher-item:hover {
  background: var(--bg-hover);
}
.switcher-item--active {
  background: var(--bg-active);
}
.switcher-item-copy {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-width: 0;
  line-height: 1.25;
  font-size: 12.4px;
}
.switcher-item-sub {
  font-size: 10.5px;
  color: var(--text-3);
}
.switcher-dot {
  flex: none;
  width: 6px;
  height: 6px;
  border-radius: 50%;
  border: 1px solid var(--line-strong);
}
.switcher-dot--on {
  background: #22b877;
  border-color: transparent;
}

.quota {
  display: flex;
  flex-direction: column;
  gap: 5px;
  padding: 2px 2px 8px;
}
.quota-track {
  height: 3px;
  border-radius: 999px;
  background: var(--bg-active);
  overflow: hidden;
}
.quota-fill {
  height: 100%;
  border-radius: 999px;
  background: var(--text-3);
  transition: width var(--dur-3) var(--ease-out), background-color var(--dur-2) var(--ease-out);
}
.quota-fill--full {
  background: #e0554f;
}
.quota-label {
  font-size: 10.4px;
  color: var(--text-3);
  font-variant-numeric: tabular-nums;
}

.nav {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.nav-item {
  position: relative;
  display: flex;
  align-items: center;
  gap: 10px;
  height: 34px;
  padding: 0 10px;
  border-radius: var(--radius-sm);
  font-size: 12.8px;
  font-weight: 490;
  color: var(--text-2);
  transition: background-color var(--dur-2) var(--ease-out),
    color var(--dur-1) var(--ease-out), box-shadow var(--dur-2) var(--ease-out);
}
.nav-item:hover:not(:disabled) {
  background: var(--bg-hover);
  color: var(--text);
}
/* No `scale()` on press — see docs/tauri-webkitgtk-flicker.md. */
.nav-item:active:not(:disabled) {
  background: var(--bg-active);
}
.nav-item--active {
  color: var(--text);
  background: var(--bg);
  box-shadow: var(--shadow-sm), 0 0 0 1px var(--line);
}
:root[data-theme="dark"] .nav-item--active {
  background: rgba(255, 255, 255, 0.07);
  box-shadow: 0 0 0 1px var(--line);
}
.nav-item:disabled {
  opacity: 0.42;
  pointer-events: none;
}
.nav-label {
  flex: 1;
  text-align: left;
}
.nav-badge {
  min-width: 18px;
  height: 18px;
  padding: 0 5px;
  border-radius: 999px;
  display: grid;
  place-items: center;
  font-size: 10.5px;
  font-weight: 600;
  color: var(--on-fill);
  background: var(--fill);
  font-variant-numeric: tabular-nums;
}
.sidebar-foot {
  margin-top: auto;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
/* ---------------------------- account bubble ---------------------------- */
.account-area {
  position: relative;
}
.account-pop {
  position: absolute;
  z-index: 30;
  bottom: calc(100% + 6px);
  left: 0;
  right: 0;
  padding: 10px;
  border-radius: var(--radius-md);
  border: 1px solid var(--line);
  background: var(--bg-raised);
  box-shadow: var(--shadow-lg);
  /* Same WebKitGTK workaround as the drive switcher: a stable identity
     transform plus a layer hint keeps the popup from re-rasterising. */
  transform: translateY(0);
  will-change: opacity, transform;
  contain: layout paint;
}
.account-pop-head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 2px 2px 9px;
  border-bottom: 1px solid var(--line);
}
.account-pop-logo {
  flex: none;
  width: 20px;
  height: 20px;
  object-fit: contain;
}
.account-pop-copy {
  display: flex;
  flex-direction: column;
  min-width: 0;
  line-height: 1.3;
}
.account-pop-drive {
  font-size: 12px;
  font-weight: 560;
  color: var(--text);
}
.account-pop-name {
  font-size: 10.6px;
  color: var(--text-3);
}
.account-pop-facts {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin: 8px 0 9px;
  padding: 0 2px;
}
.account-pop-fact {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 8px;
  font-size: 11px;
}
.account-pop-fact dt {
  color: var(--text-3);
}
.account-pop-fact dd {
  color: var(--text-2);
  font-variant-numeric: tabular-nums;
}
.account-pop-actions {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.account-pop-btn {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  height: 28px;
  padding: 0 7px;
  border-radius: var(--radius-xs);
  font-size: 12px;
  color: var(--text-2);
  text-align: left;
  transition: background-color var(--dur-1) var(--ease-out),
    color var(--dur-1) var(--ease-out);
}
.account-pop-btn:hover:not(:disabled) {
  background: var(--bg-hover);
  color: var(--text);
}
.account-pop-btn:disabled {
  opacity: 0.5;
}
.account-pop-btn--danger:hover:not(:disabled) {
  color: #e0554f;
}
.account {
  display: flex;
  align-items: center;
  gap: 9px;
  padding: 8px 9px;
  border-radius: var(--radius-sm);
  text-align: left;
  transition: background-color var(--dur-1) var(--ease-out);
}
.account:hover {
  background: var(--bg-hover);
}
.avatar {
  position: relative;
  flex: none;
  display: grid;
  place-items: center;
  width: 26px;
  height: 26px;
  border-radius: 50%;
  overflow: hidden;
  background: var(--bg-active);
  color: var(--text-2);
  font-size: 11px;
  font-weight: 600;
  box-shadow: inset 0 0 0 1px var(--line);
  transition: box-shadow var(--dur-2) var(--ease-out),
    background-color var(--dur-2) var(--ease-out);
}
.avatar--on {
  color: var(--on-fill);
  background: var(--fill);
}
.avatar img {
  width: 100%;
  height: 100%;
  object-fit: cover;
}
.avatar-initial {
  line-height: 1;
  letter-spacing: 0;
}
.account-copy {
  display: flex;
  flex-direction: column;
  min-width: 0;
  line-height: 1.3;
}
.account-title {
  font-size: 12px;
  font-weight: 540;
}
.account-sub {
  font-size: 10.5px;
  color: var(--text-3);
}
.account-chevron {
  color: var(--text-3);
  margin-left: auto;
}
.theme-row {
  display: flex;
  gap: 2px;
  padding: 3px;
  border-radius: var(--radius-sm);
  background: var(--bg);
  box-shadow: inset 0 0 0 1px var(--line);
}
:root[data-theme="dark"] .theme-row {
  background: rgba(255, 255, 255, 0.04);
}
.theme-btn {
  flex: 1;
  display: grid;
  place-items: center;
  height: 26px;
  border-radius: 6px;
  color: var(--text-3);
  transition: background-color var(--dur-2) var(--ease-out),
    color var(--dur-2) var(--ease-out);
}
.theme-btn:hover {
  color: var(--text-2);
}
.theme-btn--active {
  color: var(--text);
  background: var(--bg-active);
}
</style>
