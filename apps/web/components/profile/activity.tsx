/** Recent lands in a profile's repositories, newest first. */
import Link from "next/link";
import { Panel } from "@/components/ui";
import { LandLine } from "@/components/repo/land";
import type { Changeset } from "@/lib/api/types";
import { repoHref } from "@/lib/routes";

export interface ProfileLand {
  repo: string;
  changeset: Changeset;
}

const time = (c: Changeset) => Date.parse(c.meta?.committerTime?.time ?? c.meta?.authorTime?.time ?? "") || 0;

/** Newest first, ties broken by repo name so the order is the same on every load. */
export function newestLands(lands: ProfileLand[], limit: number): ProfileLand[] {
  return [...lands].sort((a, b) => time(b.changeset) - time(a.changeset) || a.repo.localeCompare(b.repo)).slice(0, limit);
}

export function RecentActivity({ lands, title = "Recent activity" }: { lands: ProfileLand[]; title?: string }) {
  return (
    <Panel title={title} id="activity-heading">
      {lands.length ? (
        <ul className="rule divide-y divide-line">
          {lands.map((l, i) => (
            <li key={l.repo + (l.changeset.id?.raw ?? i)} className="flex flex-col gap-1 py-3.5">
              <Link href={repoHref(l.repo)} className="truncate text-md font-medium hover:underline">
                {l.repo}
              </Link>
              <LandLine changeset={l.changeset} />
            </li>
          ))}
        </ul>
      ) : (
        <p className="text-base text-ink-3">No public activity yet.</p>
      )}
    </Panel>
  );
}
