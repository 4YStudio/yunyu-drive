/**
 * Shared popup motion (drive switcher, account bubble, context menu).
 *
 * These used to be a CSS `transition` under a `<Transition name="pop">`. On
 * WebKitGTK that transition is not stepped frame by frame: the compositor
 * paints one intermediate frame, then sits on it for the whole duration and
 * jumps to the final state at the end. Measured with a frame-accurate probe
 * (`get_snapshot`, 1100x720 @2x): only 3 unique frames per open, with the
 * largest gap between two consecutive paints at 210 ms — that stall-then-jump
 * is what reads as the "twitch" around the drive switcher, and it also drags a
 * full-content-area repaint (21k–43k px outside the popup) along with it.
 *
 * The identical animation driven through `element.animate()` is stepped
 * normally: 10 unique frames, largest gap 26 ms. So the popups animate with the
 * Web Animations API, exactly like the view swap in `App.vue`.
 *
 * `fill: "both"` is required, not cosmetic — see the long note in `App.vue`.
 * Without it WebKitGTK drops the compositing layer the moment the animation
 * ends and repaints the area underneath once from scratch.
 */

export const prefersReducedMotion =
  typeof window !== "undefined" &&
  window.matchMedia?.("(prefers-reduced-motion: reduce)").matches === true;

const EASE = "cubic-bezier(0.22, 1, 0.36, 1)";

function run(el: Element, frames: Keyframe[], duration: number, done: () => void) {
  const anim = (el as HTMLElement).animate(frames, { duration, easing: EASE, fill: "both" });
  // `finished` rejects if the element is torn down mid-flight; either way the
  // transition has to be told it is over or the element stays pinned.
  anim.finished.then(done, done);
  return anim;
}

/** Slide+fade a popup in from 4px above its resting place. */
export function popEnter(el: Element, done: () => void) {
  if (prefersReducedMotion) {
    done();
    return;
  }
  run(
    el,
    [
      { opacity: "0", transform: "translateY(-4px)" },
      { opacity: "1", transform: "translateY(0)" },
    ],
    190,
    done,
  );
}

/** Slide+fade it back out. Faster than the entrance so dismissal feels instant. */
export function popLeave(el: Element, done: () => void) {
  if (prefersReducedMotion) {
    done();
    return;
  }
  run(
    el,
    [
      { opacity: "1", transform: "translateY(0)" },
      { opacity: "0", transform: "translateY(-4px)" },
    ],
    110,
    done,
  );
}

/** Ready-to-bind hook object for `<Transition :css="false" v-bind="pop">`. */
export const pop = { onEnter: popEnter, onLeave: popLeave };
