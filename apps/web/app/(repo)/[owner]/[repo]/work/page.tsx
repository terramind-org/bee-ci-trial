/**
 * The Issues tab (docs/spec/web-ui.md §3, "Issues"): what people and agents have asked for. An
 * issue is how a person hands work to agents, so the list opens with one line to write one, and
 * each row says, after `#n` and the title, who is working on it and for whom.
 *
 * In the agent's view the same rows are lines of text, followed by the tools this page calls.
 */
import Link from "next/link";
import type { Metadata } from "next";
import { PageFacts } from "@/components/bee/page-facts";
import { FilterList } from "@/components/filter-list";
import { NewIssue } from "@/components/work/issue-actions";
import { IssueWorkState } from "@/components/work/issue-work-state";
import { issueAssignees } from "@/components/work/assignees";
import { PageMenu } from "@/components/menu";
import { Quiet, Sep } from "@/components/parts";
import { Time } from "@/components/time";
import { ButtonLink, TabLinks } from "@/components/kit";
import { type RepoParams, currentPrincipal, scopeFromParams, serverClient } from "@/lib/api/server";
import type { ChangeRequest } from "@/lib/api/types";
import { issueLine, issueWork, linkedPulls, pullsText } from "@/lib/issues";
import { guard, param, type SearchParams } from "@/lib/page";
import { directoryOf } from "@/lib/pulls";
import { repoHref } from "@/lib/routes";
import { pageTools } from "@/lib/tools";
import { requestMode } from "../../../../request-mode";

export const metadata: Metadata = { title: "Issues" };

export default async function IssuesPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  const sp = await searchParams;
  const state = param(sp, "state") === "closed" ? "closed" : "open";
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName, owner } = await scopeFromParams(params);
    const req = { repo: ref, state: state as "open" | "closed" };
    const [me, { mode }] = await Promise.all([currentPrincipal(), requestMode()]);
    const [list, all, changes, { members, people }] = await Promise.all([
      api.issue.listIssues(req),
      api.issue.listIssues({ repo: ref }),
      // Names the pull requests an issue links to; without them a row says nothing about them.
      api.change.listChanges({ repo: ref }).then((r) => r.changes, (): ChangeRequest[] => []),
      issueAssignees(api, owner, ref, !!me, mode === "keeper"),
    ]);
    const directory = directoryOf(members);
    const counts = { open: all.issues.filter((i) => i.state === "open").length, closed: all.issues.filter((i) => i.state === "closed").length };

    if (mode === "bee")
      return (
        <PageFacts
          page="issues"
          title={fullName + " · issues"}
          groups={[
            { title: "facts", facts: [{ label: "repository", value: fullName }, { label: "showing", value: state }, { label: "open", value: String(counts.open) }, { label: "closed", value: String(counts.closed) }] },
            { title: state + " issues", lines: list.issues.map((i) => issueLine(i, directory, changes) + " · " + repoHref(fullName, "work/" + i.number)), empty: state === "open" ? "No open issues." : "No closed issues." },
          ]}
          tools={pageTools("issues").filter((tool) => tool.kind === "read" || !!me)}
        />
      );

    const items = list.issues.map((i) => {
      const work = issueWork(i, directory);
      const pulls = pullsText(linkedPulls(i, changes));
      return {
        key: i.id,
        text: [i.number, i.title, i.type, ...i.labels, ...i.assignees.map((a) => a.display), work.person?.display, i.author?.display, i.milestone].join(" "),
        node: (
          <Link href={repoHref(fullName, "work/" + i.number)} data-nav-item className="flex flex-col gap-1.5 py-5 transition-colors duration-[120ms] hover:bg-hover focus-visible:bg-hover">
            <span className="text-lg font-light tracking-[-0.01em] text-ink [overflow-wrap:anywhere]">
              <span className="num text-ink-2">#{i.number}</span> {i.title}
            </span>
            <span className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-ink-2">
              <IssueWorkState issue={i} work={work} />
              {pulls ? (
                <>
                  <Sep />
                  <span>{pulls}</span>
                </>
              ) : null}
              {i.author ? (
                <>
                  <Sep />
                  <span>asked by {i.author.display || i.author.id}</span>
                </>
              ) : null}
              {i.type ? (
                <>
                  <Sep />
                  <span>{i.type}</span>
                </>
              ) : null}
              {i.labels.map((l) => (
                <span key={l} className="inline-flex items-center gap-x-2">
                  <Sep />
                  {l}
                </span>
              ))}
              {i.parentNumber ? (
                <>
                  <Sep />
                  <span>part of #{i.parentNumber}</span>
                </>
              ) : null}
              {i.subIssuesTotal ? (
                <>
                  <Sep />
                  <span>
                    {i.subIssuesDone}/{i.subIssuesTotal} sub-issues
                  </span>
                </>
              ) : null}
              {i.milestone ? (
                <>
                  <Sep />
                  <span>{i.milestone}</span>
                </>
              ) : null}
              <Sep />
              <Time iso={i.createdAt} />
            </span>
          </Link>
        ),
      };
    });
    return (
      <>
        <h1 className="sr-only">Issues</h1>
        {me ? <NewIssue key={fullName} repo={fullName} people={people} /> : null}
        <div className="flex items-start gap-3">
          <div className="min-w-0 flex-1">
            <TabLinks
              label="Issue state"
              items={[
                { href: repoHref(fullName, "work"), label: "Open", active: state === "open", count: counts.open },
                { href: repoHref(fullName, "work?state=closed"), label: "Closed", active: state === "closed", count: counts.closed },
              ]}
            />
          </div>
          <ButtonLink href={repoHref(fullName, "work/board")} size="sm">
            Board
          </ButtonLink>
          <PageMenu op={{ service: "IssueService", method: "ListIssues", request: req, label: "List issues" }} />
        </div>
        <FilterList
          items={items}
          quiet
          label="Filter issues"
          placeholder="Filter by title, label, agent or person"
          empty={<Quiet className="py-5">{state === "open" ? "No open issues." : "No closed issues."}</Quiet>}
        />
      </>
    );
  });
}
