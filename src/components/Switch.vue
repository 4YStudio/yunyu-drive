<script setup lang="ts">
import { motion } from "motion-v";
import { thumbTransition } from "@/lib/motion";

withDefaults(
  defineProps<{
    modelValue: boolean;
    label?: string;
    hint?: string;
    disabled?: boolean;
  }>(),
  { disabled: false },
);
const emit = defineEmits<{ "update:modelValue": [boolean] }>();

function toggle(current: boolean) {
  emit("update:modelValue", !current);
}
</script>

<template>
  <div class="switch-row">
    <div v-if="label || hint" class="switch-copy">
      <span v-if="label" class="switch-label">{{ label }}</span>
      <span v-if="hint" class="switch-hint">{{ hint }}</span>
    </div>
    <button
      type="button"
      class="switch"
      :class="{ 'switch--on': modelValue }"
      role="switch"
      :aria-checked="modelValue"
      :aria-label="label"
      :disabled="disabled"
      @click="toggle(modelValue)"
    >
      <motion.span
        class="switch-knob"
        :animate="{ x: modelValue ? 17 : 0 }"
        :transition="thumbTransition"
      />
    </button>
  </div>
</template>

<style scoped>
.switch-row {
  display: flex;
  align-items: center;
  gap: 20px;
  justify-content: space-between;
}
.switch-copy {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}
.switch-label {
  font-size: 13px;
  font-weight: 500;
}
.switch-hint {
  font-size: 11.5px;
  color: var(--text-3);
}
.switch {
  position: relative;
  flex: none;
  width: 40px;
  height: 23px;
  border-radius: 999px;
  background: var(--bg-active);
  box-shadow: inset 0 0 0 1px var(--line);
  transition: background-color var(--dur-2) var(--ease-out),
    box-shadow var(--dur-2) var(--ease-out);
}
.switch--on {
  background: var(--fill);
  box-shadow: inset 0 0 0 1px transparent;
}
.switch-knob {
  /* WebKitGTK drops a compositing layer the moment the last `will-change`
     disappears, and motion clears its inline `will-change` as soon as an
     animation finishes. The repaint that follows uses the element's *layout*
     position, so a travelled slider flashes back to its slot of origin for a
     frame before the final transform is re-applied — the "B → A → B" snap.
     Declaring the hint in CSS keeps the layer alive after motion is done. */
  will-change: transform;

  position: absolute;
  top: 2.5px;
  left: 2.5px;
  width: 18px;
  height: 18px;
  border-radius: 50%;
  background: var(--bg);
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.25);
  /* The slide is motion-v's (`thumbTransition`); only the colour fades in CSS,
     because a colour cannot be handed to the compositor anyway. */
  transition: background-color var(--dur-2) var(--ease-out);
}
.switch--on .switch-knob {
  background: var(--on-fill);
}
.switch:disabled {
  opacity: 0.45;
  pointer-events: none;
}
</style>
