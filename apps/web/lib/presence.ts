/**
 * Pure bookkeeping behind usePresence (components/motion.tsx): which rows just left a list and
 * where to keep drawing them while they fade out. No React, no clocks, so it is unit-tested.
 */
export type Ghost<T> = { item: T; key: string; index: number; gen: number };

export interface PresenceState<T> {
  sig: string;
  context: string;
  items: T[];
  gen: number;
  ghosts: Ghost<T>[];
}

export function signature<T>(items: T[], keyOf: (t: T) => string): string {
  return items.map(keyOf).join("\u0000");
}

export function initialPresence<T>(items: T[], keyOf: (t: T) => string, context: string): PresenceState<T> {
  return { sig: signature(items, keyOf), context, items, gen: 0, ghosts: [] };
}

/**
 * The next state when the list changes, or null when nothing did. Rows that vanished become ghosts
 * of a new generation at their old index; a row that comes back stops being a ghost. A changed
 * `context` (a new filter) drops every ghost: that is a new view, not data leaving.
 */
export function stepPresence<T>(state: PresenceState<T>, items: T[], keyOf: (t: T) => string, context: string): PresenceState<T> | null {
  const sig = signature(items, keyOf);
  if (state.sig === sig && state.context === context) return null;
  const gen = state.gen + 1;
  if (state.context !== context) return { sig, context, items, gen, ghosts: [] };
  const now = new Set(items.map(keyOf));
  const gone = state.items.flatMap((item, index) => {
    const key = keyOf(item);
    return now.has(key) ? [] : [{ item, key, index, gen }];
  });
  return { sig, context, items, gen, ghosts: [...state.ghosts.filter((g) => !now.has(g.key)), ...gone] };
}

/** Items to render, in order, with leaving rows spliced back in where they were. */
export function withGhosts<T>(items: T[], ghosts: Ghost<T>[], keyOf: (t: T) => string): { item: T; key: string; leaving: boolean }[] {
  const out = items.map((item) => ({ item, key: keyOf(item), leaving: false }));
  for (const g of [...ghosts].sort((a, b) => a.index - b.index)) out.splice(Math.min(g.index, out.length), 0, { item: g.item, key: g.key, leaving: true });
  return out;
}

/**
 * When to run a coalesced refresh (components/live-refresh.tsx): after `quietMs` without another
 * event, but never later than `maxMs` after the first event of the burst.
 */
export function refreshDelay(firstAt: number, now: number, quietMs = 250, maxMs = 1000): number {
  return Math.max(0, Math.min(quietMs, firstAt + maxMs - now));
}
