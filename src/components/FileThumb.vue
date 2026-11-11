<script setup lang="ts">
import { onMounted, ref, watch } from "vue";
import FileIcon from "@/components/FileIcon.vue";
import { api } from "@/lib/backend";

const props = withDefaults(
  defineProps<{
    thumb?: string | null;
    isDir: boolean;
    ext: string;
    name: string;
    size?: number;
  }>(),
  { thumb: null, size: 44 },
);

/**
 * Inlined previews, keyed by the signed url. Grid mode re-mounts cards while
 * scrolling and the same file can appear twice (search + a folder), so the
 * backend fetch is memoised for the life of the window. Signed urls rotate on
 * every listing, which is what keeps this from growing without bound.
 */
const cache = new Map<string, string | null>();

/** Signed urls rotate on every listing, so the map only needs a small window. */
function remember(url: string, data: string | null) {
  if (cache.size > 400) cache.clear();
  cache.set(url, data);
}

/**
 * Four in flight at a time. A folder of photos is 100+ previews and each one
 * costs a backend round trip; firing them all at once starved the listing.
 */
const LIMIT = 4;
let active = 0;
const waiting: (() => void)[] = [];

async function slot(): Promise<void> {
  if (active < LIMIT) {
    active += 1;
    return;
  }
  await new Promise<void>((resolve) => waiting.push(resolve));
}

function release() {
  const next = waiting.shift();
  if (next) next();
  else active -= 1;
}

const src = ref<string | null>(null);
const failed = ref(false);

async function load() {
  const url = props.thumb;
  if (!url) return;
  if (cache.has(url)) {
    src.value = cache.get(url) ?? null;
    failed.value = src.value === null;
    return;
  }
  await slot();
  try {
    const data = await api.inlineImage(url);
    remember(url, data);
    src.value = data;
    failed.value = data === null;
  } catch {
    remember(url, null);
    failed.value = true;
  } finally {
    release();
  }
}

onMounted(load);
watch(() => props.thumb, load);
</script>

<template>
  <span class="thumb" :style="{ width: `${size}px`, height: `${size}px` }">
    <img v-if="src && !failed" class="thumb-img" :src="src" :alt="name" loading="lazy" />
    <FileIcon v-else :is-dir="isDir" :ext="ext" :name="name" :size="size" />
  </span>
</template>

<style scoped>
.thumb {
  display: grid;
  place-items: center;
  flex: none;
}
.thumb-img {
  width: 100%;
  height: 100%;
  object-fit: cover;
  border-radius: var(--radius-xs);
  /* A hairline keeps light previews from bleeding into the card. */
  box-shadow: inset 0 0 0 1px var(--line);
  background: var(--bg-sunken);
}
</style>
