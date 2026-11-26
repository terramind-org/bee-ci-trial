/**
 * Server-side client selection. Only import this from server components, route handlers and
 * server actions.
 *
 *   http (default)   connect-web to BEE_API_URL (default http://127.0.0.1:7420), with the
 *                    session cookie's token, or BEE_TOKEN when there is no session
 *   fixture          the in-memory demo world, only when BEE_ADAPTER=fixture
 */
import { cookies } from "next/headers";
import { cache } from "react";
import { AuthService } from "../gen/bee/v1/auth_pb";
import { ReleaseService } from "../gen/bee/v1/releases_pb";
import { UserService } from "../gen/bee/v1/users_pb";
import { ObjectService } from "../gen/bee/v1/vcs_pb";
import { isRepoFullName } from "../routes";
import { createClient, type BeeClient } from "./client";
import { DEFAULT_API_URL, FIXTURE_ORG, FIXTURE_REPO, REPO_COOKIE, TOKEN_COOKIE } from "./config";
import { fixtureRpc, getFixtureStore } from "./fixture";
import { httpRpc } from "./http";
import type { Rpc } from "./rpc";
import { BeeError } from "./transport";
import type { Principal, Repo, RepoRef } from "./types";

export function adapterKind(): "http" | "fixture" {
  return process.env.BEE_ADAPTER === "fixture" ? "fixture" : "http";
}

/** The bee server the http adapter talks to (no trailing slash). */
export function apiUrl(): string {
  return (process.env.BEE_API_URL || DEFAULT_API_URL).replace(/\/+$/, "");
}

export function rpcFor(token: string | undefined): Rpc {
  return adapterKind() === "http" ? httpRpc({ baseUrl: apiUrl(), token }) : fixtureRpc(getFixtureStore(), token);
}

/**
 * The bytes of a fixture evidence asset, for the evidence route: the signed URL is checked and
 * the caller must still be able to read the repo, as the real server does when a URL is redeemed.
 */
export function fixtureEvidence(token: string | undefined, segments: string[], query: URLSearchParams): { contentType: string; bytes: Uint8Array; digest: string } {
  const store = getFixtureStore();
  return store.briefs.redeem(store, segments, query, store.viewer(token));
}

/** A PUT to a fixture upload URL, the stand-in for the server's signed upload route. */
export function acceptFixtureUpload(token: string | undefined, segments: string[], query: URLSearchParams, bytes: Uint8Array): void {
  const store = getFixtureStore();
  store.briefs.acceptUpload(store, segments, query, bytes, store.viewer(token));
}

/**
 * A PUT to a fixture post-media upload URL, the stand-in for the server's signed `/posts/…` route
 * (app/api/post-media). Added beside `acceptFixtureUpload` so the route does not import the fixture.
 */
export function acceptFixturePostUpload(mediaId: string, bytes: Uint8Array): void {
  getFixtureStore().posts.receive(mediaId, bytes);
}

/** The session cookie's token; in development a server-wide BEE_TOKEN stands in for it. */
export async function sessionToken(): Promise<string | undefined> {
  return (await cookies()).get(TOKEN_COOKIE)?.value || process.env.BEE_TOKEN || undefined;
}

export async function serverClient(): Promise<BeeClient> {
  return createClient(rpcFor(await sessionToken()));
}

const NAME = /^[\w.-]+\/[\w.-]+$/;

/** Route params of every page under `/{owner}/{repo}`. */
export interface RepoParams {
  owner: string;
  repo: string;
}

/** The `/{owner}/{repo}` a request is rendering, set from route params. */
export interface RequestScope {
  owner: string;
  repo: string;
}

// One mutable box per request (React `cache` is per request on the server).
const scopeBox = cache((): { value: RequestScope | null } => ({ value: null }));

/** Records the repo in the URL for this request; `currentScope()` then answers with it. */
export function setRequestScope(scope: RequestScope): void {
  scopeBox().value = scope;
}

export function getRequestScope(): RequestScope | null {
  return scopeBox().value;
}

/**
 * For pages under `/{owner}/{repo}`: reads the params, records the scope, and returns the repo.
 * Layouts and pages may render in parallel, so every page calls this itself rather than
 * relying on the layout having run first.
 */
export async function scopeFromParams(params: Promise<RepoParams> | RepoParams): Promise<{ fullName: string; ref: RepoRef; owner: string }> {
  const { owner, repo } = await params;
  const o = decodeURIComponent(owner);
  const r = decodeURIComponent(repo);
  setRequestScope({ owner: o, repo: r });
  const fullName = o + "/" + r;
  return { fullName, ref: { fullName }, owner: o };
}

/**
 * Global pages (no repo in the URL) fall back to the repo picker's cookie, else BEE_REPO /
 * BEE_ORG, else the first repo of the caller's first org.
 */
const fallbackScope = cache(async (): Promise<{ org: string; repo: string }> => {
  const raw = (await cookies()).get(REPO_COOKIE)?.value;
  if (raw && NAME.test(raw) && isRepoFullName(raw)) return { org: raw.split("/")[0] as string, repo: raw };
  if (adapterKind() === "fixture") return { org: FIXTURE_ORG, repo: FIXTURE_REPO };
  const envRepo = process.env.BEE_REPO;
  if (envRepo && NAME.test(envRepo)) return { org: envRepo.split("/")[0] as string, repo: envRepo };
  const api = await serverClient();
  let org = process.env.BEE_ORG || "";
  try {
    if (!org) org = (await api.org.listOrgs({})).orgs[0]?.name ?? "";
    const first = org ? (await api.repo.listRepos({ org })).repos[0]?.fullName : undefined;
    return { org, repo: first ?? (org ? org + "/" : "") };
  } catch (err) {
    // Signed out or unreachable: the shell decides what to show; pages render their own error.
    if (err instanceof BeeError) return { org, repo: org ? org + "/" : "" };
    throw err;
  }
});

/**
 * The org and repo a screen is scoped to: the repo in the URL when there is one (org = its
 * owner), otherwise the fallback above.
 */
export async function currentScope(): Promise<{ org: string; repo: string }> {
  const s = getRequestScope();
  if (s) return { org: s.owner, repo: s.owner + "/" + s.repo };
  return fallbackScope();
}

export async function currentRepo(): Promise<{ fullName: string; ref: RepoRef }> {
  const { repo } = await currentScope();
  return { fullName: repo, ref: { fullName: repo } };
}

export async function currentOrg(): Promise<string> {
  return (await currentScope()).org;
}

/**
 * Who is signed in, once per request. `error` is set when the server answered something other
 * than a principal: UNAUTHENTICATED for a rejected token, UNAVAILABLE when unreachable.
 */
export const sessionState = cache(async (): Promise<{ me: Principal | null; error: BeeError | null }> => {
  // Without a token a real server is not asked: that is simply signed out. The fixture world is
  // asked anyway and, like the server's anonymous gate, answers a token-less WhoAmI with
  // UNAUTHENTICATED, so `me` is null there too (with that error set).
  if (adapterKind() === "http" && !(await sessionToken())) return { me: null, error: null };
  try {
    return { me: (await (await serverClient()).auth.whoAmI({})).principal ?? null, error: null };
  } catch (err) {
    if (err instanceof BeeError) return { me: null, error: err };
    throw err;
  }
});

/** Who is signed in, once per request; null when signed out, rejected or unreachable. */
export async function currentPrincipal(): Promise<Principal | null> {
  return (await sessionState()).me;
}

/** The repo for a `/{owner}/{repo}` request, once per request; null when it is not readable. */
export const loadRepo = cache(async (fullName: string): Promise<Repo | null> => {
  try {
    return await (await serverClient()).repo.getRepo({ repo: { fullName } });
  } catch (err) {
    // NOT_FOUND, PERMISSION_DENIED and UNAUTHENTICATED all read as "not here": existence is never leaked.
    if (err instanceof BeeError && (err.code === "not_found" || err.code === "permission_denied" || err.code === "unauthenticated")) return null;
    throw err;
  }
});

/**
 * Whether the caller may write to the repo (commit), once per request. Signed out never may.
 * AuthService.CheckAccess answers it; a server without that RPC falls back to the token's
 * effective scope from WhoAmI.
 */
export const canWriteRepo = cache(async (fullName: string): Promise<boolean> => {
  const token = await sessionToken();
  if (adapterKind() === "http" && !token) return false;
  try {
    const res = await rpcFor(token).call(AuthService.method.checkAccess, { repo: { fullName }, action: "commit", paths: [""] });
    return res.allowed?.[0] === true;
  } catch (err) {
    if (!(err instanceof BeeError) || err.code !== "unimplemented") return false;
  }
  try {
    const who = await (await serverClient()).auth.whoAmI({});
    const actions = who.effectiveScope?.actions ?? [];
    return actions.includes("commit") || actions.includes("admin");
  } catch {
    return false;
  }
});

/** The Agents lifecycle controls check the management grant; the server rechecks role and branch on writes. */
export const canManageRepo = cache(async (fullName: string): Promise<boolean> => {
  const token = await sessionToken();
  if (!token) return false;
  try {
    const access = await rpcFor(token).call(AuthService.method.checkAccess, { repo: { fullName }, action: "manage_settings", paths: [""] });
    return access.allowed?.[0] === true;
  } catch (error) {
    if (error instanceof BeeError) return false;
    throw error;
  }
});

/*
 * Stars and follows for the repo header and the star/follow buttons. These call UserService
 * directly because the typed UI methods for them (`starRepo`, `followUser`, …) arrive with the
 * community data methods in lib/api/methods.ts (foundation-data); once they land, these become
 * thin wrappers over those methods.
 */

export interface StarState {
  starred: boolean;
  count?: number;
  /** Why starring cannot work here, when the server does not offer it. */
  unavailable?: string;
}

/**
 * The stargazer count (ListRepoStargazers) and, signed in, whether the caller starred the repo
 * (IsRepoStarred), once per request. UNIMPLEMENTED disables starring and says so; any other
 * failure just leaves the count off.
 */
export const repoStarState = cache(async (fullName: string, signedIn: boolean): Promise<StarState> => {
  const rpc = rpcFor(await sessionToken());
  const repo = { fullName };
  const [count, starred] = await Promise.allSettled([
    rpc.call(UserService.method.listRepoStargazers, { repo, page: { pageSize: 1 } }).then((r) => Number(r.totalCount ?? 0)),
    signedIn ? rpc.call(UserService.method.isRepoStarred, { repo }).then((r) => r.starred === true) : Promise.resolve(false),
  ]);
  const unimplemented = [count, starred].some((r) => r.status === "rejected" && r.reason instanceof BeeError && r.reason.code === "unimplemented");
  if (unimplemented) return { starred: false, unavailable: "Stars are not available on this server (UNIMPLEMENTED)" };
  return {
    starred: starred.status === "fulfilled" ? starred.value : false,
    count: count.status === "fulfilled" && Number.isFinite(count.value) ? count.value : undefined,
  };
});

/** UserService.StarRepo or UnstarRepo as the session's principal. */
export async function setStar(fullName: string, on: boolean): Promise<void> {
  const rpc = rpcFor(await sessionToken());
  const req = { repo: { fullName } };
  await (on ? rpc.call(UserService.method.starRepo, req) : rpc.call(UserService.method.unstarRepo, req));
}

/** UserService.FollowUser or UnfollowUser as the session's principal. */
export async function setFollowing(login: string, on: boolean): Promise<void> {
  const rpc = rpcFor(await sessionToken());
  const req = { login };
  await (on ? rpc.call(UserService.method.followUser, req) : rpc.call(UserService.method.unfollowUser, req));
}

/**
 * ReleaseService.GetSourceArchiveUrl as the session's principal, resolved to an address a browser
 * can open. There is no typed UI method for it yet (the releases screen lists archives only), so
 * the repo home's download buttons come through here, like stars and follows above.
 */
export async function sourceArchiveUrl(fullName: string, format: "tar.gz" | "zip"): Promise<string> {
  const rpc = rpcFor(await sessionToken());
  const res = await rpc.call(ReleaseService.method.getSourceArchiveUrl, {
    repo: { fullName },
    format: format === "zip" ? "SOURCE_ARCHIVE_FORMAT_ZIP" : "SOURCE_ARCHIVE_FORMAT_TAR_GZ",
  });
  const url = res.url ?? "";
  if (/^https?:\/\//.test(url)) return url;
  // A signed path is relative to the server's public address, which only the server knows.
  const info = await rpc.call(ObjectService.method.getServerInfo, {});
  return (info.connectBaseUrl ?? "").replace(/\/+$/, "") + "/" + url.replace(/^\/+/, "");
}

export type Edition = "cloud" | "enterprise";

const isEdition = (v: string | undefined): v is Edition => v === "cloud" || v === "enterprise";

/**
 * Which edition of the forge this server is, once per request. BEE_EDITION (development
 * only) wins; otherwise GetServerInfo answers, and the fixture world answers it from
 * BEE_FIXTURE_EDITION (default cloud). An unreachable server or an empty value is the
 * self-hosted edition, the safer surface.
 */
export const edition = cache(async (): Promise<Edition> => {
  const forced = process.env.BEE_EDITION;
  if (isEdition(forced)) return forced;
  try {
    const info = await rpcFor(undefined).call(ObjectService.method.getServerInfo, {});
    return isEdition(info.edition) ? info.edition : "enterprise";
  } catch {
    return "enterprise";
  }
});
