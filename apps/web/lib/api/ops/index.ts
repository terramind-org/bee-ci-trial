/**
 * The mapping layer: every UI method in `methods.ts`, implemented over the generated bee.v1
 * descriptors. Both adapters run the same mapping; only the Connect transport underneath
 * differs, so they return identical shapes (`lib/api/contract.test.ts`).
 */
import type { MethodName, ServiceName } from "../methods";
import { adminOps } from "./admin";
import { agentOps } from "./agents";
import { briefOps } from "./briefs";
import { cloudOps } from "./cloud";
import { communityOps } from "./community";
import { forgeOps } from "./forge";
import { governanceOps } from "./governance";
import { hubOps } from "./hub";
import { postOps } from "./posts";
import { safetyOps } from "./safety";
import { swarmOps } from "./swarms";
import type { OpOf, Ops, Wire } from "./shared";
import { vcsOps } from "./vcs";
import { workOps } from "./work";

type Intersect<U> = (U extends unknown ? (k: U) => void : never) extends (k: infer I) => void ? I : never;

/** Merges per-service mapping tables; the intersection type is what proves every UI method has a mapping. */
function merge<P extends object[]>(...parts: P): Intersect<P[number]> {
  const out: Record<string, Record<string, unknown>> = {};
  for (const part of parts) for (const [service, methods] of Object.entries(part)) out[service] = { ...out[service], ...(methods as Record<string, unknown>) };
  return out as Intersect<P[number]>;
}

export const OPS: Ops = merge(forgeOps, vcsOps, agentOps, workOps, governanceOps, hubOps, communityOps, postOps, swarmOps, safetyOps, briefOps, cloudOps, adminOps);

export function opOf<S extends ServiceName, M extends MethodName<S>>(service: S, method: M): OpOf<S, M> {
  const op = (OPS[service] as Record<string, unknown> | undefined)?.[method];
  if (!op) throw new Error("no mapping for " + service + "/" + method);
  return op as OpOf<S, M>;
}

/** The bee.v1 call behind a UI operation, for Copy as. Unknown operations pass through. */
export function wireOf(service: string, method: string, request: unknown): Wire {
  const op = (OPS as unknown as Record<string, Record<string, { wire(r: unknown): Wire } | undefined> | undefined>)[service]?.[method];
  return op ? op.wire(request) : { service, method, streaming: false, request };
}

export type { Wire } from "./shared";
