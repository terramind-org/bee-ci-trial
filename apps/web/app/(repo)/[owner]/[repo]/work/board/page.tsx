import Link from "next/link";
import type { Metadata } from "next";
import { PageFacts } from "@/components/bee/page-facts";
import { ButtonLink } from "@/components/kit";
import { PageMenu } from "@/components/menu";
import { Quiet, Rows, Sep, State } from "@/components/parts";
import { PageHeader } from "@/components/ui";
import { issueAssignees } from "@/components/work/assignees";
import { BoardMove } from "@/components/work/issue-actions";
import { IssueWorkState } from "@/components/work/issue-work-state";
import { type RepoParams, currentPrincipal, scopeFromParams, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { Issue } from "@/lib/api/types";
import { issueWork, issueWorkText } from "@/lib/issues";
import { guard } from "@/lib/page";
import { directoryOf } from "@/lib/pulls";
import { repoHref } from "@/lib/routes";
import { pageTools } from "@/lib/tools";
import { requestMode } from "../../../../../request-mode";

export const metadata: Metadata = { title: "Board" };

export default async function BoardPage({ params }: { params: Promise<RepoParams> }) {
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName, owner } = await scopeFromParams(params);
    const [me, { mode }, project] = await Promise.all([
      currentPrincipal(),
      requestMode(),
      api.project.getProject({ repo: ref }).catch((err: unknown) => {
        if (err instanceof BeeError && err.code === "not_found") return null;
        throw err;
      }),
    ]);
    const op = { service: "ProjectService" as const, method: "GetProject", request: { repo: ref }, label: "Show the board" };
    const back = <ButtonLink href={repoHref(fullName, "work")} size="sm">Issues</ButtonLink>;
    if (!project) {
      const empty = "No board is linked to this repository.";
      return mode === "bee" ? (
        <PageFacts page="issue-board" title={fullName + " · Board"} groups={[{ title: "board", empty }]} tools={pageTools("issue-board").filter((tool) => tool.op === "ProjectService/GetProject")} />
      ) : (
        <>
          <PageHeader title="Board" actions={back} />
          <Quiet>{empty}</Quiet>
        </>
      );
    }

    const [{ issues }, { members }] = await Promise.all([
      api.issue.listIssues({ repo: ref }),
      issueAssignees(api, owner, ref, !!me, false),
    ]);
    const directory = directoryOf(members);
    const byNumber = new Map(issues.map((issue) => [issue.number, issue]));
    const columns = project.columns.map((column) => ({
      ...column,
      issues: column.issueNumbers.map((number) => byNumber.get(number)).filter((issue): issue is Issue => !!issue),
    }));

    if (mode === "bee") {
      return (
        <PageFacts
          page="issue-board"
          title={fullName + " · " + project.name}
          groups={columns.length ? columns.map((column) => ({
            title: column.name,
            lines: column.issues.map((issue) => (
              "#" + issue.number + " · " + issue.title + " · " + issue.state + " · " +
              issueWorkText(issueWork(issue, directory)) +
              (issue.labels.length ? " · " + issue.labels.join(", ") : "") +
              " · " + repoHref(fullName, "work/" + issue.number)
            )),
            empty: "No issues in this column.",
          })) : [{ title: "board", empty: "This board has no columns." }]}
          tools={pageTools("issue-board").filter((tool) => tool.kind === "read" || !!me)}
        />
      );
    }

    return (
      <>
        <PageHeader title={project.name} actions={<>{back}<PageMenu op={op} /></>} />
        {columns.length ? (
          <div className="grid gap-8 sm:grid-cols-2 xl:grid-cols-4">
            {columns.map((column) => (
              <section key={column.id} aria-labelledby={"col-" + column.id} className="min-w-0">
                <h2 id={"col-" + column.id} className="mb-3 flex items-baseline justify-between gap-2 text-md font-medium">
                  {column.name}
                  <span aria-hidden="true" className="num text-sm font-normal text-ink-2">{column.issues.length}</span>
                </h2>
                {column.issues.length ? (
                  <Rows>
                    {column.issues.map((issue) => (
                      <li key={issue.id} data-testid={"card-" + issue.number} className="border-b border-line py-5">
                        <Link href={repoHref(fullName, "work/" + issue.number)} className="link inline-block min-h-6 max-w-full text-lg font-light [overflow-wrap:anywhere]">
                          <span className="num text-ink-2">#{issue.number}</span> {issue.title}
                        </Link>
                        <p className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-ink-2">
                          <State shape={issue.state}>{issue.state === "open" ? "Open" : "Closed"}</State>
                          <Sep />
                          <IssueWorkState work={issueWork(issue, directory)} />
                          {issue.labels.length ? <><Sep /><span>{issue.labels.join(", ")}</span></> : null}
                        </p>
                        {me ? <div className="mt-3"><BoardMove repo={fullName} project={project} number={issue.number} column={column.id} /></div> : null}
                      </li>
                    ))}
                  </Rows>
                ) : <Quiet>No issues in this column.</Quiet>}
              </section>
            ))}
          </div>
        ) : <Quiet>This board has no columns.</Quiet>}
      </>
    );
  });
}
