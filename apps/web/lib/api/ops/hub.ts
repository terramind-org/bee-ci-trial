/** Packages, Pages, code search, teams and apps (packages, pages, search, teams, apps .proto). */
import { AppService } from "../../gen/bee/v1/apps_pb";
import { PackageService } from "../../gen/bee/v1/packages_pb";
import { PagesService } from "../../gen/bee/v1/pages_pb";
import { SearchService } from "../../gen/bee/v1/search_pb";
import { TeamService } from "../../gen/bee/v1/teams_pb";
import { principalOf } from "../../format";
import type { Rpc } from "../rpc";
import { namer } from "./agents";
import { BeeError, type CallOptions } from "../transport";
import type * as T from "../types";
import { clean, list, mapLimit, num, pageOf, pass, str, wire, type PartialOps } from "./shared";

async function listPackages(rpc: Rpc, r: T.ListPackagesRequest, opts?: CallOptions): Promise<T.ListPackagesResponse> {
  const res = await rpc.call(PackageService.method.listPackages, clean({ owner: r.owner, repo: r.repo, page: { pageSize: 100 } }), opts);
  const packages = list(res.packages);
  const latest = await mapLimit(packages, 6, async (p) => {
    const v = await rpc.call(PackageService.method.listPackageVersions, { owner: str(p.owner) || r.owner, ecosystem: p.ecosystem, name: str(p.name), page: { pageSize: 20 } }, opts);
    // Newest first by creation time; the server's order is not part of the contract.
    return list(v.versions).sort((a, b) => Date.parse(str(b.createdAt)) - Date.parse(str(a.createdAt)))[0];
  });
  const name = await namer(rpc, latest.map((v) => v?.publisher), opts);
  return { packages: packages.map((p, i) => { const v = latest[i]; return { package: p, latest: v && { ...v, publisher: name(principalOf(v.publisher)) } }; }) };
}

async function getPages(rpc: Rpc, r: T.GetPagesRequest, opts?: CallOptions): Promise<T.PagesView> {
  let site: T.PagesSite | undefined;
  try {
    site = await rpc.call(PagesService.method.getPagesSite, { repo: r.repo }, opts);
  } catch (err) {
    // No site yet is an answer, not an error.
    if (err instanceof BeeError && err.code === "not_found") return { deployments: [] };
    throw err;
  }
  const d = await rpc.call(PagesService.method.listPagesDeployments, { repo: r.repo, page: { pageSize: 20 } }, opts);
  return { site, deployments: list(d.deployments) };
}

async function listApps(rpc: Rpc, r: T.ListAppsRequest, opts?: CallOptions): Promise<T.ListAppsResponse> {
  const [apps, installs] = await Promise.all([
    rpc.call(AppService.method.listApps, { owner: r.org, page: { pageSize: 100 } }, opts),
    rpc.call(AppService.method.listAppInstallations, { account: r.org, page: { pageSize: 100 } }, opts),
  ]);
  return { apps: list(apps.apps), installations: list(installs.installations) };
}

export const hubOps = {
  PackageService: {
    ListPackages: {
      wire: (r) => wire(PackageService.method.listPackages, clean({ owner: r.owner, repo: r.repo })),
      run: (rpc, r, opts) => listPackages(rpc, r, opts),
    },
  },
  PagesService: {
    GetPages: {
      wire: (r) => wire(PagesService.method.getPagesSite, { repo: r.repo }),
      run: (rpc, r, opts) => getPages(rpc, r, opts),
    },
  },
  SearchService: {
    SearchCode: {
      wire: (r) => wire(SearchService.method.searchCode, clean({ query: r.query, page: r.page })),
      run: async (rpc, r, opts) => {
        const res = await rpc.call(SearchService.method.searchCode, clean({ query: r.query, contextLines: 1, page: r.page }), opts);
        return { hits: list(res.hits), totalCount: String(num(res.totalCount)), incomplete: !!res.incomplete };
      },
    },
  },
  TeamService: {
    ListRepoTeams: {
      wire: (r) => wire(TeamService.method.listRepoTeams, r),
      run: async (rpc, r, opts) => {
        const res = await rpc.call(TeamService.method.listRepoTeams, clean(r), opts);
        return {
          grants: list(res.grants).map((grant) => ({
            teamId: str(grant.teamId), role: str(grant.role), inherited: !!grant.inherited, viaTeamId: str(grant.viaTeamId),
          })),
          page: pageOf(res.page),
        };
      },
    },
    ListTeamMembers: pass(TeamService.method.listTeamMembers),
    ListTeams: {
      wire: (r) => wire(TeamService.method.listTeams, { org: r.org }),
      run: async (rpc, r, opts) => {
        const teams: T.ListTeamsResponse["teams"] = [];
        let pageToken = "";
        do {
          const page = await rpc.call(TeamService.method.listTeams, { org: r.org, page: { pageSize: 200, pageToken } }, opts);
          teams.push(...list(page.teams));
          pageToken = str(page.page?.nextPageToken);
        } while (pageToken);
        return { teams };
      },
    },
  },
  AppService: {
    ListApps: {
      wire: (r) => wire(AppService.method.listApps, { owner: r.org }),
      run: (rpc, r, opts) => listApps(rpc, r, opts),
    },
  },
} satisfies PartialOps<"PackageService" | "PagesService" | "SearchService" | "TeamService" | "AppService">;
