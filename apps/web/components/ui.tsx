import { LoadingBee } from "./bees/pieces";
/**
 * The interface's pieces under their first names. The interactive ones (buttons, fields, menus,
 * dialogs, tabs) live in `components/kit`, built on UI SDK, and are re-exported here so pages
 * written before the kit keep their imports. What is defined here is layout and text only:
 * badges, key hints, a section, a page header, a number, a skeleton.
 *
 * The rhythm is docs/design/tokens.md: type and whitespace carry the structure, hairlines stand
 * in for boxes, large type is light, and mono is only for code, paths, refs and ids.
 */
import type { ReactNode } from "react";
import { cx } from "./kit/cx";
import { Swap } from "./motion";

export { cx } from "./kit/cx";
export { Button, ButtonLink, buttonClass } from "./kit/button";
export { EmptyState } from "./kit/empty";
export { Field, Input, Select, Switch, Textarea } from "./kit/field";
export { TabLinks } from "./kit/tab-links";

type Tone = "neutral" | "accent" | "ok" | "warn" | "bad" | "outline";
/** A badge is a word in a pill. The tone only tints it; the word carries the meaning. */
const TONES: Record<Tone, string> = {
  neutral: "bg-selected text-ink-2",
  accent: "text-ink shadow-[inset_0_0_0_1px_var(--line-strong)]",
  ok: "bg-ok-soft text-ok",
  warn: "bg-warn-soft text-warn",
  bad: "bg-bad-soft text-bad",
  outline: "text-ink-2 shadow-[inset_0_0_0_1px_var(--line)]",
};

export function Badge({ tone = "neutral", children, className, title }: { tone?: Tone; children: ReactNode; className?: string; title?: string }) {
  return (
    <span title={title} className={cx("inline-flex h-6 items-center gap-1 whitespace-nowrap rounded-full px-2.5 text-2xs font-medium", TONES[tone], className)}>
      {children}
    </span>
  );
}

export function Kbd({ children }: { children: ReactNode }) {
  return <kbd className="inline-flex h-6 min-w-6 items-center justify-center rounded-sm px-1.5 font-sans text-2xs font-medium text-ink-3 shadow-[inset_0_0_0_1px_var(--line)]">{children}</kbd>;
}

/**
 * A section of a page: a small grey label for a heading, optional actions on the right, and the
 * content below with no box around it. `title` is optional for a plain grouped section; pass
 * `id` so the heading labels the region. Separate sections with space, not borders.
 */
export function Panel({ title, actions, children, className, bodyClassName, id }: { title?: ReactNode; actions?: ReactNode; children: ReactNode; className?: string; bodyClassName?: string; id?: string }) {
  return (
    <section aria-labelledby={id && title ? id : undefined} className={cx("min-w-0", className)}>
      {title || actions ? (
        <header className="mb-3 flex min-h-9 flex-wrap items-center justify-between gap-x-4 gap-y-2">
          {title ? (
            <h2 id={id} className="text-xs font-medium tracking-normal text-ink-2">
              {title}
            </h2>
          ) : (
            <span />
          )}
          {actions ? <div className="flex items-center gap-2">{actions}</div> : null}
        </header>
      ) : null}
      <div className={cx(bodyClassName)}>{children}</div>
    </section>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return <span aria-hidden="true" className={cx("block rounded-sm bg-hover", className)} />;
}

export function SkeletonRows({ rows = 6 }: { rows?: number }) {
  return (
    <div className="relative divide-y divide-line">
      <div className="absolute end-0 bottom-full pointer-events-none"><LoadingBee /></div>
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="flex items-center gap-3 py-3.5">
          <Skeleton className="size-7 rounded-full" />
          <Skeleton className="h-4 flex-1" />
          <Skeleton className="h-4 w-16" />
        </div>
      ))}
    </div>
  );
}

/**
 * The top of a page: an optional crumb (`eyebrow`), the title large and light,
 * an optional lede (`lede`, one or two plain sentences), a meta line, and actions on the right.
 * The page's single "…" menu, when it has one, goes in `actions` last.
 */
export function PageHeader({ title, meta, actions, eyebrow, lede }: { title: ReactNode; meta?: ReactNode; actions?: ReactNode; eyebrow?: ReactNode; lede?: ReactNode }) {
  return (
    <header className="mb-10 flex flex-col gap-4 md:flex-row md:items-start md:justify-between">
      <div className="min-w-0">
        {eyebrow ? <div className="mb-2 text-base text-ink-3">{eyebrow}</div> : null}
        <h1 className="text-2xl text-ink [overflow-wrap:anywhere]">{title}</h1>
        {lede ? <p className="mt-2 max-w-2xl text-lg text-ink-2">{lede}</p> : null}
        {meta ? <div className="mt-3 flex flex-wrap items-center gap-x-4 gap-y-1 text-base text-ink-2">{meta}</div> : null}
      </div>
      {actions ? <div className="flex shrink-0 flex-wrap items-center gap-2">{actions}</div> : null}
    </header>
  );
}

export function Stat({ label, value, hint }: { label: string; value: ReactNode; hint?: ReactNode }) {
  return (
    <div className="min-w-0">
      <div className="text-2xs font-medium text-ink-2">{label}</div>
      <div className="num mt-1 text-2xl">{typeof value === "string" || typeof value === "number" ? <Swap value={value}>{value}</Swap> : value}</div>
      {hint ? <div className="text-sm text-ink-3">{hint}</div> : null}
    </div>
  );
}
