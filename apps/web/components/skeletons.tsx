/**
 * Loading skeletons that occupy exactly the boxes the loaded page will. Each route's loading.tsx
 * composes these with the page's real title, description and tab labels, so the swap from
 * skeleton to data moves nothing: only placeholders turn into text. Heights here are measured
 * from the rendered pages (docs/design/tokens.md, "Loading"); change them together.
 */
import type { CSSProperties, ReactNode } from "react";
import { Icon } from "./icons";
import { Kbd, PageHeader, cx } from "./ui";

/** A placeholder occupying one line box of height `lh`, with a bar of the text's visual height. */
export function Line({ w, lh = 20, bar = 10, className }: { w: number | string; lh?: number; bar?: number; className?: string }) {
  return (
    // Never wider than its container: a phone-width page must not scroll sideways while loading.
    <span aria-hidden="true" className={cx("flex max-w-full items-center", className)} style={{ height: lh }}>
      <span className="block max-w-full animate-pulse rounded-[3px] bg-hover" style={{ width: w, height: bar }} />
    </span>
  );
}

/** Deterministic, varied widths so a list of placeholders reads like text, not a barcode. */
const WIDTHS = [62, 48, 71, 55, 40, 66, 52, 44, 58, 69, 46, 60];
const pct = (i: number, scale = 1) => Math.round(WIDTHS[i % WIDTHS.length]! * scale) + "%";

export function Skel({ children }: { children: ReactNode }) {
  return (
    // Held back 120ms, then faded in: a fast navigation never flashes a skeleton at all. The
    // status role is what lets it carry an aria-label: axe refuses one on a role-less div.
    <div role="status" aria-busy="true" aria-label="Loading" className="[animation:fade_200ms_var(--ease-out)_120ms_both]">
      {children}
    </div>
  );
}

/** PageHeader with the page's real title and description; `meta` omitted means a data-bound line. */
export function HeaderSkeleton({ title, meta, eyebrow, metaWidth = 420 }: { title?: string; meta?: string; eyebrow?: boolean; metaWidth?: number }) {
  return (
    <PageHeader
      eyebrow={eyebrow ? <Line w={48} lh={20} bar={9} /> : undefined}
      title={title ?? <Line w={280} lh={28} bar={16} />}
      meta={meta ? <span>{meta}</span> : <Line w={metaWidth} lh={18} bar={9} />}
    />
  );
}

/** The breadcrumb line above detail headers: 18px plus 6px gap. */
export function CrumbSkeleton({ w = 180 }: { w?: number }) {
  return (
    <div className="mb-1.5 flex items-center gap-2 text-sm text-ink-3">
      <Line w={w} lh={18} bar={9} />
    </div>
  );
}

/** A detail page header of a measured height: title line plus a meta line. */
export function DetailHeaderSkeleton({ height, titleW = 360 }: { height: number; titleW?: number }) {
  return (
    <header className="mb-5 flex flex-col justify-start gap-1.5" style={{ height }}>
      <Line w={titleW} lh={28} bar={16} />
      <Line w={300} lh={18} bar={9} />
    </header>
  );
}

/** TabLinks, inert, with the first label (or `active`) selected and room for a count. */
export function TabsSkeleton({ labels, active = 0, counts = true }: { labels: string[]; active?: number; counts?: boolean }) {
  return (
    // The same boxes as TabLinks (components/ui.tsx): underlined tabs that scroll, never pills.
    <div aria-hidden="true" className="mb-6 border-b border-line">
      <div className="-mb-px flex gap-6 overflow-hidden">
        {labels.map((l, i) => (
          <span key={l} className={cx("inline-flex h-10 shrink-0 items-center gap-1.5 border-b-2 text-base whitespace-nowrap", i === active ? "border-ink font-medium text-ink" : "border-transparent text-ink-2")}>
            {l}
            {counts ? <span className="inline-block h-2 w-2.5 animate-pulse rounded-[2px] bg-hover" /> : null}
          </span>
        ))}
      </div>
    </div>
  );
}

/** The stats strip: real labels, placeholder values. */
export function StatsSkeleton({ labels }: { labels: string[] }) {
  const cols = labels.length === 5 ? "sm:grid-cols-5" : "sm:grid-cols-4";
  return (
    // The same boxes as a row of Stat (components/ui.tsx), unframed like the page they stand for.
    <div className={cx("mb-5 grid grid-cols-2 gap-4", cols)}>
      {labels.map((l) => (
        <div key={l} className="min-w-0">
          <div className="text-sm text-ink-3">{l}</div>
          <Line w={36} lh={32} bar={16} className="mt-1" />
        </div>
      ))}
    </div>
  );
}

/** One placeholder row: optional leading glyph, a title line and a meta line, trailing bits. */
export function RowSkeleton({ i, height, glyph = "ring", lines = 2, trailing = true }: { i: number; height: number; glyph?: "ring" | "square" | "none"; lines?: 1 | 2; trailing?: boolean }) {
  const style: CSSProperties = { height };
  return (
    <li className="flex items-center gap-3" style={style}>
      {glyph === "ring" ? <span className="size-3.5 shrink-0 animate-pulse rounded-full border-[1.5px] border-line-strong" /> : null}
      {glyph === "square" ? <span className="size-[30px] shrink-0 animate-pulse rounded-[5px] bg-hover" /> : null}
      <div className="flex min-w-0 flex-1 flex-col gap-1.5">
        <span className="block h-2.5 animate-pulse rounded-[3px] bg-hover" style={{ width: pct(i) }} />
        {lines === 2 ? <span className="block h-2 animate-pulse rounded-[3px] bg-hover/70" style={{ width: pct(i + 5, 0.5) }} /> : null}
      </div>
      {trailing ? (
        <div className="hidden shrink-0 items-center gap-3 md:flex">
          <span className="block h-2.5 w-16 animate-pulse rounded-[3px] bg-hover" />
          <span className="block h-2.5 w-10 animate-pulse rounded-[3px] bg-hover/70" />
        </div>
      ) : null}
    </li>
  );
}

/** A FilterList as it will render: the same filter bar, then fixed-height rows. */
export function ListSkeleton({ rows, rowHeight, placeholder, glyph = "ring", lines = 2, trailing = true }: { rows: number; rowHeight: number; placeholder?: string; glyph?: "ring" | "square" | "none"; lines?: 1 | 2; trailing?: boolean }) {
  return (
    // Plain rows over hairlines, like the lists they stand for; the filter is a filled field.
    <div>
      {placeholder !== undefined ? (
        <div className="mb-2 flex items-center gap-2 rounded-md bg-input px-3.5 py-1">
          <Icon name="search" className="text-ink-3" />
          <span className="flex h-7 flex-1 items-center text-sm text-ink-3">{placeholder}</span>
          <Kbd>/</Kbd>
        </div>
      ) : null}
      <ul className="divide-y divide-line">
        {Array.from({ length: rows }, (_, i) => (
          <RowSkeleton key={i} i={i} height={rowHeight} glyph={glyph} lines={lines} trailing={trailing} />
        ))}
      </ul>
    </div>
  );
}

/** A Panel with its real title and placeholder rows. `actions`: the real header carries a button (45px, not 40px). */
export function PanelSkeleton({ title, rows, rowHeight, lines = 2, actions = false, className }: { title?: string; rows: number; rowHeight: number; lines?: 1 | 2; actions?: boolean; className?: string }) {
  return (
    // The same boxes as Panel (components/ui.tsx): a heading over the rows, with no frame.
    <section className={cx("min-w-0 self-start", className)}>
      {title !== undefined ? (
        <header className={cx("mb-3 flex min-h-9 items-center", actions && "justify-between")}>
          <h2 className="text-xl font-semibold tracking-tight text-ink">{title}</h2>
        </header>
      ) : null}
      <ul className="divide-y divide-line">
        {Array.from({ length: rows }, (_, i) => (
          <RowSkeleton key={i} i={i + 3} height={rowHeight} glyph="none" lines={lines} trailing={false} />
        ))}
      </ul>
    </section>
  );
}
