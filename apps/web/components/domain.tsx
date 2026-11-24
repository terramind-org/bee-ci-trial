/** Domain marks: principals, risk, states and statuses. Shape and text carry meaning, never colour alone. */
import type { ChangeState, CheckStatus, Principal, RunStatus } from "@/lib/api/types";
import { classInfo, type Risk } from "@/lib/diffclass";
import { isImporterPrincipal } from "@/lib/infra";
import { Avatar, toAvatar } from "./avatar";
import { Icon } from "./icons";
import { RepoLink } from "./repo-link";
import { Badge, cx } from "./ui";

/**
 * The name to show for a principal. The server sends some principals with only an id (lease
 * holders carry the acting principal's id and kind, no display name), and an empty name would
 * leave an unlabelled link and image, so fall back to the id.
 */
export function principalLabel(p: Principal): string {
  return p.display || p.id;
}

/** The Beehive mark: a lattice on Nucleus cobalt. */
export function BrandMark({ size = 28 }: { size?: number }) {
  return (
    <span className="grid shrink-0 place-items-center rounded-md bg-accent" style={{ width: size, height: size }} aria-hidden="true">
      <svg width={Math.round(size * 0.54)} height={Math.round(size * 0.54)} viewBox="0 0 22 22" fill="none" stroke="white" strokeWidth="2.2" strokeLinecap="round">
        <path d="M5 3v16M11 3v16M17 3v16M3 7.5l16 7M3 14.5l16-7" />
      </svg>
    </span>
  );
}

/** A principal's calm generated avatar (components/avatar.tsx); lists use 28px or more. */
export function PrincipalMark({ principal, size = 28 }: { principal: Principal | undefined; size?: number }) {
  if (!principal) return null;
  return <Avatar principal={toAvatar(principal)} size={size} />;
}

export function PrincipalName({ principal, owner, showRuntime = true, link = true }: { principal: Principal | undefined; owner?: Principal; showRuntime?: boolean; link?: boolean }) {
  if (!principal) return <span className="text-ink-3">unknown</span>;
  const isAgent = principal.kind === "PRINCIPAL_KIND_AGENT";
  const name = (
    <span className="font-medium text-ink" title={isImporterPrincipal(principal) ? "Service principal that imports history into the organization" : undefined}>
      {principalLabel(principal)}
    </span>
  );
  return (
    <span className="inline-flex min-w-0 items-center gap-1.5">
      <PrincipalMark principal={principal} size={20} />
      {isAgent && link ? (
        <RepoLink sub={"agents/" + principal.id} className="hover:underline">
          {name}
        </RepoLink>
      ) : (
        name
      )}
      {isAgent && showRuntime && principal.runtime ? <span className="mono text-xs text-ink-3">{principal.runtime}</span> : null}
      {owner ? <span className="text-xs text-ink-3">for {owner.display}</span> : null}
    </span>
  );
}

/** Marks history that came in through an import, next to its author. */
export function ImportedBadge() {
  return (
    <Badge tone="outline" title="Imported history: the original git commit headers are kept with this changeset">
      imported
    </Badge>
  );
}

const RISK_LEVEL: Record<Risk, number> = { low: 1, medium: 2, elevated: 3, high: 4 };
const RISK_COLOR: Record<Risk, string> = { low: "bg-ink-3", medium: "bg-warn", elevated: "bg-ink", high: "bg-bad" };

/** Four bars: how much scrutiny the policy will want. Paired with the class name. */
export function RiskMeter({ risk }: { risk: Risk }) {
  const n = RISK_LEVEL[risk];
  return (
    <span className="inline-flex items-end gap-px" aria-hidden="true">
      {[1, 2, 3, 4].map((i) => (
        <span key={i} className={cx("w-[3px] rounded-[1px]", i <= n ? RISK_COLOR[risk] : "bg-line-strong")} style={{ height: 4 + i * 2 }} />
      ))}
    </span>
  );
}

export function DiffClassBadge({ cls, compact = false }: { cls: string; compact?: boolean }) {
  const info = classInfo(cls);
  const tone = info.risk === "high" ? "bad" : info.risk === "medium" ? "warn" : info.risk === "elevated" ? "outline" : "neutral";
  return (
    <Badge tone={tone} title={info.blurb ? info.label + ": " + info.blurb + " Risk: " + info.risk + "." : undefined} className="gap-1.5">
      <RiskMeter risk={info.risk} />
      {compact ? null : <span>{info.label}</span>}
      <span className="sr-only">{compact ? info.label + ", " : ""}risk {info.risk}</span>
    </Badge>
  );
}

export type ChangeGlyphState = "landed" | "closed" | "queued" | "ready" | "failing" | "blocked";

/** The ring at the start of a change row: one shape per landing state, Nucleus status colours. */
export function ChangeGlyph({ state }: { state: ChangeGlyphState }) {
  const label = { landed: "Landed", closed: "Closed", queued: "In the merge queue", ready: "Ready to land", failing: "Checks failing", blocked: "Blocked by policy" }[state];
  return (
    <span role="img" aria-label={label} title={label} className="relative inline-grid size-3.5 shrink-0 place-items-center">
      {state === "landed" ? <span className="size-3.5 rounded-full bg-ink-3/45" /> : null}
      {state === "closed" ? <span className="size-3.5 rounded-full border-[1.5px] border-dashed border-ink-3/70" /> : null}
      {state === "queued" ? <span className="size-3.5 animate-spin rounded-full border-[1.5px] border-live border-r-transparent [animation-duration:1.1s]" /> : null}
      {state === "ready" ? (
        <span className="grid size-3.5 place-items-center rounded-full border-[1.5px] border-ok">
          <span className="size-1.5 rounded-full bg-ok" />
        </span>
      ) : null}
      {state === "failing" ? (
        <span className="grid size-3.5 place-items-center rounded-full border-[1.5px] border-bad text-bad">
          <Icon name="x" size={8} />
        </span>
      ) : null}
      {state === "blocked" ? <span className="size-3.5 rounded-full border-[1.5px] border-ink-3/70" /> : null}
    </span>
  );
}

export function CrStateBadge({ state }: { state: ChangeState }) {
  if (state === "CHANGE_STATE_LANDED")
    return (
      <Badge tone="ok">
        <Icon name="check" size={12} /> Landed
      </Badge>
    );
  if (state === "CHANGE_STATE_CLOSED") return <Badge tone="outline">Closed</Badge>;
  return <Badge tone="accent">Open</Badge>;
}

const CHECK: Record<string, { label: string; cls: string; icon: "check" | "x" | "clock" | "dot" }> = {
  CHECK_STATUS_PASSED: { label: "passed", cls: "text-ok", icon: "check" },
  CHECK_STATUS_FAILED: { label: "failed", cls: "text-bad", icon: "x" },
  CHECK_STATUS_RUNNING: { label: "running", cls: "text-warn", icon: "dot" },
  CHECK_STATUS_QUEUED: { label: "queued", cls: "text-ink-3", icon: "clock" },
  CHECK_STATUS_SKIPPED: { label: "skipped", cls: "text-ink-3", icon: "dot" },
  passed: { label: "passed", cls: "text-ok", icon: "check" },
  failed: { label: "failed", cls: "text-bad", icon: "x" },
  running: { label: "running", cls: "text-warn", icon: "dot" },
  queued: { label: "queued", cls: "text-ink-3", icon: "clock" },
  cancelled: { label: "cancelled", cls: "text-ink-3", icon: "dot" },
};

export function StatusIcon({ status, withLabel = false }: { status: CheckStatus | RunStatus | string; withLabel?: boolean }) {
  const s = CHECK[status] ?? { label: status.toLowerCase(), cls: "text-ink-3", icon: "dot" as const };
  return (
    <span className={cx("inline-flex items-center gap-1", s.cls)}>
      <span className={cx("inline-grid size-4 place-items-center", s.icon === "dot" && s.label === "running" ? "animate-pulse-dot" : "")}>
        <Icon name={s.icon} size={s.icon === "dot" ? 18 : 14} />
      </span>
      {withLabel ? <span className="text-sm">{s.label}</span> : <span className="sr-only">{s.label}</span>}
    </span>
  );
}

export function RunStatusBadge({ status }: { status: RunStatus }) {
  const tone = status === "passed" ? "ok" : status === "failed" ? "bad" : status === "running" ? "warn" : "neutral";
  return (
    <Badge tone={tone}>
      <StatusIcon status={status} />
      {status}
    </Badge>
  );
}

export function LiveDot({ label = "Live" }: { label?: string }) {
  return (
    <span className="inline-flex items-center gap-1.5 text-sm text-ink-2">
      <span className="relative inline-flex size-2">
        <span className="absolute inset-0 animate-pulse-dot rounded-full bg-live" />
      </span>
      {label}
    </span>
  );
}

export function Mono({ children, className, title }: { children: React.ReactNode; className?: string; title?: string }) {
  return (
    <span title={title} className={cx("mono text-sm text-ink-2", className)}>
      {children}
    </span>
  );
}
