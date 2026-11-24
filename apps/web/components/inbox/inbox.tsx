/**
 * The signed-in home: the Inbox. "Needs you" lists only what this person can act on, "In
 * progress" what their agents are doing, "Done today" what merged, and a "This week" margin.
 * Every section streams and fails on its own; the loads are cached per request, so sections that
 * share a scan (and the rail's Inbox count) read it once.
 */
import type { ReactNode } from "react";
import { Suspense } from "react";
import { loadSection } from "@/components/home/section";
import { Icon } from "@/components/icons";
import { PageMenu } from "@/components/menu";
import { PromptCard } from "@/components/prompt-card";
import { ButtonLink, EmptyState, PageHeader, Skeleton, SkeletonRows } from "@/components/ui";
import { DAY, agentWork, doneSince, weekOf } from "@/lib/inbox/progress";
import { loadAgents, loadMerges, loadNeeds, MAX_CHANGES, needCount, MAX_REPOS, requestNow } from "@/lib/inbox/load";
import { promptTargets } from "@/lib/prompt-endpoint";
import { capitalize, countWord, list, noun } from "@/lib/inbox/words";
import { DoneToday } from "./done-today";
import { InProgress } from "./in-progress";
import { NeedsList } from "./needs-list";
import { ThisWeek } from "./this-week";

export function Inbox() {
  return (
    <div data-testid="inbox">
      <PageHeader
        title="Inbox"
        lede={
          <Suspense fallback={<Skeleton className="h-5 w-80 max-w-full" />}>
            <Lede />
          </Suspense>
        }
        actions={<PageMenu op={{ service: "RepoService", method: "ListMyRepos", request: {}, label: "List your repositories" }} />}
      />
      <Suspense fallback={<SectionsSkeleton />}>
        <Body />
      </Suspense>
    </div>
  );
}

/** The page body, or the first steps for someone with no repositories yet. */
async function Body() {
  const { endpoint } = await promptTargets();
  return loadSection("Your repositories", loadNeeds, (scan) =>
    scan.repos.length ? (
      <Sections />
    ) : (
      <EmptyInbox endpoint={endpoint} />
    ),
  );
}

/** The signed-in first-run panel keeps the setup prompt beside the two ways to get a repository. */
export function EmptyInbox({ endpoint }: { endpoint: string }) {
  return (
    <div data-testid="home-onboarding">
      <EmptyState
        icon="inbox"
        title="Nothing to look at yet"
        action={
          <>
            <ButtonLink href="/new" variant="primary" icon="plus">
              New repository
            </ButtonLink>
            <ButtonLink href="/import" icon="import">
              Import
            </ButtonLink>
          </>
        }
      >
        Your Inbox fills in once you work in a repository: reviews you are asked for, agents that are stuck, and what merged today.
      </EmptyState>
      <div className="max-w-xl border-t border-line pt-5">
        <PromptCard id="setup-repo" compact vars={{ endpoint }} />
      </div>
    </div>
  );
}

function Sections() {
  return (
    <div className="grid gap-x-16 gap-y-14 lg:grid-cols-[minmax(0,1fr)_220px]">
      <div className="flex min-w-0 flex-col gap-14">
        <Section id="needs-you" title="Needs you" fallback={<SkeletonRows rows={3} />}>
          <NeedsSection />
        </Section>
        <Section id="in-progress" title="In progress" fallback={<SkeletonRows rows={2} />}>
          <ProgressSection />
        </Section>
        <Section id="done-today" title="Done today" fallback={<SkeletonRows rows={2} />}>
          <DoneSection />
        </Section>
      </div>
      <aside aria-label="This week" className="min-w-0 lg:pt-1">
        <Section id="this-week" title="This week" small fallback={<SkeletonRows rows={3} />}>
          <WeekSection />
        </Section>
      </aside>
    </div>
  );
}

/** The sections' titles with placeholder rows, while the repositories are scanned. */
export function SectionsSkeleton() {
  const block = (title: string, rows: number, small = false) => (
    <div className="min-w-0">
      <h2 className={(small ? "text-md" : "text-xl") + " mb-3 font-semibold tracking-tight text-ink"}>{title}</h2>
      <SkeletonRows rows={rows} />
    </div>
  );
  return (
    <div className="grid gap-x-16 gap-y-14 lg:grid-cols-[minmax(0,1fr)_220px]">
      <div className="flex min-w-0 flex-col gap-14">
        {block("Needs you", 3)}
        {block("In progress", 2)}
        {block("Done today", 2)}
      </div>
      <div className="min-w-0 lg:pt-1">{block("This week", 3, true)}</div>
    </div>
  );
}

function Section({ id, title, small = false, fallback, children }: { id: string; title: string; small?: boolean; fallback: ReactNode; children: ReactNode }) {
  return (
    <section aria-labelledby={id + "-title"} id={id} className="min-w-0 scroll-mt-10">
      <h2 id={id + "-title"} className={(small ? "text-md" : "text-xl") + " mb-3 font-semibold tracking-tight text-ink"}>
        {title}
      </h2>
      <Suspense fallback={fallback}>{children}</Suspense>
    </section>
  );
}

/** "Two things need you. One agent is working, and three changes merged today." */
async function Lede() {
  const [scan, agents, merges] = await Promise.all([loadNeeds(), loadAgents(), loadMerges()]).catch(() => [null, null, null] as const);
  if (!scan || !agents || !merges) return null;
  const n = needCount(scan);
  const working = agentWork(agents.agents, agents.leases, scan.facts).length;
  const today = doneSince(merges, new Set(scan.repos.map((r) => r.fullName)), requestNow() - DAY).reduce((k, g) => k + g.items.length, 0);
  return (
    <>
      {n ? `${capitalize(countWord(n))} ${noun(n, "thing needs", "things need")} you.` : "Nothing needs you."}{" "}
      {working
        ? `${capitalize(countWord(working))} ${noun(working, "agent is", "agents are")} working, and ${countWord(today)} ${noun(today, "change", "changes")} merged today.`
        : `No agent is working right now, and ${countWord(today)} ${noun(today, "change", "changes")} merged today.`}
    </>
  );
}

async function NeedsSection() {
  return loadSection("Needs you", loadNeeds, (scan) => {
    const openNeeds = scan.needs.filter((n) => n.kind !== "post").length;
    const waiting = scan.facts.length - openNeeds;
    return (
      <>
        {scan.needs.length ? (
          <NeedsList needs={scan.needs} now={requestNow()} />
        ) : (
          <p className="flex items-center gap-2 py-2 text-md text-ink-2">
            <Icon name="check" size={20} weight="bold" className="text-ok" />
            You are all caught up.
          </p>
        )}
        {waiting > 0 ? (
          <Note>
            {capitalize(countWord(waiting))} other open change {noun(waiting, "request is", "requests are")} waiting on someone else.
          </Note>
        ) : null}
        {scan.failed.length ? <Note>Could not check {list(scan.failed)}; what needs you there is not listed.</Note> : null}
        {scan.truncated ? (
          <Note>
            Only your {MAX_REPOS} most recently pushed repositories and their {MAX_CHANGES} most recently revised open change requests are checked.
          </Note>
        ) : null}
      </>
    );
  });
}

async function ProgressSection() {
  return loadSection(
    "In progress",
    async () => {
      const [scan, agents] = await Promise.all([loadNeeds(), loadAgents()]);
      return agentWork(agents.agents, agents.leases, scan.facts);
    },
    (work) => (work.length ? <InProgress work={work} /> : <p className="py-2 text-md text-ink-2">No agent is editing files or has a change request open.</p>),
  );
}

async function DoneSection() {
  return loadSection(
    "Done today",
    async () => {
      const [scan, merges] = await Promise.all([loadNeeds(), loadMerges()]);
      return doneSince(merges, new Set(scan.repos.map((r) => r.fullName)), requestNow() - DAY);
    },
    (groups) => (groups.length ? <DoneToday groups={groups} now={requestNow()} /> : <p className="py-2 text-md text-ink-2">Nothing has merged in your repositories in the last day.</p>),
  );
}

async function WeekSection() {
  return loadSection(
    "This week",
    async () => {
      const [scan, agents, merges] = await Promise.all([loadNeeds(), loadAgents(), loadMerges()]);
      return weekOf(merges, new Set(scan.repos.map((r) => r.fullName)), requestNow(), agents.agents, scan.open);
    },
    (week) => <ThisWeek week={week} />,
  );
}

function Note({ children }: { children: ReactNode }) {
  return <p className="mt-4 text-base text-ink-3">{children}</p>;
}
