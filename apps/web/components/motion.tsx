"use client";

import { useEffect, useLayoutEffect, useRef, useState, type ReactNode, type RefObject } from "react";
import { initialPresence, stepPresence, withGhosts } from "@/lib/presence";

/**
 * FLIP for keyed lists. Live data re-renders a list from the server; without this, rows that move
 * jump to their new place and new rows pop in. Each direct child with a `data-key` is measured
 * after every commit: a row that moved is drawn at its old offset and slides to the new one, a row
 * that was not there before fades in. Transform and opacity only, so it stays on the compositor
 * and never triggers layout. Nothing animates on first paint or under reduced motion.
 */
const EASE = "cubic-bezier(0.22, 1, 0.36, 1)";
// Moves use a gentler ease-out (cubic): quint's opening frames cover a third of the distance at
// once, which reads as a jump when a whole list is pushed down by a new row.
const MOVE_EASE = "cubic-bezier(0.33, 1, 0.68, 1)";
const MOVE_MS = 300;
const ENTER_MS = 320;

export function useFlip<T extends HTMLElement>(ref: RefObject<T | null>) {
  const tops = useRef<Map<string, number> | null>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    const prev = tops.current;
    const next = new Map<string, number>();
    // Only rows that are, or were, on screen animate: a 300-row feed should not run 300 tweens.
    const origin = el.getBoundingClientRect().top;
    const onScreen = (y: number) => origin + y < window.innerHeight + 80 && origin + y > -400;
    const entering: HTMLElement[] = [];
    for (const child of el.children) {
      const node = child as HTMLElement;
      const key = node.dataset.key;
      if (!key) continue;
      // offsetTop is relative to the list, so page scroll between renders does not read as movement.
      const top = node.offsetTop;
      next.set(key, top);
      if (!prev || reduce) continue;
      const before = prev.get(key);
      if (before === undefined) {
        if (onScreen(top)) entering.push(node);
      } else if (Math.abs(before - top) > 0.5 && (onScreen(top) || onScreen(before))) {
        node.animate([{ transform: `translateY(${before - top}px)` }, { transform: "translateY(0)" }], { duration: MOVE_MS, easing: MOVE_EASE });
      }
    }
    // A full replacement (new filter, new page of data) is not an arrival: animating every row
    // would read as a flash, so only a handful of genuinely new rows get the entrance.
    if (entering.length && entering.length <= Math.max(3, next.size / 3)) {
      entering.forEach((node, i) =>
        node.animate(
          [
            { opacity: 0, transform: "translateY(-6px)", backgroundColor: "var(--accent-soft)" },
            { opacity: 1, transform: "translateY(0)", backgroundColor: "var(--accent-soft)", offset: 0.4 },
            { opacity: 1, transform: "translateY(0)", backgroundColor: "transparent" },
          ],
          { duration: ENTER_MS + 600, delay: Math.min(i, 6) * 30, easing: EASE, fill: "backwards" },
        ),
      );
    }
    tops.current = next;
  });
}

/** A list (or a stack of panels, with `as="div"`) whose keyed children move and arrive smoothly. */
export function FlipList({ as = "ul", className, children, ...rest }: { as?: "ul" | "ol" | "div"; className?: string; children: ReactNode } & Record<`data-${string}` | `aria-${string}`, string | undefined>) {
  const ref = useRef<HTMLUListElement & HTMLOListElement & HTMLDivElement>(null);
  useFlip(ref);
  const Tag = as;
  return (
    <Tag ref={ref} className={className} {...rest}>
      {children}
    </Tag>
  );
}

/**
 * Re-keys its child on `value` so a changed label or number fades in instead of snapping. Use for
 * small live values inside rows (landing state, counts); the box keeps its size so nothing shifts.
 */
export function Swap({ value, className, children }: { value: string | number; className?: string; children: ReactNode }) {
  // Remembers the first value so the initial paint never animates; only later changes do.
  const [first] = useState(() => String(value));
  const changed = String(value) !== first;
  return (
    <span key={String(value)} className={[className, changed ? "animate-swap" : ""].filter(Boolean).join(" ") || undefined}>
      {children}
    </span>
  );
}

const LEAVE_MS = 200;

/**
 * Keeps rows that just disappeared on screen for a moment so they can fade out, instead of
 * vanishing in one frame. Returns the items to render, each flagged `leaving`; leaving rows sit
 * where they were and are dropped after 200ms, at which point useFlip slides the rest up.
 * When `context` changes (the user typed a new filter) removals are instant: that is a new view,
 * not data leaving. The bookkeeping is lib/presence.ts.
 */
export function usePresence<T>(items: T[], keyOf: (t: T) => string, context = ""): { item: T; key: string; leaving: boolean }[] {
  const [state, setState] = useState(() => initialPresence(items, keyOf, context));
  const next = stepPresence(state, items, keyOf, context);
  if (next) setState(next);
  const { ghosts } = state;
  // One timer per generation of leavers, never reset by later renders, so a steady stream of live
  // updates cannot keep a ghost on screen.
  const timers = useRef(new Map<number, ReturnType<typeof setTimeout>>());
  useEffect(() => {
    for (const gen of new Set(ghosts.map((g) => g.gen))) {
      if (timers.current.has(gen)) continue;
      timers.current.set(
        gen,
        setTimeout(() => {
          timers.current.delete(gen);
          setState((s) => ({ ...s, ghosts: s.ghosts.filter((g) => g.gen !== gen) }));
        }, LEAVE_MS),
      );
    }
  }, [ghosts]);
  useEffect(() => {
    const all = timers.current;
    return () => all.forEach(clearTimeout);
  }, []);
  // Live rows always come from the current props (same key, fresh content); only leaving rows are
  // drawn from the copy remembered when they left.
  return withGhosts(items, next?.ghosts ?? ghosts, keyOf);
}
