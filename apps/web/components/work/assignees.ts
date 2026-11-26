import type { BeeClient } from "@/lib/api/client";
import { BeeError } from "@/lib/api/transport";
import type { Principal, RepoRef } from "@/lib/api/types";
import { uniqueById } from "@/lib/format";

/** A signed-in reader of a public repo can lack access to the organisation directory. */
async function visible<T>(read: Promise<T>, fallback: T): Promise<T> {
  try {
    return await read;
  } catch (err) {
    if (err instanceof BeeError && ["permission_denied", "unauthenticated", "not_found"].includes(err.code)) return fallback;
    throw err;
  }
}

export async function issueAssignees(api: BeeClient, org: string, repo: RepoRef, signedIn: boolean, includeAgents: boolean) {
  if (!signedIn) return { members: [], people: [] };
  const [{ members }, { agents }] = await Promise.all([
    visible(api.org.listMembers({ org }), { members: [] }),
    includeAgents ? visible(api.agent.listAgents({ org, repo }), { agents: [] }) : { agents: [] },
  ]);
  const principals = members.map((m) => m.principal).filter((p): p is Principal => !!p);
  return { members: principals, people: uniqueById([...agents.map((a) => a.principal), ...principals]) };
}
