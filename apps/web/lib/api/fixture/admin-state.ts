/**
 * Shared state and guards for the cloud and admin fixture handlers (billing, ops, enterprise).
 * State hangs off the store in a WeakMap so the store itself stays untouched. Every timestamp is
 * anchor-relative, so a fresh store always reads the same.
 */
import type { BillingCycleJson } from "../../gen/bee/v1/billing_pb";
import type { EnterpriseScopeJson } from "../../gen/bee/v1/enterprise_pb";
import { BeeError } from "../transport";
import type { Ctx } from "./router";
import { digestHex, ulid } from "./seed";
import type { FixtureStore } from "./store";

export const OPERATOR = "mara";
export const HUB = "https://hub.bee.example";
export const DAY = 86_400_000;
export const ENTERPRISE_SLUG = "acme-group";

/** Lattice is added to the world by another module, so its counts are fixed here. */
export const ORGS = [
  { name: "acme", display: "Acme Robotics", members: 4, repos: 4, joinedDaysAgo: 200 },
  { name: "lattice", display: "Lattice Labs", members: 3, repos: 5, joinedDaysAgo: 90 },
] as const;

export interface Scope {
  /** "ent" or "org:<name>". */
  key: string;
  kind: "org" | "enterprise";
  org: string;
  json: EnterpriseScopeJson;
}

export interface Account {
  planId: string;
  cycle: BillingCycleJson;
  purchased: number;
  consumed: number;
  cycleStart: number;
  nextBilling: number;
  pendingPlanId: string;
  pendingSeats: number;
}

export interface SamlState {
  enabled: boolean;
  enforced: boolean;
  metadataUrl: string;
  metadataXml: string;
  entityId: string;
  ssoUrl: string;
  certPem: string;
  certExpires: number;
  signatureMethod: string;
  digestMethod: string;
  sessionLifetime: number;
  lastTested: number;
  updated: number;
}

export interface ScimState {
  enabled: boolean;
  provisionsTeams: boolean;
  activeTokens: number;
  minted: number;
  provisioned: number;
}

export interface DomainState {
  id: string;
  domain: string;
  state: "VERIFIED_DOMAIN_STATE_PENDING" | "VERIFIED_DOMAIN_STATE_VERIFIED" | "VERIFIED_DOMAIN_STATE_FAILED" | "VERIFIED_DOMAIN_STATE_APPROVED";
  recordName: string;
  recordValue: string;
  challengeExpires: number;
  lastChecked: number;
  verifiedAt: number;
  created: number;
  checks: number;
}

export interface IpEntryState {
  id: string;
  cidr: string;
  description: string;
  active: boolean;
  created: number;
  updated: number;
}

export interface TwoFactorState {
  required: boolean;
  secureOnly: boolean;
  requiredSince: number;
  noncompliant: string[]; // principal handles
}

/** Settings that exist once per scope (the enterprise and each org). */
export interface ScopeSettings {
  saml: SamlState;
  scim: ScimState;
  domains: DomainState[];
  ips: IpEntryState[];
  twoFactor: TwoFactorState;
  domainSeq: number;
  ipSeq: number;
}

export interface EnterpriseState {
  id: string;
  slug: string;
  name: string;
  description: string;
  websiteUrl: string;
  managedUsers: boolean;
  shortcode: string;
  created: number;
  updated: number;
}

export interface PoliciesState {
  repoCreation: string;
  forking: string;
  basePermission: string;
  membersCanDeleteRepos: string;
  membersCanChangeRepoVisibility: string;
  membersCanInviteOutsideCollaborators: string;
  membersCanCreateTeams: string;
  membersCanInstallApps: string;
  agentsCanLandWithoutHumanApproval: string;
  actions: {
    enablement: string;
    enabledOrgs: string[];
    allowedActions: string;
    allowedActionPatterns: string[];
    allowVerifiedCreators: boolean;
    workflowPolicyId: string;
  };
  updated: number;
}

export interface AdminState {
  enterprise: EnterpriseState;
  accounts: Map<string, Account>;
  settings: Map<string, ScopeSettings>;
  policies: PoliciesState;
  orgIds: Map<string, string>;
}

const states = new WeakMap<FixtureStore, AdminState>();

export const adminState = (s: FixtureStore): AdminState => {
  let st = states.get(s);
  if (!st) {
    st = build(s);
    states.set(s, st);
  }
  return st;
};

function seedSettings(s: FixtureStore): ScopeSettings {
  const a = s.anchor;
  return {
    saml: {
      enabled: false,
      enforced: false,
      metadataUrl: "",
      metadataXml: "",
      entityId: "",
      ssoUrl: "",
      certPem: "",
      certExpires: 0,
      signatureMethod: "rsa-sha256",
      digestMethod: "sha256",
      sessionLifetime: 28_800,
      lastTested: 0,
      updated: 0,
    },
    scim: { enabled: false, provisionsTeams: false, activeTokens: 0, minted: 0, provisioned: 0 },
    domains: [
      domain("acme.example", "VERIFIED_DOMAIN_STATE_VERIFIED", a - 130 * DAY, a - 129 * DAY),
      domain("acme-robotics.example", "VERIFIED_DOMAIN_STATE_PENDING", a - 2 * DAY, 0),
    ],
    ips: [{ id: ulid(a - 90 * DAY, "ip:office-vpn"), cidr: "10.20.0.0/16", description: "Office VPN", active: true, created: a - 90 * DAY, updated: a - 90 * DAY }],
    twoFactor: { required: false, secureOnly: false, requiredSince: 0, noncompliant: ["dev", "lin", "sam"] },
    domainSeq: 0,
    ipSeq: 0,
  };
}

/** A domain record with its deterministic DNS challenge. */
export function domain(name: string, state: DomainState["state"], created: number, verifiedAt: number): DomainState {
  const approved = state === "VERIFIED_DOMAIN_STATE_APPROVED";
  return {
    id: ulid(created, "domain:" + name),
    domain: name,
    state,
    recordName: approved ? "" : "_bee-challenge." + name,
    recordValue: approved ? "" : "bee-site-verification=" + digestHex("domain-challenge:" + name, 16),
    challengeExpires: approved || verifiedAt ? 0 : created + 7 * DAY,
    lastChecked: verifiedAt,
    verifiedAt,
    created,
    checks: 0,
  };
}

/** The seat pools: acme's four members plus outside collaborators, lattice's three plus two. */
export const SEAT_POOLS = { acme: { members: 4, outside: 5 }, lattice: { members: 3, outside: 2 } } as const;

function build(s: FixtureStore): AdminState {
  const a = s.anchor;
  const orgIds = new Map<string, string>([
    ["acme", s.org.id],
    ["lattice", ulid(a - 380 * DAY, "org:lattice")],
  ]);
  const acme = SEAT_POOLS.acme.members + SEAT_POOLS.acme.outside;
  const lattice = SEAT_POOLS.lattice.members + SEAT_POOLS.lattice.outside;
  const accounts = new Map<string, Account>([
    ["org:acme", { planId: "team", cycle: "BILLING_CYCLE_MONTHLY", purchased: 12, consumed: acme, cycleStart: a - 11 * DAY, nextBilling: a + 19 * DAY, pendingPlanId: "", pendingSeats: 0 }],
    ["org:lattice", { planId: "free", cycle: "BILLING_CYCLE_MONTHLY", purchased: 5, consumed: lattice, cycleStart: a - 4 * DAY, nextBilling: a + 26 * DAY, pendingPlanId: "", pendingSeats: 0 }],
    ["ent", { planId: "enterprise", cycle: "BILLING_CYCLE_YEARLY", purchased: 25, consumed: acme + lattice, cycleStart: a - 120 * DAY, nextBilling: a + 245 * DAY, pendingPlanId: "", pendingSeats: 0 }],
  ]);
  return {
    enterprise: {
      id: ulid(a - 500 * DAY, "enterprise:" + ENTERPRISE_SLUG),
      slug: ENTERPRISE_SLUG,
      name: "Acme Group",
      description: "Acme Robotics and Lattice Labs under one account.",
      websiteUrl: "https://acme.example",
      managedUsers: false,
      shortcode: "",
      created: a - 500 * DAY,
      updated: a - 40 * DAY,
    },
    accounts,
    settings: new Map([
      ["ent", seedSettings(s)],
      ["org:acme", seedSettings(s)],
      ["org:lattice", seedSettings(s)],
    ]),
    policies: {
      repoCreation: "ENTERPRISE_REPO_CREATION_POLICY_PRIVATE_INTERNAL",
      forking: "ENTERPRISE_FORKING_POLICY_WITHIN_ENTERPRISE",
      basePermission: "ENTERPRISE_BASE_PERMISSION_READ",
      membersCanDeleteRepos: "ENTERPRISE_POLICY_TOGGLE_DISABLED",
      membersCanChangeRepoVisibility: "ENTERPRISE_POLICY_TOGGLE_DISABLED",
      membersCanInviteOutsideCollaborators: "ENTERPRISE_POLICY_TOGGLE_ALLOWED",
      membersCanCreateTeams: "ENTERPRISE_POLICY_TOGGLE_ALLOWED",
      membersCanInstallApps: "ENTERPRISE_POLICY_TOGGLE_NO_POLICY",
      agentsCanLandWithoutHumanApproval: "ENTERPRISE_POLICY_TOGGLE_DISABLED",
      actions: {
        enablement: "ENTERPRISE_ACTIONS_ENABLEMENT_ALL_ORGS",
        enabledOrgs: [],
        allowedActions: "ENTERPRISE_ALLOWED_ACTIONS_SELECTED",
        allowedActionPatterns: ["acme/*", "lattice/*"],
        allowVerifiedCreators: true,
        workflowPolicyId: "",
      },
      updated: 0,
    },
    orgIds,
  };
}

// ---------------------------------------------------------------- guards

/** Every RPC here needs a signed-in caller; there are no anonymous reads. */
export function signedIn(s: FixtureStore, ctx: Ctx): string {
  if (!ctx.token) throw new BeeError("unauthenticated", { reason: "UNAUTHENTICATED", message: "sign in to use this API" });
  return s.caller(ctx.token);
}

/** Resolves an org by name or id. */
export function orgName(s: FixtureStore, nameOrId: string): string {
  const st = adminState(s);
  const hit = ORGS.find((o) => o.name === nameOrId || st.orgIds.get(o.name) === nameOrId);
  if (!hit) throw new BeeError("not_found", { reason: "ORG_NOT_FOUND", message: "no org " + nameOrId });
  return hit.name;
}

/** Resolves the enterprise by slug, the `default` alias or id. */
export function enterpriseOf(s: FixtureStore, ref: string | undefined): EnterpriseState {
  const e = adminState(s).enterprise;
  if (!ref || (ref !== e.slug && ref !== "default" && ref !== e.id)) throw new BeeError("not_found", { reason: "ENTERPRISE_NOT_FOUND", message: "no enterprise " + (ref || "(none)") });
  return e;
}

export function scopeOf(s: FixtureStore, scope: EnterpriseScopeJson | undefined): Scope {
  if (scope?.enterprise) {
    const e = enterpriseOf(s, scope.enterprise);
    return { key: "ent", kind: "enterprise", org: "", json: { enterprise: e.slug } };
  }
  if (scope?.org) {
    const name = orgName(s, scope.org);
    return { key: "org:" + name, kind: "org", org: name, json: { org: name } };
  }
  throw new BeeError("invalid_argument", { reason: "SCOPE_REQUIRED", message: "scope must name an org or an enterprise" });
}

export const targetOf = (sc: Scope): string => (sc.kind === "enterprise" ? "enterprise:" + ENTERPRISE_SLUG : "org:" + sc.org);

/**
 * Write gate: the caller must be signed in and be the instance operator (enterprise scope) or an
 * owner of the org (mara owns acme only). A refusal is audited as denied.
 */
export function authorizeWrite(s: FixtureStore, ctx: Ctx, sc: Scope, action: string): string {
  const actor = signedIn(s, ctx);
  const owner = actor === OPERATOR && (sc.kind === "enterprise" || sc.org === "acme");
  if (!owner) {
    s.appendAudit(actor, action, targetOf(sc), "", false, sc.kind === "enterprise" ? "operator role required" : "org owner role required");
    if (sc.kind === "enterprise") throw new BeeError("permission_denied", { reason: "NOT_AN_OPERATOR", message: "only an operator can change the enterprise" });
    throw new BeeError("permission_denied", { reason: "NOT_AN_ORG_OWNER", message: "only an owner of " + sc.org + " can change it" });
  }
  return actor;
}

/** Write gate for the enterprise-level RPCs that carry a slug instead of a scope. */
export function authorizeOperator(s: FixtureStore, ctx: Ctx, action: string, target: string): string {
  const actor = signedIn(s, ctx);
  if (actor !== OPERATOR) {
    s.appendAudit(actor, action, target, "", false, "operator role required");
    throw new BeeError("permission_denied", { reason: "NOT_AN_OPERATOR", message: "only an operator can change the enterprise" });
  }
  return actor;
}

export function settingsOf(s: FixtureStore, sc: Scope): ScopeSettings {
  const hit = adminState(s).settings.get(sc.key);
  if (!hit) throw new BeeError("not_found", { reason: "ORG_NOT_FOUND", message: "no settings for " + sc.key });
  return hit;
}

export const stamp = (s: FixtureStore, ms: number): string | undefined => (ms ? s.iso(ms) : undefined);
