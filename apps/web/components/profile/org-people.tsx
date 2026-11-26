/** Members of an organization and its most-used topics. */
import Link from "next/link";
import { PrincipalMark } from "@/components/domain";
import { Panel } from "@/components/ui";
import type { Member, SearchRepoHit } from "@/lib/api/types";
import { ownerHref } from "@/lib/routes";

/** Topics ranked by how many of the repos carry them, then by name. */
export function topTopics(hits: SearchRepoHit[], limit = 10): { name: string; count: number }[] {
  const counts = new Map<string, number>();
  for (const h of hits) for (const t of h.topics) counts.set(t, (counts.get(t) ?? 0) + 1);
  return [...counts.entries()].map(([name, count]) => ({ name, count })).sort((a, b) => b.count - a.count || a.name.localeCompare(b.name)).slice(0, limit);
}

export function TopTopics({ topics }: { topics: { name: string; count: number }[] }) {
  if (!topics.length) return null;
  return (
    <Panel title="Top topics" id="topics-heading">
      <ul className="flex flex-wrap gap-x-5 gap-y-1">
        {topics.map((t) => (
          <li key={t.name}>
            <Link href={"/topics/" + encodeURIComponent(t.name)} className="inline-flex h-8 items-center gap-1.5 text-base text-accent-ink hover:underline">
              {t.name}
              <span className="num text-sm text-ink-3">{t.count}</span>
            </Link>
          </li>
        ))}
      </ul>
    </Panel>
  );
}

/** `members` is null when the server does not show members to this viewer. */
export function People({ members }: { members: Member[] | null }) {
  return (
    <Panel title="People" id="people-heading">
      {members === null ? (
        <p className="text-base text-ink-3">Members are shown to signed-in visitors.</p>
      ) : members.length === 0 ? (
        <p className="text-base text-ink-3">No members to show.</p>
      ) : (
        <ul>
          {members.map((m) =>
            m.principal ? (
              <li key={m.principal.id}>
                <Link href={ownerHref(m.principal.display)} className="flex items-center gap-3 py-2 hover:underline focus-visible:underline">
                  <PrincipalMark principal={m.principal} size={32} />
                  <span className="min-w-0 flex-1 truncate text-md font-medium">{m.principal.display}</span>
                  <span className="shrink-0 text-sm text-ink-3">{m.role.replace("ROLE_", "").toLowerCase()}</span>
                </Link>
              </li>
            ) : null,
          )}
        </ul>
      )}
    </Panel>
  );
}
