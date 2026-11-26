/**
 * The few layout pieces every redesigned screen is made of (docs/design/tokens.md, "Rhythm"):
 * the mark beside the person who answers, a list of rows between hairlines, a section with its
 * label in the left column, label and value pairs, and a state as a word with a shape.
 *
 * Text and layout only. Anything a person presses comes from the kit (components/kit).
 */
import type { ReactNode } from "react";
import { cx } from "./kit/cx";

/**
 * The yellow mark, then the name of the person who answers for the work. The mark is a dot,
 * never text, and sits nowhere else: it means only "a person answers for this".
 */
export function Mark({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <span className={cx("inline-flex items-center gap-2", className)} data-mark>
      <span className="dot" aria-hidden="true" />
      <span>{children}</span>
    </span>
  );
}

/** Rows between hairlines: one above the first row, one under each. */
export function Rows({ children, className, label, testId }: { children: ReactNode; className?: string; label?: string; testId?: string }) {
  return (
    <ul aria-label={label} data-testid={testId} className={cx("border-t border-line", className)}>
      {children}
    </ul>
  );
}

/** One row: a sentence in the row size, light, with a small grey line under it. */
export function Row({ children, meta, action, className, ...rest }: { children: ReactNode; meta?: ReactNode; action?: ReactNode; className?: string } & Record<`data-${string}`, string | undefined>) {
  return (
    <li className={cx("flex flex-col gap-3 border-b border-line py-5 sm:flex-row sm:items-start sm:gap-6", className)} {...rest}>
      <div className="min-w-0 flex-1">
        <p className="text-lg font-light tracking-[-0.01em] text-ink [overflow-wrap:anywhere]">{children}</p>
        {meta ? <p className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-ink-2">{meta}</p> : null}
      </div>
      {action ? <div className="shrink-0">{action}</div> : null}
    </li>
  );
}

/** The dot between two facts on a meta line. */
export function Sep() {
  return <span aria-hidden="true">·</span>;
}

/**
 * A section of a page: a hairline, the label in a narrow left column and the content beside it
 * (under it on a phone). `id` names the heading so the region is labelled.
 */
export function Sect({ id, title, children, className, level = 2 }: { id: string; title: ReactNode; children: ReactNode; className?: string; level?: 2 | 3 }) {
  const H = level === 2 ? "h2" : "h3";
  return (
    <section aria-labelledby={id} className={cx("grid min-w-0 grid-cols-1 gap-x-6 gap-y-3 border-t border-line pt-5 md:grid-cols-[160px_minmax(0,1fr)]", className)}>
      <H id={id} className="text-xs font-medium tracking-normal text-ink-2">
        {title}
      </H>
      <div className="min-w-0">{children}</div>
    </section>
  );
}

/** Label and value pairs. A pair with no value is left out by passing `null`. */
export function Kv({ rows }: { rows: ([label: string, value: ReactNode] | null | false | undefined)[] }) {
  const shown = rows.filter((r): r is [string, ReactNode] => !!r);
  return (
    <dl className="grid grid-cols-[minmax(0,130px)_minmax(0,1fr)] gap-x-[18px] gap-y-1.5 text-base">
      {shown.map(([label, value]) => (
        <div key={label} className="contents">
          <dt className="text-ink-2">{label}</dt>
          <dd className="flex min-w-0 flex-wrap items-center gap-x-2 text-ink [overflow-wrap:anywhere]">{value}</dd>
        </div>
      ))}
    </dl>
  );
}

export type Shape = "open" | "running" | "passed" | "failed" | "merged" | "closed" | "waiting";

/**
 * The shape beside a state's word. Ink and greys only: a ring for open, a half-filled ring for
 * running, a tick, a cross, a filled dot for merged, a dashed ring for closed, a hollow dash for
 * waiting. The word carries the meaning; the shape lets the eye find it in a list.
 */
export function StateShape({ shape }: { shape: Shape }) {
  const common = { width: 12, height: 12, viewBox: "0 0 12 12", "aria-hidden": true as const, className: "shrink-0" };
  switch (shape) {
    case "merged":
      return (
        <svg {...common}>
          <circle cx="6" cy="6" r="5" fill="currentColor" />
        </svg>
      );
    case "closed":
      return (
        <svg {...common}>
          <circle cx="6" cy="6" r="4.5" fill="none" stroke="currentColor" strokeWidth="1.3" strokeDasharray="2 2.2" />
        </svg>
      );
    case "running":
      return (
        <svg {...common}>
          <circle cx="6" cy="6" r="4.5" fill="none" stroke="currentColor" strokeWidth="1.3" />
          <path d="M6 1.5a4.5 4.5 0 0 1 0 9z" fill="currentColor" />
        </svg>
      );
    case "passed":
      return (
        <svg {...common}>
          <path d="M2 6.4 4.8 9 10 3.2" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      );
    case "failed":
      return (
        <svg {...common}>
          <path d="M2.5 2.5l7 7m0-7-7 7" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
        </svg>
      );
    case "waiting":
      return (
        <svg {...common}>
          <circle cx="6" cy="6" r="4.5" fill="none" stroke="currentColor" strokeWidth="1.3" />
          <path d="M3.6 6h4.8" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
        </svg>
      );
    default:
      return (
        <svg {...common}>
          <circle cx="6" cy="6" r="4.5" fill="none" stroke="currentColor" strokeWidth="1.3" />
        </svg>
      );
  }
}

/** A state as a word with its shape: "◐ Checks running". */
export function State({ shape, children, className, testId }: { shape: Shape; children: ReactNode; className?: string; testId?: string }) {
  return (
    <span className={cx("inline-flex items-center gap-1.5 text-ink", className)} data-state={shape} data-testid={testId}>
      <StateShape shape={shape} />
      <span>{children}</span>
    </span>
  );
}

/** A quiet closing line under a list: a count, a note, a link to more. */
export function Quiet({ children, className }: { children: ReactNode; className?: string }) {
  return <p className={cx("max-w-[44em] text-base text-ink-2", className)}>{children}</p>;
}

/** Branch and target, in mono: `ws/billing/tax → main`. With no known branch, the target alone. */
export function BranchLine({ branch, target }: { branch?: string; target: string }) {
  return (
    <span className="mono [overflow-wrap:anywhere]">
      {branch ? (
        <>
          {branch} <span aria-hidden="true">→</span>
          <span className="sr-only"> into </span>{" "}
        </>
      ) : (
        <>
          <span aria-hidden="true">→</span>
          <span className="sr-only">into</span>{" "}
        </>
      )}
      {target}
    </span>
  );
}
