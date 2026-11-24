/**
 * Frame-accurate UI probe (debug builds only).
 *
 * The drive-switcher popup and the view swap animate through the Web Animations
 * API because WebKitGTK steps CSS transitions badly. That fixed the stall, but
 * there is no way to see from inside the webview what the compositor actually
 * painted — a one-frame flash looks identical to a smooth fade in JS.
 *
 * So every animation sample is shipped to the Rust side, which appends it to
 * `app.log`. Reading the log back gives, per frame: the animation's
 * `currentTime`, the element's computed opacity/transform, whether it is still
 * in the document, and how many milliseconds passed since the previous sample.
 * A stall shows up as a timestamp gap; a flash shows up as a sample where the
 * element is detached (`conn=0`) or its opacity snaps between frames.
 *
 * Only compiled in when the app runs under `tauri dev`; `debug_probe` does not
 * exist in release builds, so the dynamic import is guarded by `import.meta.env.DEV`.
 */

import { invoke } from "@tauri-apps/api/core";

const enabled = import.meta.env.DEV;

/** Sends one sample. Never throws, never blocks the animation. */
function emit(tag: string, sample: Record<string, string | number | boolean>) {
  if (!enabled) return;
  const line = Object.entries(sample)
    .map(([k, v]) => `${k}=${typeof v === "number" ? Math.round(v * 100) / 100 : v}`)
    .join(" ");
  void invoke("debug_probe", { tag, sample: line }).catch(() => {});
}

/**
 * Watches one element for the lifetime of an animation and reports every frame.
 *
 * `raf` is the only clock that matches what the compositor is asked to paint, so
 * the sampler runs on `requestAnimationFrame` rather than a timer. It stops when
 * the animation finishes or the element leaves the document.
 */
export function probeAnimation(tag: string, el: Element, anim: Animation) {
  if (!enabled) return;
  const node = el as HTMLElement;
  const start = performance.now();
  let last = start;
  let frames = 0;

  const tick = () => {
    const now = performance.now();
    const style = getComputedStyle(node);
    frames += 1;
    emit(tag, {
      f: frames,
      t: now - start,
      dt: now - last,
      ct: anim.currentTime === null ? -1 : Number(anim.currentTime),
      op: Number.parseFloat(style.opacity || "1"),
      tr: style.transform === "none" ? "none" : style.transform,
      conn: node.isConnected ? 1 : 0,
      vis: style.visibility,
      disp: style.display,
    });
    last = now;
    if (anim.playState === "running" && node.isConnected) {
      requestAnimationFrame(tick);
    } else {
      // One final sample after the animation settles, so the log shows the
      // resting state that the user ends up looking at.
      emit(tag, {
        f: frames + 1,
        t: performance.now() - start,
        dt: performance.now() - last,
        ct: anim.currentTime === null ? -1 : Number(anim.currentTime),
        op: Number.parseFloat(getComputedStyle(node).opacity || "1"),
        tr: getComputedStyle(node).transform,
        conn: node.isConnected ? 1 : 0,
        vis: getComputedStyle(node).visibility,
        disp: getComputedStyle(node).display,
        final: 1,
      });
    }
  };

  emit(tag, { f: 0, t: 0, dt: 0, ct: anim.currentTime === null ? -1 : Number(anim.currentTime), op: -1, tr: "start", conn: 1, vis: "start", disp: "start" });
  requestAnimationFrame(tick);
}

/** Logs a single point-in-time sample, for things that are not animations. */
export function probeOnce(tag: string, el: Element | null, extra: Record<string, string | number> = {}) {
  if (!enabled || !el) return;
  const node = el as HTMLElement;
  const style = getComputedStyle(node);
  emit(tag, {
    ...extra,
    op: Number.parseFloat(style.opacity || "1"),
    tr: style.transform,
    conn: node.isConnected ? 1 : 0,
    vis: style.visibility,
    disp: style.display,
    cls: node.className,
  });
}
