import type { BeeClient } from "@/lib/api/client";
import { objectIdText } from "@/lib/api/ids";
import { BeeError } from "@/lib/api/transport";
import type { Repo } from "@/lib/api/types";

export interface ForkStatus {
  fullName: string;
  behind?: boolean;
}

/** The upstream tip must be reachable from the fork tip. A fork can be ahead and still be up to date. */
export async function forkStatus(api: BeeClient, repo: Repo): Promise<ForkStatus | undefined> {
  if (!repo.forkParentId) return;
  const upstreamRef = { id: repo.forkParentId };
  const upstream = await api.repo.getRepo({ repo: upstreamRef }).catch(() => undefined);
  if (!upstream) return;

  const status: ForkStatus = { fullName: upstream.fullName };
  const forkRef = { fullName: repo.fullName };
  const upstreamTip = await api.object.resolveRef({ repo: upstreamRef, name: upstream.defaultRef }).then((resolved) => resolved.ref?.target, () => undefined);
  if (!upstreamTip) return status;
  try {
    const forkTip = await api.object.resolveRef({ repo: forkRef, name: repo.defaultRef }).then(
      (resolved) => resolved.ref?.target,
      (err: unknown) => {
        if (err instanceof BeeError && (err.reason === "REF_NOT_FOUND" || err.reason === "NOT_FOUND")) return undefined;
        throw err;
      },
    );
    if (!forkTip) return { ...status, behind: true };
    if (forkTip.raw === upstreamTip.raw) return { ...status, behind: false };
    const { isAncestor } = await api.query.isAncestor({
      repo: forkRef,
      ancestor: { expr: objectIdText(upstreamTip) },
      descendant: { expr: objectIdText(forkTip) },
    });
    return { ...status, behind: !isAncestor };
  } catch (err) {
    // The fork store may not have upstream's newest object yet.
    if (err instanceof BeeError && ["MISSING_OBJECT", "UNKNOWN_REVISION", "NOT_FOUND"].includes(err.reason)) return { ...status, behind: true };
    return status;
  }
}
