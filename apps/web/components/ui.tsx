/**
 * Primitives, in the Document rhythm (docs/design/tokens.md): type and whitespace carry the
 * structure, hairlines stand in for boxes, Geist everywhere and mono only for code, paths and
 * SHAs. Every interactive element has a visible focus ring and a 32px+ hit target.
 */
import Link from "next/link";
import type { ButtonHTMLAttributes, InputHTMLAttributes, ReactNode, SelectHTMLAttributes, TextareaHTMLAttributes } from "react";
import { Icon, type IconName } from "./icons";
import { Swap } from "./motion";

export function cx(...parts: (string | false | null | undefined)[]): string {
  return parts.filter(Boolean).join(" ");
}

type Variant = "primary" | "secondary" | "ghost" | "danger";
type Size = "sm" | "md";

const VARIANTS: Record<Variant, string> = {
  // Disabled primary gives up the accent entirely: in dark the accent is bright enough that
  // opacity alone leaves it looking live.
  primary: "bg-accent text-on-accent hover:bg-accent-hover border-transparent disabled:bg-selected disabled:text-ink-3 disabled:border-line disabled:hover:bg-selected",
  secondary: "bg-transparent text-ink border-line-strong hover:bg-hover",
  ghost: "bg-transparent text-ink-2 border-transparent hover:bg-hover hover:text-ink",
  danger: "bg-transparent text-bad border-line-strong hover:bg-bad-soft",
};
const SIZES: Record<Size, string> = {
  sm: "h-8 px-3 text-sm gap-1.5",
  md: "h-9 px-3.5 text-base gap-2",
};

export function buttonClass(variant: Variant = "secondary", size: Size = "md", extra?: string): string {
  return cx(
    "inline-flex items-center justify-center whitespace-nowrap rounded-md border font-medium transition-[background-color,color,transform] duration-[120ms] ease-out active:scale-[0.97]",
    "disabled:opacity-60 disabled:pointer-events-none",
    VARIANTS[variant],
    SIZES[size],
    extra,
  );
}

export function Button({
  variant = "secondary",
  size = "md",
  icon,
  className,
  children,
  type = "button",
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant; size?: Size; icon?: IconName }) {
  return (
    <button type={type} className={buttonClass(variant, size, className)} {...rest}>
      {icon ? <Icon name={icon} size={size === "sm" ? 16 : 18} /> : null}
      {children}
    </button>
  );
}

export function ButtonLink({ href, variant = "secondary", size = "md", icon, children, className }: { href: string; variant?: Variant; size?: Size; icon?: IconName; children: ReactNode; className?: string }) {
  return (
    <Link href={href} className={buttonClass(variant, size, className)}>
      {icon ? <Icon name={icon} size={size === "sm" ? 16 : 18} /> : null}
      {children}
    </Link>
  );
}

type Tone = "neutral" | "accent" | "ok" | "warn" | "bad" | "outline";
const TONES: Record<Tone, string> = {
  neutral: "bg-selected text-ink-2 border-transparent",
  accent: "bg-accent-soft text-accent-ink border-transparent",
  ok: "bg-ok-soft text-ok border-transparent",
  warn: "bg-warn-soft text-warn border-transparent",
  bad: "bg-bad-soft text-bad border-transparent",
  outline: "bg-transparent text-ink-2 border-line",
};

export function Badge({ tone = "neutral", children, className, title }: { tone?: Tone; children: ReactNode; className?: string; title?: string }) {
  return (
    <span title={title} className={cx("inline-flex h-6 items-center gap-1 whitespace-nowrap rounded-md border px-2 text-xs font-medium", TONES[tone], className)}>
      {children}
    </span>
  );
}

export function Kbd({ children }: { children: ReactNode }) {
  return <kbd className="inline-flex h-6 min-w-6 items-center justify-center rounded-sm border border-line px-1.5 font-sans text-2xs font-medium text-ink-3">{children}</kbd>;
}

/**
 * A section of a page: a real heading (20px) with room above it, optional actions on the right,
 * and the content below with no box around it. `title` is optional for a plain grouped section;
 * pass `id` so the heading labels the region. Separate sections with spacing, not borders.
 */
export function Panel({ title, actions, children, className, bodyClassName, id }: { title?: ReactNode; actions?: ReactNode; children: ReactNode; className?: string; bodyClassName?: string; id?: string }) {
  return (
    <section aria-labelledby={id && title ? id : undefined} className={cx("min-w-0", className)}>
      {title || actions ? (
        <header className="mb-3 flex min-h-9 flex-wrap items-center justify-between gap-x-4 gap-y-2">
          {title ? (
            <h2 id={id} className="text-xl font-semibold tracking-tight text-ink">
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

/**
 * Nothing here yet: an icon, what this place is for in a sentence, and the one next step that
 * fills it (`action`: a ButtonLink, a CLI line, a sign-in link). Without an action the copy must
 * say why there is nothing to do.
 */
export function EmptyState({ title, children, icon = "dot", action }: { title: string; children?: ReactNode; icon?: IconName; action?: ReactNode }) {
  return (
    <div className="flex flex-col items-start gap-3 py-10" role="status">
      <span className="grid size-11 place-items-center rounded-full bg-sunken text-ink-2">
        <Icon name={icon} size={22} />
      </span>
      <div className="flex max-w-xl flex-col gap-1">
        <p className="text-lg font-semibold text-ink">{title}</p>
        {children ? <div className="text-base text-ink-2">{children}</div> : null}
      </div>
      {action ? <div className="mt-1 flex flex-wrap items-center gap-2">{action}</div> : null}
    </div>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return <span aria-hidden="true" className={cx("block animate-pulse rounded-sm bg-hover", className)} />;
}

export function SkeletonRows({ rows = 6 }: { rows?: number }) {
  return (
    <div className="divide-y divide-line" role="status" aria-busy="true" aria-label="Loading">
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

export function Field({ label, hint, error, children, htmlFor }: { label: string; hint?: ReactNode; error?: string; children: ReactNode; htmlFor: string }) {
  return (
    <div className="flex flex-col gap-1.5">
      <label htmlFor={htmlFor} className="text-base font-medium text-ink">
        {label}
      </label>
      {children}
      {error ? (
        <p className="text-sm text-bad" role="alert">
          {error}
        </p>
      ) : hint ? (
        <p className="text-sm text-ink-3">{hint}</p>
      ) : null}
    </div>
  );
}

const control = "w-full rounded-md border border-transparent bg-input px-3 text-base text-ink placeholder:text-ink-3 transition-colors duration-[120ms] hover:border-line-strong focus-visible:border-focus focus-visible:outline-none";

export function Input({ className, ...rest }: InputHTMLAttributes<HTMLInputElement>) {
  return <input className={cx(control, "h-9", className)} {...rest} />;
}

export function Textarea({ className, rows = 3, ...rest }: TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return <textarea rows={rows} className={cx(control, "py-2 leading-6", className)} {...rest} />;
}

export function Select({ className, children, ...rest }: SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <select className={cx(control, "h-9 pr-8", className)} {...rest}>
      {children}
    </select>
  );
}

/**
 * The top of a page in the Document style: an optional crumb (`eyebrow`), the title at 28px,
 * an optional lede (`lede`, one or two plain sentences), a meta line, and actions on the right.
 * The page's single "…" menu, when it has one, goes in `actions` last.
 */
export function PageHeader({ title, meta, actions, eyebrow, lede }: { title: ReactNode; meta?: ReactNode; actions?: ReactNode; eyebrow?: ReactNode; lede?: ReactNode }) {
  return (
    <header className="mb-10 flex flex-col gap-4 md:flex-row md:items-start md:justify-between">
      <div className="min-w-0">
        {eyebrow ? <div className="mb-2 text-base text-ink-3">{eyebrow}</div> : null}
        <h1 className="text-2xl font-semibold tracking-tight text-ink [overflow-wrap:anywhere]">{title}</h1>
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
      <div className="text-sm text-ink-3">{label}</div>
      <div className="num mt-1 text-2xl font-semibold tracking-tight">{typeof value === "string" || typeof value === "number" ? <Swap value={value}>{value}</Swap> : value}</div>
      {hint ? <div className="text-sm text-ink-3">{hint}</div> : null}
    </div>
  );
}

/** Link-based tabs for sub-views that deserve their own URL: plain words over one hairline. */
export function TabLinks({ items, label }: { items: { href: string; label: string; active: boolean; count?: number }[]; label: string }) {
  return (
    <nav aria-label={label} className="mb-6 border-b border-line">
      <ul className="-mb-px flex gap-6 overflow-x-auto scrollbar-thin">
        {items.map((t) => (
          <li key={t.href} className="shrink-0">
            <Link
              href={t.href}
              aria-current={t.active ? "page" : undefined}
              className={cx(
                "inline-flex h-10 items-center gap-1.5 border-b-2 text-base whitespace-nowrap transition-colors duration-[160ms]",
                t.active ? "border-ink font-medium text-ink" : "border-transparent text-ink-2 hover:border-line-strong hover:text-ink",
              )}
            >
              {t.label}
              {t.count !== undefined ? (
                <Swap value={t.count} className="num text-ink-3">
                  {t.count}
                </Swap>
              ) : null}
            </Link>
          </li>
        ))}
      </ul>
    </nav>
  );
}
