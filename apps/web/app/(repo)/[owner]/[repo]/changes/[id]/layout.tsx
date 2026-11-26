/**
 * The pull request page around its tabs (docs/spec/web-ui.md §3, "A pull request"). The top is
 * the same on every tab and is the first thing a reviewer who did not write the code needs:
 *
 *   ← Pull requests
 *   #41 Tax rounding per line
 *   billing-fixer wants to merge 3 commits into main from ws/billing/tax-rounding
 *   ● Dev Patel answers for this agent.
 *   [Approve and merge]  [Request changes]  [Close]
 *
 * The title waits for the pull request alone; the sentence under it, the person and the
 * decisions stream in after it. In the agent's view the top is one heading and the tabs.
 */
import type { Metadata } from "next";
import Link from "next/link";
import { Suspense, type ReactNode } from "react";
import { RuleList } from "@/components/brief/rules";
import { ImportedBadge } from "@/components/domain";
import { MenuLink, PageMenu } from "@/components/menu";
import { Mark, Sep } from "@/components/parts";
import { PullDecisionButtons } from "@/components/pull/decisions";
import { PullDetailLanding } from "@/components/pull/landing";
import { PullLive } from "@/components/pull/live";
import { Time } from "@/components/time";
import { Panel } from "@/components/ui";
import { objectIdText } from "@/lib/api/ids";
import { currentPrincipal, scopeFromParams } from "@/lib/api/server";
import type { ChangeRequest, Principal } from "@/lib/api/types";
import { changeHref } from "@/lib/change-page";
import { isImporterPrincipal } from "@/lib/infra";
import { guard } from "@/lib/page";
import { decisionsFor } from "@/lib/pull-page";
import { answerableFor, mergeVerb } from "@/lib/pulls";
import { repoHref } from "@/lib/routes";
import { requestMode } from "../../../../../request-mode";
import { loadBranch, loadChange, loadChecks, loadCommits, loadLanding, loadPeople, loadReviews, loadSiblings, loadStack } from "./data";
import { ChangeFrame } from "./frame";
import { RevisionList } from "./revisions";
import { StackPanel } from "./stack-panel";
import { ChangeTabs } from "./tabs";

type Params = Promise<{ owner: string; repo: string; id: string }>;

export async function generateMetadata({ params }: { params: Params }): Promise<Metadata> {
  const { id } = await params;
  const { fullName } = await scopeFromParams(params);
  try {
    const cr = await loadChange(fullName, id);
    return { title: cr.title + " · Pull request #" + cr.number };
  } catch {
    return { title: "Pull request" };
  }
}

export default async function ChangeLayout({ children, params }: { children: ReactNode; params: Params }) {
  const { id } = await params;
  const { fullName, owner } = await scopeFromParams(params);
  return guard(async () => {
    // The header waits for the pull request alone; everything else streams in after the title
    // (the page's largest paint), so it is not held back by other reads.
    const [cr, { mode }] = await Promise.all([loadChange(fullName, id), requestMode()]);
    const latest = cr.revisions.at(-1);
    const live = <PullLive key={cr.id} repo={fullName} id={cr.id} state={cr.state} />;
    const landing = <PullDetailLanding repo={fullName} id={cr.id} number={String(cr.number)} state={cr.state} target={cr.targetRef} />;
    const tabs = (
      <Suspense fallback={<ChangeTabs repo={fullName} id={id} counts={{}} />}>
        <CountedTabs fullName={fullName} id={id} head={objectIdText(latest?.head)} />
      </Suspense>
    );

    if (mode === "bee")
      return (
        <>
          <h1 className="mb-4 font-semibold">
            # {fullName} · pull request #{cr.number} · {cr.title}
          </h1>
          {landing}
          {live}
          {tabs}
          <div className="min-w-0">{children}</div>
        </>
      );

    const getOp = { service: "ChangeService" as const, method: "GetChange", request: { repo: { fullName }, id }, label: "Read this pull request" };
    return (
      <>
        <header className="mb-10">
          <div className="mb-5 flex items-center justify-between gap-4">
            <Link href={repoHref(fullName, "changes")} className="inline-flex min-h-6 items-center gap-1.5 text-sm text-ink-2 hover:text-ink hover:underline hover:underline-offset-4">
              <span aria-hidden="true">←</span> Pull requests
            </Link>
            <PageMenu op={getOp} register={false} label="Pull request actions">
              <MenuLink href={changeHref(fullName, id, "agents")} icon="terminal">
                For agents
              </MenuLink>
              <MenuLink href={repoHref(fullName, "changes/" + id + "/change.md")} icon="file">
                Open change.md
              </MenuLink>
            </PageMenu>
          </div>
          <h1 className="max-w-[24ch] text-2xl text-ink [overflow-wrap:anywhere] md:text-[40px] md:leading-[1.1]">
            <span className="num text-ink-2">#{cr.number}</span> {cr.title}
          </h1>
          {landing}
          {live}
          <Suspense fallback={<MergeLine repo={fullName} cr={cr} />}>
            <Byline fullName={fullName} owner={owner} cr={cr} />
          </Suspense>
          <div className="mt-7">
            <Suspense fallback={<span aria-hidden="true" className="block h-[46px] w-72 max-w-full rounded-full bg-sunken" />}>
              <Decisions fullName={fullName} cr={cr} />
            </Suspense>
          </div>
        </header>

        {tabs}

        <ChangeFrame
          rail={
            <Suspense fallback={<RailSkeleton />}>
              <Rail fullName={fullName} id={id} />
            </Suspense>
          }
        >
          {children}
        </ChangeFrame>
      </>
    );
  });
}

/**
 * "billing-fixer wants to merge 3 commits into main from ws/billing/tax · opened 2h ago", in
 * mono. The count and the branch are shown only when the server can say them.
 */
function MergeLine({ repo, cr, commits, branch, stackedOn }: { repo: string; cr: ChangeRequest; commits?: number; branch?: string; stackedOn?: ChangeRequest }) {
  const author = cr.author;
  return (
    <p className="mono mt-3 flex flex-wrap items-center gap-x-2 gap-y-1 text-ink-2" data-testid="merge-line">
      <span>
        <span className="text-ink">{author?.display || author?.id || "Someone"}</span> {mergeVerb(cr.state, commits)} <span className="text-ink">{cr.targetRef}</span>
        {branch ? (
          <>
            {" "}
            from <span className="text-ink [overflow-wrap:anywhere]">{branch}</span>
          </>
        ) : null}
      </span>
      <Sep />
      <span className="font-sans text-xs">
        opened <Time iso={cr.createdAt} />
      </span>
      {isImporterPrincipal(author) ? <ImportedBadge /> : null}
      {stackedOn ? (
        <>
          <Sep />
          <Link href={changeHref(repo, stackedOn.id)} className="link font-sans text-xs">
            stacked on #{stackedOn.number} {stackedOn.title}
          </Link>
        </>
      ) : null}
    </p>
  );
}

/** The merge sentence with what streams in, then the person who answers for the work. */
async function Byline({ fullName, owner, cr }: { fullName: string; owner: string; cr: ChangeRequest }) {
  const [commits, branch, people, siblings, me] = await Promise.all([
    loadCommits(fullName, cr).catch(() => ({ known: false, commits: [] })),
    loadBranch(fullName, cr),
    currentPrincipal().then((p) => (p ? loadPeople(owner) : new Map<string, Principal>())),
    cr.dependsOn ? loadSiblings(fullName) : Promise.resolve([]),
    currentPrincipal(),
  ]);
  const answers = answerableFor(cr.author, people);
  const agent = cr.author?.kind === "PRINCIPAL_KIND_AGENT";
  return (
    <>
      <MergeLine repo={fullName} cr={cr} commits={commits.known ? commits.commits.length : undefined} branch={branch} stackedOn={siblings.find((c) => c.id === cr.dependsOn)} />
      {answers ? (
        <p className="mt-4 text-md text-ink-2" data-testid="answers">
          <Mark className="text-ink">{answers.display}</Mark> {agent ? "answers for this agent." : me?.id === answers.id ? "(you) opened this and answers for it." : "opened this and answers for it."}
        </p>
      ) : null}
    </>
  );
}

/** Approve and merge, Request changes, Close: what this reader may decide, and why. */
async function Decisions({ fullName, cr }: { fullName: string; cr: ChangeRequest }) {
  const open = cr.state === "CHANGE_STATE_OPEN";
  const [viewer, landing, reviews] = await Promise.all([
    currentPrincipal(),
    open ? loadLanding(fullName, cr.id, cr.targetRef).catch(() => null) : Promise.resolve(null),
    open ? loadReviews(fullName, cr.id).catch(() => []) : Promise.resolve([]),
  ]);
  const decisions = decisionsFor({ change: cr, viewer, reviews, landing: landing?.evaluation, queue: landing?.queue });
  return (
    <PullDecisionButtons
      repo={fullName}
      crId={cr.id}
      number={String(cr.number)}
      revision={cr.revisions.at(-1)?.n ?? 0}
      target={cr.targetRef}
      decisions={decisions}
      signInHref={!viewer && open ? "/signin?next=" + encodeURIComponent(changeHref(fullName, cr.id)) : undefined}
    />
  );
}

/** The tabs with the Checks count, once the latest revision's checks are read. */
async function CountedTabs({ fullName, id, head }: { fullName: string; id: string; head: string }) {
  const checks = await loadChecks(fullName, head).catch(() => null);
  return <ChangeTabs repo={fullName} id={id} counts={{ checks: checks?.length }} />;
}

function RailSkeleton() {
  return <span aria-hidden="true" className="block h-40 rounded-lg bg-sunken" />;
}

/** The rules, the stack and revisions: beside Files changed, Checks and For agents. */
async function Rail({ fullName, id }: { fullName: string; id: string }) {
  const cr = await loadChange(fullName, id);
  const [landing, stack] = await Promise.all([cr.state === "CHANGE_STATE_OPEN" ? loadLanding(fullName, id, cr.targetRef) : Promise.resolve(null), loadStack(fullName, cr)]);
  const revisions = cr.revisions.map((r) => ({ n: r.n, head: objectIdText(r.head, true), diffClass: r.diffClass, createdAt: r.createdAt }));
  return (
    <>
      {landing ? (
        <Panel title="Rules" id="rules-title">
          <RuleList evaluation={landing.evaluation} />
        </Panel>
      ) : null}
      {stack ? <StackPanel members={stack.members} op={stack.op} /> : null}
      <Panel title="Revisions" id="revs-title">
        <Suspense fallback={null}>
          <RevisionList repo={fullName} id={id} revisions={revisions} />
        </Suspense>
      </Panel>
    </>
  );
}
