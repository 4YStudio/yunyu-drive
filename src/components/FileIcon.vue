<script setup lang="ts">
import { computed } from "vue";
import Icon from "@/components/Icon.vue";
import { fileKind } from "@/lib/format";

const props = withDefaults(
  defineProps<{ isDir: boolean; ext: string; name: string; size?: number }>(),
  { size: 34 },
);

const kind = computed(() => fileKind({ isDir: props.isDir, ext: props.ext }));
const label = computed(() => (props.ext || "file").slice(0, 4).toUpperCase());
const iconForKind = computed(() => {
  switch (kind.value) {
    case "image":
      return "cloud" as const;
    case "video":
      return "play" as const;
    case "audio":
      return "sparkles" as const;
    case "archive":
      return "database" as const;
    case "code":
      return "keyboard" as const;
    case "pdf":
      return "shield" as const;
    default:
      return "drive" as const;
  }
});
</script>

<template>
  <span class="file-icon" :class="`file-icon--${kind}`" :style="{ width: `${size}px`, height: `${size}px` }">
    <Icon v-if="isDir" name="folder" :size="size * 0.52" :stroke="1.6" />
    <template v-else>
      <span class="file-icon-ext">{{ label }}</span>
    </template>
  </span>
</template>

<style scoped>
.file-icon {
  position: relative;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  flex: none;
  border-radius: 9px;
  color: var(--text-2);
  background: var(--bg-active);
  box-shadow: inset 0 0 0 1px var(--line);
  overflow: hidden;
  transition: background-color var(--dur-2) var(--ease-out),
    color var(--dur-2) var(--ease-out);
}
.file-icon--folder {
  color: var(--text);
  background: var(--bg-sunken);
}
.file-icon-ext {
  font-family: var(--font-mono);
  font-size: 9.5px;
  font-weight: 600;
  letter-spacing: 0.03em;
  line-height: 1;
  color: var(--text-2);
  text-transform: uppercase;
}
.file-icon--image .file-icon-ext,
.file-icon--video .file-icon-ext {
  color: var(--text);
}
</style>
