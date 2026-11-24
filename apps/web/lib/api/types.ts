/**
 * View models: the shapes screens read and write through `lib/api/client.ts`.
 *
 * They are proto3 JSON (lowerCamelCase fields, enums as their full names, `bytes` as base64,
 * 64-bit integers as decimal strings, `Timestamp` as RFC 3339 strings), so they can be logged,
 * cached and passed from server to client components as plain data.
 *
 * Two groups:
 *  1. Wire-shaped types: a subset of the generated bee.v1 message JSON (`lib/gen/`). Requests
 *     and responses pass through `lib/api/ops/` unchanged. `ops/conformance.ts` checks at
 *     compile time that every field here exists in the proto with a compatible type.
 *  2. Views (after the VIEWS marker): composed from several bee.v1 calls or reshaped for a
 *     screen. Each names the messages it maps from.
 *
 * Responses are written with unpopulated fields emitted, so scalars and repeated fields are
 * typed as always present. Message-typed and oneof fields are optional.
 */
import type { SubjectKindJson } from "../gen/bee/v1/common_pb";
import type { AppInstallationJson, AppJson } from "../gen/bee/v1/apps_pb";
import type { PackageJson, PackageVersionJson } from "../gen/bee/v1/packages_pb";
import type { PagesDeploymentJson, PagesSiteJson } from "../gen/bee/v1/pages_pb";
import type { BillingCycleJson, BillingProductJson, BillingUsageGroupingJson } from "../gen/bee/v1/billing_pb";
import type {
  EnterpriseActionsEnablementJson,
  EnterpriseAllowedActionsJson,
  EnterpriseBasePermissionJson,
  EnterpriseForkingPolicyJson,
  EnterprisePolicyToggleJson,
  EnterpriseRepoCreationPolicyJson,
  VerifiedDomainStateJson,
} from "../gen/bee/v1/enterprise_pb";
import type { GetLanguageStatsRequestJson, GetLanguageStatsResponseJson } from "../gen/bee/v1/insights_pb";
import type { HealthStatusJson } from "../gen/bee/v1/ops_pb";
import type { CodeSearchHitJson, LeaderboardMetricJson, LeaderboardSubjectJson, SearchSortOrderJson, TrendingWindowJson } from "../gen/bee/v1/search_pb";
import type { TeamJson } from "../gen/bee/v1/teams_pb";

export type U64 = string;
export type Bytes = string; // base64
export type Timestamp = string; // RFC 3339

// ---------------------------------------------------------------- common.proto

export interface ObjectId {
  raw: Bytes;
}
export interface ChangeId {
  raw: Bytes;
}
export type ObjectKind =
  | "OBJECT_KIND_UNSPECIFIED"
  | "OBJECT_KIND_BLOB"
  | "OBJECT_KIND_TREE"
  | "OBJECT_KIND_CHANGESET"
  | "OBJECT_KIND_META";
export type Mode = "MODE_UNSPECIFIED" | "MODE_FILE" | "MODE_EXECUTABLE" | "MODE_SYMLINK" | "MODE_TREE";

export type RepoRef = { id: string; fullName?: never } | { fullName: string; id?: never };
export interface Revision {
  expr: string;
}
export interface Signature {
  name: string;
  time?: Timestamp;
  tzOffsetMinutes: number;
}
export type PrincipalKind =
  | "PRINCIPAL_KIND_UNSPECIFIED"
  | "PRINCIPAL_KIND_HUMAN"
  | "PRINCIPAL_KIND_AGENT"
  | "PRINCIPAL_KIND_SERVICE";
export interface Principal {
  id: string;
  kind: PrincipalKind;
  display: string;
  ownerId: string;
  runtime: string;
  externalIds: string[];
}
export interface LocalCheck {
  name: string;
  result: string;
  durationMs: U64;
}
export interface Cost {
  inputTokens: U64;
  outputTokens: U64;
  usdMicros: U64;
}
export interface Provenance {
  principalChain: string[];
  runtime: string;
  model: string;
  session: string;
  promptDigest: Bytes;
  checksRun: LocalCheck[];
  cost?: Cost;
}
export interface Meta {
  author: string;
  authorTime?: Signature;
  committer: string;
  committerTime?: Signature;
  creator: string;
  message: string;
  provenance?: Provenance;
  /** Extension entries by key; key 64 holds the original git commit headers of imported history (SPEC §17.4). */
  extensions?: MetaExtension[];
}
export interface MetaExtension {
  key: number;
  /** Base64 of the entry's bytes. */
  value: string;
}
export interface Put {
  mode: Mode;
  blob?: ObjectId;
  size: U64;
}
export interface DeltaEntry {
  path: Bytes;
  put?: Put;
  delete?: boolean;
}
export interface Changeset {
  id?: ObjectId;
  parents: ObjectId[];
  root?: ObjectId;
  metaId?: ObjectId;
  changeId?: ChangeId;
  delta: DeltaEntry[];
  meta?: Meta;
  filtered: boolean;
  generation: U64;
}
export interface Ref {
  name: string;
  target?: ObjectId;
  version: U64;
}
export interface ConflictSide {
  mode: Mode;
  blob?: ObjectId;
}
export interface LeaseConflict {
  leaseIds: string[];
  paths: Bytes[];
}
export interface PolicyDenial {
  rule: string;
  missing: string[];
}
export interface ErrorDetail {
  reason: string;
  message: string;
  retryable: boolean;
  leaseConflict?: LeaseConflict;
  policyDenial?: PolicyDenial;
}
export interface PageRequest {
  pageSize?: number;
  pageToken?: string;
}
export interface PageResponse {
  nextPageToken: string;
}

// ---------------------------------------------------------------- forge.proto

export interface Org {
  id: string;
  name: string;
  display: string;
  createdAt?: Timestamp;
}
export type Role = "ROLE_UNSPECIFIED" | "ROLE_OWNER" | "ROLE_ADMIN" | "ROLE_MEMBER";
export interface Member {
  principal?: Principal;
  role: Role;
}
export interface GetOrgRequest {
  nameOrId: string;
}
export interface ListOrgsRequest {
  principalId?: string;
  page?: PageRequest;
}
export interface ListOrgsResponse {
  orgs: Org[];
  page?: PageResponse;
}
export interface ListMembersRequest {
  org: string;
  page?: PageRequest;
}
export interface ListMembersResponse {
  members: Member[];
  page?: PageResponse;
}
export type Visibility =
  | "VISIBILITY_UNSPECIFIED"
  | "VISIBILITY_PRIVATE"
  | "VISIBILITY_INTERNAL"
  | "VISIBILITY_PUBLIC";
export interface Repo {
  id: string;
  orgId: string;
  name: string;
  fullName: string;
  visibility: Visibility;
  defaultRef: string;
  description: string;
  createdAt?: Timestamp;
  archived: boolean;
  topics: string[];
  homepage: string;
  pushedAt?: Timestamp;
  stars: U64;
  forks: U64;
  openIssues: U64;
  openChanges: U64;
  /** SPDX id of the repo's license; empty when none was detected. */
  licenseSpdx: string;
  primaryLanguage: string;
}
export interface GetRepoRequest {
  repo: RepoRef;
}
export interface ListReposRequest {
  org: string;
  page?: PageRequest;
}
export interface ListReposResponse {
  repos: Repo[];
  page?: PageResponse;
}
export interface GetContextRequest {
  repo: RepoRef;
  at?: Revision;
  paths: Bytes[];
}
export interface PathOwners {
  path: Bytes;
  owners: string[];
}
export interface RepoContext {
  owners: PathOwners[];
  activeLeases: Lease[];
  openChangesTouching: ChangeRequest[];
  failingChecks: CheckRun[];
  landRules: string[];
}

export type ChangeState =
  | "CHANGE_STATE_UNSPECIFIED"
  | "CHANGE_STATE_OPEN"
  | "CHANGE_STATE_LANDED"
  | "CHANGE_STATE_CLOSED";

/** SPEC §11.3, ordered from lowest to highest risk. */
export const DIFF_CLASSES = [
  "generated",
  "docs",
  "comment_only",
  "test_only",
  "rename_only",
  "dependency",
  "logic",
  "sensitive",
] as const;
export type DiffClass = (typeof DIFF_CLASSES)[number];

export interface ChangeRevision {
  n: number;
  head?: ObjectId;
  base?: ObjectId;
  diffClass: string;
  createdAt?: Timestamp;
}
export interface ChangeRequest {
  id: string;
  repoId: string;
  targetRef: string;
  changeIds: ChangeId[];
  state: ChangeState;
  author?: Principal;
  title: string;
  body: string;
  revisions: ChangeRevision[];
  dependsOn: string;
  createdAt?: Timestamp;
  /** Per-repo number shared with issues; runs refer to change requests by number. */
  number: U64;
  /** The change's post, as a read-only summary; absent when it has none. */
  post?: Post;
  /** The post this change was inspired by; absent when none, or when the caller cannot read the source repo. */
  inspiredBy?: PostRef;
  /** What the change does in words (SPEC §11.9); absent until someone writes it. */
  brief?: ChangeBrief;
}
export interface OpenChangeRequest {
  repo: RepoRef;
  targetRef: string;
  head?: ObjectId;
  title: string;
  body: string;
  dependsOn?: string;
  /** A post to draft with the change; agents write one when they open it. */
  postText?: string;
  /** A PUBLISHED post the caller can read; `post_not_readable` otherwise. */
  inspiredByPostId?: string;
  /** The brief to set with the change; the server fills `writtenForRevision`, `author` and `updatedAt`. */
  brief?: ChangeBrief;
}
export interface GetChangeRequest {
  repo: RepoRef;
  id: string;
}
export interface ListChangesRequest {
  repo: RepoRef;
  state?: ChangeState;
  authorId?: string;
  /** GitHub-compatible qualifiers, e.g. "is:open label:bug author:codex". */
  query?: string;
  page?: PageRequest;
}
export interface ListChangesResponse {
  changes: ChangeRequest[];
  page?: PageResponse;
}
export interface CloseChangeRequest {
  repo: RepoRef;
  id: string;
  reason: string;
}
export type Verdict = "VERDICT_UNSPECIFIED" | "VERDICT_APPROVE" | "VERDICT_REQUEST_CHANGES" | "VERDICT_COMMENT";
export interface ReviewComment {
  id: string;
  path: Bytes;
  side: "old" | "new" | "";
  startLine: number;
  endLine: number;
  body: string;
  revision: number;
}
export interface Review {
  id: string;
  changeId: string;
  revision: number;
  reviewer?: Principal;
  verdict: Verdict;
  body: string;
  comments: ReviewComment[];
  createdAt?: Timestamp;
}
export interface SubmitReviewRequest {
  repo: RepoRef;
  changeRequestId: string;
  revision: number;
  verdict: Verdict;
  body: string;
  comments: ReviewComment[];
}
export interface ListReviewsRequest {
  repo: RepoRef;
  changeRequestId: string;
}
export interface ListReviewsResponse {
  reviews: Review[];
}
export interface EvaluateLandingRequest {
  repo: RepoRef;
  changeRequestId: string;
}
export interface LandingEvaluation {
  allowed: boolean;
  satisfied: string[];
  missing: string[];
  diffClass: string;
}

export type CheckStatus =
  | "CHECK_STATUS_UNSPECIFIED"
  | "CHECK_STATUS_QUEUED"
  | "CHECK_STATUS_RUNNING"
  | "CHECK_STATUS_PASSED"
  | "CHECK_STATUS_FAILED"
  | "CHECK_STATUS_SKIPPED";
export interface CheckRun {
  id: string;
  repoId: string;
  changeset?: ObjectId;
  name: string;
  status: CheckStatus;
  url: string;
  summary: string;
  postLand: boolean;
  startedAt?: Timestamp;
  finishedAt?: Timestamp;
  // The fields below are what the change page's Checks tab reads (B9). The server has always
  // sent them; the fixture leaves them out, so every one is optional.
  /** The reporter's rich output (title, summary, Markdown text, annotations, images). */
  output?: CheckOutput;
  /** The app, workflow or agent that reported the check. */
  reporter?: Principal;
  annotationsCount?: number;
  /** Set when a Beehive workflow job produced the check. */
  workflowJobId?: string;
}
/** forge.proto CheckAnnotation, as JSON: the path is base64 bytes. */
export interface CheckAnnotation {
  path: Bytes;
  startLine?: number;
  endLine?: number;
  level?: "ANNOTATION_LEVEL_UNSPECIFIED" | "ANNOTATION_LEVEL_NOTICE" | "ANNOTATION_LEVEL_WARNING" | "ANNOTATION_LEVEL_FAILURE";
  message?: string;
  title?: string;
}
/** forge.proto CheckImage: a picture the reporter links to (never loaded by the web app). */
export interface CheckImage {
  alt?: string;
  imageUrl?: string;
  caption?: string;
}
export interface CheckOutput {
  title?: string;
  summary?: string;
  text?: string;
  annotations?: CheckAnnotation[];
  images?: CheckImage[];
}
export interface ListChecksRequest {
  repo: RepoRef;
  changeset?: ObjectId;
}
export interface ListChecksResponse {
  checks: CheckRun[];
}

export type LeaseMode = "LEASE_MODE_UNSPECIFIED" | "LEASE_MODE_ADVISORY" | "LEASE_MODE_EXCLUSIVE";
export interface Lease {
  id: string;
  repoId: string;
  ref: string;
  paths: Bytes[];
  holder?: Principal;
  changeId?: ChangeId;
  mode: LeaseMode;
  expiresAt?: Timestamp;
  note: string;
}
export interface AcquireLeaseRequest {
  repo: RepoRef;
  ref: string;
  paths: Bytes[];
  mode: LeaseMode;
  ttlSeconds?: number;
  note?: string;
}
export interface AcquireLeaseResponse {
  lease?: Lease;
  overlapping: Lease[];
}
export interface ReleaseLeaseRequest {
  repo: RepoRef;
  id: string;
}
export type ReleaseLeaseResponse = Record<string, never>;
export interface ListLeasesRequest {
  repo: RepoRef;
  paths?: Bytes[];
}
export interface ListLeasesResponse {
  leases: Lease[];
}

export interface Event {
  seq: U64;
  repoId: string;
  kind: string;
  time?: Timestamp;
  principal: string;
  refUpdated?: Ref;
  change?: ChangeRequest;
  review?: Review;
  check?: CheckRun;
  lease?: Lease;
  policyEpoch?: string;
  revokedToken?: string;
  /** The post.* kinds. */
  post?: Post;
  /** Derived from Event.subject and Event.payload_json for kinds without a typed body. */
  note?: EventNote;
}
/** Title and link target of an issue, run, release, security, agent or thread event. */
export interface EventNote {
  /** What happened, in words, e.g. "run completed", "commented on". */
  verb?: string;
  title: string;
  target: string; // e.g. "issue:42", "run:01J…", "release:v1.4.0"; empty when nothing to open
}
export interface SubscribeRequest {
  repo: RepoRef;
  fromSeq?: U64;
  kinds?: string[];
}
/** EventService.ListEvents: events with `seq >= fromSeq`, at most `limit` (server max 1000). */
export interface ListEventsRequest {
  repo: RepoRef;
  fromSeq?: U64;
  kinds?: string[];
  limit?: number;
}
export interface ListEventsResponse {
  events: Event[];
  nextSeq: U64;
}
export interface GetEventLogInfoRequest {
  repo: RepoRef;
}
export interface EventLogInfo {
  oldestSeq: U64;
  latestSeq: U64;
  retentionDays: number;
}

// ---------------------------------------------------------------- vcs.proto

export interface ResolveRefRequest {
  repo: RepoRef;
  name: string;
  minVersion?: U64;
}
export interface ResolveRefResponse {
  ref?: Ref;
  rootTree?: ObjectId;
  stale: boolean;
}
export interface GetChangesetsRequest {
  repo: RepoRef;
  ids: ObjectId[];
  includeMeta: boolean;
}
export interface GetChangesetsResponse {
  changesets: Changeset[];
}
export interface ReadPathRequest {
  repo: RepoRef;
  at: Revision;
  path: Bytes;
}
export interface ReadPathChunk {
  mode: Mode;
  blob?: ObjectId;
  size: U64;
  data: Bytes;
  last: boolean;
}
export interface ListTreeRequest {
  repo: RepoRef;
  at: Revision;
  path: Bytes;
  recursive?: boolean;
  page?: PageRequest;
}
export interface TreeEntry {
  path: Bytes;
  mode: Mode;
  id?: ObjectId;
  size: U64;
  restricted: boolean;
}
export interface ListTreeResponse {
  entries: TreeEntry[];
  page?: PageResponse;
}
export interface LogRequest {
  repo: RepoRef;
  from: Revision;
  path?: Bytes;
  firstParent?: boolean;
  page?: PageRequest;
}
export interface LogResponse {
  changesets: Changeset[];
  page?: PageResponse;
}
export interface BlameRequest {
  repo: RepoRef;
  at: Revision;
  path: Bytes;
}
export interface BlameRange {
  startLine: number;
  lineCount: number;
  changeset?: ObjectId;
  originalStartLine: number;
}
export interface BlameResponse {
  ranges: BlameRange[];
}
export interface DiffRequest {
  repo: RepoRef;
  from: Revision;
  to: Revision;
  paths?: Bytes[];
  includeHunks: boolean;
  contextLines?: number;
}
export interface DiffHunk {
  oldStart: number;
  oldLen: number;
  newStart: number;
  newLen: number;
  lines: string[];
}
export interface DiffItem {
  path: Bytes;
  old?: ConflictSide;
  new?: ConflictSide;
  binary: boolean;
  hunks: DiffHunk[];
  diffClass: string;
}
export interface LandRequest {
  repo: RepoRef;
  ref: string;
  head?: ObjectId;
  expectedVersion?: U64;
  changeRequestId?: string;
}
export interface LandedChange {
  changeId?: ChangeId;
  source?: ObjectId;
  result?: ObjectId;
  skipped: boolean;
}
export interface LandResponse {
  ref?: Ref;
  landed: LandedChange[];
  attempts: number;
}
export interface RefLogEntry {
  version: U64;
  old?: ObjectId;
  new?: ObjectId;
  principal: string;
  time?: Timestamp;
  landed: LandedChange[];
}
export interface GetRefLogRequest {
  repo: RepoRef;
  name: string;
  fromVersion?: U64;
  page?: PageRequest;
}
export interface GetRefLogResponse {
  entries: RefLogEntry[];
  page?: PageResponse;
}
export interface CompareRevisionsRequest {
  repo: RepoRef;
  base?: Revision;
  head?: Revision;
  maxChangesets?: number;
  includeFiles?: boolean;
}
export interface CompareRevisionsResponse {
  mergeBases: ObjectId[];
  aheadBy: U64;
  behindBy: U64;
  changesets: Changeset[];
  truncated: boolean;
}
export interface ListRefsRequest {
  repo: RepoRef;
  prefix?: string;
  page?: PageRequest;
}
export interface ListRefsResponse {
  refs: Ref[];
  page?: PageResponse;
}

// ---------------------------------------------------------------- auth.proto

export interface LoginRequest {
  githubDeviceCode?: string;
  githubStart?: boolean;
  oidcIdToken?: string;
  /** A GitHub access token; the server proves the account with GitHub and logs in its principal. */
  githubToken?: string;
}
export interface LoginResponse {
  token: string;
  verificationUri: string;
  userCode: string;
  deviceCode: string;
  intervalSeconds: number;
}
export type WhoAmIRequest = Record<string, never>;
export interface Budget {
  maxChangesets: U64;
  maxLanded: U64;
  maxUploadBytes: U64;
}
export interface TokenScope {
  repos: string[];
  readPaths: Bytes[];
  writePaths: Bytes[];
  refs: string[];
  actions: string[];
  expiresAt?: Timestamp;
  budget?: Budget;
  session: string;
}
export interface WhoAmIResponse {
  principal?: Principal;
  onBehalfOf: string[];
  effectiveScope?: TokenScope;
  remaining?: Budget;
}
export interface MintTokenRequest {
  principalId: string;
  scope: TokenScope;
}
export interface MintTokenResponse {
  token: string;
  tokenId: string;
}
export interface RevokeRequest {
  tokenId: string;
}
export type RevokeResponse = Record<string, never>;

// ---------------------------------------------------------------- import.proto

export type ImportPhase =
  | "IMPORT_PHASE_UNSPECIFIED"
  | "IMPORT_PHASE_DRY_RUN"
  | "IMPORT_PHASE_MIRROR"
  | "IMPORT_PHASE_BRIDGE"
  | "IMPORT_PHASE_CUTOVER";
export type ImportState =
  | "IMPORT_STATE_UNSPECIFIED"
  | "IMPORT_STATE_QUEUED"
  | "IMPORT_STATE_GIT"
  | "IMPORT_STATE_METADATA"
  | "IMPORT_STATE_LIVE"
  | "IMPORT_STATE_FAILED"
  | "IMPORT_STATE_CANCELLED";
export interface ImportProgress {
  category: string;
  done: U64;
  total: U64;
}
export interface Import {
  id: string;
  source: string;
  repoIds: string[];
  phase: ImportPhase;
  state: ImportState;
  progress: ImportProgress[];
  eta?: Timestamp;
  rateLimitNote: string;
  startedAt?: Timestamp;
  liveSince?: Timestamp;
}
export interface StartImportRequest {
  source: string;
  org: boolean;
  targetOrg: string;
  phase: ImportPhase;
  githubInstallationId?: string;
  githubToken?: string;
}
export interface GetImportRequest {
  id: string;
}
export interface ListImportsRequest {
  targetOrg: string;
  page?: PageRequest;
}
export interface ListImportsResponse {
  imports: Import[];
  page?: PageResponse;
}
export interface SetImportPhaseRequest {
  id: string;
  phase: ImportPhase;
}
export interface CancelImportRequest {
  id: string;
}
export interface GetFidelityReportRequest {
  id: string;
}
export interface FidelityDrop {
  ref: string;
  reason: string;
}
export interface FidelityItem {
  category: string;
  imported: U64;
  archived: U64;
  dropped: U64;
  drops: FidelityDrop[];
}
export interface FidelityReport {
  importId: string;
  items: FidelityItem[];
  manualSteps: string[];
  generatedAt?: Timestamp;
}

// ---------------------------------------------------------------- briefs.proto

/** forge.proto: one thing a change does, with the paths that do it. */
export interface BriefHighlight {
  text: string;
  paths: string[];
}
/** A decision the author asks a person to make; answering posts `Decision on "<question>": <option>`. */
export interface BriefAsk {
  question: string;
  options: string[];
}
export interface BriefRisk {
  text: string;
  mitigation: string;
}
export interface ChangeBrief {
  outcome: string;
  highlights: BriefHighlight[];
  asks: BriefAsk[];
  risks: BriefRisk[];
  rollback: string;
  tryItUrl: string;
  /** Server-set. */
  writtenForRevision?: number;
  author?: Principal;
  updatedAt?: Timestamp;
}

export type EvidenceKind =
  | "EVIDENCE_KIND_UNSPECIFIED"
  | "EVIDENCE_KIND_SCREENSHOT"
  | "EVIDENCE_KIND_BEFORE_AFTER"
  | "EVIDENCE_KIND_VIDEO"
  | "EVIDENCE_KIND_BENCHMARK"
  | "EVIDENCE_KIND_IMAGE"
  | "EVIDENCE_KIND_AUDIO"
  | "EVIDENCE_KIND_DOCUMENT"
  | "EVIDENCE_KIND_PREVIEW_LINK"
  | "EVIDENCE_KIND_TEST_REPORT";
/** UPLOADED reads as "From the agent"; the other two as "Verified by CI". */
export type EvidenceSource = "EVIDENCE_SOURCE_UNSPECIFIED" | "EVIDENCE_SOURCE_UPLOADED" | "EVIDENCE_SOURCE_CHECK_RUN" | "EVIDENCE_SOURCE_WORKFLOW_ARTIFACT";
export type EvidenceState = "EVIDENCE_STATE_UNSPECIFIED" | "EVIDENCE_STATE_PENDING" | "EVIDENCE_STATE_READY";
export type EvidenceAssetRole =
  | "EVIDENCE_ASSET_ROLE_UNSPECIFIED"
  | "EVIDENCE_ASSET_ROLE_MAIN"
  | "EVIDENCE_ASSET_ROLE_BEFORE"
  | "EVIDENCE_ASSET_ROLE_AFTER"
  | "EVIDENCE_ASSET_ROLE_POSTER"
  | "EVIDENCE_ASSET_ROLE_CAPTIONS";
export interface BenchmarkSeries {
  name: string;
  unit: string;
  baseline: number;
  value: number;
  lowerIsBetter: boolean;
}
export interface TestFailure {
  name: string;
  message: string;
}
export interface TestReport {
  passed: number;
  failed: number;
  skipped: number;
  durationMs: U64;
  failures: TestFailure[];
  summary: string;
}
export interface EvidenceSpec {
  kind: EvidenceKind;
  title: string;
  caption: string;
  benchmark: BenchmarkSeries[];
  testReport?: TestReport;
  /** PREVIEW_LINK only; https. */
  url: string;
  paths: string[];
  hero: boolean;
  order: number;
}
export interface EvidenceAsset {
  role: EvidenceAssetRole;
  /** The type the server sniffed. */
  contentType: string;
  /** `sha256:<hex>` of the bytes. */
  digest: string;
  sizeBytes: U64;
  width: number;
  height: number;
  durationMs: U64;
  /** Required for images. */
  alt: string;
  /**
   * Same-origin: the mapping rewrites the server's signed `/_evidence/…` URL to
   * `/api/evidence/…` (SPEC web-ui §9), so the browser never reaches the bee server.
   */
  url: string;
  urlExpiresAt?: Timestamp;
}
export interface Evidence {
  id: string;
  repoId: string;
  changeRequestId: string;
  /** The revision the evidence was captured for. */
  revision: number;
  head?: ObjectId;
  spec?: EvidenceSpec;
  assets: EvidenceAsset[];
  source: EvidenceSource;
  checkRunId: string;
  workflowArtifactId: string;
  uploader?: Principal;
  state: EvidenceState;
  /** Computed on read: the change request has a newer revision. */
  stale: boolean;
  createdAt?: Timestamp;
  updatedAt?: Timestamp;
}
export interface BriefChecklistItem {
  /** "brief", "brief:risk" or "evidence:ui". */
  id: string;
  /** In words, e.g. "a screenshot of the UI change". */
  detail: string;
  kinds: EvidenceKind[];
  paths: string[];
  satisfied: boolean;
  evidenceIds: string[];
}
/** Advisory: what the brief and evidence of one revision still owe. */
export interface BriefChecklist {
  revision: number;
  complete: boolean;
  items: BriefChecklistItem[];
}
export interface SetChangeBriefRequest {
  repo: RepoRef;
  changeId: string;
  brief: ChangeBrief;
  /** 0 = any. */
  expectedVersion?: U64;
}
export interface SetChangeBriefResponse {
  brief?: ChangeBrief;
  checklist?: BriefChecklist;
  version: U64;
}
export interface GetBriefChecklistRequest {
  repo: RepoRef;
  changeId: string;
  /** 0 = current. */
  revision?: number;
}
export interface InlineEvidenceAsset {
  role: EvidenceAssetRole;
  contentType: string;
  /** base64. */
  data: Bytes;
  alt?: string;
  width?: number;
  height?: number;
  durationMs?: U64;
}
export interface AddEvidenceRequest {
  repo: RepoRef;
  changeId: string;
  spec: EvidenceSpec;
  /** 0 = current. */
  revision?: number;
  checkRunId?: string;
  assets?: InlineEvidenceAsset[];
  workflowArtifactId?: string;
}
export interface EvidenceUploadAsset {
  role: EvidenceAssetRole;
  contentType: string;
  sizeBytes: U64;
  alt?: string;
  width?: number;
  height?: number;
  durationMs?: U64;
}
export interface BeginEvidenceUploadRequest {
  repo: RepoRef;
  changeId: string;
  spec: EvidenceSpec;
  revision?: number;
  checkRunId?: string;
  assets: EvidenceUploadAsset[];
}
export interface EvidenceUploadTarget {
  role: EvidenceAssetRole;
  uploadUrl: string;
  uploadHeaders: Record<string, string>;
  expiresAt?: Timestamp;
}
export interface BeginEvidenceUploadResponse {
  evidence?: Evidence;
  uploads: EvidenceUploadTarget[];
}
export interface EvidenceAssetDigest {
  role: EvidenceAssetRole;
  /** Lowercase hex. */
  sha256: string;
}
export interface FinalizeEvidenceRequest {
  repo: RepoRef;
  evidenceId: string;
  digests: EvidenceAssetDigest[];
}
export interface GetEvidenceRequest {
  repo: RepoRef;
  id: string;
}
export interface ListEvidenceRequest {
  repo: RepoRef;
  /** 1 to 100. */
  changeIds: string[];
  /** At most one item per change: its hero, else its first. */
  heroOnly?: boolean;
  page?: PageRequest;
}
export interface ListEvidenceResponse {
  evidence: Evidence[];
  page?: PageResponse;
}
export interface EvidenceAltText {
  role: EvidenceAssetRole;
  alt: string;
}
export interface UpdateEvidenceRequest {
  repo: RepoRef;
  id: string;
  spec: EvidenceSpec;
  altTexts?: EvidenceAltText[];
}
export interface DeleteEvidenceRequest {
  repo: RepoRef;
  id: string;
}
export type DeleteEvidenceResponse = Record<string, never>;

// ---------------------------------------------------------------- comments and sessions

/** content.proto Comment: here a conversation comment on a change request, including `Decision on "<q>": <option>` answers. */
export interface Comment {
  id: string;
  subject?: { kind: SubjectKindJson; repo?: RepoRef; id: string };
  author?: Principal;
  body: string;
  createdAt?: Timestamp;
  updatedAt?: Timestamp;
  edited: boolean;
}
/** ChangeService.ListChangeComments: the change request's conversation, oldest first. */
export interface ListCommentsRequest {
  repo: RepoRef;
  changeRequestId: string;
  page?: PageRequest;
}
export interface ListCommentsResponse {
  comments: Comment[];
  page?: PageResponse;
}
/** ChangeService.CreateChangeComment without an anchor: a conversation comment. */
export interface CommentChangeRequest {
  repo: RepoRef;
  changeRequestId: string;
  body: string;
}
/** agents.proto AgentSession: one run's provenance. */
export interface AgentSession {
  session: string;
  runId: string;
  agentPrincipalId: string;
  model: string;
  runtime: string;
  promptDigest: Bytes;
  chain: string[];
  changesets: ObjectId[];
  changeRequestIds: string[];
  cost?: Cost;
  createdAt?: Timestamp;
}
/** At least one of `changeset` and `changeRequest` (a number or id). */
export interface GetSessionsForChangeRequest {
  repo: RepoRef;
  changeset?: ObjectId;
  changeRequest?: string;
}
export interface GetSessionsForChangeResponse {
  sessions: AgentSession[];
}

// ---------------------------------------------------------------- posts.proto

export type PostState =
  | "POST_STATE_UNSPECIFIED"
  | "POST_STATE_DRAFT"
  | "POST_STATE_PENDING_APPROVAL"
  | "POST_STATE_PUBLISHED"
  | "POST_STATE_WITHDRAWN"
  | "POST_STATE_DISCARDED";
export type PostMediaKind = "POST_MEDIA_KIND_UNSPECIFIED" | "POST_MEDIA_KIND_IMAGE" | "POST_MEDIA_KIND_VIDEO";
/** A repo as a post names it: the server fills both members on reads. */
export interface PostRepo {
  id?: string;
  fullName?: string;
}
export interface PostMedia {
  id: string;
  kind: PostMediaKind;
  contentType: string;
  sizeBytes: U64;
  width: number;
  height: number;
  /** Video only. */
  durationMs: number;
  /** Required for images, optional for video. */
  altText: string;
  sha256: string;
  /** A signed download URL, filled on every read and never stored. */
  url: string;
}
export interface PostCounts {
  likes: U64;
  reposts: U64;
  comments: U64;
  /** Adoptions in repos the caller can read. */
  adoptions: U64;
}
export interface PostViewer {
  liked: boolean;
  reposted: boolean;
  canEdit: boolean;
  canApprove: boolean;
  canWithdraw: boolean;
}
export interface Post {
  id: string;
  repo?: PostRepo;
  changeId: string;
  changeNumber: U64;
  changeTitle: string;
  /** May be an agent. */
  author?: Principal;
  text: string;
  media: PostMedia[];
  state: PostState;
  createdAt?: Timestamp;
  updatedAt?: Timestamp;
  publishedAt?: Timestamp;
  approvedBy?: Principal;
  counts?: PostCounts;
  viewer?: PostViewer;
  version: U64;
  withdrawnReason: string;
}
/** Set when the post is in the feed because someone the caller follows reposted it. */
export interface FeedItem {
  post?: Post;
  repostedBy?: Principal;
  /** `published_at`, or the repost time. */
  at?: Timestamp;
}
export interface PostRef {
  postId: string;
  repo?: PostRepo;
  changeNumber: U64;
  changeTitle: string;
}
/** What adopted a post: an issue (id is its number) or a change request (id is its id). */
export interface PostSubject {
  kind: SubjectKindJson;
  id: string;
  repo?: PostRepo;
}
export interface PostAdoption {
  repo?: PostRepo;
  subject?: PostSubject;
  title: string;
  author?: Principal;
  createdAt?: Timestamp;
}
export interface SetChangePostRequest {
  repo: RepoRef;
  changeId: string;
  text: string;
  mediaIds?: string[];
  expectedVersion?: U64;
}
export interface ClearChangePostRequest {
  repo: RepoRef;
  changeId: string;
}
export type ClearChangePostResponse = Record<string, never>;
export interface GetChangePostRequest {
  repo: RepoRef;
  changeId: string;
}
export interface BeginPostMediaUploadRequest {
  repo: RepoRef;
  changeId: string;
  kind: PostMediaKind;
  contentType: string;
  sizeBytes: U64;
}
export interface BeginPostMediaUploadResponse {
  mediaId: string;
  uploadUrl: string;
  uploadHeaders: Record<string, string>;
  expiresAt?: Timestamp;
}
export interface FinalizePostMediaRequest {
  repo: RepoRef;
  mediaId: string;
  sha256: string;
  altText?: string;
  width?: number;
  height?: number;
  durationMs?: number;
}
export interface ApprovePostRequest {
  repo: RepoRef;
  postId: string;
}
export interface WithdrawPostRequest {
  repo: RepoRef;
  postId: string;
  reason?: string;
}
export interface GetPostRequest {
  postId: string;
}
export interface ListRepoPostsRequest {
  repo: RepoRef;
  page?: PageRequest;
}
export interface ListRepoPostsResponse {
  posts: Post[];
  page?: PageResponse;
}
export interface LikePostRequest {
  postId: string;
}
export type UnlikePostRequest = LikePostRequest;
export type RepostPostRequest = LikePostRequest;
export type UndoRepostRequest = LikePostRequest;
export interface ListFollowingFeedRequest {
  page?: PageRequest;
  /** Poll mode: only items newer than this cursor, oldest first. */
  after?: string;
}
export interface ListFollowingFeedResponse {
  items: FeedItem[];
  page?: PageResponse;
  /** The cursor to save for the next `after`. */
  newestCursor: string;
}
export interface ListPostAdoptionsRequest {
  postId: string;
  page?: PageRequest;
}
export interface ListPostAdoptionsResponse {
  adoptions: PostAdoption[];
  page?: PageResponse;
}

// ================================================================ VIEWS
//
// View models the screens use for areas where the UI composes several bee.v1 calls, or where
// the proto shape is richer than a screen needs. `lib/api/ops/` maps them to and from the real
// messages (named in each comment).

/** AgentService.ListAgentPrincipals (AgentPrincipalSummary) plus AuthService.GetPrincipal for the owner. */
export interface AgentSummary {
  principal: Principal;
  owner?: Principal;
  purpose: string;
  /** Derived: active with a lease or work in the last 6 hours, else idle. */
  status: "active" | "idle" | "suspended";
  /** AgentPrincipalSummary.last_model */
  model: string;
  /** AgentPrincipalSummary.last_session */
  session: string;
  /** AgentPrincipalSummary.open_change_requests */
  openChanges: number;
  /** AgentStats.outcomes.change_requests_landed over the 7-day window. */
  landed7d: number;
  /** AgentStats.cost.usd_micros over the 7-day window. */
  spend7dUsdMicros: U64;
  activeLeases: number;
  lastSeen?: Timestamp;
  /**
   * False for an agent principal that is an org member but has no agent definition in the
   * registry (definitions have no RPC yet, docs/spec/agents.md §9): its numbers are then derived
   * from the repo's history (QueryService.Log meta), change requests and leases.
   */
  registered: boolean;
}
export interface ListAgentsRequest {
  org: string;
  /** The repo whose history, change requests and leases fill in unregistered agents. */
  repo?: RepoRef;
}
export interface ListAgentsResponse {
  agents: AgentSummary[];
}
export interface GetAgentRequest {
  /** ListAgentPrincipals is per org; there is no single-agent summary RPC. */
  org: string;
  principalId: string;
  repo?: RepoRef;
}
/** agents.proto: when a followed repo publishes a post, the agent evaluates whether to adopt the upgrade. */
export interface AgentFeedTrigger {
  repo?: PostRepo;
  agent?: Principal;
  enabled: boolean;
  dailyUsdBudget: number;
  maxRunsPerDay: number;
  sources: PostRepo[];
  /** Posts that arrive within this window are folded into one run. */
  checkIntervalSeconds: U64;
  runsToday: number;
  spentUsdToday: number;
  createdAt?: Timestamp;
  updatedAt?: Timestamp;
}
export interface SetAgentFeedTriggerRequest {
  repo: RepoRef;
  /** The agent's principal id. */
  agent: string;
  enabled: boolean;
  dailyUsdBudget?: number;
  maxRunsPerDay?: number;
  sources?: RepoRef[];
}
export interface GetAgentFeedTriggerRequest {
  repo: RepoRef;
  agent: string;
}
export interface ListAgentFeedTriggersRequest {
  repo: RepoRef;
  page?: PageRequest;
}
export interface ListAgentFeedTriggersResponse {
  triggers: AgentFeedTrigger[];
  page?: PageResponse;
}

/**
 * One feed-trigger run: an agent evaluating a followed repo's post (AGENT_TASK_KIND_ADOPT_EVALUATE)
 * from AgentService.ListAgentRuns. The outcome is read from `AgentRun.summary`, which a run writes
 * as "not relevant: <reason>", "opened issue #<n>: <title>" or "opened change request #<n>: <title>".
 */
export type AdoptOutcome = "running" | "not_relevant" | "opened_issue" | "opened_change" | "failed";
export interface AdoptRun {
  id: string;
  agent?: Principal;
  postId: string;
  /** The post that was evaluated, when the caller can still read it. */
  post?: PostRef;
  outcome: AdoptOutcome;
  /** Why it was not relevant, or why the run failed; empty otherwise. */
  reason: string;
  /** The issue or change request the run opened, in the agent's repo. */
  opened?: { kind: "issue" | "change"; number: U64; title: string; repo?: PostRepo };
  startedAt?: Timestamp;
  finishedAt?: Timestamp;
  costUsdMicros: U64;
}
export interface ListAdoptRunsRequest {
  repo: RepoRef;
  /** An agent's principal id; every agent's runs when omitted. */
  agentPrincipalId?: string;
  limit?: number;
}
export interface ListAdoptRunsResponse {
  runs: AdoptRun[];
}

/** AuthService.ListTokens (TokenInfo): a token as shown to its owner; never the secret. */
export interface TokenInfo {
  tokenId: string;
  parentTokenId: string;
  principalId: string;
  scope: TokenScope;
  /** TokenInfo.used: consumption against scope.budget */
  used?: Budget;
  createdAt?: Timestamp;
  revoked: boolean;
  /** TokenInfo.description */
  label: string;
}
export interface ListTokensRequest {
  principalId: string;
}
export interface ListTokensResponse {
  tokens: TokenInfo[];
}
/**
 * One record per changeset an agent created: AgentService.ListAgentWorkItems, with the
 * provenance from QueryService.GetChangesets and the change request from ListChanges.
 */
export interface ProvenanceRecord {
  changeset?: ObjectId;
  repo: string;
  message: string;
  changeRequestId: string;
  landed: boolean;
  provenance: Provenance;
  createdAt?: Timestamp;
}
export interface ListProvenanceRequest {
  principalId: string;
  page?: PageRequest;
  /** For agents without a registry definition: the repo whose history is searched. */
  repo?: RepoRef;
}
export interface ListProvenanceResponse {
  records: ProvenanceRecord[];
  page?: PageResponse;
}

/** A comment in a review thread (ReviewComment, or Comment for a thread's first comment). */
export interface ThreadComment {
  id: string;
  author?: Principal;
  body: string;
  /** Suggestion.replacement; empty when the comment is not a suggestion. */
  suggestion: string;
  createdAt?: Timestamp;
}
/** ChangeService.ListReviewThreads (ReviewThread). */
export interface ReviewThread {
  id: string;
  changeRequestId: string;
  path: Bytes;
  side: "old" | "new";
  startLine: number;
  endLine: number;
  revision: number;
  resolved: boolean;
  outdated: boolean;
  comments: ThreadComment[];
}
export interface ListThreadsRequest {
  repo: RepoRef;
  changeRequestId: string;
}
export interface ListThreadsResponse {
  threads: ReviewThread[];
}
/** ResolveReviewThread or UnresolveReviewThread. */
export interface ResolveThreadRequest {
  repo: RepoRef;
  threadId: string;
  resolved: boolean;
}
/** ReplyToReviewThread; answers the new comment. */
export interface ReplyThreadRequest {
  repo: RepoRef;
  threadId: string;
  body: string;
}
/** CreateChangeComment with a line anchor (which starts a thread); answers the new comment. */
export interface CreateThreadRequest {
  repo: RepoRef;
  changeRequestId: string;
  revision: number;
  path: Bytes;
  side: "old" | "new";
  startLine: number;
  endLine: number;
  body: string;
  suggestion?: string;
}
/** ChangeService.ApplySuggestions: batch-applies suggestion comments as a new revision. */
export interface ApplySuggestionsRequest {
  repo: RepoRef;
  changeRequestId: string;
  /** Review comments carrying suggestions (the proto applies comments, not threads). */
  commentIds: string[];
}
/**
 * File-level diff of a revision: QueryService.Diff from the revision's base to its head, or
 * ChangeService.DiffChangeRevisions between two revisions of one CR ("changes since").
 */
export interface GetChangeDiffRequest {
  repo: RepoRef;
  changeRequestId: string;
  revision: number;
  /** When non-zero, diff is from this revision's head instead of the base ("changes since"). */
  sinceRevision?: number;
  /** The revision's base and head when the caller has them (saves a GetChange). */
  base?: ObjectId;
  head?: ObjectId;
}
export interface GetChangeDiffResponse {
  items: DiffItem[];
}
/** ChangeService.GetMergeQueue (MergeQueueEntry), with titles from GetChange. */
export interface QueueEntry {
  changeRequestId: string;
  title: string;
  position: number;
  state: "waiting" | "testing" | "landing";
  /** Short id of the speculative head the entry is checked in; entries sharing it land together. */
  train: string;
  enqueuedAt?: Timestamp;
}
export interface ListQueueRequest {
  repo: RepoRef;
  ref: string;
}
export interface ListQueueResponse {
  entries: QueueEntry[];
  landsPerMinute: number;
}
/** ChangeService.EnqueueChange (policy is evaluated again at the front). */
export interface EnqueueRequest {
  repo: RepoRef;
  changeRequestId: string;
}

/** WorkflowService runs (WorkflowRun), jobs (WorkflowJob) and logs (TailWorkflowJobLogs). */
export type RunStatus = "queued" | "running" | "passed" | "failed" | "cancelled";
export interface RunStep {
  name: string;
  status: RunStatus;
  durationMs: U64;
}
export interface RunJob {
  id: string;
  name: string;
  status: RunStatus;
  steps: RunStep[];
  startedAt?: Timestamp;
  finishedAt?: Timestamp;
  runner: string;
}
/** CheckService.ListCheckAnnotations of each job's check run. */
export interface Annotation {
  path: string;
  line: number;
  level: "failure" | "warning" | "notice";
  title: string;
  message: string;
}
export interface Run {
  id: string;
  repoId: string;
  workflow: string;
  number: number;
  /** WorkflowRun.event, e.g. "change_request", "land", "schedule", "manual". */
  trigger: string;
  ref: string;
  head?: ObjectId;
  /** Resolved from WorkflowRun.change_request_numbers through GetChangeByNumber. */
  changeRequestId: string;
  actor?: Principal;
  status: RunStatus;
  jobs: RunJob[];
  /** Filled by GetRun only; lists leave it empty. */
  annotations: Annotation[];
  attempt: number;
  startedAt?: Timestamp;
  finishedAt?: Timestamp;
}
export interface ListRunsRequest {
  repo: RepoRef;
  status?: RunStatus | "";
  changeRequestId?: string;
  page?: PageRequest;
}
export interface ListRunsResponse {
  runs: Run[];
  page?: PageResponse;
}
export interface GetRunRequest {
  repo: RepoRef;
  id: string;
}
/** RerunFailedWorkflowJobs or RerunWorkflowRun. */
export interface RerunRequest {
  repo: RepoRef;
  id: string;
  failedOnly: boolean;
}
export interface StreamLogsRequest {
  repo: RepoRef;
  runId: string;
  jobId: string;
  /** Byte offset to resume from (the `end` of the last line received). */
  fromOffset?: U64;
}
/** One line of a job log. Levels come from `##[group]`, `##[error]` and `##[warning]` markers. */
export interface LogLine {
  n: number;
  jobId: string;
  text: string;
  level: "info" | "group" | "error" | "warning";
  /** Byte offset just past this line, for resuming. */
  end: U64;
}

/** IssueService (Issue, comments from ListIssueComments, sub-issues from ListIssueSubIssues). */
export interface Issue {
  id: string;
  repoId: string;
  number: number;
  title: string;
  body: string;
  state: "open" | "closed";
  /** IssueType.name, lower case. */
  type: string;
  labels: string[];
  milestone: string;
  assignees: Principal[];
  author?: Principal;
  parentNumber: number;
  /** Sub-issue numbers; filled by GetIssue (lists carry only the counts below). */
  subIssues: number[];
  subIssuesTotal: number;
  subIssuesDone: number;
  linkedChanges: string[];
  /** Filled by GetIssue; lists carry only `commentCount`. */
  comments: IssueComment[];
  commentCount: number;
  createdAt?: Timestamp;
  /** The post this issue was inspired by; absent when none, or when the caller cannot read the source repo. */
  inspiredBy?: PostRef;
}
export interface IssueComment {
  id: string;
  author?: Principal;
  body: string;
  createdAt?: Timestamp;
}
export interface ListIssuesRequest {
  repo: RepoRef;
  state?: "open" | "closed" | "";
  label?: string;
  assigneeId?: string;
}
export interface ListIssuesResponse {
  issues: Issue[];
}
/** IssueService.CreateIssue; the issue is returned as GetIssue gives it. */
export interface CreateIssueRequest {
  repo: RepoRef;
  title: string;
  body?: string;
  assignees?: string[];
  labels?: string[];
  /** A PUBLISHED post the caller can read; `post_not_readable` otherwise. */
  inspiredByPostId?: string;
}
export interface GetIssueRequest {
  repo: RepoRef;
  number: number;
}
/** Sets the full assignee list: AddIssueAssignees and RemoveIssueAssignees for the difference. */
export interface AssignIssueRequest {
  repo: RepoRef;
  number: number;
  assigneeIds: string[];
}
/** CloseIssue or ReopenIssue. */
export interface SetIssueStateRequest {
  repo: RepoRef;
  number: number;
  state: "open" | "closed";
}
export interface CommentIssueRequest {
  repo: RepoRef;
  number: number;
  body: string;
}
/** A board: the repo's first project, its "Status" single-select options as columns. */
export interface ProjectColumn {
  /** ProjectFieldOption.id */
  id: string;
  name: string;
  issueNumbers: number[];
}
export interface Project {
  id: string;
  name: string;
  repoId: string;
  columns: ProjectColumn[];
}
export interface GetProjectRequest {
  repo: RepoRef;
}
/** SetProjectItemFieldValue (Status) then MoveProjectItem to the top of the column. */
export interface MoveItemRequest {
  repo: RepoRef;
  projectId: string;
  issueNumber: number;
  toColumnId: string;
}

/** ReleaseService (Release). */
export interface ReleaseAsset {
  name: string;
  size: U64;
  downloads: number;
}
export interface Release {
  id: string;
  repoId: string;
  tag: string;
  name: string;
  notes: string;
  target?: ObjectId;
  author?: Principal;
  prerelease: boolean;
  immutable: boolean;
  assets: ReleaseAsset[];
  createdAt?: Timestamp;
}
export interface ListReleasesRequest {
  repo: RepoRef;
}
export interface ListReleasesResponse {
  releases: Release[];
}
export type SourceArchiveFormat = "SOURCE_ARCHIVE_FORMAT_UNSPECIFIED" | "SOURCE_ARCHIVE_FORMAT_ZIP" | "SOURCE_ARCHIVE_FORMAT_TAR_GZ";
export interface GetSourceArchiveUrlRequest {
  repo: RepoRef;
  at?: Revision;
  format?: SourceArchiveFormat;
  prefix?: string;
}
export interface GetSourceArchiveUrlResponse {
  url: string;
  expiresAt?: Timestamp;
  changeset?: ObjectId;
  /** The tree id the archive is cached by. */
  tree?: ObjectId;
  filtered: boolean;
}
export interface GenerateNotesRequest {
  repo: RepoRef;
  tag: string;
  previousTag?: string;
}
export interface GenerateNotesResponse {
  notes: string;
}
export interface CreateReleaseRequest {
  repo: RepoRef;
  tag: string;
  name: string;
  notes: string;
  prerelease: boolean;
}

/**
 * SecurityService: dependency, secret-scanning and code-scanning alerts in one list. Ids are
 * `<kind>:<number>` because alert numbers are per kind.
 */
export interface SecurityAlert {
  id: string;
  repoId: string;
  kind: "dependency" | "secret" | "code_scanning";
  severity: "critical" | "high" | "medium" | "low";
  state: "open" | "dismissed" | "fixed";
  title: string;
  detail: string;
  location: string;
  advisory: string;
  fixChangeRequestId: string;
  dismissedReason: string;
  createdAt?: Timestamp;
}
export interface ListAlertsRequest {
  repo: RepoRef;
  state?: SecurityAlert["state"] | "";
}
export interface ListAlertsResponse {
  alerts: SecurityAlert[];
}
/** UpdateDependencyAlert, UpdateSecretScanningAlert or UpdateCodeScanningAlert; `reason` is the comment. */
export interface DismissAlertRequest {
  repo: RepoRef;
  id: string;
  reason: string;
}

/** PolicyService.GetEffectivePolicy, with the repo file read through QueryService.ReadPath. */
export interface Policy {
  path: string;
  source: string;
  epoch: string;
  atChangeset?: ObjectId;
}
export interface GetPolicyRequest {
  repo: RepoRef;
}
export interface PolicyError {
  line: number;
  message: string;
}
export interface PolicyDecision {
  changeRequestId: string;
  title: string;
  diffClass: string;
  current: boolean;
  proposed: boolean;
  missing: string[];
}
/** PolicyService.ValidatePolicy with replay_sample: errors, rule summaries and the replay. */
export interface EvaluatePolicyRequest {
  repo: RepoRef;
  source: string;
  /** How many recent lands to replay against the proposed policy. */
  sample: number;
}
export interface PolicyEvaluation {
  valid: boolean;
  errors: PolicyError[];
  /** ValidatePolicyResponse.rule_summaries */
  rules: string[];
  /** ValidatePolicyResponse.replay */
  decisions: PolicyDecision[];
}

/** AuditService.QueryAuditLog (AuditEntry) and VerifyAuditChain. */
export interface AuditEntry {
  seq: U64;
  time?: Timestamp;
  actor?: Principal;
  /** Display names along AuditEntry.delegation_chain, after the actor. */
  onBehalfOf: string[];
  action: string;
  /** AuditEntry.target_display; falls back to the target kind and id. */
  target: string;
  repo: string;
  /** AuditEntry.result is "ok". */
  allowed: boolean;
  /** AuditEntry.policy_rule */
  rule: string;
  /** AuditEntry.user_agent and ip. */
  source: string;
  /** Hex of AuditEntry.hash and prev_hash. */
  hash: string;
  prevHash: string;
}
export interface ListAuditRequest {
  org: string;
  actorId?: string;
  action?: string;
  repo?: string;
  /** AuditLogFilter has no result filter yet; the mapping filters the page it gets. */
  deniedOnly?: boolean;
  page?: PageRequest;
}
export interface ListAuditResponse {
  entries: AuditEntry[];
  chainVerified: boolean;
  page?: PageResponse;
}

// ================================================================ HUB VIEWS
//
// Packages, Pages, search, teams and apps. These screens read the proto3-JSON messages as they
// are (the generated `*Json` types), so there is no hand-written subset to keep in step.

export type Package = PackageJson;
export type PackageVersion = PackageVersionJson;
export type PagesSite = PagesSiteJson;
export type PagesDeployment = PagesDeploymentJson;
export type CodeSearchHit = CodeSearchHitJson;
export type Team = TeamJson;
export type App = AppJson;
export type AppInstallation = AppInstallationJson;

/** PackageService.ListPackages, plus each package's newest version (ListPackageVersions). */
export interface ListPackagesRequest {
  owner: string;
  repo?: RepoRef;
}
export interface PackageSummary {
  package: Package;
  latest?: PackageVersion;
}
export interface ListPackagesResponse {
  packages: PackageSummary[];
}
/** PagesService.GetPagesSite and ListPagesDeployments; `site` is absent when the repo has none. */
export interface GetPagesRequest {
  repo: RepoRef;
}
export interface PagesView {
  site?: PagesSite;
  deployments: PagesDeployment[];
}
/** SearchService.SearchCode. */
export interface SearchCodeRequest {
  query: string;
  page?: PageRequest;
}
export interface SearchCodeResponse {
  hits: CodeSearchHit[];
  totalCount: U64;
  incomplete: boolean;
}
/** TeamService.ListTeams. */
export interface ListTeamsRequest {
  org: string;
}
export interface ListTeamsResponse {
  teams: Team[];
}
/** AppService.ListApps (apps the org owns) and ListAppInstallations (apps installed on it). */
export interface ListAppsRequest {
  org: string;
}
export interface ListAppsResponse {
  apps: App[];
  installations: AppInstallation[];
}

// ================================================================ COMMUNITY, DISCOVERY, CLOUD AND ADMIN VIEWS
//
// Repo home, profiles, stars and follows, the home feed, trending and leaderboards, topics,
// billing, the server's own info and the admin console. The community and discovery shapes are
// declared here (and checked against the proto in `ops/conformance.ts`); billing, enterprise
// and ops pass the generated `*Json` messages through, like the hub views above.

// ---------------------------------------------------------------- repos

export interface CloneUrls {
  https: string;
  /** Absent when the server does not advertise an SSH endpoint. */
  ssh?: string;
}
export interface LanguageStat {
  name: string;
  byteCount: U64;
  fileCount: number;
  percentage: number;
  color: string;
}
export interface ReadmeFile {
  /** The file that was read, e.g. "README.md". */
  path: string;
  markdown: string;
  /** The file was larger than the 256 KiB the repo home reads. */
  truncated: boolean;
}
/** RepoService.ListRepos across every org the caller belongs to. */
export interface ListMyReposRequest {
  page?: PageRequest;
}
export interface ListMyReposResponse {
  repos: Repo[];
}
/**
 * RepoService.GetRepo, QueryService.ReadPath (README), InsightsService.GetLanguageStats,
 * QueryService.Log (latest land), LeaseService.ListLeases, UserService.IsRepoStarred and
 * ObjectService.GetServerInfo (clone URLs), in parallel. `leasesVisible` is false when the
 * server does not show leases to this caller (signed out); `starredByMe` is then false too.
 */
export interface GetRepoHomeRequest {
  fullName: string;
}
export interface RepoHome {
  repo: Repo;
  readme?: ReadmeFile;
  languages: LanguageStat[];
  latestLand?: Changeset;
  openChanges: number;
  leases: Lease[];
  leasesVisible: boolean;
  cloneUrls: CloneUrls;
  starredByMe: boolean;
}

// ---------------------------------------------------------------- search and discovery

export type SearchSortOrder = SearchSortOrderJson;
export type TrendingWindow = TrendingWindowJson;
export type LeaderboardSubject = LeaderboardSubjectJson;
export type LeaderboardMetric = LeaderboardMetricJson;

export interface SearchRepoHit {
  repo?: RepoRef;
  fullName: string;
  description: string;
  topics: string[];
  primaryLanguage: string;
  starCount: U64;
  forkCount: U64;
  openIssueCount: U64;
  licenseSpdxId: string;
  archived: boolean;
  isFork: boolean;
  isTemplate: boolean;
  visibility: string;
  updatedAt?: Timestamp;
  pushedAt?: Timestamp;
}
export interface SearchUserHit {
  principal?: Principal;
  login: string;
  name: string;
  isOrg: boolean;
  bio: string;
  location: string;
  avatarUrl: string;
  followerCount: U64;
  publicRepoCount: U64;
}
export interface SearchReposRequest {
  query: string;
  /** "stars", "forks", "updated", "help-wanted-issues" or "best-match". */
  sort?: string;
  order?: SearchSortOrder;
  page?: PageRequest;
}
export interface SearchReposResponse {
  hits: SearchRepoHit[];
  totalCount: U64;
  incomplete: boolean;
  page?: PageResponse;
}
export interface TrendingRepo {
  repo?: SearchRepoHit;
  starsInWindow: U64;
  forksInWindow: U64;
  builtBy: Principal[];
  rank: number;
}
export interface ListTrendingReposRequest {
  window?: TrendingWindow;
  language?: string;
  page?: PageRequest;
}
export interface ListTrendingReposResponse {
  repos: TrendingRepo[];
  computedAt?: Timestamp;
  page?: PageResponse;
}
export interface TrendingDeveloper {
  user?: SearchUserHit;
  popularRepo?: SearchRepoHit;
  rank: number;
}
export interface ListTrendingDevelopersRequest {
  window?: TrendingWindow;
  language?: string;
  includeAgents?: boolean;
  page?: PageRequest;
}
export interface ListTrendingDevelopersResponse {
  developers: TrendingDeveloper[];
  computedAt?: Timestamp;
  page?: PageResponse;
}
export interface LeaderboardEntry {
  rank: number;
  /** Set for agent and human subjects. */
  user?: SearchUserHit;
  /** Set for repo subjects. */
  repo?: SearchRepoHit;
  score: U64;
}
export interface ListLeaderboardRequest {
  window?: TrendingWindow;
  subject: LeaderboardSubject;
  metric: LeaderboardMetric;
  page?: PageRequest;
}
export interface ListLeaderboardResponse {
  entries: LeaderboardEntry[];
  computedAt?: Timestamp;
  page?: PageResponse;
}
export interface Topic {
  name: string;
  displayName: string;
  shortDescription: string;
  description: string;
  featured: boolean;
  curated: boolean;
  aliases: string[];
  related: string[];
  logoUrl: string;
  repoCount: U64;
  topRepos: SearchRepoHit[];
}
export interface GetTopicRequest {
  name: string;
  topRepoCount?: number;
}
export interface ListFeaturedTopicsRequest {
  page?: PageRequest;
}
export interface ListFeaturedTopicsResponse {
  topics: Topic[];
  page?: PageResponse;
}
/** Recent lands on public repos: SearchRepos (`is:public sort:updated`), then each repo's newest changeset. */
export interface ListPublicActivityRequest {
  /** At most 20. */
  limit?: number;
}
export interface PublicLand {
  repo: SearchRepoHit;
  changeset: Changeset;
}
export interface ListPublicActivityResponse {
  items: PublicLand[];
}

// ---------------------------------------------------------------- users

export type ProfileKind = "user" | "agent" | "org";
export interface Profile {
  kind: ProfileKind;
  principalId: string;
  login: string;
  name: string;
  bio: string;
  avatar?: string;
  /** The profile README (`<login>/<login>`), as sanitized HTML from the server. */
  readme?: string;
  company: string;
  location: string;
  website: string;
  /** Orgs the user belongs to; empty for orgs. */
  orgs: string[];
  followers: number;
  following: number;
  followingByMe: boolean;
  publicRepos: number;
  /** Agents only: the runtime that drives them, e.g. "claude-code". */
  runtime?: string;
  /** Agents only: login of the human who owns them. */
  ownerLogin?: string;
  createdAt?: Timestamp;
}
export interface GetProfileRequest {
  owner: string;
}
export interface UserSummary {
  principal?: Principal;
  login: string;
  name: string;
  avatarUrl: string;
  bio: string;
}
export interface UserLoginRequest {
  login: string;
}
export interface FollowRepoRequest {
  repo: RepoRef;
}
export type FollowRepoResponse = Record<string, never>;
export type UnfollowRepoRequest = FollowRepoRequest;
export type UnfollowRepoResponse = Record<string, never>;
export type IsFollowingRepoRequest = FollowRepoRequest;
export interface IsFollowingRepoResponse {
  following: boolean;
}
export interface ListFollowedReposRequest {
  /** Login of the follower: a person or an agent. */
  user: string;
  page?: PageRequest;
}
export interface ListFollowedReposResponse {
  repos: Repo[];
  page?: PageResponse;
}
export interface ListRepoFollowersRequest {
  repo: RepoRef;
  page?: PageRequest;
}
export interface ListRepoFollowersResponse {
  followers: Principal[];
  page?: PageResponse;
}
export interface StarRepoRequest {
  repo: RepoRef;
}
export type StarRepoResponse = Record<string, never>;
export type UnstarRepoRequest = StarRepoRequest;
export type UnstarRepoResponse = Record<string, never>;
export type FollowUserRequest = UserLoginRequest;
export type FollowUserResponse = Record<string, never>;
export type UnfollowUserRequest = UserLoginRequest;
export type UnfollowUserResponse = Record<string, never>;
export interface ListFollowersRequest {
  login: string;
  page?: PageRequest;
}
export interface ListFollowersResponse {
  users: UserSummary[];
  page?: PageResponse;
}
export type ListFollowingRequest = ListFollowersRequest;
export type ListFollowingResponse = ListFollowersResponse;
export interface StarredRepo {
  repo?: RepoRef;
  fullName: string;
  description: string;
  language: string;
  starCount: U64;
  starredAt?: Timestamp;
}
export interface ListStarredRequest {
  login: string;
  language?: string;
  sort?: string;
  page?: PageRequest;
}
export interface ListStarredResponse {
  repos: StarredRepo[];
  page?: PageResponse;
}
export interface ContributionDay {
  /** YYYY-MM-DD. */
  date: string;
  count: number;
  /** 0 (none) to 4 (most), relative to the user's busiest day. */
  level: number;
}
export interface GetContributionGraphRequest {
  login: string;
  fromDate?: string;
  toDate?: string;
  org?: string;
}
export interface ContributionGraph {
  login: string;
  fromDate: string;
  toDate: string;
  total: number;
  days: ContributionDay[];
  includesPrivate: boolean;
}
export interface Achievement {
  slug: string;
  name: string;
  description: string;
  tier: string;
  count: number;
  imageUrl: string;
  unlockedAt?: Timestamp;
}
export interface ListAchievementsRequest {
  login: string;
}
export interface ListAchievementsResponse {
  achievements: Achievement[];
}
export interface FeedSubject {
  kind: SubjectKindJson;
  id: string;
  repo?: RepoRef;
}
export interface FeedEvent {
  id: string;
  actor?: Principal;
  kind: string;
  subject?: FeedSubject;
  summary: string;
  payload: string;
  createdAt?: Timestamp;
}
/** UserService.ListUserFeedEvents; `kinds` is the filter, empty for everything. */
export interface ListHomeFeedRequest {
  kinds?: string[];
  page?: PageRequest;
}
export interface ListHomeFeedResponse {
  events: FeedEvent[];
  page?: PageResponse;
}

// ---------------------------------------------------------------- server info

export interface ServerInfoRequest {
  repo?: RepoRef;
}
export interface ServerInfo {
  version: string;
  tier: string;
  algorithms: string[];
  maxBlobBytes: U64;
  protocolVersions: string[];
  policyEpoch: string;
  redactionEpoch: string;
  githubClientId: string;
  cell: string;
  region: string;
  features: string[];
  connectBaseUrl: string;
  /** "cloud" or "enterprise"; empty when the server predates the field. */
  edition: string;
}

// ---------------------------------------------------------------- cloud: orgs, repos, billing

export interface CreateOrgRequest {
  name: string;
  display?: string;
}
export interface CreateRepoRequest {
  org: string;
  name: string;
  visibility?: Visibility;
  description?: string;
  topics?: string[];
  homepage?: string;
  autoInit?: boolean;
  licenseTemplate?: string;
}
export type BillingProduct = BillingProductJson;
export type BillingCycle = BillingCycleJson;
export type BillingUsageGrouping = BillingUsageGroupingJson;
export type HealthStatus = HealthStatusJson;
export interface EnterpriseScope {
  org?: string;
  enterprise?: string;
}
export interface BillingIncludedUsage {
  product: BillingProduct;
  quantity: number;
  unit: string;
}
/** A plan on offer. Prices are micro-USD per seat; `selfServe` is false for plans sold by contact. */
export interface BillingPlan {
  id: string;
  name: string;
  description: string;
  seatMonthlyPriceMicroUsd: U64;
  seatYearlyPriceMicroUsd: U64;
  included: BillingIncludedUsage[];
  features: string[];
  selfServe: boolean;
}
export interface ListBillingPlansRequest {
  scope?: EnterpriseScope;
}
export interface ListBillingPlansResponse {
  plans: BillingPlan[];
}
export interface AccountPlan {
  scope?: EnterpriseScope;
  plan?: BillingPlan;
  cycle: BillingCycle;
  seatsPurchased: number;
  seatsConsumed: number;
  cycleStartedAt?: Timestamp;
  nextBillingAt?: Timestamp;
  /** A downgrade that takes effect at the end of the cycle. */
  pendingPlanId: string;
  pendingSeats: number;
  billingDisabled: boolean;
}
export interface GetAccountPlanRequest {
  org: string;
}
export interface Seat {
  principal?: Principal;
  org: string;
  outsideCollaborator: boolean;
  costCenterId: string;
  consumedSince?: Timestamp;
  lastActivityAt?: Timestamp;
}
export interface ListSeatsRequest {
  org: string;
  page?: PageRequest;
}
export interface ListSeatsResponse {
  seats: Seat[];
  totalConsumed: number;
  page?: PageResponse;
}
export interface UsageLine {
  date: string;
  product: BillingProduct;
  sku: string;
  repo?: RepoRef;
  org: string;
  costCenterId: string;
  principalId: string;
  quantity: number;
  unit: string;
  unitPriceMicroUsd: U64;
  grossMicroUsd: U64;
  discountMicroUsd: U64;
  netMicroUsd: U64;
}
export interface QueryUsageRequest {
  org: string;
  startAt?: Timestamp;
  endAt?: Timestamp;
  groupBy?: BillingUsageGrouping[];
  products?: BillingProduct[];
  page?: PageRequest;
}
export interface QueryUsageResponse {
  lineItems: UsageLine[];
  totalGrossMicroUsd: U64;
  totalNetMicroUsd: U64;
  dataCompleteUntil?: Timestamp;
  page?: PageResponse;
}
export interface ChangePlanRequest {
  org: string;
  planId: string;
  seats?: number;
  cycle?: BillingCycle;
}

// ---------------------------------------------------------------- admin: ops

/** One component of the server's health (a store, the event log, the search index …). */
export interface OpsCheck {
  name: string;
  liveness: HealthStatus;
  readiness: HealthStatus;
  reason: string;
  detail: string;
  criticalPath: boolean;
  checkedAt?: Timestamp;
}
export interface GetOpsHealthRequest {
  cell?: string;
  includeComponents?: boolean;
}
export interface OpsHealth {
  liveness: HealthStatus;
  readiness: HealthStatus;
  components: OpsCheck[];
  node: string;
  cell: string;
  version: string;
}
export interface OpsCell {
  name: string;
  region: string;
  version: string;
  configVersion: string;
  schemaVersion: string;
  status: HealthStatus;
  metadataPrimaryUp: boolean;
  servingStaleReads: boolean;
  staleLagMs: U64;
  nodes: number;
  orgs: number;
  redactionEpoch: U64;
  policyEpoch: string;
}
export interface GetClusterStatusRequest {
  cells?: string[];
}
export interface ClusterStatus {
  cells: OpsCell[];
  directoryUp: boolean;
  directoryVersion: string;
  observedAt?: Timestamp;
}
export interface GetEffectiveOpsConfigRequest {
  cell?: string;
  node?: string;
  component?: string;
}
/** The config in force, secrets replaced by "<redacted>"; `appliedAt` is when the process started. */
export interface EffectiveOpsConfig {
  toml: string;
  configVersion: string;
  appliedAt?: Timestamp;
}
export interface OpsConfigError {
  keyPath: string;
  line: number;
  column: number;
  message: string;
  code: string;
}
export interface ValidateOpsConfigRequest {
  toml: string;
  component?: string;
}
export interface ValidateOpsConfigResponse {
  valid: boolean;
  errors: OpsConfigError[];
  warnings: OpsConfigError[];
  normalizedToml: string;
}

// ---------------------------------------------------------------- admin: enterprise

export interface Enterprise {
  id: string;
  slug: string;
  name: string;
  description: string;
  websiteUrl: string;
  managedUsers: boolean;
  managedUsersShortcode: string;
  orgCount: number;
  memberCount: number;
  createdAt?: Timestamp;
  updatedAt?: Timestamp;
}
export interface GetEnterpriseRequest {
  enterprise: string;
}
export interface UpdateEnterpriseRequest {
  enterprise: string;
  name?: string;
  description?: string;
  websiteUrl?: string;
  managedUsers?: boolean;
  managedUsersShortcode?: string;
}
export interface EnterpriseOrg {
  enterpriseId: string;
  org: string;
  orgId: string;
  memberCount: number;
  repoCount: number;
  joinedAt?: Timestamp;
}
export interface ListEnterpriseOrgsRequest {
  enterprise: string;
  query?: string;
  page?: PageRequest;
}
export interface ListEnterpriseOrgsResponse {
  orgs: EnterpriseOrg[];
  page?: PageResponse;
}
export interface EnterpriseOwner {
  principal?: Principal;
  addedAt?: Timestamp;
}
export interface ListEnterpriseOwnersRequest {
  enterprise: string;
  page?: PageRequest;
}
export interface ListEnterpriseOwnersResponse {
  owners: EnterpriseOwner[];
  page?: PageResponse;
}
export interface SamlConfig {
  scope?: EnterpriseScope;
  enabled: boolean;
  enforced: boolean;
  idpMetadataUrl: string;
  idpMetadataXml: string;
  idpEntityId: string;
  idpSsoUrl: string;
  idpCertificatePem: string;
  idpCertificateExpiresAt?: Timestamp;
  spEntityId: string;
  spAcsUrl: string;
  signatureMethod: string;
  digestMethod: string;
  sessionLifetimeSeconds: number;
  lastTestedAt?: Timestamp;
  updatedAt?: Timestamp;
}
export interface GetSamlConfigRequest {
  scope: EnterpriseScope;
}
export interface UpdateSamlConfigRequest {
  scope: EnterpriseScope;
  enabled?: boolean;
  enforced?: boolean;
  idpMetadataUrl?: string;
  idpMetadataXml?: string;
  idpEntityId?: string;
  idpSsoUrl?: string;
  idpCertificatePem?: string;
  signatureMethod?: string;
  digestMethod?: string;
  sessionLifetimeSeconds?: number;
}
export interface TestSamlConfigRequest {
  scope: EnterpriseScope;
  idpMetadataUrl?: string;
  idpMetadataXml?: string;
}
export interface TestSamlConfigResponse {
  metadataOk: boolean;
  problems: string[];
  testSignInUrl: string;
}
export interface ScimConfig {
  scope?: EnterpriseScope;
  enabled: boolean;
  endpointUrl: string;
  provisionsTeams: boolean;
  activeTokenCount: number;
  provisionedCount: U64;
  lastRequestAt?: Timestamp;
}
export interface GetScimConfigRequest {
  scope: EnterpriseScope;
}
export interface UpdateScimConfigRequest {
  scope: EnterpriseScope;
  enabled?: boolean;
  provisionsTeams?: boolean;
}
export interface CreateScimTokenRequest {
  scope: EnterpriseScope;
  description?: string;
  ttlSeconds?: number;
}
/** The token is shown once; the server keeps only its id. */
export interface CreateScimTokenResponse {
  token: string;
  tokenId: string;
  expiresAt?: Timestamp;
}
export type VerifiedDomainState = VerifiedDomainStateJson;
export interface VerifiedDomain {
  id: string;
  scope?: EnterpriseScope;
  domain: string;
  state: VerifiedDomainState;
  dnsTxtRecordName: string;
  dnsTxtRecordValue: string;
  challengeExpiresAt?: Timestamp;
  lastCheckedAt?: Timestamp;
  verifiedAt?: Timestamp;
  createdAt?: Timestamp;
}
export interface ListVerifiedDomainsRequest {
  scope: EnterpriseScope;
  page?: PageRequest;
}
export interface ListVerifiedDomainsResponse {
  domains: VerifiedDomain[];
  page?: PageResponse;
}
export interface AddVerifiedDomainRequest {
  scope: EnterpriseScope;
  domain: string;
  approvedOnly?: boolean;
}
export interface CheckVerifiedDomainRequest {
  scope: EnterpriseScope;
  domainId: string;
}
export interface DeleteVerifiedDomainRequest {
  scope: EnterpriseScope;
  domainId: string;
}
export type DeleteVerifiedDomainResponse = Record<string, never>;
export interface TwoFactorRequirement {
  scope?: EnterpriseScope;
  required: boolean;
  secureMethodsOnly: boolean;
  noncompliantCount: number;
  requiredSince?: Timestamp;
}
export interface GetTwoFactorRequirementRequest {
  scope: EnterpriseScope;
}
export interface UpdateTwoFactorRequirementRequest {
  scope: EnterpriseScope;
  required?: boolean;
  secureMethodsOnly?: boolean;
  removeNoncompliant?: boolean;
}
export interface UpdateTwoFactorRequirementResponse {
  requirement?: TwoFactorRequirement;
  removedPrincipalIds: string[];
}
export interface IpAllowEntry {
  id: string;
  scope?: EnterpriseScope;
  cidr: string;
  description: string;
  active: boolean;
  appId: string;
  createdAt?: Timestamp;
  updatedAt?: Timestamp;
}
export interface ListIpAllowListRequest {
  scope: EnterpriseScope;
  page?: PageRequest;
}
export interface ListIpAllowListResponse {
  entries: IpAllowEntry[];
  page?: PageResponse;
}
export interface CreateIpAllowListEntryRequest {
  scope: EnterpriseScope;
  cidr: string;
  description?: string;
  active?: boolean;
}
export interface DeleteIpAllowListEntryRequest {
  scope: EnterpriseScope;
  entryId: string;
}
export type DeleteIpAllowListEntryResponse = Record<string, never>;
export type EnterpriseRepoCreationPolicy = EnterpriseRepoCreationPolicyJson;
export type EnterpriseForkingPolicy = EnterpriseForkingPolicyJson;
export type EnterpriseBasePermission = EnterpriseBasePermissionJson;
export type EnterprisePolicyToggle = EnterprisePolicyToggleJson;
export type EnterpriseActionsEnablement = EnterpriseActionsEnablementJson;
export type EnterpriseAllowedActions = EnterpriseAllowedActionsJson;
export interface EnterpriseActionsPolicy {
  enablement: EnterpriseActionsEnablement;
  enabledOrgs: string[];
  allowedActions: EnterpriseAllowedActions;
  allowedActionPatterns: string[];
  allowVerifiedCreators: boolean;
  workflowPolicyId: string;
}
export interface EnterprisePolicies {
  enterpriseId: string;
  repoCreation: EnterpriseRepoCreationPolicy;
  forking: EnterpriseForkingPolicy;
  basePermission: EnterpriseBasePermission;
  membersCanDeleteRepos: EnterprisePolicyToggle;
  membersCanChangeRepoVisibility: EnterprisePolicyToggle;
  membersCanInviteOutsideCollaborators: EnterprisePolicyToggle;
  membersCanCreateTeams: EnterprisePolicyToggle;
  membersCanInstallApps: EnterprisePolicyToggle;
  agentsCanLandWithoutHumanApproval: EnterprisePolicyToggle;
  actions?: EnterpriseActionsPolicy;
  orgRulesetIds: string[];
  updatedAt?: Timestamp;
}
export interface GetEnterprisePoliciesRequest {
  enterprise: string;
}
/** A field left out (or UNSPECIFIED) is left as it is. */
export interface UpdateEnterprisePoliciesRequest {
  enterprise: string;
  repoCreation?: EnterpriseRepoCreationPolicy;
  forking?: EnterpriseForkingPolicy;
  basePermission?: EnterpriseBasePermission;
  membersCanDeleteRepos?: EnterprisePolicyToggle;
  membersCanChangeRepoVisibility?: EnterprisePolicyToggle;
  membersCanInviteOutsideCollaborators?: EnterprisePolicyToggle;
  membersCanCreateTeams?: EnterprisePolicyToggle;
  membersCanInstallApps?: EnterprisePolicyToggle;
  agentsCanLandWithoutHumanApproval?: EnterprisePolicyToggle;
  actions?: Partial<EnterpriseActionsPolicy>;
}

// ---------------------------------------------------------------- insights

export type GetLanguagesRequest = GetLanguageStatsRequestJson;
export type GetLanguagesResponse = GetLanguageStatsResponseJson;
