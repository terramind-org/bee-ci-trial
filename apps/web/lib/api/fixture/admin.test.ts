import { describe, expect, it } from "vitest";
import { createClient } from "./../client";
import { FixtureStore, fixtureRpc } from "./index";
import { BeeError } from "../transport";

const ANCHOR = Date.UTC(2026, 9, 18, 9, 0, 0);

function setup() {
  const store = new FixtureStore({ anchor: ANCHOR, clock: () => 0, live: false });
  return { store, api: createClient(fixtureRpc(store, "mara-token-0001")), anon: createClient(fixtureRpc(store)) };
}

async function fails(p: Promise<unknown>): Promise<BeeError> {
  try {
    await p;
  } catch (e) {
    expect(e).toBeInstanceOf(BeeError);
    return e as BeeError;
  }
  throw new Error("expected the call to fail");
}

const ent = { enterprise: "acme-group" };
const CERT = "-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIUQ0ZpZXh0dXJl\n-----END CERTIFICATE-----";
const METADATA = `<?xml version="1.0"?>
<md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata" entityID="https://idp.acme.example/saml">
  <md:IDPSSODescriptor>
    <md:KeyDescriptor><ds:KeyInfo xmlns:ds="http://www.w3.org/2000/09/xmldsig#"><ds:X509Data><ds:X509Certificate>MIIB</ds:X509Certificate></ds:X509Data></ds:KeyInfo></md:KeyDescriptor>
    <md:SingleSignOnService Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect" Location="https://idp.acme.example/sso"/>
  </md:IDPSSODescriptor>
</md:EntityDescriptor>`;

describe("billing", () => {
  it("lists free, team and enterprise plans", async () => {
    const { api } = setup();
    const { plans = [] } = await api.billing.listBillingPlans({});
    expect(plans.map((p) => p.id)).toEqual(["free", "team", "enterprise"]);
    expect(plans.map((p) => p.selfServe)).toEqual([true, true, false]);
    expect(plans[1]?.seatMonthlyPriceMicroUsd).toBe("4000000");
    expect(plans[1]?.seatYearlyPriceMicroUsd).toBe("40000000");
    expect(plans[2]?.seatMonthlyPriceMicroUsd).toBe("0");
    expect(plans[1]?.features?.some((f) => /student|nonprofit/i.test(f))).toBe(true);
    expect(plans.every((p) => (p.included ?? []).length === 4)).toBe(true);
  });

  it("reports acme on team and lattice on free", async () => {
    const { api } = setup();
    const acme = await api.billing.getAccountPlan({ org: "acme" });
    expect(acme.plan?.id).toBe("team");
    expect(acme.cycle).toBe("BILLING_CYCLE_MONTHLY");
    expect(acme.seatsPurchased).toBe(12);
    expect(acme.seatsConsumed).toBe(9);
    expect(Date.parse(acme.nextBillingAt ?? "")).toBeGreaterThan(ANCHOR);
    expect(Date.parse(acme.cycleStartedAt ?? "")).toBeLessThan(ANCHOR);
    const lattice = await api.billing.getAccountPlan({ org: "lattice" });
    expect(lattice.plan?.id).toBe("free");
    expect(lattice.seatsConsumed).toBe(5);
    expect((await fails(api.billing.getAccountPlan({ org: "nope" }))).reason).toBe("ORG_NOT_FOUND");
  });

  it("refuses bad plan changes and applies good ones", async () => {
    const { api, store } = setup();
    expect((await fails(api.billing.changePlan({ org: "lattice", planId: "team" }))).code).toBe("permission_denied");
    expect(store.audit.at(-1)?.allowed).toBe(false);
    expect((await fails(api.billing.changePlan({ org: "acme", planId: "gold" }))).reason).toBe("PLAN_NOT_FOUND");
    const ec = await fails(api.billing.changePlan({ org: "acme", planId: "enterprise" }));
    expect([ec.code, ec.reason]).toEqual(["failed_precondition", "PLAN_NOT_SELF_SERVE"]);
    expect((await fails(api.billing.changePlan({ org: "acme", planId: "team", seats: 4 }))).reason).toBe("SEATS_BELOW_CONSUMED");
    expect((await fails(api.billing.changePlan({ org: "acme", planId: "free" }))).code).toBe("failed_precondition");

    const more = await api.billing.changePlan({ org: "acme", planId: "team", seats: 20, cycle: "BILLING_CYCLE_YEARLY" });
    expect([more.seatsPurchased, more.cycle, more.pendingSeats]).toEqual([20, "BILLING_CYCLE_YEARLY", 0]);
    const fewer = await api.billing.changePlan({ org: "acme", planId: "team", seats: 10 });
    expect([fewer.seatsPurchased, fewer.pendingSeats, fewer.pendingPlanId]).toEqual([20, 10, "team"]);
    expect(store.audit.at(-1)?.action).toBe("billing.plan.change");
    expect(store.audit.at(-1)?.allowed).toBe(true);
  });

  it("lists exactly as many seats as are consumed", async () => {
    const { api } = setup();
    const first = await api.billing.listSeats({ org: "acme", page: { pageSize: 4 } });
    expect(first.totalConsumed).toBe(9);
    expect(first.seats).toHaveLength(4);
    expect(first.seats?.[0]?.principal?.display).toBe("Mara Okafor");
    const rest = await api.billing.listSeats({ org: "acme", page: { pageSize: 50, pageToken: first.page?.nextPageToken } });
    const all = [...(first.seats ?? []), ...(rest.seats ?? [])];
    expect(all).toHaveLength(9);
    expect(all.filter((x) => x.outsideCollaborator)).toHaveLength(5);
    expect(new Set(all.map((x) => x.principal?.id)).size).toBe(9);
    expect((await api.billing.listSeats({ org: "lattice" })).seats).toHaveLength(5);
  });

  it("totals usage exactly as the sum of its lines", async () => {
    const { api } = setup();
    const sumOf = async (req: Parameters<typeof api.billing.queryUsage>[0]) => {
      let gross = 0n;
      let net = 0n;
      let token = "";
      let rows = 0;
      let totals: [string | undefined, string | undefined] = [undefined, undefined];
      do {
        const r = await api.billing.queryUsage({ ...req, page: { pageSize: 25, pageToken: token } });
        for (const i of r.lineItems ?? []) {
          rows++;
          expect(BigInt(i.grossMicroUsd ?? "0") - BigInt(i.discountMicroUsd ?? "0")).toBe(BigInt(i.netMicroUsd ?? "0"));
          expect(BigInt(i.grossMicroUsd ?? "0")).toBe(BigInt(Math.round(i.quantity as number)) * BigInt(i.unitPriceMicroUsd ?? "0"));
          gross += BigInt(i.grossMicroUsd ?? "0");
          net += BigInt(i.netMicroUsd ?? "0");
        }
        totals = [r.totalGrossMicroUsd, r.totalNetMicroUsd];
        expect(Date.parse(r.dataCompleteUntil ?? "")).toBe(ANCHOR);
        token = r.page?.nextPageToken ?? "";
      } while (token);
      expect(totals).toEqual([gross.toString(), net.toString()]);
      return rows;
    };
    expect(await sumOf({ org: "acme" })).toBe(120);
    expect(await sumOf({ org: "acme", groupBy: ["BILLING_USAGE_GROUPING_PRODUCT"] })).toBe(4);
    expect(await sumOf({ org: "acme", groupBy: ["BILLING_USAGE_GROUPING_MONTH"] })).toBe(8);
    expect(await sumOf({ org: "acme", products: ["BILLING_PRODUCT_ACTIONS_MINUTES"] })).toBe(30);
    expect(await sumOf({ org: "lattice", startAt: "2026-10-10T00:00:00Z", endAt: "2026-10-13T00:00:00Z" })).toBe(12);
    const again = await api.billing.queryUsage({ org: "acme", groupBy: ["BILLING_USAGE_GROUPING_PRODUCT"] });
    const net = (again.lineItems ?? []).reduce((n, i) => n + BigInt(i.netMicroUsd ?? "0"), 0n);
    expect(again.totalNetMicroUsd).toBe(net.toString());
    expect(net).toBeGreaterThan(0n);
  });
});

describe("ops", () => {
  it("reports health with a degraded search index", async () => {
    const { api } = setup();
    const h = await api.ops.getOpsHealth({ includeComponents: true });
    expect([h.node, h.cell, h.version, h.liveness, h.readiness]).toEqual(["node-a1", "eu-1", "0.9.0-dev", "HEALTH_STATUS_OK", "HEALTH_STATUS_DEGRADED"]);
    expect(h.components).toHaveLength(7);
    expect(h.components?.find((c) => c.name === "search_index")?.detail).toBe("indexing lag 38s behind the event log");
    expect(h.components?.filter((c) => c.criticalPath).length).toBeGreaterThan(0);
    expect((await api.ops.getOpsHealth({})).components).toEqual([]);
  });

  it("reports two cells and honours the cells filter", async () => {
    const { api } = setup();
    const c = await api.ops.getClusterStatus({});
    expect(c.cells?.map((x) => x.name)).toEqual(["eu-1", "us-1"]);
    const us = c.cells?.find((x) => x.name === "us-1");
    expect([us?.servingStaleReads, us?.staleLagMs, us?.status]).toEqual([true, "1800", "HEALTH_STATUS_DEGRADED"]);
    expect(c.directoryUp).toBe(true);
    expect((await api.ops.getClusterStatus({ cells: ["eu-1"] })).cells).toHaveLength(1);
  });

  it("validates TOML config", async () => {
    const { api } = setup();
    const ok = await api.ops.validateOpsConfig({ toml: '\n[server]\nedition = "cloud"  # hosted\n\n' });
    expect([ok.valid, ok.normalizedToml, ok.errors]).toEqual([true, '[server]\nedition = "cloud"  # hosted\n', []]);

    const header = await api.ops.validateOpsConfig({ toml: '[server\nedition = "cloud"' });
    expect(header.valid).toBe(false);
    expect(header.errors?.[0]).toMatchObject({ line: 1, code: "toml_syntax" });

    const noEq = await api.ops.validateOpsConfig({ toml: "[server]\nedition" });
    expect(noEq.errors?.[0]).toMatchObject({ line: 2, code: "toml_syntax" });

    const edition = await api.ops.validateOpsConfig({ toml: '[server]\nedition = "community"' });
    expect(edition.errors?.[0]).toMatchObject({ keyPath: "server.edition", code: "invalid_config", line: 2 });

    const unknown = await api.ops.validateOpsConfig({ toml: '[server]\nedition = "enterprise"\n[widgets]\nsize = 3' });
    expect(unknown.valid).toBe(true);
    expect(unknown.warnings?.[0]).toMatchObject({ code: "unknown_table", keyPath: "widgets", line: 3 });
  });
});

describe("enterprise", () => {
  it("serves the enterprise by slug, alias or id and rejects others", async () => {
    const { api } = setup();
    const e = await api.enterprise.getEnterprise({ enterprise: "acme-group" });
    expect([e.name, e.slug, e.orgCount, e.memberCount, e.managedUsers]).toEqual(["Acme Group", "acme-group", 2, 7, false]);
    expect((await api.enterprise.getEnterprise({ enterprise: "default" })).id).toBe(e.id);
    expect((await api.enterprise.getEnterprise({ enterprise: e.id })).slug).toBe("acme-group");
    expect((await fails(api.enterprise.getEnterprise({ enterprise: "other" }))).reason).toBe("ENTERPRISE_NOT_FOUND");

    const owners = await api.enterprise.listEnterpriseOwners({ enterprise: "acme-group" });
    expect(owners.owners?.map((o) => o.principal?.display)).toEqual(["Mara Okafor"]);
    const orgs = await api.enterprise.listEnterpriseOrgs({ enterprise: "acme-group" });
    expect(orgs.orgs?.map((o) => [o.org, o.memberCount, o.repoCount])).toEqual([
      ["acme", 4, 4],
      ["lattice", 3, 5],
    ]);
    expect((await api.enterprise.listEnterpriseOrgs({ enterprise: "acme-group", query: "latt" })).orgs).toHaveLength(1);

    const upd = await api.enterprise.updateEnterprise({ enterprise: "acme-group", name: "Acme Group Ltd", managedUsers: true, managedUsersShortcode: "acme" });
    expect([upd.name, upd.managedUsers, upd.managedUsersShortcode]).toEqual(["Acme Group Ltd", true, "acme"]);
    expect((await fails(api.enterprise.updateEnterprise({ enterprise: "acme-group", websiteUrl: "nope" }))).reason).toBe("INVALID_URL");
    expect((await fails(api.enterprise.updateEnterprise({ enterprise: "acme-group", name: " " }))).reason).toBe("NAME_REQUIRED");
  });

  it("starts SAML unconfigured and validates updates", async () => {
    const { api } = setup();
    const c = await api.enterprise.getSamlConfig({ scope: ent } as never);
    expect(c).toMatchObject({ enabled: false, enforced: false, idpEntityId: "", spEntityId: "https://hub.bee.example/saml/metadata", spAcsUrl: "https://hub.bee.example/saml/acs", sessionLifetimeSeconds: 28800 });
    expect((await fails(api.enterprise.updateSamlConfig({ scope: ent, enabled: true } as never))).reason).toBe("SAML_INCOMPLETE");
    expect((await fails(api.enterprise.updateSamlConfig({ scope: ent, enforced: true } as never))).code).toBe("invalid_argument");
    expect((await fails(api.enterprise.updateSamlConfig({ scope: ent, idpCertificatePem: "garbage" } as never))).reason).toBe("INVALID_CERTIFICATE");
    const on = await api.enterprise.updateSamlConfig({ scope: ent, enabled: true, idpEntityId: "https://idp.acme.example/saml", idpSsoUrl: "https://idp.acme.example/sso", idpCertificatePem: CERT } as never);
    expect(on.enabled).toBe(true);
    expect(on.updatedAt).toBeTruthy();
    expect(on.idpCertificateExpiresAt).toBeTruthy();
    const enforced = await api.enterprise.updateSamlConfig({ scope: ent, enforced: true } as never);
    expect([enforced.enabled, enforced.enforced]).toEqual([true, true]);
  });

  it("tests SAML metadata", async () => {
    const { api } = setup();
    const ok = await api.enterprise.testSamlConfig({ scope: ent, idpMetadataXml: METADATA } as never);
    expect(ok.metadataOk).toBe(true);
    expect(ok.testSignInUrl).toContain("https://hub.bee.example/saml/test");
    const none = await api.enterprise.testSamlConfig({ scope: ent } as never);
    expect([none.metadataOk, none.problems?.length]).toEqual([false, 1]);
    const bad = await api.enterprise.testSamlConfig({ scope: ent, idpMetadataXml: "<EntityDescriptor><IDPSSODescriptor></EntityDescriptor>" } as never);
    expect(bad.metadataOk).toBe(false);
    expect(bad.problems?.join(" ")).toMatch(/well-formed/);
    const noSso = await api.enterprise.testSamlConfig({ scope: ent, idpMetadataXml: '<EntityDescriptor entityID="x"></EntityDescriptor>' } as never);
    expect(noSso.problems).toContain("no SingleSignOnService endpoint found");
    expect((await api.enterprise.testSamlConfig({ scope: ent, idpMetadataUrl: "https://idp.acme.example/metadata" } as never)).metadataOk).toBe(true);
    expect((await api.enterprise.testSamlConfig({ scope: ent, idpMetadataUrl: "http://idp.acme.example/metadata" } as never)).metadataOk).toBe(false);
  });

  it("mints a SCIM token only once SCIM is on", async () => {
    const { api } = setup();
    const c = await api.enterprise.getScimConfig({ scope: ent } as never);
    expect([c.enabled, c.endpointUrl, c.activeTokenCount]).toEqual([false, "https://hub.bee.example/scim/v2", 0]);
    expect((await fails(api.enterprise.createScimToken({ scope: ent } as never))).reason).toBe("SCIM_DISABLED");
    expect((await api.enterprise.updateScimConfig({ scope: ent, enabled: true } as never)).enabled).toBe(true);
    const t = await api.enterprise.createScimToken({ scope: ent, description: "Okta" } as never);
    expect(t.token).toMatch(/^scim_[0-9a-f]{48}$/);
    expect(Date.parse(t.expiresAt ?? "")).toBe(ANCHOR + 90 * 86_400_000);
    const t2 = await api.enterprise.createScimToken({ scope: ent, ttlSeconds: 3600 } as never);
    expect(t2.token).not.toBe(t.token);
    expect(Date.parse(t2.expiresAt ?? "")).toBe(ANCHOR + 3_600_000);
    expect((await api.enterprise.getScimConfig({ scope: ent } as never)).activeTokenCount).toBe(2);
  });

  it("verifies domains over two checks", async () => {
    const { api } = setup();
    const list = (await api.enterprise.listVerifiedDomains({ scope: ent } as never)).domains ?? [];
    expect(list.map((d) => [d.domain, d.state])).toEqual([
      ["acme.example", "VERIFIED_DOMAIN_STATE_VERIFIED"],
      ["acme-robotics.example", "VERIFIED_DOMAIN_STATE_PENDING"],
    ]);
    expect(list[0]?.verifiedAt).toBeTruthy();
    expect(list[1]).toMatchObject({ dnsTxtRecordName: "_bee-challenge.acme-robotics.example" });
    expect(list[1]?.dnsTxtRecordValue).toBeTruthy();
    const pending = list[1]?.id ?? "";
    expect((await api.enterprise.checkVerifiedDomain({ scope: ent, domainId: list[0]?.id } as never)).state).toBe("VERIFIED_DOMAIN_STATE_VERIFIED");
    const first = await api.enterprise.checkVerifiedDomain({ scope: ent, domainId: pending } as never);
    expect([first.state, Boolean(first.lastCheckedAt), first.verifiedAt]).toEqual(["VERIFIED_DOMAIN_STATE_PENDING", true, undefined]);
    expect((await api.enterprise.checkVerifiedDomain({ scope: ent, domainId: pending } as never)).state).toBe("VERIFIED_DOMAIN_STATE_VERIFIED");
    expect((await fails(api.enterprise.checkVerifiedDomain({ scope: ent, domainId: "nope" } as never))).reason).toBe("DOMAIN_NOT_FOUND");
  });

  it("adds and deletes domains with validation", async () => {
    const { api } = setup();
    expect((await fails(api.enterprise.addVerifiedDomain({ scope: ent, domain: "not a domain" } as never))).reason).toBe("INVALID_DOMAIN");
    expect((await fails(api.enterprise.addVerifiedDomain({ scope: ent, domain: "ACME.example" } as never))).reason).toBe("DOMAIN_EXISTS");
    const d = await api.enterprise.addVerifiedDomain({ scope: ent, domain: "Lattice.example" } as never);
    expect([d.domain, d.state, d.dnsTxtRecordName]).toEqual(["lattice.example", "VERIFIED_DOMAIN_STATE_PENDING", "_bee-challenge.lattice.example"]);
    expect((await api.enterprise.addVerifiedDomain({ scope: ent, domain: "mail.example", approvedOnly: true } as never)).state).toBe("VERIFIED_DOMAIN_STATE_APPROVED");
    await api.enterprise.deleteVerifiedDomain({ scope: ent, domainId: d.id } as never);
    expect((await api.enterprise.listVerifiedDomains({ scope: ent } as never)).domains).toHaveLength(3);
    expect((await fails(api.enterprise.deleteVerifiedDomain({ scope: ent, domainId: d.id } as never))).reason).toBe("DOMAIN_NOT_FOUND");
  });

  it("manages the IP allow list", async () => {
    const { api } = setup();
    const list = (await api.enterprise.listIpAllowList({ scope: ent } as never)).entries ?? [];
    expect(list.map((e) => [e.cidr, e.description, e.active])).toEqual([["10.20.0.0/16", "Office VPN", true]]);
    for (const bad of ["10.0.0.0", "10.0.0.0/33", "300.1.1.1/8", "2001:db8::/129", "2001:::1/64", "abc/8"])
      expect((await fails(api.enterprise.createIpAllowListEntry({ scope: ent, cidr: bad } as never))).reason).toBe("INVALID_CIDR");
    expect((await fails(api.enterprise.createIpAllowListEntry({ scope: ent, cidr: "10.20.0.0/16" } as never))).code).toBe("already_exists");
    const v4 = await api.enterprise.createIpAllowListEntry({ scope: ent, cidr: "203.0.113.0/24", description: "CI", active: true } as never);
    const v6 = await api.enterprise.createIpAllowListEntry({ scope: ent, cidr: "2001:db8::/32", active: true } as never);
    expect((await api.enterprise.listIpAllowList({ scope: ent } as never)).entries).toHaveLength(3);
    await api.enterprise.deleteIpAllowListEntry({ scope: ent, entryId: v4.id } as never);
    await api.enterprise.deleteIpAllowListEntry({ scope: ent, entryId: v6.id } as never);
    expect((await fails(api.enterprise.deleteIpAllowListEntry({ scope: ent, entryId: v4.id } as never))).reason).toBe("IP_ENTRY_NOT_FOUND");
    expect((await api.enterprise.listIpAllowList({ scope: ent } as never)).entries).toHaveLength(1);
  });

  it("requires two-factor authentication and removes the noncompliant", async () => {
    const { api, store } = setup();
    const before = await api.enterprise.getTwoFactorRequirement({ scope: ent } as never);
    expect([before.required, before.noncompliantCount]).toEqual([false, 3]);
    expect((await fails(api.enterprise.updateTwoFactorRequirement({ scope: ent, removeNoncompliant: true } as never))).code).toBe("invalid_argument");
    const soft = await api.enterprise.updateTwoFactorRequirement({ scope: ent, required: true, secureMethodsOnly: true } as never);
    expect([soft.requirement?.required, soft.requirement?.secureMethodsOnly, soft.requirement?.noncompliantCount, soft.removedPrincipalIds]).toEqual([true, true, 3, []]);
    const hard = await api.enterprise.updateTwoFactorRequirement({ scope: ent, removeNoncompliant: true } as never);
    expect(hard.removedPrincipalIds).toHaveLength(3);
    expect(hard.removedPrincipalIds).toContain(store.p("dev").id);
    expect(hard.requirement?.noncompliantCount).toBe(0);
  });

  it("applies policy updates partially", async () => {
    const { api } = setup();
    const p = await api.enterprise.getEnterprisePolicies({ enterprise: "acme-group" });
    expect([p.repoCreation, p.basePermission, p.agentsCanLandWithoutHumanApproval]).toEqual(["ENTERPRISE_REPO_CREATION_POLICY_PRIVATE_INTERNAL", "ENTERPRISE_BASE_PERMISSION_READ", "ENTERPRISE_POLICY_TOGGLE_DISABLED"]);
    const u = await api.enterprise.updateEnterprisePolicies({ enterprise: "acme-group", basePermission: "ENTERPRISE_BASE_PERMISSION_WRITE", forking: "ENTERPRISE_FORKING_POLICY_UNSPECIFIED" });
    expect(u.basePermission).toBe("ENTERPRISE_BASE_PERMISSION_WRITE");
    expect(u.forking).toBe(p.forking);
    expect(u.repoCreation).toBe(p.repoCreation);
    expect(u.updatedAt).toBeTruthy();
    expect((await api.enterprise.getEnterprisePolicies({ enterprise: "acme-group" })).basePermission).toBe("ENTERPRISE_BASE_PERMISSION_WRITE");
    const sel = await fails(api.enterprise.updateEnterprisePolicies({ enterprise: "acme-group", actions: { enablement: "ENTERPRISE_ACTIONS_ENABLEMENT_SELECTED_ORGS" } }));
    expect(sel.reason).toBe("NO_ORGS_SELECTED");
  });
});

describe("access", () => {
  it("answers unauthenticated without a token", async () => {
    const { anon } = setup();
    const calls: Promise<unknown>[] = [
      anon.billing.listBillingPlans({}),
      anon.billing.getAccountPlan({ org: "acme" }),
      anon.ops.getOpsHealth({}),
      anon.ops.validateOpsConfig({ toml: "" }),
      anon.enterprise.getEnterprise({ enterprise: "acme-group" }),
      anon.enterprise.updateScimConfig({ scope: ent, enabled: true } as never),
      anon.enterprise.listIpAllowList({ scope: ent } as never),
    ];
    for (const call of calls) expect((await fails(call)).code).toBe("unauthenticated");
  });

  it("refuses writes by a principal who is not the operator, and audits the refusal", async () => {
    const { store } = setup();
    store.tokens.push({ tokenId: "tok_agent", parentTokenId: "", principalId: store.p("triage").id, revoked: false } as never);
    store.tokenSecrets.set("agent-secret-0001", "tok_agent");
    const agent = createClient(fixtureRpc(store, "agent-secret-0001"));
    const e = await fails(agent.enterprise.updateScimConfig({ scope: ent, enabled: true } as never));
    expect([e.code, e.reason]).toEqual(["permission_denied", "NOT_AN_OPERATOR"]);
    expect(store.audit.at(-1)).toMatchObject({ allowed: false, action: "enterprise.scim.update" });
    expect((await fails(agent.billing.changePlan({ org: "acme", planId: "team", seats: 12 }))).reason).toBe("NOT_AN_ORG_OWNER");
    expect((await fails(agent.enterprise.updateEnterprisePolicies({ enterprise: "acme-group", forking: "ENTERPRISE_FORKING_POLICY_DISABLED" }))).code).toBe("permission_denied");
    expect((await agent.enterprise.getEnterprise({ enterprise: "acme-group" })).slug).toBe("acme-group");
  });

  it("audits successful admin writes", async () => {
    const { api, store } = setup();
    const n = store.audit.length;
    await api.enterprise.updateScimConfig({ scope: ent, enabled: true } as never);
    expect(store.audit).toHaveLength(n + 1);
    expect(store.audit.at(-1)).toMatchObject({ action: "enterprise.scim.update", allowed: true });
  });
});
