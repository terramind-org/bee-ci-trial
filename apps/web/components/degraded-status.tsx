import { Suspense } from "react";
import { serverClient } from "@/lib/api/server";
import { isOutageError } from "@/lib/infra";
import { DegradedBanner } from "./degraded-banner";

/**
 * Asks the server whether the repo's default ref is being served stale (`ResolveRefResponse.stale`)
 * and, if so, how old. Only operators can read the age (`OpsCell.staleLagMs`); everyone else gets
 * the banner without a number. Never blocks or breaks the page: it streams in after the content.
 */
export function DegradedStatus({ fullName, defaultRef }: { fullName: string; defaultRef: string }) {
  return (
    <Suspense fallback={null}>
      <Probe fullName={fullName} defaultRef={defaultRef} />
    </Suspense>
  );
}

async function Probe({ fullName, defaultRef }: { fullName: string; defaultRef: string }) {
  const api = await serverClient();
  let degraded = false;
  try {
    degraded = (await api.object.resolveRef({ repo: { fullName }, name: defaultRef || "main" })).stale;
  } catch (e) {
    degraded = isOutageError(e);
  }
  let ageSeconds: number | null = null;
  if (degraded) {
    try {
      const { cells } = await api.ops.getClusterStatus({});
      const lags = cells.filter((c) => c.servingStaleReads).map((c) => Number(c.staleLagMs));
      if (lags.length) ageSeconds = Math.round(Math.max(...lags) / 1000);
    } catch {
      ageSeconds = null;
    }
  }
  return <DegradedBanner degraded={degraded} ageSeconds={ageSeconds} />;
}
