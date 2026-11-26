/**
 * One issue (docs/spec/web-ui.md §3, "Issues"): the title, whether it is open, who asked, and
 * who is working on it and for whom; then the body, its sub-issues, the pull requests that
 * answer it, who it is assigned to, and the conversation. One reading column.
 *
 * In the agent's view the same facts are plain text, followed by the tools this page calls.
 */
import Link from "next/link";
import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { PageFacts } from "@/components/bee/page-facts";
import { PrincipalMark } from "@/components/domain";
import { InspiredByBanner } from "@/components/inspired-by-banner";
import { AssignIssue, IssueComment, IssueStateButton } from "@/components/work/issue-actions";
import { LiveRefresh } from "@/components/live-refresh";
import { Markdown } from "@/components/markdown";
import { PageMenu } from "@/components/menu";
import { Mark, Quiet, Sect, Sep, State } from "@/components/parts";
import { Time } from "@/components/time";
import { type RepoParams, scopeFromParams, serverClient, sessionState } from "@/lib/api/server";
import { issueAssignees } from "@/components/work/assignees";
import { issueLine, issueWork, issueWorkText, linkedPulls } from "@/lib/issues";
import { guard } from "@/lib/page";
import { directoryOf } from "@/lib/pulls";
import { repoHref } from "@/lib/routes";
import { pageTools } from "@/lib/tools";
import { requestMode } from "../../../../../request-mode";

export async function generateMetadata({ params }: { params: Promise<{ number: string }> }): Promise<Metadata> {
  return { title: "Issue #" + (await params).number };
}

const PULL_STATE: Record<string, { word: string; shape: "open" | "merged" | "closed" }> = {
  CHANGE_STATE_OPEN: { word: "Open", shape: "open" },
  CHANGE_STATE_LANDED: { word: "Merged", shape: "merged" },
  CHANGE_STATE_CLOSED: { word: "Closed", shape: "closed" },
};

export default async function IssuePage({ params }: { params: Promise<RepoParams & { number: string }> }) {
  const n = Number((await params).number);
  if (!Number.isInteger(n) || n <= 0) notFound();
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName, owner: org } = await scopeFromParams(params);
    const [{ me }, { mode }] = await Promise.all([sessionState(), requestMode()]);
    const [issue, all, { members, people }, changes] = await Promise.all([
      api.issue.getIssue({ repo: ref, number: n }),
      api.issue.listIssues({ repo: ref }),
      issueAssignees(api, org, ref, !!me, mode === "keeper"),
      api.change.listChanges({ repo: ref }),
    ]);
    const directory = directoryOf(members);
    const subs = all.issues.filter((i) => i.parentNumber === n);
    const parent = issue.parentNumber ? all.issues.find((i) => i.number === issue.parentNumber) : undefined;
    const linked = linkedPulls(issue, changes.changes);
    const getOp = { service: "IssueService" as const, method: "GetIssue", request: { repo: ref, number: n }, label: "Show this issue" };
    const doneSubs = subs.filter((s) => s.state === "closed").length;
    const work = issueWork(issue, directory);
    const live = <LiveRefresh repo={fullName} ids={[String(n)]} kinds={["issue.*", "issue_comment.*"]} />;

    if (mode === "bee")
      return (
        <>
          {live}
          <PageFacts
            page="issue"
            title={fullName + " · issue #" + n + " · " + issue.title}
            groups={[
              {
                title: "facts",
                facts: [
                  { label: "issue", value: fullName + " #" + n },
                  { label: "title", value: issue.title },
                  { label: "state", value: issue.state },
                  issue.author ? { label: "asked by", value: issue.author.display || issue.author.id } : null,
                  issue.createdAt ? { label: "opened", value: issue.createdAt } : null,
                  { label: "who has it", value: issueWorkText(work) },
                  issue.type ? { label: "type", value: issue.type } : null,
                  issue.labels.length ? { label: "labels", value: issue.labels.join(", ") } : null,
                  issue.milestone ? { label: "milestone", value: issue.milestone } : null,
                  parent ? { label: "part of", value: "#" + parent.number + " " + parent.title } : null,
                  { label: "page", value: repoHref(fullName, "work/" + n) },
                ],
              },
              { title: "request", lines: issue.body.trim() ? [<span key="request" className="whitespace-pre-wrap">{issue.body}</span>] : [], empty: "No description." },
              ...(subs.length ? [{ title: "sub-issues, " + doneSubs + " of " + subs.length + " done", lines: subs.map((s) => issueLine(s, directory, changes.changes)) }] : []),
              { title: "pull requests", lines: linked.map((c) => "#" + c.number + " · " + c.title + " · " + (PULL_STATE[c.state]?.word ?? "Open").toLowerCase() + " · " + repoHref(fullName, "changes/" + c.id)), empty: "No linked pull requests." },
              { title: "assigned to", lines: issue.assignees.map((a) => (a.display || a.id) + (a.runtime ? " · " + a.runtime : "")), empty: "Nobody is assigned." },
              { title: "conversation", lines: issue.comments.map((c) => (
                <span key={c.id} className="whitespace-pre-wrap">
                  {[c.author?.display || c.author?.id, c.createdAt].filter(Boolean).join(" · ")}
                  {"\n" + c.body}
                </span>
              )), empty: "No comments yet." },
            ]}
            tools={pageTools("issue").filter((t) => {
              if (t.kind !== "read" && !me) return false;
              return t.op !== "IssueService/SetIssueState" || t.name === (issue.state === "open" ? "issue_close_issue" : "issue_reopen_issue");
            })}
          />
        </>
      );

    return (
      <>
        {live}
        <header className="mb-10">
          <div className="mb-5 flex flex-wrap items-center justify-between gap-4">
            <span className="flex flex-wrap items-center gap-x-2 gap-y-1 text-sm text-ink-2">
              <Link href={repoHref(fullName, "work")} className="inline-flex min-h-6 items-center gap-1.5 hover:text-ink hover:underline hover:underline-offset-4">
                <span aria-hidden="true">←</span> Issues
              </Link>
              {parent ? (
                <>
                  <Sep />
                  <span>part of</span>
                  <Link href={repoHref(fullName, "work/" + parent.number)} className="link">
                    #{parent.number} {parent.title}
                  </Link>
                </>
              ) : null}
            </span>
            <span className="flex shrink-0 items-center gap-2">
              {me ? <IssueStateButton repo={fullName} number={n} state={issue.state} /> : null}
              <PageMenu op={getOp} />
            </span>
          </div>
          {/* web-adopt: shown when this issue was opened from another repo's post and the reader can read it. */}
          <div className="mb-6 empty:hidden">
            <InspiredByBanner inspiredBy={issue.inspiredBy} />
          </div>
          <h1 className="max-w-[24ch] text-2xl text-ink [overflow-wrap:anywhere]">
            <span className="num text-ink-2">#{n}</span> {issue.title}
          </h1>
          <p className="mt-4 flex flex-wrap items-center gap-x-2 gap-y-1 text-base text-ink-2">
            <State shape={issue.state === "open" ? "open" : "closed"}>{issue.state === "open" ? "Open" : "Closed"}</State>
            {issue.author ? (
              <>
                <Sep />
                <span>asked by {issue.author.display || issue.author.id}</span>
              </>
            ) : null}
            <Sep />
            <Time iso={issue.createdAt} />
            {issue.type ? (
              <>
                <Sep />
                <span>{issue.type}</span>
              </>
            ) : null}
            {issue.labels.map((l) => (
              <span key={l} className="inline-flex items-center gap-x-2">
                <Sep />
                {l}
              </span>
            ))}
            {issue.milestone ? (
              <>
                <Sep />
                <span>{issue.milestone}</span>
              </>
            ) : null}
          </p>
          <p className="mt-3 flex flex-wrap items-center gap-x-2 text-md text-ink" data-testid="issue-work">
            {work.personIsAssignee && work.person ? <Mark>{work.text}.</Mark> : <span>{work.text}{work.person ? "," : "."}</span>}
            {work.person && !work.personIsAssignee ? (
              <span className="inline-flex items-center gap-1.5">
                for <Mark>{work.person.display || work.person.id}.</Mark>
              </span>
            ) : null}
          </p>
        </header>

        <div className="flex max-w-[860px] min-w-0 flex-col gap-10">
          {issue.body.trim() ? <Markdown source={issue.body} repo={fullName} /> : <Quiet>No description.</Quiet>}

          {subs.length ? (
            <Sect id="subs-title" title={"Sub-issues · " + doneSubs + " of " + subs.length + " done"}>
              <ul className="flex flex-col divide-y divide-line">
                {subs.map((s) => (
                  <li key={s.id}>
                    <Link href={repoHref(fullName, "work/" + s.number)} className="flex flex-wrap items-center gap-x-3 gap-y-1 py-3 text-md hover:bg-hover">
                      <span className="min-w-0 flex-1 [overflow-wrap:anywhere]">
                        <span className="num text-ink-2">#{s.number}</span> {s.title}
                      </span>
                      <span className="text-xs text-ink-2">
                        <State shape={s.state === "closed" ? "closed" : s.assignees.length ? "running" : "open"}>{s.state === "closed" ? "Closed" : issueWork(s, directory).text}</State>
                      </span>
                    </Link>
                  </li>
                ))}
              </ul>
            </Sect>
          ) : null}

          <Sect id="linked-title" title="Pull requests">
            {linked.length ? (
              <ul className="flex flex-col divide-y divide-line">
                {linked.map((c) => {
                  const st = PULL_STATE[c.state] ?? { word: "Open", shape: "open" as const };
                  return (
                    <li key={c.id}>
                      <Link href={repoHref(fullName, "changes/" + c.id)} className="flex flex-wrap items-center gap-x-3 gap-y-1 py-3 text-md hover:bg-hover">
                        <span className="min-w-0 flex-1 [overflow-wrap:anywhere]">
                          <span className="num text-ink-2">#{c.number}</span> {c.title}
                        </span>
                        <span className="text-xs text-ink-2">
                          <State shape={st.shape}>{st.word}</State>
                        </span>
                      </Link>
                    </li>
                  );
                })}
              </ul>
            ) : (
              <Quiet>No linked pull requests.</Quiet>
            )}
          </Sect>

          <Sect id="assign-title" title="Assigned to">
            <div className="flex max-w-md flex-col gap-4">
              {issue.assignees.length ? (
                <ul className="flex flex-col gap-1.5 text-md">
                  {issue.assignees.map((a) => (
                    <li key={a.id} className="flex flex-wrap items-center gap-x-2">
                      {a.kind === "PRINCIPAL_KIND_AGENT" ? (
                        <>
                          <Link href={repoHref(fullName, "agents/" + a.id)} className="link">
                            {a.display || a.id}
                          </Link>
                          <span className="text-xs text-ink-2">agent{a.runtime ? " · " + a.runtime : ""}</span>
                        </>
                      ) : (
                        <Mark>{a.display || a.id}</Mark>
                      )}
                    </li>
                  ))}
                </ul>
              ) : (
                <Quiet>Nobody is assigned.</Quiet>
              )}
              {me && (people.length > 0 || issue.assignees.length > 0) ? <AssignIssue key={issue.assignees.map((a) => a.id).join(",")} repo={fullName} number={n} current={issue.assignees.map((a) => a.id)} people={people} /> : null}
            </div>
          </Sect>

          <Sect id="conv-title" title="Conversation">
            <ol className="flex flex-col gap-6">
              {issue.comments.map((c) => (
                <li key={c.id} className="flex gap-3">
                  <PrincipalMark principal={c.author} size={28} />
                  <div className="min-w-0 flex-1">
                    <p className="text-base">
                      <span className="font-medium">{c.author?.display || c.author?.id}</span> <Time iso={c.createdAt} className="text-xs text-ink-2" />
                    </p>
                    <Markdown source={c.body} className="mt-1" repo={fullName} />
                  </div>
                </li>
              ))}
            </ol>
            {issue.comments.length === 0 ? <Quiet>No comments yet.</Quiet> : null}
            {me ? (
              <div className="mt-8 max-w-2xl">
                <IssueComment repo={fullName} number={n} />
              </div>
            ) : null}
          </Sect>
        </div>
      </>
    );
  });
}
