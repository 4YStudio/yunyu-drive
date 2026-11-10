<script setup lang="ts">
/**
 * Loading indicator for the file list: a ring spinner with the logo of the
 * drive being read in the middle.
 *
 * It replaces the shimmering skeleton rows, which read as "content is already
 * here and about to appear" and gave no hint about which drive was loading —
 * switching drives looked identical to refreshing the same one.
 */
import { computed } from "vue";
import { PROVIDERS } from "@/lib/types";

const props = withDefaults(
  defineProps<{
    /** Drive whose logo sits in the middle; falls back to a dot when unknown. */
    provider?: string | null;
    size?: number;
    caption?: string;
  }>(),
  { provider: null, size: 62, caption: "" },
);

const logo = computed(() => PROVIDERS.find((p) => p.id === props.provider)?.logo ?? null);
</script>

<template>
  <div class="drive-loader-wrap" role="status" :aria-label="caption || '正在加载'">
    <div class="drive-loader" :style="{ '--loader-size': `${size}px` }">
      <svg class="drive-loader-ring" viewBox="0 0 48 48" aria-hidden="true">
        <circle class="ring-track" cx="24" cy="24" r="21" />
        <circle class="ring-arc" cx="24" cy="24" r="21" />
      </svg>
      <img v-if="logo" class="drive-loader-logo" :src="logo" alt="" draggable="false" />
      <span v-else class="drive-loader-dot" />
    </div>
    <p v-if="caption" class="drive-loader-caption">{{ caption }}</p>
  </div>
</template>

<style scoped>
.drive-loader-wrap {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 14px;
  padding: 18px;
}
.drive-loader {
  position: relative;
  display: grid;
  place-items: center;
  width: var(--loader-size);
  height: var(--loader-size);
}
.drive-loader-ring {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  animation: drive-spin 1.15s linear infinite;
}
.ring-track {
  fill: none;
  stroke: var(--line-strong);
  stroke-width: 2.5;
}
.ring-arc {
  fill: none;
  stroke: var(--text-2);
  stroke-width: 2.5;
  stroke-linecap: round;
  /* circumference of r=21 is 131.9 — a 38/132 dash is a ~29% arc. */
  stroke-dasharray: 38 94;
}
.drive-loader-logo {
  width: calc(var(--loader-size) * 0.42);
  height: calc(var(--loader-size) * 0.42);
  object-fit: contain;
  user-select: none;
}
.drive-loader-dot {
  width: calc(var(--loader-size) * 0.18);
  height: calc(var(--loader-size) * 0.18);
  border-radius: 50%;
  background: var(--text-3);
}
.drive-loader-caption {
  margin: 0;
  font-size: 12.4px;
  font-weight: 450;
  letter-spacing: 0.01em;
  color: var(--text-3);
}
@keyframes drive-spin {
  to {
    transform: rotate(360deg);
  }
}
@media (prefers-reduced-motion: reduce) {
  .drive-loader-ring {
    animation-duration: 2.6s;
  }
}
</style>
