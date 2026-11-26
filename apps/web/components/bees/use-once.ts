"use client";

import { useCallback, useId, useState } from "react";
import { canAnimate, createEventClaims, nextSnapshot } from "./event-keys";

const played = createEventClaims();
const pending = new Set<string>();

/** Baseline the first snapshot, then return keys for confirmed changes while this view is open. */
export function useEventTransitions(scope: string, values: Readonly<Record<string, string>>, includeNew = false) {
  const instance = useId();
  const [previous, setPrevious] = useState(() => nextSnapshot(undefined, scope, values, instance));
  const current = nextSnapshot(previous, scope, values, instance, includeNew);

  // Updating during render retries this component before its descendants see stale keys.
  if (current !== previous) setPrevious(current);
  return current.keys;
}

/** Claim a committed event, including when its picture is still. Never replay a claimed key. */
export function useOnce(eventKey: string | undefined, still = false) {
  return useCallback((node: SVGSVGElement | null) => {
    if (!node || eventKey === undefined || still) return;

    const view = node.ownerDocument.defaultView;
    if (!view) return;

    let armed = false;
    const started = () => { played.claim(eventKey); };
    const rest = () => {
      if (!armed) return;
      armed = false;
      pending.delete(eventKey);
      delete node.dataset.play;
      node.removeEventListener("animationstart", started);
      node.removeEventListener("animationend", rest);
      node.removeEventListener("animationcancel", rest);
    };

    // Live refresh can briefly replace refs before the next paint. Wait for that frame and
    // only consume an animated event once the browser has actually started its animation.
    const frame = view.requestAnimationFrame(() => {
      if (!node.isConnected || played.has(eventKey) || pending.has(eventKey)) return;
      const mode = node.ownerDocument.documentElement.dataset.mode;
      const reduced = view.matchMedia("(prefers-reduced-motion: reduce)").matches;
      if (!canAnimate(mode, reduced, still)) {
        played.claim(eventKey);
        return;
      }

      armed = true;
      pending.add(eventKey);
      node.addEventListener("animationstart", started);
      node.addEventListener("animationend", rest);
      node.addEventListener("animationcancel", rest);
      node.dataset.play = "";
    });

    return () => {
      view.cancelAnimationFrame(frame);
      rest();
    };
  }, [eventKey, still]);
}
