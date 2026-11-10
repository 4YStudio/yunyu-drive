<script setup lang="ts">
import Icon from "@/components/Icon.vue";
import type { IconName } from "@/lib/icons";

withDefaults(
  defineProps<{
    icon?: IconName;
    title: string;
    description?: string;
  }>(),
  { icon: "folder" },
);
</script>

<template>
  <div class="empty">
    <div class="empty-glyph">
      <Icon :name="icon" :size="26" :stroke="1.4" />
    </div>
    <p class="empty-title">{{ title }}</p>
    <p v-if="description" class="empty-desc">{{ description }}</p>
    <div v-if="$slots.default" class="empty-actions">
      <slot />
    </div>
  </div>
</template>

<style scoped>
.empty {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 6px;
  padding: 64px 24px 72px;
  text-align: center;
  transform: translateY(0);
  animation: empty-in var(--dur-4) var(--ease-out);
}
@keyframes empty-in {
  from {
    opacity: 0;
    transform: translateY(6px);
  }
}
.empty-glyph {
  display: grid;
  place-items: center;
  width: 58px;
  height: 58px;
  margin-bottom: 8px;
  border-radius: 20px;
  color: var(--text-2);
  background: var(--bg);
  box-shadow: inset 0 0 0 1px var(--line), var(--shadow-sm);
}
.empty-title {
  margin: 0;
  font-size: 13.5px;
  font-weight: 520;
}
.empty-desc {
  margin: 0;
  max-width: 42ch;
  font-size: 12.2px;
  color: var(--text-3);
  line-height: 1.6;
}
.empty-actions {
  display: flex;
  gap: 8px;
  margin-top: 14px;
}
</style>
