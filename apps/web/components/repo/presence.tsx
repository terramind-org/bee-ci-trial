/** Show who is editing, and link each mention to its work on the Agents tab. */
import Link from "next/link";
import { Avatar, toAvatar } from "@/components/avatar";
import { Popover } from "@/components/kit";
import { finishedRun, workAnchor } from "@/components/agents/work";
import type { Lease, Principal, RepoWorkItem } from "@/lib/api/types";
import { pathText } from "@/lib/api/ids";
import { repoHref } from "@/lib/routes";

/** Whether a lease on `leased` covers `path` or something below it (both repo-relative). */
export function leaseTouches(leased: string, path: string): boolean {
  const a = leased.replace(/\/+$/, "");
  return a === path || a.startsWith(path + "/") || path.startsWith(a + "/");
}

/** Joins names as prose: "a", "a and b", "a, b and c". */
export function listing(names: string[]): string {
  if (names.length <= 1) return names[0] ?? "";
  return names.slice(0, -1).join(", ") + " and " + names[names.length - 1];
}

export function editingSentence(leases: Lease[], under?: string): string {
  const parts = leases.map((l) => {
    const paths = l.paths.map(pathText).filter((p) => under === undefined || leaseTouches(p, under));
    const note = l.note ? " (" + l.note + ")" : "";
    return (l.holder?.display ?? "Someone") + " is editing " + (listing(paths) || "the whole repository") + note;
  });
  return listing(parts) + ".";
}

/** The distinct holders of a set of leases, in the order they first appear. */
export function holders(leases: Lease[]): Principal[] {
  const seen = new Map<string, Principal>();
  for (const l of leases) if (l.holder && !seen.has(l.holder.id)) seen.set(l.holder.id, l.holder);
  return [...seen.values()];
}

export function activityHref(repo: string, work: RepoWorkItem[], principal?: Principal, lease?: Lease): string {
  const tab = repoHref(repo, "agents");
  if (!principal || principal.kind !== "PRINCIPAL_KIND_AGENT") return tab;
  const eligible = work.filter(({ swarm, run }) =>
    (swarm && ["SWARM_STATE_PLANNING", "SWARM_STATE_WORKING", "SWARM_STATE_PAUSED"].includes(swarm.state)) ||
    (run && !finishedRun(run)),
  );
  const exact = eligible.filter(({ swarm, run }) => {
    if (!lease) return false;
    if (run?.leaseIds?.includes(lease.id)) return true;
    return swarm?.tasks.some((task) =>
      task.state === "SWARM_TASK_STATE_WORKING" &&
      task.attempts.some((attempt) => attempt.runId === task.runId && attempt.agent?.id === principal.id) &&
      task.paths.some((path) => lease.paths.some((leased) => leaseTouches(pathText(leased), pathText(path)))),
    );
  });
  const matches = exact.length ? exact : eligible.filter(({ swarm, run }) =>
    run?.agent?.id === principal.id || swarm?.lead?.id === principal.id ||
    swarm?.tasks.some((task) => task.attempts.some((attempt) => attempt.runId === task.runId && attempt.agent?.id === principal.id)),
  );
  const item = matches.length === 1 ? matches[0] : undefined;
  // Claims from external editors do not necessarily belong to a hosted run.
  return item ? tab + "#" + workAnchor(item) : repoHref(repo, "agents/" + encodeURIComponent(principal.id));
}

export function Presence({ repo, work = [], leases, under, size = 28 }: { repo: string; work?: RepoWorkItem[]; leases: Lease[]; under?: string; size?: number }) {
  const people = holders(leases);
  if (!people.length) return null;
  return (
    <Popover
      title="Editing now"
      triggerClass="inline-flex min-h-6 items-center rounded-full focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-(--focus)"
      className="max-w-72 whitespace-normal"
      trigger={
        <>
          <span className="flex -space-x-2" aria-hidden="true">
            {people.slice(0, 4).map((p) => (
              <span key={p.id} className="rounded-full ring-2 ring-bg">
                <Avatar principal={toAvatar(p)} size={size} />
              </span>
            ))}
          </span>
          <span className="sr-only">{editingSentence(leases, under)}</span>
        </>
      }
    >
      <ul className="space-y-2 text-sm">
        {leases.map((lease) => (
          <li key={lease.id}>
            <Link className="link inline-flex min-h-6 items-center [overflow-wrap:anywhere]" href={activityHref(repo, work, lease.holder, lease)}>{editingSentence([lease], under)}</Link>
          </li>
        ))}
      </ul>
    </Popover>
  );
}
