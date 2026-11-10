<script setup lang="ts">
import { computed, onMounted, ref } from "vue";

import Sidebar from "@/components/Sidebar.vue";
import TitleBar from "@/components/TitleBar.vue";
import ResizeFrame from "@/components/ResizeFrame.vue";
import Toasts from "@/components/Toasts.vue";
import Icon from "@/components/Icon.vue";
import FilesView from "@/views/FilesView.vue";
import TransfersView from "@/views/TransfersView.vue";
import SettingsView from "@/views/SettingsView.vue";
import ShareView from "@/views/ShareView.vue";
import LoginView from "@/views/LoginView.vue";
import { BRAND } from "@/lib/brand";
import { IS_TAURI } from "@/lib/host";
import { app, bootstrap, toast } from "@/store/app";

const booting = ref(true);
const bootFailed = ref(false);

const currentView = computed(() => {
  const map = {
    files: FilesView,
    share: ShareView,
    transfers: TransfersView,
    settings: SettingsView,
    login: LoginView,
  } as const;
  return map[app.view];
});

// The sidebar is the only place to pick a drive, so it is always visible: with
// the drive chooser gone from the login screen, hiding the sidebar there would
// leave an account with no signed-in drive no way to reach another one.
const showSidebar = computed(() => !booting.value);
const showChrome = IS_TAURI;

/* The screen swap is driven by the Web Animations API instead of a CSS
   transition, and that is not a style choice — it is the only form that stops
   the packaged app from flashing.

   With `.view-enter-active { transition: transform }` WebKitGTK 2.54 on
   amdgpu/DMABUF occasionally paints one frame that is neither the outgoing nor
   the incoming screen right after the transition ends (measured: 11 anomalous
   frames over 10 runs of 8 view swaps; each one repaints 150k–250k px, i.e. the
   whole content area). Every CSS-level workaround was measured and rejected:
   resting identity transform, `translateZ(0)`, `will-change`, `contain: paint`,
   `isolation`, animating the child instead, shorter durations, keyframes — all
   still leave 7–22 anomalies, several are worse than the baseline. Moving the
   identical animation to `element.animate()` leaves 0 anomalies over 18 runs.

   Keep the animation transform-only: `<KeepAlive>` replays the enter hook when
   a cached screen is re-activated, so an opacity fade would run twice and read
   as a blink. A 6px slide replayed at that point is invisible. */
const reduceMotion =
  typeof window !== "undefined" &&
  window.matchMedia?.("(prefers-reduced-motion: reduce)").matches === true;

function viewEnter(el: Element, done: () => void) {
  if (reduceMotion) {
    done();
    return;
  }
  const anim = (el as HTMLElement).animate(
    [{ transform: "translateY(6px)" }, { transform: "translateY(0)" }],
    // `fill: "both"` is required, not cosmetic. Without it WebKitGTK drops the
    // animation's compositing layer the moment it finishes and repaints the
    // whole content area once from scratch — the flash this code exists to
    // avoid. Measured over 10 runs of 8 swaps each: 19 anomalous frames without
    // it, 0 with it. Filling forwards leaves the layer resting on the same
    // identity transform the stylesheet already declares, so nothing else
    // changes.
    {
      duration: 300,
      easing: "cubic-bezier(0.22, 1, 0.36, 1)",
      fill: "both",
    },
  );
  // `finished` rejects if the element is torn down mid-flight; either way the
  // transition has to be told it is over or the screen stays pinned.
  anim.finished.then(done, done);
}

onMounted(async () => {
  try {
    await bootstrap();
  } catch (err) {
    bootFailed.value = true;
    toast(String(err), "error");
  } finally {
    // Let the splash breathe for a beat so the reveal animation reads.
    window.setTimeout(() => (booting.value = false), 180);
  }
});

async function retryBoot() {
  bootFailed.value = false;
  booting.value = true;
  try {
    await bootstrap();
  } catch (err) {
    bootFailed.value = true;
    toast(String(err), "error");
  } finally {
    window.setTimeout(() => (booting.value = false), 180);
  }
}
</script>

<template>
  <div class="shell" :class="{ 'shell--decorated': !showChrome }">
    <TitleBar v-if="showChrome" />

    <div class="body">
      <Sidebar v-if="showSidebar" />

      <main class="stage">
        <!-- `:css="false"` + `@enter`: no CSS classes are applied, the swap is
             animated by `viewEnter` above. No leave hook on purpose — the
             outgoing screen must be gone before the incoming one animates in,
             otherwise both sit in the DOM and WebKitGTK paints from a stale
             tile. See the note on `viewEnter`. -->
        <Transition name="view" :css="false" @enter="viewEnter">
          <KeepAlive>
            <component :is="currentView" :key="app.view" class="view" />
          </KeepAlive>
        </Transition>
      </main>
    </div>

    <ResizeFrame v-if="showChrome" />

    <Toasts />

    <!-- boot splash -->
    <Transition name="fade">
      <div v-if="booting" class="boot">
        <div class="boot-inner">
          <div class="boot-mark">
            <!-- The squircle icon carries its own dark plate, so it reads on
                 both themes without a variant. -->
            <img src="/logo.png" alt="" draggable="false" />
          </div>
          <span class="boot-name">{{ BRAND.name }}</span>
          <span class="boot-bar"><i /></span>
        </div>
      </div>
    </Transition>

    <!-- fatal boot error -->
    <Transition name="fade">
      <div v-if="!booting && bootFailed" class="boot boot--error">
        <div class="boot-inner">
          <div class="boot-mark boot-mark--error">
            <Icon name="alert" :size="20" />
          </div>
          <span class="boot-name">启动失败</span>
          <p class="boot-msg">{{ app.bootError }}</p>
          <button class="btn btn--solid" @click="retryBoot">
            <Icon name="refresh" :size="14" />
            <span>重试</span>
          </button>
        </div>
      </div>
    </Transition>
  </div>
</template>

<style scoped>
.shell {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  background: var(--bg);
  overflow: hidden;
}
.body {
  flex: 1;
  min-height: 0;
  display: flex;
  overflow: hidden;
}
.stage {
  position: relative;
  flex: 1;
  min-width: 0;
  min-height: 0;
  display: flex;
  background: var(--bg);
}
/* Every screen's box, stacking order and opaque background live in
   src/styles/global.css (`.stage > .view`), right next to the swap transition
   that has to override them — a scoped `.view[data-v-…]` rule here would win
   that fight by specificity and re-open the stacking hole. */

/* ---------- boot splash ---------- */
.boot {
  position: fixed;
  inset: 0;
  z-index: 90;
  display: grid;
  place-items: center;
  background: var(--bg);
}
.boot-inner {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 14px;
}
.boot-mark {
  display: grid;
  place-items: center;
  width: 56px;
  height: 56px;
  transform: scale(1);
  animation: mark-in var(--dur-4) var(--ease-out) both;
}
.boot-mark img {
  width: 100%;
  height: 100%;
  object-fit: contain;
}
.boot-mark--error {
  color: var(--text);
  background: var(--bg-sunken);
  border: 1px solid var(--line);
}
.boot-mark--error :deep(.icon) {
  animation: none;
}
/* Boot mark: opacity only. A `scale()`/`rotate()` here ends on `transform: none`,
   which tears the layer down and repaints — the same flicker class as the rest
   of the UI (docs/tauri-webkitgtk-flicker.md). */
@keyframes mark-in {
  from {
    opacity: 0;
  }
}
.boot-name {
  font-size: 13px;
  font-weight: 540;
  letter-spacing: 0.01em;
  color: var(--text-2);
}
.boot-msg {
  margin: 0;
  max-width: 46ch;
  text-align: center;
  font-size: 11.8px;
  line-height: 1.6;
  color: var(--text-3);
}
.boot-bar {
  position: relative;
  width: 96px;
  height: 2px;
  overflow: hidden;
  border-radius: 99px;
  background: var(--line-strong);
}
.boot-bar i {
  position: absolute;
  inset: 0 auto 0 0;
  width: 40%;
  border-radius: 99px;
  background: var(--text);
  animation: bar-slide 1.15s var(--ease-in-out) infinite;
}
@keyframes bar-slide {
  0% {
    transform: translateX(-105%);
  }
  100% {
    transform: translateX(255%);
  }
}
</style>
