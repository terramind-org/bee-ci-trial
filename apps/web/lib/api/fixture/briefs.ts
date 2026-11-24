/**
 * Fixture handlers for briefs and evidence (SPEC §11.9): the BriefService, a change request's
 * brief, its conversation comments and the agent sessions behind it.
 *
 * The rules are the server's: who may write, the limits, per-kind asset rules, byte sniffing
 * (never SVG, HTML or script), staleness on read, the advisory checklist and signed asset URLs
 * that are decided again when redeemed. State lives in `BriefsWorld` on the store. Time is the
 * store's clock and ids derive from content, so a run of the fixture is reproducible. Bytes come
 * from generated frames (`lib/media/frames.ts`), the committed post media, or an upload.
 */
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import type { AgentSessionJson } from "../../gen/bee/v1/agents_pb";
import { BriefService } from "../../gen/bee/v1/briefs_pb";
import type { CommentJson } from "../../gen/bee/v1/content_pb";
import { frame, FRAME_HEIGHT, FRAME_WIDTH, WALKTHROUGH_VTT, type FrameName } from "../../media/frames";
import { objectIdText } from "../ids";
import { BeeError } from "../transport";
import { DIFF_CLASSES, type DiffClass } from "../types";
import type * as T from "../types";
import { defineService, type Ctx } from "./router";
import { MEDIA_ALT, MEDIA_FILES } from "./posts-world";
import { ulid } from "./seed";
import type { CrState, FixtureStore, RepoState } from "./store";
import { invalid, page } from "./util";

const MIN = 60_000;
const MiB = 1024 * 1024;
const MAX_TEXT = 280;
const MAX_LISTED = 20;
const MAX_ITEMS = 64;
const MAX_TOTAL = 512 * MiB;
const MAX_INLINE = 8 * MiB;
const MAX_PATHS = 20;
const MAX_IMAGE_SET = 12;
const MAX_SERIES = 20;
const MAX_FAILURES = 50;
const UPLOAD_TTL = 15 * MIN;
const PENDING_TTL = 60 * MIN;
const URL_TTL = 15 * MIN;

type Kind = T.EvidenceKind;
type Role = T.EvidenceAssetRole;
const UPLOADED: T.EvidenceSource = "EVIDENCE_SOURCE_UPLOADED";
const CHECK_RUN: T.EvidenceSource = "EVIDENCE_SOURCE_CHECK_RUN";
const PENDING: T.EvidenceState = "EVIDENCE_STATE_PENDING";
const READY: T.EvidenceState = "EVIDENCE_STATE_READY";

const MAIN: Role = "EVIDENCE_ASSET_ROLE_MAIN";
const BEFORE: Role = "EVIDENCE_ASSET_ROLE_BEFORE";
const AFTER: Role = "EVIDENCE_ASSET_ROLE_AFTER";
const POSTER: Role = "EVIDENCE_ASSET_ROLE_POSTER";
const CAPTIONS: Role = "EVIDENCE_ASSET_ROLE_CAPTIONS";

/** The part of a URL path that names a role: `main`, `before`, `after`, `poster` or `captions`. */
const roleName = (r: Role): string => r.replace("EVIDENCE_ASSET_ROLE_", "").toLowerCase();
const roleOf = (name: string): Role | undefined => (["main", "before", "after", "poster", "captions"].includes(name) ? (("EVIDENCE_ASSET_ROLE_" + name.toUpperCase()) as Role) : undefined);

const lengthOf = (x: string) => Array.from(x).length;
const sha256 = (b: Uint8Array) => createHash("sha256").update(b).digest("hex");

const notFound = (id: string) => new BeeError("not_found", { reason: "EVIDENCE_NOT_FOUND", message: "no evidence " + id });
const denied = (message: string) => new BeeError("permission_denied", { reason: "PERMISSION_DENIED", message });
const tooLarge = (message: string) => new BeeError("resource_exhausted", { reason: "TOO_LARGE", message, subcode: "limit_exceeded" } as never);

// ------------------------------------------------------------------ bytes

type MediaClass = "image" | "video" | "audio" | "pdf" | "text";

interface Sniffed {
  contentType: string;
  class: MediaClass;
}

/** What the bytes are, by their magic numbers. SVG, HTML and script are never accepted. */
export function sniffEvidence(b: Uint8Array): Sniffed | undefined {
  const at = (i: number, ...v: number[]) => v.every((x, k) => b[i + k] === x);
  if (at(0, 0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)) return { contentType: "image/png", class: "image" };
  if (at(0, 0xff, 0xd8, 0xff)) return { contentType: "image/jpeg", class: "image" };
  if (at(0, 0x47, 0x49, 0x46, 0x38)) return { contentType: "image/gif", class: "image" };
  if (at(0, 0x52, 0x49, 0x46, 0x46) && at(8, 0x57, 0x45, 0x42, 0x50)) return { contentType: "image/webp", class: "image" };
  if (at(4, 0x66, 0x74, 0x79, 0x70)) return { contentType: "video/mp4", class: "video" };
  if (at(0, 0x1a, 0x45, 0xdf, 0xa3)) return { contentType: "video/webm", class: "video" };
  if (at(0, 0x4f, 0x67, 0x67, 0x53)) return { contentType: "audio/ogg", class: "audio" };
  if (at(0, 0x52, 0x49, 0x46, 0x46) && at(8, 0x57, 0x41, 0x56, 0x45)) return { contentType: "audio/wav", class: "audio" };
  if (at(0, 0x49, 0x44, 0x33) || (b[0] === 0xff && ((b[1] ?? 0) & 0xe0) === 0xe0)) return { contentType: "audio/mpeg", class: "audio" };
  if (at(0, 0x25, 0x50, 0x44, 0x46)) return { contentType: "application/pdf", class: "pdf" };
  if (b.length === 0 || b.includes(0)) return undefined;
  let text: string;
  try {
    text = new TextDecoder("utf-8", { fatal: true }).decode(b);
  } catch {
    return undefined;
  }
  const head = text.trimStart().slice(0, 64).toLowerCase();
  if (head.startsWith("<") || /<script[\s>]/i.test(text)) return undefined;
  return { contentType: text.startsWith("WEBVTT") ? "text/vtt" : "text/plain", class: "text" };
}

/** The sizes a PNG states in its header. */
function pngSize(b: Uint8Array): { width: number; height: number } | undefined {
  if (b.length < 24 || !sniffEvidence(b) || sniffEvidence(b)?.contentType !== "image/png") return undefined;
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  return { width: v.getUint32(16), height: v.getUint32(20) };
}

type Source = { gen: FrameName } | { media: string } | { text: string } | { bytes: Uint8Array };

function readSource(src: Source): Uint8Array | undefined {
  if ("gen" in src) return frame(src.gen);
  if ("text" in src) return new TextEncoder().encode(src.text);
  if ("bytes" in src) return src.bytes;
  const file = MEDIA_FILES[src.media];
  if (!file) return undefined;
  try {
    return new Uint8Array(readFileSync(join(process.cwd(), "public", "fixture", "posts", file.file)));
  } catch {
    return undefined;
  }
}

// ------------------------------------------------------------------ rows

interface AssetRow {
  role: Role;
  contentType: string;
  width: number;
  height: number;
  durationMs: number;
  alt: string;
  sizeBytes: number;
  /** Lowercase hex; empty while the upload is pending. */
  sha256: string;
  /** Where the bytes come from; absent until an upload arrives. */
  source?: Source;
}

export interface EvidenceRow {
  id: string;
  /** Store key of the repo. */
  repo: string;
  crId: string;
  revision: number;
  spec: T.EvidenceSpec;
  assets: AssetRow[];
  source: T.EvidenceSource;
  checkRunId: string;
  /** Handle of the uploader. */
  by: string;
  state: T.EvidenceState;
  createdAt: number;
  updatedAt: number;
}

const ALLOWED: Record<string, Role[]> = {
  EVIDENCE_KIND_SCREENSHOT: [MAIN],
  EVIDENCE_KIND_BEFORE_AFTER: [BEFORE, AFTER],
  EVIDENCE_KIND_VIDEO: [MAIN, POSTER, CAPTIONS],
  EVIDENCE_KIND_BENCHMARK: [],
  EVIDENCE_KIND_IMAGE: [MAIN],
  EVIDENCE_KIND_AUDIO: [MAIN, CAPTIONS],
  EVIDENCE_KIND_DOCUMENT: [MAIN],
  EVIDENCE_KIND_PREVIEW_LINK: [],
  EVIDENCE_KIND_TEST_REPORT: [MAIN],
};

const LIMIT: Record<MediaClass, number> = { image: 10 * MiB, video: 100 * MiB, audio: 25 * MiB, pdf: 20 * MiB, text: 20 * MiB };

/** The media class an asset of this role in this kind must have. */
function expectedClass(kind: Kind, role: Role): MediaClass[] {
  if (role === CAPTIONS) return ["text"];
  if (role === POSTER || role === BEFORE || role === AFTER) return ["image"];
  if (kind === "EVIDENCE_KIND_VIDEO") return ["video"];
  if (kind === "EVIDENCE_KIND_AUDIO") return ["audio"];
  if (kind === "EVIDENCE_KIND_DOCUMENT" || kind === "EVIDENCE_KIND_TEST_REPORT") return ["pdf", "text"];
  return ["image"];
}

const UI_FILE = /\.(tsx|jsx|vue|svelte|astro|html|css|scss|less)$/;
const rank = (c: DiffClass): number => DIFF_CLASSES.indexOf(c);
const SHOWS_UI: Kind[] = ["EVIDENCE_KIND_SCREENSHOT", "EVIDENCE_KIND_BEFORE_AFTER", "EVIDENCE_KIND_VIDEO"];

export class BriefsWorld {
  items: EvidenceRow[] = [];
  /** Briefs by change request id. */
  briefs = new Map<string, T.ChangeBrief>();
  /** How many times each change request's brief was set; part of its `version`. */
  briefSets = new Map<string, number>();
  /** Conversation comments by change request id, oldest first. */
  comments = new Map<string, T.Comment[]>();

  // ------------------------------------------------------------------ lookups

  byId(id: string | undefined): EvidenceRow | undefined {
    return this.items.find((e) => e.id === id);
  }

  forChange(crId: string): EvidenceRow[] {
    return this.items.filter((e) => e.crId === crId);
  }

  briefFor(crId: string): T.ChangeBrief | undefined {
    return this.briefs.get(crId);
  }

  /** The change request's version for `expected_version`: it moves on a revision or a new brief. */
  version(c: CrState): number {
    return c.revisions.length + (this.briefSets.get(c.id) ?? 0);
  }

  /** Whether the caller may edit the change request and so write its brief and evidence. */
  canEdit(s: FixtureStore, who: string | undefined, repo: RepoState, c: CrState): boolean {
    return s.posts.canEditChange(s, who, repo, c.author);
  }

  // ------------------------------------------------------------------ signed URLs

  private signature(path: string, exp: number): string {
    return createHash("sha256").update("evidence-url:" + path + ":" + exp).digest("hex").slice(0, 24);
  }

  /** The signed download URL of an asset, valid for 15 minutes from the store's clock. */
  signedUrl(s: FixtureStore, e: EvidenceRow, role: Role): { url: string; expiresAt: number } {
    const path = "/_evidence/fixture/" + e.id + "/" + roleName(role);
    const exp = s.now() + URL_TTL;
    return { url: path + "?exp=" + exp + "&sig=" + this.signature(path, exp), expiresAt: exp };
  }

  /**
   * Redeems a signed URL the way the server does: the signature must match, it must not have
   * expired, and the caller must still be able to read the repo. Returns the asset's bytes.
   */
  redeem(s: FixtureStore, segments: string[], query: URLSearchParams, viewer: string | undefined): { contentType: string; bytes: Uint8Array; digest: string; media: boolean } {
    const [ns, id, name] = segments;
    const e = ns === "fixture" && segments.length === 3 ? this.byId(id) : undefined;
    const role = roleOf(name ?? "");
    const exp = Number(query.get("exp"));
    const path = "/_evidence/" + segments.join("/");
    if (!e || !role || !exp || query.get("sig") !== this.signature(path, exp)) throw new BeeError("permission_denied", { reason: "BAD_SIGNATURE", message: "the link is not valid" });
    if (exp < s.now()) throw new BeeError("permission_denied", { reason: "URL_EXPIRED", message: "the link has expired" });
    const repo = s.repos.get(e.repo);
    const cr = s.crs.find((x) => x.id === e.crId);
    // A pending item is for the people who may finish it.
    if (!repo || !cr || !s.canRead(viewer, repo) || (e.state === PENDING && !this.canEdit(s, viewer, repo, cr))) throw new BeeError("not_found", { reason: "EVIDENCE_NOT_FOUND", message: "no evidence " + e.id });
    const a = e.assets.find((x) => x.role === role);
    const bytes = a?.source ? readSource(a.source) : undefined;
    if (!a || !bytes) throw notFound(e.id);
    return { contentType: a.contentType, bytes, digest: "sha256:" + a.sha256, media: a.contentType.startsWith("image/") || a.contentType.startsWith("video/") || a.contentType.startsWith("audio/") };
  }

  // ------------------------------------------------------------------ views

  assetView(s: FixtureStore, e: EvidenceRow, a: AssetRow): T.EvidenceAsset {
    const signed = e.state === READY ? this.signedUrl(s, e, a.role) : undefined;
    return {
      role: a.role,
      contentType: a.contentType,
      digest: a.sha256 ? "sha256:" + a.sha256 : "",
      sizeBytes: String(a.sizeBytes),
      width: a.width,
      height: a.height,
      durationMs: String(a.durationMs),
      alt: a.alt,
      url: signed?.url ?? "",
      ...(signed ? { urlExpiresAt: s.iso(signed.expiresAt) } : {}),
    };
  }

  view(s: FixtureStore, e: EvidenceRow): T.Evidence {
    const c = s.crs.find((x) => x.id === e.crId) as CrState;
    return {
      id: e.id,
      repoId: (s.repos.get(e.repo) as RepoState).repo.id,
      changeRequestId: e.crId,
      revision: e.revision,
      head: c.revisions.find((r) => r.n === e.revision)?.head.id,
      spec: e.spec,
      assets: e.assets.map((a) => this.assetView(s, e, a)),
      source: e.source,
      checkRunId: e.checkRunId,
      workflowArtifactId: "",
      uploader: s.p(e.by),
      state: e.state,
      stale: e.revision < s.current(c).n,
      createdAt: s.iso(e.createdAt),
      updatedAt: s.iso(e.updatedAt),
    };
  }

  /** The Evidence an event carries as `payload_json`: no signed URLs, those are never stored. */
  payload(s: FixtureStore, e: EvidenceRow): T.Evidence {
    const v = this.view(s, e);
    return { ...v, assets: v.assets.map((a) => ({ ...a, url: "", urlExpiresAt: undefined })) };
  }

  // ------------------------------------------------------------------ the brief

  /** Validates a brief and returns it as stored, with the server-set fields filled in. */
  normalizeBrief(s: FixtureStore, input: T.ChangeBrief | undefined, c: CrState, actor: string): T.ChangeBrief {
    const b = input ?? ({} as Partial<T.ChangeBrief>);
    const text = (x: string | undefined, what: string): string => {
      const t = (x ?? "").normalize("NFC").trim();
      if (lengthOf(t) > MAX_TEXT) throw invalid(what + " is at most " + MAX_TEXT + " characters", "brief_too_long");
      return t;
    };
    const paths = (p: string[] | undefined): string[] => {
      if ((p?.length ?? 0) > MAX_PATHS) throw invalid("at most " + MAX_PATHS + " paths per highlight", "bad_brief");
      return [...(p ?? [])];
    };
    const highlights = b.highlights ?? [];
    const asks = b.asks ?? [];
    const risks = b.risks ?? [];
    if (highlights.length > MAX_LISTED || asks.length > MAX_LISTED || risks.length > MAX_LISTED) throw invalid("a brief has at most " + MAX_LISTED + " highlights, asks and risks each", "bad_brief");
    const tryIt = (b.tryItUrl ?? "").trim();
    if (tryIt) {
      let ok = false;
      try {
        ok = new URL(tryIt).protocol === "https:";
      } catch {
        ok = false;
      }
      if (!ok) throw invalid("the try-it link must be an https URL", "bad_url");
    }
    return {
      outcome: text(b.outcome, "the outcome"),
      highlights: highlights.map((h) => ({ text: text(h.text, "a highlight"), paths: paths(h.paths) })),
      asks: asks.map((a) => {
        const options = (a.options ?? []).map((o) => o.trim());
        if (options.length < 2 || options.length > 6 || options.some((o) => !o || lengthOf(o) > 80)) throw invalid("an ask has 2 to 6 options of at most 80 characters", "bad_brief");
        return { question: text(a.question, "a question"), options };
      }),
      risks: risks.map((r) => ({ text: text(r.text, "a risk"), mitigation: text(r.mitigation, "a mitigation") })),
      rollback: text(b.rollback, "the rollback"),
      tryItUrl: tryIt,
      writtenForRevision: s.current(c).n,
      author: s.p(actor),
      updatedAt: s.iso(s.now()),
    };
  }

  /** Stores the brief. Opening a change request with a brief does not announce it separately: `cr.opened` carries it. */
  setBrief(s: FixtureStore, repo: RepoState, c: CrState, actor: string, input: T.ChangeBrief | undefined, announce = true): T.ChangeBrief {
    const brief = this.normalizeBrief(s, input, c, actor);
    this.briefs.set(c.id, brief);
    this.briefSets.set(c.id, (this.briefSets.get(c.id) ?? 0) + 1);
    if (announce) {
      s.emit(repo, "cr.brief_set", actor, { change: s.crView(c, actor) });
      s.appendAudit(actor, "brief.set", c.title, repo.name, true, "");
    }
    return brief;
  }

  // ------------------------------------------------------------------ the checklist

  /** What the brief and evidence of one revision still owe (SPEC §11.9). A pure function of the stored state. */
  checklist(s: FixtureStore, c: CrState, revision: number): T.BriefChecklist {
    const rev = c.revisions.find((r) => r.n === (revision || s.current(c).n));
    if (!rev) throw new BeeError("not_found", { reason: "UNKNOWN_REVISION", message: "no revision r" + revision });
    const entries = Object.entries(rev.perPath).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
    const pathsWhere = (f: (path: string, cls: DiffClass) => boolean) => entries.filter(([p, k]) => f(p, k)).map(([p]) => p).slice(0, MAX_PATHS);
    const brief = this.briefs.get(c.id);
    const here = this.forChange(c.id).filter((e) => e.state === READY && e.revision === rev.n);
    const items: T.BriefChecklistItem[] = [];

    const logic = pathsWhere((_, k) => rank(k) >= rank("logic"));
    if (s.kindOf(c.author) !== "human" && logic.length) {
      const done = !!brief?.outcome && brief.highlights.length > 0;
      items.push({ id: "brief", detail: "an outcome and at least one highlight", kinds: [], paths: logic, satisfied: done, evidenceIds: [] });
    }
    const sensitive = pathsWhere((_, k) => k === "sensitive");
    if (sensitive.length) {
      const done = !!brief && (brief.risks.length > 0 || !!brief.rollback);
      items.push({ id: "brief:risk", detail: "a risk or a rollback plan", kinds: [], paths: sensitive, satisfied: done, evidenceIds: [] });
    }
    const ui = pathsWhere((p, k) => UI_FILE.test(p) && rank(k) >= rank("logic"));
    if (ui.length) {
      const shows = here.filter((e) => SHOWS_UI.includes(e.spec.kind));
      items.push({ id: "evidence:ui", detail: "a screenshot, before and after, or video of the UI change", kinds: [...SHOWS_UI], paths: ui, satisfied: shows.length > 0, evidenceIds: shows.map((e) => e.id) });
    }
    return { revision: rev.n, complete: items.every((i) => i.satisfied), items };
  }

  // ------------------------------------------------------------------ evidence rules

  /** Validates a spec for its kind and returns it normalised. `kind` must already be known. */
  normalizeSpec(spec: T.EvidenceSpec | undefined): T.EvidenceSpec {
    const sp = spec ?? ({} as Partial<T.EvidenceSpec>);
    const kind = sp.kind ?? "EVIDENCE_KIND_UNSPECIFIED";
    if (!(kind in ALLOWED)) throw invalid("name a kind of evidence", "bad_evidence");
    const title = (sp.title ?? "").normalize("NFC").trim();
    if (!title || lengthOf(title) > 120) throw invalid("a title of 1 to 120 characters", "bad_evidence");
    const caption = (sp.caption ?? "").normalize("NFC");
    if (lengthOf(caption) > 1000) throw invalid("a caption is at most 1000 characters", "bad_evidence");
    const paths = sp.paths ?? [];
    if (paths.length > MAX_PATHS) throw invalid("at most " + MAX_PATHS + " paths", "bad_evidence");
    const benchmark = sp.benchmark ?? [];
    if (kind === "EVIDENCE_KIND_BENCHMARK" ? benchmark.length < 1 || benchmark.length > MAX_SERIES : benchmark.length > 0) throw invalid("a benchmark has 1 to " + MAX_SERIES + " series, and only benchmarks have any", "bad_evidence");
    if (benchmark.some((x) => !x.name || !Number.isFinite(x.baseline) || !Number.isFinite(x.value))) throw invalid("every series needs a name and numbers", "bad_evidence");
    if (kind === "EVIDENCE_KIND_TEST_REPORT" ? !sp.testReport : !!sp.testReport) throw invalid("only a test report carries test results, and it must", "bad_evidence");
    if (sp.testReport && sp.testReport.failures.length > MAX_FAILURES) throw invalid("at most " + MAX_FAILURES + " failures", "bad_evidence");
    const url = (sp.url ?? "").trim();
    if (kind === "EVIDENCE_KIND_PREVIEW_LINK") {
      let ok = false;
      try {
        ok = new URL(url).protocol === "https:";
      } catch {
        ok = false;
      }
      if (!ok) throw invalid("a preview link is an https URL", "bad_url");
    } else if (url) {
      throw invalid("only a preview link has a URL", "bad_evidence");
    }
    return {
      kind,
      title,
      caption,
      benchmark: benchmark.map((x) => ({ name: x.name, unit: x.unit ?? "", baseline: x.baseline, value: x.value, lowerIsBetter: !!x.lowerIsBetter })),
      ...(sp.testReport ? { testReport: { passed: sp.testReport.passed ?? 0, failed: sp.testReport.failed ?? 0, skipped: sp.testReport.skipped ?? 0, durationMs: String(sp.testReport.durationMs ?? "0"), failures: sp.testReport.failures ?? [], summary: sp.testReport.summary ?? "" } } : {}),
      url,
      paths: [...paths],
      hero: !!sp.hero,
      order: sp.order ?? 0,
    };
  }

  /** Checks the roles an item's assets play against what its kind allows. */
  checkRoles(kind: Kind, roles: Role[]): void {
    const allowed = ALLOWED[kind] ?? [];
    const has = (r: Role) => roles.filter((x) => x === r).length;
    if (roles.some((r) => !allowed.includes(r))) throw invalid("a " + kind.replace("EVIDENCE_KIND_", "").toLowerCase() + " item does not take those files", "bad_evidence");
    const need = (rs: Role[]) => rs.every((r) => has(r) === 1);
    const ok =
      kind === "EVIDENCE_KIND_SCREENSHOT"
        ? need([MAIN])
        : kind === "EVIDENCE_KIND_BEFORE_AFTER"
          ? need([BEFORE, AFTER])
          : kind === "EVIDENCE_KIND_VIDEO" || kind === "EVIDENCE_KIND_AUDIO"
            ? need([MAIN]) && has(CAPTIONS) <= 1 && has(POSTER) <= 1
            : kind === "EVIDENCE_KIND_IMAGE"
              ? has(MAIN) >= 1 && has(MAIN) <= MAX_IMAGE_SET
              : kind === "EVIDENCE_KIND_DOCUMENT"
                ? need([MAIN])
                : kind === "EVIDENCE_KIND_TEST_REPORT"
                  ? has(MAIN) <= 1
                  : roles.length === 0;
    // Too many or unexpected files are a bad item; too few are a missing asset.
    const surplus = roles.some((r) => !allowed.includes(r) || has(r) > (kind === "EVIDENCE_KIND_IMAGE" && r === MAIN ? MAX_IMAGE_SET : 1));
    if (!ok) throw new BeeError("invalid_argument", { reason: surplus ? "bad_evidence" : "missing_asset", message: "the files do not match what a " + kind.replace("EVIDENCE_KIND_", "").toLowerCase() + " item needs" });
  }

  /** Per-CR limits: 64 items and 512 MiB of assets. */
  checkRoom(c: CrState, addBytes: number): void {
    const mine = this.forChange(c.id);
    if (mine.length >= MAX_ITEMS) throw tooLarge("a change request has at most " + MAX_ITEMS + " evidence items");
    if (mine.reduce((n, e) => n + e.assets.reduce((m, a) => m + a.sizeBytes, 0), 0) + addBytes > MAX_TOTAL) throw tooLarge("a change request has at most 512 MiB of evidence");
  }

  /** The revision an item is captured for: the request's, the current one, or the one a check run tested. */
  revisionFor(s: FixtureStore, c: CrState, requested: number | undefined, checkRunId: string | undefined): { revision: number; source: T.EvidenceSource } {
    if (checkRunId) {
      // The fixture has no record of who reported a check run, so any editor of the change may attach it.
      const run = [...s.checks.values()].flat().find((x) => x.id === checkRunId);
      const rev = run?.changeset ? c.revisions.find((r) => objectIdText(r.head.id) === objectIdText(run.changeset as T.ObjectId)) : undefined;
      if (!run) throw new BeeError("not_found", { reason: "CHECK_NOT_FOUND", message: "no check run " + checkRunId });
      if (!rev) throw new BeeError("failed_precondition", { reason: "INVALID_STATE", message: "the check run did not test a revision of this change request" });
      return { revision: rev.n, source: CHECK_RUN };
    }
    const n = requested || s.current(c).n;
    if (!c.revisions.some((r) => r.n === n)) throw new BeeError("not_found", { reason: "UNKNOWN_REVISION", message: "no revision r" + n });
    return { revision: n, source: UPLOADED };
  }

  /** Makes this item the change request's only hero when it asks to be. */
  claimHero(row: EvidenceRow): void {
    if (!row.spec.hero) return;
    for (const e of this.items) if (e.crId === row.crId && e !== row && e.spec.hero) e.spec = { ...e.spec, hero: false };
  }

  /** Builds an asset row from sniffed bytes, enforcing the type, size and alt-text rules. */
  assetFrom(kind: Kind, role: Role, bytes: Uint8Array, declared: string | undefined, alt: string | undefined, dims: { width?: number; height?: number; durationMs?: number | string }): AssetRow {
    const sniff = sniffEvidence(bytes);
    if (!sniff || (declared && declared !== sniff.contentType && !(sniff.class === "text" && declared.startsWith("text/")))) throw invalid("the bytes are not " + (declared || "a supported type"), "bad_media");
    if (!expectedClass(kind, role).includes(sniff.class)) throw invalid("a " + roleName(role) + " file of that kind must be " + expectedClass(kind, role).join(" or "), "bad_evidence");
    if (bytes.length > LIMIT[sniff.class]) throw tooLarge("that file is over the " + LIMIT[sniff.class] / MiB + " MiB limit");
    const text = (alt ?? "").trim();
    if (sniff.class === "image" && (!text || lengthOf(text) > 1000)) throw invalid("every image needs alt text of 1 to 1000 characters", "missing_alt");
    const size = sniff.contentType === "image/png" ? pngSize(bytes) : undefined;
    return {
      role,
      contentType: declared && sniff.class === "text" ? declared : sniff.contentType,
      width: size?.width ?? dims.width ?? 0,
      height: size?.height ?? dims.height ?? 0,
      durationMs: Number(dims.durationMs ?? 0),
      alt: text,
      sizeBytes: bytes.length,
      sha256: sha256(bytes),
      source: { bytes },
    };
  }

  /** Receives the bytes of a pending upload: the stand-in for the server's signed PUT route. */
  receive(evidenceId: string, role: Role, bytes: Uint8Array, s: FixtureStore): void {
    const e = this.byId(evidenceId);
    const a = e?.assets.find((x) => x.role === role);
    if (!e || !a) throw notFound(evidenceId);
    if (e.state !== PENDING) throw new BeeError("failed_precondition", { reason: "INVALID_STATE", message: "this upload is already finalized" });
    if (e.createdAt + UPLOAD_TTL < s.now()) throw new BeeError("permission_denied", { reason: "URL_EXPIRED", message: "the upload link has expired" });
    if (bytes.length !== a.sizeBytes) throw invalid("the upload is " + bytes.length + " bytes, not the " + a.sizeBytes + " announced", "bad_media");
    a.source = { bytes };
  }

  /** A PUT to a signed upload URL: the same checks as a download, and the caller must be able to edit the change. */
  acceptUpload(s: FixtureStore, segments: string[], query: URLSearchParams, bytes: Uint8Array, viewer: string | undefined): void {
    const [ns, id, name] = segments;
    const e = ns === "fixture" && segments.length === 3 ? this.byId(id) : undefined;
    const role = roleOf(name ?? "");
    const exp = Number(query.get("exp"));
    const path = "/_evidence/" + segments.join("/");
    if (!e || !role || !exp || query.get("sig") !== this.signature(path, exp)) throw new BeeError("permission_denied", { reason: "BAD_SIGNATURE", message: "the link is not valid" });
    if (exp < s.now()) throw new BeeError("permission_denied", { reason: "URL_EXPIRED", message: "the link has expired" });
    const repo = s.repos.get(e.repo);
    const cr = s.crs.find((x) => x.id === e.crId);
    if (!repo || !cr || !this.canEdit(s, viewer, repo, cr)) throw denied("only the author or someone with write access can upload evidence");
    this.receive(e.id, role, bytes, s);
  }

  /** Drops items left PENDING past their hour. */
  sweep(s: FixtureStore): void {
    this.items = this.items.filter((e) => e.state !== PENDING || e.createdAt + PENDING_TTL >= s.now());
  }

  announce(s: FixtureStore, repo: RepoState, kind: string, actor: string, e: EvidenceRow): void {
    s.emit(repo, kind, actor, { note: { verb: kind.replace("evidence.", "evidence "), title: e.spec.title, target: "evidence:" + e.id } });
  }

  // ------------------------------------------------------------------ the conversation

  addComment(s: FixtureStore, repo: RepoState, c: CrState, actor: string, body: string): T.Comment {
    const text = body.trim();
    if (!text) throw invalid("a comment needs a body");
    const list = this.comments.get(c.id) ?? [];
    const at = s.iso(s.now());
    const cm: T.Comment = {
      id: ulid(s.now(), "comment:" + c.id + ":" + list.length),
      subject: { kind: "SUBJECT_KIND_CHANGE_REQUEST", repo: { fullName: repo.repo.fullName }, id: c.id },
      author: s.p(actor),
      body: text,
      createdAt: at,
      updatedAt: at,
      edited: false,
    };
    this.comments.set(c.id, [...list, cm]);
    s.emit(repo, "review.commented", actor, { note: { title: c.title, target: "cr:" + c.id } });
    return cm;
  }

  // ------------------------------------------------------------------ the demo world

  seed(s: FixtureStore): void {
    const ago = (minutes: number) => s.ago(minutes);
    const put = (
      cr: string,
      n: number,
      at: number,
      by: string,
      spec: Partial<T.EvidenceSpec> & { kind: Kind; title: string },
      assets: Array<{ role: Role; source: Source; contentType: string; alt?: string; width?: number; height?: number; durationMs?: number; sizeBytes?: number; sha256?: string }> = [],
      source: T.EvidenceSource = UPLOADED,
      checkName?: string,
    ) => {
      const c = s.crByKey(cr);
      const id = ulid(at, "evidence:" + cr + ":" + spec.title);
      const rows: AssetRow[] = assets.map((a) => {
        const bytes = a.sizeBytes === undefined ? readSource(a.source) : undefined;
        return {
          role: a.role,
          contentType: a.contentType,
          width: a.width ?? 0,
          height: a.height ?? 0,
          durationMs: a.durationMs ?? 0,
          alt: a.alt ?? "",
          sizeBytes: a.sizeBytes ?? bytes?.length ?? 0,
          sha256: a.sha256 ?? (bytes ? sha256(bytes) : ""),
          source: a.source,
        };
      });
      const check = checkName ? (s.checks.get(objectIdText(c.revisions.find((r) => r.n === n)?.head.id as T.ObjectId)) ?? []).find((x) => x.name === checkName) : undefined;
      const row: EvidenceRow = {
        id,
        repo: c.repo,
        crId: c.id,
        revision: n,
        spec: this.normalizeSpec({ benchmark: [], url: "", caption: "", paths: [], hero: false, order: 0, ...spec } as T.EvidenceSpec),
        assets: rows,
        source,
        checkRunId: check?.id ?? "",
        by,
        state: READY,
        createdAt: at,
        updatedAt: at,
      };
      this.items.push(row);
      return row;
    };
    const shot = (key: string) => {
      const f = MEDIA_FILES[key];
      return { contentType: f?.contentType ?? "image/png", width: f?.width, height: f?.height, sizeBytes: f?.sizeBytes, sha256: f?.sha256, source: { media: key } as Source };
    };

    // Rotate session ids (three revisions, written by an agent): a brief, and evidence for r1 that
    // r3 has made stale, plus evidence for the current revision.
    const rotate = s.crByKey("rotate");
    const rotateRepo = s.repos.get(rotate.repo) as RepoState;
    this.briefs.set(rotate.id, {
      outcome: "Changing someone's role now signs out their old sessions, so a downgraded admin can no longer act with admin rights.",
      highlights: [
        { text: "A session gets a new id the moment its owner's role changes.", paths: ["src/auth/session.ts"] },
        { text: "The old id stops working at once; other tabs sign in again.", paths: ["src/auth/session.ts", "tests/auth.test.ts"] },
      ],
      asks: [{ question: "Sign out other devices when a role changes?", options: ["Only this session", "All sessions of that person"] }],
      risks: [{ text: "People with several tabs open are signed out of the old ones.", mitigation: "Only a role change triggers it; ordinary requests keep the session." }],
      rollback: "Revert this change request; sessions keep their ids as before.",
      tryItUrl: "https://preview.acme.example/api/pr-rotate",
      writtenForRevision: 3,
      author: s.p("triage"),
      updatedAt: s.iso(ago(95)),
    });
    this.briefSets.set(rotate.id, 1);
    put(
      "rotate",
      1,
      ago(2_880),
      "triage",
      { kind: "EVIDENCE_KIND_BEFORE_AFTER", title: "Sessions list, before and after", caption: "First attempt: the current session is marked, other sessions can be signed out.", order: 3, paths: ["src/auth/session.ts"] },
      [
        { role: BEFORE, source: { gen: "before" }, contentType: "image/png", alt: "The sessions list: four identical rows.", width: FRAME_WIDTH, height: FRAME_HEIGHT },
        { role: AFTER, source: { gen: "after" }, contentType: "image/png", alt: "The sessions list: the first row is outlined in green with a Current badge, the others have a Sign out button.", width: FRAME_WIDTH, height: FRAME_HEIGHT },
      ],
    );
    put(
      "rotate",
      3,
      ago(100),
      "triage",
      { kind: "EVIDENCE_KIND_SCREENSHOT", title: "The page after the change", caption: "The change request page for this change, captured at 1440 by 900.", hero: true, order: 0 },
      [{ role: MAIN, ...shot("inbox-change"), alt: MEDIA_ALT["inbox-change"] }],
    );
    put(
      "rotate",
      3,
      ago(98),
      "triage",
      { kind: "EVIDENCE_KIND_VIDEO", title: "Walkthrough", caption: "Eight seconds: sign in as an admin, change the role, watch the old session end.", order: 1 },
      [
        { role: MAIN, ...shot("walkthrough"), alt: "", durationMs: 8000 },
        { role: POSTER, ...shot("inbox-home"), alt: MEDIA_ALT["inbox-home"] },
        { role: CAPTIONS, source: { text: WALKTHROUGH_VTT }, contentType: "text/vtt" },
      ],
    );
    put(
      "rotate",
      3,
      ago(92),
      "triage",
      {
        kind: "EVIDENCE_KIND_TEST_REPORT",
        title: "Auth tests",
        caption: "Run by CI on the current head.",
        order: 2,
        testReport: { passed: 14, failed: 0, skipped: 0, durationMs: "3120", failures: [], summary: "All 14 auth tests pass" },
      },
      [],
      CHECK_RUN,
      "test",
    );
    // The ask has been answered: agents read decisions where they read every comment.
    this.addSeedComment(s, rotateRepo, rotate, "mara", 'Decision on "Sign out other devices when a role changes?": Only this session', ago(80));
    this.addSeedComment(s, rotateRepo, rotate, "triage", "Thanks. Keeping other devices signed in; the tests cover it.", ago(78));

    // Cache invoice totals (a person's change): a benchmark against the target branch.
    const cache = s.crByKey("cache");
    this.briefs.set(cache.id, {
      outcome: "Invoice totals load about 30% faster for customers with long histories.",
      highlights: [{ text: "Totals are remembered per customer instead of recomputed on every request.", paths: ["src/billing/invoice.ts"] }],
      asks: [],
      risks: [{ text: "The cache never evicts, so memory grows with the number of customers.", mitigation: "A follow-up adds a size limit before it ships." }],
      rollback: "",
      tryItUrl: "",
      writtenForRevision: 1,
      author: s.p("lin"),
      updatedAt: s.iso(ago(380)),
    });
    this.briefSets.set(cache.id, 1);
    put("cache", 1, ago(380), "lin", {
      kind: "EVIDENCE_KIND_BENCHMARK",
      title: "Invoice totals, p95 latency",
      caption: "1,000 requests against a customer with 2,400 invoices.",
      hero: true,
      benchmark: [
        { name: "p95 latency", unit: "ms", baseline: 120, value: 85, lowerIsBetter: true },
        { name: "memory", unit: "MB", baseline: 210, value: 224, lowerIsBetter: true },
      ],
    });
    put(
      "cache",
      1,
      ago(375),
      "lin",
      { kind: "EVIDENCE_KIND_IMAGE", title: "Profiles", caption: "Flame graphs before and after.", order: 1 },
      [
        { role: MAIN, source: { gen: "panel" }, contentType: "image/png", alt: "A flame graph of the old totals path: one wide block.", width: FRAME_WIDTH, height: FRAME_HEIGHT },
        { role: MAIN, source: { gen: "after" }, contentType: "image/png", alt: "A flame graph of the new totals path: the wide block is gone.", width: FRAME_WIDTH, height: FRAME_HEIGHT },
      ],
    );

    // The sessions list (an agent's change in acme/web): a screenshot of r1 that r2 made stale, then
    // before/after, a captioned video and an ask nobody has answered yet. It mirrors the `ui`
    // change of the real-server seed (e2e/seed/briefs.ts).
    const ui = s.crByKey("ui");
    this.briefs.set(ui.id, {
      outcome: "The settings page marks which session is yours and lets you sign out of the others.",
      highlights: [
        { text: "The row for this device is outlined and says \"This device\".", paths: ["components/sessions.tsx"] },
        { text: "Every other row has a Sign out button that ends only that session.", paths: ["components/sessions.tsx"] },
      ],
      asks: [{ question: "Show the device name or only \"This device\" on the current row?", options: ["Device name", "Only \"This device\""] }],
      risks: [{ text: "People who keep several browsers signed in can now sign them out by accident.", mitigation: "Signing out asks for no confirmation, but it only ends that one session." }],
      rollback: "Revert this change request; the list goes back to plain rows.",
      tryItUrl: "https://preview.acme.example/web/settings/sessions",
      writtenForRevision: 2,
      author: s.p("triage"),
      updatedAt: s.iso(ago(125)),
    });
    this.briefSets.set(ui.id, 1);
    put(
      "ui",
      1,
      ago(310),
      "triage",
      { kind: "EVIDENCE_KIND_SCREENSHOT", title: "First attempt", caption: "Every row marked, with a banner the design later dropped.", order: 2, paths: ["components/sessions.tsx"] },
      [{ role: MAIN, source: { gen: "panel" }, contentType: "image/png", alt: "The first attempt at the sessions list: every row is marked and a banner sits below the list.", width: FRAME_WIDTH, height: FRAME_HEIGHT }],
    );
    put(
      "ui",
      2,
      ago(120),
      "triage",
      { kind: "EVIDENCE_KIND_BEFORE_AFTER", title: "Sessions list, before and after", caption: "Captured on the preview.", hero: true, order: 0, paths: ["components/sessions.tsx"] },
      [
        { role: BEFORE, source: { gen: "before" }, contentType: "image/png", alt: "The sessions list before the change: four rows that all look the same.", width: FRAME_WIDTH, height: FRAME_HEIGHT },
        { role: AFTER, source: { gen: "after" }, contentType: "image/png", alt: "The sessions list after the change: the first row is outlined in green with a filled button, the other three have an outlined Sign out button.", width: FRAME_WIDTH, height: FRAME_HEIGHT },
      ],
    );
    put(
      "ui",
      2,
      ago(118),
      "triage",
      { kind: "EVIDENCE_KIND_VIDEO", title: "Walkthrough", caption: "Eight seconds: each row switches to its new look in turn.", order: 1, paths: ["components/sessions.tsx"] },
      [
        { role: MAIN, ...shot("walkthrough"), alt: "", durationMs: 8000 },
        { role: POSTER, ...shot("inbox-home"), alt: MEDIA_ALT["inbox-home"] },
        { role: CAPTIONS, source: { text: WALKTHROUGH_VTT }, contentType: "text/vtt" },
      ],
    );

    // The rounding fix: its test report is CI's, so it carries the check run's source chip.
    const tax = s.crByKey("tax");
    this.briefs.set(tax.id, {
      outcome: "Invoice totals match the printed invoice to the cent: tax is allocated per line, so the lines always add up.",
      highlights: [{ text: "Line taxes use the largest-remainder rule, so they sum to the tax on the total.", paths: ["src/billing/tax.ts"] }],
      asks: [],
      risks: [],
      rollback: "",
      tryItUrl: "",
      writtenForRevision: 2,
      author: s.p("billing"),
      updatedAt: s.iso(ago(690)),
    });
    this.briefSets.set(tax.id, 1);
    put(
      "tax",
      2,
      ago(680),
      "billing",
      {
        kind: "EVIDENCE_KIND_TEST_REPORT",
        title: "Billing tests",
        caption: "Run by CI on the current head.",
        hero: true,
        order: 0,
        testReport: { passed: 9, failed: 0, skipped: 0, durationMs: "2480", failures: [], summary: "All 9 billing tests pass" },
      },
      [],
      CHECK_RUN,
      "test",
    );
    // `webhooks` is a docs change: it has no brief and no evidence, and the checklist owes nothing.
  }

  private addSeedComment(s: FixtureStore, repo: RepoState, c: CrState, who: string, body: string, at: number): void {
    const list = this.comments.get(c.id) ?? [];
    this.comments.set(c.id, [
      ...list,
      {
        id: ulid(at, "comment:" + c.id + ":" + list.length),
        subject: { kind: "SUBJECT_KIND_CHANGE_REQUEST", repo: { fullName: repo.repo.fullName }, id: c.id },
        author: s.p(who),
        body,
        createdAt: s.iso(at),
        updatedAt: s.iso(at),
        edited: false,
      },
    ]);
  }
}

// ------------------------------------------------------------------ sessions

type RepoLike = { id?: string; fullName?: string };
const quoted = (x: string | undefined) => x ?? "";

/** The agent sessions behind a change request or a changeset (AgentService.GetSessionsForChange). */
export function sessionsFor(s: FixtureStore, req: { repo?: RepoLike; changeset?: T.ObjectId; changeRequest?: string }): { sessions: AgentSessionJson[] } {
  const repo = s.repo(req.repo);
  if (!req.changeset && !req.changeRequest) throw invalid("name a changeset or a change request");
  const crs = req.changeRequest
    ? [s.cr(repo, quoted(req.changeRequest))].filter(Boolean)
    : s.crs.filter((c) => c.repo === repo.name && c.revisions.some((r) => objectIdText(r.head.id) === objectIdText(req.changeset as T.ObjectId)));
  const bySession = new Map<string, { sessions: AgentSessionJson; time: number }>();
  for (const c of crs) {
    for (const r of c.revisions) {
      const p = r.head.provenance;
      if (!p?.session) continue;
      if (req.changeset && objectIdText(r.head.id) !== objectIdText(req.changeset)) continue;
      const cost = p.cost;
      const prev = bySession.get(p.session);
      const sum = (a: string | undefined, b: string | undefined) => String(Number(a ?? 0) + Number(b ?? 0));
      bySession.set(p.session, {
        time: Math.min(prev?.time ?? Infinity, r.head.time),
        sessions: {
          session: p.session,
          runId: "run_" + p.session.slice(4),
          agentPrincipalId: p.principalChain[0] ?? "",
          model: p.model,
          runtime: p.runtime,
          promptDigest: p.promptDigest,
          chain: p.principalChain,
          changesets: [...(prev?.sessions.changesets ?? []), r.head.id],
          changeRequestIds: [...new Set([...(prev?.sessions.changeRequestIds ?? []), c.id])],
          cost: { inputTokens: sum(prev?.sessions.cost?.inputTokens, cost?.inputTokens), outputTokens: sum(prev?.sessions.cost?.outputTokens, cost?.outputTokens), usdMicros: sum(prev?.sessions.cost?.usdMicros, cost?.usdMicros) },
        },
      });
    }
  }
  return { sessions: [...bySession.values()].sort((a, b) => a.time - b.time || String(a.sessions.session).localeCompare(String(b.sessions.session))).map((x) => ({ ...x.sessions, createdAt: s.iso(x.time) })) };
}

// ------------------------------------------------------------------ the BriefService

const me = (s: FixtureStore, ctx: Ctx): string => s.caller(ctx.token);

/** The change request of a request, with the right to edit it checked. */
function editable(s: FixtureStore, ctx: Ctx, repoRef: RepoLike | undefined, id: string | undefined): { actor: string; repo: RepoState; c: CrState } {
  const actor = me(s, ctx);
  const repo = s.repo(repoRef);
  const c = s.cr(repo, id ?? "");
  if (!s.briefs.canEdit(s, actor, repo, c)) throw denied("only the author or someone with write access can change the brief and evidence");
  return { actor, repo, c };
}

function evidenceOf(s: FixtureStore, repoRef: RepoLike | undefined, id: string | undefined, viewer: string | undefined): { e: EvidenceRow; repo: RepoState; c: CrState } {
  const repo = s.repo(repoRef);
  const e = s.briefs.byId(id);
  const c = e && s.crs.find((x) => x.id === e.crId);
  if (!e || !c || e.repo !== repo.name) throw notFound(id ?? "");
  if (e.state === PENDING && !s.briefs.canEdit(s, viewer, repo, c)) throw notFound(id ?? "");
  return { e, repo, c };
}

type InlineAsset = { role?: Role; contentType?: string; data?: string; alt?: string; width?: number; height?: number; durationMs?: string };
type UploadAsset = { role?: Role; contentType?: string; sizeBytes?: string; alt?: string; width?: number; height?: number; durationMs?: string };

function decodeInline(a: InlineAsset): Uint8Array {
  const bytes = new Uint8Array(Buffer.from(a.data ?? "", "base64"));
  if (bytes.length > MAX_INLINE) throw tooLarge("inline files total at most 8 MiB; use an upload for larger ones");
  return bytes;
}

export const BRIEF_SERVICE = defineService(BriefService, {
  setChangeBrief: (s, req, ctx) => {
    const { actor, repo, c } = editable(s, ctx, req.repo, req.changeId);
    const expected = Number(req.expectedVersion ?? 0);
    if (expected && expected !== s.briefs.version(c)) throw new BeeError("failed_precondition", { reason: "VERSION_MISMATCH", message: "the change request is at version " + s.briefs.version(c) + ", not " + expected });
    const brief = s.briefs.setBrief(s, repo, c, actor, req.brief as T.ChangeBrief | undefined);
    return { brief, checklist: s.briefs.checklist(s, c, 0), version: String(s.briefs.version(c)) };
  },
  getBriefChecklist: (s, req) => s.briefs.checklist(s, s.cr(s.repo(req.repo), req.changeId ?? ""), req.revision ?? 0),
  addEvidence: (s, req, ctx) => {
    const { actor, repo, c } = editable(s, ctx, req.repo, req.changeId);
    const spec = s.briefs.normalizeSpec(req.spec as T.EvidenceSpec | undefined);
    const inline = (req.assets ?? []) as InlineAsset[];
    if (req.workflowArtifactId || (req.artifactAssets ?? []).length) throw new BeeError("unimplemented", { reason: "UNIMPLEMENTED", message: "the fixture has no workflow artifacts to copy from" });
    s.briefs.checkRoles(spec.kind, inline.map((a) => a.role ?? "EVIDENCE_ASSET_ROLE_UNSPECIFIED"));
    const total = inline.reduce((n, a) => n + Buffer.from(a.data ?? "", "base64").length, 0);
    if (total > MAX_INLINE) throw tooLarge("inline files total at most 8 MiB; use an upload for larger ones");
    const assets = inline.map((a) => s.briefs.assetFrom(spec.kind, a.role as Role, decodeInline(a), a.contentType, a.alt, a));
    s.briefs.checkRoom(c, assets.reduce((n, a) => n + a.sizeBytes, 0));
    const when = s.briefs.revisionFor(s, c, req.revision, req.checkRunId);
    const row: EvidenceRow = {
      id: ulid(s.now(), "evidence:new:" + c.id + ":" + s.briefs.items.length),
      repo: c.repo,
      crId: c.id,
      revision: when.revision,
      spec,
      assets,
      source: when.source,
      checkRunId: req.checkRunId ?? "",
      by: actor,
      state: READY,
      createdAt: s.now(),
      updatedAt: s.now(),
    };
    s.briefs.items.push(row);
    s.briefs.claimHero(row);
    s.briefs.announce(s, repo, "evidence.added", actor, row);
    s.appendAudit(actor, "evidence.add", spec.title + " (" + c.title + ")", repo.name, true, "");
    return s.briefs.view(s, row);
  },
  beginEvidenceUpload: (s, req, ctx) => {
    const { actor, repo, c } = editable(s, ctx, req.repo, req.changeId);
    s.briefs.sweep(s);
    const spec = s.briefs.normalizeSpec(req.spec as T.EvidenceSpec | undefined);
    const declared = (req.assets ?? []) as UploadAsset[];
    s.briefs.checkRoles(spec.kind, declared.map((a) => a.role ?? "EVIDENCE_ASSET_ROLE_UNSPECIFIED"));
    s.briefs.checkRoom(c, declared.reduce((n, a) => n + Number(a.sizeBytes ?? 0), 0));
    for (const a of declared) {
      const limit = Math.max(...expectedClass(spec.kind, a.role as Role).map((k) => LIMIT[k]));
      if (Number(a.sizeBytes ?? 0) > limit) throw tooLarge("that file is over the " + limit / MiB + " MiB limit");
    }
    const when = s.briefs.revisionFor(s, c, req.revision, req.checkRunId);
    const row: EvidenceRow = {
      id: ulid(s.now(), "evidence:new:" + c.id + ":" + s.briefs.items.length),
      repo: c.repo,
      crId: c.id,
      revision: when.revision,
      spec,
      assets: declared.map((a) => ({
        role: a.role as Role,
        contentType: a.contentType ?? "",
        width: a.width ?? 0,
        height: a.height ?? 0,
        durationMs: Number(a.durationMs ?? 0),
        alt: (a.alt ?? "").trim(),
        sizeBytes: Number(a.sizeBytes ?? 0),
        sha256: "",
      })),
      source: when.source,
      checkRunId: req.checkRunId ?? "",
      by: actor,
      state: PENDING,
      createdAt: s.now(),
      updatedAt: s.now(),
    };
    s.briefs.items.push(row);
    s.appendAudit(actor, "evidence.begin", spec.title + " (" + c.title + ")", repo.name, true, "");
    return {
      evidence: s.briefs.view(s, row),
      uploads: row.assets.map((a) => {
        const u = s.briefs.signedUrl(s, row, a.role);
        return { role: a.role, uploadUrl: u.url, uploadHeaders: { "content-type": a.contentType }, expiresAt: s.iso(s.now() + UPLOAD_TTL) };
      }),
    };
  },
  finalizeEvidence: (s, req, ctx) => {
    const actor = me(s, ctx);
    const { e, repo, c } = evidenceOf(s, req.repo, req.evidenceId, actor);
    if (!s.briefs.canEdit(s, actor, repo, c)) throw denied("only the author or someone with write access can finalize an upload");
    if (e.state !== PENDING) throw new BeeError("failed_precondition", { reason: "INVALID_STATE", message: "this evidence is not waiting for an upload" });
    const digests = new Map((req.digests ?? []).map((d) => [d.role as Role, (d.sha256 ?? "").toLowerCase()]));
    const finished: AssetRow[] = [];
    for (const a of e.assets) {
      const bytes = a.source && "bytes" in a.source ? a.source.bytes : undefined;
      if (!bytes) throw invalid("the " + roleName(a.role) + " file was never uploaded", "missing_asset");
      if (sha256(bytes) !== digests.get(a.role)) throw new BeeError("data_loss", { reason: "INTEGRITY", message: "the " + roleName(a.role) + " file does not match the digest sent" });
      finished.push({ ...s.briefs.assetFrom(e.spec.kind, a.role, bytes, a.contentType, a.alt, a), role: a.role });
    }
    e.assets = finished;
    e.state = READY;
    e.updatedAt = s.now();
    s.briefs.claimHero(e);
    s.briefs.announce(s, repo, "evidence.added", actor, e);
    return s.briefs.view(s, e);
  },
  getEvidence: (s, req, ctx) => {
    const { e } = evidenceOf(s, req.repo, req.id, s.viewer(ctx.token));
    return s.briefs.view(s, e);
  },
  listEvidence: (s, req) => {
    const repo = s.repo(req.repo);
    const ids = req.changeIds ?? [];
    if (ids.length < 1 || ids.length > 100) throw invalid("name 1 to 100 change requests", "INVALID_ARGUMENT");
    const crs = ids.map((id) => s.cr(repo, id));
    const rows: EvidenceRow[] = [];
    for (const c of crs) {
      const mine = s.briefs
        .forChange(c.id)
        .filter((e) => e.state === READY)
        .sort((a, b) => Number(b.spec.hero) - Number(a.spec.hero) || a.spec.order - b.spec.order || a.createdAt - b.createdAt || (a.id < b.id ? -1 : 1));
      rows.push(...(req.heroOnly ? mine.slice(0, 1) : mine));
    }
    const pg = page(rows, req.page, 100);
    return { evidence: pg.items.map((e) => s.briefs.view(s, e)), page: { nextPageToken: pg.next } };
  },
  updateEvidence: (s, req, ctx) => {
    const actor = me(s, ctx);
    const { e, repo, c } = evidenceOf(s, req.repo, req.id, actor);
    if (!s.briefs.canEdit(s, actor, repo, c)) throw denied("only the author or someone with write access can edit evidence");
    if (req.spec?.kind !== e.spec.kind) throw invalid("the kind of an evidence item never changes; add a new item", "kind_immutable");
    const next = s.briefs.normalizeSpec(req.spec as T.EvidenceSpec | undefined);
    for (const a of req.altTexts ?? []) {
      const asset = e.assets.find((x) => x.role === a.role);
      if (!asset) throw invalid("no " + roleName(a.role as Role) + " file", "bad_evidence");
      const alt = (a.alt ?? "").trim();
      if (asset.contentType.startsWith("image/") && (!alt || lengthOf(alt) > 1000)) throw invalid("every image needs alt text of 1 to 1000 characters", "missing_alt");
      asset.alt = alt;
    }
    e.spec = next;
    e.updatedAt = s.now();
    s.briefs.claimHero(e);
    s.briefs.announce(s, repo, "evidence.updated", actor, e);
    s.appendAudit(actor, "evidence.update", next.title + " (" + c.title + ")", repo.name, true, "");
    return s.briefs.view(s, e);
  },
  deleteEvidence: (s, req, ctx) => {
    const actor = me(s, ctx);
    const { e, repo, c } = evidenceOf(s, req.repo, req.id, actor);
    if (!s.briefs.canEdit(s, actor, repo, c)) throw denied("only the author or someone with write access can delete evidence");
    // CI evidence belongs to whoever reported it, and to maintainers.
    if (e.source !== UPLOADED && actor !== e.by && !s.posts.isAdmin(s, actor, repo)) throw denied("only the reporter or a maintainer can delete evidence from CI");
    s.briefs.items = s.briefs.items.filter((x) => x !== e);
    s.briefs.announce(s, repo, "evidence.deleted", actor, e);
    s.appendAudit(actor, "evidence.delete", e.spec.title + " (" + c.title + ")", repo.name, true, "");
    return {};
  },
});

// ------------------------------------------------------------------ change request comments

/** `ChangeService/ListChangeComments`: the conversation, oldest first. */
export function listConversation(s: FixtureStore, req: { repo?: RepoLike; changeRequestId?: string; page?: { pageSize?: number; pageToken?: string } }): { comments: CommentJson[]; page: { nextPageToken: string } } {
  const c = s.cr(s.repo(req.repo), req.changeRequestId ?? "");
  const pg = page(s.briefs.comments.get(c.id) ?? [], req.page, 100);
  return { comments: pg.items as CommentJson[], page: { nextPageToken: pg.next } };
}

export const BRIEF_SERVICES = [BRIEF_SERVICE];
