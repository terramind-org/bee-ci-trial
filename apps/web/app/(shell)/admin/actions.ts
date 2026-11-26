"use server";

/**
 * The admin console's writes. Each one is a UI method on the session's client, so it reaches the
 * server exactly as an agent's call would; the answer is a result, never a throw, so the form can
 * show the server's SPEC §12.2 reason. Reads that depend on interaction (re-running the readiness
 * checks, testing a SAML provider, checking a domain) go through the same door.
 */
import { revalidatePath } from "next/cache";
import { serverClient } from "@/lib/api/server";
import type { Req } from "@/lib/api/methods";
import { BeeError } from "@/lib/api/transport";
import type { BeeClient } from "@/lib/api/client";

const WRITES = {
  validateOpsConfig: (c: BeeClient, r: Req<"OpsService", "ValidateOpsConfig">) => c.ops.validateOpsConfig(r),
  updateEnterprise: (c: BeeClient, r: Req<"EnterpriseService", "UpdateEnterprise">) => c.enterprise.updateEnterprise(r),
  updateSamlConfig: (c: BeeClient, r: Req<"EnterpriseService", "UpdateSamlConfig">) => c.enterprise.updateSamlConfig(r),
  testSamlConfig: (c: BeeClient, r: Req<"EnterpriseService", "TestSamlConfig">) => c.enterprise.testSamlConfig(r),
  updateScimConfig: (c: BeeClient, r: Req<"EnterpriseService", "UpdateScimConfig">) => c.enterprise.updateScimConfig(r),
  createScimToken: (c: BeeClient, r: Req<"EnterpriseService", "CreateScimToken">) => c.enterprise.createScimToken(r),
  addVerifiedDomain: (c: BeeClient, r: Req<"EnterpriseService", "AddVerifiedDomain">) => c.enterprise.addVerifiedDomain(r),
  checkVerifiedDomain: (c: BeeClient, r: Req<"EnterpriseService", "CheckVerifiedDomain">) => c.enterprise.checkVerifiedDomain(r),
  deleteVerifiedDomain: (c: BeeClient, r: Req<"EnterpriseService", "DeleteVerifiedDomain">) => c.enterprise.deleteVerifiedDomain(r),
  updateTwoFactorRequirement: (c: BeeClient, r: Req<"EnterpriseService", "UpdateTwoFactorRequirement">) => c.enterprise.updateTwoFactorRequirement(r),
  createIpAllowListEntry: (c: BeeClient, r: Req<"EnterpriseService", "CreateIpAllowListEntry">) => c.enterprise.createIpAllowListEntry(r),
  deleteIpAllowListEntry: (c: BeeClient, r: Req<"EnterpriseService", "DeleteIpAllowListEntry">) => c.enterprise.deleteIpAllowListEntry(r),
  updateEnterprisePolicies: (c: BeeClient, r: Req<"EnterpriseService", "UpdateEnterprisePolicies">) => c.enterprise.updateEnterprisePolicies(r),
  changePlan: (c: BeeClient, r: Req<"BillingService", "ChangePlan">) => c.billing.changePlan(r),
} as const;

type Writes = typeof WRITES;
export type AdminWrite = keyof Writes;
export type AdminResult<T> = { ok: true; data: T } | { ok: false; reason: string; message: string; unimplemented: boolean };

/** Runs one admin write for the signed-in caller; `revalidate` is the admin path to refresh. */
export async function adminWrite<K extends AdminWrite>(name: K, request: Parameters<Writes[K]>[1], revalidate?: string): Promise<AdminResult<Awaited<ReturnType<Writes[K]>>>> {
  try {
    const call = WRITES[name] as (c: BeeClient, r: Parameters<Writes[K]>[1]) => Promise<Awaited<ReturnType<Writes[K]>>>;
    const data = await call(await serverClient(), request);
    if (revalidate) revalidatePath(revalidate);
    return { ok: true, data };
  } catch (err) {
    if (err instanceof BeeError) return { ok: false, reason: err.reason, message: err.message, unimplemented: err.code === "unimplemented" };
    return { ok: false, reason: "INTERNAL", message: err instanceof Error ? err.message : String(err), unimplemented: false };
  }
}
