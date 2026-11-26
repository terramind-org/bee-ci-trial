/**
 * Small UI fragments for "Built for many agents at once", assembled from the app's own
 * components. They illustrate behaviour and carry no figures; each is labelled as an example.
 */
import { ChangeGlyph, PrincipalMark, RiskMeter } from "@/components/domain";
import { Badge } from "@/components/ui";
import { principalOf } from "@/lib/format";

const agent = (display: string, runtime: string) => principalOf({ id: display, kind: "PRINCIPAL_KIND_AGENT", display, runtime });

const frame = "overflow-hidden rounded-lg border border-line bg-surface";

export function LeaseFragment() {
  return (
    <figure className={frame} aria-label="Example: a lease on a directory">
      <ul className="divide-y divide-line">
        <li className="flex items-center gap-2.5 px-3 py-2.5">
          <PrincipalMark principal={agent("agent-a", "claude-code")} size={22} />
          <div className="min-w-0 flex-1">
            <p className="mono truncate text-base">src/billing/**</p>
            <p className="truncate text-sm text-ink-3">held by agent-a</p>
          </div>
          <Badge tone="warn">exclusive</Badge>
        </li>
        <li className="flex items-center gap-2.5 px-3 py-2.5">
          <PrincipalMark principal={agent("agent-b", "codex")} size={22} />
          <div className="min-w-0 flex-1">
            <p className="mono truncate text-base">src/billing/**</p>
            <p className="truncate text-sm text-ink-3">agent-b is told who holds it</p>
          </div>
          <Badge tone="outline">waiting</Badge>
        </li>
      </ul>
    </figure>
  );
}

export function TrainFragment() {
  const rows: { state: "landed" | "queued" | "ready"; title: string; note: string }[] = [
    { state: "landed", title: "Retry failed uploads", note: "merged" },
    { state: "queued", title: "Cache the schema lookup", note: "in the queue" },
    { state: "ready", title: "Fix the rounding in invoices", note: "ready to merge" },
  ];
  return (
    <figure className={frame} aria-label="Example: pull requests in a merge queue">
      <ul className="divide-y divide-line">
        {rows.map((r) => (
          <li key={r.title} className="flex items-center gap-2.5 px-3 py-2.5">
            <ChangeGlyph state={r.state} />
            <p className="min-w-0 flex-1 truncate text-base">{r.title}</p>
            <span className="shrink-0 text-sm text-ink-3">{r.note}</span>
          </li>
        ))}
      </ul>
    </figure>
  );
}

export function PolicyFragment() {
  const rows: { cls: string; risk: "low" | "elevated" | "high"; rule: string }[] = [
    { cls: "docs", risk: "low", rule: "merges on green checks" },
    { cls: "logic", risk: "elevated", rule: "needs one review" },
    { cls: "sensitive", risk: "high", rule: "needs a human" },
  ];
  return (
    <figure className={frame} aria-label="Example: the rules for merging, by kind of change">
      <ul className="divide-y divide-line">
        {rows.map((r) => (
          <li key={r.cls} className="flex items-center gap-2.5 px-3 py-2.5">
            <RiskMeter risk={r.risk} />
            <p className="mono min-w-0 flex-1 truncate text-base">{r.cls}</p>
            <span className="shrink-0 text-sm text-ink-3">{r.rule}</span>
          </li>
        ))}
      </ul>
    </figure>
  );
}
