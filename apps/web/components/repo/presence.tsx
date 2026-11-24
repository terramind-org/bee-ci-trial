/**
 * Who is working somewhere, as a small avatar stack. Hovering or focusing it shows one plain
 * sentence ("triage is editing src/auth (Rotating session ids)."). No panel, no list. The
 * sentence is the button's own text, so it is also its accessible name.
 */
import { Avatar, toAvatar } from "@/components/avatar";
import type { Lease, Principal } from "@/lib/api/types";
import { pathText } from "@/lib/api/ids";

/** Whether a lease on `leased` covers `path` or something below it (both repo-relative). */
export function leaseTouches(leased: string, path: string): boolean {
  const a = leased.replace(/\/+$/, "");
  return a === path || a.startsWith(path + "/") || path.startsWith(a + "/");
}

/** Joins names as prose: "a", "a and b", "a, b and c". */
export function listing(names: string[]): string {
  if (names.length <= 1) return names[0] ?? "";
  return names.slice(0, -1).join(", ") + " and " + names[names.length - 1];
}

/** "triage is editing src/auth (Rotating session ids), and Lin Zhou is editing src/billing." */
export function editingSentence(leases: Lease[], under?: string): string {
  const parts = leases.map((l) => {
    const paths = l.paths.map(pathText).filter((p) => under === undefined || leaseTouches(p, under));
    const note = l.note ? " (" + l.note + ")" : "";
    return (l.holder?.display ?? "Someone") + " is editing " + (listing(paths) || "the whole repository") + note;
  });
  return listing(parts) + ".";
}

/** The distinct holders of a set of leases, in the order they first appear. */
export function holders(leases: Lease[]): Principal[] {
  const seen = new Map<string, Principal>();
  for (const l of leases) if (l.holder && !seen.has(l.holder.id)) seen.set(l.holder.id, l.holder);
  return [...seen.values()];
}

export function Presence({ people, sentence, size = 28 }: { people: Principal[]; sentence: string; size?: number }) {
  if (!people.length) return null;
  return (
    <button type="button" className="group relative inline-flex min-h-6 items-center rounded-full focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-(--focus)">
      <span className="flex -space-x-2" aria-hidden="true">
        {people.slice(0, 4).map((p) => (
          <span key={p.id} className="rounded-full ring-2 ring-bg">
            <Avatar principal={toAvatar(p)} size={size} />
          </span>
        ))}
      </span>
      <span className="sr-only">{sentence}</span>
      <span aria-hidden="true" className="pointer-events-none absolute hidden right-0 bottom-[calc(100%+8px)] z-20 w-max max-w-72 rounded-lg border border-line bg-popover px-3 py-2 text-left text-sm text-ink shadow-overlay group-hover:block group-focus-visible:block">
        {sentence}
      </span>
    </button>
  );
}
