import type { BeeClient } from "@/lib/api/client";
import { BeeError } from "@/lib/api/transport";
import type { RepoRef, SafetyRecord } from "@/lib/api/types";
import { recentPeriod } from "./model";

const STOP = "safety.stopped";
const RESUME = "safety.resumed";
const STOP_ACTIONS = [STOP, RESUME];

export function unavailableToViewer(error: unknown): boolean {
  return error instanceof BeeError && (["unauthenticated", "permission_denied", "not_found", "unimplemented"].includes(error.code) || error.reason === "POLICY_DENIED");
}

export async function safetyRecords(api: BeeClient, repo: RepoRef, start?: string) {
  const records: SafetyRecord[] = [];
  let cursor = start;
  // The live service caps each response at seven records. Group one bounded batch per screen.
  for (let i = 0; i < 4; i++) {
    const result = await api.safety.listSafetyRecords({ repo, page: { pageSize: 7, pageToken: cursor } });
    records.push(...result.records);
    cursor = result.page?.nextPageToken;
    if (!cursor) break;
  }
  return { records, cursor };
}

export async function safetyAttention(api: BeeClient, repo: RepoRef) {
  const items = [];
  let cursor: string | undefined;
  do {
    const response = await api.safety.listSafetyAttention({ repo, page: { pageSize: 100, pageToken: cursor } });
    items.push(...response.items);
    cursor = response.page?.nextPageToken;
  } while (cursor);
  return items;
}

/** The service has no freeze read. The latest successful org audit entry records each transition. */
export async function auditFreeze(api: BeeClient, org: string, orgId: string): Promise<boolean | undefined> {
  const read = async (action: string) => {
    let pageToken: string | undefined;
    do {
      const page = await api.audit.listAudit({ org, action, allowedOnly: true, page: { pageSize: 50, pageToken } });
      const entry = page.entries.find((e) => e.action === action && [org, orgId, "org " + orgId].includes(e.target));
      if (entry) return BigInt(entry.seq);
      pageToken = page.page?.nextPageToken;
    } while (pageToken);
    return -1n;
  };
  const [stopped, resumed] = await Promise.all([read(STOP), read(RESUME)]);
  return stopped === -1n && resumed === -1n ? undefined : stopped > resumed;
}

export async function repoFreeze(api: BeeClient, org: string, orgId: string, repo: RepoRef): Promise<boolean | undefined> {
  try {
    const result = await auditFreeze(api, org, orgId);
    if (result !== undefined) return result;
  } catch (error) {
    if (!(error instanceof BeeError)) throw error;
  }
  return eventFreeze(api, orgId, repo);
}

/** Repo readers can fall back to the last retained transition when the audit log is private. */
export async function eventFreeze(api: BeeClient, orgId: string, repo: RepoRef): Promise<boolean | undefined> {
  let latest: string | undefined;
  let fromSeq = "0";
  try {
    for (;;) {
      const result = await api.event.listEvents({ repo, kinds: STOP_ACTIONS, fromSeq, limit: 100 });
      const last = result.events.filter((event) => event.subject?.id === orgId).at(-1);
      if (last) latest = last.kind;
      if (!result.events.length || result.nextSeq === fromSeq) break;
      fromSeq = result.nextSeq;
    }
  } catch (error) {
    if (!(error instanceof BeeError)) throw error;
  }
  return latest === undefined ? undefined : latest === STOP;
}

/** Carry the period in sample links so refreshes preserve the cohort and the server’s ranking. */
export function samplePeriod(end: string) {
  const time = Number(end);
  return /^\d{13}$/.test(end) && Number.isFinite(time) && time <= Date.now() ? recentPeriod(time) : null;
}
