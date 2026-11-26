# Forge issues and projects (`bee-forge::work`)

Issues, planning objects and Projects v2. Contract: SPEC §11.8, §17.3; PRODUCT §4.15.
Conventions: `docs/spec/forge-core.md` (one transaction per store method, `emit_event` +
`audit`, `ForgeError`, service shape). Serves `IssueService` (67 RPCs) and `ProjectService`
(50 RPCs). `CommentService` belongs to the community module; the work store exposes the
issue-side operations it needs (see §8).

## 1. Layout

| path | owns |
|---|---|
| `work/mod.rs` | `MODULE` (surface `work`, 28 tables, migration `work/0001`), event kinds, `Work`, hooks, number space |
| `work/issues.rs` | issues, comments, edit history, reactions, timeline, close/reopen/duplicate, lock, pin, transfer, import |
| `work/labels.rs`, `fields.rs`, `relations.rs`, `assign.rs`, `saved.rs` | labels and milestones; org types and fields; sub-issues, dependencies, development links; assignees and agent runs; saved replies |
| `work/templates.rs`, `text.rs`, `search.rs` | templates and YAML forms; task lists, closing keywords, mentions, references; issue list filters |
| `work/projects.rs`, `items.rs`, `views.rs`, `automation.rs`, `insights.rs`, `pfilter.rs` | projects; items; fields, iterations, views; workflows; charts; the project filter language |
| `work/listener.rs` | `WorkListener`: reacts to `cr.*` and `review.submitted` events |
| `service/issues.rs`, `projects.rs`, `work.rs` | the tonic services and their shared access helpers |

## 2. One number space per repo

Issues and change requests share one counter per repo (GitHub semantics; SPEC §17.3 keeps
imported numbers). The counter and the registry live in the `work` surface: table
`forge_work_numbers` (scope repo, id = zero-padded number, body = kind and subject id) and a
counter row. Everything that takes a number goes through `Work`:

- `allocate_number(cx, repo, kind, subject_id)`: next number (own transaction). The changes
  module calls it with `NumberKind::ChangeRequest` before it inserts the CR; a failed insert
  leaves a gap, never a collision. Issues call the same function inside their transaction.
- `reserve_number(cx, repo, n, kind, subject_id)`: claims `n` (imports) and moves the counter
  past it; `ALREADY_EXISTS` if taken.
- Deleted issues leave a `Retired` entry (numbers are never reused); a transferred issue
  leaves a `Redirect` entry to its new repo and number.
- `locate_change(cr_id)` finds a CR's repo and number through the registry.

Test: `work_store.rs` allocates issues and CR numbers concurrently on memory and SQLite
(and Postgres) and asserts no collision and no gap.

## 3. Authorization

| call | needs |
|---|---|
| reads (get, list, timeline, templates, search) | `read` |
| open an issue, comment, edit or close one's own issue | `read` (reader role); planning fields at creation need triage |
| edit or close someone else's issue, labels, milestones, assignees, type, fields, lock, pin, sub-issues, dependencies, links | `triage` (write role) |
| transfer, create issue branch | `commit` (write role), on both repos for a transfer |
| delete an issue | `manage_settings` and repo admin |
| issue types and fields (org level) | org owner or admin |
| saved replies | any token that allows `read`; per user |
| projects: read | org member, or anyone for a public project |
| projects: items, fields, views, charts, status updates | org member with a token that allows `triage` and is not repo-restricted |
| projects: settings, workflows, links, delete | org owner/admin, or the project's creator |
| user-owned projects | the owner writes; others read when public |

Hidden things answer `NOT_FOUND`; denied state changes are audited through `guarded`. Archived
repos answer `ARCHIVED`, repos with issues disabled `LOCKED`. Project items whose content the
caller cannot read are returned as `PROJECT_ITEM_KIND_REDACTED` placeholders.

## 4. Issues

- Records: crate-private `IssueRec` (row k1 = zero-padded number, k2 = org id, n1 = updated).
  Labels, milestone counters, project item snapshots and reaction counts are maintained in
  the same transaction (`save_issue`).
- Close reasons completed / not planned / duplicate (`duplicate_of` required, never itself);
  reopening clears the duplicate mark. Locking blocks non-triagers from commenting (`LOCKED`).
- Transfer: same org only; new number in the target, `Redirect` in the source, labels and
  milestone mapped by name (labels created with `create_labels`), pin dropped, events in both
  repo logs. `GetIssueByNumber` follows the redirect; the service then requires `read` on the
  new home.
- Sub-issues: one parent, cross-repo within an org, 100 children, 8 levels, cycles rejected,
  ordered with gap positions; progress counts direct children and the percentage covers all
  descendants. Dependencies: 50 per side, cycles rejected.
- Task lists, closing keywords (`fixes #n`, `org/repo#n`), mentions and `#n` references are
  parsed from Markdown outside code. Mentions and cross-references become timeline events
  (references only when the target is readable and the source repo is public or same-org).
- Custom fields: text, number, date, single-select, iteration; org-level, optionally limited to
  issue types; values are validated by kind and option; removing an option, a type or a field
  clears what referenced it.
- Search: `ListIssues` compiles `query` with `bee-search` (`parse_forge_query`) and evaluates
  the filter over the repo's issue rows plus the few side tables a query needs (comments for
  `in:comments`, project items for `project:`). Qualifiers that only make sense for change
  requests (`is:pr`, `review:`, `path:` ...) match nothing. No search index is involved. Page
  tokens are keyset (`sort value:number`).
- Templates: `TemplateSource::read_dir(repo, dir)` returns files of `.beehive/ISSUE_TEMPLATE`
  (else `.github/ISSUE_TEMPLATE`) on the default ref; the server backs it with the VCS. Forms
  (YAML) are parsed with the workflow YAML reader and rendered into `### Label` sections;
  broken files appear with `parse_error` and cannot be used.
- Default labels (GitHub's nine) are seeded once per repo on first label use.

## 5. Agents (PRODUCT §4.15 done-when)

Assigning an agent principal calls `AgentGate::admit` (default: every agent takes work;
`BUDGET_EXHAUSTED` aborts before any change). Each started run gets an id stored on the issue,
a timeline entry, and an **`issue.assigned_to_agent`** event whose JSON payload carries
`repo_id`, `repo_full_name`, `issue_id`, `number`, `title`, `body`, `agent`, `agent_run_id`,
`instructions`, `base_ref`: the agent runner consumes it. When the agent's change request opens
(`cr.opened` with a closing keyword or `linked_issues`), the issue gets a development link
(`AGENT_RUN` source, `closes`); landing it on the default ref closes the issue (reason
completed, timeline entry with the CR id) and ends the run (`issue.agent_run_finished`).
`Work::finish_agent_run` lets the runner end a run itself.

## 6. Projects

- Every project has built-in fields (Title, Assignees, Labels, Milestone, Repository, Linked
  changes, Reviewers, Parent issue, Sub-issues progress, Type: derived, read-only), a `Status`
  single-select (Todo, In Progress, Done), ten default workflows (`is_default`: can be disabled,
  not deleted) and a first table view.
- Items: issues, change requests, drafts; unique per content; snapshots of the content are
  refreshed when the issue changes; manual order by gap positions.
- Filter language (`items`, `view_id`, charts, workflows): terms `key:a,"b c"`, `-term`, bare
  words, keys `is no has assignee label milestone repo type title author state reviewers
  parent-issue linked-changes updated` and field names; numbers `>=3 1..5`, dates `>@today-7d`,
  iterations `@current @next @previous`. Unknown keys are `INVALID_ARGUMENT`.
- Workflows run **in the same transaction** as the issue change (closing an issue and moving
  its item commit together); change request events arrive through `WorkListener`. Kinds:
  item added / reopened / closed, change landed / changes requested / approved set a Status
  option; auto-add (linked repo + filter), auto-close (status reached), auto-add sub-issues;
  auto-archive runs in `Work::sweep_projects` (called by `Forge::maintain`).
- Views (table, board, roadmap) save filter, grouping, columns, slicing, sort, visible and
  summed fields, roadmap dates and zoom; ids are validated against the project's fields.
- Insights: current charts bucket items by a field (count, sum, average, min, max over a number
  field, optional series split); historical charts replay item activity (`forge_work_item_log`)
  in at most 366 daily-or-wider points. The chart filter applies to items that still exist.
- Status updates, repo links, team links (recorded as given), copy and templates (copy brings
  fields, views, workflows, charts and, optionally, drafts), public projects.

## 7. Events and audit

Every state change emits an event (`issue.*`, `issue_comment.*`, `label.*`, `milestone.*`,
`issue_type.changed`, `issue_field.changed`, `project.*`, `project_item.changed`,
`project_config.changed`, `project_status_update.changed`) with the resource as proto3-JSON and
writes an audit entry under the prefixes `issue.`, `issue_comment.`, `issue_type.`,
`issue_field.`, `milestone.`, `label.`, `saved_reply.`, `project.`, `project_item.`,
`project_config.`, `project_status_update.`. Imports (`Work::import_issue`) are audited but emit
no events. Saved replies and user-owned projects audit into the `system` chain (no org).

## 8. Hooks and what other modules call

- `WorkHooks` (set with `Work::set_hooks`): `TemplateSource`, `BranchProvisioner`
  (`CreateIssueBranch`; `UNIMPLEMENTED` until the server attaches a VCS), `AgentGate`,
  `ChangeSource` (title and state of CRs for project items).
- Changes module: `allocate_number` (§2). Community module's `CommentService` for issue
  conversations: `comment_location`, `get_comment`, `update_comment`, `delete_comment`,
  `list_comment_edits`, `add_reaction`, `remove_reaction`, `list_reactions`,
  `reaction_subject_repo`.
- Import: `import_issue` (number, timestamps, labels, milestone, comments, lock) and
  `reserve_number`.

## 9. API gaps

- `INTERACTION_LIMITED`: no interaction-limit store exists yet, so it is never returned.
- `body_html` is rendered for `GetIssue`, `GetIssueByNumber` and `ListIssueComments` with
  `include_html`; lists, timelines and search results carry Markdown only.
- `LinkProjectTeam` stores the team as given: the teams module is enterprise's, so team roles
  on projects are not enforced.
- `Project` has no `status` besides the latest status update.
- `ListIssues` evaluates over the repo's rows (bounded by `SCAN_CAP` = 50 000 per call); a
  repo beyond that needs a secondary index.
- Milestone counters count issues only; change requests with milestones belong to changes.
- `ProjectItem` has no content author or reviewers fields; filters use private snapshots.

Closed (2026-10-19): the `issues_write` policy action (opening issues and commenting need it;
every built-in role has it, so tokens decide), sanitized `body_html` (`bee_forge::markdown`),
and `IssueTimelineEvent.mentioned_user` (`assignee` / `current_value` still carry it).
