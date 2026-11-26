/** EnterpriseService for the fixture: the enterprise, SAML, SCIM, domains, 2FA, IP allow lists and policies. */
import {
  EnterpriseService,
  type EnterpriseActionsPolicyJson,
  type EnterprisePoliciesJson,
  type EnterpriseTwoFactorRequirementJson,
  type IpAllowListEntryJson,
  type SamlSsoConfigJson,
  type ScimConfigJson,
  type VerifiedDomainJson,
} from "../../gen/bee/v1/enterprise_pb";
import { BeeError } from "../transport";
import {
  adminState,
  authorizeOperator,
  authorizeWrite,
  DAY,
  domain as newDomain,
  enterpriseOf,
  HUB,
  ORGS,
  scopeOf,
  settingsOf,
  signedIn,
  stamp,
  targetOf,
  type DomainState,
  type EnterpriseState,
  type IpEntryState,
  type SamlState,
  type Scope,
  type ScopeSettings,
} from "./admin-state";
import { defineService } from "./router";
import { digestHex, ulid } from "./seed";
import type { FixtureStore } from "./store";
import { invalid, page } from "./util";

// ---------------------------------------------------------------- enterprise

function enterpriseJson(s: FixtureStore, e: EnterpriseState) {
  return {
    id: e.id,
    slug: e.slug,
    name: e.name,
    description: e.description,
    websiteUrl: e.websiteUrl,
    managedUsers: e.managedUsers,
    managedUsersShortcode: e.shortcode,
    orgCount: ORGS.length,
    memberCount: ORGS.reduce((n, o) => n + o.members, 0),
    createdAt: s.iso(e.created),
    updatedAt: s.iso(e.updated),
  };
}

const httpUrl = (v: string) => {
  try {
    const u = new URL(v);
    return u.protocol === "https:" || u.protocol === "http:";
  } catch {
    return false;
  }
};

// ---------------------------------------------------------------- SAML

const PEM = /-----BEGIN CERTIFICATE-----[\s\S]+?-----END CERTIFICATE-----/;
const SIGNATURES = ["rsa-sha256", "rsa-sha384", "rsa-sha512"];
const DIGESTS = ["sha256", "sha384", "sha512"];

function samlJson(s: FixtureStore, sc: Scope, v: SamlState): SamlSsoConfigJson {
  return {
    scope: sc.json,
    enabled: v.enabled,
    enforced: v.enforced,
    idpMetadataUrl: v.metadataUrl,
    idpMetadataXml: v.metadataXml,
    idpEntityId: v.entityId,
    idpSsoUrl: v.ssoUrl,
    idpCertificatePem: v.certPem,
    idpCertificateExpiresAt: stamp(s, v.certExpires),
    spEntityId: HUB + "/saml/metadata",
    spAcsUrl: HUB + "/saml/acs",
    signatureMethod: v.signatureMethod,
    digestMethod: v.digestMethod,
    sessionLifetimeSeconds: v.sessionLifetime,
    lastTestedAt: stamp(s, v.lastTested),
    updatedAt: stamp(s, v.updated),
  };
}

interface Metadata {
  problems: string[];
  entityId: string;
  ssoUrl: string;
}

/** A minimal, forgiving read of SAML IdP metadata: well-formedness, entityID and the SSO endpoint. */
export function readMetadata(xml: string): Metadata {
  const problems: string[] = [];
  const text = xml.trim();
  if (!text.startsWith("<")) return { problems: ["metadata is not XML"], entityId: "", ssoUrl: "" };
  const stack: string[] = [];
  let broken = false;
  for (const m of text.replace(/<!--[\s\S]*?-->/g, "").replace(/<\?[\s\S]*?\?>/g, "").matchAll(/<(\/?)([A-Za-z_][\w:.-]*)[^>]*?(\/?)>/g)) {
    const [, closing, name = "", selfClosing] = m;
    if (selfClosing) continue;
    if (!closing) stack.push(name);
    else if (stack.pop() !== name) {
      broken = true;
      break;
    }
  }
  if (broken || stack.length) problems.push("metadata XML is not well-formed");
  if (!/<(?:[\w.-]+:)?EntityDescriptor\b/.test(text)) {
    problems.push("metadata has no EntityDescriptor element");
    return { problems, entityId: "", ssoUrl: "" };
  }
  const entityId = /<(?:[\w.-]+:)?EntityDescriptor\b[^>]*\bentityID\s*=\s*"([^"]+)"/.exec(text)?.[1] ?? "";
  if (!entityId) problems.push("EntityDescriptor has no entityID");
  const sso = /<(?:[\w.-]+:)?SingleSignOnService\b[^>]*\bLocation\s*=\s*"([^"]+)"/.exec(text)?.[1] ?? "";
  if (!sso) problems.push("no SingleSignOnService endpoint found");
  else if (!sso.startsWith("https://")) problems.push("the SingleSignOnService Location must use https");
  if (!/<(?:[\w.-]+:)?X509Certificate\b/.test(text)) problems.push("metadata has no signing certificate");
  return { problems, entityId, ssoUrl: sso };
}

// ---------------------------------------------------------------- SCIM

function scimJson(s: FixtureStore, sc: Scope, v: ScopeSettings["scim"]): ScimConfigJson {
  return {
    scope: sc.json,
    enabled: v.enabled,
    endpointUrl: HUB + "/scim/v2",
    provisionsTeams: v.provisionsTeams,
    activeTokenCount: v.activeTokens,
    provisionedCount: String(v.provisioned),
    lastRequestAt: undefined,
  };
}

// ---------------------------------------------------------------- domains

function domainJson(s: FixtureStore, sc: Scope, d: DomainState): VerifiedDomainJson {
  return {
    id: d.id,
    scope: sc.json,
    domain: d.domain,
    state: d.state,
    dnsTxtRecordName: d.recordName,
    dnsTxtRecordValue: d.recordValue,
    challengeExpiresAt: stamp(s, d.challengeExpires),
    lastCheckedAt: stamp(s, d.lastChecked),
    verifiedAt: stamp(s, d.verifiedAt),
    createdAt: s.iso(d.created),
  };
}

const DOMAIN = /^(?=.{1,253}$)([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$/;

// ---------------------------------------------------------------- IP allow list

function validIpv4(a: string): boolean {
  const parts = a.split(".");
  return parts.length === 4 && parts.every((p) => /^\d{1,3}$/.test(p) && Number(p) <= 255);
}

function validIpv6(a: string): boolean {
  if (!/^[0-9a-fA-F:]+$/.test(a) || a.includes(":::")) return false;
  const halves = a.split("::");
  if (halves.length > 2) return false;
  const groups = halves.flatMap((h) => (h ? h.split(":") : []));
  if (!groups.every((g) => /^[0-9a-fA-F]{1,4}$/.test(g))) return false;
  return halves.length === 2 ? groups.length <= 7 : groups.length === 8;
}

/** Whether `cidr` is `address/prefix` for IPv4 or IPv6. */
export function validCidr(cidr: string): boolean {
  const m = /^([^/]+)\/(\d{1,3})$/.exec(cidr);
  if (!m) return false;
  const [, addr = "", prefix = ""] = m;
  const n = Number(prefix);
  if (addr.includes(":")) return validIpv6(addr) && n <= 128;
  return validIpv4(addr) && n <= 32;
}

function ipJson(s: FixtureStore, sc: Scope, e: IpEntryState): IpAllowListEntryJson {
  return { id: e.id, scope: sc.json, cidr: e.cidr, description: e.description, active: e.active, appId: "", createdAt: s.iso(e.created), updatedAt: s.iso(e.updated) };
}

// ---------------------------------------------------------------- two-factor

function twoFactorJson(s: FixtureStore, sc: Scope, v: ScopeSettings["twoFactor"]): EnterpriseTwoFactorRequirementJson {
  return { scope: sc.json, required: v.required, secureMethodsOnly: v.secureOnly, noncompliantCount: v.noncompliant.length, requiredSince: stamp(s, v.requiredSince) };
}

// ---------------------------------------------------------------- policies

function policiesJson(s: FixtureStore): EnterprisePoliciesJson {
  const st = adminState(s);
  const p = st.policies;
  return {
    enterpriseId: st.enterprise.id,
    repoCreation: p.repoCreation as EnterprisePoliciesJson["repoCreation"],
    forking: p.forking as EnterprisePoliciesJson["forking"],
    basePermission: p.basePermission as EnterprisePoliciesJson["basePermission"],
    membersCanDeleteRepos: p.membersCanDeleteRepos as EnterprisePoliciesJson["membersCanDeleteRepos"],
    membersCanChangeRepoVisibility: p.membersCanChangeRepoVisibility as EnterprisePoliciesJson["membersCanChangeRepoVisibility"],
    membersCanInviteOutsideCollaborators: p.membersCanInviteOutsideCollaborators as EnterprisePoliciesJson["membersCanInviteOutsideCollaborators"],
    membersCanCreateTeams: p.membersCanCreateTeams as EnterprisePoliciesJson["membersCanCreateTeams"],
    membersCanInstallApps: p.membersCanInstallApps as EnterprisePoliciesJson["membersCanInstallApps"],
    agentsCanLandWithoutHumanApproval: p.agentsCanLandWithoutHumanApproval as EnterprisePoliciesJson["agentsCanLandWithoutHumanApproval"],
    actions: {
      enablement: p.actions.enablement as EnterpriseActionsPolicyJson["enablement"],
      enabledOrgs: p.actions.enabledOrgs,
      allowedActions: p.actions.allowedActions as EnterpriseActionsPolicyJson["allowedActions"],
      allowedActionPatterns: p.actions.allowedActionPatterns,
      allowVerifiedCreators: p.actions.allowVerifiedCreators,
      workflowPolicyId: p.actions.workflowPolicyId,
    },
    orgRulesetIds: [],
    updatedAt: stamp(s, p.updated),
  };
}

/** An enum field in an update: UNSPECIFIED means leave it alone. */
const pick = (v: string | undefined, cur: string): string => (v && !v.endsWith("_UNSPECIFIED") ? v : cur);

const audit = (s: FixtureStore, actor: string, action: string, target: string) => s.appendAudit(actor, action, target, "", true, "");

export const enterprise = defineService(EnterpriseService, {
  getEnterprise: (s, req, ctx) => {
    signedIn(s, ctx);
    return enterpriseJson(s, enterpriseOf(s, req.enterprise));
  },

  updateEnterprise: (s, req, ctx) => {
    const e = enterpriseOf(s, req.enterprise);
    const actor = authorizeOperator(s, ctx, "enterprise.update", "enterprise:" + e.slug);
    if (req.name !== undefined && !req.name.trim()) throw invalid("an enterprise needs a name", "NAME_REQUIRED");
    if (req.name !== undefined && req.name.length > 80) throw invalid("the name is at most 80 characters", "NAME_TOO_LONG");
    if (req.description !== undefined && req.description.length > 500) throw invalid("the description is at most 500 characters", "DESCRIPTION_TOO_LONG");
    if (req.websiteUrl && !httpUrl(req.websiteUrl)) throw invalid("the website must be an http or https URL", "INVALID_URL");
    const shortcode = req.managedUsersShortcode ?? e.shortcode;
    if (req.managedUsersShortcode !== undefined && req.managedUsersShortcode && !/^[A-Za-z0-9]{2,16}$/.test(req.managedUsersShortcode))
      throw invalid("the shortcode is 2 to 16 letters or digits", "INVALID_SHORTCODE");
    const managed = req.managedUsers ?? e.managedUsers;
    if (managed && !shortcode) throw invalid("managed users need a shortcode to suffix their usernames", "INVALID_SHORTCODE");
    if (req.name !== undefined) e.name = req.name.trim();
    if (req.description !== undefined) e.description = req.description.trim();
    if (req.websiteUrl !== undefined) e.websiteUrl = req.websiteUrl.trim();
    e.managedUsers = managed;
    e.shortcode = shortcode;
    e.updated = s.now();
    audit(s, actor, "enterprise.update", "enterprise:" + e.slug);
    return enterpriseJson(s, e);
  },

  listEnterpriseOrgs: (s, req, ctx) => {
    signedIn(s, ctx);
    const e = enterpriseOf(s, req.enterprise);
    const st = adminState(s);
    const needle = (req.query ?? "").trim().toLowerCase();
    const rows = ORGS.filter((o) => !needle || o.name.includes(needle) || o.display.toLowerCase().includes(needle)).map((o) => ({
      enterpriseId: e.id,
      org: o.name,
      orgId: st.orgIds.get(o.name) ?? "",
      memberCount: o.members,
      repoCount: o.repos,
      joinedAt: s.iso(s.anchor - o.joinedDaysAgo * DAY),
    }));
    const p = page(rows, req.page);
    return { orgs: p.items, page: { nextPageToken: p.next } };
  },

  listEnterpriseOwners: (s, req, ctx) => {
    signedIn(s, ctx);
    const e = enterpriseOf(s, req.enterprise);
    const p = page([{ principal: s.p("mara"), addedAt: s.iso(e.created) }], req.page);
    return { owners: p.items, page: { nextPageToken: p.next } };
  },

  getSamlSsoConfig: (s, req, ctx) => {
    signedIn(s, ctx);
    const sc = scopeOf(s, req.scope);
    return samlJson(s, sc, settingsOf(s, sc).saml);
  },

  updateSamlSsoConfig: (s, req, ctx) => {
    const sc = scopeOf(s, req.scope);
    const actor = authorizeWrite(s, ctx, sc, "enterprise.saml.update");
    const cur = settingsOf(s, sc).saml;
    const next: SamlState = {
      ...cur,
      enabled: req.enabled ?? cur.enabled,
      enforced: req.enforced ?? cur.enforced,
      metadataUrl: req.idpMetadataUrl ?? cur.metadataUrl,
      metadataXml: req.idpMetadataXml ?? cur.metadataXml,
      entityId: req.idpEntityId ?? cur.entityId,
      ssoUrl: req.idpSsoUrl ?? cur.ssoUrl,
      certPem: req.idpCertificatePem ?? cur.certPem,
      signatureMethod: req.signatureMethod || cur.signatureMethod,
      digestMethod: req.digestMethod || cur.digestMethod,
      sessionLifetime: req.sessionLifetimeSeconds || cur.sessionLifetime,
    };
    if (next.metadataUrl && !(httpUrl(next.metadataUrl) && next.metadataUrl.startsWith("https://"))) throw invalid("the metadata URL must be an https URL", "INVALID_URL");
    if (next.ssoUrl && !next.ssoUrl.startsWith("https://")) throw invalid("the IdP sign-on URL must be an https URL", "INVALID_URL");
    if (next.certPem && !PEM.test(next.certPem)) throw invalid("the certificate must be PEM encoded (-----BEGIN CERTIFICATE-----)", "INVALID_CERTIFICATE");
    if (!SIGNATURES.includes(next.signatureMethod)) throw invalid("signatureMethod must be one of " + SIGNATURES.join(", "), "INVALID_SIGNATURE_METHOD");
    if (!DIGESTS.includes(next.digestMethod)) throw invalid("digestMethod must be one of " + DIGESTS.join(", "), "INVALID_DIGEST_METHOD");
    if (next.sessionLifetime < 300 || next.sessionLifetime > 30 * 86_400) throw invalid("the session lifetime is between 5 minutes and 30 days", "INVALID_SESSION_LIFETIME");
    if (next.metadataXml) {
      const meta = readMetadata(next.metadataXml);
      if (!next.entityId) next.entityId = meta.entityId;
      if (!next.ssoUrl) next.ssoUrl = meta.ssoUrl;
    }
    if (next.enabled) {
      const manual = next.entityId && next.ssoUrl && next.certPem;
      if (!manual && !next.metadataUrl && !next.metadataXml)
        throw invalid("turning SSO on needs the IdP entity id, sign-on URL and certificate, or an IdP metadata URL or XML", "SAML_INCOMPLETE");
    }
    if (next.enforced && !next.enabled) throw invalid("SSO must be enabled before it can be enforced", "SAML_NOT_ENABLED");
    next.certExpires = next.certPem ? s.anchor + 400 * DAY : 0;
    next.updated = s.now();
    settingsOf(s, sc).saml = next;
    audit(s, actor, "enterprise.saml.update", targetOf(sc));
    return samlJson(s, sc, next);
  },

  testSamlSsoConfig: (s, req, ctx) => {
    const sc = scopeOf(s, req.scope);
    const actor = authorizeWrite(s, ctx, sc, "enterprise.saml.test");
    const cur = settingsOf(s, sc).saml;
    const url = req.idpMetadataUrl || (req.idpMetadataXml ? "" : cur.metadataUrl);
    const xml = req.idpMetadataXml || (req.idpMetadataUrl ? "" : cur.metadataXml);
    let problems: string[];
    if (xml) problems = readMetadata(xml).problems;
    else if (url) {
      problems = [];
      if (!httpUrl(url)) problems.push("the metadata URL is not a valid URL");
      else if (!url.startsWith("https://")) problems.push("the metadata URL must use https");
      else if (new URL(url).hostname.endsWith(".invalid")) problems.push("could not fetch the metadata: host not found");
    } else problems = ["no IdP metadata to test: give a metadata URL or XML"];
    const ok = problems.length === 0;
    if (ok) settingsOf(s, sc).saml.lastTested = s.now();
    audit(s, actor, "enterprise.saml.test", targetOf(sc));
    return {
      metadataOk: ok,
      problems,
      testSignInUrl: ok ? `${HUB}/saml/test?scope=${encodeURIComponent(targetOf(sc))}&state=${digestHex("saml-test:" + sc.key + ":" + (xml || url), 8)}` : "",
    };
  },

  getScimConfig: (s, req, ctx) => {
    signedIn(s, ctx);
    const sc = scopeOf(s, req.scope);
    return scimJson(s, sc, settingsOf(s, sc).scim);
  },

  updateScimConfig: (s, req, ctx) => {
    const sc = scopeOf(s, req.scope);
    const actor = authorizeWrite(s, ctx, sc, "enterprise.scim.update");
    const v = settingsOf(s, sc).scim;
    v.enabled = req.enabled ?? v.enabled;
    v.provisionsTeams = req.provisionsTeams ?? v.provisionsTeams;
    audit(s, actor, "enterprise.scim.update", targetOf(sc));
    return scimJson(s, sc, v);
  },

  createScimToken: (s, req, ctx) => {
    const sc = scopeOf(s, req.scope);
    const actor = authorizeWrite(s, ctx, sc, "enterprise.scim.token");
    const v = settingsOf(s, sc).scim;
    if (!v.enabled) throw new BeeError("failed_precondition", { reason: "SCIM_DISABLED", message: "turn on SCIM provisioning before creating a token" });
    if ((req.description ?? "").length > 120) throw invalid("the description is at most 120 characters", "DESCRIPTION_TOO_LONG");
    const ttl = req.ttlSeconds || 90 * 86_400;
    if (ttl > 365 * 86_400) throw invalid("a token lives at most 365 days", "TTL_TOO_LONG");
    v.minted++;
    v.activeTokens++;
    const now = s.now();
    audit(s, actor, "enterprise.scim.token", targetOf(sc));
    return {
      token: "scim_" + digestHex(`scim-token:${sc.key}:${v.minted}`, 24),
      tokenId: ulid(now, `scim-token:${sc.key}:${v.minted}`),
      expiresAt: s.iso(now + ttl * 1000),
    };
  },

  listVerifiedDomains: (s, req, ctx) => {
    signedIn(s, ctx);
    const sc = scopeOf(s, req.scope);
    const p = page(settingsOf(s, sc).domains, req.page);
    return { domains: p.items.map((d) => domainJson(s, sc, d)), page: { nextPageToken: p.next } };
  },

  addVerifiedDomain: (s, req, ctx) => {
    const sc = scopeOf(s, req.scope);
    const actor = authorizeWrite(s, ctx, sc, "enterprise.domain.add");
    const name = (req.domain ?? "").trim().toLowerCase();
    if (!DOMAIN.test(name)) throw invalid(`${req.domain || "(empty)"} is not a domain name`, "INVALID_DOMAIN");
    const set = settingsOf(s, sc);
    if (set.domains.some((d) => d.domain === name)) throw new BeeError("already_exists", { reason: "DOMAIN_EXISTS", message: name + " is already on the list" });
    set.domainSeq++;
    const d = newDomain(name, req.approvedOnly ? "VERIFIED_DOMAIN_STATE_APPROVED" : "VERIFIED_DOMAIN_STATE_PENDING", s.now(), 0);
    d.id = ulid(s.now(), `domain:${sc.key}:${name}:${set.domainSeq}`);
    if (d.state === "VERIFIED_DOMAIN_STATE_PENDING") d.recordValue = "bee-site-verification=" + digestHex(`domain-challenge:${sc.key}:${name}`, 16);
    set.domains.push(d);
    audit(s, actor, "enterprise.domain.add", name);
    return domainJson(s, sc, d);
  },

  checkVerifiedDomain: (s, req, ctx) => {
    const sc = scopeOf(s, req.scope);
    const actor = authorizeWrite(s, ctx, sc, "enterprise.domain.check");
    const d = settingsOf(s, sc).domains.find((x) => x.id === req.domainId);
    if (!d) throw new BeeError("not_found", { reason: "DOMAIN_NOT_FOUND", message: "no domain " + (req.domainId || "(none)") });
    const now = s.now();
    if (d.state === "VERIFIED_DOMAIN_STATE_FAILED") {
      // A new check after expiry issues a fresh challenge.
      d.state = "VERIFIED_DOMAIN_STATE_PENDING";
      d.challengeExpires = now + 7 * DAY;
      d.checks = 0;
    }
    if (d.state === "VERIFIED_DOMAIN_STATE_PENDING") {
      d.lastChecked = now;
      d.checks++;
      if (now > d.challengeExpires) d.state = "VERIFIED_DOMAIN_STATE_FAILED";
      // The first lookup misses (DNS has not propagated); the second finds the record.
      else if (d.checks >= 2) {
        d.state = "VERIFIED_DOMAIN_STATE_VERIFIED";
        d.verifiedAt = now;
        d.challengeExpires = 0;
      }
    }
    audit(s, actor, "enterprise.domain.check", d.domain);
    return domainJson(s, sc, d);
  },

  deleteVerifiedDomain: (s, req, ctx) => {
    const sc = scopeOf(s, req.scope);
    const actor = authorizeWrite(s, ctx, sc, "enterprise.domain.delete");
    const set = settingsOf(s, sc);
    const at = set.domains.findIndex((x) => x.id === req.domainId);
    if (at < 0) throw new BeeError("not_found", { reason: "DOMAIN_NOT_FOUND", message: "no domain " + (req.domainId || "(none)") });
    const [gone] = set.domains.splice(at, 1);
    audit(s, actor, "enterprise.domain.delete", gone?.domain ?? "");
    return {};
  },

  getTwoFactorRequirement: (s, req, ctx) => {
    signedIn(s, ctx);
    const sc = scopeOf(s, req.scope);
    return twoFactorJson(s, sc, settingsOf(s, sc).twoFactor);
  },

  updateTwoFactorRequirement: (s, req, ctx) => {
    const sc = scopeOf(s, req.scope);
    const actor = authorizeWrite(s, ctx, sc, "enterprise.2fa.update");
    const v = settingsOf(s, sc).twoFactor;
    const required = req.required ?? v.required;
    if (req.removeNoncompliant && !required) throw invalid("members can only be removed while two-factor authentication is required", "REMOVE_REQUIRES_REQUIREMENT");
    if (required && !v.required) v.requiredSince = s.now();
    if (!required) v.requiredSince = 0;
    v.required = required;
    v.secureOnly = req.secureMethodsOnly ?? v.secureOnly;
    let removed: string[] = [];
    if (req.removeNoncompliant) {
      removed = v.noncompliant.map((h) => s.p(h).id);
      v.noncompliant = [];
    }
    audit(s, actor, "enterprise.2fa.update", targetOf(sc) + (removed.length ? ` (removed ${removed.length})` : ""));
    return { requirement: twoFactorJson(s, sc, v), removedPrincipalIds: removed };
  },

  listIpAllowListEntries: (s, req, ctx) => {
    signedIn(s, ctx);
    const sc = scopeOf(s, req.scope);
    const p = page(settingsOf(s, sc).ips, req.page);
    return { entries: p.items.map((e) => ipJson(s, sc, e)), page: { nextPageToken: p.next } };
  },

  createIpAllowListEntry: (s, req, ctx) => {
    const sc = scopeOf(s, req.scope);
    const actor = authorizeWrite(s, ctx, sc, "enterprise.ip.create");
    const cidr = (req.cidr ?? "").trim().toLowerCase();
    if (!validCidr(cidr)) throw invalid(`${req.cidr || "(empty)"} is not a CIDR range such as 203.0.113.0/24 or 2001:db8::/32`, "INVALID_CIDR");
    if ((req.description ?? "").length > 200) throw invalid("the description is at most 200 characters", "DESCRIPTION_TOO_LONG");
    const set = settingsOf(s, sc);
    if (set.ips.some((e) => e.cidr === cidr)) throw new BeeError("already_exists", { reason: "IP_ENTRY_EXISTS", message: cidr + " is already on the allow list" });
    set.ipSeq++;
    const now = s.now();
    const e: IpEntryState = { id: ulid(now, `ip:${sc.key}:${cidr}:${set.ipSeq}`), cidr, description: (req.description ?? "").trim(), active: req.active ?? false, created: now, updated: now };
    set.ips.push(e);
    audit(s, actor, "enterprise.ip.create", cidr);
    return ipJson(s, sc, e);
  },

  deleteIpAllowListEntry: (s, req, ctx) => {
    const sc = scopeOf(s, req.scope);
    const actor = authorizeWrite(s, ctx, sc, "enterprise.ip.delete");
    const set = settingsOf(s, sc);
    const at = set.ips.findIndex((e) => e.id === req.entryId);
    if (at < 0) throw new BeeError("not_found", { reason: "IP_ENTRY_NOT_FOUND", message: "no allow list entry " + (req.entryId || "(none)") });
    const [gone] = set.ips.splice(at, 1);
    audit(s, actor, "enterprise.ip.delete", gone?.cidr ?? "");
    return {};
  },

  getEnterprisePolicies: (s, req, ctx) => {
    signedIn(s, ctx);
    enterpriseOf(s, req.enterprise);
    return policiesJson(s);
  },

  updateEnterprisePolicies: (s, req, ctx) => {
    const e = enterpriseOf(s, req.enterprise);
    const actor = authorizeOperator(s, ctx, "enterprise.policies.update", "enterprise:" + e.slug);
    const p = adminState(s).policies;
    let actions = p.actions;
    if (req.actions) {
      const a = req.actions;
      const enablement = pick(a.enablement, p.actions.enablement);
      const enabledOrgs = a.enabledOrgs ?? [];
      for (const o of enabledOrgs) if (!ORGS.some((x) => x.name === o)) throw new BeeError("not_found", { reason: "ORG_NOT_FOUND", message: "no org " + o });
      if (enablement === "ENTERPRISE_ACTIONS_ENABLEMENT_SELECTED_ORGS" && !enabledOrgs.length) throw invalid("choose at least one org for Actions", "NO_ORGS_SELECTED");
      actions = {
        enablement,
        enabledOrgs: enablement === "ENTERPRISE_ACTIONS_ENABLEMENT_SELECTED_ORGS" ? enabledOrgs : [],
        allowedActions: pick(a.allowedActions, p.actions.allowedActions),
        allowedActionPatterns: a.allowedActionPatterns ?? [],
        allowVerifiedCreators: a.allowVerifiedCreators ?? false,
        workflowPolicyId: a.workflowPolicyId ?? "",
      };
    }
    p.repoCreation = pick(req.repoCreation, p.repoCreation);
    p.forking = pick(req.forking, p.forking);
    p.basePermission = pick(req.basePermission, p.basePermission);
    p.membersCanDeleteRepos = pick(req.membersCanDeleteRepos, p.membersCanDeleteRepos);
    p.membersCanChangeRepoVisibility = pick(req.membersCanChangeRepoVisibility, p.membersCanChangeRepoVisibility);
    p.membersCanInviteOutsideCollaborators = pick(req.membersCanInviteOutsideCollaborators, p.membersCanInviteOutsideCollaborators);
    p.membersCanCreateTeams = pick(req.membersCanCreateTeams, p.membersCanCreateTeams);
    p.membersCanInstallApps = pick(req.membersCanInstallApps, p.membersCanInstallApps);
    p.agentsCanLandWithoutHumanApproval = pick(req.agentsCanLandWithoutHumanApproval, p.agentsCanLandWithoutHumanApproval);
    p.actions = actions;
    p.updated = s.now();
    audit(s, actor, "enterprise.policies.update", "enterprise:" + e.slug);
    return policiesJson(s);
  },
});
