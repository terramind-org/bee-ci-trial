import Link from "next/link";
import type { Metadata } from "next";
import { PrincipalMark } from "@/components/domain";
import { PageMenu } from "@/components/menu";
import { LiveFeed } from "@/components/live-feed";
import { Time } from "@/components/time";
import { FlipList, Swap } from "@/components/motion";
import { PageHeader, Panel, cx } from "@/components/ui";
import { pathText } from "@/lib/api/ids";
import { type RepoParams, scopeFromParams, serverClient, sessionState } from "@/lib/api/server";
import type { Principal } from "@/lib/api/types";
import { usd } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { guard, param, type SearchParams } from "@/lib/page";

export const metadata: Metadata = { title: "Activity" };

export default async function ActivityPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  const sp = await searchParams;
  return guard(async () => {
    const api = await serverClient();
    const { owner: org, fullName, ref } = await scopeFromParams(params);
    const all = param(sp, "scope") === "all";
    const scope = all ? org + "/*" : fullName;
    const { me } = await sessionState();
    // Agents and members are for members of the org; a signed-out visitor sees the rest.
    const [agents, members, leases, queue, open] = await Promise.all([
      me ? api.agent.listAgents({ org, repo: ref }) : { agents: [] },
      me ? api.org.listMembers({ org }) : { members: [] },
      api.lease.listLeases({ repo: { fullName: scope } }),
      api.change.listQueue({ repo: ref, ref: "main" }),
      api.change.listChanges({ repo: ref, state: "CHANGE_STATE_OPEN" }),
    ]);
    const everyone: Principal[] = [...members.members.map((m) => m.principal).filter((p): p is Principal => !!p), ...agents.agents.map((a) => a.principal)];
    const directory = [...new Map(everyone.map((p) => [p.id, p])).values()];
    const active = agents.agents.filter((a) => a.status === "active");
    const spend = agents.agents.reduce((n, a) => n + Number(a.spend7dUsdMicros), 0);
    const subscribe = { repo: { fullName: scope }, fromSeq: "0" };

    const streamOp = { service: "EventService" as const, method: "Subscribe", request: subscribe, label: "Follow this stream" };

    return (
      <>
        <PageHeader
          title="Activity"
          lede="Everything agents and people do here, as it happens."
          meta={
            <nav aria-label="Scope" className="flex gap-1">
              <Link href={repoHref(fullName, "activity")} aria-current={!all ? "page" : undefined} className={cx("rounded-md px-2.5 py-1", !all ? "bg-selected font-medium text-ink" : "text-ink-3 hover:text-ink")}>
                {fullName}
              </Link>
              <Link href={repoHref(fullName, "activity?scope=all")} aria-current={all ? "page" : undefined} className={cx("rounded-md px-2.5 py-1", all ? "bg-selected font-medium text-ink" : "text-ink-3 hover:text-ink")}>
                All repositories
              </Link>
            </nav>
          }
          actions={<PageMenu op={streamOp} />}
        />
        <p className="mb-10 max-w-2xl text-lg text-ink-2">
          <Swap value={active.length + "/" + agents.agents.length}>
            {active.length} of {agents.agents.length} {agents.agents.length === 1 ? "agent is" : "agents are"} working
          </Swap>
          {". "}
          <Swap value={open.changes.length}>
            {open.changes.length} open change {open.changes.length === 1 ? "request" : "requests"}
          </Swap>
          {", "}
          <Swap value={usd(spend)}>{usd(spend)} of agent spend this week.</Swap>
        </p>
        <div className="grid grid-cols-1 gap-12 xl:grid-cols-[minmax(0,1fr)_320px]">
          <LiveFeed key={scope} repo={scope} directory={directory} />
          <FlipList as="div" className="flex min-w-0 flex-col gap-10">
            <div data-key="leases">
              <Panel title="Working now" id="leases-title">
                {leases.leases.length === 0 ? (
                  <p className="text-base text-ink-2">Nobody has announced work on any path.</p>
                ) : (
                  <FlipList className="flex flex-col gap-4">
                    {leases.leases.map((l) => (
                      <li key={l.id} data-key={l.id} className="flex items-start gap-3">
                        <PrincipalMark principal={l.holder} size={28} />
                        <p className="min-w-0 text-base text-ink-2">
                          <span className="font-medium text-ink">{l.holder?.display}</span> {l.mode === "LEASE_MODE_EXCLUSIVE" ? "has exclusive claim on" : "is working in"}{" "}
                          <span className="mono text-sm text-ink [overflow-wrap:anywhere]">{l.paths.map(pathText).join(", ")}</span>
                          {l.note ? <span className="block text-ink-3">{l.note}</span> : null}
                          <span className="block text-sm text-ink-3">
                            until <Time iso={l.expiresAt} />
                          </span>
                        </p>
                      </li>
                    ))}
                  </FlipList>
                )}
              </Panel>
            </div>
            <div data-key="queue">
              <Panel title="Merge queue · main" id="queue-title">
                {queue.entries.length === 0 ? (
                  <p className="text-base text-ink-2">The queue is empty. Pull requests line up here once the rules allow them.</p>
                ) : (
                  <FlipList as="ol" className="flex flex-col gap-3">
                    {queue.entries.map((q) => (
                      <li key={q.changeRequestId} data-key={q.changeRequestId} className="flex items-baseline gap-3">
                        <span className="num w-5 text-base text-ink-3">{q.position}</span>
                        <Link href={repoHref(fullName, "changes/" + q.changeRequestId)} className="min-w-0 flex-1 text-base hover:underline">
                          {q.title}
                        </Link>
                        <span className="text-sm text-ink-3">{q.state}</span>
                      </li>
                    ))}
                  </FlipList>
                )}
                <p className="mt-4 text-sm text-ink-3">{queue.landsPerMinute} merges per minute on main</p>
              </Panel>
            </div>
          </FlipList>
        </div>
      </>
    );
  });
}
