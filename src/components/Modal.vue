<script setup lang="ts">
import { onBeforeUnmount, watch } from "vue";

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
    <Transition name="modal">
      <div v-if="open" class="backdrop" @click.self="closeOnBackdrop !== false && emit('close')">
        <div
          class="modal"
          :style="{ width: width ? `${width}px` : undefined }"
          role="dialog"
          aria-modal="true"
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
        </div>
      </div>
    </Transition>
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
.modal-enter-active {
  transition: none;
}
/* Leaving the backdrop is instant too, for the same reason: an opacity
   transition that finishes on a full-viewport layer costs one full-surface
   repaint right after it ends (measured 140 000 px, bbox = whole window, ~60 ms
   after the transition completed). Instant removal is a single honest frame. */
.modal-leave-active {
  transition: none;
}
/* Translate only, and rest on an identity transform. `scale()` on a panel that
   carries text plus `--shadow-lg` forces a re-raster of the whole box every
   frame, and dropping the transform at the end tears the layer down — WebKitGTK
   then repaints one frame from the stale tile, which reads as the panel (and
   the page behind it) blinking. See docs/tauri-webkitgtk-flicker.md. */
.modal-enter-active .modal {
  transition: transform var(--dur-3) var(--ease-out), opacity var(--dur-3) var(--ease-out);
}
.modal-leave-active .modal {
  transition: transform var(--dur-2) var(--ease-out), opacity var(--dur-2) var(--ease-out);
}
.modal-leave-to {
  opacity: 0;
}
.modal-enter-from .modal,
.modal-leave-to .modal {
  opacity: 0;
  transform: translateY(10px);
}
:where(.modal) {
  transform: translateY(0);
}
</style>
