<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { AnimatePresence, motion } from "motion-v";
import Icon from "@/components/Icon.vue";
import {
  app,
  activeProviderState,
  activeTransfers,
  goLoginFor,
  loadQuota,
  loadTraffic,
  logoutProvider,
  setActiveProvider,
  setThemeMode,
  toast,
  type View,
} from "@/store/app";
import { errText, formatSize } from "@/lib/format";
import {
  barTransition,
  itemEnter,
  slideTo,
  panelHidden,
  panelLeaveTransition,
  panelShown,
  panelTransition,
  popHidden,
  popLeaveTransition,
  popShown,
  popTransition,
  pressDown,
  pressTransition,
} from "@/lib/motion";
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
  // Creating a share is not universal either: 139 has no endpoint for it, so
  // the manager page would only ever be empty there.
  if (activeProviderState.value?.capabilities.shareCreate) {
    list.push({ view: "myShares", label: "我的分享", icon: "share" });
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

/** Where the travelling highlight has to sit, in rows. -1 parks it off-screen
 *  for the one frame where the current section is not in the rail. */
const navIndex = computed(() => items.value.findIndex((item) => item.view === app.view));

/* The rails below are driven by `slideTo` rather than motion-v: a travelling
   element needs its final transform *held* (WAAPI `fill: "both"`), because
   WebKitGTK repaints once from the layout position if the transform is ever
   briefly absent — see `slideTo` in `lib/motion.ts` for the full explanation. */
const navThumbEl = ref<HTMLElement | null>(null);
const NAV_ROW = 36;

const themeThumbEl = ref<HTMLElement | null>(null);


/** Theme switch, in the order the segments are drawn. */
const themeOptions = [
  { value: "light", title: "浅色", icon: "sun" },
  { value: "system", title: "跟随系统", icon: "palette" },
  { value: "dark", title: "深色", icon: "moon" },
] as const;

const themeIndex = computed(() => {
  const at = themeOptions.findIndex((option) => option.value === app.themeMode);
  return at < 0 ? 0 : at;
});

/* Park both rails where they belong on first paint, then hand every change to
   `slideTo` — see the note above `navThumbEl`. */
onMounted(() => {
  if (navThumbEl.value) navThumbEl.value.style.transform = `translateY(${navIndex.value * NAV_ROW}px)`;
  if (themeThumbEl.value) themeThumbEl.value.style.transform = `translateX(${themeIndex.value * 100}%)`;
});

watch(navIndex, (next, previous) => {
  slideTo(navThumbEl.value, `translateY(${previous * NAV_ROW}px)`, `translateY(${next * NAV_ROW}px)`);
});

watch(themeIndex, (next, previous) => {
  slideTo(themeThumbEl.value, `translateX(${previous * 100}%)`, `translateX(${next * 100}%)`, 0.26);
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

/* The switcher used to close only when you clicked the button again or picked a
   drive, so it stayed on screen while you clicked anywhere else — which reads
   as the menu being stuck. Mirror the account bubble: click-outside and Escape. */
function onSwitcherOutside(event: PointerEvent) {
  const target = event.target as Node | null;
  if (target && !(target as HTMLElement).closest?.(".switcher")) switcherOpen.value = false;
}
function onSwitcherKey(event: KeyboardEvent) {
  if (event.key === "Escape") switcherOpen.value = false;
}

watch(switcherOpen, (open) => {
  if (open) {
    // Only one popup at a time: two glass panels stacked on the sidebar read as
    // a glitch, and the second one has nowhere to go.
    accountOpen.value = false;
    window.addEventListener("pointerdown", onSwitcherOutside, true);
    window.addEventListener("keydown", onSwitcherKey);
  } else {
    window.removeEventListener("pointerdown", onSwitcherOutside, true);
    window.removeEventListener("keydown", onSwitcherKey);
  }
});

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

/** Monthly download allowance of the active drive, when it meters one. */
const traffic = computed(() => app.traffics[app.activeProvider] ?? null);
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
    if (activeProviderState.value?.authenticated) {
      void loadQuota();
      void loadTraffic();
    }
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
    switcherOpen.value = false;
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
  window.removeEventListener("pointerdown", onSwitcherOutside, true);
  window.removeEventListener("keydown", onSwitcherKey);
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
    toast(errText(err), "error");
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
      <motion.button
        class="switcher-btn"
        :aria-expanded="switcherOpen"
        :while-press="pressDown"
        :transition="pressTransition"
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
      </motion.button>

      <!-- The menu unrolls from zero height rather than scaling open: a
           shadowed panel that scales is re-rasterised every frame, and the last
           frame of the shrink is what read as the close twitching. The clip
           wrapper carries the height, the inner box carries the border and
           shadow, and the extra padding around it gives that shadow room to
           paint without being cut off. -->
      <AnimatePresence>
        <motion.div
          v-if="switcherOpen"
          class="switcher-clip"
          :initial="panelHidden"
          :animate="panelShown"
          :exit="{ height: 0, opacity: 0, transition: panelLeaveTransition }"
          :transition="panelTransition"
        >
          <div class="switcher-menu">
            <motion.button
              v-for="(p, i) in PROVIDERS"
              :key="p.id"
              class="switcher-item"
              :class="{ 'switcher-item--active': p.id === app.activeProvider }"
              v-bind="itemEnter(i)"
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
            </motion.button>
          </div>
        </motion.div>
      </AnimatePresence>
    </div>

    <nav class="nav">
      <!-- One highlight for the whole rail: it travels to the section you pick
           instead of blinking there. Step is the row height plus the 2px gap. -->
      <span
        ref="navThumbEl"
        class="nav-thumb"
        :style="{ opacity: navIndex < 0 ? 0 : 1 }"
      />
      <motion.button
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
      </motion.button>
    </nav>

    <div class="sidebar-foot">
      <!-- Capacity of the drive on screen. Hidden for drives without a quota
           endpoint and while the request is in flight. -->
      <div v-if="quota" class="quota" :title="quotaLabel">
        <div class="quota-track">
          <motion.div
            class="quota-fill"
            :class="{ 'quota-fill--full': quotaPercent >= 90 }"
            :animate="{ width: `${quotaPercent}%` }"
            :transition="barTransition"
          />
        </div>
        <span class="quota-label truncate">{{ quotaLabel }}</span>
      </div>
      <!-- Monthly download allowance; only drives that meter downloads show it. -->
      <div
        v-if="traffic"
        class="quota quota--traffic"
        :title="`本月还能下载 ${formatSize(traffic.left)}（分享提取剩余 ${formatSize(traffic.share)}）`"
      >
        <span class="quota-label truncate" :class="{ 'quota-label--low': traffic.left < 1024 ** 3 }">
          下载流量剩余 {{ formatSize(traffic.left) }}
        </span>
      </div>

      <div class="account-area">
        <AnimatePresence>
          <motion.div
            v-if="accountOpen"
            class="account-pop"
            role="dialog"
            aria-label="账号"
            :initial="popHidden"
            :animate="popShown"
            :exit="{ ...popHidden, transition: popLeaveTransition }"
            :transition="popTransition"
          >
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
          </motion.div>
        </AnimatePresence>

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
        <span
          ref="themeThumbEl"
          class="theme-thumb"
        />
        <motion.button
          v-for="option in themeOptions"
          :key="option.value"
          class="theme-btn"
          :class="{ 'theme-btn--active': app.themeMode === option.value }"
          :title="option.title"
          :while-press="pressDown"
          :transition="pressTransition"
          @click="setThemeMode(option.value)"
        >
          <Icon :name="option.icon" :size="15" />
        </motion.button>
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
.switcher-clip {
  /* Carries the height animation; the inner box carries the chrome. The 8px
     side padding and 40px bottom padding exist only so `--shadow-lg` (which
     reaches ~36px below the box) is inside the clip box instead of being cut
     off — `overflow: hidden` is what makes a height animation possible at all. */
  position: absolute;
  z-index: 30;
  top: calc(100% + 2px);
  left: -8px;
  right: -8px;
  padding: 8px 8px 40px;
  overflow: hidden;
  pointer-events: none;
  /* `content-box` is load-bearing: the app is `border-box` globally, and with
     `border-box` a `height: 0` animation can never get below the 48px of
     vertical padding — the first 8px of the menu would stay visible at rest. */
  box-sizing: content-box;
}
.switcher-menu {
  padding: 4px;
  border-radius: var(--radius-md);
  border: 1px solid var(--line);
  background: var(--bg-raised);
  box-shadow: var(--shadow-lg);
  pointer-events: auto;
  /* Animated by motion-v as a height change (`panelHidden` → `panelShown`),
     which is why the motion lives on `.switcher-clip` and not here: scaling a
     shadowed panel re-rasterises the blur every frame, and the last frame of a
     shrink is what read as the menu twitching on close.
     Nothing motion-related belongs here: motion-v writes `transform`/`opacity`
     inline and manages `will-change` itself, removing it when it settles.
     Do NOT add `transform` or `will-change` back. */
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
  /* Width is animated by motion-v (`barTransition`); only the colour is CSS. */
  transition: background-color var(--dur-2) var(--ease-out);
}
.quota-fill--full {
  background: #e0554f;
}
/* Under 1 GB left is when a download starts failing, so say it in red. */
.quota-label--low {
  color: #e0554f;
}
.quota--traffic {
  padding-top: 0;
  padding-bottom: 10px;
}
.quota-label {
  font-size: 10.4px;
  color: var(--text-3);
  font-variant-numeric: tabular-nums;
}

.nav {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
/* The travelling highlight. Absolutely placed and behind the buttons, so the
   rail keeps one moving surface instead of one background per row appearing and
   disappearing. Height matches `.nav-item` (34px); the step used by `navIndex`
   in the script is 34 + the 2px gap above. */
.nav-thumb {
  /* WebKitGTK drops a compositing layer the moment the last `will-change`
     disappears, and motion clears its inline `will-change` as soon as an
     animation finishes. The repaint that follows uses the element's *layout*
     position, so a travelled slider flashes back to its slot of origin for a
     frame before the final transform is re-applied — the "B → A → B" snap.
     Declaring the hint in CSS keeps the layer alive after motion is done. */
  will-change: transform;

  position: absolute;
  z-index: 0;
  top: 0;
  left: 0;
  right: 0;
  height: 34px;
  border-radius: var(--radius-sm);
  background: var(--bg);
  /* A ring, not a blurred shadow: this element *travels*, and a blur has to be
     re-rasterised on every frame of that travel. On WebKitGTK the result is a
     visible wobble in the ring — the "抽搐" that made the highlight feel broken
     even though the motion itself was smooth. An inset ring is painted once and
     composited. */
  box-shadow: inset 0 0 0 1px var(--line);
  pointer-events: none;
}
:root[data-theme="dark"] .nav-thumb {
  background: rgba(255, 255, 255, 0.07);
}
.nav-item {
  position: relative;
  z-index: 1;
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
    color var(--dur-1) var(--ease-out);
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
  /* Background and shadow live on `.nav-thumb`, which slides between rows. */
  color: var(--text);
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
  /* Same rule as `.switcher-menu`: motion-v owns the animation, so no
     `will-change`, no `contain`, no static transform here. */
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
  /* A grid rather than a flex row with gaps: the thumb has to be exactly one
     third wide for `x: index * 100%` to land on a segment, and a gap would make
     each segment narrower than a third. */
  position: relative;
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  padding: 3px;
  border-radius: var(--radius-sm);
  background: var(--bg);
  box-shadow: inset 0 0 0 1px var(--line);
}
:root[data-theme="dark"] .theme-row {
  background: rgba(255, 255, 255, 0.04);
}
/* Slides to the selected mode instead of blinking there — `thumbTransition`. */
.theme-thumb {
  /* WebKitGTK drops a compositing layer the moment the last `will-change`
     disappears, and motion clears its inline `will-change` as soon as an
     animation finishes. The repaint that follows uses the element's *layout*
     position, so a travelled slider flashes back to its slot of origin for a
     frame before the final transform is re-applied — the "B → A → B" snap.
     Declaring the hint in CSS keeps the layer alive after motion is done. */
  will-change: transform;

  position: absolute;
  z-index: 0;
  top: 3px;
  bottom: 3px;
  left: 3px;
  /* An absolutely positioned box resolves percentages against the ancestor's
     *padding* box, so the 3px of padding on each side has to come back out —
     otherwise the thumb is 2px too wide and each step drifts another 2px, which
     is why the middle slot never lined up and the last one overflowed. */
  width: calc((100% - 6px) / 3);
  border-radius: 6px;
  background: var(--bg-active);
  pointer-events: none;
}
.theme-btn {
  position: relative;
  z-index: 1;
  display: grid;
  place-items: center;
  height: 26px;
  border-radius: 6px;
  color: var(--text-3);
  transition: color var(--dur-2) var(--ease-out);
}
.theme-btn:hover {
  color: var(--text-2);
}
.theme-btn--active {
  color: var(--text);
}
</style>
