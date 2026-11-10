<script setup lang="ts">
import { computed } from "vue";

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
</script>

<template>
  <div class="segmented" :class="{ 'segmented--compact': compact }" role="tablist">
    <span
      class="segmented-thumb"
      :style="{
        width: `calc((100% - 6px) / ${options.length})`,
        transform: `translateX(calc(${activeIndex} * 100%))`,
      }"
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
  position: absolute;
  z-index: 0;
  top: 3px;
  bottom: 3px;
  left: 3px;
  border-radius: 6px;
  background: var(--bg);
  box-shadow: var(--shadow-sm), 0 0 0 1px var(--line);
  /* No spring easing: the overshoot moves the thumb past its slot and back,
     which reads as a jitter on WebKitGTK. Plain ease-out settles once. */
  transition: transform var(--dur-2) var(--ease-out);
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
