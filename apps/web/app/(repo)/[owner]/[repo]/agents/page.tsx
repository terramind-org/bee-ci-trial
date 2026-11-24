import Link from "next/link";
import type { Metadata } from "next";
import { SignInToSee } from "@/components/error-state";
import { PrincipalMark } from "@/components/domain";
import { FilterList } from "@/components/filter-list";
import { Time } from "@/components/time";
import { PageMenu } from "@/components/menu";
import { Badge, ButtonLink, EmptyState, PageHeader } from "@/components/ui";
import { type RepoParams, scopeFromParams, serverClient, sessionState } from "@/lib/api/server";
import type { AgentSummary } from "@/lib/api/types";
import { usd } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { guard } from "@/lib/page";

export const metadata: Metadata = { title: "Agents" };

export default async function AgentsPage({ params }: { params: Promise<RepoParams> }) {
  return guard(async () => {
    const api = await serverClient();
    const { owner: org, ref, fullName } = await scopeFromParams(params);
    const req = { org, repo: ref };
    // The agent list is for members of the org (ViewMembers): a signed-out visitor is asked to sign in.
    if (!(await sessionState()).me) {
      return (
        <>
          <PageHeader title="Agents" />
          <SignInToSee title="Sign in to see agents" next={repoHref(fullName, "agents")} icon="agents">
            Who works on this repository, their spend and their open changes are shown to members.
          </SignInToSee>
        </>
      );
    }
    const { agents } = await api.agent.listAgents(req);
    const items = agents.map((a) => ({
      key: a.principal.id,
      text: [a.principal.display, a.principal.runtime, a.owner?.display, a.purpose, a.model, a.status].join(" "),
      node: (
        <Link href={repoHref(fullName, "agents/" + a.principal.id)} data-nav-item className="flex items-start gap-4 px-2 py-5 hover:bg-hover focus-visible:bg-hover">
          <PrincipalMark principal={a.principal} size={44} />
          <div className="min-w-0 flex-1">
            <p className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
              <span className="text-lg font-semibold text-ink">{a.principal.display}</span>
              <span className="text-sm text-ink-3">{a.principal.runtime}</span>
              {a.status === "active" ? <span className="text-sm font-medium text-live">working now</span> : null}
              {a.registered ? null : <Badge tone="warn">unregistered</Badge>}
            </p>
            <p className="mt-1 text-md text-ink-2">{doing(a)}</p>
            <p className="mt-1 text-sm text-ink-3">
              {a.registered && a.purpose ? a.purpose + " · " : ""}
              {a.owner ? "owned by " + a.owner.display + " · " : ""}
              {a.model}
              {a.lastSeen ? (
                <>
                  {" · seen "}
                  <Time iso={a.lastSeen} />
                </>
              ) : null}
            </p>
          </div>
        </Link>
      ),
    }));
    return (
      <>
        <PageHeader
          title="Agents"
          lede="Every agent has an owner and tokens no broader than its owner’s, so each change is attributable end to end."
          actions={<PageMenu op={{ service: "AgentService", method: "ListAgents", request: req, label: "List agents" }} />}
        />
        <FilterList
          items={items}
          label="Filter agents"
          placeholder="Filter by name, runtime, owner or model…"
          empty={
            <EmptyState title="No agents yet" icon="agents" action={<ButtonLink href={repoHref(fullName, "settings")} variant="secondary" size="sm">Open settings</ButtonLink>}>
              Create one with <code className="mono">bee agent create</code>; it appears here with its owner.
            </EmptyState>
          }
        />
      </>
    );
  });
}

/** What the agent is doing, in a sentence, from the numbers the server reports. */
function doing(a: AgentSummary): string {
  const open = Number(a.openChanges);
  const leases = Number(a.activeLeases);
  const landed = Number(a.landed7d);
  const parts: string[] = [];
  if (open) parts.push((a.status === "active" ? "working on " : "has ") + open + " open change " + (open === 1 ? "request" : "requests"));
  if (leases) parts.push("holding " + leases + (leases === 1 ? " file claim" : " file claims"));
  const now = parts.length ? parts.join(" and ") : a.status === "active" ? "working, nothing open right now" : "idle right now";
  const done = landed ? "merged " + landed + (landed === 1 ? " change" : " changes") + " this week" : "nothing merged this week";
  const spend = "spent " + usd(a.spend7dUsdMicros);
  return now.charAt(0).toUpperCase() + now.slice(1) + "; " + done + ", " + spend + ".";
}
