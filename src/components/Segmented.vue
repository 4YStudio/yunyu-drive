<script setup lang="ts">
import { computed } from "vue";
import { motion } from "motion-v";
import { thumbTransition } from "@/lib/motion";

export interface SegmentOption<T extends string = string> {
  value: T;
  label: string;
}

const props = defineProps<{
  modelValue: string;
  options: SegmentOption[];
  compact?: boolean;
}>();
const emit = defineEmits<{ "update:modelValue": [string] }>();

const activeIndex = computed(() =>
  Math.max(
    0,
    props.options.findIndex((o) => o.value === props.modelValue),
  ),
);

/** Stable target object — a fresh literal on every render makes motion restart
 *  the slide from the segment it began on (an unrelated re-render of the parent
 *  is enough to trigger it). Same rule as `navThumb` in Sidebar.vue. */
const thumbTarget = computed(() => ({ x: `${activeIndex.value * 100}%` }));
</script>

<template>
  <div class="segmented" :class="{ 'segmented--compact': compact }" role="tablist">
    <motion.span
      class="segmented-thumb"
      :style="{ width: `calc((100% - 6px) / ${options.length})` }"
      :initial="false"
      :animate="thumbTarget"
      :transition="thumbTransition"
    />
    <button
      v-for="option in options"
      :key="option.value"
      type="button"
      role="tab"
      class="segmented-item"
      :class="{ 'segmented-item--active': option.value === modelValue }"
      :aria-selected="option.value === modelValue"
      @click="emit('update:modelValue', option.value)"
    >
      {{ option.label }}
    </button>
  </div>
</template>

<style scoped>
.segmented {
  position: relative;
  display: inline-grid;
  grid-auto-flow: column;
  grid-auto-columns: 1fr;
  align-items: center;
  padding: 3px;
  border-radius: var(--radius-sm);
  background: var(--bg-sunken);
  box-shadow: inset 0 0 0 1px var(--line);
}
:root[data-theme="dark"] .segmented {
  background: rgba(255, 255, 255, 0.045);
}
.segmented-thumb {
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
  border-radius: 6px;
  background: var(--bg);
  box-shadow: var(--shadow-sm), 0 0 0 1px var(--line);
  /* Driven by motion-v (`thumbTransition`): a spring stiff enough that it
     settles in one beat. A softer spring overshoots past the slot and back,
     which reads as jitter and re-rasterises the shadowed thumb layer twice. */
}
.segmented-item {
  position: relative;
  z-index: 1;
  min-width: 62px;
  height: 26px;
  padding: 0 12px;
  white-space: nowrap;
  border-radius: 6px;
  font-size: 12px;
  font-weight: 500;
  color: var(--text-2);
  transition: color var(--dur-2) var(--ease-out);
}
.segmented-item--active {
  color: var(--text);
}
.segmented--compact .segmented-item {
  min-width: 0;
  height: 22px;
  padding: 0 10px;
  font-size: 11.5px;
}
</style>
