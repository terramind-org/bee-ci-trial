# Forge community (`bee-forge::community`)

Collaboration surfaces of Beehive (PRODUCT §4.16): discussions, wikis, gists, releases,
notifications, users and profiles, insights, sponsors, and posts with the Following feed (§9).
Services: `DiscussionService`, `WikiService`, `GistService`, `ReleaseService`,
`NotificationService`, `UserService`, `InsightsService`, `SponsorService`, `PostService`. Conventions are those of `forge-core.md` (§2.3 store
methods, §6 audit, §7 authorization, §8 errors, §9 extension points).

## 1. Layout and surfaces

| surface | owns |
|---|---|
| `community` | discussions, reactions, wiki switches, gists, releases and assets, traffic counters, sponsors |
| `notifications` | inbox threads, subscriptions, watches, settings, feed, reminders, delivery queue, fan-out jobs |
| `users` | profiles, logins and redirects, emails, follows, repo follows, stars, star lists, exports, security log |

Each surface has its own pool, so inbox traffic cannot starve discussions. Migrations are
`community/0001_create_community`, `0002_create_notifications`, `0003_create_users`,
`0004_create_activity_and_signing_keys`, `0005_create_repo_follows`. Event
kinds and audit prefixes (`discussion.`, `release.`, `tag.`, `wiki.`, `gist.`, `comment.`,
`reaction.`, `notification.`, `user.`, `follow.`, `star.`, `star_list.`, `sponsor.`,
`insights.`) are declared in `MODULE`. Services are registered by
`ForgeServices::community_services` / `register`.

## 2. Ports the server backs

`Hooks::community` (`community::Ports`). Every port has a detached default (`UNAVAILABLE` /
`port_unavailable`, or empty where empty is right) and an in-memory implementation.

| port | used for | in-memory |
|---|---|---|
| `ContentVcs` | wiki (`<repo>.wiki`) and gist repos, file reads of real repos (forms, `release.yml`, FUNDING, citation, license, languages) | `MemoryVcs` |
| `TagVcs` | `tags/*` refs, annotations | `MemoryVcs` |
| `ChangeSource` | landed change requests between two tags | `MemoryChanges`; real: `backing::ChangesSource(Arc<Changes>)` |
| `ActivitySource` | log with provenance, change requests, issues, network graph | `MemoryActivity` |
| `IssueBridge` | issue to discussion and back | `MemoryIssues`; real: `backing::WorkIssues` (installed by `Forge::open` when none is given) |
| `DependencySource` | org dependency insights | none (empty) |
| `UrlSigner` | upload and download URLs (assets, avatars, exports, archives) | `HmacSigner` |
| `SponsorPayments` | checkout and cancel | `OfflinePayments` (sponsorships are active at once) |

Revisions resolve through the forge's `RevisionResolver` hook. Workers the server runs: mail
(`Notifications::claim_deliveries(Email)` / `complete_delivery`), Slack and Teams (same call per
channel), inbound mail (`resolve_reply`), email verification mail (`Users::claim_verifications`),
archive builder (`Releases::pending_archives` / `mark_archive_built`), export builder
(`Users::claim_exports` / `complete_export`), signing (`Releases::record_attestation`),
billing (`Sponsors::confirm_payment`, `renew`, `record_payout`), traffic recording
(`Insights::record_view` / `record_clone`), imports (`Users::register_placeholder`).

## 3. Discussions

Home = repo or org; numbers per home; ids `<home>.<number>` and `<discussion>.<ULID>`. Needs
`has_discussions` on repo homes (`INVALID_STATE` / `discussions_disabled`).

| action | repo home | org home |
|---|---|---|
| read, list | `read` | member |
| start, comment, react, vote, upvote, edit/close/delete own | `read` | member |
| edit/close/delete others', lock, labels, mark answer (non-author), convert | `triage` | owner or admin |
| categories, pin | `manage_settings` + maintain | owner or admin |

Announcement categories: only moderators start discussions. Poll categories: 2 to 8 options,
one changeable vote, none once closed or locked. Q&A: one answer. Locked: moderators only
(`LOCKED`). Replies to replies attach to the root. At most 4 pins, 25 categories, 20 labels.
Forms: `.github/DISCUSSION_TEMPLATE/<category slug>.yml` on the default ref composes the body
(`### <label>` per answered field; required fields `form_field_required`). Conversion calls the
`IssueBridge` and emits `discussion.converted_from_issue` / `discussion.converted_to_issue`.
Counters are denormalized under row locks. Archived repos: writes are `ARCHIVED`.

## 4. Wiki and gists

Wiki: `EnableWiki` creates the sibling repo (`ContentVcs::init`); pages are files `<slug>.<ext>`
(`md`, `adoc`, `rst`, `org`, `textile`, `mediawiki`); `_Sidebar` and `_Footer` are special; each
change is a changeset by the caller; `expected_changeset` guards page edits
(`VERSION_MISMATCH`); edit access `COLLABORATORS` (`commit`) or `READERS` (`read`).

Gists: user-owned repo `gist/<id>`; public (listed) or secret (by id); revisions are changesets;
`expected_head` guards updates; forks copy history; stars and comments counted; owner-only
changes. Token must allow `read` (reads) or `commit` (changes).

## 5. Releases

Draft (no tag until publish), prerelease (never latest), latest = newest `latest_at` among
published, non-prerelease, not `make_latest = FALSE`; `TRUE` pins now. Immutable (drafts only):
after publish the tag, target and assets are frozen, the tag cannot be deleted, the name is
tombstoned after deletion. Writes need `commit`; deleting the tag with the release also
`manage_refs`. Drafts are visible to `commit` holders. Tags these calls create or delete
(`CreateRepoTag`, `DeleteRepoTag`, publishing a release whose tag does not exist yet, deleting a
published release with its tag) also pass the repo policy's ref rules for `tags/<name>`
(`[[refs]]`, SPEC §9.4), exactly as `RefService` and git pushes do; a refusal is
`POLICY_DENIED`.

Generated notes: change requests landed in the history of the new tag but not of the previous
one (`ChangeSource`), in landing order; `## What's Changed` (or the categories of
`.github/release.yml`: `exclude.labels/authors`, `categories[].title/labels/exclude`, `*`),
`## New Contributors` (only with a previous tag), `**Full Changelog**: <compare link>`.
Previous tag = named, else the newest published release's. Assets: begin, out-of-band PUT,
finalize with SHA-256 (the blob id), signed downloads that count. Archive requests are recorded
per (tree, format, prefix); `GetSourceArchiveUrl` needs read access to every path of the repo
(the root and each policy access rule), since an archive carries the whole tree. The server
serves the signed URL (`/archives/<repo>/<tree hex>/<prefix>.<tar.gz|zip>`): a built archive
from the blob store (`x-bee-archive-cache: hit`), otherwise streamed entry by entry while it
is written from the tree (`miss`), then kept under the same key (up to 256 MiB) and marked
built. Bytes are a pure function of tree, prefix and format (`bee_server::source_archive`:
path order, 1980-01-01 timestamps, ustar with PAX long names, zip with Unix modes). GitHub's
`/tarball/{ref}` and `/zipball/{ref}` redirect to it. Attestations stay `NOT_FOUND` / `attestation_pending` until recorded.

## 6. Notifications

Threads per principal keyed by subject (`<kind>:<repo>:<id>`); new activity marks unread and
undone. Fan-out: the dispatcher reads each org log by cursor and queues jobs (cursor and jobs in
one transaction); a bounded pool processes jobs, each in one transaction that deletes it; failing
jobs retry and are dropped (audited) after 5 attempts. Recipients: subscribers (implicit through
participation, or manual), repo watchers (`ALL`; `CUSTOM` kinds; `IGNORE` silences), and direct
reasons (review requested, mention, assign, answered comment) that override unsubscribe and
ignore. Never the actor (unless `include_own_activity`), never without read access. Event shapes
consumed: typed `cr.*` and `review.*`; JSON for `issue.*`, `issue_comment.created`,
`discussion.*`, `discussion_comment.*`, `release.published`, generic `security_alert.*`,
`advisory.*`, `agent_run.*`, `workflow_run.*` through `Event.subject`. Email: per reason
preference, org routes, verified addresses only, `reply+<token>@<reply domain>` tokens (30 days),
retry with doubling backoff, failed after 5. Reminders: days, `HH:MM` times, time zone (built-in
table with US, EU, AU and NZ daylight rules, or fixed offsets), digest of waiting review
requests; real-time alerts on new review requests. The inbox stream replays a snapshot, then the
per-principal feed from a cursor, with heartbeats.

## 7. Users, insights, sponsors

Users: profile created from the directory on first use; rename keeps a 90-day redirect; delete
hides at once and purges after 90 days; emails verify by token, are unique when verified, map
commits (verified, noreply `<id>+<login>@<domain>`, placeholders until claimed), privacy flags
and `commit_email_allowed`; settings validated; exports are request records for a worker.
Served elsewhere (`UNIMPLEMENTED` / `served_elsewhere`): SSH authentication keys and sessions
(identity).

Signing keys (`community/keys.rs`): GPG keys are ASCII-armored v4 public keys (key id = low 8
bytes of the v4 fingerprint; subkey ids listed; user-id emails kept only when verified on the
account), SSH keys are OpenSSH lines whose blob must carry their own type (key id = the
`SHA256:` fingerprint). A key id belongs to one account (`ALREADY_EXISTS`); at most 50 keys per
account; anyone may list a user's keys; `SigningKeys::owner_of` resolves a primary, subkey or
fingerprint id to its owner for signature verification.

Signature verification (`community/sigverify.rs`, `SigningKeys::verify_signature`, F49-004):
OpenPGP v4 signature packets (types 0x00 and 0x01; SHA-256/384/512; RSA ≥ 2048 bits, EdDSA and
Ed25519, ECDSA P-256/SHA-256 and P-384/SHA-384; issuer from the issuer-fingerprint or issuer
subpacket, matching the primary key or a subkey) and SSHSIG blobs (namespace `git`, `sha256` or
`sha512`; `ssh-ed25519`, `ecdsa-sha2-nistp256/384`, `ssh-rsa` with `rsa-sha2-256/512`; the
embedded key must be the registered key), all with `ring`. The verdict is
`SecuritySignatureReason` (`VALID` with the key's owner as signer, `UNKNOWN_KEY`,
`BAD_SIGNATURE`, `EXPIRED_KEY`, `MALFORMED`). The signer is the key's owner; the commit's author
and committer fields are not compared with it. An annotation made through `TagVcs::create`
signs its message bytes.

Activity (`community/activity.rs`) is derived from the event logs: a background pass reads every
org log and the system log from its own cursors and indexes events by actor (`u:<principal>`)
and repo news by repo (`r:<repo>`). Contribution classes: `ref.updated` on a branch
(changesets), `cr.opened`, `review.submitted`, `issue.opened`; `cr.landed` also credits the
CR's author for achievements. The contribution graph (default: the year ending today in the
requested zone, at most 371 days, levels 0..4 relative to the busiest day) counts only repos
the caller can read, plus others anonymously when the user set `show_private_contributions`
(`includes_private`). The overview gives class shares and the top five repos and orgs, readable
repos only. The home feed merges the activity of followed users with the news (`cr.landed`,
`release.published`, `discussion.created`) of starred repos, newest first, one entry per
event, without the caller's own actions or rows of repos it cannot read. Achievements count
classes over all time in readable repos, with base/bronze/silver/gold thresholds; the oldest
qualifying row is `unlocked_by`.

Insights (read): languages, license, community profile, citation, pulse, contributors, commit
activity, code frequency, network graph, agent stats. Traffic (needs `commit`): 14-day counters
with hashed unique visitors; no event or audit entry per hit.

Sponsors: profile, tiers (draft, published, retired), sponsorships (pending, active, cancelling,
ended, failed), sponsor-only repo access through an `AccessSource`, payouts, fiscal host,
`FUNDING.yml` links with warnings. Private sponsorships are visible to their parties only.

## 9. Posts and feeds

A change request carries at most one **post**: a short public note about the change, shown to
the people and agents who follow its repo or its author (`crate::posts`, `PostService`; protos in
`posts.proto`). Decisions: a post belongs to its change request, never stands alone; agent posts
publish only after a person approves; following a repo is separate from stars and watches;
"For you" is on hold, but the `post.*` events it would rank on are recorded now.

**Storage.** Every posts table lives on the `changes` surface (`posts::M1`, listed as
`changes/0004_create_posts`), so landing, closing and reopening a change request update its post
in the same transaction. Tables: posts (scope repo, `k1` author, `k2` change request, unique;
`n1` the publication time, set only while published), post ids (post → repo), media (`k1`
change request, `k2` the post it is attached to, `n1` since when it is unattached), likes,
reposts (scope reposter, `k1` post) and counters. Repo follows are on the `users` surface
(`forge_repo_follows`: scope follower, `k1` repo, `n1` time; counters `repo_followers`,
`followed_repos`). The posts module owns the `post.` audit prefix and the event kinds
`post.drafted`, `post.approval_requested`, `post.published`, `post.withdrawn`, `post.liked`,
`post.reposted` (`Event.body.post`, subject `SUBJECT_KIND_POST`) and `post.adopted` (below);
repo follows add
`follow.repo_added` and `follow.repo_removed`.

**Text and media.**

- Text is stored as NFC, at most 500 Unicode scalar values, with no control character but
  `\n` (`text_too_long`, `bad_text`); it may be empty only when the post has media
  (`empty_post`).
- Media is uploaded with `BeginPostMediaUpload` (declared kind, content type and size), a
  `PUT` of the bytes to the signed URL it returns (`/posts/<repo>/<media>`, 15 minutes), and
  `FinalizePostMedia` with the SHA-256 the client computed. The server sniffs the bytes (png,
  jpeg, gif, webp, mp4 with an `ftyp` box, webm; never SVG, HTML or JavaScript; `bad_media`),
  computes their SHA-256 and reads image dimensions itself; a digest or size that differs from
  what arrived is `INTEGRITY`, and an expired upload `INVALID_STATE` / `upload_expired`. Images
  need alt text of 1–1000 characters (`alt_text_required`).
- A post carries at most four images (8 MiB each) or exactly one video (64 MiB, no images
  beside it), all finalized, all uploaded for the same change request (`bad_media`,
  `media_not_final`). Media never attached, or removed from a draft, is deleted after 24 hours.
  The shared upload code is `crate::media` (limits as data, sniffing, the Begin/PUT/Finalize
  flow, signed URLs); `docs/spec/server.md` §8.8 lists the limits.

**Who may write.** `SetChangePost`, `ClearChangePost` and the media upload need the right to edit
the change request: its author (with read) or `commit` on the repo, on a repo that is not
archived, and a post that is `DRAFT` or `PENDING_APPROVAL` (or none yet on an open change;
`post_not_editable`). The post's author is whoever set its text last. Setting the text clears an
earlier approval (it approved other words); a person who rewrites a post waiting for approval
becomes its author and it publishes then.

**Lifecycle.**

| from | event | to |
|---|---|---|
| none | `SetChangePost` (or `OpenChange.post_text`) on an open change request | `DRAFT` (`post.drafted`) |
| `DRAFT` by a person, or approved | the change lands | `PUBLISHED`, `published_at` = the landing time (`post.published`) |
| `DRAFT` by an agent or service, not approved | the change lands | `PENDING_APPROVAL` (`post.approval_requested`, for the approver's Needs you) |
| `DRAFT` | `ApprovePost` | `DRAFT` with `approved_by` |
| `PENDING_APPROVAL` | `ApprovePost` | `PUBLISHED`, `published_at` = now |
| `DRAFT` | the change is closed without landing | `DISCARDED` |
| `DISCARDED` | the change is reopened | `DRAFT` |
| `PENDING_APPROVAL`, `PUBLISHED` | `WithdrawPost` | `WITHDRAWN` (`post.withdrawn`; media deleted) |

`ApprovePost` needs a human (`human_required` otherwise) with `commit` who is not the post's
author (`own_post`); a person's post needs none (`approval_not_needed`). `WithdrawPost` is for
the post's author or a repo admin; withdrawing again is a no-op, a draft is cleared instead
(`post_not_published`). A withdrawn post keeps its permalink, served without text and media and
with `withdrawn_reason`.

**Reads and visibility.** Every read checks repo `read` for the caller: a post on a private repo
is `NOT_FOUND` to anyone who cannot read the repo, and a post on a deleted repo to everyone.
`GetPost` and `ListRepoPosts` (published posts, newest first, cursor `"<ms>|<post id>"`) also
answer callers without credentials on public repos (`docs/spec/server.md` §2).
`ChangeRequest.post` is filled on `GetChange`, `GetChangeByNumber`, `ListChanges` and
`OpenChange`. Each read fills the post's counts (likes, reposts, the change request's comments),
what the caller did and may do (`PostViewer`), its version, and a media URL per item signed for
the caller's principal chain (empty for a signed-out caller). The server serves those URLs
itself and decides repo `read` again for that chain whenever one is redeemed (SPEC §9.6), so a
URL stops working as soon as its reader loses access; responses carry the sniffed type,
`nosniff`, `Content-Security-Policy: sandbox; default-src 'none'` and honour `Range`.

**Engagement.** `LikePost`, `RepostPost` and their undo need an account (agents included) and a
published post (`post_not_published`); doing it twice is a no-op. Counters are kept per post.

**Following.** `FollowRepo`, `UnfollowRepo`, `IsFollowingRepo`, `ListFollowedRepos` (a user named
by login, an agent by principal id; only repos the caller can read) and `ListRepoFollowers` work
for people and agents alike; following needs repo `read`, unfollowing does not. A repo's creator
follows it when `repo.created` is relayed (`community::users::AutoFollow`). Following people
reuses `FollowUser`.

**The Following feed** (`ListFollowingFeed`, `posts/feed.rs`) reads up to 1000 followed repos and
1000 followed principals: the published posts of each repo, the posts each principal wrote and
the posts each reposted (`FeedItem.reposted_by`). A post appears once, under its newest reason
(a repost beats its publication at the same millisecond; among reposts the smallest reposter
id), ordered by `at` then post id, newest first; the page token is the cursor
`"<at_ms>|<post_id>"`, and a malformed one is `INVALID_ARGUMENT` / `bad_page_token`. The sources
are merge-scanned: each scan is bounded by the cursor, items beyond every truncated scan's
frontier are emitted, and a round without progress doubles the scan size. Every item is checked
for repo `read`; only `PUBLISHED` posts appear. With `after` set the feed returns only newer
items, oldest first (page size still caps them), and `newest_cursor` is the cursor of the newest
item returned, or `after` itself when nothing is newer, so an agent polls by passing it back. On
an empty first read (no `after`, no items) `newest_cursor` is `"<server now ms>|0"`, a position
before any post published from now on, so the first poll is never left without one (the clock
is read here only; no id depends on it). The id part of a cursor may be empty (`"<ms>|"`, before
every post of that millisecond). Every `Post.repo` the service returns (feed, `GetPost`,
`ListRepoPosts`, a change request's post) is the repo's `full_name`.
The feed needs an account; agents read it with their own tokens.

**Adoptions ("Inspired by")** (`posts/adopt.rs`). `CreateIssueRequest.inspired_by_post_id` and
`OpenChangeRequest.inspired_by_post_id` name the post that inspired an issue or change request.
The post must be `PUBLISHED`, its repo live, and every principal of the caller's chain must be
able to read that repo (it is public, or each holds a role there); anything else is
`INVALID_ARGUMENT` / `post_not_readable`, the same answer for a missing, unpublished or
unreadable post, and nothing is opened. The token's repo restriction is not consulted: a link
reveals nothing its principals cannot read, and an agent run's token is limited to the repo it
works in. The issue's or change request's own transaction writes the adoption row on its surface
(issue adoptions on the `work` surface, `posts::M_WORK` listed as `work/0002`; change adoptions
on `changes`, `posts::M2` listed as `changes/0005`; scope `""`, id the subject, `k1` the post,
`n1` the time) and emits `post.adopted` in the adopting repo (subject the issue or change
request; payload the post id, the subject kind and number, never the source's text), audited as
`post.adopt`. An issue's row follows it when it is transferred and is deleted with it.

- **`inspired_by`** (`PostRef`: post id, the post's repo by full name, change number and title) is
  filled per caller on every `IssueService` reply that carries issues and on `GetChange`,
  `GetChangeByNumber`, `ListChanges` and `OpenChange`; it stays empty when the caller cannot
  read the post's repo. Events and webhooks never carry it.
- **`PostCounts.adoptions`** counts the adoptions in repos the caller can read (a signed-out
  caller: public repos only), so a private repo's adoption never shows.
- **`ListPostAdoptions`** needs an account that reads the post's repo and lists, newest first
  (then subject id), the adoptions in repos the caller reads, with the subject's current number
  and title; the page token is `"<ms>|<subject id>"` (`bad_page_token` otherwise). At most 1000
  adoptions of each kind are read per post.

Agents adopt automatically through feed triggers (`docs/spec/agents.md` §5).

## 8. API gaps

- `CommentService` is generic across domains. Community exposes `Discussions::{get_comment,
  edit_comment, delete_comment, comment_edits}` and `Reactions`; the router that serves it by
  comment id across discussions, issues and change requests is `service::comments`
  (`docs/spec/operations.md` §10).
- `ReleaseAsset` has no blob id; the SHA-256 is the blob-store content id.
- `Discussion` labels are free names; there is no link to the repo's label table.
- No RPC sets a sponsor fiscal host or records payouts and payments (store APIs for workers).
- `PrincipalDirectory` is read-only: renames live in `Users::resolve_login`; the server's
  directory must consult it.
- No default discussion categories are created when discussions are enabled.
- Team reminders digest the creator's inbox restricted to the org; team membership belongs to
  the enterprise module.
- `ReleaseAttestation.bundle` needs a signing worker; there is no RPC to submit one.
- Time zones outside the built-in table are refused (`unknown_time_zone`).
- `NotificationThread` has no org field; the stream resolves it from the stored thread.
- Inherited community files (org `.github` repo) are not reported (`CommunityFile.inherited`).
