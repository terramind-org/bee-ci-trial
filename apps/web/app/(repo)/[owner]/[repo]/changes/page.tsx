/**
 * The Pull requests tab (docs/spec/web-ui.md §3): open pull requests grouped into Needs you,
 * Checks running and Waiting on someone else, then what merged this week. Each row is `#n` and
 * the title, the state as a word with a shape, the agent, the person who answers for it, and the
 * branch with its target. Merged, Closed and All are flat lists of the same row.
 *
 * In the agent's view the same rows are lines of text, followed by the tools this page calls.
 */
import type { Metadata } from "next";
import { PageFacts, type FactGroup } from "@/components/bee/page-facts";
import { LiveRefresh } from "@/components/live-refresh";
import { PageMenu } from "@/components/menu";
import { PullListLanding } from "@/components/pull/landing";
import { QueueList, type QueueListGroup } from "@/components/queue/queue-list";
import { QueueRowView, rowLine } from "@/components/queue/queue-row";
import { Button, EmptyState, Input, TabLinks } from "@/components/ui";
import { type RepoParams, currentPrincipal, scopeFromParams, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { BriefChecklist, ChangeRequest, ChangeState, CheckRun, Comment, Evidence, LandingEvaluation, Principal, QueueEntry, Ref, Review } from "@/lib/api/types";
import { classInfo } from "@/lib/diffclass";
import { shortId } from "@/lib/format";
import { groupQueue, type QueueFacts, type QueueRow } from "@/lib/queue";
import { statusOf } from "@/lib/status";
import { repoHref } from "@/lib/routes";
import { guard, param, type SearchParams } from "@/lib/page";
import { answerableFor, branchOf, directoryOf } from "@/lib/pulls";
import { pageTools } from "@/lib/tools";
import { requestMode } from "../../../../request-mode";

export const metadata: Metadata = { title: "Pull requests" };

type Tab = "open" | "merged" | "closed" | "all";
const STATES: Record<Tab, ChangeState> = { open: "CHANGE_STATE_OPEN", merged: "CHANGE_STATE_LANDED", closed: "CHANGE_STATE_CLOSED", all: "CHANGE_STATE_UNSPECIFIED" };

/** `?state=`: `merged` is the word in the interface; `landed`, its first name, still opens the same list. */
function tabOf(raw: string): Tab {
  if (raw === "landed") return "merged";
  return raw === "merged" || raw === "closed" || raw === "all" ? raw : "open";
}

/** ListEvidence takes 1 to 100 change ids per call. */
const EVIDENCE_BATCH = 100;

/** A read that is decoration or advice: when it fails the list still renders. */
function optional<T>(p: Promise<T>): Promise<T | undefined> {
  return p.catch(() => undefined);
}

export default async function ChangesPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  const sp = await searchParams;
  const tab = tabOf(param(sp, "state"));
  const query = param(sp, "q").trim();
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName, owner } = await scopeFromParams(params);
    const now = Date.now();
    const [viewer, { mode }] = await Promise.all([currentPrincipal(), requestMode()]);
    const listRequest = (state: ChangeState) => (query ? { repo: ref, state, query } : { repo: ref, state });
    const req = listRequest(STATES[tab]);
    let queryError = "";
    const listed = (r: ReturnType<typeof listRequest>) =>
      api.change.listChanges(r).catch((err: unknown) => {
        // A query the server cannot parse is shown beside the query box, not as a page error.
        if (query && err instanceof BeeError && err.code === "invalid_argument") {
          queryError = err.reason + ": " + err.message;
          return { changes: [] as ChangeRequest[] };
        }
        throw err;
      });
    // The Open tab also shows what merged in the last seven days.
    const [list, landedList, all, queue, refs, members] = await Promise.all([
      listed(req),
      tab === "open" ? listed(listRequest("CHANGE_STATE_LANDED")) : Promise.resolve({ changes: [] as ChangeRequest[] }),
      api.change.listChanges({ repo: ref }),
      api.change.listQueue({ repo: ref, ref: "main" }),
      // Names the branch each pull request comes from; without it a row shows the target alone.
      api.ref.listRefs({ repo: ref }).then((r) => r.refs, (): Ref[] => []),
      // The organisation's people, to name who answers for an agent. Members only.
      viewer ? api.org.listMembers({ org: owner }).then((r) => r.members.map((m) => m.principal), (): (Principal | undefined)[] => []) : Promise.resolve<(Principal | undefined)[]>([]),
    ]);
    const directory = directoryOf(members);
    const changes = tab === "open" ? [...list.changes, ...landedList.changes] : list.changes;
    // A merge can move a row across groups or into the selected tab. Keep the unfiltered state
    // alongside the shown rows so the client can tell it apart from a newly visible old merge.
    const states = Object.fromEntries([...all.changes, ...changes].map((c) => [c.id, c.state]));
    const open = changes.filter((c) => c.state === "CHANGE_STATE_OPEN");

    async function heroes(ids: string[]): Promise<Map<string, Evidence>> {
      const out = new Map<string, Evidence>();
      for (let i = 0; i < ids.length; i += EVIDENCE_BATCH) {
        const page = await optional(api.brief.listEvidence({ repo: ref, changeIds: ids.slice(i, i + EVIDENCE_BATCH), heroOnly: true }));
        for (const e of page?.evidence ?? []) if (!out.has(e.changeRequestId)) out.set(e.changeRequestId, e);
      }
      return out;
    }

    const [evals, checks, reviews, checklists, comments, hero] = await Promise.all([
      Promise.all(open.map((c) => api.change.evaluateLanding({ repo: ref, changeRequestId: c.id }).then((e) => [c.id, e] as const))),
      Promise.all(changes.map((c) => api.check.listChecks({ repo: ref, changeset: c.revisions.at(-1)?.head }).then((r) => [c.id, r.checks] as const))),
      Promise.all(open.map((c) => api.change.listReviews({ repo: ref, changeRequestId: c.id }).then((r) => [c.id, r.reviews] as const))),
      Promise.all(open.map((c) => optional(api.brief.getBriefChecklist({ repo: ref, changeId: c.id })).then((r) => [c.id, r] as const))),
      Promise.all(open.filter((c) => c.brief?.asks.length).map((c) => optional(api.change.listComments({ repo: ref, changeRequestId: c.id })).then((r) => [c.id, r?.comments] as const))),
      changes.length ? heroes(changes.map((c) => c.id)) : Promise.resolve(new Map<string, Evidence>()),
    ]);

    const evalBy = new Map<string, LandingEvaluation>(evals);
    const checksBy = new Map<string, CheckRun[]>(checks);
    const reviewsBy = new Map<string, Review[]>(reviews);
    const checklistBy = new Map<string, BriefChecklist | undefined>(checklists);
    const commentsBy = new Map<string, Comment[] | undefined>(comments);
    const queueBy = new Map<string, QueueEntry>(queue.entries.map((q) => [q.changeRequestId, q]));
    const count = (s: ChangeState) => all.changes.filter((c) => c.state === s).length;

    const facts: QueueFacts[] = changes.map((c) => ({
      change: c,
      landedAt: c.landedAt,
      landing: evalBy.get(c.id),
      checks: checksBy.get(c.id) ?? [],
      reviews: reviewsBy.get(c.id) ?? [],
      checklist: checklistBy.get(c.id),
      comments: commentsBy.get(c.id),
      queue: queueBy.get(c.id) ?? null,
    }));
    const extra = (row: QueueRow) => {
      const c = row.facts.change;
      return { branch: branchOf(refs, c.revisions.at(-1)?.head, c.targetRef), answers: answerableFor(c.author, directory) };
    };
    const item = (row: QueueRow, group: string) => {
      const c = row.facts.change;
      const rev = c.revisions.at(-1);
      const { branch, answers } = extra(row);
      const landing = c.state === "CHANGE_STATE_LANDED" ? <PullListLanding id={c.id} number={String(c.number)} target={c.targetRef} /> : null;
      return {
        key: c.id,
        text: ["#" + c.number, c.title, c.brief?.outcome, c.author?.display, answers?.display, c.author?.runtime, branch, rev?.diffClass, classInfo(rev?.diffClass ?? "").label, shortId(c.id)].join(" "),
        line: rowLine(row, { branch, answers }) + " · " + repoHref(fullName, "changes/" + c.id),
        landing,
        node: <QueueRowView row={row} repo={fullName} hero={hero.get(c.id)} showWhy={group !== "landed" && group !== "all"} needsYou={group === "needs-you"} branch={branch} answers={answers} titleDecoration={landing} />,
      };
    };

    let groups: (Omit<QueueListGroup, "items"> & { items: ReturnType<typeof item>[] })[];
    if (tab === "open") {
      groups = groupQueue(facts, viewer, now).map((g) => ({ key: g.key, label: g.label, empty: g.empty, items: g.rows.map((r) => item(r, g.key)) }));
    } else {
      // Merged, Closed and All are a flat list in the same row, newest first.
      const newest = (f: QueueFacts) => f.change.revisions.at(-1)?.createdAt ?? f.change.createdAt ?? "";
      const rows = [...facts]
        .sort((a, b) => newest(b).localeCompare(newest(a)) || a.change.id.localeCompare(b.change.id))
        .map((f): QueueRow => {
          const status = statusOf(f);
          return { facts: f, status, why: status.sentence };
        });
      groups = [{ key: "all", label: null, empty: "", items: rows.map((r) => item(r, "all")) }];
    }

    const live = <LiveRefresh repo={fullName} kinds={["cr.*", "merge_queue.*", "check.*", "review.*", "evidence.*"]} />;
    if (mode === "bee") {
      const facts: FactGroup[] = [
        {
          title: "facts",
          facts: [
            { label: "repository", value: fullName },
            { label: "showing", value: tab + (query ? ", matching " + query : "") },
            { label: "open", value: String(count("CHANGE_STATE_OPEN")) },
            { label: "merged", value: String(count("CHANGE_STATE_LANDED")) },
            { label: "closed", value: String(count("CHANGE_STATE_CLOSED")) },
            queryError ? { label: "query error", value: queryError } : null,
          ],
        },
        ...groups.map((g) => ({ title: (g.label ?? tab + " pull requests").toLowerCase(), lines: g.items.map((i) => i.landing ? <>{i.line} {i.landing}</> : i.line), empty: g.empty || "None." })),
      ];
      return <>{live}<PageFacts page="pulls" title={fullName + " · pull requests"} groups={facts} tools={pageTools("pulls")} /></>;
    }

    return (
      <>
        {live}
        <h1 className="sr-only">Pull requests</h1>
        <div className="flex items-start gap-4">
          <div className="min-w-0 flex-1">
            <TabLinks
              label="Pull request state"
              items={[
                { href: repoHref(fullName, "changes"), label: "Open", active: tab === "open", count: count("CHANGE_STATE_OPEN") },
                { href: repoHref(fullName, "changes?state=merged"), label: "Merged", active: tab === "merged", count: count("CHANGE_STATE_LANDED") },
                { href: repoHref(fullName, "changes?state=closed"), label: "Closed", active: tab === "closed", count: count("CHANGE_STATE_CLOSED") },
                { href: repoHref(fullName, "changes?state=all"), label: "All", active: tab === "all", count: all.changes.length },
              ]}
            />
          </div>
          <PageMenu op={{ service: "ChangeService", method: "ListChanges", request: req, label: "List pull requests" }} />
        </div>
        {/* The server-side query is one step away: open when it is in use or has failed. */}
        <details className="group mb-4 max-w-2xl" open={!!query || !!queryError}>
          <summary className="inline-flex min-h-6 cursor-pointer list-none items-center text-xs text-ink-2 hover:text-ink [&::-webkit-details-marker]:hidden">
            <span className="underline decoration-line-strong underline-offset-[3px]">Search with a query</span>
          </summary>
          <form action={repoHref(fullName, "changes")} className="mt-3 flex flex-col gap-1.5" role="search" aria-label="Query pull requests">
            {tab !== "open" ? <input type="hidden" name="state" value={tab} /> : null}
            <div className="flex gap-2">
              <label htmlFor="cr-query" className="sr-only">
                Query
              </label>
              <Input id="cr-query" name="q" defaultValue={query} placeholder="label:bug author:codex review:required" className="mono h-[34px]" aria-invalid={queryError ? true : undefined} aria-describedby={queryError ? "cr-query-error" : undefined} />
              <Button type="submit" size="sm">
                Search
              </Button>
            </div>
            {queryError ? (
              <p id="cr-query-error" role="alert" className="text-sm text-bad">
                {queryError}
              </p>
            ) : query ? (
              <p className="text-sm text-ink-2">
                Showing what matches <span className="mono">{query}</span> ·{" "}
                <a href={repoHref(fullName, tab === "open" ? "changes" : "changes?state=" + tab)} className="link">
                  clear
                </a>
              </p>
            ) : null}
          </form>
        </details>
        <QueueList
          groups={groups}
          label="Filter pull requests"
          placeholder="Filter by title, agent, person or branch"
          landing={{ scope: JSON.stringify([fullName, tab, query]), states }}
          empty={
            <EmptyState title={tab === "open" ? "No open pull requests" : "Nothing here yet"}>
              Agents open pull requests with <code className="mono">bee cr open</code>; they show up here with the person who answers for them.
            </EmptyState>
          }
        />
      </>
    );
  });
}
