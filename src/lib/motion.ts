/**
 * Shared motion for the whole app — the single place where the interface's
 * personality is defined.
 *
 * Everything that moves goes through motion-v, which drives the Web Animations
 * API and touches only `transform` / `opacity` / `height`. That is not
 * decoration: on WebKitGTK 2.54 a CSS `transition` on these surfaces is not
 * stepped frame by frame at all (one intermediate paint, then a 200 ms stall,
 * then a jump), and animating anything that ends on `transform: none` tears the
 * compositing layer down and repaints everything behind it. Both read as a
 * flicker.
 *
 * ## The motion language
 *
 * Three verbs, and nothing else:
 *
 *   `panel*`  something that belongs to an anchor opens **out of that anchor** —
 *             the drive switcher unrolls from zero height. No scale: a shadowed
 *             panel that shrinks is re-rasterised on every frame.
 *   `pop*`    something that appears *over* the page (context menu, account
 *             bubble) springs up from just under its anchor with a hit of
 *             overshoot.
 *   `item*`   contents of a panel arrive in order, a few milliseconds apart, so
 *             a menu reads as unrolling rather than as a bitmap being swapped.
 *
 * `thumb*` is the one continuous motion in the app: the segmented control's and
 * the theme switch's highlight both *travel* to the new slot instead of blinking
 * there. It is deliberately the springiest thing here, because a highlight that
 * slides is the cheapest way to make a static sidebar feel alive.
 *
 * Everything scales no further than 0.9 and translates no more than 14px. This
 * is a file manager, not a landing page.
 *
 * ## Two hard-won rules
 *
 *   1. **A full-stage element never animates `transform`.** The router outlet is
 *      `position: absolute; inset: 0`; promoting a layer that size and then
 *      dropping the promotion leaves the raster *offset* on WebKitGTK for a
 *      frame — the content visibly jumps a few pixels and snaps back. The route
 *      transition therefore fades and nothing else, and the life comes from the
 *      rows staggering in underneath.
 *   2. **`will-change` and `contain: paint` never sit in CSS.** motion-v owns
 *      both while an animation runs and drops them when it settles; leaving them
 *      on permanently keeps a shadowed layer promoted, and WebKitGTK re-uploads
 *      it on every frame.
 */

import type { Transition } from "motion-v";

/** Reduced motion is honoured everywhere: motion-v jumps straight to the end. */
export const reducedMotion =
  typeof window !== "undefined" &&
  window.matchMedia?.("(prefers-reduced-motion: reduce)").matches === true;

const EASE_OUT = [0.22, 1, 0.36, 1] as const;
const EASE_IN = [0.4, 0, 1, 1] as const;

const still = (): Transition => ({ duration: 0 });

/* ------------------------------------------------------------------ panels */

/**
 * A panel that opens from its anchor: the drive switcher.
 *
 * Height, not scale. Scaling a bordered, shadowed panel makes the compositor
 * re-rasterise the blur every frame, and on WebKitGTK the last frame of that
 * shrinks the shadow by a couple of pixels before the node is removed — which is
 * exactly the "抽搐" you see when a menu closes. Growing the box height instead
 * never touches the layer's scale, so the shadow is painted once.
 *
 * `height: "auto"` is resolved by motion-v from the measured content, so this
 * works for any number of drives.
 */
export const panelHidden = { height: 0, opacity: 0 };
export const panelShown = { height: "auto", opacity: 1 };

export const panelTransition: Transition = reducedMotion
  ? still()
  : { duration: 0.26, ease: [...EASE_OUT] };

/** Collapse is quicker than the reveal and never overshoots. */
export const panelLeaveTransition: Transition = reducedMotion
  ? still()
  : { duration: 0.17, ease: [...EASE_IN] };

/* ------------------------------------------------------------------- pops */

/**
 * Overlays that spring up from their anchor: context menu, account bubble.
 *
 * `type: "spring"` rather than a bezier because the settle is what makes a
 * desktop menu feel physical — and because motion-v keeps spring state across
 * interrupts, so clicking twice mid-flight retargets instead of snapping back to
 * the start.
 */
export const popTransition: Transition = reducedMotion
  ? still()
  : { type: "spring", stiffness: 540, damping: 30, mass: 0.7 };

export const popLeaveTransition: Transition = reducedMotion
  ? still()
  : { duration: 0.12, ease: [...EASE_IN] };

export const popHidden = { opacity: 0, scale: 0.96, y: -4 };
export const popShown = { opacity: 1, scale: 1, y: 0 };

/* ----------------------------------------------------------------- dialogs */

/** Modal: heavier than a menu, so it carries more travel and less speed. */
export const dialogTransition: Transition = reducedMotion
  ? still()
  : { type: "spring", stiffness: 420, damping: 30, mass: 0.85 };

export const dialogLeaveTransition: Transition = reducedMotion
  ? still()
  : { duration: 0.13, ease: [...EASE_IN] };

export const dialogHidden = { opacity: 0, scale: 0.94, y: 14 };
export const dialogShown = { opacity: 1, scale: 1, y: 0 };

/* ------------------------------------------------------------------- route */

/**
 * Screens.
 *
 * Opacity only — see rule 1 at the top of this file. `<KeepAlive>` replays the
 * enter animation when a cached screen is re-activated, which is fine here: a
 * fade-in is what a route change should look like, and it cannot shift layout.
 */
export const viewTransition: Transition = reducedMotion ? still() : { duration: 0.18, ease: "easeOut" };

export const viewHidden = { opacity: 0 };
export const viewShown = { opacity: 1 };

/* -------------------------------------------------------------------- rows */

/** A listing fades its rows in, but only the first `ROW_ANIM_LIMIT` of them.
 *  A folder with 800 files would otherwise spin up 800 simultaneous animations
 *  on every load — invisible work (they are off-screen anyway) and exactly the
 *  kind of burst that makes WebKitGTK drop a frame. Past the limit rows simply
 *  appear, which nobody can see because they are below the fold.
 *  The first few are staggered so the list reads as arriving rather than
 *  popping; the delay is capped so a fast scroll never catches up with it. */
export const ROW_ANIM_LIMIT = 40;
const ROW_STAGGER = 0.014;
const ROW_STAGGER_CAP = 8;

export const rowTransition: Transition = reducedMotion ? still() : { duration: 0.16, ease: [...EASE_OUT] };

export const rowHidden = { opacity: 0, y: -6 };
export const rowShown = { opacity: 1, y: 0 };

/** Spread onto a row: `<motion.div v-bind="rowEnter(i)">`. */
export function rowEnter(index: number) {
  if (reducedMotion || index >= ROW_ANIM_LIMIT) {
    return { initial: false as const, animate: rowShown, transition: still() };
  }
  return {
    initial: rowHidden,
    animate: rowShown,
    transition: {
      ...rowTransition,
      delay: Math.min(index, ROW_STAGGER_CAP) * ROW_STAGGER,
    },
  };
}

/**
 * Panel contents: each row of a menu arrives a beat after the one above it.
 *
 * The delay is what turns "a bitmap appeared" into "a list unrolled", and it is
 * capped at six entries so a long menu never feels slow to respond.
 */
export const itemHidden = { opacity: 0, y: -3 };
export const itemShown = { opacity: 1, y: 0 };

export function itemEnter(index: number) {
  if (reducedMotion) {
    return { initial: false as const, animate: itemShown, transition: still() };
  }
  return {
    initial: itemHidden,
    animate: itemShown,
    transition: { duration: 0.18, ease: [...EASE_OUT], delay: 0.05 + Math.min(index, 6) * 0.022 },
  };
}

/* ------------------------------------------------------------------ thumbs */

/**
 * Segmented control / theme switch / nav highlight: one continuous motion.
 *
 * Fast, and **critically damped on purpose**: for `stiffness 640 / mass 0.6`
 * the critical damping is `2 * sqrt(k * m)` ≈ 39, so anything below that
 * overshoots past the slot and comes back. A highlight that slides past its
 * target and snaps back does not read as playful, it reads as a twitch (抽搐) —
 * and on WebKitGTK it also re-rasterises the moving layer twice. The life in
 * this motion comes from the **travel** (it visibly slides between slots), not
 * from the bounce at the end.
 */
export const thumbTransition: Transition = reducedMotion
  ? still()
  : { type: "spring", stiffness: 640, damping: 42, mass: 0.6 };

/* -------------------------------------------------------------------- bars */

/**
 * Progress tracks (download bar, quota bar).
 *
 * These were the last CSS `transition: width` left in the app, and width is a
 * layout property: every tick re-ran layout for the whole row *and*, on
 * WebKitGTK, stalled mid-transition like every other CSS transition here. Let
 * motion drive the width instead so a progress bar advances smoothly between
 * the once-a-second updates.
 */
export const barTransition: Transition = reducedMotion
  ? still()
  : { duration: 0.34, ease: [...EASE_OUT] };

/* ------------------------------------------------------------------ toasts */

export const toastTransition: Transition = reducedMotion
  ? still()
  : { type: "spring", stiffness: 440, damping: 30, mass: 0.6 };

export const toastHidden = { opacity: 0, y: 12, scale: 0.94 };
export const toastShown = { opacity: 1, y: 0, scale: 1 };

/* ------------------------------------------------------------------- press */

/**
 * Press feedback, for `:while-press`.
 *
 * A 4% squeeze with no travel: on WebKitGTK a scaling button re-rasterises its
 * text, so this stays a transform the compositor can handle in one pass, and it
 * is released on the same spring the thumbs use so the whole UI answers at the
 * same rate.
 */
export const pressTransition: Transition = reducedMotion ? still() : { duration: 0.13, ease: [...EASE_IN] };

export const pressDown = { scale: 0.96 };

/* ---------------------------------------------------------------- travelling */

/**
 * Move a "slider" element with the Web Animations API instead of motion-v.
 *
 * Motion drives a travel animation by writing an inline `transform` every frame
 * and simply *leaving* the last value in place when it finishes. WebKitGTK
 * treats that hand-off differently from Chromium: at the moment the animation
 * stops being the source of the transform, the element is repainted once from
 * its *layout* position before the inline value is honoured again. On a
 * travelling highlight that shows up as the slider landing on the new row,
 * jumping back to the old one and coming forward again — three frames, very
 * visible, and impossible to reproduce in a Chromium browser.
 *
 * `fill: "both"` is what removes the gap: the animation keeps *holding* the
 * final value, so the element never has a frame where no transform applies. The
 * inline style is then written while the animation still fills, and the
 * animation is cancelled afterwards — the value never changes, the fill simply
 * stops being needed. Same recipe the view-transition code already uses.
 */
const running = new WeakMap<Element, Animation>();

export function slideTo(
  el: Element | null | undefined,
  from: string,
  to: string,
  duration = 0.3,
) {
  const node = el as HTMLElement | null;
  if (!node) return;
  running.get(node)?.cancel();
  // Always own an inline transform *before* any animation exists, so there is no
  // frame in which the element has no transform and falls back to its layout
  // position.
  node.style.transform = from;
  if (reducedMotion || duration <= 0) {
    node.style.transform = to;
    running.delete(node);
    return;
  }
  const anim = node.animate([{ transform: from }, { transform: to }], {
    duration: duration * 1000,
    easing: "cubic-bezier(0.22, 1, 0.36, 1)",
    fill: "both",
  });
  running.set(node, anim);
  // The animation is deliberately never cancelled when it finishes: `fill:
  // "both"` keeps holding the final transform, so the hand-off to the inline
  // style below cannot open a gap. Cancelling here is what re-created the
  // element's transform from scratch on WebKitGTK and flashed it at the layout
  // position.
  anim.onfinish = () => {
    node.style.transform = to;
  };
}
