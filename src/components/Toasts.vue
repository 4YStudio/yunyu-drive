<script setup lang="ts">
import { AnimatePresence, motion } from "motion-v";
import { app, dismissToast } from "@/store/app";
import Icon from "@/components/Icon.vue";
import { toastHidden, toastShown, toastTransition } from "@/lib/motion";
</script>

<template>
  <Teleport to="body">
    <div class="toasts" aria-live="polite">
      <AnimatePresence>
        <motion.div
          v-for="item in app.toasts"
          :key="item.id"
          class="toast"
          :class="`toast--${item.kind}`"
          :initial="toastHidden"
          :animate="toastShown"
          :exit="{ opacity: 0, scale: 0.96, transition: { duration: 0.12 } }"
          :transition="toastTransition"
          layout
          @click="dismissToast(item.id)"
        >
          <Icon
            :name="item.kind === 'error' ? 'alert' : item.kind === 'success' ? 'check-circle' : 'info'"
            :size="15"
          />
          <span class="toast-text">{{ item.message }}</span>
        </motion.div>
      </AnimatePresence>
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
  /* Animated by motion-v; a CSS transition here would fight it. */
}
.toast-text {
  line-height: 1.45;
}
.toast--error {
  box-shadow: var(--shadow-md), inset 0 0 0 1px var(--line-strong);
}
/* Entry/exit come from motion-v (see the template). These classes are inert —
   Vue never adds them any more. `layout` on the motion element handles the
   "slide up when the one below disappears" reflow that `.toast-move` used to. */
/* Rest on an identity transform rather than `none`: WebKitGTK tears the
   compositing layer down when the animated transform disappears and repaints
   the area once from scratch. */
:where(.toast) {
  transform: translateY(0);
}
</style>
