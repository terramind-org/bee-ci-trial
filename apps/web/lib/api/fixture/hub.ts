/**
 * Apps, teams, packages and Pages for the fixture: what an org's settings and a repo's Packages
 * and Pages screens read. The data is a small, fixed story (acme ships a container and an npm
 * client, publishes its console as a Pages site) so every screen has something real to show.
 */
import { AppService, type AppInstallationJson, type AppJson } from "../../gen/bee/v1/apps_pb";
import { PackageService, type PackageJson, type PackageVersionJson } from "../../gen/bee/v1/packages_pb";
import { PagesService, type PagesDeploymentJson } from "../../gen/bee/v1/pages_pb";
import { TeamService, type TeamJson, type TeamMemberJson } from "../../gen/bee/v1/teams_pb";
import { BeeError } from "../transport";
import { defineService, type ServiceDef } from "./router";
import { digestHex, objectId, ulid } from "./seed";
import type { FixtureStore } from "./store";
import { page } from "./util";

const DAY = 86_400_000;
const MINUTE = 60_000;

// ------------------------------------------------------------------ apps

interface AppSeed {
  slug: string;
  name: string;
  description: string;
  permissions: [string, "read" | "write"][];
  public: boolean;
  ageDays: number;
  installs: number;
}

const APPS: AppSeed[] = [
  { slug: "release-notes", name: "Release notes", description: "Drafts release notes from the pull requests that merged since the last tag.", permissions: [["changes", "read"], ["releases", "write"]], public: true, ageDays: 61, installs: 3 },
  { slug: "status-sync", name: "Status sync", description: "Mirrors check results to the fleet console's status page.", permissions: [["checks", "read"], ["issues", "write"]], public: false, ageDays: 24, installs: 1 },
];

const level = (l: "read" | "write") => (l === "read" ? "APP_PERMISSION_LEVEL_READ" : "APP_PERMISSION_LEVEL_WRITE");
const perms = (p: AppSeed["permissions"]) => p.map(([resource, l]) => ({ resource, level: level(l) }) as const);

function appJson(s: FixtureStore, owner: string, a: AppSeed): AppJson {
  const at = s.iso(s.anchor - a.ageDays * DAY);
  return {
    id: ulid(s.anchor - a.ageDays * DAY, "app:" + owner + ":" + a.slug),
    slug: a.slug,
    name: a.name,
    description: a.description,
    owner,
    clientId: "Iv1." + digestHex("app-client:" + a.slug, 8),
    permissions: [...perms(a.permissions)],
    events: ["change.landed"],
    public: a.public,
    installationCount: String(a.installs),
    createdAt: at,
    updatedAt: at,
  };
}

function installations(s: FixtureStore, account: string): AppInstallationJson[] {
  return APPS.filter((a) => a.public)
    .slice(0, 1)
    .map((a) => ({
      id: ulid(s.anchor - 40 * DAY, "install:" + account + ":" + a.slug),
      appSlug: a.slug,
      target: account,
      targetKind: "APP_INSTALLATION_TARGET_KIND_ORG",
      repositorySelection: "APP_REPOSITORY_SELECTION_ALL",
      permissions: [...perms(a.permissions)],
      events: ["change.landed"],
      installedBy: s.p("mara"),
      createdAt: s.iso(s.anchor - 40 * DAY),
      updatedAt: s.iso(s.anchor - 40 * DAY),
    }));
}

const apps = defineService(AppService, {
  listApps: (s, req) => {
    const owner = req.owner || s.org.name;
    s.orgRecord(owner);
    // Apps belong to the org that registered them; the others only have what they installed.
    const rows = owner === s.org.name ? APPS.map((a) => appJson(s, owner, a)) : [];
    const pg = page(rows, req.page, 100);
    return { apps: pg.items, page: { nextPageToken: pg.next } };
  },
  listAppInstallations: (s, req) => {
    const account = req.account || s.org.name;
    s.orgRecord(account);
    const rows = installations(s, account);
    const pg = page(rows, req.page, 100);
    return { installations: pg.items, page: { nextPageToken: pg.next } };
  },
});

// ------------------------------------------------------------------ teams

interface TeamSeed {
  slug: string;
  name: string;
  description: string;
  members: string[];
  repos: Record<string, string>;
  parent?: string;
  secret?: boolean;
  idp?: boolean;
}

const TEAMS: TeamSeed[] = [
  { slug: "platform", name: "Platform", description: "Owns the API, the cells and the release train.", members: ["mara", "dev", "lin"], repos: { api: "maintain", web: "maintain", infra: "maintain" } },
  { slug: "billing", name: "Billing", description: "Invoices, plans and the money paths. Policy requires its approval on src/billing.", members: ["dev", "sam"], repos: { api: "write" }, parent: "platform" },
  { slug: "docs", name: "Docs", description: "Keeps the guides and the reference in step with the code.", members: ["lin"], repos: { api: "write", web: "write" } },
  { slug: "security-review", name: "Security review", description: "Reviews anything that touches auth or secrets.", members: ["mara", "dev"], repos: { api: "write", web: "write", infra: "write" }, secret: true, idp: true },
];

function teamJson(s: FixtureStore, org: string, t: TeamSeed): TeamJson {
  const at = s.iso(s.anchor - 90 * DAY);
  return {
    id: ulid(s.anchor - 90 * DAY, "team:" + org + ":" + t.slug),
    org,
    slug: t.slug,
    name: t.name,
    description: t.description,
    privacy: t.secret ? "TEAM_PRIVACY_SECRET" : "TEAM_PRIVACY_VISIBLE",
    parentTeamId: t.parent ? ulid(s.anchor - 90 * DAY, "team:" + org + ":" + t.parent) : "",
    notificationsEnabled: true,
    policyGroup: "@" + org + "/" + t.slug,
    memberCount: t.members.length,
    childTeamCount: TEAMS.filter((c) => c.parent === t.slug).length,
    repoCount: Object.keys(t.repos).length,
    idpSynced: !!t.idp,
    createdAt: at,
    updatedAt: at,
  };
}

const teams = defineService(TeamService, {
  listRepoTeams: (s, req, ctx) => {
    const repo = s.repo(req.repo);
    const actor = s.caller(ctx.token);
    const role = s.orgRecord(repo.owner).members.get(actor);
    const maintain = repo.owner === s.org.name && TEAMS.some((team) => team.repos[repo.name] === "maintain" && team.members.includes(actor));
    if (role !== "ROLE_OWNER" && role !== "ROLE_ADMIN" && !maintain) {
      throw new BeeError("permission_denied", { reason: "POLICY_DENIED", message: "Repository maintainer access is required." });
    }
    const rows = repo.owner === s.org.name ? TEAMS.flatMap((team) => {
      const role = team.repos[repo.name];
      return role ? [{ teamId: teamJson(s, repo.owner, team).id, repo: { fullName: repo.repo.fullName }, role }] : [];
    }) : [];
    const pg = page(rows, req.page, 100);
    return { grants: pg.items, page: { nextPageToken: pg.next } };
  },
  // Match the team directory's direct and inherited membership response.
  listTeamMembers: (s, req) => {
    const org = s.orgRecord(req.org || s.org.name).org.name;
    const seeds = org === s.org.name ? TEAMS : [];
    const team = seeds.find((t) => t.slug === req.team || teamJson(s, org, t).id === req.team);
    if (!team) throw new BeeError("not_found", { reason: "NOT_FOUND", message: "This team could not be found." });
    const selected = teamJson(s, org, team);
    const all = new Map<string, TeamMemberJson>();
    function add(t: TeamSeed, inherited: boolean) {
      const source = teamJson(s, org, t);
      for (const handle of t.members) {
        const principal = s.p(handle);
        if (!inherited || !all.has(principal.id)) {
          all.set(principal.id, { teamId: selected.id, principal, role: "TEAM_MEMBER_ROLE_MEMBER", inherited, viaTeamId: inherited ? source.id : "" });
        }
      }
      if (req.includeChildTeams) for (const child of seeds.filter((row) => row.parent === t.slug)) add(child, true);
    }
    add(team, false);
    const rows = [...all.values()].filter((m) => !req.role || req.role === "TEAM_MEMBER_ROLE_UNSPECIFIED" || req.role === m.role);
    const pg = page(rows, req.page, 100);
    return { members: pg.items, page: { nextPageToken: pg.next } };
  },
  listTeams: (s, req) => {
    const org = req.org || s.org.name;
    s.orgRecord(org);
    const q = (req.query ?? "").toLowerCase();
    const rows = (org === s.org.name ? TEAMS : [])
      .filter((t) => !req.topLevelOnly || !t.parent)
      .filter((t) => !q || t.slug.includes(q) || t.name.toLowerCase().includes(q))
      .map((t) => teamJson(s, org, t));
    const pg = page(rows, req.page, 100);
    return { teams: pg.items, page: { nextPageToken: pg.next } };
  },
});

// ------------------------------------------------------------------ packages

interface VersionSeed {
  version: string;
  tags: string[];
  daysAgo: number;
  sizeBytes: number;
  downloads: number;
  publisher: string;
  provenance: boolean;
}

interface PackageSeed {
  ecosystem: PackageJson["ecosystem"];
  name: string;
  description: string;
  visibility: PackageJson["visibility"];
  repo: string;
  downloads: number;
  versions: VersionSeed[];
}

const PACKAGES: PackageSeed[] = [
  {
    ecosystem: "PACKAGE_ECOSYSTEM_CONTAINER",
    name: "console",
    description: "The fleet console image: Next.js server and static assets.",
    visibility: "PACKAGE_VISIBILITY_PRIVATE",
    repo: "acme/web",
    downloads: 4180,
    versions: [
      { version: "1.8.2", tags: ["latest", "stable"], daysAgo: 2, sizeBytes: 148_300_000, downloads: 912, publisher: "deps", provenance: true },
      { version: "1.8.1", tags: [], daysAgo: 9, sizeBytes: 147_900_000, downloads: 1544, publisher: "mara", provenance: true },
      { version: "1.8.0", tags: [], daysAgo: 21, sizeBytes: 146_100_000, downloads: 1724, publisher: "mara", provenance: false },
    ],
  },
  {
    ecosystem: "PACKAGE_ECOSYSTEM_NPM",
    name: "@acme/fleet-client",
    description: "Typed client for the Acme fleet API.",
    visibility: "PACKAGE_VISIBILITY_INTERNAL",
    repo: "acme/api",
    downloads: 12_460,
    versions: [
      { version: "0.14.0", tags: ["latest"], daysAgo: 5, sizeBytes: 218_400, downloads: 3120, publisher: "billing", provenance: true },
      { version: "0.13.3", tags: [], daysAgo: 19, sizeBytes: 216_900, downloads: 9340, publisher: "dev", provenance: true },
    ],
  },
  {
    ecosystem: "PACKAGE_ECOSYSTEM_RUBYGEMS",
    name: "acme-telemetry",
    description: "Ruby bindings for the cell telemetry stream.",
    visibility: "PACKAGE_VISIBILITY_PUBLIC",
    repo: "acme/infra",
    downloads: 640,
    versions: [{ version: "2.1.0", tags: ["latest"], daysAgo: 33, sizeBytes: 41_200, downloads: 640, publisher: "lin", provenance: false }],
  },
];

function packageJson(s: FixtureStore, owner: string, p: PackageSeed): PackageJson {
  const newest = p.versions[0] as VersionSeed;
  const oldest = p.versions[p.versions.length - 1] as VersionSeed;
  return {
    id: ulid(s.anchor - (oldest.daysAgo + 10) * DAY, "pkg:" + owner + ":" + p.name),
    owner,
    ownerKind: "PACKAGE_OWNER_KIND_ORG",
    ecosystem: p.ecosystem,
    name: p.name,
    visibility: p.visibility,
    repo: { fullName: p.repo },
    inheritAccessFromRepo: true,
    description: p.description,
    versionCount: p.versions.length,
    downloadCount: String(p.downloads),
    registryUrl: registryUrl(p, owner),
    createdAt: s.iso(s.anchor - (oldest.daysAgo + 10) * DAY),
    updatedAt: s.iso(s.anchor - newest.daysAgo * DAY),
  };
}

function registryUrl(p: PackageSeed, owner: string): string {
  switch (p.ecosystem) {
    case "PACKAGE_ECOSYSTEM_CONTAINER":
      return "registry.bee.example/" + owner + "/" + p.name;
    case "PACKAGE_ECOSYSTEM_NPM":
      return "https://bee.example/npm/" + owner + "/";
    default:
      return "https://bee.example/rubygems/" + owner + "/";
  }
}

function versionJson(s: FixtureStore, owner: string, p: PackageSeed, v: VersionSeed): PackageVersionJson {
  const pkg = packageJson(s, owner, p);
  const at = s.iso(s.anchor - v.daysAgo * DAY);
  return {
    id: ulid(s.anchor - v.daysAgo * DAY, "pkgver:" + owner + ":" + p.name + ":" + v.version),
    packageId: pkg.id,
    version: v.version,
    digest: "sha256:" + digestHex("pkg:" + p.name + ":" + v.version, 32),
    tags: v.tags,
    sizeBytes: String(v.sizeBytes),
    downloadCount: String(v.downloads),
    publisher: s.p(v.publisher),
    hasProvenance: v.provenance,
    sourceChangeset: v.provenance ? objectId("changeset", "pkg-source:" + p.name + ":" + v.version) : undefined,
    createdAt: at,
    updatedAt: at,
  };
}

const packages = defineService(PackageService, {
  listPackages: (s, req, ctx) => {
    const owner = req.owner || s.org.name;
    s.orgRecord(owner);
    const viewer = s.viewer(ctx.token);
    const rows = (owner === s.org.name ? PACKAGES : [])
      .filter((p) => !req.ecosystem || req.ecosystem === "PACKAGE_ECOSYSTEM_UNSPECIFIED" || p.ecosystem === req.ecosystem)
      .filter((p) => !req.repo?.fullName || p.repo === req.repo.fullName)
      // A signed-out visitor sees only what is public; a member sees the rest through the repo.
      .filter((p) => p.visibility === "PACKAGE_VISIBILITY_PUBLIC" || (viewer !== undefined && s.inOrg(viewer, owner)))
      .map((p) => packageJson(s, owner, p));
    const pg = page(rows, req.page, 100);
    return { packages: pg.items, page: { nextPageToken: pg.next } };
  },
  listPackageVersions: (s, req) => {
    const owner = req.owner || s.org.name;
    const p = PACKAGES.find((x) => owner === s.org.name && x.name === req.name && (!req.ecosystem || req.ecosystem === "PACKAGE_ECOSYSTEM_UNSPECIFIED" || x.ecosystem === req.ecosystem));
    if (!p) throw new BeeError("not_found", { reason: "PACKAGE_NOT_FOUND", message: "no package " + (req.name ?? "") + " owned by " + owner });
    const rows = p.versions.filter((v) => !req.taggedOnly || v.tags.length > 0).map((v) => versionJson(s, owner, p, v));
    const pg = page(rows, req.page, 50);
    return { versions: pg.items, page: { nextPageToken: pg.next } };
  },
});

// ------------------------------------------------------------------ pages

/** acme/api publishes its API reference; every other repo has no site yet. */
const PAGES_REPO = "acme/api";

function deployments(s: FixtureStore): PagesDeploymentJson[] {
  const row = (n: number, state: PagesDeploymentJson["state"], minutesAgo: number, by: string, ms: number, error?: string): PagesDeploymentJson => ({
    id: ulid(s.anchor - minutesAgo * MINUTE, "pages:dep:" + n),
    repo: { fullName: PAGES_REPO },
    state,
    buildType: "PAGES_BUILD_TYPE_REF",
    ref: "main",
    changeset: objectId("changeset", "pages-dep:" + n),
    pageUrl: "https://acme.pages.bee.example/api/",
    errorMessage: error ?? "",
    durationMs: String(ms),
    creator: s.p(by),
    createdAt: s.iso(s.anchor - minutesAgo * MINUTE),
    updatedAt: s.iso(s.anchor - minutesAgo * MINUTE + ms),
  });
  return [
    row(4, "PAGES_DEPLOYMENT_STATE_SUCCEEDED", 140, "docs", 41_000),
    row(3, "PAGES_DEPLOYMENT_STATE_FAILED", 60 * 26, "docs", 12_000, "docs/guide/cells.md links to a page that does not exist"),
    row(2, "PAGES_DEPLOYMENT_STATE_SUCCEEDED", 60 * 24 * 6, "lin", 38_000),
    row(1, "PAGES_DEPLOYMENT_STATE_SUCCEEDED", 60 * 24 * 19, "lin", 52_000),
  ];
}

function noSite(name: string | undefined): BeeError {
  return new BeeError("not_found", { reason: "PAGES_SITE_NOT_FOUND", message: (name ?? "this repository") + " has no Pages site" });
}

const pages = defineService(PagesService, {
  getPagesSite: (s, req) => {
    const repo = s.repo(req.repo);
    if (repo.repo.fullName !== PAGES_REPO) throw noSite(repo.repo.fullName);
    const current = deployments(s)[0] as PagesDeploymentJson;
    return {
      repo: { fullName: repo.repo.fullName },
      url: "https://acme.pages.bee.example/api/",
      status: "PAGES_SITE_STATUS_BUILT",
      buildType: "PAGES_BUILD_TYPE_REF",
      source: { ref: "main", path: "/docs" },
      enforceHttps: true,
      access: "PAGES_ACCESS_PUBLIC",
      currentDeploymentId: current.id,
      servedTree: objectId("tree", "pages-serve"),
      createdAt: s.iso(s.anchor - 60 * DAY),
      updatedAt: current.createdAt,
    };
  },
  listPagesDeployments: (s, req) => {
    const repo = s.repo(req.repo);
    if (repo.repo.fullName !== PAGES_REPO) throw noSite(repo.repo.fullName);
    const pg = page(deployments(s), req.page, 20);
    return { deployments: pg.items, page: { nextPageToken: pg.next } };
  },
});

export const HUB_SERVICES: ServiceDef[] = [apps, teams, packages, pages];
