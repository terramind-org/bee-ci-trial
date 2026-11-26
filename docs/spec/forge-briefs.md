# Forge briefs and evidence (`bee-forge::briefs`)

What a change request shows a busy person: the **brief** in words, **evidence** in media, and
an advisory **checklist** of what is still owed. Contract: SPEC §11.9 (briefs and evidence),
§11.7 (events), §9.6 (signed media URLs), §12.2 (errors); wire: `proto/bee/v1/briefs.proto`
(`BriefService`) and `ChangeBrief` in `forge.proto`. Nothing here is consulted by landing: the
checklist nudges, it never gates.

## 1. Layout

| path | owns |
|---|---|
| `src/changes/brief.rs` | the brief: pure validation, `Changes::set_brief`, the brief written with `OpenChange`, `Changes::revision_paths` |
| `src/briefs/mod.rs` | the `briefs` surface, the `forge_evidence` table, migration `briefs/0001`, event kinds, the `evidence.` audit prefix, bounds |
| `src/briefs/model.rs` | the stored record (`EvidenceRec`, `AssetRec`) |
| `src/briefs/rules.rs` | pure checks of a spec and its assets: per-kind roles, kinds, alt text, sizes |
| `src/briefs/checklist.rs` | the checklist, a pure function |
| `src/briefs/store.rs` | `Briefs`: add (inline, from a workflow artifact, without files), begin, receive, finalize, get, list, update, delete; the brief and checklist as `BriefService` serves them |
| `src/briefs/transfer.rs` | the signed `/_evidence/...` URLs and what they serve |
| `src/briefs/maintain.rs` | upkeep of abandoned uploads |
| `src/briefs/service.rs` | the tonic `BriefService` (`BriefServices`) |
| `src/media/` | shared with posts: kinds and sniffing, the evidence limits, `inspect`, response headers and `Range` |

## 2. The brief

The brief lives on the stored `ChangeRequest` (field `brief`, boxed in Rust), so it is written in
the changes surface's transaction with the CR's optimistic version and read wherever the CR is.

- `SetChangeBrief` → `Changes::set_brief(cx, repo, id, brief, expected_version)` replaces it;
  `OpenChange.brief` → `NewChange.brief` sets it in the open transaction. Both validate first (a
  refused brief opens nothing) and emit `cr.brief_set` (after `cr.opened` on open); the audit
  action is `cr.set_brief` (opening with a brief is audited once, as `cr.open`).
- A brief may be written in any state of the CR (a landed change's brief still explains it),
  and so may evidence (a benchmark measured after landing).
- The server sets `written_for_revision` (the current revision), `author` (the caller, with
  directory data) and `updated_at`; values the caller sends in them are ignored.
- Limits (`changes::brief::validate`): outcome, rollback and every text at most 280 characters,
  ask options at most 80 (`brief_too_long`); at most 20 highlights, asks and risks, 2 to 6 options
  per ask, at most 20 non-empty paths per highlight, and no empty highlight, question, option or
  risk (`bad_brief`); `try_it_url` empty or an absolute `https` URL with a host, no user info and
  no white space (`bad_url`). An all-empty brief is valid and clears it.

## 3. Evidence

### 3.1 Storage

| table | surface | scope | id | keys | body |
|---|---|---|---|---|---|
| `forge_evidence` | `briefs` | repo | evidence (ULID) | `k1` change request, `n1` expiry ms (pending only) | `EvidenceRec` |

`EvidenceRec` holds the `Evidence` as served without URLs and staleness, the org id, one
`AssetRec` per asset (role, CI blob id, declared type and size, server SHA-256, received size)
and the pending expiry. Bytes live in the CI blob store (`ci::ports::CiBlobs`, the repo's org
namespace; the server wires `StoreCiBlobs`); URLs are signed with `ci::ports::UrlSigner`.
Ports: `BriefPorts { blobs, urls, artifacts }`, where `artifacts` (`ArtifactSource`, implemented
by `ci::Ci`) is optional and evidence from artifacts is `UNIMPLEMENTED` (`served_elsewhere`)
without it.

### 3.2 Kinds and assets

`rules::slots` is the table of roles each kind allows; anything else is `bad_evidence`.

| kind | assets | spec |
|---|---|---|
| `SCREENSHOT` | one `MAIN` image | |
| `BEFORE_AFTER` | one `BEFORE` and one `AFTER` image | |
| `VIDEO` | one `MAIN` video; optional `POSTER` image and `CAPTIONS` | |
| `BENCHMARK` | none | 1–20 `benchmark` series (non-empty name ≤120, unit ≤20, finite numbers) |
| `IMAGE` | 1–12 `MAIN` images | |
| `AUDIO` | one `MAIN` audio file; optional `CAPTIONS` | |
| `DOCUMENT` | one `MAIN` PDF, plain text, Markdown or CSV file | |
| `PREVIEW_LINK` | none | `url`, `https` (`bad_url`) |
| `TEST_REPORT` | optional `MAIN` document | `test_report` (≤50 failures) |

Every spec: a kind, a title of 1–120 characters, a caption ≤1000, ≤20 non-empty paths;
benchmark series, test reports and URLs only on their own kind. Every image asset needs alt text
of 1–1000 characters (`missing_alt`). Classes and limits (`media::EVIDENCE_LIMITS`, `TOO_LARGE`
with `limit_exceeded` and limit `evidence_<class>_bytes`):

| class | kinds | limit |
|---|---|---|
| image | png, jpeg, gif, webp | 10 MiB |
| video | mp4, webm | 100 MiB |
| audio | mpeg, ogg, wav | 25 MiB |
| document | pdf, plain text, Markdown, CSV; WebVTT (captions only) | 20 MiB |

Per change request: at most 64 items (`evidence_items`) and 512 MiB of assets
(`evidence_bytes`; pending items count with their declared sizes), checked in the insert's
transaction. Inline assets of one request: at most 8 MiB (`evidence_inline_bytes`).

### 3.3 Byte safety

The declared content type must name a kind its role's class accepts (`bad_media`; SVG, HTML and
JavaScript types are never one), and the bytes must be that kind (`media::sniff_as`): binary kinds
by signature, text kinds as declared and only when inert (UTF-8, no control characters but tab,
line feed, form feed and carriage return, no binary signature, and not starting with `<`, which
refuses HTML, SVG and XML; WebVTT must start with `WEBVTT`). Script bytes declared as plain text
are plain text: they are served as `text/plain` with `nosniff`, the sandbox CSP and
`Content-Disposition: attachment`, so nothing runs them. Image dimensions are read by the server;
video sizes and durations are declared.

Files copied from a workflow artifact carry no type of their own: each is declared by its
signature, else (single-file artifacts) by the artifact's text content type, else by its
extension (`.txt`/`.log`, `.md`, `.csv`, `.vtt`), then checked the same way.

### 3.4 Revisions, sources and staleness

- An item records the revision it was captured for and that revision's head: the current
  revision unless the request names another (`bad_evidence` for one the CR does not have).
- `check_run_id`: the run must exist (`NOT_FOUND`), the caller must be its reporter
  (`PERMISSION_DENIED`), and its changeset must be the head of a revision of the CR
  (`bad_evidence`), which becomes the item's revision (a different requested revision is
  `bad_evidence`). Source `CHECK_RUN`.
- `workflow_artifact_id`: the artifact must be finalized and unexpired (`INVALID_STATE`), and its
  `head_changeset` the head of a revision of the CR (`bad_evidence`); with a check run too, both
  must name the same revision. Source `WORKFLOW_ARTIFACT`. Inline assets and artifact assets do
  not mix (`bad_evidence`); a zip's file is named by its path inside the archive (`bad_evidence`
  when absent), a single-file artifact takes an empty path.
- Otherwise the source is `UPLOADED`. `uploader` is the caller.
- `stale` is computed on every read: the item's revision is older than the CR's current one.
  It is never stored, and stale items stay listed.

### 3.5 Writes

| call | does | emits | audit |
|---|---|---|---|
| `AddEvidence` | checks, stores the bytes, inserts a `READY` item | `evidence.added` | `evidence.add` |
| `BeginEvidenceUpload` | checks the declared assets, inserts a `PENDING` item that expires in 1 h; one PUT URL per asset, valid 15 min (never past the item's expiry) | — | `evidence.begin_upload` |
| PUT to an upload URL | `Briefs::receive`: the item must be pending and unexpired (`INVALID_STATE`, `upload_expired`); checks the bytes against the asset's declared type and class limit; computes their SHA-256; replaces earlier bytes | — | — |
| `FinalizeEvidence` | every asset received (`missing_asset`); each digest (paired with assets in order within each role; `bad_digest` when malformed, missing or left over) equal to the server's and each size to the declared one (`INTEGRITY`); the item becomes `READY` | `evidence.added` | `evidence.finalize` |
| `UpdateEvidence` | replaces the spec (same kind, else `kind_immutable`) and the alt text of assets (paired in order within each role); the files never change | `evidence.updated` | `evidence.update` |
| `DeleteEvidence` | deletes the item and its files (a pending item emits nothing) | `evidence.deleted` | `evidence.delete` |

Events have the CR as subject (so `LiveRefresh` on the CR sees them) and the `Evidence` as
`payload_json`. Setting `hero` on a ready item clears the CR's other hero in the same
transaction. Bytes are stored before the transaction and deleted again when it fails.

### 3.6 Reads

- `GetEvidence`: any state.
- `ListEvidence`: 1–100 change request ids (`missing_field`; more is `TOO_LARGE`), each once in
  the order given; each CR's `READY` items hero first, then `spec.order`, then creation;
  `hero_only` keeps the first of each. Pages are offsets into that list.
- Every read signs a download URL per asset of a ready item for the caller's principal chain
  (empty for a signed-out caller), valid 15 minutes, with `url_expires_at`.

## 4. Transfer URLs

| method | path |
|---|---|
| `PUT` | `/_evidence/<repo>/<evidence>/<asset index>/upload` |
| `GET`, `HEAD` | `/_evidence/<repo>/<evidence>/<asset index>/as/<chain>` |

Both carry `?expires=<ms>&signature=<hex>` from `UrlSigner` over the path and expiry. A
download path names the principal chain it was issued to (`media::upload::encode_chain`, `-` for
a signed-out reader), so the signature binds it. The server (`bee-server`, task B4):

1. `Briefs::verify_upload` / `verify_download` (`PERMISSION_DENIED` for unsigned, tampered or
   the other kind of URL, `TOKEN_EXPIRED` once expired);
2. for downloads, decides repo `read` again for the URL's chain (SPEC §9.6), then
   `Briefs::download` (ready items only, else `NOT_FOUND`);
3. for uploads, `Briefs::receive_upload` (bounded by `Briefs::max_upload_bytes`, 100 MiB);
4. serves with `media::serve::SAFETY_HEADERS`, the sniffed type, `Download::disposition`
   (`attachment` for anything but images, video and audio), `ETag` from `Download::digest`, and
   `Range` via `media::serve::parse_range`.

## 5. Checklist

`checklist::compute` is a pure function of the revision, the CR author's kind, the revision's
changed paths with their diff classes, the brief and the evidence. `GetBriefChecklist` (revision
0 = current) and `SetChangeBrief` return it. The current revision's classes are the stored ones;
an older revision is classified again from its head and base under the repo's current policy
(`Changes::revision_paths`).

| item | raised when | satisfied by |
|---|---|---|
| `brief` | the CR's author is an agent or a service (not a human) and a path is `logic` or `sensitive` | an outcome and at least one highlight |
| `brief:risk` | a path is `sensitive` | at least one risk, or a rollback |
| `evidence:ui` | a UI file (`.tsx .jsx .vue .svelte .astro .html .css .scss .less`, case-insensitive) is `logic` or `sensitive` | a `READY` screenshot, before/after or video captured for this revision |

Items come in that order; `paths` lists at most 20 of the paths that raised an item, in path
order; `evidence_ids` are sorted; `complete` is true when every item is satisfied (and when there
are none). "Agent-authored" counts services because both are supervised the same way: a person
reviews what a non-human author produced.

## 6. Authorization (`service.rs`)

| RPCs | who |
|---|---|
| `GetBriefChecklist`, `GetEvidence`, `ListEvidence` | repo `read` |
| `SetChangeBrief`, `AddEvidence`, `BeginEvidenceUpload`, `FinalizeEvidence`, `UpdateEvidence` | the CR's author (with `read`) or `commit`; never on an archived repo; the token must allow `commit`; denials are audited under the write's action |
| `DeleteEvidence` | uploaded evidence as above; CI evidence (`CHECK_RUN`, `WORKFLOW_ARTIFACT`) only its uploader (the reporter) or a repo maintainer |

## 7. Upkeep

`Briefs::maintain` deletes pending items past their expiry (1 h after `BeginEvidenceUpload`)
with their files, at most 1000 per pass. The server calls it on its maintenance interval.

## 8. Server wiring (B4)

Construct `Briefs::new(&forge, changes, BriefPorts { blobs: StoreCiBlobs, urls: UrlSigner,
artifacts: Some(ci) })`, register `BriefServices::new(forge, briefs).register(..)`, route
`/_evidence/*` as in §4, and call `maintain` on the maintenance loop.
