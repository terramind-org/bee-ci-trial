/**
 * Contract: the same screen-level API calls through both adapters return identical results.
 *
 *   fixture  createClient(fixtureRpc(store))  in-process Connect router
 *   http     createClient(httpRpc(...))       connect-web over real HTTP (JSON codec) to a fake
 *                                             bee.v1 server: @connectrpc/connect-node serving
 *                                             the generated descriptors, backed by a second store
 *
 * Both stores are built from identical options with a frozen clock, so any difference is a
 * difference in what the adapters do with the wire, not in the data.
 */
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { connectNodeAdapter } from "@connectrpc/connect-node";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { createClient, type BeeClient } from "./client";
import { FixtureStore, fixtureRpc, routesFor } from "./fixture";
import { httpRpc } from "./http";
import { pathBytes } from "./ids";
import type * as T from "./types";

const ANCHOR = Date.UTC(2026, 9, 18, 9, 0, 0);
const repo = { fullName: "acme/api" };
const org = "acme";
/** Any token of eight or more characters signs in as the demo owner; no token is an anonymous caller. */
const TOKEN = "mara-token-0001";
const WEEK = "TRENDING_WINDOW_WEEKLY" as const;

const newStore = () => new FixtureStore({ anchor: ANCHOR, clock: () => 0, live: false, logMs: 100, importMs: 1000 });

async function serve(store: FixtureStore, omit: string[] = []): Promise<{ server: Server; url: string }> {
  const server = createServer(connectNodeAdapter({ routes: routesFor(store, { omit }), grpc: false, grpcWeb: false }));
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  return { server, url: "http://127.0.0.1:" + (server.address() as AddressInfo).port };
}

async function take<X>(it: AsyncIterable<X>, n: number, ctl: AbortController): Promise<X[]> {
  const out: X[] = [];
  for await (const x of it) {
    out.push(x);
    if (out.length === n) {
      ctl.abort();
      break;
    }
  }
  return out;
}

async function outcome<X>(p: Promise<X>): Promise<X | { error: string; reason: string }> {
  try {
    return await p;
  } catch (e) {
    const err = e as { code?: string; reason?: string };
    return { error: String(err.code), reason: String(err.reason) };
  }
}

/** Every read a screen makes, in page order. */
async function screens(api: BeeClient): Promise<Record<string, unknown>> {
  const r: Record<string, unknown> = {};
  r.org = await api.org.getOrg({ nameOrId: org });
  r.members = await api.org.listMembers({ org });
  r.repos = await api.repo.listRepos({ org });
  const changes = await api.change.listChanges({ repo });
  r.changes = changes;
  const cr = changes.changes.find((c) => c.title.startsWith("Rotate session")) as T.ChangeRequest;
  const rev = cr.revisions[cr.revisions.length - 1];
  r.change = await api.change.getChange({ repo, id: cr.id });
  r.reviews = await api.change.listReviews({ repo, changeRequestId: cr.id });
  r.threads = await api.change.listThreads({ repo, changeRequestId: cr.id });
  r.landing = await api.change.evaluateLanding({ repo, changeRequestId: cr.id });
  r.queue = await api.change.listQueue({ repo, ref: "main" });
  r.diff = await api.change.getChangeDiff({ repo, changeRequestId: cr.id, revision: cr.revisions.length, base: rev?.base, head: rev?.head });
  r.diffNoHeads = await api.change.getChangeDiff({ repo, changeRequestId: cr.id, revision: 2 });
  r.diffSince = await api.change.getChangeDiff({ repo, changeRequestId: cr.id, revision: 2, sinceRevision: 1 });
  r.checks = await api.check.listChecks({ repo, changeset: rev?.head });
  r.provenance = await api.query.getChangesets({ repo, ids: rev?.head ? [rev.head] : [], includeMeta: true });
  r.context = await api.repo.getContext({ repo, at: { expr: "main" }, paths: [pathBytes("src/auth")] });
  r.leases = await api.lease.listLeases({ repo });
  r.orgLeases = await api.lease.listLeases({ repo: { fullName: "acme/*" } });
  r.tree = await api.query.listTree({ repo, at: { expr: "main" }, path: "" });
  r.file = await api.readText({ repo, at: { expr: "main" }, path: pathBytes("src/routes/health.ts") });
  r.restricted = await outcome(api.readText({ repo, at: { expr: "main" }, path: pathBytes("ops/secrets.enc.yaml") }));
  r.log = await api.query.log({ repo, from: { expr: "main" } });
  r.blame = await api.query.blame({ repo, at: { expr: "main" }, path: pathBytes("src/routes/health.ts") });
  r.compare = await api.diffAll({ repo, from: { expr: "main~3" }, to: { expr: "main" }, includeHunks: true });
  r.refs = await api.ref.listRefs({ repo });
  r.resolve = await api.object.resolveRef({ repo, name: "main" });
  r.whoami = await api.auth.whoAmI({});
  const agents = await api.agent.listAgents({ org });
  r.agents = agents;
  const triage = agents.agents.find((a) => a.principal.display === "triage") as T.AgentSummary;
  r.agent = await api.agent.getAgent({ org, principalId: triage.principal.id });
  r.tokens = await api.agent.listTokens({ principalId: triage.principal.id });
  r.agentProvenance = await api.agent.listProvenance({ principalId: triage.principal.id });
  const runs = await api.workflow.listRuns({ repo });
  r.runs = runs;
  r.runsForCr = await api.workflow.listRuns({ repo, changeRequestId: cr.id });
  r.failedRuns = await api.workflow.listRuns({ repo, status: "failed" });
  const done = runs.runs.find((x) => x.status === "failed") as T.Run;
  r.run = await api.workflow.getRun({ repo, id: done.id });
  r.log0 = await take(api.workflow.streamLogs({ repo, runId: done.id, jobId: done.jobs[0]?.id ?? "" }), 1000, new AbortController());
  r.issues = await api.issue.listIssues({ repo });
  r.openIssues = await api.issue.listIssues({ repo, state: "open" });
  r.issue = await api.issue.getIssue({ repo, number: 12 });
  r.board = await api.project.getProject({ repo });
  r.releases = await api.release.listReleases({ repo });
  r.notes = await api.release.generateNotes({ repo, tag: "v1.4.0" });
  r.alerts = await api.security.listAlerts({ repo });
  r.openAlerts = await api.security.listAlerts({ repo, state: "open" });
  const policy = await api.policy.getPolicy({ repo });
  r.policy = policy;
  r.evaluate = await api.policy.evaluatePolicy({ repo, source: policy.source, sample: 20 });
  r.badPolicy = await api.policy.evaluatePolicy({ repo, source: "version = 1\n[land.\n", sample: 5 });
  r.audit = await api.audit.listAudit({ org });
  r.deniedAudit = await api.audit.listAudit({ org, deniedOnly: true });
  r.imports = await api.import.listImports({ targetOrg: org });
  const ctl = new AbortController();
  r.events = await take(api.event.subscribe({ repo, fromSeq: "0" }, { signal: ctl.signal }), 40, ctl);
  r.missing = await outcome(api.change.getChange({ repo, id: "nope" }));
  Object.assign(r, await community(api), await cloud(api));
  return r;
}

/** Community and discovery reads, as a signed-in caller. */
async function community(api: BeeClient): Promise<Record<string, unknown>> {
  const r: Record<string, unknown> = {};
  r.serverInfo = await api.object.getServerInfo({});
  r.myRepos = await api.repo.listMyRepos({});
  r.homePrivate = await api.repo.getRepoHome({ fullName: "acme/api" });
  r.homePublic = await api.repo.getRepoHome({ fullName: "lattice/engine" });
  r.homeHidden = await outcome(api.repo.getRepoHome({ fullName: "lattice/internal-tools" }));
  r.languages = await api.insights.getLanguages({ repo: { fullName: "lattice/engine" } });
  r.search = await api.search.searchRepos({ query: "is:public", sort: "stars" });
  r.searchTopic = await api.search.searchRepos({ query: "topic:agents language:typescript", sort: "updated" });
  r.trending = await api.search.listTrendingRepos({ window: WEEK });
  r.trendingTs = await api.search.listTrendingRepos({ window: "TRENDING_WINDOW_MONTHLY", language: "TypeScript" });
  r.developers = await api.search.listTrendingDevelopers({ window: WEEK, includeAgents: true });
  r.humansOnly = await api.search.listTrendingDevelopers({ window: WEEK });
  r.boardAgents = await api.search.listLeaderboard({ window: "TRENDING_WINDOW_MONTHLY", subject: "LEADERBOARD_SUBJECT_AGENTS", metric: "LEADERBOARD_METRIC_LANDS" });
  r.boardHumans = await api.search.listLeaderboard({ window: "TRENDING_WINDOW_MONTHLY", subject: "LEADERBOARD_SUBJECT_HUMANS", metric: "LEADERBOARD_METRIC_REVIEWS" });
  r.boardRepos = await api.search.listLeaderboard({ window: "TRENDING_WINDOW_MONTHLY", subject: "LEADERBOARD_SUBJECT_REPOS", metric: "LEADERBOARD_METRIC_STARS" });
  r.boardInvalid = await outcome(api.search.listLeaderboard({ subject: "LEADERBOARD_SUBJECT_REPOS", metric: "LEADERBOARD_METRIC_REVIEWS" }));
  r.topic = await api.search.getTopic({ name: "agents" });
  r.topicMissing = await outcome(api.search.getTopic({ name: "no-such-topic" }));
  r.featured = await api.search.listFeaturedTopics({});
  r.publicActivity = await api.search.listPublicActivity({});
  r.profileUser = await api.user.getProfile({ owner: "rhea" });
  r.profileAgent = await api.user.getProfile({ owner: "patcher" });
  r.profileOrg = await api.user.getProfile({ owner: "lattice" });
  r.profileMissing = await outcome(api.user.getProfile({ owner: "nobody-here" }));
  r.followers = await api.user.listFollowers({ login: "rhea" });
  r.following = await api.user.listFollowing({ login: "rhea" });
  r.starred = await api.user.listStarred({ login: "mara" });
  r.graph = await api.user.getContributionGraph({ login: "rhea" });
  r.achievements = await api.user.listAchievements({ login: "rhea" });
  r.feed = await api.user.listHomeFeed({});
  r.feedLands = await api.user.listHomeFeed({ kinds: ["ref.updated"] });
  return r;
}

/** Cloud and admin reads. */
async function cloud(api: BeeClient): Promise<Record<string, unknown>> {
  const r: Record<string, unknown> = {};
  const scope = { enterprise: "acme-group" };
  r.plans = await api.billing.listBillingPlans({});
  r.accountPlan = await api.billing.getAccountPlan({ org });
  r.freePlan = await api.billing.getAccountPlan({ org: "lattice" });
  r.seats = await api.billing.listSeats({ org });
  r.usage = await api.billing.queryUsage({ org });
  r.usageByProduct = await api.billing.queryUsage({ org, groupBy: ["BILLING_USAGE_GROUPING_PRODUCT"] });
  r.opsHealth = await api.ops.getOpsHealth({ includeComponents: true });
  r.cluster = await api.ops.getClusterStatus({});
  r.configOk = await api.ops.validateOpsConfig({ toml: '[server]\nedition = "cloud"\n' });
  r.configBad = await api.ops.validateOpsConfig({ toml: '[server]\nedition = "hosted"\n' });
  r.enterprise = await api.enterprise.getEnterprise({ enterprise: "acme-group" });
  r.enterpriseOrgs = await api.enterprise.listEnterpriseOrgs({ enterprise: "acme-group" });
  r.enterpriseOwners = await api.enterprise.listEnterpriseOwners({ enterprise: "acme-group" });
  r.saml = await api.enterprise.getSamlConfig({ scope });
  r.scim = await api.enterprise.getScimConfig({ scope });
  r.domains = await api.enterprise.listVerifiedDomains({ scope });
  r.twoFactor = await api.enterprise.getTwoFactorRequirement({ scope });
  r.ipAllow = await api.enterprise.listIpAllowList({ scope });
  r.policies = await api.enterprise.getEnterprisePolicies({ enterprise: "acme-group" });
  return r;
}

/** What a visitor with no credentials may read, and what stays closed to them. */
async function anonymous(api: BeeClient): Promise<Record<string, unknown>> {
  const r: Record<string, unknown> = {};
  r.serverInfo = await api.object.getServerInfo({});
  r.homePublic = await api.repo.getRepoHome({ fullName: "lattice/engine" });
  r.homeSdk = await api.repo.getRepoHome({ fullName: "acme/sdk" });
  r.homePrivate = await outcome(api.repo.getRepoHome({ fullName: "acme/api" }));
  r.homeInternal = await outcome(api.repo.getRepoHome({ fullName: "acme/web" }));
  r.homeMissing = await outcome(api.repo.getRepoHome({ fullName: "acme/does-not-exist" }));
  r.search = await api.search.searchRepos({ query: "", sort: "stars" });
  r.searchPrivate = await api.search.searchRepos({ query: "is:private" });
  r.trending = await api.search.listTrendingRepos({ window: WEEK });
  r.developers = await api.search.listTrendingDevelopers({ window: WEEK, includeAgents: true });
  r.board = await api.search.listLeaderboard({ window: WEEK, subject: "LEADERBOARD_SUBJECT_AGENTS", metric: "LEADERBOARD_METRIC_LANDS" });
  r.topic = await api.search.getTopic({ name: "agents" });
  r.featured = await api.search.listFeaturedTopics({});
  r.publicActivity = await api.search.listPublicActivity({});
  r.profile = await api.user.getProfile({ owner: "rhea" });
  r.orgProfile = await api.user.getProfile({ owner: "lattice" });
  r.followers = await api.user.listFollowers({ login: "rhea" });
  r.starred = await api.user.listStarred({ login: "rhea" });
  r.graph = await api.user.getContributionGraph({ login: "rhea" });
  r.achievements = await api.user.listAchievements({ login: "rhea" });
  r.tree = await api.query.listTree({ repo: { fullName: "lattice/engine" }, at: { expr: "main" }, path: "" });
  r.treePrivate = await outcome(api.query.listTree({ repo, at: { expr: "main" }, path: "" }));
  // The tabs of a public repo read without credentials; a private repo is not found.
  const pub = { fullName: "lattice/engine" };
  r.changes = await api.change.listChanges({ repo: pub });
  r.issues = await api.issue.listIssues({ repo: pub });
  r.runs = await api.workflow.listRuns({ repo: pub });
  r.leases = await api.lease.listLeases({ repo: pub });
  r.changesPrivate = await outcome(api.change.listChanges({ repo }));
  r.issuesPrivate = await outcome(api.issue.listIssues({ repo }));
  r.runsPrivate = await outcome(api.workflow.listRuns({ repo }));
  // Agents and members stay with the org's members.
  r.agents = await outcome(api.agent.listAgents({ org: "lattice", repo: pub }));
  r.members = await outcome(api.org.listMembers({ org: "lattice" }));
  // Closed to a visitor with no credentials.
  r.myRepos = await outcome(api.repo.listMyRepos({}));
  r.feed = await outcome(api.user.listHomeFeed({}));
  r.star = await outcome(api.user.starRepo({ repo: { fullName: "lattice/engine" } }));
  r.follow = await outcome(api.user.followUser({ login: "rhea" }));
  r.createOrg = await outcome(api.org.createOrg({ name: "newcomers" }));
  r.plans = await outcome(api.billing.listBillingPlans({}));
  r.health = await outcome(api.ops.getOpsHealth({}));
  r.enterprise = await outcome(api.enterprise.getEnterprise({ enterprise: "acme-group" }));
  return r;
}

/** Every write a screen makes, in a fixed order, then the reads that show the result. */
async function writes(api: BeeClient): Promise<Record<string, unknown>> {
  const r: Record<string, unknown> = {};
  const changes = (await api.change.listChanges({ repo })).changes;
  const cr = changes.find((c) => c.title.startsWith("Rotate session")) as T.ChangeRequest;
  const threads = (await api.change.listThreads({ repo, changeRequestId: cr.id })).threads;
  r.created = await api.change.createThread({ repo, changeRequestId: cr.id, revision: cr.revisions.length, path: pathBytes("src/auth/session.ts"), side: "new", startLine: 3, endLine: 3, body: "why?", suggestion: "// why" });
  r.reply = await api.change.replyThread({ repo, threadId: threads[0]?.id ?? "", body: "because" });
  r.resolved = await api.change.resolveThread({ repo, threadId: threads[0]?.id ?? "", resolved: true });
  r.reopened = await api.change.resolveThread({ repo, threadId: threads[0]?.id ?? "", resolved: false });
  const open = (await api.change.listThreads({ repo, changeRequestId: cr.id })).threads.filter((t) => !t.resolved && t.comments.some((c) => c.suggestion));
  r.applied = await api.change.applySuggestions({ repo, changeRequestId: cr.id, commentIds: open.flatMap((t) => t.comments.filter((c) => c.suggestion).map((c) => c.id)) });
  r.review = await outcome(api.change.submitReview({ repo, changeRequestId: cr.id, revision: 1, verdict: "VERDICT_APPROVE", body: "", comments: [] }));
  const docs = changes.find((c) => c.title.startsWith("Document the webhook")) as T.ChangeRequest;
  r.enqueue = await outcome(api.change.enqueue({ repo, changeRequestId: docs.id }));
  r.land = await outcome(api.write.land({ repo, ref: "main", changeRequestId: cr.id }));
  r.lease = await api.lease.acquireLease({ repo, ref: "main", paths: [pathBytes("src/billing")], mode: "LEASE_MODE_ADVISORY", note: "contract" });
  r.leased = await outcome(api.lease.acquireLease({ repo, ref: "main", paths: [pathBytes("src/auth/tokens.ts")], mode: "LEASE_MODE_ADVISORY" }));
  const agents = (await api.agent.listAgents({ org })).agents;
  const docsWriter = agents.find((a) => a.principal.display === "docs-writer") as T.AgentSummary;
  r.assigned = await api.issue.assignIssue({ repo, number: 21, assigneeIds: [docsWriter.principal.id] });
  r.closed = await api.issue.setIssueState({ repo, number: 22, state: "closed" });
  r.commented = await api.issue.commentIssue({ repo, number: 22, body: "done" });
  const board = await api.project.getProject({ repo });
  r.moved = await api.project.moveItem({ repo, projectId: board.id, issueNumber: 14, toColumnId: "done" });
  r.release = await api.release.createRelease({ repo, tag: "v1.5.0", name: "v1.5.0", notes: "n", prerelease: false });
  r.tagTaken = await outcome(api.release.createRelease({ repo, tag: "v1.5.0", name: "", notes: "", prerelease: false }));
  const alert = (await api.security.listAlerts({ repo, state: "open" })).alerts[0] as T.SecurityAlert;
  r.dismissed = await api.security.dismissAlert({ repo, id: alert.id, reason: "not reachable" });
  const scope = { repos: ["acme/api"], readPaths: [], writePaths: [pathBytes("docs")], refs: ["ws/*"], actions: ["read", "commit"], session: "c", expiresAt: new Date(ANCHOR + 3_600_000).toISOString() };
  r.minted = await api.auth.mintToken({ principalId: docsWriter.principal.id, scope });
  r.tooWide = await outcome(api.auth.mintToken({ principalId: docsWriter.principal.id, scope: { ...scope, actions: ["admin"] } }));
  r.rerun = await outcome(api.workflow.rerun({ repo, id: (await api.workflow.listRuns({ repo, status: "failed" })).runs[0]?.id ?? "", failedOnly: true }));
  r.import = await api.import.startImport({ source: "octo/widgets", org: false, targetOrg: org, phase: "IMPORT_PHASE_DRY_RUN" });
  Object.assign(r, await communityWrites(api), await cloudWrites(api));
  r.after = await screens(api);
  return r;
}

/** Stars, follows and creation, with the reads that show them. */
async function communityWrites(api: BeeClient): Promise<Record<string, unknown>> {
  const r: Record<string, unknown> = {};
  const engine = { fullName: "lattice/engine" };
  r.starred = await api.user.starRepo({ repo: engine });
  r.starredAgain = await api.user.starRepo({ repo: engine });
  r.starredHome = (await api.repo.getRepoHome({ fullName: "lattice/engine" })).starredByMe;
  r.starsAfter = (await api.search.searchRepos({ query: "repo:lattice/engine" })).hits.map((h) => h.starCount);
  r.starPrivate = await outcome(api.user.starRepo({ repo: { fullName: "lattice/internal-tools" } }));
  r.unstarred = await api.user.unstarRepo({ repo: engine });
  r.followed = await api.user.followUser({ login: "rhea" });
  r.followedProfile = (await api.user.getProfile({ owner: "rhea" })).followingByMe;
  r.followersAfter = await api.user.listFollowers({ login: "rhea" });
  r.followSelf = await outcome(api.user.followUser({ login: "mara" }));
  r.followMissing = await outcome(api.user.followUser({ login: "nobody-here" }));
  r.unfollowed = await api.user.unfollowUser({ login: "rhea" });
  r.createdOrg = await api.org.createOrg({ name: "newcomers", display: "Newcomers" });
  r.orgTaken = await outcome(api.org.createOrg({ name: "newcomers" }));
  r.orgBadName = await outcome(api.org.createOrg({ name: "Not Valid" }));
  r.orgReserved = await outcome(api.org.createOrg({ name: "explore" }));
  r.createdRepo = await api.repo.createRepo({ org: "newcomers", name: "hello", visibility: "VISIBILITY_PUBLIC", description: "A first repo", topics: ["agents"], autoInit: true });
  r.repoTaken = await outcome(api.repo.createRepo({ org: "newcomers", name: "hello" }));
  r.repoInOtherOrg = await outcome(api.repo.createRepo({ org: "lattice", name: "mine" }));
  r.newHome = await api.repo.getRepoHome({ fullName: "newcomers/hello" });
  return r;
}

/** Posts, follows and the feed trigger: every read a screen makes. */
async function postScreens(api: BeeClient): Promise<Record<string, unknown>> {
  const r: Record<string, unknown> = {};
  const engine = { fullName: "lattice/engine" };
  r.feed = await api.post.listFollowingFeed({ page: { pageSize: 5 } });
  const feed = r.feed as T.ListFollowingFeedResponse;
  r.feedNext = await api.post.listFollowingFeed({ page: { pageSize: 5, pageToken: feed.page?.nextPageToken } });
  r.feedPoll = await api.post.listFollowingFeed({ after: (r.feedNext as T.ListFollowingFeedResponse).newestCursor });
  r.feedBadCursor = await outcome(api.post.listFollowingFeed({ page: { pageToken: "x" } }));
  r.repoPosts = await api.post.listRepoPosts({ repo: engine });
  const first = (r.repoPosts as T.ListRepoPostsResponse).posts[0] as T.Post;
  r.post = await api.post.getPost({ postId: first.id });
  r.postMissing = await outcome(api.post.getPost({ postId: "01NOSUCHPOST0000000000000A" }));
  r.adoptions = await api.post.listPostAdoptions({ postId: first.id });
  const changes = (await api.change.listChanges({ repo: engine, state: "CHANGE_STATE_LANDED" })).changes;
  r.landedWithPost = changes.map((c) => [c.number, c.post?.id, c.post?.state]);
  r.changePost = await api.post.getChangePost({ repo: engine, changeId: (changes.find((c) => c.post) as T.ChangeRequest).id });
  r.followedRepos = await api.user.listFollowedRepos({ user: "mara" });
  r.followers = await api.user.listRepoFollowers({ repo: engine });
  r.following = await api.user.isFollowingRepo({ repo: engine });
  const triage = (await api.agent.listAgents({ org })).agents.find((a) => a.principal.display === "triage") as T.AgentSummary;
  r.triggers = await api.agent.listAgentFeedTriggers({ repo });
  r.trigger = await api.agent.getAgentFeedTrigger({ repo, agent: triage.principal.id });
  r.adoptRuns = await api.agent.listAdoptRuns({ repo });
  r.issues = (await api.issue.listIssues({ repo })).issues.filter((i) => i.inspiredBy);
  return r;
}

/** Writing a post and engaging with it, including the refusals. */
async function postWrites(api: BeeClient): Promise<Record<string, unknown>> {
  const r: Record<string, unknown> = {};
  const infra = { fullName: "acme/infra" };
  const engine = { fullName: "lattice/engine" };
  const cr = (await api.change.listChanges({ repo: infra })).changes[0] as T.ChangeRequest;
  r.empty = await outcome(api.post.setChangePost({ repo: infra, changeId: cr.id, text: "  " }));
  r.tooLong = await outcome(api.post.setChangePost({ repo: infra, changeId: cr.id, text: "x".repeat(501) }));
  const up = await api.post.beginPostMediaUpload({ repo: infra, changeId: cr.id, kind: "POST_MEDIA_KIND_IMAGE", contentType: "image/png", sizeBytes: "2048" });
  r.upload = { ...up, mediaId: up.mediaId.length };
  r.noAlt = await outcome(api.post.finalizePostMedia({ repo: infra, mediaId: up.mediaId, sha256: "c".repeat(64) }));
  r.media = await api.post.finalizePostMedia({ repo: infra, mediaId: up.mediaId, sha256: "c".repeat(64), altText: "A diagram of the new cell", width: 800, height: 600 });
  r.svg = await outcome(api.post.beginPostMediaUpload({ repo: infra, changeId: cr.id, kind: "POST_MEDIA_KIND_IMAGE", contentType: "image/svg+xml", sizeBytes: "10" }));
  r.set = await api.post.setChangePost({ repo: infra, changeId: cr.id, text: "A new cell in us-east-2.", mediaIds: [up.mediaId] });
  r.stale = await outcome(api.post.setChangePost({ repo: infra, changeId: cr.id, text: "again", expectedVersion: "7" }));
  r.get = await api.post.getChangePost({ repo: infra, changeId: cr.id });
  r.withdrawn = await api.post.withdrawPost({ repo: infra, postId: (r.set as T.Post).id, reason: "Not ready" });
  r.cleared = await outcome(api.post.clearChangePost({ repo: infra, changeId: cr.id }));
  const post = ((await api.post.listRepoPosts({ repo: engine })).posts[0]) as T.Post;
  r.liked = await api.post.likePost({ postId: post.id });
  r.likedAgain = await api.post.likePost({ postId: post.id });
  r.unliked = await api.post.unlikePost({ postId: post.id });
  r.reposted = await api.post.repostPost({ postId: post.id });
  r.undone = await api.post.undoRepost({ postId: post.id });
  r.likeDraft = await outcome(api.post.likePost({ postId: (await api.change.listChanges({ repo })).changes.map((c) => c.post).find((p) => p)?.id ?? "x" }));
  r.followRepo = await api.user.followRepo({ repo: { fullName: "lattice/docs" } });
  r.followingDocs = await api.user.isFollowingRepo({ repo: { fullName: "lattice/docs" } });
  r.followPrivate = await outcome(api.user.followRepo({ repo: { fullName: "lattice/internal-tools" } }));
  r.unfollowRepo = await api.user.unfollowRepo({ repo: { fullName: "lattice/docs" } });
  r.issue = await api.issue.createIssue({ repo, title: "Walk the invoice tree without recursion", labels: ["perf"], inspiredByPostId: post.id });
  r.issueBad = await outcome(api.issue.createIssue({ repo, title: "x", inspiredByPostId: "01NOSUCHPOST0000000000000A" }));
  r.opened = await api.change.openChange({ repo, targetRef: "main", title: "Port the walk", body: "", inspiredByPostId: post.id, postText: "Ported the walk." });
  r.postAfter = await api.post.getPost({ postId: post.id });
  r.adoptionsAfter = await api.post.listPostAdoptions({ postId: post.id });
  const triage = (await api.agent.listAgents({ org })).agents.find((a) => a.principal.display === "triage") as T.AgentSummary;
  r.setTrigger = await api.agent.setAgentFeedTrigger({ repo, agent: triage.principal.id, enabled: false, dailyUsdBudget: 2.5, maxRunsPerDay: 3, sources: [engine] });
  r.badBudget = await outcome(api.agent.setAgentFeedTrigger({ repo, agent: triage.principal.id, enabled: true, dailyUsdBudget: -1 }));
  r.triggersAfter = await api.agent.listAgentFeedTriggers({ repo });
  return r;
}

/** What a signed-out visitor sees of posts. */
async function postsAnonymous(api: BeeClient): Promise<Record<string, unknown>> {
  const r: Record<string, unknown> = {};
  const engine = { fullName: "lattice/engine" };
  r.repoPosts = await api.post.listRepoPosts({ repo: engine });
  const first = (r.repoPosts as T.ListRepoPostsResponse).posts[0] as T.Post;
  r.post = await api.post.getPost({ postId: first.id });
  r.private = await outcome(api.post.listRepoPosts({ repo }));
  r.changes = (await api.change.listChanges({ repo: engine })).changes.map((c) => [c.number, c.post?.id, c.post?.state, c.post?.viewer]);
  const calls: Record<string, () => Promise<unknown>> = {
    feed: () => api.post.listFollowingFeed({}),
    like: () => api.post.likePost({ postId: first.id }),
    repost: () => api.post.repostPost({ postId: first.id }),
    adoptions: () => api.post.listPostAdoptions({ postId: first.id }),
    follow: () => api.user.followRepo({ repo: engine }),
    triggers: () => api.agent.listAgentFeedTriggers({ repo: { fullName: "acme/sdk" } }),
    set: () => api.post.setChangePost({ repo: engine, changeId: "x", text: "hi" }),
  };
  for (const [k, call] of Object.entries(calls)) {
    r[k] = await outcome(call());
  }
  return r;
}

/** Plan changes and every admin write, in a fixed order. */
/** Briefs, evidence, the conversation and sessions of a change request. */
async function briefScreens(api: BeeClient): Promise<Record<string, unknown>> {
  const rotate = (await api.change.listChanges({ repo })).changes.find((c) => c.title === "Rotate session ids on role change") as T.ChangeRequest;
  const cache = (await api.change.listChanges({ repo })).changes.find((c) => c.title === "Cache invoice totals per customer") as T.ChangeRequest;
  const evidence = await api.brief.listEvidence({ repo, changeIds: [rotate.id, cache.id] });
  return {
    brief: rotate.brief,
    evidence,
    hero: await api.brief.listEvidence({ repo, changeIds: [rotate.id, cache.id], heroOnly: true }),
    one: await api.brief.getEvidence({ repo, id: (evidence.evidence[0] as T.Evidence).id }),
    checklist: await api.brief.getBriefChecklist({ repo, changeId: rotate.id }),
    comments: await api.change.listComments({ repo, changeRequestId: rotate.id }),
    sessions: await api.agent.getSessionsForChange({ repo, changeRequest: rotate.id }),
    missing: await outcome(api.brief.getEvidence({ repo, id: "01NOSUCHEVIDENCE000000000" })),
  };
}

async function briefWrites(api: BeeClient): Promise<Record<string, unknown>> {
  const cache = (await api.change.listChanges({ repo })).changes.find((c) => c.title === "Cache invoice totals per customer") as T.ChangeRequest;
  const brief = { outcome: "Faster totals.", highlights: [{ text: "Cached per customer.", paths: ["src/billing/invoice.ts"] }], asks: [{ question: "Ship?", options: ["Yes", "No"] }], risks: [], rollback: "Revert.", tryItUrl: "" };
  const set = await api.brief.setChangeBrief({ repo, changeId: cache.id, brief });
  const spec = { kind: "EVIDENCE_KIND_PREVIEW_LINK" as const, title: "Preview", caption: "", benchmark: [], url: "https://preview.example/pr", paths: [], hero: true, order: 0 };
  const added = await api.brief.addEvidence({ repo, changeId: cache.id, spec });
  const updated = await api.brief.updateEvidence({ repo, id: added.id, spec: { ...spec, title: "Preview v2" } });
  const comment = await api.change.commentChange({ repo, changeRequestId: cache.id, body: 'Decision on "Ship?": Yes' });
  const out = {
    set,
    added,
    updated,
    comment,
    long: await outcome(api.brief.setChangeBrief({ repo, changeId: cache.id, brief: { ...brief, outcome: "x".repeat(300) } })),
    http: await outcome(api.brief.addEvidence({ repo, changeId: cache.id, spec: { ...spec, url: "http://insecure.example" } })),
    kind: await outcome(api.brief.updateEvidence({ repo, id: added.id, spec: { ...spec, kind: "EVIDENCE_KIND_BENCHMARK" } })),
    svg: await outcome(
      api.brief.addEvidence({
        repo,
        changeId: cache.id,
        spec: { ...spec, kind: "EVIDENCE_KIND_SCREENSHOT", url: "" },
        assets: [{ role: "EVIDENCE_ASSET_ROLE_MAIN", contentType: "image/svg+xml", data: Buffer.from("<svg xmlns='http://www.w3.org/2000/svg'/>").toString("base64"), alt: "x" }],
      }),
    ),
    del: await api.brief.deleteEvidence({ repo, id: added.id }),
    gone: await outcome(api.brief.getEvidence({ repo, id: added.id })),
    after: await api.brief.listEvidence({ repo, changeIds: [cache.id] }),
  };
  return out;
}

/** What a signed-out visitor sees of briefs. */
async function briefsAnonymous(api: BeeClient): Promise<Record<string, unknown>> {
  const engine = { fullName: "lattice/engine" };
  const cr = (await api.change.listChanges({ repo: engine })).changes[0] as T.ChangeRequest;
  const rotate = { id: "01NOTREAL" };
  return {
    evidence: await api.brief.listEvidence({ repo: engine, changeIds: [cr.id] }),
    checklist: await api.brief.getBriefChecklist({ repo: engine, changeId: cr.id }),
    private: await outcome(api.brief.listEvidence({ repo, changeIds: [rotate.id] })),
    set: await outcome(api.brief.setChangeBrief({ repo: engine, changeId: cr.id, brief: { outcome: "x", highlights: [], asks: [], risks: [], rollback: "", tryItUrl: "" } })),
    comments: await outcome(api.change.listComments({ repo: engine, changeRequestId: cr.id })),
  };
}

async function cloudWrites(api: BeeClient): Promise<Record<string, unknown>> {
  const r: Record<string, unknown> = {};
  const scope = { enterprise: "acme-group" };
  r.upgrade = await api.billing.changePlan({ org, planId: "team", seats: 14 });
  r.seatsBelow = await outcome(api.billing.changePlan({ org, planId: "team", seats: 2 }));
  r.notSelfServe = await outcome(api.billing.changePlan({ org, planId: "enterprise" }));
  r.noPlan = await outcome(api.billing.changePlan({ org, planId: "platinum" }));
  r.notOwner = await outcome(api.billing.changePlan({ org: "lattice", planId: "team", seats: 5 }));
  r.planAfter = await api.billing.getAccountPlan({ org });
  r.entUpdated = await api.enterprise.updateEnterprise({ enterprise: "acme-group", description: "All of Acme" });
  r.samlIncomplete = await outcome(api.enterprise.updateSamlConfig({ scope, enabled: true }));
  r.samlTest = await api.enterprise.testSamlConfig({ scope, idpMetadataUrl: "https://idp.example/metadata" });
  r.scimOff = await outcome(api.enterprise.createScimToken({ scope, description: "okta" }));
  r.scimOn = await api.enterprise.updateScimConfig({ scope, enabled: true, provisionsTeams: true });
  r.scimToken = await api.enterprise.createScimToken({ scope, description: "okta", ttlSeconds: 86400 });
  const added = await api.enterprise.addVerifiedDomain({ scope, domain: "acme.dev" });
  r.domainAdded = added;
  r.domainBad = await outcome(api.enterprise.addVerifiedDomain({ scope, domain: "not a domain" }));
  r.domainDup = await outcome(api.enterprise.addVerifiedDomain({ scope, domain: "acme.dev" }));
  r.domainCheck1 = await api.enterprise.checkVerifiedDomain({ scope, domainId: added.id });
  r.domainCheck2 = await api.enterprise.checkVerifiedDomain({ scope, domainId: added.id });
  r.domainDeleted = await api.enterprise.deleteVerifiedDomain({ scope, domainId: added.id });
  r.domainGone = await outcome(api.enterprise.deleteVerifiedDomain({ scope, domainId: added.id }));
  r.twoFactor = await api.enterprise.updateTwoFactorRequirement({ scope, required: true, secureMethodsOnly: true, removeNoncompliant: true });
  const ip = await api.enterprise.createIpAllowListEntry({ scope, cidr: "192.0.2.0/24", description: "Staging", active: true });
  r.ipCreated = ip;
  r.ipBad = await outcome(api.enterprise.createIpAllowListEntry({ scope, cidr: "999.1.1.1/8" }));
  r.ipList = await api.enterprise.listIpAllowList({ scope });
  r.ipDeleted = await api.enterprise.deleteIpAllowListEntry({ scope, entryId: ip.id });
  r.policies = await api.enterprise.updateEnterprisePolicies({ enterprise: "acme-group", repoCreation: "ENTERPRISE_REPO_CREATION_POLICY_PRIVATE", forking: "ENTERPRISE_FORKING_POLICY_WITHIN_ORG" });
  return r;
}

describe("contract: fixture adapter vs connect-web against a fake bee.v1 server", () => {
  const servers: Server[] = [];
  let url = "";
  let bare = "";
  let partial = "";
  beforeAll(async () => {
    const a = await serve(newStore());
    const b = await serve(newStore(), ["bee.v1.PolicyService"]);
    const c = await serve(newStore(), ["bee.v1.InsightsService", "bee.v1.UserService", "bee.v1.BillingService", "bee.v1.OpsService", "bee.v1.EnterpriseService"]);
    servers.push(a.server, b.server, c.server);
    url = a.url;
    bare = b.url;
    partial = c.url;
  });
  afterAll(async () => {
    for (const s of servers) {
      s.closeAllConnections();
      await new Promise((r) => s.close(r));
    }
  });

  it("every screen read returns identical results", async () => {
    const viaFixture = await screens(createClient(fixtureRpc(newStore(), TOKEN)));
    const viaHttp = await screens(createClient(httpRpc({ baseUrl: url, token: TOKEN })));
    expect(viaHttp).toEqual(viaFixture);
    // Sanity: the comparison covers real data, not empty answers.
    expect((viaFixture.changes as T.ListChangesResponse).changes.length).toBeGreaterThan(5);
    expect((viaFixture.events as T.Event[]).length).toBe(40);
    expect((viaFixture.agents as T.ListAgentsResponse).agents.some((a) => a.model)).toBe(true);
  });

  it("every screen write returns identical results, errors included", async () => {
    const viaFixture = await writes(createClient(fixtureRpc(newStore(), TOKEN)));
    const viaHttp = await writes(createClient(httpRpc({ baseUrl: url, token: TOKEN })));
    expect(viaHttp).toEqual(viaFixture);
    expect(viaFixture.leased).toMatchObject({ reason: "LEASED" });
    expect(viaFixture.tagTaken).toMatchObject({ reason: "TAG_EXISTS" });
  });

  it("the G1-G11 fields reach the screens from bee.v1 itself", async () => {
    const r = await screens(createClient(httpRpc({ baseUrl: url, token: TOKEN })));
    const agents = (r.agents as T.ListAgentsResponse).agents;
    expect(agents.some((a) => a.model && a.session && a.openChanges > 0)).toBe(true);
    expect((r.agent as T.AgentSummary).principal.display).toBe("triage");
    expect((r.tokens as T.ListTokensResponse).tokens.some((t) => t.used)).toBe(true);
    expect((r.evaluate as T.PolicyEvaluation).rules.length).toBeGreaterThan(0);
    expect((r.evaluate as T.PolicyEvaluation).decisions.length).toBeGreaterThan(0);
    expect((r.audit as T.ListAuditResponse).entries.some((e) => e.rule)).toBe(true);
    expect((r.deniedAudit as T.ListAuditResponse).entries.every((e) => !e.allowed)).toBe(true);
    expect((r.runs as T.ListRunsResponse).runs.some((x) => x.jobs.length > 0 && x.changeRequestId)).toBe(true);
    expect((r.queue as T.ListQueueResponse).entries.every((e) => e.title)).toBe(true);
    expect((r.orgLeases as T.ListLeasesResponse).leases.length).toBeGreaterThanOrEqual((r.leases as T.ListLeasesResponse).leases.length);
  });

  it("a service the server does not serve fails with reason UNIMPLEMENTED; nothing else changes", async () => {
    const full = await screens(createClient(fixtureRpc(newStore(), TOKEN)));
    const api = createClient(httpRpc({ baseUrl: bare, token: TOKEN }));
    await expect(api.policy.getPolicy({ repo })).rejects.toMatchObject({ code: "unimplemented", reason: "UNIMPLEMENTED" });
    const audit = await api.audit.listAudit({ org });
    expect(audit).toEqual(full.audit);
  });

  it("community, discovery, cloud and admin reads come back with real data", async () => {
    const fresh = await serve(newStore());
    servers.push(fresh.server);
    const r = await screens(createClient(httpRpc({ baseUrl: fresh.url, token: TOKEN })));
    const home = r.homePublic as T.RepoHome;
    expect(home.repo.fullName).toBe("lattice/engine");
    expect(home.readme?.markdown.length).toBeGreaterThan(40);
    expect(home.languages.length).toBeGreaterThan(0);
    expect(home.latestLand?.meta?.message).toBeTruthy();
    expect(home.leasesVisible).toBe(true);
    expect(home.cloneUrls.https).toBe("https://hub.bee.example/lattice/engine.git");
    expect(home.cloneUrls.ssh).toBe("git@hub.bee.example:lattice/engine.git");
    expect(Number(home.repo.stars)).toBeGreaterThan(100);
    expect(home.repo.topics.length).toBeGreaterThan(0);
    expect(r.homeHidden).toMatchObject({ error: "not_found", reason: "REPO_NOT_FOUND" });
    expect((r.myRepos as T.ListMyReposResponse).repos.map((x) => x.fullName)).toContain("acme/api");
    expect((r.myRepos as T.ListMyReposResponse).repos.map((x) => x.fullName)).not.toContain("lattice/internal-tools");

    const hits = (r.search as T.SearchReposResponse).hits;
    expect(hits.length).toBeGreaterThan(3);
    expect(hits.every((h) => h.visibility === "public")).toBe(true);
    expect(hits.map((h) => Number(h.starCount))).toEqual([...hits.map((h) => Number(h.starCount))].sort((a, b) => b - a));

    // Trending and the leaderboard count the same stars and lands.
    const trending = (r.trending as T.ListTrendingReposResponse).repos;
    expect(trending.map((x) => x.rank)).toEqual(trending.map((_, i) => i + 1));
    const month = new Map((r.boardRepos as T.ListLeaderboardResponse).entries.map((e) => [e.repo?.fullName, e.score]));
    expect(month.size).toBeGreaterThan(0);
    const agents = (r.boardAgents as T.ListLeaderboardResponse).entries;
    expect(agents.length).toBeGreaterThan(1);
    expect(agents.every((e) => e.user && !e.repo)).toBe(true);
    expect(agents.map((e) => Number(e.score))).toEqual([...agents.map((e) => Number(e.score))].sort((a, b) => b - a));
    expect(r.boardInvalid).toMatchObject({ error: "invalid_argument" });
    expect((r.publicActivity as T.ListPublicActivityResponse).items.length).toBeGreaterThan(2);
    expect((r.publicActivity as T.ListPublicActivityResponse).items.length).toBeLessThanOrEqual(20);

    expect((r.profileUser as T.Profile).kind).toBe("user");
    expect((r.profileAgent as T.Profile)).toMatchObject({ kind: "agent", runtime: "claude-code", ownerLogin: "rhea" });
    expect((r.profileOrg as T.Profile).kind).toBe("org");
    expect(r.profileMissing).toMatchObject({ error: "not_found", reason: "USER_NOT_FOUND" });
    const graph = r.graph as T.ContributionGraph;
    expect(graph.days).toHaveLength(365);
    expect(graph.days.reduce((n, d) => n + d.count, 0)).toBe(graph.total);
    expect((r.feed as T.ListHomeFeedResponse).events.length).toBeGreaterThan(3);
    expect((r.topic as T.Topic).topRepos.length).toBeGreaterThan(0);
    expect(r.topicMissing).toMatchObject({ error: "not_found" });
    expect((r.serverInfo as T.ServerInfo).edition).toBe("cloud");

    const plans = (r.plans as T.ListBillingPlansResponse).plans;
    expect(plans.map((p) => p.id)).toEqual(["free", "team", "enterprise"]);
    expect(plans.find((p) => p.id === "enterprise")?.selfServe).toBe(false);
    expect((r.usage as T.QueryUsageResponse).lineItems.length).toBeGreaterThan(10);
    expect((r.saml as T.SamlConfig).enabled).toBe(false);
    expect((r.domains as T.ListVerifiedDomainsResponse).domains).toHaveLength(2);
    expect((r.ipAllow as T.ListIpAllowListResponse).entries).toHaveLength(1);
    expect((r.configBad as T.ValidateOpsConfigResponse).valid).toBe(false);
  });

  it("an anonymous visitor reads public data through both adapters and nothing else", async () => {
    const fresh = await serve(newStore());
    servers.push(fresh.server);
    const viaFixture = await anonymous(createClient(fixtureRpc(newStore())));
    const viaHttp = await anonymous(createClient(httpRpc({ baseUrl: fresh.url })));
    expect(viaHttp).toEqual(viaFixture);
    const r = viaFixture;
    expect((r.homePublic as T.RepoHome).repo.visibility).toBe("VISIBILITY_PUBLIC");
    // Leases are a read of the repo, open to visitors of a public one.
    expect((r.homePublic as T.RepoHome).leasesVisible).toBe(true);
    expect((r.homePublic as T.RepoHome).starredByMe).toBe(false);
    expect((r.homeSdk as T.RepoHome).repo.fullName).toBe("acme/sdk");
    // Private, internal and missing repos look the same: not found.
    for (const k of ["homePrivate", "homeInternal", "homeMissing", "treePrivate"]) expect(r[k], k).toMatchObject({ error: "not_found", reason: "REPO_NOT_FOUND" });
    expect((r.search as T.SearchReposResponse).hits.every((h) => h.visibility === "public")).toBe(true);
    expect((r.searchPrivate as T.SearchReposResponse).hits).toHaveLength(0);
    expect((r.trending as T.ListTrendingReposResponse).repos.every((x) => x.repo?.visibility === "public")).toBe(true);
    expect((r.graph as T.ContributionGraph).includesPrivate).toBe(false);
    expect((r.tree as T.ListTreeResponse).entries.length).toBeGreaterThan(0);
    for (const k of ["myRepos", "feed", "star", "follow", "createOrg", "plans", "health", "enterprise", "agents", "members"]) expect(r[k], k).toMatchObject({ error: "unauthenticated" });
    expect((r.changes as T.ListChangesResponse).changes.length).toBeGreaterThan(0);
    expect((r.issues as T.ListIssuesResponse).issues.length).toBeGreaterThan(0);
    expect((r.runs as T.ListRunsResponse).runs.length).toBeGreaterThan(0);
    for (const k of ["changesPrivate", "issuesPrivate", "runsPrivate"]) expect(r[k], k).toMatchObject({ error: "not_found", reason: "REPO_NOT_FOUND" });
  });

  it("posts, follows and feed triggers read and write identically through both adapters", async () => {
    const fresh = await serve(newStore());
    servers.push(fresh.server);
    const viaFixture = createClient(fixtureRpc(newStore(), TOKEN));
    const viaHttp = createClient(httpRpc({ baseUrl: fresh.url, token: TOKEN }));
    const reads = await postScreens(viaFixture);
    expect(await postScreens(viaHttp)).toEqual(reads);
    const writes = await postWrites(viaFixture);
    expect(await postWrites(viaHttp)).toEqual(writes);
    expect(await postScreens(viaHttp)).toEqual(await postScreens(viaFixture));
    // Sanity: real data, and the refusals are the contract's reasons.
    expect((reads.feed as T.ListFollowingFeedResponse).items).toHaveLength(5);
    expect((reads.adoptRuns as T.ListAdoptRunsResponse).runs.map((x) => x.outcome)).toEqual(["opened_issue", "not_relevant"]);
    expect(reads.feedBadCursor).toMatchObject({ error: "invalid_argument", reason: "bad_page_token" });
    expect(reads.postMissing).toMatchObject({ error: "not_found", reason: "POST_NOT_FOUND" });
    expect(writes.empty).toMatchObject({ reason: "empty_post" });
    expect(writes.tooLong).toMatchObject({ reason: "text_too_long" });
    expect(writes.noAlt).toMatchObject({ reason: "alt_text_required" });
    expect(writes.svg).toMatchObject({ reason: "bad_media" });
    expect(writes.stale).toMatchObject({ reason: "VERSION_MISMATCH" });
    expect(writes.issueBad).toMatchObject({ reason: "post_not_readable" });
    expect((writes.postAfter as T.Post).counts?.adoptions).toBe("2");
  });

  it("signed-out visitors read public posts through both adapters and nothing else", async () => {
    const fresh = await serve(newStore());
    servers.push(fresh.server);
    const viaFixture = await postsAnonymous(createClient(fixtureRpc(newStore())));
    const viaHttp = await postsAnonymous(createClient(httpRpc({ baseUrl: fresh.url })));
    expect(viaHttp).toEqual(viaFixture);
    const r = viaFixture;
    expect((r.repoPosts as T.ListRepoPostsResponse).posts.length).toBeGreaterThan(2);
    expect((r.post as T.Post).viewer).toEqual({ liked: false, reposted: false, canEdit: false, canApprove: false, canWithdraw: false });
    expect(r.private).toMatchObject({ error: "not_found", reason: "REPO_NOT_FOUND" });
    expect((r.changes as unknown[][]).some((c) => c[1])).toBe(true);
    for (const k of ["feed", "like", "repost", "adoptions", "follow", "triggers", "set"]) expect(r[k], k).toMatchObject({ error: "unauthenticated" });
  });

  it("briefs, evidence, comments and sessions read and write identically through both adapters", async () => {
    const fresh = await serve(newStore());
    servers.push(fresh.server);
    const viaFixture = createClient(fixtureRpc(newStore(), TOKEN));
    const viaHttp = createClient(httpRpc({ baseUrl: fresh.url, token: TOKEN }));
    const reads = await briefScreens(viaFixture);
    expect(await briefScreens(viaHttp)).toEqual(reads);
    const writes = await briefWrites(viaFixture);
    expect(await briefWrites(viaHttp)).toEqual(writes);
    expect(await briefScreens(viaHttp)).toEqual(await briefScreens(viaFixture));
    expect((reads.evidence as T.ListEvidenceResponse).evidence).toHaveLength(6);
    expect(reads.missing).toMatchObject({ error: "not_found", reason: "EVIDENCE_NOT_FOUND" });
    expect(writes.long).toMatchObject({ error: "invalid_argument", reason: "brief_too_long" });
    expect(writes.http).toMatchObject({ reason: "bad_url" });
    expect(writes.kind).toMatchObject({ reason: "kind_immutable" });
    expect(writes.svg).toMatchObject({ reason: "bad_media" });
    expect(writes.gone).toMatchObject({ error: "not_found" });
  });

  it("signed-out visitors read public briefs through both adapters and cannot write", async () => {
    const fresh = await serve(newStore());
    servers.push(fresh.server);
    const viaFixture = await briefsAnonymous(createClient(fixtureRpc(newStore())));
    const viaHttp = await briefsAnonymous(createClient(httpRpc({ baseUrl: fresh.url })));
    expect(viaHttp).toEqual(viaFixture);
    expect(viaFixture.private).toMatchObject({ error: "not_found", reason: "REPO_NOT_FOUND" });
    expect(viaFixture.set).toMatchObject({ error: "unauthenticated" });
    expect(viaFixture.comments).toMatchObject({ error: "unauthenticated" });
  });

  it("a server missing a service fails those methods with UNIMPLEMENTED and never with fake data", async () => {
    const api = createClient(httpRpc({ baseUrl: partial, token: TOKEN }));
    const unimplemented = { code: "unimplemented", reason: "UNIMPLEMENTED" };
    await expect(api.user.starRepo({ repo: { fullName: "lattice/engine" } })).rejects.toMatchObject(unimplemented);
    await expect(api.user.getProfile({ owner: "rhea" })).rejects.toMatchObject(unimplemented);
    await expect(api.billing.listBillingPlans({})).rejects.toMatchObject(unimplemented);
    await expect(api.ops.getOpsHealth({})).rejects.toMatchObject(unimplemented);
    await expect(api.enterprise.getEnterprise({ enterprise: "acme-group" })).rejects.toMatchObject(unimplemented);
    // Repo home reads languages, leases and the star state from services this server lacks: it fails
    // instead of showing a repo home with the parts quietly missing.
    await expect(api.repo.getRepoHome({ fullName: "lattice/engine" })).rejects.toMatchObject(unimplemented);
    // What the server does serve is untouched.
    expect((await api.search.listLeaderboard({ window: WEEK, subject: "LEADERBOARD_SUBJECT_AGENTS", metric: "LEADERBOARD_METRIC_LANDS" })).entries.length).toBeGreaterThan(0);
  });
});

