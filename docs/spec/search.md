# Search (code search and query languages)

Status: implemented in `crates/bee-search` (layer 4). Scope: PRODUCT §4.4 code search
(regex, symbols, path and language qualifiers, cross-repo, honouring path permissions), code
navigation (search-based), the GitHub-compatible query languages for issues, change requests,
repos, changesets, users, topics, discussions and packages, and saved-search validation. The
wire surface is `bee.v1.SearchService` (`proto/bee/v1/search.proto`); this crate provides
the logic the server maps onto it.

Everything here is deterministic: indexing, ranking, pagination and parsing are pure
functions of their inputs. No clock, randomness or hash-map iteration order reaches an
output; `@today` and `@me` are resolved from values the caller passes.

## 1. Architecture

| Piece | Type | Role |
|---|---|---|
| Index store | `IndexStore` (`MemoryIndexStore`, `DiskIndexStore`) | Documents, posting lists, manifests |
| Indexer | `Indexer` | Brings a `(repo, ref)` manifest to a changeset from `MetaStore` + `BlobStore` |
| Searcher | `Searcher` | Code, symbol and reference search; outlines; suggestions |
| Code grammar | `parse_code_query` | Typed AST for code and symbol queries |
| Forge grammars | `parse_forge_query`, `ForgeQuery::to_filter` | Typed AST, validator and normalized filter |
| Parse API | `parse_search_query` | `ParseSearchQuery`: qualifiers with positions, errors, normalized form |
| Saved searches | `validate_saved_search` | Name and query checks; returns the normalized query to store |

Search is never a dependency of a list view (PRODUCT §15.1). The forge grammars only parse
and validate; the forge translates `Filter` into queries over its own tables, so issue and
change request lists keep working with the code index absent, stale or rebuilding. The code
index is derived data: it can be dropped and rebuilt from the stores at any time.

## 2. The code index

### 2.1 Documents and dedup

A **document** is one blob of one org, keyed by its content-addressed id (SPEC §6.1 dedup):
a blob that appears in many repos, refs or paths of an org is fetched and indexed once.
Documents carry no path. A document's content is indexed unless it is larger than 1 MiB
(`MAX_INDEXED_BYTES`, `skip = too_large`, the bytes are not fetched) or has a NUL in its
first 8 KiB (`skip = binary`). Skipped documents are still recorded so they are never
re-fetched. Document numbers (`DocNum`, u32) are assigned in input order, never reused.

The index unit is a **trigram**: three consecutive bytes with ASCII letters folded to lower
case. Every window is indexed, newlines included. Folding makes case-insensitive search as
cheap as case-sensitive search; the verifier restores exact semantics.

### 2.2 Manifests

A **manifest** is the indexed view of one `(repo, ref)`: the changeset it reflects and every
file path with `{blob, mode, size, doc, language}`. Symlinks and gitlinks are listed (so
`path:` finds them) with no document. Language comes from the path (extension or well-known
file name such as `Dockerfile`, `Makefile`), else from a shebang (`#!/usr/bin/env python3`).

### 2.3 Incremental indexing

`Indexer::index_ref(org, repo, ref, target)`:

1. Walk first parents from `target` back to the manifest's changeset, at most
   `max_delta_walk` steps (default 1024). If the walk reaches the old changeset, replay the
   walked changesets' deltas onto the old files; if it reaches a root, replay them onto an
   empty tree (a root's delta is its whole tree). Deltas are exact file-level differences
   from the first parent (SPEC §4.4 invariant D), so this is exact for merges too.
2. Otherwise (unrelated history, or a walk too long) list the target's whole tree,
   iteratively.
3. For each path whose blob or mode changed, look the blob up in the org's index and fetch,
   verify (`read_object_bytes`) and index it only if it is new.
4. Write new documents, then the manifest (documents before the row that refers to them).

`IndexStats` reports `paths_changed`, `deltas_replayed`, `blobs_indexed` (fetched and new),
`blobs_reused` (changed paths whose blob was already indexed) and `full_listing`. Re-running
is a no-op. `sync_ref` reads the ref's target from the metadata store; `sync_repo` indexes
every ref under a prefix and removes manifests of deleted refs; `collect_garbage` drops
documents no manifest of the org references (`IndexStore::compact`), keeping the numbers of
survivors.

Rebuilding from scratch yields byte-identical manifests: documents are added in blob-id
order, so numbers are a function of the stores' contents.

### 2.4 The on-disk store

`DiskIndexStore` uses only `std::fs` (no mmap, no embedded database):

```text
orgs/<ORG ULID>/segments/seg-<seq, 16 digits>.seg
orgs/<ORG ULID>/manifests/<REPO ULID>/<hex of ref name>.man
```

A segment holds one `add_documents` batch:

```text
header   "SWSEG001" seq:u64 replaces_through:u64 next_doc:u32
contents raw document bytes, back to back
postings per trigram, LEB128 deltas of ascending doc numbers
footer   ndocs:u32 { doc:u32 blob:34B skip:u8 offset:u64 len:u32 }*
         ntri:u32 { trigram:u32 offset:u64 len:u32 count:u32 }*
trailer  footer_offset:u64 blake3(header ‖ footer):32B "SWSEGEND"
```

Integers are little-endian. A manifest file is `"SWMAN001"`, org and repo ULIDs, the ref
name, the changeset id, then `{path, blob, mode:u8, size:u64, doc:u32 (u32::MAX = none),
language:u16 (0 = none)}` per file, followed by the BLAKE3 digest of everything before it.

Every file is written to `.tmp-<name>`, synced, renamed into place and its directory synced,
so a crash leaves the old state or the new one. Opening reads only headers and footers
(verified by digest; a mismatch is `INTEGRITY`); posting lists and contents are read on
demand. Leftover temporary files are deleted on open. Compaction writes one segment with
`replaces_through` = the last segment it replaces; on open, replaced segments are ignored and
deleted, so a crash during compaction is harmless. I/O failures are `UNAVAILABLE`.

## 3. Code query language

### 3.1 Syntax

Terms are separated by whitespace. Juxtaposition and `AND` are conjunction, `OR` is
disjunction (lower precedence), `NOT` or a leading `-` negates, parentheses group. Operators
are upper case only. A term is:

- a bare word (`charge_card(`), matched literally;
- a `"quoted phrase"` with `\"` and `\\` escapes, matched literally;
- a `/regex/` (Rust `regex` syntax, multi-line mode so `^`/`$` match at lines; `\/` is a
  slash). The closing slash must be followed by whitespace, `)` or the end; otherwise the
  token is a bare word (`/usr/bin` is literal);
- `key:value` for the keys below, where the value is any of the above.

Parenthesis rule: `(` opens a group only at the start of a term; `)` closes one only at the
end of a term while a group is open and the term has no unmatched `(` of its own. So
`foo()` and `charge_card(` stay literal, and `(a OR b)` groups. A lone `()` is an
`empty_group` error; quote it to search for it.

Unknown keys are not qualifiers in code search: `std::io::Read` and `http://x` are literals.
`content:` forces a literal (`content:path:x` searches for the text `path:x`).

| Key | Aliases | Meaning |
|---|---|---|
| (none), `content:` | | File content contains the pattern |
| `path:` | `file:` | Path matches (3.2) |
| `lang:` | `language:` | Detected language (canonical name or alias: `rs`, `py`, `js`, `ts`, `golang`, `cpp`, `c#`, `sh` ...) |
| `repo:` | | `org/name` exactly, `name` in any org, or `/regex/` over `org/name`; case-insensitive |
| `org:` | | Org name, case-insensitive |
| `symbol:` | | The file defines a symbol whose name equals the literal, or matches the `/regex/` (3.6) |
| `case:` | | `yes` makes the whole query case-sensitive (default `no`) |

`case:` is a modifier: it applies to the whole query wherever it appears in a conjunction
(`a OR b case:yes` is fine) and is `misplaced_qualifier` under a negation or as a direct
alternative of `OR`. A query with no condition (only `case:`) is `empty_query`.

### 3.2 Paths

- No glob characters: the value occurs in the path starting at a component boundary.
  `path:billing/` matches `billing/a.rs` and `src/billing/a.rs`, not `rebilling/a.rs`;
  `path:README` matches `docs/README.md`.
- A glob (`*` within a component, `**` across components, `?`, `[...]` with `!` or `^`
  negation): anchored at the end of the path; anchored at the repo root when it starts with
  `/`, otherwise it may start at any component. `path:*.rs` matches Rust files anywhere,
  `path:/src/**` only under the root `src/`.
- `/regex/`: a regex over the whole path.

Paths compare case-insensitively unless `case:yes`.

### 3.3 Planning and verification

Each content and symbol pattern is planned into a trigram formula (`plan_regex`): for every
sub-expression the planner tracks the exact set of strings it can match, or a query plus the
sets of possible prefixes and suffixes, combining them bottom-up (concatenation crosses
suffixes with prefixes to find trigrams that straddle the boundary). It gives up
information, never correctness, whenever a set exceeds 64 strings, a class exceeds 16
characters, or a repetition can be empty. Negated terms and metadata terms (`path:`,
`lang:`, `repo:`, `org:`) plan to "all" for the content index. `path:` terms are also
planned the same way, over each pattern's regex form, for the **path index**
(`bee_search::pathindex`): folded trigram postings over the paths of one manifest, built on
first use and kept per (repo, ref, changeset). The path formula selects candidate files before
any other work, and each candidate is still verified by the real path matcher, so results
equal a scan of every path. The formula selects candidate documents from the
posting lists; every candidate file is then verified by running the real regex. Literals go
through the same path (escaped), so Unicode case folding (`ſ` for `s`) is planned soundly.

The property test `index_equals_brute_force` checks, over generated corpora, that results
(files, match counts and lines) equal a scan of every file.

### 3.4 Results and ranking

A hit is one file: repo, path, ref, changeset, blob, language, line matches (line number,
first match column, line text cut at 500 bytes on a character boundary, match ranges, up to
10 context lines before and after), `match_count`, `path_matched` (a positive `path:` term
matched) and matched symbols. Matches that span lines are reported on their first line.
Line text of invalid UTF-8 is converted lossily (ranges are byte offsets into the raw line).

Rank: the best match kind in the file, then shallower path depth:

1. exact text of a query literal bounded by word boundaries (symbol definitions count here);
2. any whole-word match (case-folded literal, or a regex match on word boundaries);
3. exact text of a literal inside a longer word;
4. any other match.

`score = kind × 1000 − depth`. Ties break by repo full name, ref and path, so the order is
total. Metadata-only queries (`path:*.rs lang:rust`) list files with kind 0.

Facets (`include_facets`) count matching files per language and per repo, by count
descending then value.

### 3.5 Pagination

A page token is the hex encoding of the sort key of the last hit returned (score, repo, ref,
path) plus an 8-byte fingerprint of the normalized query and case flag. The next page starts
strictly after that key, so paging is stable: no hit is repeated or skipped when it exists on
both requests, and hits that disappear do not shift later pages. A token from another query
or a malformed token is `INVALID_ARGUMENT` (subcode `bad_page_token`). Page size defaults to
25 and is capped at 100. `stream_code` returns every hit in rank order without paging and
stops after `max_matches` matches (`limit_hit`).

Targets whose ref has no manifest yet are listed in `skipped_repos` and set `incomplete`.

### 3.6 Symbols and navigation

Symbols come from a lightweight tags extractor: per-language line regexes for Rust, Go,
Python, JavaScript and TypeScript, Java, C#, Kotlin, Scala, C, C++, Objective-C, Ruby, PHP,
Swift and shell. Kinds mirror `bee.v1.SymbolKind`. Limits:

- A definition must start on one line (`fn name`, `def name`, `class Name` ...); names split
  across lines or produced by macros and code generation are missed.
- Containers (impl, class, module ...) are tracked by indentation: a method is attributed to
  the nearest less-indented container above it. Badly indented code gets the wrong container,
  never a wrong name.
- Definitions inside comments or strings that look like code are reported.
- C and C++ functions are recognized only at column 0 with the `(` on the same line and no
  `;` after it.
- Symbols are extracted at query time from candidate files (the name must occur in the
  content, so the trigram plan applies); they are not stored.

APIs: `search_symbols` (bare terms name symbols; `kinds` filter; paged like code search),
`definitions(name)` (exact, case-sensitive), `references(name, include_declaration, filter)`
(whole-word, case-sensitive occurrences; definitions excluded unless asked; `filter` is an
optional code query), `document_symbols(target, path)` (outline; `NOT_FOUND` when absent or
unreadable, indistinguishable). Results are `SymbolLocation`s with `precise = false` on the
wire (search-based).

### 3.7 Suggestions

`suggest(prefix, limit)` returns, case-insensitively: repos whose name or full name starts
with the prefix; readable paths whose file name starts with it; and, for prefixes of three
bytes or more, names of symbols in readable files that start with it. Ordered by kind then
text.

### 3.8 Errors

Query errors are `INVALID_ARGUMENT` with payload `subcode = bad_query` and `errors`, a list
of `{subcode, message, start, end}` (byte offsets). A regex whose compiled program exceeds
1 MiB is `TOO_LARGE` (`limit = regex_size`). Subcodes: `unbalanced_paren`, `empty_group`,
`dangling_operator`, `unterminated_quote`, `bad_regex`, `regex_too_large`, `bad_value`,
`unknown_language`, `misplaced_qualifier`, `duplicate_qualifier`, `empty_query`, and for
forge grammars `unknown_qualifier`, `wrong_kind`, `bad_date`.

## 4. Permissions

Every file passes the caller's `PathAuthorizer::can_read(repo, path)` (SPEC §9.3, the
`object_paths` rule) before its content is read, matched, counted or ranked. The caller
passes as targets only repos it may see at all. Consequences:

- hits, `total_count`, facets, previews and context lines only ever come from readable files;
- symbol search, definitions, references, outlines and suggestions apply the same check;
- dedup never leaks: a document shared by a readable and an unreadable path is reported only
  through the readable `(repo, path)`;
- an unreadable file is `NOT_FOUND`, exactly like an absent one.

`tests/permissions.rs` runs a matrix of four principals (admin, path-restricted, more
restricted with a hidden repo, outsider) against ten queries and checks hits, counts,
facets, previews, dedup, symbols, definitions, references, outlines and suggestions.

## 5. Forge query languages

`parse_forge_query(kind, query)` lexes with the shared syntax (3.1, without `/regex/`), but
any `key:value` whose key is not a qualifier is `unknown_qualifier` (free text containing a
colon must be quoted, `"error: x"`; a colon followed by a space is plain text). Keys and enum
values are case-insensitive; the normalized form uses canonical keys, lower-case words and
puts `in:` and `sort:` last. `sort:` and `in:` are modifiers (same placement rule as
`case:`), at most once each.

### 5.1 Value types

| Type | Accepted |
|---|---|
| text | any non-empty value (quote phrases) |
| principal | a login, or `@me` (resolved from `FilterContext.me`) |
| enum | one of the listed words |
| bool | `true`/`false` (`yes`/`no`) |
| number | `N`, `>N`, `>=N`, `<N`, `<=N`, `N..M`, `N..*`, `*..M` |
| date | the number forms over `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM[:SS][Z|±HH:MM]`, `@today`, `@today-Nd` |
| list | comma-separated enum words |

Dates without a time are whole UTC days: `created:2024-01-01` is that day, `>D` starts after
it ends, `<=D` includes it. Instants are normalized to UTC (`...T10:00+02:00` → `...T08:00:00Z`).
`@today` needs `FilterContext.today`; without it `to_filter` is `INVALID_ARGUMENT`.

### 5.2 Issues and change requests

| Qualifier | Type | Filter |
|---|---|---|
| `is:` | open, closed, merged, unmerged, draft, issue, pr, cr, locked, unlocked, public, private, archived, queued | `state`, `type` (`pr` = `cr`), `draft`, `locked`, `visibility`, `archived`, `queued`; `unmerged` = `type:cr AND NOT state:merged` |
| `state:` | open, closed, merged | `Enum state` |
| `type:` | issue, pr, cr | `Enum type` |
| `author:` `assignee:` `commenter:` `involves:` `mentions:` `reviewed-by:` `review-requested:` | principal | `Person{role, login}` |
| `team-review-requested:` `team:` | text (`org/team`) | `Name` |
| `label:` `milestone:` `project:` | text | `Name` |
| `review:` | none, required, approved, changes_requested | `Enum review` |
| `status:` | success, failure, pending | `Enum status` |
| `in:` | title, body, comments | text fields (default title, body) |
| `created:` `updated:` `closed:` `merged:` | date | `Range` (unix ms) |
| `comments:` `reactions:` `interactions:` | number | `Range` |
| `no:` | label, milestone, assignee, project, reviewer | `Missing` |
| `head:` `base:` | text | `Name` |
| `path:` (Beehive) | text | `PathPrefix` (leading `/` dropped): the CR touches paths at or under it |
| `risk:` (Beehive) | a diff class (SPEC §11.3) or a comparison over the risk order (`>=logic`, `docs..test_only`) | `Range` over the class index |
| `agent:` (Beehive) | `true`, `false` or an agent login | `AgentAuthor` |
| `repo:` `org:` `user:` `language:` (`lang:`) | text | `Name` |
| `linked:` | pr, cr, issue | `Enum linked` |
| `reason:` | completed, not-planned, duplicate | `Enum reason` |
| `archived:` `draft:` | bool | `Flag` |
| `sort:` | created, updated, comments, reactions, interactions, best-match, each with optional `-asc`/`-desc` (default desc) | `SortSpec` |

### 5.3 Other kinds

| Kind | Qualifiers |
|---|---|
| repos | `is:` public, private, internal, fork, template, archived, mirror, sponsorable; `topic:` `language:` `license:` `repo:` `org:` `user:` text; `stars:` `forks:` `size:` `topics:` `followers:` `good-first-issues:` `help-wanted-issues:` number; `created:` `pushed:` date; `archived:` `mirror:` `template:` bool; `fork:` true, false, only; `in:` name, description, readme, topics; `sort:` stars, forks, updated, help-wanted-issues, best-match |
| changesets | `author:` `committer:` principal; `author-name:` `committer-name:` `author-email:` `committer-email:` `hash:` `parent:` `tree:` `change-id:` `repo:` `org:` `user:` text; `author-date:` `committer-date:` date; `merge:` bool; `is:` public, private; `risk:`; `agent:`; `sort:` author-date, committer-date, best-match |
| users | `type:` user, org, agent; `in:` login, name, email; `repos:` `followers:` number; `location:` `language:` text; `created:` date; `is:` sponsorable; `sort:` followers, repositories, joined, best-match |
| topics | `is:` featured, curated; `repositories:` number; `created:` date |
| discussions | `category:` `label:` `repo:` `org:` `user:` text; `author:` `commenter:` `involves:` principal; `is:` answered, unanswered, open, closed, locked, unlocked; `created:` `updated:` date; `comments:` number; `in:` title, body, comments; `sort:` created, updated, comments, best-match |
| packages | `type:` npm, container, maven, rubygems, nuget, cargo, pypi, go; `repo:` `org:` `user:` text; `sort:` downloads, updated, best-match |

A qualifier valid for another kind is `wrong_kind`. Free text applies to the kind's default
fields (issues and discussions: title, body; repos, topics, packages: name, description;
changesets: message; users: login, name) unless `in:` says otherwise.

### 5.4 The filter

`ForgeQuery::to_filter(&FilterContext)` returns `CompiledFilter {kind, filter, sort}` where
`filter` is a boolean tree (`All`, `Pred`, `Not`, `And`, `Or`) of predicates: `Text{text,
fields}`, `Person{role, login}`, `AgentAuthor{is_agent, login}`, `Name{field, value}`
(case-insensitive equality), `PathPrefix`, `Enum{field, value}`, `Flag{field, value}`,
`Range{field, min, max}` (numbers; dates as unix ms; risk as class index) and
`Missing(field)`. Without `sort:` the sort is `best-match` descending. The forge maps each
predicate to its tables; any predicate it cannot serve is its own `INVALID_ARGUMENT`.

Example (AS-SEARCH-02): `is:open is:cr review:required path:billing/` →
`And[Enum state=open, Enum type=cr, Enum review=required, PathPrefix "billing/"]`.

## 6. Saved searches

`validate_saved_search(kind, name, query)` checks a non-blank name of at most 100 bytes with
no control characters (`INVALID_ARGUMENT`, subcode `bad_name`; `TOO_LARGE` when longer) and a
query of at most 1024 bytes that parses for its kind (`bad_query`), and returns the
normalized query. Storage, per-user ownership and `ALREADY_EXISTS` for taken names are the
forge's.

## 7. Performance

`tests/perf.rs` indexes 50,000 generated Rust files (67.6 MB, 10 repos) into the in-memory
store and runs 50 selective queries (literals, regexes, `path:`, `repo:`, `lang:`,
`symbol:`, `OR`). Measured in a debug build on a laptop: indexing 23.9 s; query latency p50
6.6 ms, p95 27.6 ms, max 32 ms. Queries whose plan
is "all" (every literal shorter than three bytes, negation-only queries, metadata-only
queries) scan every readable file of the targets.
