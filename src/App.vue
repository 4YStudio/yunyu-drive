<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from "vue";
import { AnimatePresence, animate, motion } from "motion-v";

import Sidebar from "@/components/Sidebar.vue";
import TitleBar from "@/components/TitleBar.vue";
import ResizeFrame from "@/components/ResizeFrame.vue";
import Toasts from "@/components/Toasts.vue";
import Icon from "@/components/Icon.vue";
import FilesView from "@/views/FilesView.vue";
import RecycleView from "@/views/RecycleView.vue";
import MySharesView from "@/views/MySharesView.vue";
import TransfersView from "@/views/TransfersView.vue";
import SettingsView from "@/views/SettingsView.vue";
import ShareView from "@/views/ShareView.vue";
import LoginView from "@/views/LoginView.vue";
import { BRAND } from "@/lib/brand";
import { IS_TAURI } from "@/lib/host";
import { errText } from "@/lib/format";
import { popHidden, popShown, reducedMotion, viewHidden, viewShown, viewTransition } from "@/lib/motion";
import { app, bootstrap, toast } from "@/store/app";

const booting = ref(true);
const bootFailed = ref(false);

const currentView = computed(() => {
  const map = {
    files: FilesView,
    recycle: RecycleView,
    myShares: MySharesView,
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

/* The screen swap is driven by motion-v, which runs animations through the Web
   Animations API through the compositor.

   History, because every one of these was measured and matters: a CSS
   `transition: transform` on the view left WebKitGTK 2.54 (amdgpu/DMABUFF)
   painting one frame that is neither the outgoing nor the incoming screen right
   after the transition ends — 11 anomalous frames over 10 runs of 8 swaps, each
   one repainting 150k–250k px (the whole content area). Every CSS-level
   workaround was measured and rejected: resting identity transform,
   `translateZ(0)`, `will-change`, `contain: paint`, `isolation`, animating the
   child instead, shorter durations, keyframes — all still leave 7–22 anomalies,
   several worse than the baseline. Driving the identical animation from JS
   leaves 0 over 18 runs.

   So: **opacity only, no transform.** Sliding the view root up 6px still left
   the content jumping a couple of pixels downwards and snapping back once the
   animation ended: the view is `position: absolute; inset: 0` — the largest
   surface in the app — and dropping the promoted layer at the end of the
   animation leaves WebKitGTK's raster offset behind for a frame. A fade cannot
   shift anything, so it cannot do that, and the movement a section change needs
   comes from the rows staggering in underneath (`rowEnter` in the listings).

   No leave animation on purpose: the outgoing screen is removed before the
   incoming one fades in, because two mounted screens let WebKitGTK paint a
   frame from the outgoing screen's stale tile. */
const stageView = ref<{ $el?: unknown } | null>(null);

async function playViewEnter() {
  if (reducedMotion) return;
  await nextTick();
  const el = stageView.value?.$el as HTMLElement | undefined;
  if (!el) return;
  // Start from zero *before* the first paint of the incoming screen: a cached
  // screen keeps `opacity: 1` inline from its previous run, so waiting for the
  // animation's own first frame would show it for one frame and then blink it
  // out again.
  el.style.opacity = "0";
  animate(el, { opacity: [viewHidden.opacity, viewShown.opacity] }, viewTransition);
}

// Fires on every swap, including a return to a `<KeepAlive>`-cached screen —
// that replay is what makes switching sections feel continuous.
watch(() => app.view, playViewEnter);

onMounted(async () => {
  try {
    await bootstrap();
  } catch (err) {
    bootFailed.value = true;
    toast(errText(err), "error");
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
    toast(errText(err), "error");
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
        <!-- The swap is animated imperatively by `playViewEnter` above, not by
             a <Transition>: there is deliberately no leave animation, so the
             outgoing screen is gone before the incoming one animates in.
             Two screens in the DOM at once let WebKitGTK paint a frame from the
             outgoing screen's stale tile — the flash this code exists to avoid. -->
        <KeepAlive>
          <component :is="currentView" :key="app.view" ref="stageView" class="view" />
        </KeepAlive>
      </main>
    </div>

    <ResizeFrame v-if="showChrome" />

    <Toasts />

    <!-- boot splash -->
    <AnimatePresence>
      <motion.div
        v-if="booting"
        class="boot"
        :initial="{ opacity: 0 }"
        :animate="{ opacity: 1 }"
        :exit="{ opacity: 0 }"
        :transition="{ duration: 0.16 }"
      >
        <div class="boot-inner">
          <motion.div
            class="boot-mark"
            :initial="{ opacity: 0, scale: 0.92 }"
            :animate="{ opacity: 1, scale: 1 }"
            :transition="{ type: 'spring', stiffness: 260, damping: 26, mass: 0.9 }"
          >
            <!-- The squircle icon carries its own dark plate, so it reads on
                 both themes without a variant. -->
            <img src="/logo.png" alt="" draggable="false" />
          </motion.div>
          <span class="boot-name">{{ BRAND.name }}</span>
          <span class="boot-bar"><i /></span>
        </div>
      </motion.div>
    </AnimatePresence>

    <!-- fatal boot error -->
    <AnimatePresence>
      <motion.div
        v-if="!booting && bootFailed"
        class="boot boot--error"
        :initial="popHidden"
        :animate="popShown"
        :transition="{ duration: 0.2, ease: [0.22, 1, 0.36, 1] }"
        :exit="{ opacity: 0, transition: { duration: 0.12 } }"
      >
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
      </motion.div>
    </AnimatePresence>
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
/* The boot mark is a `motion.div` now (spring scale-in). Keep the resting
   transform at `scale(1)` rather than `none` so the layer is not torn down and
   repainted when the animation ends — same reason as `.stage > .view`. */
.boot-mark {
  transform: scale(1);
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
