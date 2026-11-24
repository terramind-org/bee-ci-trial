import Link from "next/link";
import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { PageMenu } from "@/components/menu";
import { LiveRefresh } from "@/components/live-refresh";
import { CrStateBadge, DiffClassBadge, PrincipalMark, PrincipalName } from "@/components/domain";
import { Icon } from "@/components/icons";
import { AssignIssue, IssueComment, IssueStateButton } from "@/components/issue-actions";
import { InspiredByBanner } from "@/components/inspired-by-banner";
import { Markdown } from "@/components/markdown";
import { Time } from "@/components/time";
import { Badge, Panel } from "@/components/ui";
import { type RepoParams, scopeFromParams, serverClient, sessionState } from "@/lib/api/server";
import type { Principal } from "@/lib/api/types";
import { uniqueById } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { guard } from "@/lib/page";

export async function generateMetadata({ params }: { params: Promise<{ number: string }> }): Promise<Metadata> {
  return { title: "Issue #" + (await params).number };
}

export default async function IssuePage({ params }: { params: Promise<RepoParams & { number: string }> }) {
  const n = Number((await params).number);
  if (!Number.isInteger(n) || n <= 0) notFound();
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName, owner: org } = await scopeFromParams(params);
    const { me } = await sessionState();
    // Agents and members (the assignee picker) are for members of the org; skip them signed out.
    const [issue, all, agents, members, changes] = await Promise.all([
      api.issue.getIssue({ repo: ref, number: n }),
      api.issue.listIssues({ repo: ref }),
      me ? api.agent.listAgents({ org, repo: ref }) : { agents: [] },
      me ? api.org.listMembers({ org }) : { members: [] },
      api.change.listChanges({ repo: ref }),
    ]);
    const people: Principal[] = uniqueById([...agents.agents.map((a) => a.principal), ...members.members.map((m) => m.principal).filter((p): p is Principal => !!p)]);
    const subs = all.issues.filter((i) => i.parentNumber === n);
    const parent = issue.parentNumber ? all.issues.find((i) => i.number === issue.parentNumber) : undefined;
    const linked = changes.changes.filter((c) => issue.linkedChanges.includes(c.id));
    const getOp = { service: "IssueService" as const, method: "GetIssue", request: { repo: ref, number: n }, label: "Show this issue" };
    const doneSubs = subs.filter((s) => s.state === "closed").length;
    return (
      <>
        <LiveRefresh repo={fullName} ids={[String(n)]} kinds={["issue.*", "issue_comment.*"]} />
        <div className="mb-3 flex flex-wrap items-center gap-2 text-base text-ink-3">
          <Link href={repoHref(fullName, "work")} className="hover:text-ink">
            Issues
          </Link>
          <Icon name="chevronRight" size={12} />
          <span className="num">#{n}</span>
          {parent ? (
            <>
              <span>· part of</span>
              <Link href={repoHref(fullName, "work/" + parent.number)} className="link">
                #{parent.number} {parent.title}
              </Link>
            </>
          ) : null}
        </div>
        {/* web-adopt: shown when this issue was opened from another repo's post and the reader can read it. */}
        <div className="mb-8 empty:hidden">
          <InspiredByBanner inspiredBy={issue.inspiredBy} />
        </div>
        <header className="mb-10 flex flex-col gap-4 md:flex-row md:items-start md:justify-between">
          <div className="min-w-0">
            <h1 className="text-2xl font-semibold tracking-tight [overflow-wrap:anywhere]">{issue.title}</h1>
            <p className="mt-3 flex flex-wrap items-center gap-x-3 gap-y-1 text-base text-ink-2">
              <Badge tone={issue.state === "open" ? "accent" : "outline"}>{issue.state === "open" ? "Open" : "Closed"}</Badge>
              <span className="inline-flex items-center gap-1.5">
                Opened by <PrincipalName principal={issue.author} showRuntime={false} />
              </span>
              <Time iso={issue.createdAt} />
              {issue.type ? <span>{issue.type}</span> : null}
              {issue.labels.map((l) => (
                <span key={l} className="text-ink-3">
                  {l}
                </span>
              ))}
              {issue.milestone ? <span>{issue.milestone}</span> : null}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <IssueStateButton repo={fullName} number={n} state={issue.state} />
            <PageMenu op={getOp} />
          </div>
        </header>
        <div className="grid grid-cols-1 gap-12 lg:grid-cols-[minmax(0,1fr)_280px]">
          <div className="flex min-w-0 max-w-3xl flex-col gap-12">
            <Markdown source={issue.body} repo={fullName} />
            {subs.length ? (
              <Panel title={"Sub-issues · " + doneSubs + " of " + subs.length + " done"} id="subs-title">
                <ul className="flex flex-col">
                  {subs.map((s) => (
                    <li key={s.id}>
                      <Link href={repoHref(fullName, "work/" + s.number)} className="flex items-center gap-3 rounded-md px-2 py-2.5 hover:bg-hover">
                        <Icon name={s.state === "closed" ? "check" : "dot"} className={s.state === "closed" ? "text-ok" : "text-ink-3"} />
                        <span className="num text-ink-3">#{s.number}</span>
                        <span className="min-w-0 flex-1 text-md">{s.title}</span>
                        {s.assignees.map((a) => (
                          <PrincipalMark key={a.id} principal={a} size={28} />
                        ))}
                      </Link>
                    </li>
                  ))}
                </ul>
              </Panel>
            ) : null}
            <Panel title="Conversation" id="conv-title">
              <ol className="flex flex-col gap-6">
                {issue.comments.map((c) => (
                  <li key={c.id} className="flex gap-3">
                    <PrincipalMark principal={c.author} size={28} />
                    <div className="min-w-0 flex-1">
                      <p className="text-base">
                        <span className="font-medium">{c.author?.display}</span> <Time iso={c.createdAt} className="text-ink-3" />
                      </p>
                      <Markdown source={c.body} className="mt-1" repo={fullName} />
                    </div>
                  </li>
                ))}
              </ol>
              {issue.comments.length === 0 ? <p className="text-md text-ink-2">No comments yet.</p> : null}
              <div className="mt-8">
                <IssueComment repo={fullName} number={n} />
              </div>
            </Panel>
          </div>
          <aside className="flex flex-col gap-10" aria-label="Issue details">
            <Panel title="Assigned to" id="assign-title">
              <div className="flex flex-col gap-4">
                {issue.assignees.length ? (
                  <ul className="flex flex-col gap-2">
                    {issue.assignees.map((a) => (
                      <li key={a.id} className="text-base">
                        <PrincipalName principal={a} />
                      </li>
                    ))}
                  </ul>
                ) : (
                  <p className="text-base text-ink-2">Nobody yet.</p>
                )}
                <AssignIssue repo={fullName} number={n} current={issue.assignees.map((a) => a.id)} people={people} />
              </div>
            </Panel>
            <Panel title="Linked changes" id="linked-title">
              {linked.length ? (
                <ul className="flex flex-col gap-4">
                  {linked.map((c) => (
                    <li key={c.id}>
                      <Link href={repoHref(fullName, "changes/" + c.id)} className="flex flex-col gap-1 hover:underline">
                        <span className="text-base font-medium">{c.title}</span>
                        <span className="flex items-center gap-2 no-underline">
                          <CrStateBadge state={c.state} />
                          <DiffClassBadge cls={c.revisions.at(-1)?.diffClass ?? ""} />
                        </span>
                      </Link>
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="text-base text-ink-2">None yet. A change request that says “Fixes #{n}” links here and closes this issue when it merges.</p>
              )}
            </Panel>
          </aside>
        </div>
      </>
    );
  });
}
