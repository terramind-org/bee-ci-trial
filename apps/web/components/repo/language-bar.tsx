/** The language mix of a repo: one segmented bar and a legend that prints every share as text. */
import type { LanguageStat } from "@/lib/api/types";

const HEX = /^#[0-9a-f]{6}$/i;
/** Used when the server sends no colour for a language: zinc steps, so the bar never goes garish. */
const FALLBACK = ["var(--ink-3)", "var(--line-strong)", "var(--ink-2)", "var(--hover)"];

export function percentText(p: number): string {
  return p > 0 && p < 0.1 ? "<0.1%" : (Math.round(p * 10) / 10).toString() + "%";
}

export function LanguageBar({ languages }: { languages: LanguageStat[] }) {
  const shown = languages.filter((l) => l.percentage > 0);
  if (!shown.length) return null;
  const colour = (l: LanguageStat, i: number) => (HEX.test(l.color) ? l.color : FALLBACK[i % FALLBACK.length]);
  return (
    <div className="flex flex-col gap-2">
      <div role="img" aria-label={"Languages: " + shown.map((l) => l.name + " " + percentText(l.percentage)).join(", ")} className="flex h-2 overflow-hidden rounded-full bg-sunken">
        {shown.map((l, i) => (
          <span key={l.name} style={{ width: l.percentage + "%", background: colour(l, i) }} className="block h-full min-w-px" />
        ))}
      </div>
      <ul className="flex flex-wrap gap-x-3 gap-y-1 text-xs text-ink-2" aria-hidden="true">
        {shown.map((l, i) => (
          <li key={l.name} className="inline-flex items-center gap-1.5">
            <span className="size-2 rounded-full" style={{ background: colour(l, i) }} />
            <span className="text-ink">{l.name}</span>
            <span className="num text-ink-3">{percentText(l.percentage)}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}
