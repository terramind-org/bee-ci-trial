import Link from "next/link";
import type { Metadata } from "next";
import { PageMenu } from "@/components/menu";
import { PrincipalMark } from "@/components/domain";
import { BoardMove } from "@/components/issue-actions";
import { PageHeader } from "@/components/ui";
import { type RepoParams, scopeFromParams, serverClient } from "@/lib/api/server";
import { repoHref } from "@/lib/routes";
import { guard } from "@/lib/page";

export const metadata: Metadata = { title: "Board" };

export default async function BoardPage({ params }: { params: Promise<RepoParams> }) {
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName } = await scopeFromParams(params);
    const [project, issues] = await Promise.all([api.project.getProject({ repo: ref }), api.issue.listIssues({ repo: ref })]);
    const byNumber = new Map(issues.issues.map((i) => [i.number, i]));
    const op = { service: "ProjectService" as const, method: "GetProject", request: { repo: ref }, label: "Show the board" };
    return (
      <>
        <PageHeader
          eyebrow={
            <Link href={repoHref(fullName, "work")} className="hover:text-ink">
              Issues
            </Link>
          }
          title={project.name}
          lede="Agents move cards through the same API you do, so everyone works on one board."
          actions={<PageMenu op={op} />}
        />
        <div className="grid gap-8 overflow-x-auto pb-2 md:grid-cols-4">
          {project.columns.map((col) => (
            <section key={col.id} aria-labelledby={"col-" + col.id} className="flex min-w-0 flex-col">
              <h2 id={"col-" + col.id} className="mb-3 flex items-baseline justify-between border-b border-line pb-2 text-md font-semibold">
                {col.name}
                <span className="num text-base font-normal text-ink-3">{col.issueNumbers.length}</span>
              </h2>
              <ul className="flex flex-col gap-1">
                {col.issueNumbers.map((n) => {
                  const i = byNumber.get(n);
                  if (!i) return null;
                  return (
                    <li key={n} className="rounded-md px-2 py-3 hover:bg-hover" data-testid={"card-" + n}>
                      <Link href={repoHref(fullName, "work/" + n)} className="text-base font-medium hover:underline">
                        <span className="num text-ink-3">#{n}</span> {i.title}
                      </Link>
                      <div className="mt-2 flex items-center justify-between gap-2">
                        <span className="flex flex-wrap gap-x-2 text-sm text-ink-3">
                          {i.labels.slice(0, 2).map((l) => (
                            <span key={l}>{l}</span>
                          ))}
                        </span>
                        <span className="flex items-center gap-1.5">
                          {i.assignees.map((a) => (
                            <PrincipalMark key={a.id} principal={a} size={28} />
                          ))}
                          <BoardMove repo={fullName} project={project} number={n} column={col.id} />
                        </span>
                      </div>
                    </li>
                  );
                })}
                {col.issueNumbers.length === 0 ? <li className="px-2 py-3 text-base text-ink-3">Nothing here</li> : null}
              </ul>
            </section>
          ))}
        </div>
      </>
    );
  });
}
