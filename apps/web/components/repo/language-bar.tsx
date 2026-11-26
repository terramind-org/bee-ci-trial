/**
 * The language mix of a repo: one segmented bar and a legend that prints every share as text.
 * Ink and greys only (docs/design/tokens.md: one colour, and it is not for languages): the
 * largest share is ink, the next ones step down. The server's per-language colour is not used.
 */
import type { LanguageStat } from "@/lib/api/types";

/** Largest share first: ink, the second grey, the faint grey, then the strong hairline. */
const STEPS = ["var(--ink)", "var(--ink-2)", "var(--faint)", "var(--line-strong)"];

export function percentText(p: number): string {
  return p > 0 && p < 0.1 ? "<0.1%" : (Math.round(p * 10) / 10).toString() + "%";
}

/** "Rust 90%, Shell 10%". */
export function languagesText(languages: LanguageStat[]): string {
  return languages
    .filter((l) => l.percentage > 0)
    .map((l) => l.name + " " + percentText(l.percentage))
    .join(", ");
}

export function LanguageBar({ languages }: { languages: LanguageStat[] }) {
  const shown = languages.filter((l) => l.percentage > 0);
  if (!shown.length) return null;
  const tone = (i: number) => STEPS[Math.min(i, STEPS.length - 1)];
  return (
    <div className="flex max-w-md flex-col gap-2">
      <div role="img" aria-label={"Languages: " + languagesText(shown)} className="flex h-1.5 gap-px overflow-hidden rounded-full">
        {shown.map((l, i) => (
          <span key={l.name} style={{ width: l.percentage + "%", background: tone(i) }} className="block h-full min-w-px" />
        ))}
      </div>
      <ul className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-ink-2" aria-hidden="true">
        {shown.map((l, i) => (
          <li key={l.name} className="inline-flex items-center gap-1.5">
            <span className="size-2 rounded-full" style={{ background: tone(i) }} />
            <span className="text-ink">{l.name}</span>
            <span className="num">{percentText(l.percentage)}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}
