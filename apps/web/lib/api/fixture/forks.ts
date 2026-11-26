import type { RepoService } from "../../gen/bee/v1/forge_pb";
import type { QueryService } from "../../gen/bee/v1/vcs_pb";
import { BeeError } from "../transport";
import type { Impl } from "./router";
import { ulid } from "./seed";
import { loginOf } from "./social";
import type { Commit } from "./store";
import { invalid, page } from "./util";

/** The fixture models a linear history per ref. Fork refs copy the tips, then move independently. */
function history(tip: Commit | undefined): Commit[] {
  const commits: Commit[] = [];
  for (let c: Commit | null = tip ?? null; c; c = c.parent) commits.push(c);
  return commits;
}

export const forkRepo: NonNullable<Impl<typeof RepoService>["forkRepo"]> = (s, req, ctx) => {
  const actor = s.caller(ctx.token);
  const source = s.repo(req.repo);
  const name = req.name || source.repo.name;
  if (!/^[a-z0-9][a-z0-9._-]{0,99}$/.test(name)) throw invalid("Invalid repository name", "INVALID_NAME");
  const login = loginOf(s, actor);
  const org = req.org ? s.orgRecord(req.org) : s.orgs.get(login);
  if (org && !s.inOrg(actor, org.org.name)) throw new BeeError("permission_denied", { reason: "PERMISSION_DENIED", message: "You cannot create a repository in " + org.org.name });
  // The service returns the existing member of this fork network in the destination org.
  const existing = [...s.repos.values()].find((r) => r.owner === (org?.org.name ?? login) && r.repo.forkParentId === source.repo.id);
  if (existing) return s.repoView(existing);
  const fullName = (org?.org.name ?? login) + "/" + name;
  if (s.lookup({ fullName })) throw new BeeError("already_exists", { reason: "REPO_EXISTS", message: fullName + " already exists" });

  const at = s.now();
  const destination = org ?? {
    org: { id: ulid(at, "org:" + login), name: login, display: s.p(actor).display, createdAt: s.iso(at) },
    description: "", website: "", location: "", email: "", members: new Map([[actor, "ROLE_OWNER" as const]]),
  };
  if (!org) s.orgs.set(login, destination);
  const fork = s.addRepo(destination, name, {
    visibility: source.repo.visibility === "VISIBILITY_PUBLIC" ? "VISIBILITY_PUBLIC" : "VISIBILITY_PRIVATE",
    description: source.repo.description,
    topics: source.topics,
    license: source.license,
    homepage: source.homepage,
    restricted: [...source.restricted],
    createdAt: at,
    idAt: at,
  });
  fork.repo.defaultRef = source.repo.defaultRef;
  fork.repo.forkParentId = source.repo.id;
  for (const [ref, value] of source.refs) {
    if (!req.defaultRefOnly || ref === source.repo.defaultRef) fork.refs.set(ref, { target: value.target, version: 1, log: [] });
  }
  fork.main = history(fork.refs.get(fork.repo.defaultRef)?.target).reverse();
  s.forks.set(source.name, [...(s.forks.get(source.name) ?? []), at]);
  return s.repoView(fork);
};

export const listForks: NonNullable<Impl<typeof RepoService>["listForks"]> = (s, req, ctx) => {
  const source = s.repo(req.repo);
  const forks = s.readableRepos(s.viewer(ctx.token)).filter((r) => r.repo.forkParentId === source.repo.id);
  const result = page(forks.sort((a, b) => a.repo.fullName.localeCompare(b.repo.fullName)), req.page);
  return { forks: result.items.map((r) => s.repoView(r)), page: { nextPageToken: result.next } };
};

export const syncFork: NonNullable<Impl<typeof RepoService>["syncFork"]> = (s, req, ctx) => {
  const actor = s.caller(ctx.token);
  const fork = s.repo(req.repo);
  if (!s.inOrg(actor, fork.owner)) throw new BeeError("permission_denied", { reason: "PERMISSION_DENIED", message: "You cannot update " + fork.repo.fullName });
  if (fork.archived) throw new BeeError("failed_precondition", { reason: "ARCHIVED", message: fork.repo.fullName + " is archived" });
  if (!fork.repo.forkParentId) throw invalid(fork.repo.fullName + " is not a fork", "INVALID_STATE");
  const source = s.repo({ id: fork.repo.forkParentId });
  if (!s.canRead(actor, source)) throw new BeeError("not_found", { reason: "REPO_NOT_FOUND", message: "Upstream repository is unavailable" });

  const name = req.ref || fork.repo.defaultRef;
  const upstreamRef = name === fork.repo.defaultRef ? source.repo.defaultRef : name;
  const upstream = source.refs.get(upstreamRef)?.target;
  if (!upstream) throw new BeeError("not_found", { reason: "REF_NOT_FOUND", message: "No upstream ref " + upstreamRef });
  const current = fork.refs.get(name);
  const local = history(current?.target);
  const upstreamHistory = history(upstream);
  if (local.includes(upstream) && current) {
    return { ref: { name, target: current.target.id, version: String(current.version) }, changesetsPulled: "0" };
  }

  const upstreamSet = new Set(upstreamHistory);
  const common = local.findIndex((c) => upstreamSet.has(c));
  const stack = (common < 0 ? local : local.slice(0, common)).reverse();
  // Check the full replay before changing the ref. The fixture stores files as whole strings.
  const snapshot = new Map(upstream.files);
  const patches = stack.map((c) => c.changed.map((path) => {
    const before = c.parent?.files.get(path);
    const after = c.files.get(path);
    if (snapshot.get(path) !== before && snapshot.get(path) !== after) {
      throw new BeeError("failed_precondition", { reason: "MERGE_CONFLICT", message: "Upstream and the fork both changed " + path });
    }
    if (after === undefined) snapshot.delete(path);
    else snapshot.set(path, after);
    return [path, after ?? null] as const;
  }));
  let target = upstream;
  for (const [i, commit] of stack.entries()) {
    target = s.makeCommit(fork.name, target, Object.fromEntries(patches[i] ?? []), commit.author, commit.time, commit.message, "sync:" + commit.idText + ":" + target.idText);
    target.changeId = commit.changeId;
  }
  const version = (current?.version ?? 0) + 1;
  fork.refs.set(name, {
    target, version,
    log: [...(current?.log ?? []), { version: String(version), old: current?.target.id, new: target.id, principal: s.p(actor).id, time: s.iso(s.now()), landed: [] }],
  });
  if (name === fork.repo.defaultRef) fork.main = history(target).reverse();
  s.emit(fork, "fork.synced", actor, {});
  const known = new Set(local);
  return { ref: { name, target: target.id, version: String(version) }, changesetsPulled: String(upstreamHistory.filter((c) => !known.has(c)).length) };
};

export const isAncestor: NonNullable<Impl<typeof QueryService>["isAncestor"]> = (s, req) => {
  const repo = s.repo(req.repo);
  const descendant = s.resolve(repo, req.descendant?.expr ?? "");
  const ancestor = s.resolve(repo, req.ancestor?.expr ?? "");
  return { isAncestor: history(descendant).includes(ancestor) };
};
