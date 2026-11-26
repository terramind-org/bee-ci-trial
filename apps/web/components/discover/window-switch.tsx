/** Choices whose options are links: the choice is in the URL, so it survives reload and share. Plain words, the current one underlined. */
import Link from "next/link";
import { tabClass } from "@/components/kit/tabs-style";
import { cx } from "@/components/ui";
import { SINCE_LABEL, type Since } from "./params";

export interface Segment {
  value: string;
  label: string;
  href: string;
}

export function Segments({ label, segments, current }: { label: string; segments: Segment[]; current: string }) {
  return (
    <nav aria-label={label} className="flex flex-wrap items-center gap-x-5 gap-y-1">
      {segments.map((s) => (
        <Link
          key={s.value}
          href={s.href}
          aria-current={s.value === current ? "page" : undefined}
          className={cx(tabClass(s.value === current), "min-h-8 py-1 text-sm")}
        >
          {s.label}
        </Link>
      ))}
    </nav>
  );
}

/** Today | This week | This month (or the subset a page offers). */
export function WindowSwitch({ current, options, hrefFor, label = "Time window" }: { current: Since; options: readonly Since[]; hrefFor: (since: Since) => string; label?: string }) {
  return <Segments label={label} current={current} segments={options.map((v) => ({ value: v, label: SINCE_LABEL[v], href: hrefFor(v) }))} />;
}
