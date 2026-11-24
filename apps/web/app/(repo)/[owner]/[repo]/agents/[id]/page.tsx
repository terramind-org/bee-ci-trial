import Link from "next/link";
import type { Metadata } from "next";
import { PageMenu } from "@/components/menu";
import { AdoptSections } from "./adopt-sections";
import { PrincipalMark, PrincipalName } from "@/components/domain";
import { Icon } from "@/components/icons";
import { Time } from "@/components/time";
import { MintTokenForm, RevokeToken } from "@/components/token-actions";
import { Badge, ButtonLink, EmptyState, Panel, cx } from "@/components/ui";
import { objectIdText, pathText } from "@/lib/api/ids";
import { type RepoParams, scopeFromParams, serverClient } from "@/lib/api/server";
import type { TokenInfo } from "@/lib/api/types";
import { compact, usd } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { guard } from "@/lib/page";

export const metadata: Metadata = { title: "Agent" };

function Budget({ used, max, label }: { used: string | undefined; max: string | undefined; label: string }) {
  const m = Number(max ?? 0);
  const u = Number(used ?? 0);
  if (!m) return <span className="text-xs text-ink-3">{label}: {u ? u + " used, " : ""}no limit reported</span>;
  const pct = Math.min(100, Math.round((u / m) * 100));
  return (
    <div className="min-w-32">
      <div className="flex justify-between text-sm text-ink-3">
        <span>{label}</span>
        <span className="num">
          {u}/{m}
        </span>
      </div>
      <div className="mt-1 h-1.5 overflow-hidden rounded-full bg-sunken" role="meter" aria-label={label + " used"} aria-valuenow={u} aria-valuemin={0} aria-valuemax={m}>
        <div className={cx("h-full rounded-full", pct > 85 ? "bg-bad" : "bg-ink-2")} style={{ width: pct + "%" }} />
      </div>
    </div>
  );
}

function TokenRow({ t, depth, now }: { t: TokenInfo; depth: number; now: number }) {
  const expired = t.scope.expiresAt ? Date.parse(t.scope.expiresAt) < now : false;
  // A revoked token is not dimmed: opacity-70 failed contrast, and the "revoked" badge already says it.
  return (
    <li className="flex flex-col gap-3 py-4 md:flex-row md:items-center md:gap-6" style={{ paddingLeft: depth * 24 }}>
      <div className="min-w-0 flex-1">
        <p className="flex flex-wrap items-center gap-2">
          {depth ? <Icon name="chevronRight" size={12} className="text-ink-3" /> : null}
          <span className="text-md font-medium">{t.label || "Unnamed token"}</span>
          {t.revoked ? <Badge tone="bad">revoked</Badge> : expired ? <Badge tone="outline">expired</Badge> : <Badge tone="ok">active</Badge>}
          <span className="mono min-w-0 text-xs text-ink-3" title={t.tokenId}>…{t.tokenId.slice(-8)}</span>
        </p>
        {t.scope.actions.length === 0 && t.scope.repos.length === 0 ? (
          <p className="mt-1 text-sm text-ink-2">
            This server reports only the expiry. {expired ? "Expired" : "Expires"} <Time iso={t.scope.expiresAt} />.
          </p>
        ) : (
          <p className="mt-1 flex flex-wrap gap-x-4 gap-y-0.5 text-sm text-ink-2">
            <span>
              repos <span className="mono">{t.scope.repos.join(", ") || "inherit"}</span>
            </span>
            <span>
              write <span className="mono">{t.scope.writePaths.map(pathText).join(", ") || "inherit"}</span>
            </span>
            <span>
              refs <span className="mono">{t.scope.refs.join(", ") || "inherit"}</span>
            </span>
            <span>
              actions <span className="mono">{t.scope.actions.join(", ")}</span>
            </span>
            <span>
              {expired ? "expired" : "expires"} <Time iso={t.scope.expiresAt} />
            </span>
          </p>
        )}
      </div>
      <div className="flex flex-wrap items-center gap-4">
        <Budget label="changesets" used={t.used?.maxChangesets} max={t.scope.budget?.maxChangesets} />
        <Budget label="landed" used={t.used?.maxLanded} max={t.scope.budget?.maxLanded} />
        {!t.revoked ? <RevokeToken tokenId={t.tokenId} label={t.label} /> : <span className="w-[108px]" />}
      </div>
    </li>
  );
}

export default async function AgentPage({ params }: { params: Promise<RepoParams & { id: string }> }) {
  const { id } = await params;
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName, owner: org } = await scopeFromParams(params);
    const [agent, tokens, prov, leases] = await Promise.all([
      api.agent.getAgent({ org, principalId: id, repo: ref }),
      api.agent.listTokens({ principalId: id }),
      api.agent.listProvenance({ principalId: id, repo: ref }),
      api.lease.listLeases({ repo: { fullName: org + "/*" } }),
    ]);
    const mine = leases.leases.filter((l) => l.holder?.id === id);
    const roots = tokens.tokens.filter((t) => !t.parentTokenId || !tokens.tokens.some((p) => p.tokenId === t.parentTokenId));
    const tree: { t: TokenInfo; depth: number }[] = [];
    const walk = (t: TokenInfo, depth: number) => {
      tree.push({ t, depth });
      for (const c of tokens.tokens.filter((x) => x.parentTokenId === t.tokenId)) walk(c, depth + 1);
    };
    roots.forEach((r) => walk(r, 0));
    const now = Date.now();
    const totalCost = prov.records.reduce((n, r) => n + Number(r.provenance.cost?.usdMicros ?? 0), 0);
    const getOp = { service: "AgentService" as const, method: "GetAgent", request: { principalId: id }, label: "Show this agent" };

    const sentence = [
      agent.status === "active" ? "Working now" : "Idle right now",
      agent.openChanges + " open change " + (Number(agent.openChanges) === 1 ? "request" : "requests"),
      agent.landed7d + " merged this week",
      usd(agent.spend7dUsdMicros) + " spent this week (" + usd(totalCost) + " in all recorded work)",
      mine.length + (mine.length === 1 ? " file claim" : " file claims") + " held",
    ].join(" · ");

    return (
      <>
        <div className="mb-3 flex items-center gap-2 text-base text-ink-3">
          <Link href={repoHref(fullName, "agents")} className="hover:text-ink">
            Agents
          </Link>
          <Icon name="chevronRight" size={12} />
          <span>{agent.principal.display}</span>
        </div>
        <header className="mb-12 flex flex-col gap-4 md:flex-row md:items-start md:justify-between">
          <div className="flex items-start gap-4">
            <PrincipalMark principal={agent.principal} size={56} />
            <div className="min-w-0">
              <h1 className="flex flex-wrap items-baseline gap-x-3 text-2xl font-semibold tracking-tight">
                {agent.principal.display}
                <span className="text-base font-normal text-ink-3">{agent.principal.runtime}</span>
              </h1>
              {agent.purpose ? <p className="mt-2 max-w-2xl text-lg text-ink-2">{agent.purpose}</p> : null}
              <p className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-base text-ink-2">
                <span className="inline-flex items-center gap-1.5">
                  Owned by <PrincipalName principal={agent.owner} />
                </span>
                <span>{agent.model}</span>
                {agent.session ? <span className="text-ink-3">last session <span className="mono text-sm">{agent.session}</span></span> : null}
              </p>
              <p className="mt-3 text-md text-ink">{sentence}.</p>
            </div>
          </div>
          <PageMenu op={getOp} />
        </header>

        <div className="flex max-w-4xl flex-col gap-14">
          <Panel title="Recent work" id="prov-history">
            {prov.records.length ? (
              <ol className="flex flex-col gap-5" aria-label="Changesets with model, session and cost">
                {prov.records.map((r) => (
                  <li key={objectIdText(r.changeset)}>
                    <p className="text-md text-ink">
                      {r.changeRequestId ? (
                        <Link href={repoHref(fullName, "changes/" + r.changeRequestId)} className="font-medium hover:underline">
                          {r.message}
                        </Link>
                      ) : (
                        <span className="font-medium">{r.message}</span>
                      )}
                      {r.landed ? <Badge tone="ok" className="ml-2">on main</Badge> : null}
                    </p>
                    <p className="mt-0.5 text-sm text-ink-3">
                      {r.provenance.model} · {compact(Number(r.provenance.cost?.inputTokens ?? 0) + Number(r.provenance.cost?.outputTokens ?? 0))} tokens · {usd(r.provenance.cost?.usdMicros ?? 0)} ·{" "}
                      <Time iso={r.createdAt} />
                      {r.provenance.session ? (
                        <>
                          {" · session "}
                          <span className="mono">{r.provenance.session}</span>
                        </>
                      ) : null}
                      {" · "}
                      <span className="mono">{objectIdText(r.changeset, true)}</span>
                    </p>
                  </li>
                ))}
              </ol>
            ) : (
              <EmptyState title="No work recorded yet" icon="history" action={<ButtonLink href={repoHref(fullName, "work")} size="sm">Browse issues to assign</ButtonLink>}>
                Changes this agent makes are listed here with the model, session and cost behind each one.
              </EmptyState>
            )}
          </Panel>

          <Panel title="Working now" id="agent-leases">
            {mine.length ? (
              <ul className="flex flex-col gap-3">
                {mine.map((l) => (
                  <li key={l.id} className="text-md text-ink-2">
                    {l.mode === "LEASE_MODE_EXCLUSIVE" ? "Has exclusive claim on " : "Is working in "}
                    <span className="mono text-sm text-ink">{l.paths.map(pathText).join(", ")}</span>
                    {l.note ? <span>: {l.note}</span> : null}
                    <span className="text-sm text-ink-3">
                      {" · until "}
                      <Time iso={l.expiresAt} />
                    </span>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="text-md text-ink-2">Not holding a claim on any file right now.</p>
            )}
          </Panel>

          <AdoptSections repo={fullName} agentId={id} login={agent.principal.display} />

          <Panel title="Access" id="tokens-title">
            {tree.length ? (
              <ul className="divide-y divide-line">
                {tree.map(({ t, depth }) => (
                  <TokenRow key={t.tokenId} t={t} depth={depth} now={now} />
                ))}
              </ul>
            ) : (
              <EmptyState title="No tokens" icon="key">This agent cannot act until its owner mints a token.</EmptyState>
            )}
            <details className="mt-4" open={tree.length === 0}>
              <summary className="inline-flex h-9 cursor-pointer items-center text-base font-medium text-accent-ink hover:underline">Mint a narrowed token</summary>
              <div className="pt-3">
                <p className="mb-3 text-base text-ink-2">The new token is attenuated from this agent&apos;s token: it can never do more. Revoking the parent revokes it.</p>
                <MintTokenForm principalId={id} defaultRepo={fullName} now={now} />
              </div>
            </details>
          </Panel>
        </div>
      </>
    );
  });
}
