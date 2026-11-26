/** Class helpers for the kit. No dependency: the kit's needs are small and known. */

export function cx(...parts: (string | false | null | undefined)[]): string {
  return parts.filter(Boolean).join(" ");
}

/**
 * Which of a control's own properties a caller's class replaces. Tailwind decides conflicts by
 * its stylesheet's order, not the order in `class`, so a caller's `h-7` beside the kit's
 * `h-[46px]` would win or lose by accident. `tw` drops the kit's class for a property the
 * caller sets, for the handful of properties callers do override: size, padding, gap, radius, text size, the
 * distance from the top, and the height limit.
 */
const GROUPS: [RegExp, string[]][] = [
  [/^size-/, ["h", "w"]],
  [/^h-/, ["h"]],
  [/^w-/, ["w"]],
  [/^min-w-/, ["min-w"]],
  [/^px-/, ["px"]],
  [/^py-/, ["py"]],
  [/^p-/, ["px", "py"]],
  [/^gap-/, ["gap"]],
  [/^rounded(-|$)/, ["rounded"]],
  [/^text-(2xs|xs|sm|base|md|lg|xl|2xl|3xl|\[\d)/, ["size"]],
  [/^top-/, ["top"]],
  [/^max-h-/, ["max-h"]],
];

function groupsOf(token: string): string[] {
  // Variants (`hover:`, `md:`) style another state or width, so they replace nothing.
  if (token.includes(":")) return [];
  for (const [re, groups] of GROUPS) if (re.test(token)) return groups;
  return [];
}

/** `base` with the caller's `extra` after it, minus the base classes `extra` replaces. */
export function tw(base: string, extra?: string | false | null): string {
  if (!extra) return base;
  const replaced = new Set(extra.split(/\s+/).flatMap(groupsOf));
  if (!replaced.size) return base + " " + extra;
  const kept = base.split(/\s+/).filter((t) => !groupsOf(t).some((g) => replaced.has(g)));
  return kept.join(" ") + " " + extra;
}
