/**
 * Seeds a fresh `bee-server --dev` through its public API, for the real-server e2e run.
 *
 *   bun e2e/seed.ts <data-dir>        reads <data-dir>/credentials (endpoint, root token)
 *
 * Everything goes through the generated bee.v1 clients, as any agent or CLI would: agents and
 * their tokens, history landed on main, change requests with revisions, reviews, threads and a
 * suggestion, the merge queue, leases, issues on a board, CI runs executed by a runner speaking
 * the job protocol, releases, security alerts, a package, a Pages site, a team, an app and a
 * second repo. The ids and titles the specs need are written to e2e/.seed.json.
 */
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { gzipSync } from "node:zlib";
import { createHash } from "node:crypto";
import type { ObjectId } from "../lib/gen/bee/v1/common_pb";
import { SubjectKind } from "../lib/gen/bee/v1/common_pb";
import { ConnectError, Code } from "@connectrpc/connect";
import { LeaseMode, RepoRole, Role, Verdict, Visibility, type ChangeRequest } from "../lib/gen/bee/v1/forge_pb";
import { PagesBuildType } from "../lib/gen/bee/v1/pages_pb";
import { PostMediaKind, PostState } from "../lib/gen/bee/v1/posts_pb";
import { ProjectFieldKind } from "../lib/gen/bee/v1/projects_pb";
import { RunnerArch, RunnerOs, RunnerScopeKind, WorkflowConclusion, WorkflowStatus, type RunnerJobAssignment } from "../lib/gen/bee/v1/workflows_pb";
import * as C from "./seed/content";
import * as D from "./seed/discovery";
import { seedBriefs } from "./seed/briefs";
import { flatPng } from "./seed/png";
import { commit, connect, enc, idText, log, repoRef, tip, type Api, type Commit } from "./seed/lib";

const dataDir = process.argv[2];
if (!dataDir) throw new Error("usage: bun e2e/seed.ts <data-dir>");
const cred = readFileSync(join(dataDir, "credentials"), "utf8");
const endpoint = /endpoint = "(.*)"/.exec(cred)?.[1] ?? "";
const rootToken = /^token = "(.*)"/m.exec(cred)?.[1] ?? "";
// `--dev` also writes a second human, `reviewer`, with a token of its own.
const reviewerId = /^reviewer = "(.*)"/m.exec(cred)?.[1] ?? "";
const reviewerToken = /^reviewer_token = "(.*)"/m.exec(cred)?.[1] ?? "";

// The discovery data set (second org, public repos, topics, logins) lives in ./seed/discovery so
// a spec can import it without starting the seeder: `import { PUBLIC_REPOS } from "./seed/discovery"`.
import { LOGINS } from "./seed/discovery";
export { DISCOVERY_ORG, PUBLIC_REPOS, PRIVATE_REPO, DISCOVERY_TOPICS, BUSIEST_TOPIC, LOGINS, EXPECTED, STARS, FOLLOWS } from "./seed/discovery";

const ORG = "dev";
const REPO = "dev/playground";
const DAY = 86_400;
const NOW = Math.floor(Date.now() / 1000);
const T0 = NOW - 6 * DAY;
const HUMAN = "dev <dev@bee.local>";
const CLAUDE = "claude-code <claude-code@agents.bee.local>";
const CODEX = "codex <codex@agents.bee.local>";
const R = repoRef(REPO);

const root = connect(endpoint, rootToken);
const manifest: Record<string, unknown> = { org: ORG, repo: REPO, endpoint };
const failingRefs = new Set<string>();
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const ts = (s: number) => ({ seconds: BigInt(s), nanos: 0 });

async function land(head: ObjectId, label: string): Promise<ObjectId> {
  const res = await root.write.land({ repo: R, ref: "main", head });
  const t = res.ref?.target;
  if (!t) throw new Error("land returned no tip for " + label);
  log("landed", label, idText(t).slice(0, 14));
  return t;
}

async function agent(display: string, runtime: string, purpose: string, session: string) {
  const p = await root.auth.createAgent({ display, runtime, purpose, org: ORG });
  const minted = await root.auth.mintToken({
    principalId: p.id,
    scope: { repos: [(await root.repo.getRepo({ repo: R })).id, ORG + "/*"], actions: ["read", "commit", "review", "lease", "land", "issues_write"], expiresAt: ts(NOW + 7 * DAY), session, budget: { maxChangesets: 40n, maxLanded: 10n, maxUploadBytes: 50_000_000n } },
  });
  // Org members who collaborate on the repo can be assigned issues and requested as reviewers.
  await root.org.setMember({ org: ORG, principalId: p.id, role: Role.MEMBER });
  await root.repo.addCollaborator({ repo: R, principalId: p.id, role: RepoRole.WRITE });
  log("agent", display, p.id);
  return { principal: p, token: minted.token, tokenId: minted.tokenId, api: connect(endpoint, minted.token) };
}

type Agent = Awaited<ReturnType<typeof agent>>;

const claudeProv = (label: string, i: number): Commit["provenance"] => ({
  runtime: "claude-code",
  model: "claude-opus-4-1",
  session: "sess-claude-0193",
  inputTokens: 12_000 + i * 3_100,
  outputTokens: 1_400 + i * 420,
  usdMicros: 260_000 + i * 51_000,
  checks: [["bun test", label.includes("fail") ? "fail" : "pass", 3_900 + i * 100]],
});
const codexProv = (i: number): Commit["provenance"] => ({ runtime: "codex", model: "gpt-5-codex", session: "sess-codex-0071", inputTokens: 9_000 + i * 2_000, outputTokens: 900 + i * 300, usdMicros: 140_000 + i * 30_000 });

/** Opens a change request from a changeset on top of `parent`, as `who`. */
async function openChange(who: Agent | null, parent: ObjectId, c: Commit, label: string, title: string, body: string, extra: { linkedIssues?: bigint[]; labels?: string[] } = {}): Promise<ChangeRequest> {
  const api = who?.api ?? root;
  const head = await commit(api, REPO, parent, c, label);
  const cr = await api.change.openChange({ repo: R, targetRef: "main", head, title, body, linkedIssues: extra.linkedIssues ?? [], labels: extra.labels ?? [] });
  log("opened", "#" + cr.number, title);
  return cr;
}

// ------------------------------------------------------------ CI runner (job protocol)

/** plan_json templates: literal strings and `{expr}` parts (docs/spec/workflows.md §9). */
type Template = string | (string | { expr: string })[];
const text = (t: Template | undefined) => (typeof t === "string" ? t : (t ?? []).map((p) => (typeof p === "string" ? p : "${{ " + p.expr + " }}")).join(""));

interface Step {
  name?: Template;
}

/** Runs queued CI jobs with the RunnerService job protocol until the queue stays empty. */
async function runJobs(credential: string, runnerId: string, idleMs: number): Promise<number> {
  const api = connect(endpoint, credential);
  let done = 0;
  const ctl = new AbortController();
  let timer = setTimeout(() => ctl.abort(), idleMs);
  try {
    // One stream for the whole session: every assignment it delivers is leased to this runner.
    for await (const a of api.runner.acquireRunnerJob({ runnerId, maxJobs: 1 }, { signal: ctl.signal })) {
      if (!a.jobId) continue; // keep-alive
      clearTimeout(timer);
      await runJob(api, runnerId, a);
      done++;
      timer = setTimeout(() => ctl.abort(), idleMs);
    }
  } catch (err) {
    if (!ctl.signal.aborted) throw err;
  } finally {
    clearTimeout(timer);
    ctl.abort();
  }
  return done;
}

async function runJob(api: Api, runnerId: string, a: RunnerJobAssignment): Promise<void> {
  const plan = JSON.parse(a.planJson || "{}") as { name?: Template; steps?: Step[] };
  const steps = plan.steps ?? [];
  const jobName = text(plan.name) || "job";
  // The lint job fails on the rounding fix (it touches invoice.ts), with an annotation on the line.
  const failing = jobName === "lint" && failingRefs.has(a.headRef);
  if (failing) failingRefs.delete(a.headRef); // only the first revision fails
  let offset = 0n;
  const append = async (text: string, stepNumber: number) => {
    const data = enc(text);
    const res = await api.runner.appendRunnerJobLog({ runnerId, jobId: a.jobId, leaseId: a.leaseId, offset, data, stepNumber });
    offset = res.storedEnd || offset + BigInt(data.length);
  };
  let t = NOW;
  for (let i = 0; i < steps.length; i++) {
    const n = i + 1;
    const name = text(steps[i]?.name) || "step " + n;
    const fail = failing && i === steps.length - 1;
    await api.runner.updateRunnerJobStep({ runnerId, jobId: a.jobId, leaseId: a.leaseId, stepNumber: n, stepName: name, status: WorkflowStatus.IN_PROGRESS, startedAt: ts(t) });
    await append("##[group]" + name + "\n", n);
    if (name === "Install") await append("bun install v1.2.0\nResolved 2 packages in 41ms\n+ hono@4.6.3\n+ zod@3.22.0\n", n);
    else if (name === "Unit tests") await append("bun test v1.2.0\n\nsrc/billing/invoice.test.ts:\n✓ lineTotal multiplies quantity by unit price\n✓ invoiceTotal adds tax per line\n\n 2 pass\n 0 fail\n", n);
    else if (fail) await append("src/billing/invoice.ts\n::error file=src/billing/invoice.ts,line=27,title=no-floating-tax::Tax must be rounded before it is added to the total\n##[error]Process completed with exit code 1.\n", n);
    else await append("Checked 6 files in 12ms. No fixes applied.\n", n);
    t += 3 + i;
    await api.runner.updateRunnerJobStep({ runnerId, jobId: a.jobId, leaseId: a.leaseId, stepNumber: n, stepName: name, status: WorkflowStatus.COMPLETED, conclusion: fail ? WorkflowConclusion.FAILURE : WorkflowConclusion.SUCCESS, startedAt: ts(t - 3 - i), completedAt: ts(t) });
  }
  await api.runner.completeRunnerJob({ runnerId, jobId: a.jobId, leaseId: a.leaseId, conclusion: failing ? WorkflowConclusion.FAILURE : WorkflowConclusion.SUCCESS, completedAt: ts(t) });
  log("ran job", jobName, a.headRef, failing ? "failed" : "passed");
}

// ------------------------------------------------------------ discovery data set

const isExists = (err: unknown) => (err instanceof ConnectError && err.code === Code.AlreadyExists) || /already exists/i.test(err instanceof Error ? err.message : "");

/** Runs one RPC; a failure is rethrown with the RPC name. `exists` tolerates already-exists. */
async function rpc<T>(name: string, call: () => Promise<T>, opts: { exists?: boolean; unimplemented?: boolean } = {}): Promise<T | undefined> {
  try {
    return await call();
  } catch (err) {
    if (opts.exists && isExists(err)) return undefined;
    if (opts.unimplemented && err instanceof ConnectError && err.code === Code.Unimplemented) {
      log(name, "is not implemented by this server; skipped");
      return undefined;
    }
    throw new Error(name + " failed: " + (err instanceof Error ? err.message : String(err)));
  }
}

/** What the seeded post gave the specs. */
interface SeededPost {
  mediaId: string;
  sha256: string;
  altText: string;
  text: string;
}

/**
 * A post with an image on a change request: Begin, PUT the bytes, Finalize, then set the text.
 * Returns undefined when the server has no PostService yet (the seed then leaves posts out).
 */
async function seedPost(fullName: string, changeId: string, number: number): Promise<SeededPost | undefined> {
  const repo = repoRef(fullName);
  const png = flatPng(64, 40, [34, 40, 49]);
  const sha256 = createHash("sha256").update(png).digest("hex");
  const begin = await rpc("PostService/BeginPostMediaUpload " + fullName, () => root.post.beginPostMediaUpload({ repo, changeId, kind: PostMediaKind.IMAGE, contentType: "image/png", sizeBytes: BigInt(png.length) }), { unimplemented: true });
  if (!begin) return undefined;
  const put = await fetch(new URL(begin.uploadUrl, endpoint), { method: "PUT", headers: begin.uploadHeaders, body: Buffer.from(png) });
  if (!put.ok) throw new Error("post media upload: " + put.status + " " + (await put.text()));
  const altText = "A flat dark swatch that stands in for a screenshot";
  const media = await rpc("PostService/FinalizePostMedia " + fullName, () => root.post.finalizePostMedia({ repo, mediaId: begin.mediaId, sha256, altText, width: 64, height: 40 }));
  if (!media) throw new Error("FinalizePostMedia returned nothing");
  const text = "Contributor notes are in: how a change gets reviewed and landed. #docs";
  await rpc("PostService/SetChangePost " + fullName, () => root.post.setChangePost({ repo, changeId, text, mediaIds: [media.id] }));
  log("drafted a post with one image on", fullName, "change #" + number);
  return { mediaId: media.id, sha256, altText, text };
}

/** After landing: the post is published; the second human follows the repo and likes it. */
async function engagePost(fullName: string, changeId: string, seeded: SeededPost): Promise<void> {
  const repo = repoRef(fullName);
  const post = await root.post.getChangePost({ repo, changeId });
  if (post.state !== PostState.PUBLISHED) throw new Error("the post should publish when its change lands, but is " + PostState[post.state]);
  const reviewer = connect(endpoint, reviewerToken);
  await rpc("UserService/FollowRepo " + fullName, () => reviewer.user.followRepo({ repo }));
  const liked = await rpc("PostService/LikePost " + fullName, () => reviewer.post.likePost({ postId: post.id }));
  manifest.post = { id: post.id, repo: fullName, changeNumber: Number(post.changeNumber), changeTitle: post.changeTitle, text: seeded.text, mediaId: seeded.mediaId, altText: seeded.altText, sha256: seeded.sha256, likes: Number(liked?.counts?.likes ?? 0), followedBy: LOGINS.reviewer };
  log("post", post.id, "published on", fullName, "; followed and liked by", LOGINS.reviewer);
}

/** Seeds org `lattice`: public repos with topics, READMEs and sources; lands, a review, stars, follows. */
async function seedDiscovery(humanId: string, claude: Agent, codex: Agent): Promise<void> {
  if (!reviewerId || !reviewerToken) throw new Error("credentials has no reviewer (is the server running with --dev?)");
  const org = D.DISCOVERY_ORG;
  const reviewer = connect(endpoint, reviewerToken);
  const created = await rpc("OrgService/CreateOrg", () => root.org.createOrg({ name: org, display: "Lattice" }), { exists: true });
  if (!created) {
    log("discovery org", org, "already exists; leaving it as it is");
    return;
  }
  for (const who of [reviewerId, claude.principal.id, codex.principal.id]) await rpc("OrgService/SetMember", () => root.org.setMember({ org, principalId: who, role: Role.MEMBER }));

  // Repos first (with their topics and collaborators), so the agents' tokens can name the repo ids:
  // some change request calls match the token's repo list by id.
  const repoIds: string[] = [];
  for (const r of D.REPOS) {
    const vis = r.visibility === "public" ? Visibility.PUBLIC : Visibility.PRIVATE;
    const made = await rpc("RepoService/CreateRepo " + r.fullName, () => root.repo.createRepo({ org, name: r.name, visibility: vis, description: r.description, autoInit: true }), { exists: true });
    repoIds.push(made?.id ?? (await root.repo.getRepo({ repo: repoRef(r.fullName) })).id);
    if (r.topics.length) await rpc("RepoService/SetRepoTopics " + r.fullName, () => root.repo.setRepoTopics({ repo: repoRef(r.fullName), topics: r.topics }));
    for (const principalId of [reviewerId, claude.principal.id, codex.principal.id]) await rpc("RepoService/AddCollaborator " + r.fullName, () => root.repo.addCollaborator({ repo: repoRef(r.fullName), principalId, role: RepoRole.WRITE }), { exists: true });
  }
  // Agents work in the org with tokens scoped to it.
  const scope = (session: string) => ({ repos: [...repoIds, org + "/*"], actions: ["read", "commit", "review", "lease", "land", "issues_write"], expiresAt: ts(NOW + 7 * DAY), session, budget: { maxChangesets: 100n, maxLanded: 50n, maxUploadBytes: 50_000_000n } });
  const actors = {
    dev: { id: humanId, api: root },
    reviewer: { id: reviewerId, api: reviewer },
    "claude-code": { id: claude.principal.id, api: connect(endpoint, (await root.auth.mintToken({ principalId: claude.principal.id, scope: scope("sess-claude-0193") })).token) },
    codex: { id: codex.principal.id, api: connect(endpoint, (await root.auth.mintToken({ principalId: codex.principal.id, scope: scope("sess-codex-0071") })).token) },
  } as const;

  const landAs = async (api: Api, repo: string, head: ObjectId, label: string) => {
    const res = await rpc("WriteService/Land " + label, () => api.write.land({ repo: repoRef(repo), ref: "main", head }));
    log("landed", repo, label, idText(res?.ref?.target).slice(0, 14));
  };
  const counters = { "claude-code": 0, codex: 0 };
  /** Creates an agent's changeset for one follow-up edit, on top of the current tip. */
  const byAgent = async (who: "claude-code" | "codex", repo: string, [path, text, message]: [string, string, string]) => {
    const label = repo + ":" + message;
    const i = counters[who]++;
    const api = actors[who].api;
    const parent = await tip(api, repo);
    return commit(api, repo, parent, { author: who === "claude-code" ? CLAUDE : CODEX, message, files: { [path]: text }, provenance: who === "claude-code" ? claudeProv(label, i) : codexProv(i) }, label);
  };

  for (const r of D.REPOS) {
    // The first landed commit: README and sources, by the human owner.
    const parent = await tip(root, r.fullName);
    const first = await commit(root, r.fullName, parent, { author: HUMAN, message: "Start " + r.name, files: r.files }, r.fullName + ":start");
    await landAs(root, r.fullName, first, "start");
    if (r.visibility !== "public") continue;

    // Agents each land a follow-up, with provenance.
    const [one, two] = D.FOLLOW_UPS[r.fullName] ?? [];
    if (one) await landAs(actors["claude-code"].api, r.fullName, await byAgent("claude-code", r.fullName, one), "claude " + one[2]);
    if (two) await landAs(actors.codex.api, r.fullName, await byAgent("codex", r.fullName, two), "codex " + two[2]);

    // A reviewed change on two repos: the agent opens it, the second human and the other agent review it,
    // and the owner lands it.
    if (r.name === "agentkit" || r.name === "benchkit") {
      const api = actors["claude-code"].api;
      const label = r.fullName + ":reviewed";
      const head = await commit(api, r.fullName, await tip(api, r.fullName), { author: CLAUDE, message: "Add contributor notes", files: { "CONTRIBUTING.md": "# Contributing\n\nSmall changes land on main after one review.\n" }, provenance: claudeProv(label, counters["claude-code"]++) }, label);
      const cr = await rpc("ChangeService/OpenChange " + r.fullName, () => api.change.openChange({ repo: repoRef(r.fullName), targetRef: "main", head, title: "Add contributor notes", body: "How changes get reviewed and landed.", linkedIssues: [], labels: [] }));
      if (!cr) throw new Error("OpenChange returned nothing for " + r.fullName);
      await rpc("ChangeService/SubmitReview " + r.fullName, () => reviewer.change.submitReview({ repo: repoRef(r.fullName), changeRequestId: cr.id, revision: 1, verdict: Verdict.APPROVE, body: "Clear and short. Approved." }));
      await rpc("ChangeService/SubmitReview " + r.fullName + " (agent)", () => actors.codex.api.change.submitReview({ repo: repoRef(r.fullName), changeRequestId: cr.id, revision: 1, verdict: Verdict.APPROVE, body: "Agree." }));
      // The owner posts about the change before it lands, so the post publishes with it.
      const posted = r.name === "agentkit" ? await seedPost(r.fullName, cr.id, Number(cr.number)) : undefined;
      await rpc("ChangeService/LandChange " + r.fullName, () => root.change.landChange({ repo: repoRef(r.fullName), id: cr.id }));
      log("landed", r.fullName, "reviewed change #" + cr.number);
      if (posted) await engagePost(r.fullName, cr.id, posted);
    }
    // A last human land.
    const lic = await commit(root, r.fullName, await tip(root, r.fullName), { author: HUMAN, message: "Add the MIT license", files: { LICENSE: "MIT License\n\nPermission is hereby granted, free of charge, to any person obtaining a copy of this software to deal in it without restriction.\n" } }, r.fullName + ":license");
    await landAs(root, r.fullName, lic, "license");
  }

  // Stars (trending, repo star counts) and follows (followers/following counts).
  let stars = 0;
  for (const [repo, who] of Object.entries(D.STARS)) {
    for (const login of who) {
      const a = actors[login as keyof typeof actors];
      await rpc("UserService/StarRepo " + repo, () => a.api.user.starRepo({ repo: repoRef(repo) }), { unimplemented: true });
      stars++;
    }
  }
  for (const [from, to] of D.FOLLOWS) {
    await rpc("UserService/FollowUser " + from + "->" + to, () => actors[from as keyof typeof actors].api.user.followUser({ login: to }), { exists: true, unimplemented: true });
  }
  manifest.discovery = { org, publicRepos: D.PUBLIC_REPOS, privateRepo: D.PRIVATE_REPO, topics: D.DISCOVERY_TOPICS, stars, follows: D.FOLLOWS.length };
  log("discovery:", org, D.PUBLIC_REPOS.length, "public repos +", D.PRIVATE_REPO, ";", stars, "stars,", D.FOLLOWS.length, "follows");
}

// ------------------------------------------------------------ main

async function main() {
  const who = await root.auth.whoAmI({});
  const humanId = who.principal?.id ?? "";
  manifest.human = { id: humanId, display: who.principal?.display };

  const claude = await agent("claude-code", "claude-code", "Implements issues assigned to it and fixes review feedback.", "sess-claude-0193");
  const codex = await agent("codex", "codex", "Keeps dependencies current and rotates credentials.", "sess-codex-0071");
  // A narrower child token, minted by the agent itself: the token tree on the agent page.
  const child = await claude.api.auth.mintToken({ scope: { repos: [(await root.repo.getRepo({ repo: R })).id], actions: ["read", "commit"], writePaths: [enc("docs")], expiresAt: ts(NOW + DAY), session: "sess-claude-docs", budget: { maxChangesets: 5n, maxLanded: 0n, maxUploadBytes: 1_000_000n } } });
  manifest.agents = { claude: { id: claude.principal.id, display: "claude-code", tokenId: claude.tokenId, childTokenId: child.tokenId }, codex: { id: codex.principal.id, display: "codex", tokenId: codex.tokenId } };

  // A runner for the repo, before anything triggers CI.
  const reg = await root.runner.createRunnerRegistrationToken({ scope: { kind: RunnerScopeKind.REPO, repo: R } });
  const runner = await root.runner.registerRunner({ registrationToken: reg.token, name: "seed-runner", os: RunnerOs.LINUX, arch: RunnerArch.X64, labels: ["seed"], version: "seed" });
  const runnerId = runner.runner?.id ?? "";

  // ---------------------------------------------------------- history on main (before the policy)
  let main = await tip(root, REPO);
  const base = await commit(root, REPO, main, {
    author: HUMAN,
    time: T0,
    message: "Start the invoicing service",
    files: {
      "README.md": C.README,
      "package.json": C.PACKAGE_JSON("3.22.0"),
      "src/billing/invoice.ts": C.INVOICE_V1,
      "src/billing/tax.ts": C.TAX,
      "src/auth/session.ts": C.SESSION_V1,
      "src/routes/health.ts": C.HEALTH,
      "docs/webhooks.md": C.WEBHOOKS_V1,
    },
  }, "base");
  main = await land(base, "base");
  const tax = await commit(claude.api, REPO, main, { author: CLAUDE, time: T0 + DAY, message: "Add the Texas sales tax rate", files: { "src/billing/tax.ts": C.TAX_V2 }, provenance: claudeProv("tax", 0) }, "tax");
  main = await land(tax, "tax");
  // Push protection refuses a committed credential (an audited denial; nothing is stored).
  const leak = await commit(codex.api, REPO, main, { author: CODEX, time: T0 + 2 * DAY, message: "Add local deployment settings", files: { "config/local.toml": C.LEAKED_CONFIG } }, "leak").then(
    () => "stored",
    (err: Error) => err.message,
  );
  log("leak:", leak);
  const health = await commit(root, REPO, main, { author: HUMAN, time: T0 + 2 * DAY, message: "Report the version from /health", files: { "src/routes/health.ts": C.HEALTH.replace("0.1.0", "0.1.1") } }, "health");
  main = await land(health, "health");
  const v010 = main;
  const ci = await commit(root, REPO, main, { author: HUMAN, time: T0 + 3 * DAY, message: "Run tests and lint in CI; add the land policy", files: { ".beehive/workflows/ci.yml": C.CI, ".beehive/policy.toml": C.POLICY } }, "ci");
  main = await land(ci, "ci+policy");
  manifest.history = { base: idText(base), tax: idText(tax), health: idText(health), blockedPush: leak, ci: idText(ci) };

  // ---------------------------------------------------------- labels, issues, board
  for (const [name, color, description] of [
    ["bug", "d73a4a", "Something is wrong"],
    ["billing", "0e8a16", "Invoices and tax"],
    ["docs", "0075ca", "Documentation"],
    ["agent-ready", "8250df", "Scoped well enough for an agent"],
  ] as const) {
    // Repos start with GitHub's default labels; add only the missing ones.
    await root.issue.createIssueLabel({ repo: R, name, color, description }).catch((err: Error) => {
      if (!/already exists/.test(err.message)) throw err;
    });
  }
  const issue = async (title: string, body: string, labels: string[], assignees: string[] = [], parent?: bigint) =>
    root.issue.createIssue({ repo: R, title, body, labels, assignees, parent: parent ? { repo: R, number: parent } : undefined });
  const drift = await issue("Invoice totals drift by a cent", "Totals on multi-line invoices are one cent off the printed invoice. Tax should be rounded per line.", ["bug", "billing", "agent-ready"], [claude.principal.id]);
  const rotate = await issue("Rotate session ids when a role changes", "A stolen session id keeps working after the user is promoted. Rotate on role change.", ["bug"], [codex.principal.id]);
  const retries = await issue("Document webhook retries", "Operators ask how often failed deliveries are retried.", ["docs", "agent-ready"]);
  const states = await issue("Tax coverage for US states", "Track the states we invoice in.", ["billing"]);
  const texas = await issue("Add the Texas rate", "6.25% state rate.", ["billing"], [], states.number);
  const florida = await issue("Add the Florida rate", "6% state rate.", ["billing", "agent-ready"], [], states.number);
  await root.issue.closeIssue({ repo: R, number: texas.number });
  await root.issue.createIssueComment({ repo: R, number: drift.number, body: "Reproduced with two EU-DE lines of 3.33: the total is 793 cents, the printed invoice says 792." });
  await claude.api.issue.createIssueComment({ repo: R, number: drift.number, body: "Picked this up. I'll round tax per line and add a test." });
  manifest.issues = { drift: Number(drift.number), rotate: Number(rotate.number), retries: Number(retries.number), states: Number(states.number), texas: Number(texas.number), florida: Number(florida.number) };

  const project = await root.project.createProject({ owner: ORG, title: "Roadmap", shortDescription: "What agents and people are working on", linkRepos: [R] });
  // New projects come with a Status single-select field (Todo, In Progress, Done).
  const fields = await root.project.listProjectFields({ projectId: project.id });
  const status = fields.fields.find((f) => f.name === "Status" && f.kind === ProjectFieldKind.SINGLE_SELECT);
  if (!status) throw new Error("the new project has no Status field");
  const opt = (name: string) => status.options.find((o) => o.name.toLowerCase() === name.toLowerCase())?.id ?? "";
  for (const [iss, col] of [
    [drift, "In progress"],
    [rotate, "In progress"],
    [retries, "Todo"],
    [florida, "Todo"],
    [texas, "Done"],
  ] as const) {
    const item = await root.project.addProjectItem({ projectId: project.id, content: { kind: SubjectKind.ISSUE, repo: R, id: iss.id } });
    await root.project.setProjectItemFieldValue({ projectId: project.id, itemId: item.id, value: { fieldId: status.id, value: { case: "optionId", value: opt(col) } } });
  }
  manifest.project = { id: project.id, title: "Roadmap" };

  // ---------------------------------------------------------- leases
  await claude.api.lease.acquireLease({ repo: R, ref: "main", paths: [enc("src/billing")], mode: LeaseMode.ADVISORY, ttlSeconds: 7200, note: "Fixing rounding drift (#" + drift.number + ")" });
  await codex.api.lease.acquireLease({ repo: R, ref: "main", paths: [enc("src/auth")], mode: LeaseMode.EXCLUSIVE, ttlSeconds: 7200, note: "Session rotation" });

  // ---------------------------------------------------------- change requests
  // 1. Logic fix by an agent: r1 approved, r2 makes the approval stale, so it needs a fresh one.
  const fix = await openChange(claude, main, { author: CLAUDE, time: NOW - 5 * 3600, message: "Round tax per invoice line", files: { "src/billing/invoice.ts": C.INVOICE_FIX_R1 }, provenance: claudeProv("fix-r1", 1) }, "fix-r1", "Fix rounding drift in invoice tax lines", "Rounds tax per line, as the printed invoice does. Fixes #" + drift.number + ".", { linkedIssues: [drift.number], labels: ["bug", "billing"] });
  failingRefs.add("cr-" + fix.number);
  await root.change.submitReview({ repo: R, changeRequestId: fix.id, revision: 1, verdict: Verdict.APPROVE, body: "Matches the printed invoice. Ship it." });
  await root.change.createChangeComment({ repo: R, changeRequestId: fix.id, body: "Can the doc comment say why we round per line?", path: enc("src/billing/invoice.ts"), side: "new", startLine: 21, endLine: 21, revision: 1 });
  const fixR2 = await commit(claude.api, REPO, main, { author: CLAUDE, time: NOW - 4 * 3600, message: "Round tax per invoice line", files: { "src/billing/invoice.ts": C.INVOICE_FIX_R2 }, provenance: claudeProv("fix-r2", 2) }, "fix-r2");
  await claude.api.change.reviseChange({ repo: R, id: fix.id, head: fixR2 });

  // 2. Sensitive change: needs two human approvals, has one.
  const rot = await openChange(codex, main, { author: CODEX, time: NOW - 3 * 3600, message: "Rotate session ids on role change", files: { "src/auth/session.ts": C.SESSION_ROTATE }, provenance: codexProv(1) }, "rotate", "Rotate session ids on role change", "A role change now issues a new session id. Fixes #" + rotate.number + ".", { linkedIssues: [rotate.number] });
  await root.change.submitReview({ repo: R, changeRequestId: rot.id, revision: 1, verdict: Verdict.APPROVE, body: "Rotation only on role change, good." });

  // 3. Docs by an agent: policy lets it land without a human.
  const docs = await openChange(claude, main, { author: CLAUDE, time: NOW - 2 * 3600, message: "Document webhook retries", files: { "docs/webhooks.md": C.WEBHOOKS_V2 }, provenance: claudeProv("docs", 3) }, "docs", "Document webhook retries", "Adds the retry schedule. Fixes #" + retries.number + ".", { linkedIssues: [retries.number], labels: ["docs"] });

  // 4. Dependency bump, queued: the merge queue lands it.
  const bump = await openChange(codex, main, { author: CODEX, time: NOW - 90 * 60, message: "Bump zod from 3.22.0 to 3.23.8", files: { "package.json": C.PACKAGE_JSON("3.23.8") }, provenance: codexProv(2) }, "bump", "Bump zod from 3.22.0 to 3.23.8", "Routine dependency update.");
  await codex.api.change.enqueueChange({ repo: R, id: bump.id });

  // 5. A change with a suggested replacement waiting to be applied.
  const cache = await openChange(claude, main, { author: CLAUDE, time: NOW - 60 * 60, message: "Cache invoice totals per customer", files: { "src/billing/cache.ts": C.CACHE } , provenance: claudeProv("cache", 4) }, "cache", "Cache invoice totals per customer", "Memoises totals; invalidated when an invoice changes.");
  await root.change.createChangeComment({ repo: R, changeRequestId: cache.id, body: "Bound the cache so it cannot grow forever.", path: enc("src/billing/cache.ts"), side: "new", startLine: 3, endLine: 3, revision: 1, suggestion: { replacement: "const totals = new Map<string, number>(); // bounded by evictOldest()" } });
  await root.change.createChangeComment({ repo: R, changeRequestId: cache.id, body: "Should this live next to invoice.ts?" });

  // 6. Closed without landing.
  const exp = await openChange(null, main, { author: HUMAN, time: NOW - 50 * 60, message: "Try decimal.js for money", files: { "src/billing/money.ts": C.MONEY } }, "money", "Experiment: decimal.js for money", "Exploring; probably not worth a dependency.");
  await root.change.closeChange({ repo: R, id: exp.id, reason: "Integer cents are enough." });

  // 7. A UI change by an agent: r1 is shown to the page with a screenshot, r2 replaces it.
  const uiR1 = await commit(claude.api, REPO, main, { author: CLAUDE, time: NOW - 40 * 60, message: "Mark the current session in the sessions list", files: { "src/web/sessions.tsx": C.SESSIONS_TSX_R1 }, provenance: claudeProv("ui-r1", 5) }, "ui");
  const ui = await claude.api.change.openChange({ repo: R, targetRef: "main", head: uiR1, title: "Mark the current session in the sessions list", body: "The settings page listed every signed-in session the same way. Mark this device and let people sign out of the others.", linkedIssues: [], labels: [] });
  log("opened", "#" + ui.number, ui.title);

  // Briefs and evidence, through BriefService (see e2e/seed/briefs.ts).
  const briefs = await seedBriefs({
    repo: REPO,
    ui: {
      cr: ui,
      agent: claude.api,
      r1Head: uiR1,
      revise: async () => {
        const r2 = await commit(claude.api, REPO, main, { author: CLAUDE, time: NOW - 30 * 60, message: "Mark the current session in the sessions list", files: { "src/web/sessions.tsx": C.SESSIONS_TSX_R2 }, provenance: claudeProv("ui-r2", 6) }, "ui-r2");
        await claude.api.change.reviseChange({ repo: R, id: ui.id, head: r2 });
      },
    },
    cache: { cr: cache, agent: claude.api },
    fix: { cr: fix, agent: claude.api, head: fixR2 },
    rotate: { cr: rot, agent: codex.api },
    docs: { cr: docs, agent: claude.api },
  });
  manifest.briefs = briefs;

  manifest.changes = {
    ui: { id: ui.id, number: Number(ui.number), title: ui.title },
    fix: { id: fix.id, number: Number(fix.number), title: fix.title },
    rotate: { id: rot.id, number: Number(rot.number), title: rot.title },
    docs: { id: docs.id, number: Number(docs.number), title: docs.title },
    bump: { id: bump.id, number: Number(bump.number), title: bump.title },
    cache: { id: cache.id, number: Number(cache.number), title: cache.title },
    closed: { id: exp.id, number: Number(exp.number), title: exp.title },
  };

  // ---------------------------------------------------------- CI
  const jobs = await runJobs(runner.credential, runnerId, 4000);
  log("ran", jobs, "jobs");

  // Wait for the merge queue to land the bump.
  for (let i = 0; i < 40; i++) {
    const c = await root.change.getChange({ repo: R, id: bump.id });
    if (c.state === 3 /* LANDED */) break;
    await sleep(250);
  }
  // The land of the bump triggers a push run on main.
  log("ran", await runJobs(runner.credential, runnerId, 3000), "more jobs");
  const runs = (await root.workflow.listWorkflowRuns({ repo: R })).runs;
  const failed = runs.find((r) => r.conclusion === WorkflowConclusion.FAILURE);
  manifest.runs = { failed: failed?.id ?? "", failedNumber: Number(failed?.runNumber ?? 0n), total: runs.length };

  // ---------------------------------------------------------- releases
  // Immutable releases are created as drafts and then published.
  const first = await root.release.createRelease({ repo: R, tagName: "v0.1.0", target: { expr: idText(v010) }, name: "v0.1.0", body: "First release of the invoicing service.", immutable: true, draft: true });
  await root.release.publishRelease({ repo: R, releaseId: first.id });
  await root.release.createRelease({ repo: R, tagName: "v0.2.0-rc.1", name: "v0.2.0-rc.1", generateNotes: true, prerelease: true, previousTagName: "v0.1.0" });
  manifest.releases = ["v0.1.0", "v0.2.0-rc.1"];

  // ---------------------------------------------------------- security: SARIF upload (code scanning)
  const head = await tip(root, REPO);
  const sarif = {
    version: "2.1.0",
    runs: [
      {
        tool: { driver: { name: "bee-lint", rules: [{ id: "js/insecure-randomness", name: "Insecure randomness", shortDescription: { text: "Insecure randomness" }, properties: { "security-severity": "7.5", tags: ["security", "CWE-338"] } }] } },
        results: [{ ruleId: "js/insecure-randomness", level: "error", message: { text: "Session ids must come from a CSPRNG." }, locations: [{ physicalLocation: { artifactLocation: { uri: "src/auth/session.ts" }, region: { startLine: 13 } } }] }],
      },
    ],
  };
  try {
    await root.security.uploadSarif({ repo: R, changeset: head, ref: "main", sarif: { case: "sarifGzip", value: gzipSync(enc(JSON.stringify(sarif))) }, toolName: "bee-lint" });
  } catch (err) {
    log("sarif upload refused:", (err as Error).message);
  }

  // ---------------------------------------------------------- packages, pages, team, app, second repo
  // npm publish through the server's npm registry (the npm client's wire format).
  const tarball = enc("seed tarball for @dev/invoice-sdk 0.1.0\n");
  const pkg = "@dev/invoice-sdk";
  const version = { name: pkg, version: "0.1.0", description: "Typed client for the invoicing API", repository: { type: "git", url: endpoint + "/" + REPO + ".git" }, dist: { shasum: createHash("sha1").update(tarball).digest("hex"), tarball: endpoint + "/npm/" + ORG + "/" + pkg + "/-/invoice-sdk-0.1.0.tgz" } };
  const published = await fetch(endpoint + "/npm/" + ORG + "/" + encodeURIComponent(pkg).replace("%40", "@"), {
    method: "PUT",
    headers: { authorization: "Bearer " + rootToken, "content-type": "application/json", "x-bee-repo": REPO },
    body: JSON.stringify({ _id: pkg, name: pkg, "dist-tags": { latest: "0.1.0" }, versions: { "0.1.0": version }, access: "restricted", _attachments: { "invoice-sdk-0.1.0.tgz": { content_type: "application/octet-stream", data: Buffer.from(tarball).toString("base64"), length: tarball.length } } }),
  });
  if (!published.ok) throw new Error("npm publish: " + published.status + " " + (await published.text()));
  manifest.package = pkg;
  try {
    await root.pages.createPagesSite({ repo: R, buildType: PagesBuildType.REF, source: { ref: "main", path: "/docs" } });
    manifest.pages = true;
  } catch (err) {
    log("pages site failed:", (err as Error).message);
  }
  await root.team.createTeam({ org: ORG, name: "Platform", description: "Owns billing and auth", maintainerIds: [humanId] });
  try {
    await root.app.createAppFromManifest({ org: ORG, manifestJson: JSON.stringify({ name: "bee-ci-bot", url: "https://example.com/ci-bot", description: "Posts check results", public: false, default_permissions: { checks: "write", contents: "read" } }) });
    manifest.app = "bee-ci-bot";
  } catch (err) {
    log("app failed:", (err as Error).message);
  }
  await root.repo.createRepo({ org: ORG, name: "website", visibility: Visibility.PRIVATE, description: "Marketing site", autoInit: true });
  manifest.repos = [REPO, "dev/website"];

  // ---------------------------------------------------------- discovery: a second org of public repos
  await seedDiscovery(humanId, claude, codex);
}

main()
  .then(() => {
    writeFileSync(new URL("./.seed.json", import.meta.url), JSON.stringify(manifest, null, 2) + "\n");
    log("done");
    process.exit(0);
  })
  .catch((err) => {
    console.error("[seed] failed:", err);
    process.exit(1);
  });
