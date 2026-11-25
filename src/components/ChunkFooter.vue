<script setup lang="ts">
import { computed } from "vue";

/**
 * Footer under a chunked listing.
 *
 * Scrolling already mounts the next chunk; this is the visible explanation for
 * why the list stops where it does, plus a way to page without a wheel.
 */
const props = defineProps<{
  /** Rows currently in the DOM — never the window size, which overshoots. */
  mounted: number;
  rest: number;
  chunk?: number;
  noun?: string;
}>();

const emit = defineEmits<{ (e: "more"): void }>();

const step = computed(() => Math.min(props.rest, props.chunk ?? 200));
</script>

<template>
  <div class="chunk-foot">
    <span class="chunk-hint">
      已显示 {{ mounted }} 项{{ rest ? `，还有 ${rest} 项${noun ?? ""}` : "" }}
    </span>
    <button v-if="rest" class="btn btn--ghost" @click="emit('more')">
      再加载 {{ step }} 项
    </button>
  </div>
</template>

<style scoped>
.chunk-foot {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 12px;
  padding: 14px 0 6px;
  font-size: 11.5px;
  color: var(--text-3);
}
.chunk-hint {
  white-space: nowrap;
}
</style>
