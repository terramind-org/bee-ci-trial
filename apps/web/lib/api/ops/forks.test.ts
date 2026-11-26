import { describe, expect, it } from "vitest";
import { forkStatus } from "@/components/repo-header-data";
import { toCli } from "../../copyas";
import { createClient } from "../client";
import { FixtureStore, fixtureRpc } from "../fixture";
import { wireOf } from "../ops";

const repo = { fullName: "acme/api" };
const fixture = () => {
  const store = new FixtureStore({ live: false });
  const api = createClient(fixtureRpc(store, "demo-token-1"));
  return { store, api };
};

describe("repository forks", () => {
  it("copies the upstream and lists fork IDs through the typed client, including personal destinations", async () => {
    const { api } = fixture();
    const source = await api.repo.getRepo({ repo });
    const fork = await api.repo.forkRepo({ repo, name: "api-sandbox", defaultRefOnly: true });
    const viewer = await api.user.getViewer({});
    expect(fork.fullName).toBe(viewer.login + "/api-sandbox");
    expect((await api.search.searchRepos({ query: "user:" + viewer.login + " is:fork" })).hits.map((h) => h.fullName)).toContain(fork.fullName);
    expect(fork.forkParentId).toBe(source.id);
    expect(fork.id).not.toBe(source.id);
    expect(await api.ref.listRefs({ repo: { id: fork.id } })).toMatchObject({ refs: [{ name: "main" }] });
    expect(await api.repo.forkRepo({ repo })).toMatchObject({ id: fork.id });
    const orgFork = await api.repo.forkRepo({ repo, org: "acme", name: "api-org-fork" });
    expect(orgFork).toMatchObject({ fullName: "acme/api-org-fork", forkParentId: source.id });
    const first = await api.repo.listForks({ repo, page: { pageSize: 1 } });
    expect(first.forks).toHaveLength(1);
    const next = first.page?.nextPageToken;
    expect(next).toBeTruthy();
    const second = await api.repo.listForks({ repo, page: { pageSize: 1, pageToken: next } });
    expect([...first.forks, ...second.forks].map((r) => r.id).sort()).toEqual([fork.id, orgFork.id].sort());
    expect(Number((await api.repo.getRepo({ repo })).forks)).toBe(Number(source.forks) + 2);
    expect(await api.repo.getRepo({ repo: { id: fork.id } })).toMatchObject({ id: fork.id, forkParentId: source.id });
  });

  it("detects a behind fork, syncs its branch, and leaves an ahead fork alone", async () => {
    const { store, api } = fixture();
    const fork = await api.repo.forkRepo({ repo, name: "api-sync", defaultRefOnly: true });
    const ref = { id: fork.id };
    expect(await forkStatus(api, fork)).toEqual({ fullName: repo.fullName, behind: false });
    const parent = store.repo(repo);
    const newHead = store.makeCommit(parent.name, store.tip(parent), { "fork-sync.md": "Upstream\n" }, "mara", store.now(), "Update upstream", "fork-test-update");
    store.advanceMain(parent, newHead, "mara");

    expect(await forkStatus(api, fork)).toEqual({ fullName: repo.fullName, behind: true });
    expect(await api.repo.syncFork({ repo: ref })).toMatchObject({ ref: { name: "main", target: newHead.id }, changesetsPulled: "1" });
    expect(await api.readText({ repo: ref, at: { expr: "main" }, path: "Zm9yay1zeW5jLm1k" })).toMatchObject({ text: "Upstream\n" });
    expect(await forkStatus(api, fork)).toEqual({ fullName: repo.fullName, behind: false });

    const branch = store.repo(ref);
    const ahead = store.makeCommit(branch.name, newHead, { "local.md": "Fork\n" }, "mara", store.now(), "Change the fork", "fork-test-ahead");
    store.advanceMain(branch, ahead, "mara");
    expect(await forkStatus(api, fork)).toEqual({ fullName: repo.fullName, behind: false });
    expect(await api.repo.syncFork({ repo: ref })).toMatchObject({ ref: { target: ahead.id }, changesetsPulled: "0" });
  });

  it("keeps private forks private and rejects anonymous writes", async () => {
    const { store, api } = fixture();
    const anonymous = createClient(fixtureRpc(store));
    const source = await api.repo.createRepo({ org: "acme", name: "private-upstream" });
    const ref = { fullName: source.fullName };
    const fork = await api.repo.forkRepo({ repo: ref, name: "private-fork" });
    expect(fork.visibility).toBe("VISIBILITY_PRIVATE");
    await expect(anonymous.repo.getRepo({ repo: { id: fork.id } })).rejects.toMatchObject({ reason: "REPO_NOT_FOUND" });
    await expect(anonymous.repo.forkRepo({ repo })).rejects.toMatchObject({ reason: "UNAUTHENTICATED" });
  });

  it("names the real fork RPCs in Copy as", () => {
    const request = { repo, org: "acme", name: "copy" };
    expect(wireOf("RepoService", "ForkRepo", request)).toMatchObject({ service: "RepoService", method: "ForkRepo", request });
    expect(toCli({ service: "RepoService", method: "ForkRepo", request })).toBe("bee repo fork-repo --repo acme/api --org acme --name copy --json");
    expect(toCli({ service: "RepoService", method: "SyncFork", request: { repo } })).toBe("bee repo sync-fork --repo acme/api --json");
    expect(toCli({ service: "RepoService", method: "ListForks", request: { repo } })).toBe("bee repo list-forks --repo acme/api --json");
  });
});
