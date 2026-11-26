"use client";

import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import { useEffect, useMemo, useRef, useState } from "react";
import { browserClient } from "@/lib/api/browser";
import { objectIdText, pathText } from "@/lib/api/ids";
import type { Event, Principal } from "@/lib/api/types";
import { BeeError } from "@/lib/api/transport";
import { DiffClassBadge, LiveDot, PrincipalMark, StatusIcon } from "./domain";
import { Time } from "./time";
import { Icon } from "./icons";
import { Badge, Button, EmptyState, SkeletonRows, cx } from "./ui";
import { repoFromPath, repoHref } from "@/lib/routes";

type Filter = "all" | "landing" | "review" | "leases" | "checks" | "work";
const CHECKS = /^(check\.|check_suite\.|run\.|workflow_)/;
/** Bookkeeping the server records for every job step; shown under "Checks and runs" only. */
const BOOKKEEPING = (e: Event) => /^(workflow_usage\.|check_suite\.|workflow_job\.(queued|in_progress))/.test(e.kind) || (e.kind === "check.updated" && (e.check?.status === "CHECK_STATUS_QUEUED" || e.check?.status === "CHECK_STATUS_RUNNING"));
const FILTERS: { id: Filter; label: string; kinds: (e: Event) => boolean }[] = [
  { id: "all", label: "Everything", kinds: (e) => !BOOKKEEPING(e) },
  { id: "landing", label: "Merging", kinds: (e) => /^(cr\.|merge_queue\.|ref\.updated)/.test(e.kind) },
  { id: "review", label: "Review", kinds: (e) => /^(review\.|review_thread\.|thread\.|cr_comment\.)/.test(e.kind) },
  { id: "leases", label: "Leases", kinds: (e) => e.kind.startsWith("lease.") },
  { id: "checks", label: "Checks and runs", kinds: (e) => CHECKS.test(e.kind) },
  { id: "work", label: "Issues and releases", kinds: (e) => /^(issue|issue_comment|release|agent|security|token|package|pages|code_scanning|secret_scanning|dependabot|push_protection)[._]/.test(e.kind) },
];

const MAX = 300;

export function LiveFeed({ repo, directory }: { repo: string; directory: Principal[] }) {
  const router = useRouter();
  // Links go to the repo whose page this is; `repo` may be an org-wide stream (`org/*`).
  const linkRepo = repoFromPath(usePathname()) ?? repo;
  const [events, setEvents] = useState<Event[] | null>(null);
  const [error, setError] = useState<BeeError | null>(null);
  const [filter, setFilter] = useState<Filter>("all");
  const [agentsOnly, setAgentsOnly] = useState(false);
  const [paused, setPaused] = useState(false);
  const [buffered, setBuffered] = useState(0);
  const [attempt, setAttempt] = useState(0);
  const pausedRef = useRef(paused);
  const bufferRef = useRef<Event[]>([]);
  const lastSeq = useRef("0");
  // Live events are batched per animation frame: a burst lands as one render, one smooth push.
  const queue = useRef<Event[]>([]);
  const frame = useRef(0);
  const people = useMemo(() => new Map(directory.map((p) => [p.id, p])), [directory]);
  const titles = useMemo(() => {
    const m = new Map<string, string>();
    for (const e of events ?? []) if (e.change && !m.has(e.change.id)) m.set(e.change.id, e.change.title);
    return m;
  }, [events]);

  useEffect(() => {
    pausedRef.current = paused;
    if (!paused && bufferRef.current.length) {
      const incoming = bufferRef.current.splice(0);
      setEvents((prev) => [...incoming.reverse(), ...(prev ?? [])].slice(0, MAX));
      setBuffered(0);
    }
  }, [paused]);

  useEffect(() => {
    const ctl = new AbortController();
    let replaying = true;
    let refreshTimer: ReturnType<typeof setTimeout> | null = null;
    let quiet: ReturnType<typeof setTimeout> | null = null;
    const replay: Event[] = [];
    const scope = { repo: { fullName: repo } };
    (async () => {
      try {
        setError(null);
        const api = browserClient().event;
        // First connect: read the newest page of history in one request (GetEventLogInfo, then
        // ListEvents) and render it once, then stream only what is new. Nothing replays through
        // the stream, so a long log never trickles in and pushes the page around.
        if (lastSeq.current === "0") {
          try {
            const info = await api.getEventLogInfo(scope, { signal: ctl.signal });
            const latest = BigInt(info.latestSeq || "0");
            let start = latest - BigInt(MAX) + 1n;
            if (start < BigInt(info.oldestSeq || "0")) start = BigInt(info.oldestSeq || "0");
            if (start < 1n) start = 1n;
            const page = latest > 0n ? await api.listEvents({ ...scope, fromSeq: String(start), limit: MAX }, { signal: ctl.signal }) : { events: [] };
            if (ctl.signal.aborted) return;
            replaying = false;
            setEvents((prev) => [...page.events].reverse().concat(prev ?? []).slice(0, MAX));
            lastSeq.current = page.events.at(-1)?.seq ?? String(latest);
          } catch {
            // A server without ListEvents: fall through and replay history through the stream.
            if (ctl.signal.aborted) return;
          }
        }
        const it = api.subscribe({ ...scope, fromSeq: lastSeq.current }, { signal: ctl.signal });
        // Replay through the stream (reconnects, older servers) renders as one batch. The stream
        // has no caught-up marker, so history is told apart by time: anything stamped before we
        // subscribed (less clock-skew margin) is replay. The skeleton stays up until the batch
        // lands, and the empty state only shows after a silent stream, never as a flash.
        const since = Date.now() - 5000;
        const endReplay = () => {
          if (!replaying) return;
          replaying = false;
          if (quiet) clearTimeout(quiet);
          setEvents((prev) => [...replay.reverse(), ...(prev ?? [])].slice(0, MAX));
        };
        if (replaying) quiet = setTimeout(endReplay, 2500);
        for await (const ev of it) {
          lastSeq.current = ev.seq;
          const historic = !ev.time || Date.parse(ev.time) < since;
          if (replaying && historic) {
            replay.push(ev);
            if (quiet) clearTimeout(quiet);
            quiet = setTimeout(endReplay, 300);
            continue;
          }
          endReplay();
          if (pausedRef.current) {
            bufferRef.current.push(ev);
            setBuffered(bufferRef.current.length);
          } else {
            queue.current.push(ev);
            if (!frame.current)
              frame.current = requestAnimationFrame(() => {
                frame.current = 0;
                const batch = queue.current.splice(0);
                if (batch.length) setEvents((prev) => [...batch.reverse(), ...(prev ?? [])].slice(0, MAX));
              });
          }
          if (/^(lease\.|cr\.landed|cr\.enqueued|cr\.dequeued|merge_queue\.|ref\.updated)/.test(ev.kind)) {
            if (refreshTimer) clearTimeout(refreshTimer);
            refreshTimer = setTimeout(() => router.refresh(), 1200);
          }
        }
        if (quiet) clearTimeout(quiet);
        endReplay();
      } catch (err) {
        if (ctl.signal.aborted) return;
        setError(err instanceof BeeError ? err : new BeeError("unavailable", { reason: "UNAVAILABLE", message: String(err), retryable: true }));
        setEvents((prev) => prev ?? []);
        // Resume from the last sequence we saw: no gaps, no duplicates.
        setTimeout(() => setAttempt((a) => a + 1), 3000);
      }
    })();
    return () => {
      ctl.abort();
      if (quiet) clearTimeout(quiet);
      if (refreshTimer) clearTimeout(refreshTimer);
      if (frame.current) cancelAnimationFrame(frame.current);
      frame.current = 0;
      queue.current = [];
    };
  }, [repo, attempt, router]);

  const f = FILTERS.find((x) => x.id === filter) ?? FILTERS[0];
  const visible = (events ?? []).filter((e) => f?.kinds(e) && (!agentsOnly || people.get(e.principal)?.kind === "PRINCIPAL_KIND_AGENT"));
  const days = useMemo(() => groupByDay(visible), [visible]);

  return (
    <section aria-labelledby="feed-title" className="min-w-0">
      <header className="mb-3 flex flex-wrap items-center gap-2">
        <h2 id="feed-title" className="mr-1 text-xl font-semibold tracking-tight">
          Stream
        </h2>
        {error ? <Badge tone="bad">Reconnecting · {error.reason}</Badge> : paused ? <Badge tone="warn">Paused</Badge> : <LiveDot />}
        <div className="ml-auto flex flex-wrap items-center gap-1.5">
          <label className="flex h-8 items-center gap-1.5 rounded-md px-2 text-sm text-ink-2 hover:bg-hover">
            <input type="checkbox" checked={agentsOnly} onChange={(e) => setAgentsOnly(e.target.checked)} className="accent-[var(--accent)]" />
            Agents only
          </label>
          <Button size="sm" variant="ghost" icon={paused ? "play" : "pause"} onClick={() => setPaused((p) => !p)} aria-pressed={paused}>
            {paused ? (buffered ? "Resume (" + buffered + " new)" : "Resume") : "Pause"}
          </Button>
        </div>
      </header>
      <div role="radiogroup" aria-label="Filter events" className="mb-6 flex gap-1 overflow-x-auto scrollbar-thin">
        {FILTERS.map((x) => (
          <button
            key={x.id}
            type="button"
            role="radio"
            aria-checked={filter === x.id}
            onClick={() => setFilter(x.id)}
            className={cx("h-8 shrink-0 rounded-md px-3 text-sm", filter === x.id ? "bg-selected font-medium text-ink" : "text-ink-3 hover:text-ink")}
          >
            {x.label}
          </button>
        ))}
      </div>
      {events === null ? (
        <SkeletonRows rows={10} />
      ) : visible.length === 0 ? (
        <EmptyState
          title={events.length ? "Nothing matches this filter" : "Quiet for now"}
          icon="activity"
          action={
            events.length ? (
              <Button size="sm" onClick={() => { setFilter("all"); setAgentsOnly(false); }}>
                Show everything
              </Button>
            ) : (
              <Link href={repoHref(linkRepo, "agents")} className="link text-base">
                See who can work here
              </Link>
            )
          }
        >
          {events.length ? "Try Everything, or turn off Agents only." : "What agents and people do shows up here the moment it happens. No polling."}
        </EmptyState>
      ) : (
        <div data-testid="event-list" aria-live="polite" aria-relevant="additions" className="flex flex-col gap-10">
          {days.map((day) => (
            <section key={day.key} aria-labelledby={"day-" + day.key}>
              <h3 id={"day-" + day.key} className="mb-4 text-base font-semibold text-ink">
                {day.label}
              </h3>
              <div className="flex flex-col gap-6">
                {day.groups.map((g) => (
                  <div key={g.key}>
                    {g.changeId ? (
                      <p className="mb-1 flex items-center gap-2 text-base text-ink-3">
                        <Icon name="changes" size={16} />
                        <Link href={repoHref(linkRepo, "changes/" + g.changeId)} className="font-medium text-ink hover:underline">
                          {titles.get(g.changeId) ?? "A pull request"}
                        </Link>
                      </p>
                    ) : null}
                    <ol className={cx("flex flex-col", g.changeId && "ml-2.5 border-l border-line pl-4")}>
                      {g.events.map((e) => (
                        <EventRow key={e.seq} e={e} who={people.get(e.principal)} titles={titles} repo={linkRepo} grouped={!!g.changeId} />
                      ))}
                    </ol>
                  </div>
                ))}
              </div>
            </section>
          ))}
        </div>
      )}
    </section>
  );
}

type Group = { key: string; changeId: string | null; events: Event[] };
type Day = { key: string; label: string; groups: Group[] };

/** The change request an event is about, when it is about one. */
function changeOf(e: Event): string | null {
  if (e.change && (e.kind.startsWith("cr.") || e.kind.startsWith("merge_queue."))) return e.change.id;
  if (e.review) return e.review.changeId;
  return null;
}

function dayOf(iso: string | undefined, now = new Date()): { key: string; label: string } {
  if (!iso || Number.isNaN(Date.parse(iso))) return { key: "earlier", label: "Earlier" };
  const d = new Date(iso);
  const key = d.getFullYear() + "-" + (d.getMonth() + 1) + "-" + d.getDate();
  const days = Math.round((new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime() - new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime()) / 86_400_000);
  if (days === 0) return { key, label: "Today" };
  if (days === 1) return { key, label: "Yesterday" };
  return { key, label: d.toLocaleDateString("en-US", { weekday: "long", month: "long", day: "numeric", year: d.getFullYear() === now.getFullYear() ? undefined : "numeric" }) };
}

/** Newest first: days, then change requests (and a loose group per day) by their newest event. */
function groupByDay(events: Event[]): Day[] {
  const days: Day[] = [];
  for (const e of events) {
    const { key, label } = dayOf(e.time);
    let day = days.find((d) => d.key === key);
    if (!day) days.push((day = { key, label, groups: [] }));
    const changeId = changeOf(e);
    const gk = changeId ?? "loose";
    let g = day.groups.find((x) => x.key === gk);
    if (!g) day.groups.push((g = { key: gk, changeId, events: [] }));
    g.events.push(e);
  }
  return days;
}

function crLink(id: string, titles: Map<string, string>, repo: string, grouped: boolean) {
  if (grouped) return <>it</>;
  return (
    <Link href={repoHref(repo, "changes/" + id)} className="font-medium text-ink hover:underline">
      {titles.get(id) ?? "a pull request"}
    </Link>
  );
}

function targetHref(target: string, repo: string): string | null {
  const [kind, id] = target.split(":");
  if (!id) return null;
  if (kind === "issue") return repoHref(repo, "work/" + id);
  if (kind === "run") return repoHref(repo, "runs/" + id);
  if (kind === "release") return repoHref(repo, "releases#" + id);
  if (kind === "cr") return repoHref(repo, "changes/" + id);
  if (kind === "alert") return repoHref(repo, "security");
  if (kind === "package") return repoHref(repo, "packages");
  if (kind === "pages") return repoHref(repo, "pages");
  return null;
}

/** One event as the rest of a sentence: the actor's name comes first. */
function describe(e: Event, titles: Map<string, string>, repo: string, grouped: boolean): { text: React.ReactNode; extra?: React.ReactNode } {
  const k = e.kind;
  if (e.change && (k.startsWith("cr.") || k.startsWith("merge_queue."))) {
    const rev = e.change.revisions.at(-1);
    const obj = crLink(e.change.id, titles, repo, grouped);
    const text: Record<string, React.ReactNode> = {
      "cr.opened": <>opened {obj}</>,
      "cr.revised": <>pushed a new version of {obj} (r{rev?.n ?? ""})</>,
      "cr.landed": <>merged {obj}</>,
      "cr.brief_set": <>wrote the brief for {obj}</>,
      "cr.closed": <>closed {obj}</>,
      "cr.reopened": <>reopened {obj}</>,
      "cr.enqueued": <>queued {obj} to merge</>,
      "cr.dequeued": <>took {obj} out of the merge queue</>,
      "merge_queue.entered": <>queued {obj} to merge</>,
      "merge_queue.updated": <>moved {obj} in the merge queue</>,
      "merge_queue.left": <>took {obj} out of the merge queue</>,
    };
    return { text: text[k] ?? <>{k.replace(/^[a-z_]+\./, "").replace(/_/g, " ")} {obj}</>, extra: rev?.diffClass ? <DiffClassBadge cls={rev.diffClass} compact /> : undefined };
  }
  if (e.review) {
    const verb = e.review.verdict === "VERDICT_APPROVE" ? "approved" : e.review.verdict === "VERDICT_REQUEST_CHANGES" ? "asked for changes on" : "commented on";
    return { text: <>{verb} version r{e.review.revision} of {crLink(e.review.changeId, titles, repo, grouped)}</> };
  }
  if (e.check) {
    return {
      text: (
        <>
          saw check {e.check.name} {e.check.status.replace("CHECK_STATUS_", "").toLowerCase()} on <span className="mono">{objectIdText(e.check.changeset, true)}</span>
        </>
      ),
      extra: <StatusIcon status={e.check.status} />,
    };
  }
  if (e.lease) {
    const paths = e.lease.paths.map(pathText).join(", ");
    const mode = e.lease.mode === "LEASE_MODE_EXCLUSIVE" ? "an exclusive" : "an advisory";
    return {
      text: k === "lease.released" ? <>let go of <span className="mono text-ink">{paths}</span></> : <>claimed <span className="mono text-ink">{paths}</span> with {mode} lease</>,
      extra: e.lease.note && k !== "lease.released" ? <span className="text-ink-3">“{e.lease.note}”</span> : undefined,
    };
  }
  if (e.refUpdated) return { text: <>moved <span className="mono text-ink">{e.refUpdated.name}</span> to <span className="mono">{objectIdText(e.refUpdated.target, true)}</span></> };
  if (e.revokedToken) return { text: <>revoked token {e.revokedToken}</> };
  if (e.policyEpoch) return { text: <>changed the repository policy</> };
  if (e.note) {
    const href = targetHref(e.note.target, repo);
    const verb = e.note.verb ?? k.replace(/^[a-z_]+\./, "").replace(/_/g, " ");
    return { text: <>{verb} {href ? <Link href={href} className="font-medium text-ink hover:underline">{e.note.title}</Link> : <span className="font-medium text-ink">{e.note.title}</span>}</> };
  }
  return { text: k };
}

function EventRow({ e, who, titles, repo, grouped }: { e: Event; who: Principal | undefined; titles: Map<string, string>; repo: string; grouped: boolean }) {
  const d = describe(e, titles, repo, grouped);
  const isCi = CHECKS.test(e.kind);
  const actor: Principal = who ?? { id: e.principal, kind: "PRINCIPAL_KIND_SERVICE", display: isCi ? "CI" : /^[0-9A-HJKMNP-TV-Z]{26}$/.test(e.principal) ? "someone" : e.principal, ownerId: "", runtime: "", externalIds: [] };
  return (
    <li className="flex items-start gap-3 py-2" data-key={e.seq} data-kind={e.kind}>
      <PrincipalMark principal={actor} size={28} />
      <div className="min-w-0 flex-1">
        <p className="text-md leading-7 text-ink-2">
          <span className="font-medium text-ink">{actor.display}</span> {d.text}
        </p>
        {d.extra ? <div className="flex items-center gap-2 text-sm">{d.extra}</div> : null}
      </div>
      <span className="shrink-0 pt-1 text-sm text-ink-3" title={"Event " + e.seq + ": resume from here with from_seq"}>
        <Time iso={e.time} />
      </span>
    </li>
  );
}
