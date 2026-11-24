/**
 * The change request page around its four tabs (docs/spec/web-ui.md §3 "Change detail"): the title
 * as the page's heading with a byline, the tabs, and the side rail every tab shares (landing, the
 * stack and revisions; under the content below 1024px). Document's reading column: big type,
 * hairlines instead of boxes.
 */
import type { Metadata } from "next";
import Link from "next/link";
import { Suspense, type ReactNode } from "react";
import { Avatar, toAvatar } from "@/components/avatar";
import { CloseChange, LandPanel } from "@/components/change-actions";
import { CrStateBadge, ImportedBadge } from "@/components/domain";
import { Icon } from "@/components/icons";
import { LiveRefresh } from "@/components/live-refresh";
import { MenuLink, PageMenu } from "@/components/menu";
import { Time } from "@/components/time";
import { Panel } from "@/components/ui";
import { objectIdText } from "@/lib/api/ids";
import { scopeFromParams } from "@/lib/api/server";
import { changeHref } from "@/lib/change-page";
import { isImporterPrincipal } from "@/lib/infra";
import { guard } from "@/lib/page";
import { repoHref } from "@/lib/routes";
import { runtimeName } from "@/lib/team";
import { loadChange, loadChecks, loadLanding, loadSiblings, loadStack } from "./data";
import { RevisionList } from "./revisions";
import { StackPanel } from "./stack-panel";
import { ChangeTabs } from "./tabs";

type Params = Promise<{ owner: string; repo: string; id: string }>;

export async function generateMetadata({ params }: { params: Params }): Promise<Metadata> {
  const { id } = await params;
  const { fullName } = await scopeFromParams(params);
  try {
    const cr = await loadChange(fullName, id);
    return { title: cr.title + " · Change request #" + cr.number };
  } catch {
    return { title: "Change request" };
  }
}

const VERB: Record<string, string> = { CHANGE_STATE_OPEN: "wants to merge into", CHANGE_STATE_LANDED: "merged into", CHANGE_STATE_CLOSED: "proposed merging into" };

export default async function ChangeLayout({ children, params }: { children: ReactNode; params: Params }) {
  const { id } = await params;
  const { fullName } = await scopeFromParams(params);
  return guard(async () => {
    // The header waits for the change alone; the tab count and "stacked on" stream in after it,
    // so the title (the page's largest paint) is not held back by other reads.
    const cr = await loadChange(fullName, id);
    const latest = cr.revisions.at(-1);
    const author = cr.author;
    const runtime = author?.kind === "PRINCIPAL_KIND_AGENT" ? runtimeName(author.runtime ?? "") : "";
    const getOp = { service: "ChangeService" as const, method: "GetChange", request: { repo: { fullName }, id }, label: "Read this change request" };
    return (
      <>
        <LiveRefresh repo={fullName} ids={[id]} kinds={["cr.*", "merge_queue.*", "review.*", "review_thread.*", "cr_comment.*", "check.*", "evidence.*", "post.*"]} />
        <header className="mb-8">
          <nav aria-label="Breadcrumb" className="mb-3 flex items-center gap-1.5 text-base text-ink-3">
            <Link href={repoHref(fullName, "changes")} className="inline-flex min-h-6 items-center hover:text-ink hover:underline">
              Change requests
            </Link>
            <Icon name="chevronRight" size={12} />
            <span aria-current="page" className="num">
              #{cr.number}
            </span>
          </nav>
          <div className="flex flex-col gap-4 md:flex-row md:items-start md:justify-between">
            <h1 className="max-w-[28ch] text-[28px] leading-[1.2] font-semibold tracking-tight text-ink [overflow-wrap:anywhere] md:text-[34px]">{cr.title}</h1>
            <div className="flex shrink-0 items-center gap-2">
              {cr.state === "CHANGE_STATE_OPEN" ? <CloseChange repo={fullName} crId={id} /> : null}
              <PageMenu op={getOp} register={false} label="Change request actions">
                <MenuLink href={changeHref(fullName, id, "agents")} icon="terminal">
                  For agents
                </MenuLink>
                <MenuLink href={repoHref(fullName, "changes/" + id + "/change.md")} icon="file">
                  Open change.md
                </MenuLink>
              </PageMenu>
            </div>
          </div>
          <p className="mt-4 flex flex-wrap items-center gap-x-2 gap-y-2 text-md text-ink-2">
            <Avatar principal={toAvatar(author)} size={28} />
            <span>
              <span className="font-medium text-ink">{author?.display || author?.id || "Someone"}</span>
              {runtime ? <> via {runtime}</> : null} {VERB[cr.state] ?? "targets"} <span className="mono text-sm text-ink">{cr.targetRef}</span>
              <span className="text-ink-3">
                {" · opened "}
                <Time iso={cr.createdAt} />
              </span>
            </span>
            <CrStateBadge state={cr.state} />
            {isImporterPrincipal(author) ? <ImportedBadge /> : null}
            {cr.dependsOn ? (
              <Suspense fallback={null}>
                <StackedOn fullName={fullName} parentId={cr.dependsOn} />
              </Suspense>
            ) : null}
          </p>
        </header>

        <Suspense fallback={<ChangeTabs repo={fullName} id={id} counts={{}} />}>
          <CountedTabs fullName={fullName} id={id} head={objectIdText(latest?.head)} />
        </Suspense>

        <div className="grid grid-cols-1 gap-x-14 gap-y-10 lg:grid-cols-[minmax(0,1fr)_300px]">
          {/* The tab first, so on a phone Code, Checks and For agents open on their own content. */}
          <div className="min-w-0">{children}</div>
          <aside className="flex min-w-0 flex-col gap-8" aria-label="Landing, stack and revisions">
            <Suspense fallback={<RailSkeleton />}>
              <Rail fullName={fullName} id={id} />
            </Suspense>
          </aside>
        </div>
      </>
    );
  });
}

/** The tabs with the Checks count, once the latest revision's checks are read. */
async function CountedTabs({ fullName, id, head }: { fullName: string; id: string; head: string }) {
  const checks = await loadChecks(fullName, head).catch(() => null);
  return <ChangeTabs repo={fullName} id={id} counts={{ checks: checks?.length }} />;
}

/** "stacked on <parent>": the change this one depends on, by title. */
async function StackedOn({ fullName, parentId }: { fullName: string; parentId: string }) {
  const parent = (await loadSiblings(fullName)).find((c) => c.id === parentId);
  if (!parent) return null;
  return (
    <Link href={changeHref(fullName, parent.id)} className="inline-flex min-h-6 items-center gap-1 text-ink-2 hover:text-ink hover:underline">
      <Icon name="branch" size={14} /> stacked on {parent.title}
    </Link>
  );
}

function RailSkeleton() {
  return <span aria-hidden="true" className="block h-40 animate-pulse rounded-lg bg-sunken" />;
}

/** Landing, the stack and revisions: the same on every tab. */
async function Rail({ fullName, id }: { fullName: string; id: string }) {
  const cr = await loadChange(fullName, id);
  const [{ evaluation, queue }, stack] = await Promise.all([loadLanding(fullName, id, cr.targetRef), loadStack(fullName, cr)]);
  const revisions = cr.revisions.map((r) => ({ n: r.n, head: objectIdText(r.head, true), diffClass: r.diffClass, createdAt: r.createdAt }));
  return (
    <>
      <Panel title="Landing" id="land-title">
        <LandPanel repo={fullName} crId={id} state={cr.state} evaluation={evaluation} queue={queue} />
      </Panel>
      {stack ? <StackPanel members={stack.members} op={stack.op} /> : null}
      <Panel title="Revisions" id="revs-title">
        <Suspense fallback={null}>
          <RevisionList repo={fullName} id={id} revisions={revisions} />
        </Suspense>
      </Panel>
    </>
  );
}
