<script setup lang="ts">
import { onBeforeUnmount, watch } from "vue";
import { AnimatePresence, motion } from "motion-v";
import { dialogHidden, dialogLeaveTransition, dialogShown, dialogTransition, reducedMotion } from "@/lib/motion";

const props = defineProps<{
  open: boolean;
  title?: string;
  subtitle?: string;
  width?: number;
  closeOnBackdrop?: boolean;
}>();
const emit = defineEmits<{ close: [] }>();

function onKey(event: KeyboardEvent) {
  if (event.key === "Escape") emit("close");
}

watch(
  () => props.open,
  (open) => {
    if (open) window.addEventListener("keydown", onKey);
    else window.removeEventListener("keydown", onKey);
  },
);

onBeforeUnmount(() => window.removeEventListener("keydown", onKey));
</script>

<template>
  <!-- Teleported for the same reason the context menu is: a `transform` on an
       ancestor turns it into the containing block for `position: fixed`, and
       the screens carry an identity transform. -->
  <Teleport to="body">
    <AnimatePresence>
      <motion.div
        v-if="open"
        class="backdrop"
        :initial="{ opacity: 0 }"
        :animate="{ opacity: 1 }"
        :exit="{ opacity: 0 }"
        :transition="{ duration: reducedMotion ? 0 : 0.14 }"
        @click.self="closeOnBackdrop !== false && emit('close')"
      >
        <motion.div
          class="modal"
          :style="{ width: width ? `${width}px` : undefined }"
          role="dialog"
          aria-modal="true"
          :initial="dialogHidden"
          :animate="dialogShown"
          :exit="{ opacity: 0, scale: 0.96, y: 6, transition: dialogLeaveTransition }"
          :transition="dialogTransition"
        >
          <header v-if="title" class="modal-head">
            <div>
              <h2 class="modal-title">{{ title }}</h2>
              <p v-if="subtitle" class="modal-sub">{{ subtitle }}</p>
            </div>
            <button class="icon-btn" aria-label="关闭" @click="emit('close')">
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round">
                <path d="M6 6l12 12M18 6L6 18" />
              </svg>
            </button>
          </header>
          <div class="modal-body">
            <slot />
          </div>
          <footer v-if="$slots.footer" class="modal-foot">
            <slot name="footer" />
          </footer>
        </motion.div>
      </motion.div>
    </AnimatePresence>
  </Teleport>
</template>

<style scoped>
.backdrop {
  position: fixed;
  inset: 0;
  z-index: 90;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 32px;
  background: color-mix(in srgb, var(--bg-sunken) 62%, transparent);
  backdrop-filter: blur(14px) saturate(120%);
}
.modal {
  max-width: 100%;
  max-height: 100%;
  display: flex;
  flex-direction: column;
  border-radius: var(--radius-lg);
  background: var(--bg);
  border: 1px solid var(--line);
  box-shadow: var(--shadow-lg);
  overflow: hidden;
}
.modal-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 16px;
  padding: 20px 18px 12px 22px;
}
.modal-title {
  margin: 0;
  font-size: 15px;
  font-weight: 560;
  letter-spacing: -0.01em;
}
.modal-sub {
  margin: 3px 0 0;
  font-size: 12px;
  color: var(--text-2);
  max-width: 44ch;
}
.modal-body {
  padding: 0 22px 4px;
  overflow: auto;
}
.modal-foot {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 8px;
  padding: 16px 18px 18px;
}

/* The backdrop appears instantly on enter. It covers the whole viewport, so any
   opacity transition on it builds a full-screen compositing layer; when that
   transition finishes and the layer is torn down, WebKitGTK re-rasterises the
   entire WebView surface — one frame painted from stale tiles, which is the
   whole-window flash. Measured: with the backdrop fading, the frame after the
   transition covers 243 000 px (8.7% of the viewport, bbox = the whole screen);
   with it instant, only 36 000 px (bbox = the panel alone). See
   docs/tauri-webkitgtk-flicker.md. */
:where(.modal) {
  /* The dialog is a `motion.div` inside `<AnimatePresence>`; its spring and its
     exit live in the template. Nothing here may declare a transform — motion-v
     writes one inline and a stylesheet value only fights it. */
}
</style>
