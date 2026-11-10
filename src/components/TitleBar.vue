<script setup lang="ts">
/**
 * Custom window chrome.
 *
 * The main window runs undecorated (`decorations: false`) so the UI owns the
 * top strip. Tauri's drag shim handles moving the window: any element carrying
 * `data-tauri-drag-region="deep"` drags, while real `<button>` children stay
 * clickable, and a double click toggles maximise.
 */
import { onBeforeUnmount, onMounted, ref } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";

import Icon from "@/components/Icon.vue";
import { BRAND } from "@/lib/brand";
import { isDark } from "@/store/app";
import { IS_TAURI } from "@/lib/host";

const win = IS_TAURI ? getCurrentWindow() : null;
const maximized = ref(false);
let unlisten: (() => void) | undefined;

async function syncMaximized() {
  if (!win) return;
  try {
    maximized.value = await win.isMaximized();
  } catch {
    /* the window manager may refuse; keep the last known state */
  }
}

function minimize() {
  void win?.minimize().catch(() => {});
}

async function toggleMaximize() {
  if (!win) return;
  try {
    await win.toggleMaximize();
  } catch {
    /* ignore */
  }
  await syncMaximized();
}

function close() {
  void win?.close().catch(() => {});
}

onMounted(async () => {
  await syncMaximized();
  try {
    unlisten = await win?.onResized(() => void syncMaximized());
  } catch {
    /* not fatal */
  }
});

onBeforeUnmount(() => {
  unlisten?.();
  unlisten = undefined;
});
</script>

<template>
  <header class="titlebar" data-tauri-drag-region="deep">
    <div class="brand">
      <!-- Dark theme gets the white/purple mark; on light surfaces the white
           artwork would disappear, so it switches to the ink variant. -->
      <img
        class="brand-logo"
        :src="isDark ? '/mark.png' : '/mark-ink.png'"
        alt=""
        draggable="false"
      />
      <span class="brand-name">{{ BRAND.name }}</span>
      <span class="brand-sub">{{ BRAND.latin }}</span>
    </div>

    <div class="spacer" />

    <div class="controls">
      <button class="wbtn" type="button" title="最小化" @click="minimize">
        <Icon name="window-minimize" :size="14" :stroke="1.6" />
      </button>
      <button
        class="wbtn"
        type="button"
        :title="maximized ? '还原' : '最大化'"
        @click="toggleMaximize"
      >
        <Icon :name="maximized ? 'window-restore' : 'window-maximize'" :size="13" :stroke="1.6" />
      </button>
      <button class="wbtn wbtn--close" type="button" title="关闭" @click="close">
        <Icon name="window-close" :size="14" :stroke="1.6" />
      </button>
    </div>
  </header>
</template>

<style scoped>
.titlebar {
  display: flex;
  align-items: center;
  flex: none;
  height: 40px;
  padding-left: 16px;
  background: var(--bg);
  user-select: none;
  /* The strip itself is the drag handle; children opt in via "deep". */
  position: relative;
  z-index: 60;
}
.brand {
  display: flex;
  align-items: center;
  gap: 8px;
  pointer-events: none;
}
.brand-logo {
  width: 20px;
  height: 20px;
  /* The mark is a coloured disc on transparency — no radius or backing plate,
     otherwise it reads as a tile. */
  object-fit: contain;
}
.brand-name {
  font-size: 12.4px;
  font-weight: 560;
  letter-spacing: -0.005em;
  color: var(--text);
}
.brand-sub {
  font-size: 10.5px;
  color: var(--text-3);
  letter-spacing: 0.02em;
}
.spacer {
  flex: 1;
  height: 100%;
}
.controls {
  display: flex;
  align-items: stretch;
  height: 100%;
}
.wbtn {
  display: grid;
  place-items: center;
  width: 44px;
  height: 100%;
  color: var(--text-2);
  transition: background-color var(--dur-1) var(--ease-out),
    color var(--dur-1) var(--ease-out);
}
.wbtn:hover {
  background: var(--bg-hover);
  color: var(--text);
}
.wbtn:active {
  background: var(--bg-active);
}
/* Monochrome "danger": invert on hover so it reads without a red accent. */
.wbtn--close:hover {
  background: var(--fill);
  color: var(--on-fill);
}
</style>
