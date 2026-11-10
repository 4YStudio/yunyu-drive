<script setup lang="ts">
import { nextTick, onBeforeUnmount, ref, watch } from "vue";
import Icon from "@/components/Icon.vue";
import { pop } from "@/lib/motion";
import type { MenuItem } from "@/lib/menu";

const props = defineProps<{
  open: boolean;
  x: number;
  y: number;
  items: MenuItem[];
}>();
const emit = defineEmits<{ select: [string]; close: [] }>();

const root = ref<HTMLElement | null>(null);
const pos = ref({ x: props.x, y: props.y });

function clamp() {
  if (!root.value) return;
  const rect = root.value.getBoundingClientRect();
  const margin = 8;
  let x = props.x;
  let y = props.y;
  if (x + rect.width + margin > window.innerWidth) x = window.innerWidth - rect.width - margin;
  if (y + rect.height + margin > window.innerHeight) y = props.y - rect.height;
  pos.value = { x: Math.max(margin, x), y: Math.max(margin, y) };
}

watch(
  () => props.open,
  async (open) => {
    if (!open) return;
    pos.value = { x: props.x, y: props.y };
    await nextTick();
    clamp();
    window.addEventListener("pointerdown", onOutside, true);
    window.addEventListener("keydown", onKey);
    window.addEventListener("blur", close);
  },
);

function onOutside(event: PointerEvent) {
  if (root.value && !root.value.contains(event.target as Node)) close();
}
function onKey(event: KeyboardEvent) {
  if (event.key === "Escape") close();
}
function close() {
  window.removeEventListener("pointerdown", onOutside, true);
  window.removeEventListener("keydown", onKey);
  window.removeEventListener("blur", close);
  emit("close");
}
onBeforeUnmount(close);

function pick(item: MenuItem) {
  if (item.disabled) return;
  emit("select", item.id);
  close();
}
</script>

<template>
  <Teleport to="body">
    <Transition :css="false" v-bind="pop">
      <div
        v-if="open"
        ref="root"
        class="menu"
        :style="{ left: `${pos.x}px`, top: `${pos.y}px` }"
        role="menu"
      >
        <template v-for="item in items" :key="item.id">
          <div v-if="item.separatorBefore" class="menu-sep" />
          <button
            class="menu-item"
            :class="{ 'menu-item--danger': item.danger, 'menu-item--disabled': item.disabled }"
            role="menuitem"
            :disabled="item.disabled"
            @click="pick(item)"
          >
            <Icon v-if="item.icon" :name="item.icon" :size="15" class="menu-icon" />
            <span v-else class="menu-icon" />
            <span class="menu-label">{{ item.label }}</span>
            <span v-if="item.hint" class="menu-hint">{{ item.hint }}</span>
          </button>
        </template>
      </div>
    </Transition>
  </Teleport>
</template>

<style scoped>
.menu {
  position: fixed;
  z-index: 120;
  min-width: 194px;
  padding: 5px;
  border-radius: var(--radius-md);
  background: color-mix(in srgb, var(--bg) 92%, transparent);
  backdrop-filter: blur(22px) saturate(150%);
  border: 1px solid var(--line);
  box-shadow: var(--shadow-lg);
}
.menu-item {
  display: flex;
  align-items: center;
  gap: 9px;
  width: 100%;
  height: 31px;
  padding: 0 9px;
  border-radius: var(--radius-xs);
  font-size: 12.8px;
  font-weight: 470;
  color: var(--text);
  text-align: left;
  transition: background-color var(--dur-1) var(--ease-out);
}
.menu-item:hover {
  background: var(--bg-hover);
}
.menu-item--danger {
  color: var(--text);
}
.menu-item--danger:hover {
  background: var(--bg-active);
}
.menu-item--disabled {
  opacity: 0.38;
  pointer-events: none;
}
.menu-icon {
  display: inline-flex;
  width: 15px;
  color: var(--text-2);
}
.menu-item--danger .menu-icon {
  color: var(--text);
}
.menu-label {
  flex: 1;
}
.menu-hint {
  font-size: 10.5px;
  color: var(--text-3);
  font-family: var(--font-mono);
}
.menu-sep {
  height: 1px;
  margin: 5px 7px;
  background: var(--line);
}
</style>
