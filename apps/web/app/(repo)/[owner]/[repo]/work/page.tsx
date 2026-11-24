import Link from "next/link";
import type { Metadata } from "next";
import { PageMenu } from "@/components/menu";
import { PrincipalMark } from "@/components/domain";
import { FilterList } from "@/components/filter-list";
import { Time } from "@/components/time";
import { ButtonLink, EmptyState, PageHeader, TabLinks } from "@/components/ui";
import { type RepoParams, scopeFromParams, serverClient } from "@/lib/api/server";
import { repoHref } from "@/lib/routes";
import { guard, param, type SearchParams } from "@/lib/page";

export const metadata: Metadata = { title: "Issues" };

export default async function WorkPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  const sp = await searchParams;
  const state = param(sp, "state") === "closed" ? "closed" : "open";
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName } = await scopeFromParams(params);
    const req = { repo: ref, state: state as "open" | "closed" };
    const [list, all] = await Promise.all([api.issue.listIssues(req), api.issue.listIssues({ repo: ref })]);
    const items = list.issues.map((i) => ({
      key: i.id,
      text: [i.number, i.title, i.type, ...i.labels, ...i.assignees.map((a) => a.display), i.milestone].join(" "),
      node: (
        <Link href={repoHref(fullName, "work/" + i.number)} data-nav-item className="flex flex-col gap-2 px-2 py-4 hover:bg-hover focus-visible:bg-hover md:flex-row md:items-center md:gap-6">
          <div className="min-w-0 flex-1">
            <p className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
              <span className="text-md font-medium text-ink">{i.title}</span>
              {i.labels.map((l) => (
                <span key={l} className="text-sm text-ink-3">
                  {l}
                </span>
              ))}
            </p>
            <p className="mt-0.5 flex flex-wrap gap-x-3 text-sm text-ink-3">
              <span className="num">#{i.number}</span>
              {i.type ? <span>{i.type}</span> : null}
              {i.parentNumber ? <span>part of #{i.parentNumber}</span> : null}
              {i.subIssuesTotal ? <span>{i.subIssuesDone}/{i.subIssuesTotal} sub-issues</span> : null}
              {i.linkedChanges.length ? <span>{i.linkedChanges.length} linked change{i.linkedChanges.length === 1 ? "" : "s"}</span> : null}
              {i.milestone ? <span>{i.milestone}</span> : null}
              <Time iso={i.createdAt} />
            </p>
          </div>
          <div className="flex items-center gap-1.5">
            {i.assignees.length ? (
              i.assignees.map((a) => <PrincipalMark key={a.id} principal={a} size={28} />)
            ) : (
              <span className="text-sm text-ink-3">unassigned</span>
            )}
          </div>
        </Link>
      ),
    }));
    return (
      <>
        <PageHeader
          title="Issues"
          lede="Things to do in this repository. Assign one to an agent and it gets started."
          actions={
            <>
              <ButtonLink href={repoHref(fullName, "work/board")} icon="board">
                Board
              </ButtonLink>
              <PageMenu op={{ service: "IssueService", method: "ListIssues", request: req, label: "List issues" }} />
            </>
          }
        />
        <TabLinks
          label="Issue state"
          items={[
            { href: repoHref(fullName, "work"), label: "Open", active: state === "open", count: all.issues.filter((i) => i.state === "open").length },
            { href: repoHref(fullName, "work?state=closed"), label: "Closed", active: state === "closed", count: all.issues.filter((i) => i.state === "closed").length },
          ]}
        />
        <FilterList
          items={items}
          label="Filter issues"
          placeholder="Filter by title, label, assignee or type…"
          empty={
            <EmptyState title={state === "open" ? "No open issues" : "No closed issues"} icon="work" action={state === "open" ? <ButtonLink href={repoHref(fullName, "work?state=closed")} size="sm">See closed issues</ButtonLink> : <ButtonLink href={repoHref(fullName, "work")} size="sm">See open issues</ButtonLink>}>
              {state === "open" ? "Nothing is waiting. File one with bee issue create." : "Issues you close are kept here."}
            </EmptyState>
          }
        />
      </>
    );
  });
}
