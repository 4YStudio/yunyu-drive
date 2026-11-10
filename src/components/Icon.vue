<script setup lang="ts">
import { computed } from "vue";
import { ICON_PATHS, type IconName } from "@/lib/icons";

const props = withDefaults(
  defineProps<{
    name: IconName;
    size?: number | string;
    stroke?: number;
    spin?: boolean;
  }>(),
  { size: 18, stroke: 1.7, spin: false },
);

const px = computed(() =>
  typeof props.size === "number" ? `${props.size}px` : props.size,
);
const path = computed(() => ICON_PATHS[props.name] ?? "");
</script>

<template>
  <svg
    class="icon"
    :class="{ 'icon--spin': spin }"
    :width="px"
    :height="px"
    viewBox="0 0 24 24"
    fill="none"
    :stroke-width="stroke"
    stroke="currentColor"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
    focusable="false"
  >
    <path :d="path" />
  </svg>
</template>

<style scoped>
.icon {
  display: block;
  flex: none;
  overflow: visible;
}
.icon--spin {
  animation: icon-spin 900ms linear infinite;
}
@keyframes icon-spin {
  to {
    transform: rotate(360deg);
  }
}
</style>
