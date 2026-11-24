import type { Metadata } from "next";
import { LiveRefresh } from "@/components/live-refresh";
import { PageMenu } from "@/components/menu";
import { QueueList, type QueueListGroup } from "@/components/queue/queue-list";
import { QueueRowView } from "@/components/queue/queue-row";
import { Button, EmptyState, Input, PageHeader, TabLinks } from "@/components/ui";
import { type RepoParams, currentPrincipal, scopeFromParams, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { BriefChecklist, ChangeRequest, ChangeState, CheckRun, Comment, Evidence, LandingEvaluation, QueueEntry, Review } from "@/lib/api/types";
import { classInfo } from "@/lib/diffclass";
import { shortId } from "@/lib/format";
import { groupQueue, type QueueFacts, type QueueRow } from "@/lib/queue";
import { statusOf } from "@/lib/status";
import { repoHref } from "@/lib/routes";
import { guard, param, type SearchParams } from "@/lib/page";

export const metadata: Metadata = { title: "Changes" };

const STATES: Record<string, ChangeState> = { open: "CHANGE_STATE_OPEN", landed: "CHANGE_STATE_LANDED", closed: "CHANGE_STATE_CLOSED", all: "CHANGE_STATE_UNSPECIFIED" };

/** ListEvidence takes 1 to 100 change ids per call. */
const EVIDENCE_BATCH = 100;

/** A read that is decoration or advice: when it fails the list still renders. */
function optional<T>(p: Promise<T>): Promise<T | undefined> {
  return p.catch(() => undefined);
}

export default async function ChangesPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  const sp = await searchParams;
  const tab = STATES[param(sp, "state")] ? param(sp, "state") : "open";
  const query = param(sp, "q").trim();
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName } = await scopeFromParams(params);
    const now = Date.now();
    const viewer = await currentPrincipal();
    const listRequest = (state: ChangeState) => (query ? { repo: ref, state, query } : { repo: ref, state });
    const req = listRequest(STATES[tab] ?? "CHANGE_STATE_OPEN");
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
    // The queue (Open tab) also shows what landed in the last seven days.
    const [list, landedList, all, queue] = await Promise.all([
      listed(req),
      tab === "open" ? listed(listRequest("CHANGE_STATE_LANDED")) : Promise.resolve({ changes: [] as ChangeRequest[] }),
      api.change.listChanges({ repo: ref }),
      api.change.listQueue({ repo: ref, ref: "main" }),
    ]);
    const changes = tab === "open" ? [...list.changes, ...landedList.changes] : list.changes;
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
      landing: evalBy.get(c.id),
      checks: checksBy.get(c.id) ?? [],
      reviews: reviewsBy.get(c.id) ?? [],
      checklist: checklistBy.get(c.id),
      comments: commentsBy.get(c.id),
      queue: queueBy.get(c.id) ?? null,
    }));
    const item = (row: QueueRow, showWhy: boolean) => {
      const c = row.facts.change;
      const rev = c.revisions.at(-1);
      return {
        key: c.id,
        text: [c.title, c.brief?.outcome, c.author?.display, c.author?.runtime, rev?.diffClass, classInfo(rev?.diffClass ?? "").label, shortId(c.id)].join(" "),
        node: <QueueRowView row={row} repo={fullName} hero={hero.get(c.id)} showWhy={showWhy} />,
      };
    };

    let groups: QueueListGroup[];
    if (tab === "open") {
      groups = groupQueue(facts, viewer, now).map((g) => ({ key: g.key, label: g.label, empty: g.empty, items: g.rows.map((r) => item(r, g.key === "needs-you" || g.key === "waiting")) }));
    } else {
      // Landed, Closed and All are a flat list in the same row, newest first.
      const newest = (f: QueueFacts) => f.change.revisions.at(-1)?.createdAt ?? f.change.createdAt ?? "";
      const rows = [...facts]
        .sort((a, b) => newest(b).localeCompare(newest(a)) || a.change.id.localeCompare(b.change.id))
        .map((f): QueueRow => {
          const status = statusOf(f);
          return { facts: f, status, why: status.sentence };
        });
      groups = [{ key: "all", label: null, empty: "", items: rows.map((r) => item(r, false)) }];
    }

    return (
      <>
        <LiveRefresh repo={fullName} kinds={["cr.*", "merge_queue.*", "check.*", "review.*", "evidence.*"]} />
        <PageHeader title="Changes" lede="What needs a decision, what can land, and what is waiting on agents." actions={<PageMenu op={{ service: "ChangeService", method: "ListChanges", request: req, label: "List change requests" }} />} />
        <TabLinks
          label="Change state"
          items={[
            { href: repoHref(fullName, "changes"), label: "Open", active: tab === "open", count: count("CHANGE_STATE_OPEN") },
            { href: repoHref(fullName, "changes?state=landed"), label: "Landed", active: tab === "landed", count: count("CHANGE_STATE_LANDED") },
            { href: repoHref(fullName, "changes?state=closed"), label: "Closed", active: tab === "closed", count: count("CHANGE_STATE_CLOSED") },
            { href: repoHref(fullName, "changes?state=all"), label: "All", active: tab === "all", count: all.changes.length },
          ]}
        />
        <form action={repoHref(fullName, "changes")} className="mb-6 flex flex-col gap-1.5" role="search" aria-label="Query change requests">
          {tab !== "open" ? <input type="hidden" name="state" value={tab} /> : null}
          <div className="flex gap-2">
            <label htmlFor="cr-query" className="sr-only">
              Query
            </label>
            <Input id="cr-query" name="q" defaultValue={query} placeholder="Query: label:bug author:codex review:required" className="mono" aria-invalid={queryError ? true : undefined} aria-describedby={queryError ? "cr-query-error" : undefined} />
            <Button type="submit" icon="search">
              Query
            </Button>
          </div>
          {queryError ? (
            <p id="cr-query-error" role="alert" className="text-sm text-bad">
              {queryError}
            </p>
          ) : query ? (
            <p className="text-sm text-ink-3">
              Server-side query <span className="mono">{query}</span> ·{" "}
              <a href={repoHref(fullName, tab === "open" ? "changes" : "changes?state=" + tab)} className="link">
                clear
              </a>
            </p>
          ) : null}
        </form>
        <QueueList
          groups={groups}
          label="Filter changes"
          placeholder="Filter by title, outcome, author, runtime or class…"
          empty={
            <EmptyState title={tab === "open" ? "No open changes" : "Nothing here yet"} icon="changes">
              Agents open change requests with <code className="mono">bee cr open</code>; they show up here with their risk class.
            </EmptyState>
          }
        />
      </>
    );
  });
}
