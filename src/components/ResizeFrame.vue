<script setup lang="ts">
/**
 * Invisible resize grips for the undecorated window.
 *
 * With `decorations: false` the compositor draws no frame, so each edge and
 * corner gets a thin overlay that hands the drag back to the window manager
 * through `startResizeDragging`.
 */
import { getCurrentWindow } from "@tauri-apps/api/window";
import { IS_TAURI } from "@/lib/host";

/** Mirrors `ResizeDirection` from @tauri-apps/api/window (not exported there). */
type ResizeDirection =
  | "East"
  | "North"
  | "NorthEast"
  | "NorthWest"
  | "South"
  | "SouthEast"
  | "SouthWest"
  | "West";

const win = IS_TAURI ? getCurrentWindow() : null;

const grips: { dir: ResizeDirection; style: string }[] = [
  { dir: "North", style: "inset: 0 8px auto 8px; height: 4px; cursor: ns-resize" },
  { dir: "South", style: "inset: auto 8px 0 8px; height: 4px; cursor: ns-resize" },
  { dir: "West", style: "inset: 8px auto 8px 0; width: 4px; cursor: ew-resize" },
  { dir: "East", style: "inset: 8px 0 8px auto; width: 4px; cursor: ew-resize" },
  { dir: "NorthWest", style: "inset: 0 auto auto 0; width: 10px; height: 10px; cursor: nwse-resize" },
  { dir: "NorthEast", style: "inset: 0 0 auto auto; width: 10px; height: 10px; cursor: nesw-resize" },
  { dir: "SouthWest", style: "inset: auto auto 0 0; width: 10px; height: 10px; cursor: nesw-resize" },
  { dir: "SouthEast", style: "inset: auto 0 0 auto; width: 10px; height: 10px; cursor: nwse-resize" },
];

function resize(dir: ResizeDirection) {
  void win?.startResizeDragging(dir).catch(() => {});
}
</script>

<template>
  <div class="grips" aria-hidden="true">
    <div
      v-for="grip in grips"
      :key="grip.dir"
      class="grip"
      :style="grip.style"
      @mousedown.prevent="resize(grip.dir)"
    />
  </div>
</template>

<style scoped>
.grips {
  position: fixed;
  inset: 0;
  z-index: 80;
  pointer-events: none;
}
.grip {
  position: absolute;
  pointer-events: auto;
}
</style>
