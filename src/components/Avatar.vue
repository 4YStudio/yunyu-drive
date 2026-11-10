<script setup lang="ts">
import { computed } from "vue";

const props = withDefaults(
  defineProps<{
    src?: string | null;
    name?: string;
    size?: number;
    filled?: boolean;
  }>(),
  { src: null, name: "", size: 26, filled: false },
);

const initial = computed(() => (props.name || "?").trim().slice(0, 1).toUpperCase());
</script>

<template>
  <span
    class="avatar"
    :class="{ 'avatar--filled': filled }"
    :style="{ width: `${size}px`, height: `${size}px`, fontSize: `${Math.round(size * 0.42)}px` }"
  >
    <img v-if="src" :src="src" alt="" draggable="false" />
    <span v-else class="avatar-initial">{{ initial }}</span>
  </span>
</template>

<style scoped>
.avatar {
  flex: none;
  display: grid;
  place-items: center;
  border-radius: 50%;
  overflow: hidden;
  font-weight: 600;
  line-height: 1;
  color: var(--text-2);
  background: var(--bg-active);
  box-shadow: inset 0 0 0 1px var(--line);
  transition: background-color var(--dur-2) var(--ease-out),
    box-shadow var(--dur-2) var(--ease-out), color var(--dur-2) var(--ease-out);
}
.avatar--filled {
  color: var(--on-fill);
  background: var(--fill);
}
.avatar img {
  width: 100%;
  height: 100%;
  object-fit: cover;
}
.avatar-initial {
  letter-spacing: 0;
}
</style>
