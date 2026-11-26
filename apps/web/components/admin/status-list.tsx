import type { ReactNode } from "react";
import { cx } from "../ui";

export type Health = "ok" | "degraded" | "down";

/** The wire HealthStatus as the three states the console draws; anything unreported counts as down. */
export function healthOf(status: string | undefined): Health {
  if (status === "HEALTH_STATUS_OK") return "ok";
  if (status === "HEALTH_STATUS_DEGRADED") return "degraded";
  return "down";
}

/** The worse of two states (down over degraded over ok). */
export function worse(a: Health, b: Health): Health {
  const rank: Record<Health, number> = { ok: 0, degraded: 1, down: 2 };
  return rank[a] >= rank[b] ? a : b;
}

const DOT: Record<Health, string> = { ok: "bg-ok", degraded: "bg-warn", down: "bg-bad" };
const WORD: Record<Health, string> = { ok: "Operational", degraded: "Degraded", down: "Down" };

/** A status dot that also says its state to a screen reader. */
export function StatusDot({ status }: { status: Health }) {
  return (
    <span className="inline-flex items-center" data-status={status}>
      <span aria-hidden="true" className={cx("size-2 shrink-0 rounded-full", DOT[status])} />
      <span className="sr-only">{WORD[status]}</span>
    </span>
  );
}

export interface StatusItem {
  name: string;
  status: Health;
  /** What the component reported, e.g. "indexing lag 38s behind the event log". */
  detail?: string;
  /** The machine reason when it is not ok. */
  reason?: string;
  critical?: boolean;
  trailing?: ReactNode;
}

/** A component status list: one row per component, a dot, its name, what it says. */
export function StatusList({ items, label, empty = "Nothing reported." }: { items: StatusItem[]; label: string; empty?: string }) {
  if (items.length === 0) return <p className="px-3.5 py-4 text-sm text-ink-3">{empty}</p>;
  return (
    <ul aria-label={label} className="divide-y divide-line">
      {items.map((it) => (
        <li key={it.name} data-status={it.status} className="flex items-start gap-3 px-3.5 py-2.5">
          <span className="mt-1.5">
            <StatusDot status={it.status} />
          </span>
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap items-baseline gap-x-2">
              <span className="mono text-sm font-medium">{it.name}</span>
              {it.critical ? <span className="text-xs text-ink-3">critical path</span> : null}
              {it.status !== "ok" && it.reason ? <span className="mono text-xs text-ink-3">{it.reason}</span> : null}
            </div>
            {it.detail ? <p className="text-sm text-ink-2 [overflow-wrap:anywhere]">{it.detail}</p> : null}
          </div>
          {it.trailing ? <div className="shrink-0 text-sm text-ink-2">{it.trailing}</div> : null}
        </li>
      ))}
    </ul>
  );
}

/** "3d 4h", "5h 12m", "42s": how long ago `since` was, at `now`. */
export function uptime(sinceMs: number, nowMs: number): string {
  const s = Math.max(0, Math.floor((nowMs - sinceMs) / 1000));
  const d = Math.floor(s / 86_400);
  const h = Math.floor((s % 86_400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s % 60}s`;
  return `${s}s`;
}

/** How long ago `sinceMs` was, as of now. */
export const uptimeSince = (sinceMs: number): string => uptime(sinceMs, Date.now());
