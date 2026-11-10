<script setup lang="ts">
import { app, dismissToast } from "@/store/app";
import Icon from "@/components/Icon.vue";
</script>

<template>
  <Teleport to="body">
    <div class="toasts" aria-live="polite">
      <TransitionGroup name="toast">
        <div
          v-for="item in app.toasts"
          :key="item.id"
          class="toast"
          :class="`toast--${item.kind}`"
          @click="dismissToast(item.id)"
        >
          <Icon
            :name="item.kind === 'error' ? 'alert' : item.kind === 'success' ? 'check-circle' : 'info'"
            :size="15"
          />
          <span class="toast-text">{{ item.message }}</span>
        </div>
      </TransitionGroup>
    </div>
  </Teleport>
</template>

<style scoped>
.toasts {
  position: fixed;
  left: 50%;
  bottom: 26px;
  z-index: 200;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 8px;
  transform: translateX(-50%);
  pointer-events: none;
}
.toast {
  display: flex;
  align-items: center;
  gap: 9px;
  max-width: 440px;
  padding: 9px 15px;
  border-radius: var(--radius-pill);
  font-size: 12.5px;
  font-weight: 480;
  color: var(--text);
  background: color-mix(in srgb, var(--bg) 90%, transparent);
  backdrop-filter: blur(20px) saturate(160%);
  border: 1px solid var(--line);
  box-shadow: var(--shadow-md);
  pointer-events: auto;
  cursor: pointer;
  transition: opacity var(--dur-2) var(--ease-out),
    transform var(--dur-2) var(--ease-out);
}
.toast-text {
  line-height: 1.45;
}
.toast--error {
  box-shadow: var(--shadow-md), inset 0 0 0 1px var(--line-strong);
}
.toast-enter-active {
  transition: opacity var(--dur-3) var(--ease-out),
    transform var(--dur-3) var(--ease-out);
}
.toast-leave-active {
  position: absolute;
  transition: opacity var(--dur-2) var(--ease-out),
    transform var(--dur-2) var(--ease-out);
}
/* Translate only, resting on an identity transform: `scale()` here made the
   glass panel re-raster every frame and blink on teardown. */
.toast-enter-from {
  opacity: 0;
  transform: translateY(10px);
}
.toast-leave-to {
  opacity: 0;
  transform: translateY(4px);
}
.toast-move {
  transition: transform var(--dur-3) var(--ease-out);
}
:where(.toast) {
  transform: translateY(0);
}
</style>
