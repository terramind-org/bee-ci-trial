/**
 * The fixture adapter's in-memory world. Content is deterministic: the same options produce
 * the same ids, files, reviews and event order. Time is an anchor plus an injectable clock, so
 * tests pin it and the dev server lets it run. A simulated stream of agent work (`step()`)
 * drives the Activity view.
 */
import { diffText } from "../../diff";
import { classifyChange, highestClass } from "../../diffclass";
import { objectIdText, pathBytes, pathText, textToBase64 } from "../ids";
import { BeeError } from "../transport";
import type * as T from "../types";
import { describeRules, evaluateLand, ownersFor, parsePolicy, type LandRules } from "./policy";
import { changeId, digestHex, objectId, ulid } from "./seed";
import { ACME_ORG_PROFILE, COMMUNITY_ISSUES, COMMUNITY_REPOS, FOLLOWS, LATTICE_ORG, STARS } from "./community-world";
import { CRS, DOC_TOPICS, HISTORY, LEASES, PRINCIPALS, REPOS, type CrSeed } from "./world";
import { languageStats, type LanguageSummary } from "./languages";
import { PostsWorld } from "./posts";
import { BriefsWorld } from "./briefs";
import { fnv1a, prng } from "./seed";

export const DEFAULT_ANCHOR = Date.UTC(2026, 9, 18, 9, 0, 0);
const ORG = "acme";

export interface StoreOptions {
  /** Wall-clock ms that "now" maps to when the clock reads its first value. */
  anchor?: number;
  /** Monotonic ms source. Tests pass a fixed or manual clock. */
  clock?: () => number;
  /** Run the live simulation on a timer every `tickMs`. Off by default (tests call step()). */
  live?: boolean;
  tickMs?: number;
  /** How fast running jobs print log lines, in ms per line. */
  logMs?: number;
  /** How long a simulated import takes to reach LIVE, in ms. */
  importMs?: number;
}

export interface Commit {
  key: string;
  repo: string;
  id: T.ObjectId;
  idText: string;
  parent: Commit | null;
  files: Map<string, string>;
  changed: string[];
  author: string;
  time: number;
  message: string;
  changeId: T.ChangeId;
  provenance?: T.Provenance;
  generation: number;
}

interface RefState {
  target: Commit;
  version: number;
  log: T.RefLogEntry[];
}

export interface RepoState {
  /** The wire record as created; derived counters are filled in by `repoView`. */
  repo: T.Repo;
  /** Store key: the short name for acme, "owner/name" for every other org. */
  name: string;
  /** Owning org's name. */
  owner: string;
  license: string;
  homepage: string;
  archived: boolean;
  topics: string[];
  main: Commit[];
  refs: Map<string, RefState>;
  restricted: Set<string>;
}

export interface RevState {
  n: number;
  head: Commit;
  base: Commit;
  createdAt: number;
  diffClass: T.DiffClass;
  perPath: Record<string, T.DiffClass>;
  contentChanged: boolean;
}

export interface CrState {
  id: string;
  key: string;
  /** Per-repo number shared with issues; the fixture numbers change requests from 101. */
  number: number;
  repo: string;
  author: string;
  title: string;
  body: string;
  createdAt: number;
  state: T.ChangeState;
  dependsOn: string;
  changeId: T.ChangeId;
  revisions: RevState[];
  reviews: T.Review[];
  threads: T.ReviewThread[];
  ws: string;
  /** The post this change was inspired by. */
  inspiredByPostId?: string;
}

interface QueueState {
  crId: string;
  state: T.QueueEntry["state"];
  enqueuedAt: number;
  train: string;
}

interface LiveJob {
  runId: string;
  jobId: string;
  startedAt: number;
  initial: number;
  outcome: T.RunStatus;
}

interface ImportState {
  imp: T.Import;
  started: number;
  totals: Record<string, number>;
}

type Listener = (e: T.Event) => void;

/** An org with its members; agents belong through their owner. */
export interface OrgRecord {
  org: T.Org;
  description: string;
  website: string;
  location: string;
  email: string;
  members: Map<string, T.Role>; // by principal handle
}

/** One star or fork. `who` is a login; external rows stand for people outside the demo world. */
export interface StarRow {
  who: string;
  at: number;
}

export interface FollowRow {
  from: string; // login
  to: string; // login
  at: number;
}

/** Issues as stored; counts are derived when they are served. */
export type IssueRow = Omit<T.Issue, "subIssuesTotal" | "subIssuesDone" | "commentCount" | "inspiredBy"> & {
  /** The post this issue was inspired by; `inspiredBy` is derived from it for each reader. */
  inspiredByPostId?: string;
};
/** Log lines as stored; byte offsets are derived when the log is served. */
export type LogRow = Omit<T.LogLine, "end">;

const RISKY_TOKEN = "invalid";

export class FixtureStore {
  readonly anchor: number;
  private readonly clock: () => number;
  private readonly clock0: number;
  readonly logMs: number;
  readonly importMs: number;
  private timer: ReturnType<typeof setInterval> | null = null;
  private stepN = 0;

  org: T.Org;
  orgs = new Map<string, OrgRecord>(); // by org name
  /** Star and fork tables by repo key. Stars are what the repo counters and trending read. */
  stars = new Map<string, StarRow[]>();
  forks = new Map<string, number[]>();
  follows: FollowRow[] = [];
  /** Posts, repo follows, likes, reposts, adoptions and the agents' feed triggers. */
  posts = new PostsWorld();
  /** Briefs, evidence and the conversation comments of change requests. */
  briefs = new BriefsWorld();
  principals = new Map<string, T.Principal>(); // by handle
  byId = new Map<string, string>(); // principal id -> handle
  agentMeta = new Map<string, { purpose: string; model: string }>();
  /** Public login of each human and agent: the handle for humans, a slug of the display name for agents. */
  loginByHandle = new Map<string, string>();
  handleByLogin = new Map<string, string>(); // lower-case login -> handle
  private langCache = new Map<string, LanguageSummary>(); // by tip idText + options
  repos = new Map<string, RepoState>(); // by name
  commits = new Map<string, Commit>(); // by idText
  crs: CrState[] = [];
  checks = new Map<string, T.CheckRun[]>(); // by head idText
  leases: T.Lease[] = [];
  queue = new Map<string, QueueState[]>(); // by repo name
  runs: T.Run[] = [];
  logs = new Map<string, LogRow[]>(); // by run id
  live: LiveJob[] = [];
  pendingChecks: string[] = []; // head idTexts whose checks are running
  events: T.Event[] = [];
  private listeners = new Set<Listener>();
  issues: IssueRow[] = [];
  projects = new Map<string, T.Project>();
  releases: T.Release[] = [];
  alerts: T.SecurityAlert[] = [];
  audit: T.AuditEntry[] = [];
  tokens: T.TokenInfo[] = [];
  tokenSecrets = new Map<string, string>(); // secret -> token id
  imports: ImportState[] = [];
  deviceCodes = new Map<string, number>();
  private seq = 0;
  private counter = 0;

  constructor(opts: StoreOptions = {}) {
    this.anchor = opts.anchor ?? DEFAULT_ANCHOR;
    this.clock = opts.clock ?? (() => Date.now());
    this.clock0 = this.clock();
    this.logMs = opts.logMs ?? 400;
    this.importMs = opts.importMs ?? 8000;
    this.org = { id: ulid(this.anchor - 400 * 86_400_000, "org:acme"), name: ORG, display: "Acme Robotics", createdAt: this.iso(this.ago(400 * 1440)) };
    this.build();
    if (opts.live) this.start(opts.tickMs ?? 4000);
  }

  // ------------------------------------------------------------------ time

  now(): number {
    return this.anchor + (this.clock() - this.clock0);
  }
  ago(minutes: number): number {
    return this.anchor - minutes * 60_000;
  }
  iso(ms: number): string {
    return new Date(ms).toISOString();
  }
  /** Next change request number in a repo. */
  nextNumber(repo: string): number {
    return 101 + this.crs.filter((c) => c.repo === repo).length;
  }
  private nextKey(prefix: string): string {
    this.counter++;
    return prefix + ":" + this.counter;
  }
  private newId(prefix: string): string {
    return ulid(this.now(), this.nextKey(prefix));
  }

  start(tickMs: number): void {
    if (this.timer) return;
    this.timer = setInterval(() => this.step(), tickMs);
    const t = this.timer as unknown as { unref?: () => void };
    t.unref?.();
  }
  stop(): void {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
  }

  // ------------------------------------------------------------------ lookups

  p(handle: string): T.Principal {
    const pr = this.principals.get(handle);
    if (!pr) throw new Error("unknown principal " + handle);
    return pr;
  }
  handleOf(id: string): string {
    return this.byId.get(id) ?? id;
  }
  kindOf(handle: string): "human" | "agent" | "service" {
    const k = this.principals.get(handle)?.kind;
    return k === "PRINCIPAL_KIND_AGENT" ? "agent" : k === "PRINCIPAL_KIND_SERVICE" ? "service" : "human";
  }
  /** The repo a ref names, if there is one. */
  lookup(ref: { id?: string; fullName?: string } | undefined): RepoState | undefined {
    const name = ref?.fullName ? ref.fullName.replace(/^acme\//, "") : ref?.id ? [...this.repos.values()].find((r) => r.repo.id === ref.id)?.name : undefined;
    return name ? this.repos.get(name) : undefined;
  }
  repo(ref: { id?: string; fullName?: string } | undefined): RepoState {
    const r = this.lookup(ref);
    if (!r) throw new BeeError("not_found", { reason: "REPO_NOT_FOUND", message: "no repository " + (ref?.fullName ?? ref?.id ?? "(none)") });
    return r;
  }
  /**
   * The repos behind an org name or an "org/*" wildcard (org-wide activity), limited to those
   * `viewer` may read; otherwise the one repo the ref names.
   */
  reposFor(ref: { id?: string; fullName?: string } | undefined, org?: string, viewer?: string): RepoState[] {
    const wildcard = ref?.fullName && ref.fullName.endsWith("/*") ? ref.fullName.slice(0, -2) : undefined;
    const name = org || wildcard;
    if (name) {
      if (!this.orgs.has(name)) throw new BeeError("not_found", { reason: "NOT_FOUND", message: "no org " + name });
      return [...this.repos.values()].filter((r) => r.owner === name && this.canRead(viewer, r));
    }
    return [this.repo(ref)];
  }
  orgRecord(name: string | undefined): OrgRecord {
    const rec = [...this.orgs.values()].find((o) => o.org.name === name || o.org.id === name);
    if (!rec) throw new BeeError("not_found", { reason: "ORG_NOT_FOUND", message: "no org " + name });
    return rec;
  }
  /** Whether a principal belongs to an org: as a member, through the human who owns it, or as acme's service account. */
  inOrg(handle: string, org: string): boolean {
    const rec = this.orgs.get(org);
    if (!rec) return false;
    if (rec.members.has(handle)) return true;
    const pr = this.principals.get(handle);
    if (!pr) return false;
    if (pr.kind === "PRINCIPAL_KIND_SERVICE") return org === ORG;
    return pr.ownerId ? this.inOrg(this.handleOf(pr.ownerId), org) : false;
  }
  /** Public repos are readable by everyone, anonymous included; the rest by members of the owning org. */
  canRead(viewer: string | undefined, r: RepoState): boolean {
    if (r.repo.visibility === "VISIBILITY_PUBLIC") return true;
    return viewer !== undefined && this.inOrg(viewer, r.owner);
  }
  /** The signed-in principal behind a token, or undefined for an anonymous request. */
  viewer(token: string | undefined): string | undefined {
    return token ? this.caller(token) : undefined;
  }
  readableRepos(viewer: string | undefined): RepoState[] {
    return [...this.repos.values()].filter((r) => this.canRead(viewer, r));
  }
  /** Language summary at a commit, cached: commits never change. */
  languagesAt(c: Commit, opts: { includeDocumentation?: boolean; includeGenerated?: boolean } = {}): LanguageSummary {
    const key = c.idText + (opts.includeDocumentation ? "d" : "") + (opts.includeGenerated ? "g" : "");
    let v = this.langCache.get(key);
    if (!v) {
      v = languageStats(c.files, opts);
      this.langCache.set(key, v);
    }
    return v;
  }
  /** The repo record with every derived counter read from the world's tables. */
  repoView(r: RepoState): T.Repo {
    const tip = r.refs.get(r.repo.defaultRef)?.target;
    return {
      ...r.repo,
      archived: r.archived,
      topics: [...r.topics],
      homepage: r.homepage,
      pushedAt: tip ? this.iso(tip.time) : undefined,
      stars: String(this.stars.get(r.name)?.length ?? 0),
      forks: String(this.forks.get(r.name)?.length ?? 0),
      openIssues: String(this.issues.filter((i) => i.repoId === r.repo.id && i.state === "open").length),
      openChanges: String(this.crs.filter((c) => c.repo === r.name && c.state === "CHANGE_STATE_OPEN").length),
      licenseSpdx: r.license,
      primaryLanguage: tip ? this.languagesAt(tip).primary : "",
    };
  }
  cr(repo: RepoState, id: string): CrState {
    const c = this.crs.find((x) => x.repo === repo.name && (x.id === id || x.key === id));
    if (!c) throw new BeeError("not_found", { reason: "CHANGE_NOT_FOUND", message: "no change request " + id + " in " + repo.repo.fullName });
    return c;
  }
  crByKey(key: string): CrState {
    const c = this.crs.find((x) => x.key === key);
    if (!c) throw new Error("unknown cr " + key);
    return c;
  }
  current(c: CrState): RevState {
    return c.revisions[c.revisions.length - 1] as RevState;
  }
  tip(repo: RepoState): Commit {
    const t = repo.refs.get(repo.repo.defaultRef)?.target ?? repo.main[repo.main.length - 1];
    if (!t) throw new BeeError("failed_precondition", { reason: "REPO_EMPTY", message: repo.repo.fullName + " has no commits yet" });
    return t;
  }
  /** Policy epoch of a policy source: a stable label of its content. */
  policyEpoch(source: string): string {
    return "pe_" + digestHex("epoch:" + source, 6);
  }
  rules(repo: RepoState): LandRules {
    const src = repo.main.length ? this.tip(repo).files.get(".bee/policy.toml") : undefined;
    return parsePolicy(src ?? "").rules;
  }

  // ------------------------------------------------------------------ build

  private build(): void {
    for (const s of PRINCIPALS) {
      const id = ulid(this.anchor - 300 * 86_400_000, "principal:" + s.handle);
      this.principals.set(s.handle, {
        id,
        kind: s.kind,
        display: s.display,
        ownerId: "",
        runtime: s.runtime ?? "",
        externalIds: s.github ? ["github:" + s.github] : [],
      });
      this.byId.set(id, s.handle);
      if (s.kind === "PRINCIPAL_KIND_AGENT") this.agentMeta.set(s.handle, { purpose: s.purpose ?? "", model: s.model ?? "" });
    }
    for (const s of PRINCIPALS) if (s.owner) this.p(s.handle).ownerId = this.p(s.owner).id;
    for (const s of PRINCIPALS) {
      if (s.kind === "PRINCIPAL_KIND_SERVICE") continue;
      const login = s.kind === "PRINCIPAL_KIND_HUMAN" ? s.handle : s.display.toLowerCase().replace(/[^a-z0-9]+/g, "-");
      this.loginByHandle.set(s.handle, login);
      this.handleByLogin.set(login.toLowerCase(), s.handle);
    }

    // Orgs and who belongs to them.
    const acme = { org: this.org, ...ACME_ORG_PROFILE, members: new Map<string, T.Role>() };
    const latticeOrg: T.Org = { id: ulid(this.anchor - LATTICE_ORG.ageDays * 86_400_000, "org:" + LATTICE_ORG.name), name: LATTICE_ORG.name, display: LATTICE_ORG.display, createdAt: this.iso(this.ago(LATTICE_ORG.ageDays * 1440)) };
    const lattice = { org: latticeOrg, description: LATTICE_ORG.description, website: LATTICE_ORG.website, location: LATTICE_ORG.location, email: LATTICE_ORG.email, members: new Map<string, T.Role>() };
    this.orgs.set(ORG, acme);
    this.orgs.set(lattice.org.name, lattice);
    for (const s of PRINCIPALS) if (s.role) this.orgs.get(s.org ?? ORG)?.members.set(s.handle, s.role);

    for (const r of REPOS) {
      const owner = this.orgs.get(r.owner ?? ORG) as OrgRecord;
      this.addRepo(owner, r.name, {
        visibility: r.visibility,
        description: r.description,
        topics: r.topics ?? [],
        license: r.license ?? "",
        homepage: r.homepage ?? "",
        createdAt: this.ago(43_300),
        idAt: this.anchor - 200 * 86_400_000,
        restricted: r.restricted,
      });
    }
    this.buildStars();

    const timeline: { t: number; order: number; ev: Omit<T.Event, "seq"> }[] = [];
    const push = (t: number, ev: Omit<T.Event, "seq">) => timeline.push({ t, order: timeline.length, ev });

    // History of main.
    for (const h of HISTORY) {
      const repo = this.repos.get(h.repo) as RepoState;
      const parent = repo.main[repo.main.length - 1] ?? null;
      const commit = this.makeCommit(repo.name, parent, h.files, h.author, this.ago(h.ago), h.message, h.repo + "@" + repo.main.length, h.session, h.costCents);
      const version = this.advanceMain(repo, commit, h.author);
      push(commit.time, this.ev(repo, "ref.updated", h.author, commit.time, { refUpdated: { name: "main", target: commit.id, version: String(version) } }));
    }

    // Change requests.
    for (const s of CRS) this.buildCr(s, push);

    // Leases.
    for (const l of LEASES) {
      const repo = this.repos.get(l.repo) as RepoState;
      const lease: T.Lease = {
        id: ulid(this.ago(l.expiresIn > 60 ? 30 : 90), "lease:" + l.key),
        repoId: repo.repo.id,
        ref: "main",
        paths: l.paths.map(pathBytes),
        holder: this.p(l.holder),
        changeId: l.cr ? this.crByKey(l.cr).changeId : undefined,
        mode: l.mode,
        expiresAt: this.iso(this.anchor + l.expiresIn * 60_000),
        note: l.note,
      };
      this.leases.push(lease);
      push(this.anchor + l.expiresIn * 60_000 - 120 * 60_000, this.ev(repo, "lease.acquired", l.holder, this.anchor + l.expiresIn * 60_000 - 120 * 60_000, { lease }));
    }

    this.buildIssues(push);
    this.buildCommunityIssues(push);
    this.buildReleases(push);
    this.buildAlerts();

    timeline.sort((a, b) => a.t - b.t || a.order - b.order);
    for (const { ev } of timeline) this.events.push({ ...ev, seq: String(++this.seq) });

    this.buildTokens();
    this.buildAudit();
    this.buildImports();
    this.posts.seed(this);
    this.briefs.seed(this);
  }

  /** Adds an empty repo under an org. Seeds and CreateRepo both come through here. */
  addRepo(
    owner: OrgRecord,
    name: string,
    o: { visibility: T.Visibility; description: string; topics: string[]; license: string; homepage: string; createdAt: number; idAt: number; restricted?: string[] },
  ): RepoState {
    const key = owner.org.name === ORG ? name : owner.org.name + "/" + name;
    const state: RepoState = {
      name: key,
      owner: owner.org.name,
      license: o.license,
      homepage: o.homepage,
      archived: false,
      topics: [...o.topics],
      repo: {
        id: ulid(o.idAt, "repo:" + key),
        orgId: owner.org.id,
        name,
        fullName: owner.org.name + "/" + name,
        visibility: o.visibility,
        defaultRef: "main",
        description: o.description,
        createdAt: this.iso(o.createdAt),
        archived: false,
        topics: [...o.topics],
        homepage: o.homepage,
        stars: "0",
        forks: "0",
        openIssues: "0",
        openChanges: "0",
        licenseSpdx: o.license,
        primaryLanguage: "",
      },
      main: [],
      refs: new Map(),
      restricted: new Set(o.restricted ?? []),
    };
    this.repos.set(key, state);
    return state;
  }

  /** Appends a commit to main and moves the ref; returns the new ref version. */
  advanceMain(repo: RepoState, commit: Commit, author: string): number {
    repo.main.push(commit);
    const ref = repo.refs.get("main");
    const version = (ref?.version ?? 0) + 1;
    const entry: T.RefLogEntry = {
      version: String(version),
      old: ref?.target.id,
      new: commit.id,
      principal: this.p(author).id,
      time: this.iso(commit.time),
      landed: [{ changeId: commit.changeId, source: commit.id, result: commit.id, skipped: false }],
    };
    repo.refs.set("main", { target: commit, version, log: [...(ref?.log ?? []), entry] });
    return version;
  }

  /** Star and fork tables: named stargazers from the seeds plus deterministic outsiders. */
  private buildStars(): void {
    const outsiders = (key: string, kind: "stars" | "forks", total: number, heat: number): StarRow[] => {
      const rnd = prng(fnv1a(kind + ":" + key));
      const rows: StarRow[] = [];
      for (let i = 0; i < total; i++) rows.push({ who: kind.slice(0, -1) + "r-" + i, at: this.ago(Math.floor(Math.pow(rnd(), heat) * 180 * 1440)) });
      return rows;
    };
    for (const r of COMMUNITY_REPOS) {
      const key = r.owner === ORG || !r.owner ? r.name : r.owner + "/" + r.name;
      this.stars.set(key, outsiders(key, "stars", r.externalStars, r.heat));
      this.forks.set(key, outsiders(key, "forks", r.externalForks, r.heat).map((x) => x.at));
    }
    for (const st of STARS) {
      const rows = this.stars.get(st.repo) ?? [];
      rows.push({ who: st.who, at: this.ago(st.ago) });
      this.stars.set(st.repo, rows);
    }
    for (const rows of this.stars.values()) rows.sort((a, b) => a.at - b.at || a.who.localeCompare(b.who));
    for (const f of FOLLOWS) this.follows.push({ from: f.from, to: f.to, at: this.ago(f.ago) });
  }

  private buildCommunityIssues(push: (t: number, ev: Omit<T.Event, "seq">) => void): void {
    for (const i of COMMUNITY_ISSUES) {
      const repo = this.repos.get(i.repo) as RepoState;
      const at = this.ago(i.ago);
      this.issues.push({
        id: ulid(at, "issue:" + i.repo + ":" + i.number),
        repoId: repo.repo.id,
        number: i.number,
        title: i.title,
        body: i.body,
        type: i.type,
        labels: i.labels,
        state: "open",
        milestone: "",
        parentNumber: 0,
        linkedChanges: [],
        assignees: [],
        author: this.p(i.author),
        subIssues: [],
        comments: [],
        createdAt: this.iso(at),
      });
      push(at, this.ev(repo, "issue.opened", i.author, at, { note: { title: "#" + i.number + " " + i.title, target: "issue:" + i.number } }));
    }
  }

  makeCommit(
    repo: string,
    parent: Commit | null,
    files: Record<string, string | null>,
    author: string,
    time: number,
    message: string,
    key: string,
    session?: string,
    costCents?: number,
  ): Commit {
    const snapshot = new Map(parent?.files ?? []);
    for (const [path, content] of Object.entries(files)) {
      if (content === null) snapshot.delete(path);
      else snapshot.set(path, content);
    }
    const id = objectId("changeset", key);
    const pr = this.p(author);
    const isAgent = pr.kind === "PRINCIPAL_KIND_AGENT";
    const owner = isAgent ? this.handleOf(pr.ownerId) : "";
    const commit: Commit = {
      key,
      repo,
      id,
      idText: objectIdText(id),
      parent,
      files: snapshot,
      changed: Object.keys(files).sort(),
      author,
      time,
      message,
      changeId: changeId(key),
      generation: (parent?.generation ?? 0) + 1,
      provenance: isAgent
        ? {
            principalChain: [pr.id, this.p(owner).id],
            runtime: pr.runtime + (pr.runtime === "codex" ? "/0.48" : "/3.2"),
            model: this.agentMeta.get(author)?.model ?? "",
            session: session ?? "ses_" + digestHex("session:" + key, 6),
            promptDigest: textToBase64(digestHex("prompt:" + key, 16)),
            checksRun: [
              { name: "bun test", result: "passed", durationMs: String(4000 + (key.length * 337) % 9000) },
              { name: "bun run lint", result: "passed", durationMs: String(900 + (key.length * 71) % 700) },
            ],
            cost: {
              inputTokens: String((costCents ?? 10) * 9_400),
              outputTokens: String((costCents ?? 10) * 610),
              usdMicros: String((costCents ?? 10) * 10_000),
            },
          }
        : undefined,
    };
    this.commits.set(commit.idText, commit);
    return commit;
  }

  private buildCr(s: CrSeed, push: (t: number, ev: Omit<T.Event, "seq">) => void): void {
    const repo = this.repos.get(s.repo) as RepoState;
    const id = ulid(this.ago(s.ago), "cr:" + s.key);
    const c: CrState = {
      id,
      key: s.key,
      number: this.nextNumber(s.repo),
      repo: s.repo,
      author: s.author,
      title: s.title,
      body: s.body,
      createdAt: this.ago(s.ago),
      state: s.state,
      dependsOn: "",
      changeId: changeId("cr:" + s.key),
      revisions: [],
      reviews: [],
      threads: [],
      ws: s.ws,
    };
    if (s.dependsOn) c.dependsOn = this.crByKey(s.dependsOn).id;
    this.crs.push(c);
    s.revisions.forEach((r, i) => {
      const base = repo.main[r.base] as Commit;
      const head = this.makeCommit(repo.name, base, r.files, s.author, this.ago(r.ago), s.title, "cr:" + s.key + "@" + (i + 1), r.session, r.costCents);
      head.changeId = c.changeId;
      const rev = this.makeRev(i + 1, head, base, this.ago(r.ago), r.contentChanged);
      c.revisions.push(rev);
      const checks: T.CheckRun[] = Object.entries(r.checks).map(([name, status]) => this.makeCheck(repo, head, name, status, this.ago(r.ago) + 60_000));
      this.checks.set(head.idText, checks);
      this.buildRun(repo, c, rev, checks);
      push(rev.createdAt, this.ev(repo, i === 0 ? "cr.opened" : "cr.revised", s.author, rev.createdAt, { change: this.crView(c) }));
      for (const ch of checks)
        if (ch.status !== "CHECK_STATUS_QUEUED") push(rev.createdAt + 120_000 + checks.indexOf(ch) * 30_000, this.ev(repo, "check.updated", "ci", rev.createdAt + 120_000, { check: ch }));
    });
    repo.refs.set(s.ws, { target: this.current(c).head, version: c.revisions.length, log: [] });
    for (const rv of s.reviews) {
      const review: T.Review = {
        id: ulid(this.ago(rv.ago), "review:" + s.key + ":" + rv.by + ":" + rv.rev),
        changeId: id,
        revision: rv.rev,
        reviewer: this.p(rv.by),
        verdict: rv.verdict,
        body: rv.body,
        comments: [],
        createdAt: this.iso(this.ago(rv.ago)),
      };
      c.reviews.push(review);
      push(this.ago(rv.ago), this.ev(repo, "review.submitted", rv.by, this.ago(rv.ago), { review }));
    }
    for (const t of s.threads) {
      c.threads.push({
        id: ulid(this.ago(t.comments[0]?.ago ?? 0), "thread:" + t.key),
        changeRequestId: id,
        path: pathBytes(t.path),
        side: "new",
        startLine: t.start,
        endLine: t.end,
        revision: t.rev,
        resolved: t.resolved,
        outdated: t.outdated ?? false,
        comments: t.comments.map((cm, i) => ({
          id: ulid(this.ago(cm.ago), "comment:" + t.key + ":" + i),
          author: this.p(cm.by),
          body: cm.body,
          suggestion: cm.suggestion ?? "",
          createdAt: this.iso(this.ago(cm.ago)),
        })),
      });
    }
    if (s.state === "CHANGE_STATE_LANDED" && s.landedAs !== undefined) {
      const landed = repo.main[s.landedAs] as Commit;
      push(landed.time + 1, this.ev(repo, "cr.landed", s.author, landed.time + 1, { change: this.crView(c) }));
    }
    if (s.queued) {
      const list = this.queue.get(repo.name) ?? [];
      list.push({ crId: id, state: s.queued, enqueuedAt: this.ago(s.ago - 10), train: "train-" + digestHex("train:" + s.key, 3) });
      this.queue.set(repo.name, list);
    }
  }

  makeRev(n: number, head: Commit, base: Commit, createdAt: number, contentChanged: boolean): RevState {
    const paths = new Set([...head.files.keys(), ...base.files.keys()]);
    const files: { path: string; oldText: string | null; newText: string | null }[] = [];
    for (const path of [...paths].sort()) {
      const o = base.files.get(path) ?? null;
      const nw = head.files.get(path) ?? null;
      if (o !== nw) files.push({ path, oldText: o, newText: nw });
    }
    const cls = classifyChange(files);
    return { n, head, base, createdAt, diffClass: files.length ? cls.overall : "generated", perPath: cls.perPath, contentChanged };
  }

  makeCheck(repo: RepoState, head: Commit, name: string, status: T.CheckStatus, startedAt: number): T.CheckRun {
    const done = status === "CHECK_STATUS_PASSED" || status === "CHECK_STATUS_FAILED";
    return {
      id: ulid(startedAt, "check:" + head.key + ":" + name),
      repoId: repo.repo.id,
      changeset: head.id,
      name,
      status,
      url: "",
      summary: summaryFor(name, status),
      postLand: false,
      startedAt: status === "CHECK_STATUS_QUEUED" ? undefined : this.iso(startedAt),
      finishedAt: done ? this.iso(startedAt + durationFor(name)) : undefined,
    };
  }

  /** Every head gets one `ci` run with test, lint and e2e jobs that mirror its checks. */
  private buildRun(repo: RepoState, c: CrState | null, rev: RevState | { head: Commit; createdAt: number }, checks: T.CheckRun[]): T.Run {
    const head = rev.head;
    const id = ulid(rev.createdAt + 30_000, "run:" + head.key);
    const jobs: T.RunJob[] = checks.map((ch) => {
      const status = runStatusOf(ch.status);
      return {
        id: ulid(rev.createdAt + 30_000, "job:" + head.key + ":" + ch.name),
        name: ch.name,
        status,
        steps: stepsFor(ch.name, status),
        startedAt: ch.startedAt,
        finishedAt: ch.finishedAt,
        runner: "linux-arm64 · cell eu1",
      };
    });
    const run: T.Run = {
      id,
      repoId: repo.repo.id,
      workflow: "ci",
      number: this.runs.filter((r) => r.repoId === repo.repo.id).length + 1,
      trigger: c ? "change_request" : "land",
      ref: c ? c.ws : "main",
      head: head.id,
      changeRequestId: c?.id ?? "",
      actor: this.p(c ? c.author : head.author),
      status: aggregate(jobs.map((j) => j.status)),
      jobs,
      annotations: annotationsFor(head.key, checks),
      attempt: 1,
      startedAt: this.iso(rev.createdAt + 30_000),
      finishedAt: jobs.every((j) => j.finishedAt) ? jobs.map((j) => j.finishedAt as string).sort().pop() : undefined,
    };
    const lines: LogRow[] = [];
    for (const j of jobs) lines.push(...logFor(j, head.key, run.annotations));
    this.logs.set(id, lines);
    for (const ch of checks) {
      const job = jobs.find((j) => j.name === ch.name);
      ch.url = "/runs/" + id + (job ? "?job=" + job.id : "");
    }
    for (const j of jobs)
      if (j.status === "running") {
        const total = lines.filter((l) => l.jobId === j.id).length;
        this.live.push({ runId: id, jobId: j.id, startedAt: this.now(), initial: Math.min(total - 1, 12), outcome: "passed" });
      }
    this.runs.push(run);
    return run;
  }

  private buildIssues(push: (t: number, ev: Omit<T.Event, "seq">) => void): void {
    const api = this.repos.get("api") as RepoState;
    const cr = (k: string) => this.crByKey(k).id;
    const seeds: Omit<IssueRow, "id" | "repoId" | "assignees" | "author" | "comments" | "createdAt" | "subIssues">[] = [];
    const meta: { assignees: string[]; author: string; ago: number; comments: { by: string; body: string; ago: number }[] }[] = [];
    const add = (
      number: number,
      title: string,
      type: IssueRow["type"],
      labels: string[],
      assignees: string[],
      author: string,
      ago: number,
      extra: Partial<Pick<IssueRow, "state" | "parentNumber" | "linkedChanges" | "milestone" | "body">> = {},
      comments: { by: string; body: string; ago: number }[] = [],
    ) => {
      seeds.push({ number, title, type, labels, state: "open", milestone: "", parentNumber: 0, linkedChanges: [], body: "", ...extra });
      meta.push({ assignees, author, ago, comments });
    };
    add(11, "Health route should report the build sha", "feature", ["observability"], ["mara"], "lin", 5_000, { state: "closed", body: "Operators can't tell which build answered a health check." });
    add(12, "Sessions survive a role downgrade", "bug", ["auth", "security"], ["triage"], "sam", 3_200, {
      body: "Demoting an admin to viewer leaves their existing session with admin claims until it idles out.\n\nSplit into #13 (rotate on role change) and #14 (audit the rotation).",
      linkedChanges: [cr("rotate")],
      milestone: "October hardening",
    }, [
      { by: "mara", body: "Assigning to triage. Take the src/auth lease before you start.", ago: 3_100 },
      { by: "triage", body: "Lease acquired (exclusive, src/auth). Plan: rotate ids on role change, then add an audit entry.", ago: 3_000 },
    ]);
    add(13, "Rotate the session id when a role changes", "task", ["auth"], ["triage"], "triage", 3_000, { parentNumber: 12, linkedChanges: [cr("rotate")], milestone: "October hardening" });
    add(14, "Write an audit entry when a session rotates", "task", ["auth"], [], "triage", 2_990, { parentNumber: 12, milestone: "October hardening" });
    add(15, "Tax lines drift by a cent on multi-line invoices", "bug", ["billing"], ["billing"], "dev", 2_000, {
      body: "Three lines of 3.33, 3.33, 3.34 EUR at 19% sum to 1.89 in line tax but 1.90 on the total.",
      linkedChanges: [cr("tax")],
    });
    add(16, "Invoice PDF should show per-line tax", "feature", ["billing"], ["billing"], "dev", 1_000, { linkedChanges: [cr("pdf")] });
    add(17, "Document the webhook retry schedule", "task", ["docs"], ["docs"], "lin", 400, { linkedChanges: [cr("webhooks")] });
    add(18, "p99 on /health rose from 4 ms to 31 ms after hono 4.6.3", "bug", ["perf", "observability"], ["watcher"], "watcher", 1_700, {
      body: [
        "Detected by latency-watcher from the cell eu1 telemetry feed.",
        "",
        "| window | p50 | p99 |",
        "|---|---|---|",
        "| 24 h before c1_5e0d… | 1.2 ms | 4.1 ms |",
        "| 24 h after | 1.3 ms | 31.4 ms |",
        "",
        "The regression starts with the hono bump (deps-bot). Requested a latency histogram on the route to separate route time from host time.",
      ].join("\n"),
    });
    add(19, "Upgrade zod to v4", "task", ["dependencies"], ["deps"], "dev", 700, { linkedChanges: [cr("zod")] });
    add(20, "Cache invoice totals per customer", "feature", ["billing", "perf"], ["lin"], "lin", 450, { linkedChanges: [cr("cache")] });
    add(21, "429 responses are missing Retry-After", "bug", ["api"], [], "sam", 200, { body: "docs/api.md promises `Retry-After` on 429; the limiter does not send it." });
    add(22, "Paginate GET /v1/invoices", "feature", ["api"], [], "lin", 150);

    seeds.forEach((s, i) => {
      const m = meta[i] as (typeof meta)[number];
      const issue: IssueRow = {
        ...s,
        id: ulid(this.ago(m.ago), "issue:" + s.number),
        repoId: api.repo.id,
        assignees: m.assignees.map((h) => this.p(h)),
        author: this.p(m.author),
        subIssues: [],
        comments: m.comments.map((cm, k) => ({ id: ulid(this.ago(cm.ago), "ic:" + s.number + ":" + k), author: this.p(cm.by), body: cm.body, createdAt: this.iso(this.ago(cm.ago)) })),
        createdAt: this.iso(this.ago(m.ago)),
      };
      this.issues.push(issue);
      push(this.ago(m.ago), this.ev(api, "issue.opened", m.author, this.ago(m.ago), { note: { title: "#" + s.number + " " + s.title, target: "issue:" + s.number } }));
    });
    for (const i of this.issues) if (i.parentNumber) this.issues.find((p) => p.number === i.parentNumber)?.subIssues.push(i.number);
    this.projects.set("api", {
      id: ulid(this.ago(10_000), "project:api"),
      name: "API · October",
      repoId: api.repo.id,
      columns: [
        { id: "todo", name: "Todo", issueNumbers: [14, 21, 22] },
        { id: "doing", name: "In progress", issueNumbers: [13, 16, 18] },
        { id: "review", name: "In review", issueNumbers: [12, 15, 17, 19, 20] },
        { id: "done", name: "Done", issueNumbers: [11] },
      ],
    });
    for (const name of ["web", "infra"]) {
      const r = this.repos.get(name) as RepoState;
      this.projects.set(name, { id: ulid(this.ago(9_000), "project:" + name), name: "Backlog", repoId: r.repo.id, columns: [
        { id: "todo", name: "Todo", issueNumbers: [] },
        { id: "doing", name: "In progress", issueNumbers: [] },
        { id: "review", name: "In review", issueNumbers: [] },
        { id: "done", name: "Done", issueNumbers: [] },
      ] });
    }
  }

  private buildReleases(push: (t: number, ev: Omit<T.Event, "seq">) => void): void {
    const api = this.repos.get("api") as RepoState;
    const mk = (tag: string, at: number, author: string, notes: string, prerelease = false): T.Release => ({
      id: ulid(this.ago(at), "release:" + tag),
      repoId: api.repo.id,
      tag,
      name: tag,
      notes,
      target: (api.main[Math.min(api.main.length - 1, Math.max(0, api.main.findIndex((c) => c.time > this.ago(at)) - 1))] as Commit).id,
      author: this.p(author),
      prerelease,
      immutable: !prerelease,
      assets: [
        { name: "acme-api-" + tag + "-linux-arm64.tar.zst", size: String(18_400_000 + tag.length * 1000), downloads: prerelease ? 3 : 120 + tag.length },
        { name: "acme-api-" + tag + ".sbom.spdx.json", size: String(84_000), downloads: prerelease ? 1 : 14 },
      ],
      createdAt: this.iso(this.ago(at)),
    });
    this.releases.push(
      mk("v1.2.0", 20_000, "dev", "## Changes\n\n- Invoice routes (`POST /v1/invoices`, `GET /v1/invoices/:id`)\n- Idle sessions expire after 30 minutes\n"),
      mk("v1.3.0", 5_000, "mara", "## Changes\n\n- Per-line tax computation (billing-fixer)\n- hono 4.6.3 (deps-bot)\n- Webhook delivery guarantees documented (docs-writer)\n"),
      mk("v1.4.0-rc.1", 200, "mara", "## Changes\n\n- Health route reports the build sha and a latency histogram\n- Token audience check\n", true),
    );
    for (const r of this.releases) push(Date.parse(r.createdAt ?? ""), this.ev(api, "release.published", this.handleOf(r.author?.id ?? ""), Date.parse(r.createdAt ?? ""), { note: { title: r.tag, target: "release:" + r.tag } }));
  }

  private buildAlerts(): void {
    const api = this.repos.get("api") as RepoState;
    const a = (key: string, kind: T.SecurityAlert["kind"], severity: T.SecurityAlert["severity"], state: T.SecurityAlert["state"], title: string, detail: string, location: string, advisory: string, ago: number, fix = "", dismissedReason = ""): T.SecurityAlert => ({
      id: ulid(this.ago(ago), "alert:" + key),
      repoId: api.repo.id,
      kind,
      severity,
      state,
      title,
      detail,
      location,
      advisory,
      fixChangeRequestId: fix,
      dismissedReason,
      createdAt: this.iso(this.ago(ago)),
    });
    this.alerts.push(
      a("hono", "dependency", "critical", "open", "hono < 4.6.5: path traversal in serveStatic", "serveStatic resolves '..' segments on Windows hosts. acme/api does not use serveStatic, but the vulnerable code ships.", "package.json · hono 4.6.3", "GHSA-hono-7f2q", 900),
      a("zod", "dependency", "high", "open", "zod < 4.0: ReDoS in email()", "A crafted address makes `z.string().email()` backtrack for seconds.", "package.json · zod 3.23.8", "GHSA-zod-email-redos", 650, this.crByKey("zod").id),
      a("stripe", "secret", "critical", "fixed", "Stripe live key in tests/fixtures/stripe.json", "Blocked at CreateChangeset by push protection. The agent rewrote the fixture with a test key; no bypass was requested.", "tests/fixtures/stripe.json:4", "secret:stripe_live", 2_600),
      a("aws", "secret", "high", "dismissed", "AWS access key in docs/deploy.md", "Key id AKIA…EXAMPLE matches AWS's documented example credential.", "docs/deploy.md:31", "secret:aws_access_key", 9_000, "", "Documented example credential, not a real key"),
      a("redirect", "code_scanning", "medium", "open", "Unvalidated redirect", "`return_to` is passed to `c.redirect` without an allowlist.", "src/routes/invoices.ts:31", "CWE-601", 1_200),
      a("ratelimit", "code_scanning", "low", "open", "Missing rate limit on session refresh", "No limiter on the refresh path; credential stuffing is cheap.", "src/auth/session.ts:15", "CWE-307", 3_000),
    );
  }

  private buildTokens(): void {
    const mk = (key: string, principal: string, parent: string, label: string, scope: Partial<T.TokenScope>, usedCs: number, ago: number, revoked = false) => {
      const info: T.TokenInfo = {
        tokenId: "tok_" + digestHex("token:" + key, 8),
        parentTokenId: parent ? "tok_" + digestHex("token:" + parent, 8) : "",
        principalId: this.p(principal).id,
        label,
        scope: {
          repos: ["acme/api"],
          readPaths: [],
          writePaths: [],
          refs: ["ws/*"],
          actions: ["read", "commit"],
          session: "",
          ...scope,
        },
        used: { maxChangesets: String(usedCs), maxLanded: String(Math.floor(usedCs / 3)), maxUploadBytes: String(usedCs * 41_000) },
        createdAt: this.iso(this.ago(ago)),
        revoked,
      };
      this.tokens.push(info);
    };
    const exp = (days: number) => this.iso(this.anchor + days * 86_400_000);
    mk("triage-root", "triage", "", "triage · main token", { writePaths: [pathBytes("src/auth"), pathBytes("tests")], refs: ["ws/triage/*", "main"], actions: ["read", "commit", "land", "review", "lease"], expiresAt: exp(6), budget: { maxChangesets: "200", maxLanded: "40", maxUploadBytes: "52428800" } }, 61, 6_000);
    mk("triage-sub", "triage", "triage-root", "triage → test-writer sub-agent", { writePaths: [pathBytes("tests")], actions: ["read", "commit"], expiresAt: exp(1), budget: { maxChangesets: "10", maxLanded: "0", maxUploadBytes: "1048576" }, session: "ses_01JACtriage4d0" }, 4, 1_200);
    mk("triage-old", "triage", "triage-root", "triage → one-off migration helper", { writePaths: [pathBytes("migrations")], actions: ["read", "commit"], expiresAt: exp(-2), budget: { maxChangesets: "5", maxLanded: "0", maxUploadBytes: "1048576" } }, 5, 9_000, true);
    mk("billing-root", "billing", "", "billing-fixer · main token", { writePaths: [pathBytes("src/billing"), pathBytes("tests")], refs: ["ws/billing/*"], actions: ["read", "commit", "review", "lease"], expiresAt: exp(12), budget: { maxChangesets: "100", maxLanded: "0", maxUploadBytes: "20971520" } }, 23, 7_000);
    mk("docs-root", "docs", "", "docs-writer · main token", { writePaths: [pathBytes("docs")], refs: ["ws/docs/*", "main"], actions: ["read", "commit", "land", "lease"], expiresAt: exp(20), budget: { maxChangesets: "300", maxLanded: "100", maxUploadBytes: "10485760" } }, 87, 12_000);
    mk("deps-root", "deps", "", "deps-bot · main token", { repos: ["acme/*"], writePaths: [pathBytes("package.json"), pathBytes("bun.lock")], refs: ["ws/deps/*"], actions: ["read", "commit"], expiresAt: exp(30), budget: { maxChangesets: "500", maxLanded: "0", maxUploadBytes: "52428800" } }, 112, 20_000);
    mk("watcher-root", "watcher", "", "latency-watcher · read + issues", { actions: ["read", "lease"], expiresAt: exp(3) }, 0, 2_000);
    mk("reviewer-root", "reviewer", "", "first-pass · review only", { repos: ["acme/*"], actions: ["read", "review"], expiresAt: exp(9) }, 0, 4_000);
  }

  private buildAudit(): void {
    type Seed = [number, string, string, string, string, boolean, string];
    const seeds: Seed[] = [
      [43_200, "importer", "import.complete", "github:acme/api", "api", true, ""],
      [20_000, "mara", "token.mint", "tok_" + digestHex("token:deps-root", 8), "", true, ""],
      [12_000, "lin", "token.mint", "tok_" + digestHex("token:docs-root", 8), "", true, ""],
      [9_000, "mara", "token.revoke", "tok_" + digestHex("token:triage-old", 8), "", true, ""],
      [8_000, "mara", "policy.update", ".bee/policy.toml (land.main.approvals)", "api", true, "land.main: sensitive needs 2 humans"],
      [7_000, "dev", "token.mint", "tok_" + digestHex("token:billing-root", 8), "", true, ""],
      [6_000, "mara", "token.mint", "tok_" + digestHex("token:triage-root", 8), "", true, ""],
      [5_760, "deps", "change.land", "Bump hono from 4.5.11 to 4.6.3", "api", true, "land.main: dependency, owner approved"],
      [3_000, "triage", "lease.acquire", "src/auth (exclusive)", "api", true, "leases.exclusive_required"],
      [2_700, "billing", "change.land", "Fix rounding drift in invoice tax lines (r1)", "api", false, "land.main: check:test failed"],
      [2_600, "billing", "changeset.create", "tests/fixtures/stripe.json", "api", false, "push_protection: secret:stripe_live"],
      [1_440, "triage", "change.land", "Add health route latency histogram", "api", true, "land.main: logic, owner approved"],
      [1_200, "triage", "token.mint", "tok_" + digestHex("token:triage-sub", 8), "", true, "attenuated from triage · main token"],
      [1_100, "dev", "review.submit", "Rotate session ids on role change (approve r2)", "api", true, ""],
      [700, "dev", "member.set", "sam → member", "", true, ""],
      [600, "deps", "change.open", "Bump zod from 3.23.8 to 4.1.0", "api", true, ""],
      [400, "billing", "lease.acquire", "src/billing/tax.ts (advisory)", "api", true, ""],
      [300, "docs", "change.land", "docs: rate limit headers", "api", true, "land.main: agent-may-land docs"],
      [200, "triage", "change.land", "Rotate session ids on role change (r2)", "api", false, "land.main: approval:human:sensitive (1 of 2)"],
      [120, "mara", "change.land", "Tighten token audience check", "api", true, "land.main: sensitive, 2 humans"],
      [60, "mara", "change.open", "Policy: require an exclusive lease for migrations", "api", true, ""],
    ];
    for (const [ago, actor, action, target, repo, allowed, rule] of seeds) this.appendAudit(actor, action, target, repo, allowed, rule, this.ago(ago));
  }

  appendAudit(actor: string, action: string, target: string, repo: string, allowed: boolean, rule: string, at = this.now()): void {
    const pr = this.p(actor);
    const chain = pr.kind === "PRINCIPAL_KIND_AGENT" ? [this.p(this.handleOf(pr.ownerId)).display] : [];
    const prevHash = this.audit[this.audit.length - 1]?.hash ?? "0".repeat(16);
    const seq = String(this.audit.length + 1);
    const body = [seq, at, pr.id, action, target, repo, allowed, rule, prevHash].join("|");
    this.audit.push({
      seq,
      time: this.iso(at),
      actor: pr,
      onBehalfOf: chain,
      action,
      target,
      repo: repo ? (repo.includes("/") ? repo : ORG + "/" + repo) : "",
      allowed,
      rule,
      source: pr.kind === "PRINCIPAL_KIND_HUMAN" ? "web · 10.0.4.12" : pr.kind === "PRINCIPAL_KIND_AGENT" ? pr.runtime + " · runner eu1" : "service",
      hash: digestHex(body, 8),
      prevHash,
    });
  }

  verifyAudit(): boolean {
    let prev = "0".repeat(16);
    for (const e of this.audit) {
      const body = [e.seq, Date.parse(e.time ?? ""), e.actor?.id, e.action, e.target, e.repo.replace(ORG + "/", ""), e.allowed, e.rule, e.prevHash].join("|");
      if (e.prevHash !== prev || digestHex(body, 8) !== e.hash) return false;
      prev = e.hash;
    }
    return true;
  }

  private buildImports(): void {
    const imp: T.Import = {
      id: ulid(this.ago(43_300), "import:acme/api"),
      source: "github:acme/api",
      repoIds: [(this.repos.get("api") as RepoState).repo.id],
      phase: "IMPORT_PHASE_BRIDGE",
      state: "IMPORT_STATE_LIVE",
      progress: IMPORT_CATEGORIES.map(([category, total]) => ({ category, done: String(total), total: String(total) })),
      rateLimitNote: "GitHub App: 15,000 req/h, 3% used",
      startedAt: this.iso(this.ago(43_300)),
      liveSince: this.iso(this.ago(43_100)),
    };
    this.imports.push({ imp, started: this.ago(43_300), totals: Object.fromEntries(IMPORT_CATEGORIES) });
  }

  // ------------------------------------------------------------------ views

  /** `viewer` decides whether the change's post and `inspired_by` are shown; none reads as signed out. */
  crView(c: CrState, viewer?: string): T.ChangeRequest {
    const post = this.posts.summaryFor(this, c.id, viewer);
    const inspiredBy = this.posts.refFor(this, c.inspiredByPostId, viewer);
    const brief = this.briefs.briefFor(c.id);
    return {
      id: c.id,
      repoId: (this.repos.get(c.repo) as RepoState).repo.id,
      targetRef: "main",
      changeIds: [c.changeId],
      state: c.state,
      author: this.p(c.author),
      title: c.title,
      body: c.body,
      revisions: c.revisions.map((r) => ({ n: r.n, head: r.head.id, base: r.base.id, diffClass: r.diffClass, createdAt: this.iso(r.createdAt) })),
      dependsOn: c.dependsOn,
      createdAt: this.iso(c.createdAt),
      number: String(c.number),
      ...(post ? { post } : {}),
      ...(inspiredBy ? { inspiredBy } : {}),
      ...(brief ? { brief } : {}),
    };
  }

  ev(repo: RepoState, kind: string, actor: string, time: number, body: Partial<T.Event>): Omit<T.Event, "seq"> {
    return { repoId: repo.repo.id, kind, time: this.iso(time), principal: this.p(actor).id, ...body };
  }

  emit(repo: RepoState, kind: string, actor: string, body: Partial<T.Event>): T.Event {
    const e: T.Event = { ...this.ev(repo, kind, actor, this.now(), body), seq: String(++this.seq) };
    this.events.push(e);
    for (const l of this.listeners) l(e);
    return e;
  }

  onEvent(l: Listener): () => void {
    this.listeners.add(l);
    return () => this.listeners.delete(l);
  }

  // ------------------------------------------------------------------ landing

  /** Approvals that still count: those on or after the last revision that changed content. */
  approvalsFor(c: CrState): T.Review[] {
    let floor = 1;
    for (const r of c.revisions) if (r.contentChanged) floor = r.n;
    const latest = new Map<string, T.Review>();
    for (const r of c.reviews) latest.set(r.reviewer?.id ?? "", r);
    return [...latest.values()].filter((r) => r.verdict === "VERDICT_APPROVE" && r.revision >= floor);
  }

  evaluate(c: CrState, rules?: LandRules): T.LandingEvaluation {
    const repo = this.repos.get(c.repo) as RepoState;
    const rev = this.current(c);
    const r = rules ?? this.rules(repo);
    const paths = Object.keys(rev.perPath);
    const res = evaluateLand(r, {
      diffClass: rev.diffClass,
      authorKind: this.kindOf(c.author),
      authorHandle: c.author,
      paths,
      checks: (this.checks.get(rev.head.idText) ?? []).map((ch) => ({ name: ch.name, status: ch.status })),
      approvals: this.approvalsFor(c)
        .filter((a) => this.handleOf(a.reviewer?.id ?? "") !== "reviewer") // first-pass approvals never count
        .map((a) => ({ handle: this.handleOf(a.reviewer?.id ?? ""), kind: this.kindOf(this.handleOf(a.reviewer?.id ?? "")) })),
      exclusiveLeases: this.leases
        .filter((l) => l.repoId === repo.repo.id && l.mode === "LEASE_MODE_EXCLUSIVE")
        .map((l) => ({ holderHandle: this.handleOf(l.holder?.id ?? ""), paths: l.paths.map(pathText) })),
    });
    if (c.dependsOn) {
      const parent = this.crs.find((x) => x.id === c.dependsOn);
      if (parent && parent.state !== "CHANGE_STATE_LANDED") res.missing.push("stack: parent " + parent.title + " must land first");
    }
    if (c.state !== "CHANGE_STATE_OPEN") res.missing.push("state: " + c.state.replace("CHANGE_STATE_", "").toLowerCase());
    return { allowed: res.missing.length === 0, satisfied: res.satisfied, missing: res.missing, diffClass: rev.diffClass };
  }

  land(c: CrState, actor: string): T.LandResponse {
    const repo = this.repos.get(c.repo) as RepoState;
    const evaluation = this.evaluate(c);
    if (!evaluation.allowed) {
      this.appendAudit(actor, "change.land", c.title, repo.name, false, "land.main: " + evaluation.missing.join("; "));
      throw new BeeError("failed_precondition", {
        reason: "POLICY_DENIED",
        message: "policy does not allow landing this change yet",
        policyDenial: { rule: "land.main", missing: evaluation.missing },
      });
    }
    const rev = this.current(c);
    const ref = repo.refs.get("main") as RefState;
    const files: Record<string, string | null> = {};
    for (const path of Object.keys(rev.perPath)) files[path] = rev.head.files.get(path) ?? null;
    const commit = this.makeCommit(repo.name, ref.target, files, c.author, this.now(), c.title, repo.name + "@" + repo.main.length + ":" + c.key);
    commit.changeId = c.changeId;
    repo.main.push(commit);
    const version = ref.version + 1;
    const entry: T.RefLogEntry = {
      version: String(version),
      old: ref.target.id,
      new: commit.id,
      principal: this.p(actor).id,
      time: this.iso(this.now()),
      landed: [{ changeId: c.changeId, source: rev.head.id, result: commit.id, skipped: false }],
    };
    repo.refs.set("main", { target: commit, version, log: [...ref.log, entry] });
    c.state = "CHANGE_STATE_LANDED";
    this.posts.onLanded(this, c.id);
    this.queue.set(repo.name, (this.queue.get(repo.name) ?? []).filter((q) => q.crId !== c.id));
    const released = this.leases.filter((l) => l.changeId && l.changeId.raw === c.changeId.raw);
    this.leases = this.leases.filter((l) => !released.includes(l));
    this.emit(repo, "ref.updated", actor, { refUpdated: { name: "main", target: commit.id, version: String(version) } });
    this.emit(repo, "cr.landed", actor, { change: this.crView(c) });
    for (const l of released) this.emit(repo, "lease.released", this.handleOf(l.holder?.id ?? ""), { lease: l });
    this.appendAudit(actor, "change.land", c.title, repo.name, true, "land.main: " + evaluation.satisfied.join("; "));
    return { ref: { name: "main", target: commit.id, version: String(version) }, landed: entry.landed, attempts: 1 };
  }

  enqueue(c: CrState, actor: string): T.QueueEntry {
    const repo = this.repos.get(c.repo) as RepoState;
    const evaluation = this.evaluate(c);
    if (!evaluation.allowed) {
      throw new BeeError("failed_precondition", {
        reason: "POLICY_DENIED",
        message: "policy does not allow landing this change yet",
        policyDenial: { rule: "land.main", missing: evaluation.missing },
      });
    }
    const list = this.queue.get(repo.name) ?? [];
    let q = list.find((x) => x.crId === c.id);
    if (!q) {
      q = { crId: c.id, state: "waiting", enqueuedAt: this.now(), train: "train-" + digestHex("train:" + c.key + this.stepN, 3) };
      list.push(q);
      this.queue.set(repo.name, list);
      this.emit(repo, "cr.enqueued", actor, { change: this.crView(c) });
      this.appendAudit(actor, "queue.enqueue", c.title, repo.name, true, "land.main");
    }
    return this.queueView(repo, q, list.indexOf(q));
  }

  queueView(repo: RepoState, q: QueueState, index: number): T.QueueEntry {
    const c = this.crs.find((x) => x.id === q.crId);
    return { changeRequestId: q.crId, title: c?.title ?? "", position: index + 1, state: q.state, train: q.train, enqueuedAt: this.iso(q.enqueuedAt) };
  }

  // ------------------------------------------------------------------ revisions

  /** Adds a revision whose head is `files` applied on the current head; checks start running. */
  revise(c: CrState, files: Record<string, string>, actor: string): RevState {
    const repo = this.repos.get(c.repo) as RepoState;
    const cur = this.current(c);
    const n = cur.n + 1;
    const head = this.makeCommit(repo.name, cur.head, files, c.author, this.now(), c.title, "cr:" + c.key + "@" + n + ":" + this.counter++);
    head.changeId = c.changeId;
    // The new head is relative to the same base; rebuild the snapshot on the base for classification.
    const rev = this.makeRev(n, head, cur.base, this.now(), true);
    c.revisions.push(rev);
    const checks = ["test", "lint", "e2e"].map((name) => this.makeCheck(repo, head, name, "CHECK_STATUS_RUNNING", this.now()));
    this.checks.set(head.idText, checks);
    this.pendingChecks.push(head.idText);
    this.buildRun(repo, c, rev, checks);
    repo.refs.set(c.ws, { target: head, version: n, log: [] });
    this.emit(repo, "cr.revised", actor, { change: this.crView(c) });
    return rev;
  }

  // ------------------------------------------------------------------ live simulation

  /** Advances the simulated world by one beat. Deterministic in its sequence of effects. */
  step(): void {
    this.stepN++;
    this.refreshLive();
    this.promoteChecks();
    this.advanceQueue();
    this.docsLoop();
  }

  private promoteChecks(): void {
    const heads = this.pendingChecks.splice(0);
    for (const h of heads) {
      const checks = this.checks.get(h) ?? [];
      const commit = this.commits.get(h);
      const repo = commit ? this.repos.get(commit.repo) : undefined;
      if (!repo) continue;
      for (const ch of checks) {
        if (ch.status !== "CHECK_STATUS_RUNNING") continue;
        ch.status = "CHECK_STATUS_PASSED";
        ch.finishedAt = this.iso(this.now());
        ch.summary = summaryFor(ch.name, ch.status);
        this.emit(repo, "check.updated", "ci", { check: ch });
      }
      const run = this.runs.find((r) => r.head?.raw === commit?.id.raw);
      if (run) {
        for (const j of run.jobs) {
          j.status = "passed";
          j.steps = stepsFor(j.name, "passed");
          j.finishedAt = this.iso(this.now());
        }
        this.live = this.live.filter((l) => l.runId !== run.id);
        run.status = "passed";
        run.finishedAt = this.iso(this.now());
      }
    }
  }

  /** Reveals log lines of running jobs and finishes them when their log is complete. */
  refreshLive(): void {
    const now = this.now();
    for (const lj of [...this.live]) {
      const run = this.runs.find((r) => r.id === lj.runId);
      const job = run?.jobs.find((j) => j.id === lj.jobId);
      if (!run || !job) continue;
      const total = (this.logs.get(run.id) ?? []).filter((l) => l.jobId === job.id).length;
      if (this.visibleLines(lj, now) < total) continue;
      this.live = this.live.filter((x) => x !== lj);
      job.status = lj.outcome;
      job.steps = stepsFor(job.name, lj.outcome);
      job.finishedAt = this.iso(now);
      run.status = aggregate(run.jobs.map((j) => j.status));
      if (run.status !== "running") run.finishedAt = this.iso(now);
      const repo = [...this.repos.values()].find((r) => r.repo.id === run.repoId) as RepoState;
      const check = [...this.checks.values()].flat().find((ch) => ch.url.includes(job.id));
      if (check) {
        check.status = lj.outcome === "passed" ? "CHECK_STATUS_PASSED" : "CHECK_STATUS_FAILED";
        check.finishedAt = this.iso(now);
        check.summary = summaryFor(check.name, check.status);
        this.emit(repo, "check.updated", "ci", { check });
      }
      this.emit(repo, "run.completed", "ci", { note: { title: "ci #" + run.number + " " + run.status, target: "run:" + run.id } });
    }
  }

  /** Reruns a run's jobs (or only its failed ones). Outcomes are a function of the inputs, so a
   * failing job fails again: rerunning does not paper over a real failure. */
  rerun(run: T.Run, failedOnly: boolean, actor: string): T.Run {
    const now = this.now();
    const repo = [...this.repos.values()].find((r) => r.repo.id === run.repoId) as RepoState;
    const targets = run.jobs.filter((j) => j.status !== "running" && (!failedOnly || j.status === "failed"));
    if (targets.length === 0) throw new BeeError("failed_precondition", { reason: "NOTHING_TO_RERUN", message: failedOnly ? "no failed jobs to rerun" : "every job is still running" });
    run.attempt++;
    run.startedAt = this.iso(now);
    run.finishedAt = undefined;
    for (const j of targets) {
      const outcome: T.RunStatus = j.status === "failed" ? "failed" : "passed";
      j.status = "running";
      j.steps = stepsFor(j.name, "running");
      j.startedAt = this.iso(now);
      j.finishedAt = undefined;
      this.live = this.live.filter((l) => !(l.runId === run.id && l.jobId === j.id));
      this.live.push({ runId: run.id, jobId: j.id, startedAt: now, initial: 3, outcome });
      const check = [...this.checks.values()].flat().find((ch) => ch.url.includes(j.id));
      if (check) {
        check.status = "CHECK_STATUS_RUNNING";
        check.finishedAt = undefined;
        check.summary = summaryFor(check.name, check.status);
        this.emit(repo, "check.updated", "ci", { check });
      }
    }
    run.status = aggregate(run.jobs.map((j) => j.status));
    this.emit(repo, "run.started", actor, { note: { title: "ci #" + run.number + " attempt " + run.attempt, target: "run:" + run.id } });
    this.appendAudit(actor, "run.rerun", "ci #" + run.number + (failedOnly ? " (failed jobs)" : ""), repo.name, true, "");
    return run;
  }

  /** Log lines of a job as far as they have been printed. */
  jobLines(run: T.Run, jobId: string): LogRow[] {
    const all = (this.logs.get(run.id) ?? []).filter((l) => l.jobId === jobId);
    const lj = this.live.find((l) => l.runId === run.id && l.jobId === jobId);
    return lj ? all.slice(0, this.visibleLines(lj)) : all;
  }

  visibleLines(lj: LiveJob, now = this.now()): number {
    return lj.initial + Math.max(0, Math.floor((now - lj.startedAt) / this.logMs));
  }

  private advanceQueue(): void {
    for (const [name, list] of this.queue) {
      const front = list[0];
      if (!front) continue;
      const repo = this.repos.get(name) as RepoState;
      const c = this.crs.find((x) => x.id === front.crId);
      if (!c) {
        list.shift();
        continue;
      }
      if (front.state === "waiting") front.state = "testing";
      else if (front.state === "testing") front.state = "landing";
      else {
        try {
          this.land(c, c.author);
        } catch {
          list.shift();
          this.emit(repo, "cr.dequeued", "ci", { change: this.crView(c) });
        }
      }
    }
  }

  /** docs-writer opens, gets reviewed, and lands one docs change every six beats. */
  private docsLoop(): void {
    const repo = this.repos.get("api") as RepoState;
    const phase = this.stepN % 6;
    const cycle = Math.floor(this.stepN / 6);
    const topic = DOC_TOPICS[cycle % DOC_TOPICS.length] as (typeof DOC_TOPICS)[number];
    const key = "live-docs-" + cycle;
    const existing = this.crs.find((x) => x.key === key);
    switch (phase) {
      case 1: {
        const lease: T.Lease = {
          id: this.newId("lease"),
          repoId: repo.repo.id,
          ref: "main",
          paths: [pathBytes(topic.file)],
          holder: this.p("docs"),
          changeId: changeId("cr:" + key),
          mode: "LEASE_MODE_ADVISORY",
          expiresAt: this.iso(this.now() + 30 * 60_000),
          note: "Writing " + topic.title + " docs",
        };
        this.leases.push(lease);
        this.emit(repo, "lease.acquired", "docs", { lease });
        break;
      }
      case 2: {
        if (existing) break;
        const base = this.tip(repo);
        const prior = base.files.get(topic.file) ?? "# " + cap(topic.title) + "\n";
        const head = this.makeCommit(repo.name, base, { [topic.file]: prior + "\n" + topic.body + " (rev " + (cycle + 1) + ")\n" }, "docs", this.now(), "docs: " + topic.title, "cr:" + key + "@1");
        const c: CrState = {
          id: this.newId("cr"),
          key,
          number: this.nextNumber(repo.name),
          repo: repo.name,
          author: "docs",
          title: "docs: explain " + topic.title,
          body: "Generated from the code paths that implement " + topic.title + ".",
          createdAt: this.now(),
          state: "CHANGE_STATE_OPEN",
          dependsOn: "",
          changeId: changeId("cr:" + key),
          revisions: [],
          reviews: [],
          threads: [],
          ws: "ws/docs/" + topic.title.replace(/\s+/g, "-"),
        };
        head.changeId = c.changeId;
        c.revisions.push(this.makeRev(1, head, base, this.now(), true));
        this.crs.push(c);
        const checks = ["test", "lint", "e2e"].map((name) => this.makeCheck(repo, head, name, "CHECK_STATUS_RUNNING", this.now()));
        this.checks.set(head.idText, checks);
        this.buildRun(repo, c, this.current(c), checks);
        this.live = this.live.filter((l) => !this.runs.find((r) => r.id === l.runId && r.head?.raw === head.id.raw));
        repo.refs.set(c.ws, { target: head, version: 1, log: [] });
        this.emit(repo, "cr.opened", "docs", { change: this.crView(c) });
        this.pendingChecks.push(head.idText);
        break;
      }
      case 4: {
        if (!existing || existing.state !== "CHANGE_STATE_OPEN") break;
        const review: T.Review = {
          id: this.newId("review"),
          changeId: existing.id,
          revision: 1,
          reviewer: this.p("reviewer"),
          verdict: "VERDICT_APPROVE",
          body: "Matches the implementation.",
          comments: [],
          createdAt: this.iso(this.now()),
        };
        existing.reviews.push(review);
        this.emit(repo, "review.submitted", "reviewer", { review });
        break;
      }
      case 5: {
        if (!existing || existing.state !== "CHANGE_STATE_OPEN") break;
        try {
          this.land(existing, "docs");
        } catch {
          /* checks still running: try again next cycle */
        }
        const lease = this.leases.find((l) => l.holder?.id === this.p("docs").id && l.paths.some((x) => pathText(x) === topic.file));
        if (lease) {
          this.leases = this.leases.filter((l) => l !== lease);
          this.emit(repo, "lease.released", "docs", { lease });
        }
        break;
      }
      default:
        break;
    }
  }

  // ------------------------------------------------------------------ revisions and files

  /** Resolves a revision expression to a commit (SPEC §12.3 Resolve, subset). */
  resolve(repo: RepoState, expr: string): Commit {
    const e = expr.trim() || repo.repo.defaultRef;
    const tilde = /^(.*)~(\d+)$/.exec(e);
    if (tilde) {
      let c: Commit | null = this.resolve(repo, tilde[1] ?? "");
      for (let i = 0; i < Number(tilde[2]); i++) c = c?.parent ?? null;
      if (!c) throw new BeeError("not_found", { reason: "UNKNOWN_REVISION", message: "revision " + e + " goes past the root" });
      return c;
    }
    const at = /^([^@]+)@(\d+)$/.exec(e);
    if (at) {
      const ref = repo.refs.get(at[1] ?? "");
      const entry = ref?.log.find((l) => l.version === at[2]);
      const c = entry?.new ? this.commits.get(objectIdText(entry.new)) : undefined;
      if (c) return c;
    }
    const ref = repo.refs.get(e);
    if (ref) return ref.target;
    const hexPart = e.replace(/^c1_/, "");
    if (/^[0-9a-f]{6,64}$/.test(hexPart)) {
      const matches = [...this.commits.values()].filter((c) => c.repo === repo.name && c.idText.slice(3).startsWith(hexPart));
      if (matches.length === 1) return matches[0] as Commit;
      if (matches.length > 1) throw new BeeError("invalid_argument", { reason: "AMBIGUOUS_REVISION", message: "prefix " + hexPart + " matches " + matches.length + " changesets" });
    }
    throw new BeeError("not_found", { reason: "UNKNOWN_REVISION", message: "no revision " + e + " in " + repo.repo.fullName });
  }

  fileVersions(repo: RepoState, at: Commit, path: string): Commit[] {
    const chain: Commit[] = [];
    for (let c: Commit | null = at; c; c = c.parent) chain.unshift(c);
    return chain.filter((c, i) => {
      const prev = i === 0 ? undefined : chain[i - 1]?.files.get(path);
      return c.files.has(path) && c.files.get(path) !== prev;
    });
  }

  context(repo: RepoState, paths: string[]): T.RepoContext {
    const rules = this.rules(repo);
    const overlap = (a: string, b: string) => a === b || a.startsWith(b.replace(/\/$/, "") + "/") || b.startsWith(a.replace(/\/$/, "") + "/") || a === "" || b === "";
    return {
      owners: paths.map((p) => ({ path: pathBytes(p), owners: ownersFor(rules, p).map((h) => this.principals.get(h)?.display ?? h) })),
      activeLeases: this.leases.filter((l) => l.repoId === repo.repo.id && l.paths.some((lp) => paths.some((p) => overlap(pathText(lp), p)))),
      openChangesTouching: this.crs
        .filter((c) => c.repo === repo.name && c.state === "CHANGE_STATE_OPEN" && Object.keys(this.current(c).perPath).some((cp) => paths.some((p) => overlap(cp, p))))
        .map((c) => this.crView(c)),
      failingChecks: [...this.checks.values()].flat().filter((ch) => ch.repoId === repo.repo.id && ch.status === "CHECK_STATUS_FAILED"),
      landRules: describeRules(rules),
    };
  }

  // ------------------------------------------------------------------ auth

  caller(token: string | undefined): string {
    if (!token) throw new BeeError("unauthenticated", { reason: "UNAUTHENTICATED", message: "sign in to do this" });
    const id = this.tokenSecrets.get(token);
    if (id) {
      const info = this.tokens.find((t) => t.tokenId === id);
      if (!info || info.revoked) throw new BeeError("unauthenticated", { reason: "TOKEN_REVOKED", message: "this token has been revoked" });
      return this.handleOf(info.principalId);
    }
    if (token.length < 8 || token.startsWith(RISKY_TOKEN)) throw new BeeError("unauthenticated", { reason: "INVALID_TOKEN", message: "the token is not valid for this server" });
    return "mara";
  }

  revoke(tokenId: string, actor: string): void {
    const target = this.tokens.find((t) => t.tokenId === tokenId);
    if (!target) throw new BeeError("not_found", { reason: "TOKEN_NOT_FOUND", message: "no token " + tokenId });
    const doomed = new Set([tokenId]);
    let grew = true;
    while (grew) {
      grew = false;
      for (const t of this.tokens)
        if (!doomed.has(t.tokenId) && doomed.has(t.parentTokenId)) {
          doomed.add(t.tokenId);
          grew = true;
        }
    }
    const api = this.repos.get("api") as RepoState;
    for (const t of this.tokens)
      if (doomed.has(t.tokenId) && !t.revoked) {
        t.revoked = true;
        this.emit(api, "token.revoked", actor, { revokedToken: t.tokenId });
        this.appendAudit(actor, "token.revoke", t.tokenId + (t.tokenId === tokenId ? "" : " (derived)"), "", true, "");
      }
  }

  // ------------------------------------------------------------------ imports

  importView(s: ImportState): T.Import {
    if (s.imp.state === "IMPORT_STATE_CANCELLED" || s.imp.liveSince) return s.imp;
    const dry = s.imp.phase === "IMPORT_PHASE_DRY_RUN";
    const span = dry ? this.importMs / 4 : this.importMs;
    const f = Math.min(1, Math.max(0, (this.now() - s.started) / span));
    const gitCats = new Set(["commits", "refs", "lfs_objects", "wiki"]);
    const gitF = Math.min(1, f / 0.45);
    const metaF = Math.max(0, Math.min(1, (f - 0.45) / 0.55));
    s.imp.progress = Object.entries(s.totals).map(([category, total]) => ({
      category,
      done: String(Math.floor(total * (gitCats.has(category) ? gitF : metaF))),
      total: String(total),
    }));
    s.imp.state = f < 0.05 ? "IMPORT_STATE_QUEUED" : f < 0.45 ? "IMPORT_STATE_GIT" : f < 1 ? "IMPORT_STATE_METADATA" : "IMPORT_STATE_LIVE";
    s.imp.eta = f < 1 ? this.iso(s.started + span) : undefined;
    if (f >= 1) s.imp.liveSince = this.iso(this.now());
    return s.imp;
  }
}

// ---------------------------------------------------------------- helpers

export const IMPORT_CATEGORIES: [string, number][] = [
  ["commits", 18_432],
  ["refs", 214],
  ["lfs_objects", 37],
  ["wiki", 1],
  ["issues", 1_204],
  ["pull_requests", 3_311],
  ["review_comments", 15_880],
  ["releases", 48],
];

function cap(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

function runStatusOf(s: T.CheckStatus): T.RunStatus {
  switch (s) {
    case "CHECK_STATUS_PASSED":
      return "passed";
    case "CHECK_STATUS_FAILED":
      return "failed";
    case "CHECK_STATUS_RUNNING":
      return "running";
    case "CHECK_STATUS_SKIPPED":
      return "cancelled";
    default:
      return "queued";
  }
}

export function aggregate(statuses: T.RunStatus[]): T.RunStatus {
  if (statuses.includes("running")) return "running";
  if (statuses.includes("failed")) return "failed";
  if (statuses.includes("queued")) return statuses.every((s) => s === "queued") ? "queued" : "running";
  if (statuses.length && statuses.every((s) => s === "cancelled")) return "cancelled";
  return "passed";
}

function durationFor(name: string): number {
  return name === "e2e" ? 312_000 : name === "test" ? 94_000 : 21_000;
}

function summaryFor(name: string, status: T.CheckStatus): string {
  const s = status.replace("CHECK_STATUS_", "").toLowerCase();
  if (status === "CHECK_STATUS_FAILED") return name === "test" ? "1 of 31 tests failed" : name + " failed";
  if (status === "CHECK_STATUS_PASSED") return name === "test" ? "31 tests passed" : name === "e2e" ? "12 scenarios passed" : "no findings";
  return name + " " + s;
}

function stepsFor(name: string, status: T.RunStatus): T.RunStep[] {
  const names = ["Set up runner", "actions/checkout@v4", "bun install --frozen-lockfile", name === "lint" ? "bun run lint" : name === "e2e" ? "bun run e2e" : "bun test", "Upload results"];
  return names.map((n, i) => {
    let st: T.RunStatus = status;
    if (status === "failed") st = i < 3 ? "passed" : i === 3 ? "failed" : "cancelled";
    if (status === "running") st = i < 3 ? "passed" : i === 3 ? "running" : "queued";
    return { name: n, status: st, durationMs: String(st === "passed" || st === "failed" ? [2100, 900, 6400, durationFor(name) - 9800, 400][i] ?? 0 : 0) };
  });
}

function annotationsFor(key: string, checks: T.CheckRun[]): T.Annotation[] {
  const failed = checks.some((c) => c.name === "test" && c.status === "CHECK_STATUS_FAILED");
  if (!failed) return key.startsWith("cr:cache") ? [{ path: "src/billing/invoice.ts", line: 17, level: "warning", title: "unbounded-cache", message: "Map grows without eviction." }] : [];
  if (key.startsWith("cr:zod"))
    return [
      { path: "src/routes/invoices.ts", line: 22, level: "failure", title: "TS2339", message: "Property 'issues' does not exist on type 'ZodError'. Did you mean 'errors'? (zod 4 renamed it)" },
      { path: "src/routes/invoices.ts", line: 7, level: "warning", title: "deprecated", message: "z.object().strict() default changed in zod 4." },
    ];
  return [{ path: "tests/billing.test.ts", line: 17, level: "failure", title: "line taxes sum to the tax on the total", message: "expect(received).toBe(expected)\n\nExpected: 190\nReceived: 189" }];
}

function logFor(job: T.RunJob, key: string, annotations: T.Annotation[]): LogRow[] {
  const out: LogRow[] = [];
  const add = (text: string, level: LogRow["level"] = "info") => out.push({ n: out.length + 1, jobId: job.id, text, level });
  add("Set up runner", "group");
  add("Runner linux-arm64 (cell eu1), image ubuntu-24.04, 4 vCPU");
  add("Job token scoped to acme/api: read, checks:write; expires in 60 min");
  add("actions/checkout@v4", "group");
  add("Fetching " + key.replace(/^cr:/, "") + " through the node cache: 41 objects, 38 cache hits");
  add("bun install --frozen-lockfile", "group");
  add("bun install v1.4.2");
  add("+ hono@4.6.3");
  add(key.startsWith("cr:zod") ? "+ zod@4.1.0" : "+ zod@3.23.8");
  add("3 packages installed [612ms]");
  if (job.name === "lint") {
    add("bun run lint", "group");
    add("$ biome check .");
    add("Checked 14 files in 48ms. No fixes applied.");
  } else if (job.name === "e2e") {
    add("bun run e2e", "group");
    const scenarios = ["create invoice", "read invoice", "expired session", "role downgrade", "token audience", "webhook signature", "rate limit headers", "health build sha", "health histogram", "tax rounding DE", "tax rounding US", "pdf breakdown"];
    for (const s of scenarios) {
      add("  running scenario: " + s);
      add("  ✓ " + s + " (" + (200 + s.length * 37) + " ms)");
    }
    add(job.status === "failed" ? "11 passed, 1 failed" : "12 passed");
  } else {
    add("bun test", "group");
    add("bun test v1.4.2");
    const tests = ["auth.test.ts > rejects garbage", "auth.test.ts > role change issues a new session id", "billing.test.ts > US tax on one line", "billing.test.ts > line taxes sum to the tax on the total", "routes.test.ts > POST /v1/invoices validates lines", "routes.test.ts > GET /health reports build"];
    for (const t of tests) {
      const fail = job.status === "failed" && annotations.some((a) => a.level === "failure" && (t.includes(a.title) || (a.path.includes("invoices") && t.includes("invoices"))));
      add((fail ? "✗ " : "✓ ") + t, fail ? "error" : "info");
      if (fail) for (const a of annotations.filter((x) => x.level === "failure")) for (const l of (a.path + ":" + a.line + "  " + a.message).split("\n")) add("    " + l, "error");
    }
    for (const a of annotations.filter((x) => x.level === "warning")) add("warning: " + a.path + ":" + a.line + " " + a.message, "warning");
    add(job.status === "failed" ? "30 pass, 1 fail" : "31 pass, 0 fail", job.status === "failed" ? "error" : "info");
  }
  if (job.status === "failed") add("Process completed with exit code 1.", "error");
  return out;
}

export { highestClass, diffText };
