/**
 * The look of one tab, shared by real tabs (./tabs.tsx) and link tabs (./tab-links.tsx): a plain
 * word over the hairline, the current one in ink with a 1px ink underline.
 *
 * The two states are whole class strings, never one laid over the other: Tailwind settles two
 * colour classes on one element by its stylesheet's order, not the order in `class`, so
 * `border-transparent` beside `border-ink` left the current tab unmarked.
 */
const BASE = "inline-flex h-10 shrink-0 items-center gap-1.5 border-b text-base whitespace-nowrap transition-colors duration-[160ms]";

/** A tab that is not the current one. Real tabs add `data-[selected]:` variants to this. */
export const TAB = BASE + " border-transparent text-ink-2 hover:text-ink";
/** The current tab. */
export const TAB_ON = BASE + " border-ink text-ink";

export function tabClass(active: boolean): string {
  return active ? TAB_ON : TAB;
}
