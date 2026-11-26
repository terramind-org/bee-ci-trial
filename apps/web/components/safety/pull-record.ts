import type { BeeClient } from "@/lib/api/client";
import { BeeError } from "@/lib/api/transport";
import type { ChangeRequest, SafetyRef } from "@/lib/api/types";
import { RUN, SWARM, safetyHref, verdictOf } from "./model";

export interface PullSafetyLink {
  href: string;
  verdict: string;
}

/** Use the run/swarm's recorded outcome link, never infer a match from its title or author. */
export async function pullSafetyLink(api: BeeClient, fullName: string, change: Pick<ChangeRequest, "id" | "number">): Promise<PullSafetyLink | undefined> {
  const repo = { fullName };
  let pageToken: string | undefined;
  try {
    do {
      const { items, page } = await api.swarm.listRepoWork({ repo, page: { pageSize: 50, pageToken } });
      for (const item of items) {
        if (!item.changeRequestIds.includes(change.id) && !item.run?.changeRequests?.includes(change.number)) continue;
        const work: SafetyRef | undefined = item.swarm?.id ? { repo, kind: SWARM, id: item.swarm.id }
          : item.run?.id ? { repo, kind: RUN, id: item.run.id } : undefined;
        if (!work) continue;
        const record = await api.safety.getSafetyRecord(work);
        const verdict = verdictOf(record.verdict);
        if (verdict) return { href: safetyHref(fullName, record.work ?? work), verdict: verdict[1] };
      }
      pageToken = page?.nextPageToken;
    } while (pageToken);
  } catch (error) {
    // Safety uses aggregate read access. A failure to read it leaves the pull request readable.
    if (!(error instanceof BeeError)) throw error;
  }
  return undefined;
}
