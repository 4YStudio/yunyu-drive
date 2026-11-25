import { computed, ref, watch, type ComputedRef, type Ref } from "vue";

/**
 * Rows mounted at once, for every long listing in the app.
 *
 * WebKitGTK paints in software, so a list of a few thousand rows does not just
 * scroll badly — it stalls the whole window while the rows are laid out. The
 * file listing learned this first (a folder with 3000 entries froze the app for
 * seconds); this module is the same trick made reusable.
 */
export const ROW_CHUNK = 200;

export interface ChunkedRows<T> {
  /** The rows to mount right now. */
  rows: ComputedRef<T[]>;
  /** How many are mounted, for the "已显示 x / y" footer. */
  shown: Ref<number>;
  /** How many are still waiting. */
  rest: ComputedRef<number>;
  /** Mounts the next chunk (or `n` more). */
  more: (n?: number) => void;
  /** Goes back to the first chunk — call after replacing the list. */
  reset: () => void;
  /** `@scroll.passive="onScroll"` on the scrolling container. */
  onScroll: (event: Event) => void;
}

/**
 * Mounts a long list in chunks, growing the window as the user scrolls.
 *
 * `resetKey` decides when the window goes back to the first chunk — a new
 * folder, a new filter, another drive. It deliberately does *not* reset when the
 * array merely changes: transfer lists are rewritten every few hundred
 * milliseconds by progress events, and resetting there would yank the reader
 * back to the top mid-scroll.
 */
export function useChunkedRows<T>(
  source: ComputedRef<T[]> | Ref<T[]>,
  opts: { chunk?: number; resetKey?: () => unknown } = {},
): ChunkedRows<T> {
  const chunk = opts.chunk ?? ROW_CHUNK;
  const shown = ref(chunk);
  const rows = computed(() => source.value.slice(0, shown.value));
  const rest = computed(() => Math.max(0, source.value.length - shown.value));

  function more(n = chunk) {
    shown.value = Math.min(source.value.length, shown.value + n);
  }

  function onScroll(event: Event) {
    const el = event.currentTarget as HTMLElement | null;
    if (!el || rest.value === 0) return;
    // Same 600px lead as the file listing: the next chunk is already mounted by
    // the time the reader gets there.
    if (el.scrollHeight - el.scrollTop - el.clientHeight > 600) return;
    more();
  }

  function reset() {
    shown.value = chunk;
  }

  if (opts.resetKey) {
    watch(opts.resetKey, () => {
      shown.value = chunk;
    });
  }

  return { rows, shown, rest, more, reset, onScroll };
}
