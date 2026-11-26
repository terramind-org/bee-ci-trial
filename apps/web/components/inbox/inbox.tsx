/**
 * The signed-in home: Needs you (docs/spec/web-ui.md §3). One list of plain sentences across
 * every repository, highest risk first, then a quiet count of what merged without anyone, and
 * below that what agents are doing now and the week's figures. Every part streams and fails on
 * its own; the loads are cached per request, so parts that share a scan (and the rail's count)
 * read it once.
 *
 * In the agent's view the same facts are plain text, followed by the tools this page calls.
 */
import type { ReactNode } from "react";
import { Suspense } from "react";
import { PageFacts, type FactGroup } from "@/components/bee/page-facts";
import { loadSection } from "@/components/home/section";
import { PageMenu } from "@/components/menu";
import { Quiet, Sect } from "@/components/parts";
import { PromptCard } from "@/components/prompt-card";
import { ButtonLink, EmptyState, Skeleton, SkeletonRows } from "@/components/ui";
import type { FeedEvent } from "@/lib/api/types";
import { DAY, agentWork, doneSince, weekOf, type DoneGroup } from "@/lib/inbox/progress";
import { loadAgents, loadMerges, loadNeeds, MAX_CHANGES, needCount, MAX_REPOS, requestNow, type NeedsScan } from "@/lib/inbox/load";
import { needWords, type Need, type SafetyNeed } from "@/lib/inbox/needs";
import type { Mode } from "@/lib/mode";
import { promptTargets } from "@/lib/prompt-endpoint";
import { capitalize, countWord, list, noun, riskOf } from "@/lib/inbox/words";
import { repoHref } from "@/lib/routes";
import { pageTools } from "@/lib/tools";
import { DoneToday } from "./done-today";
import { InProgress, workSentence } from "./in-progress";
import { NeedsList, needHref, needRef } from "./needs-list";
import { ThisWeek, weekFigures } from "./this-week";

export function Inbox({ mode = "keeper" }: { mode?: Mode }) {
  if (mode === "bee")
    return (
      <div data-testid="inbox">
        <Suspense fallback={<p role="status">Reading your repositories.</p>}>
          <InboxFacts />
        </Suspense>
      </div>
    );
  return (
    <div data-testid="inbox">
      <header className="mb-9 flex items-start justify-between gap-4">
        <h1 className="min-w-0">
          <span className="eyebrow mb-4 block">Needs you · all repositories</span>
          <span className="display block max-w-[20ch]">
            <Suspense fallback={<Skeleton className="h-[1.06em] w-[12ch]" />}>
              <Headline />
            </Suspense>
          </span>
        </h1>
        <PageMenu op={{ service: "RepoService", method: "ListMyRepos", request: {}, label: "List your repositories" }} />
      </header>
      <Suspense fallback={<SectionsSkeleton />}>
        <Body />
      </Suspense>
    </div>
  );
}

/** Posts and safety attention appear alongside pull requests in the same count. */
export function headline(needs: Pick<Need, "kind">[], safetyCount = 0): string {
  const n = needs.length + safetyCount;
  if (!n) return "Nothing needs you";
  const mixed = safetyCount > 0 || needs.some((x) => x.kind === "post");
  return capitalize(countWord(n)) + " " + (mixed ? noun(n, "thing needs", "things need") : noun(n, "pull request needs", "pull requests need")) + " you";
}

async function Headline() {
  const scan = await loadNeeds().catch(() => null);
  return (
    <>
      {scan ? headline(scan.needs, scan.safety.length) : "Needs you"}
      <span className="text-ink-2">.</span>
    </>
  );
}

/** The page body, or the first steps for someone with no repositories yet. */
async function Body() {
  const { endpoint } = await promptTargets();
  return loadSection("Your repositories", loadNeeds, (scan) => (scan.repos.length ? <Sections /> : <EmptyInbox endpoint={endpoint} />));
}

/** The signed-in first-run panel keeps the setup prompt beside the two ways to get a repository. */
export function EmptyInbox({ endpoint }: { endpoint: string }) {
  return (
    <div data-testid="home-onboarding">
      <EmptyState
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
        This page fills in once agents work in a repository of yours: pull requests the rules send to you, agents that are stuck, and what merged without anyone.
      </EmptyState>
      <div className="max-w-xl border-t border-line pt-5">
        <PromptCard id="setup-repo" compact vars={{ endpoint }} />
      </div>
    </div>
  );
}

function Sections() {
  return (
    <>
      <section aria-label="Needs you" id="needs-you" className="scroll-mt-10">
        <Suspense fallback={<SkeletonRows rows={3} />}>
          <NeedsSection />
        </Suspense>
      </section>
      <div className="mt-6">
        <Suspense fallback={<Skeleton className="h-5 w-96 max-w-full" />}>
          <MergedLine />
        </Suspense>
      </div>
      <div className="mt-12 flex flex-col gap-10">
        <Part id="in-progress" title="In progress" rows={2}>
          <ProgressSection />
        </Part>
        <Part id="done-today" title="Merged today" rows={2}>
          <DoneSection />
        </Part>
        <Part id="this-week" title="This week" rows={3}>
          <WeekSection />
        </Part>
      </div>
    </>
  );
}

/** The quiet secondary parts while the repositories are scanned. */
export function SectionsSkeleton() {
  return (
    <>
      <SkeletonRows rows={3} />
      <div className="mt-12 flex flex-col gap-10">
        {["In progress", "Merged today", "This week"].map((title) => (
          <Sect key={title} id={"loading-" + title.toLowerCase().replace(/\s+/g, "-")} title={title}>
            <Skeleton className="h-5 w-64 max-w-full" />
          </Sect>
        ))}
      </div>
    </>
  );
}

function Part({ id, title, rows, children }: { id: string; title: string; rows: number; children: ReactNode }) {
  return (
    <Sect id={id + "-title"} title={title}>
      <div id={id} className="scroll-mt-10">
        <Suspense fallback={<SkeletonRows rows={rows} />}>{children}</Suspense>
      </div>
    </Sect>
  );
}

/** What merged in the last day, and how much of it no person had to merge. */
export function mergedToday(groups: DoneGroup[]): { merged: number; repos: number; byAgents: number } {
  const items: FeedEvent[] = groups.flatMap((g) => g.items);
  return { merged: items.length, repos: groups.length, byAgents: items.filter((e) => e.actor?.kind === "PRINCIPAL_KIND_AGENT").length };
}

/** "Six commits merged today across two repositories. Four were merged by agents on their own." */
export function mergedSentence(groups: DoneGroup[]): string {
  const { merged, repos, byAgents } = mergedToday(groups);
  if (!merged) return "Nothing has merged in your repositories in the last day.";
  const head = capitalize(countWord(merged)) + " " + noun(merged, "commit") + " merged today across " + countWord(repos) + " " + noun(repos, "repository", "repositories") + ".";
  const tail = byAgents === merged ? (merged === 1 ? " An agent merged it on its own." : " Agents merged all of them on their own.") : byAgents ? " " + capitalize(countWord(byAgents)) + " " + noun(byAgents, "was", "were") + " merged by agents on their own." : " A person merged " + (merged === 1 ? "it." : "each of them.");
  return head + tail;
}

async function loadDone(): Promise<DoneGroup[]> {
  const [scan, merges] = await Promise.all([loadNeeds(), loadMerges()]);
  return doneSince(merges, new Set(scan.repos.map((r) => r.fullName)), requestNow() - DAY);
}

async function MergedLine() {
  const groups = await loadDone().catch(() => null);
  return groups ? <Quiet>{mergedSentence(groups)}</Quiet> : null;
}

/** What the scan could not cover, in sentences: shared by both views. */
function scanNotes(scan: NeedsScan): string[] {
  const openNeeds = scan.needs.filter((n) => n.kind !== "post").length;
  const waiting = scan.facts.length - openNeeds;
  const notes: string[] = [];
  if (waiting > 0) notes.push(capitalize(countWord(waiting)) + " other open " + noun(waiting, "pull request is", "pull requests are") + " waiting on someone else.");
  if (scan.failed.length) notes.push("Could not check " + list(scan.failed) + "; what needs you there is not listed.");
  if (scan.safetyFailed.length) notes.push("Could not read safety items for " + list(scan.safetyFailed) + ".");
  if (scan.truncated) notes.push("Only your " + MAX_REPOS + " most recently pushed repositories and their " + MAX_CHANGES + " most recently revised open pull requests are checked.");
  return notes;
}

async function NeedsSection() {
  return loadSection("Needs you", loadNeeds, (scan) => (
    <>
      {needCount(scan) ? <NeedsList needs={scan.needs} safety={scan.safety} now={requestNow()} /> : null}
      {scanNotes(scan).map((note) => (
        <Quiet key={note} className="mt-4">
          {note}
        </Quiet>
      ))}
    </>
  ));
}

async function loadWork() {
  const [scan, agents] = await Promise.all([loadNeeds(), loadAgents()]);
  return agentWork(agents.agents, agents.leases, scan.facts);
}

async function ProgressSection() {
  return loadSection("In progress", loadWork, (work) => (work.length ? <InProgress work={work} /> : <Quiet>No agent is editing files or has a pull request open.</Quiet>));
}

async function DoneSection() {
  return loadSection("Merged today", loadDone, (groups) => (groups.length ? <DoneToday groups={groups} now={requestNow()} /> : <Quiet>Nothing has merged in your repositories in the last day.</Quiet>));
}

async function loadWeek() {
  const [scan, agents, merges] = await Promise.all([loadNeeds(), loadAgents(), loadMerges()]);
  return weekOf(merges, new Set(scan.repos.map((r) => r.fullName)), requestNow(), agents.agents, scan.open);
}

async function WeekSection() {
  return loadSection("This week", loadWeek, (week) => <ThisWeek week={week} />);
}

/** One need as a line of text: the same sentence the person reads, with its address. */
export function needLine(n: Need): string {
  const words = needWords(n);
  const risk = n.kind === "post" ? "" : " · " + riskOf(n.change.revisions.at(-1)?.diffClass ?? "").level.toLowerCase() + " risk";
  return needRef(n) + " · " + (n.branch ? n.branch + " → " : "→ ") + n.change.targetRef + " · " + words.who + " " + words.verb + " “" + n.change.title + "”. " + words.why + (n.answers ? " · " + n.answers.display + " answers for it" : "") + risk + " · " + needHref(n);
}

export function safetyNeedLine(n: SafetyNeed): string {
  return n.repo + " · " + n.title + ". " + n.reason + (n.answers ? " · " + n.answers.display + " answers for it" : "") + " · " + n.href;
}

/** Needs you for an agent: the same facts as text, then the tools this page calls. */
async function InboxFacts() {
  return loadSection("Needs you", loadNeeds, async (scan) => {
    // The secondary parts are best effort here too: a part that cannot load is left out.
    const [work, done, week] = await Promise.all([loadWork().catch(() => null), loadDone().catch(() => null), loadWeek().catch(() => null)]);
    const groups: FactGroup[] = [
      {
        title: "facts",
        facts: [
          { label: "page", value: "Needs you, across all repositories" },
          { label: "signed in as", value: scan.me.display || scan.me.id },
          { label: "repositories", value: scan.checked.length ? scan.checked.join(", ") : "none" },
          { label: "needs a person", value: String(needCount(scan)) },
          { label: "open pull requests", value: String(scan.open) },
          done ? { label: "merged today", value: mergedSentence(done) } : null,
        ],
      },
      { title: "needs you", lines: [...scan.safety.map(safetyNeedLine), ...scan.needs.map(needLine), ...scanNotes(scan)], empty: "Nothing needs a person." },
    ];
    if (work) groups.push({ title: "in progress", lines: work.map((w) => workSentence(w) + (w.progress ? " · " + w.progress.label.toLowerCase() : "")), empty: "No agent is editing files or has a pull request open." });
    if (week) groups.push({ title: "this week", lines: weekFigures(week) });
    groups.push({ title: "repositories", lines: scan.repos.map((r) => r.fullName + " · " + repoHref(r.fullName)), empty: "None yet." });
    return <PageFacts page="needs" title="Needs you" groups={groups} tools={pageTools("needs")} />;
  });
}
