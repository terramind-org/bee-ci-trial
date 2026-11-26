# diff3-v1

Normative diff and merge algorithm referenced by SPEC §7.3.1. Implemented by `crates/bee-merge`
(plan task W2). This document is frozen once merged: any change to an observable result (a hunk,
a region, a conflict kind, a marker byte) ships as a new algorithm id. The id is the string
`diff3-v1` (`bee_merge::ALGORITHM_ID`).

A rebase or merge result is a pure function of `(inputs, algorithm id)`. No step reads a clock,
draws randomness or depends on hash-map iteration order.

The worked examples in §11 and §12 are golden vectors: `crates/bee-merge/tests/golden.rs` parses
every `example` block in this file and asserts the implementation reproduces it byte for byte.
Examples are added, never edited.

## 1. Line model

- A text is a byte string. It is split after every `\n` byte; the terminator stays with its
  line. The last line has no terminator when the text does not end in `\n`. The empty text has
  zero lines.
- No other byte is special. `\r\n` is two ordinary bytes, so `a\r\n` and `a\n` are different
  lines (no CRLF normalization). No encoding is assumed.
- Two lines are equal iff their bytes (terminator included) are equal. A final line without a
  terminator therefore never equals the same text with one.

Line positions are 0-based everywhere in data (hunks, conflict ranges). Only the unified-diff
header and `DiffLine` line numbers are 1-based.

## 2. Diff

### 2.1 Edit graph

For old lines `a[0..n)` and new lines `b[0..m)`, a point `(x, y)` with `0 <= x <= n`,
`0 <= y <= m` means "`x` old lines and `y` new lines consumed". Moves:

- **delete** (right): `(x, y) → (x+1, y)`, removes `a[x]`;
- **insert** (down): `(x, y) → (x, y+1)`, adds `b[y]`;
- **snake** (diagonal, free): `(x, y) → (x+1, y+1)` when `a[x] == b[y]`.

Diagonal `k = x - y`. A round `d` holds, for each `k` in `-d, -d+2, …, d`, the value `V_d[k]`:
the largest x reachable on diagonal k with exactly d deletes and inserts, after following the
snake as far as it goes, or *unreachable*.

### 2.2 Forward greedy rounds

- Round 0: `V_0[0]` = end of the snake from `(0, 0)`.
- Round `d >= 1`, for each k from `-d` to `d` in steps of 2, two candidate moves:
  - *insert* from diagonal `k+1`: valid iff `k+1 <= d-1`, `V_{d-1}[k+1]` is reachable, and
    `y = V_{d-1}[k+1] - k <= m`. It reaches `x = V_{d-1}[k+1]`.
  - *delete* from diagonal `k-1`: valid iff `k-1 >= -(d-1)`, `V_{d-1}[k-1]` is reachable, and
    `V_{d-1}[k-1] + 1 <= n`. It reaches `x = V_{d-1}[k-1] + 1`.
  - If both are valid, take *insert* iff `V_{d-1}[k-1] < V_{d-1}[k+1]`; otherwise (including the
    tie `V_{d-1}[k-1] == V_{d-1}[k+1]`) take *delete*. **Ties prefer deletion.** If one is
    valid, take it; if neither, the diagonal is unreachable in this round.
  - Follow the snake from the reached point; that is `V_d[k]`.
- The first round `D` in which some diagonal reaches `(n, m)` ends the search. If several
  diagonals do in that round, the lowest k wins (only `k = n - m` can, so this never arises).

`D` is the minimum edit distance (insert + delete only). The path is recovered backwards from
`(n, m)` by replaying, for each round, the same choice that produced the value; the snakes on
that path are the **matched pairs** `(i, j)` with `a[i] == b[j]`.

Two shortcuts are part of the definition because they cannot change the result: when either
side is empty, or when no line occurs in both texts, there are no matched pairs.

**Distance cap.** The search is exact up to `MAX_EDIT_DISTANCE = 16384`. If round 16384 completes
without reaching `(n, m)` (that is, `D > 16384`), the matched pairs are instead the common prefix
`(i, i)` for `i < p`, where `p` is the length of the longest common prefix, and the common suffix
`(n-s+j, m-s+j)` for `j < s`, where `s` is the length of the longest common suffix of
`a[p..n)` and `b[p..m)`. The cap bounds the work an adversarial input can cause (a text up to
`merge_max_text_bytes` can have millions of lines); texts that far apart are rewrites, for which
one large hunk is the useful answer.

Implementations may store the rounds in any way (the reference keeps them in full up to 2²¹
entries, beyond that it keeps every ⌊√D⌋+1-th round and recomputes blocks while backtracking).
Only the matched pairs are observable.

### 2.3 Hunks

The gaps between consecutive matched pairs (and before the first and after the last) that are
non-empty on at least one side are the **hunks**, in order:
`{old_start, old_len, new_start, new_len}`. A hunk deletes `a[old_start..old_start+old_len)`
and inserts `b[new_start..new_start+new_len)`; deletions come before insertions within a hunk.
At least one unchanged line separates two hunks. The normalization means the order of moves
inside a gap is unobservable.

### 2.4 Unified rendering

`unified_diff(old, new, context)`:

- Hunks are grouped: hunk `h+1` joins the group of `h` when
  `h+1.old_start - h.old_end <= 2 × context`.
- A group covers `context` unchanged lines before its first hunk and after its last (fewer at
  the start or end of the text).
- Header: `@@ -R +R @@\n` where `R` for a range of `start` (0-based) and `len` is `start,0`
  when `len == 0`, `start+1` when `len == 1`, and `start+1,len` otherwise.
- Lines in order: unchanged lines prefixed with a space, then for each hunk its deleted lines
  prefixed with `-` followed by its inserted lines prefixed with `+`.
- A line without a terminator is followed by `\n\ No newline at end of file\n`.
- No `---`/`+++` file header is produced. Identical texts render as the empty string.

Each rendered line is also available as data (`DiffLine`): an origin (`Context`, `Removed`,
`Added`, shown as ` `, `-`, `+`), its 1-based old and new line numbers (absent on the side it is
not in) and its bytes.

### 2.5 Statistics

`DiffStats {hunks, added, removed}`: the number of hunks of §2.3 (before grouping), the sum of
`new_len` and the sum of `old_len`.

### 2.6 Performance

Time is O((n + m) · min(D, 16384)) and memory is O(n + m + D^1.5). Two unrelated 10,000-line
files (D ≈ 20,000, so the cap applies) take about 0.3 s in a release build. Diffing two 10,000-line
files that differ in 1% of lines takes well under 50 ms in a release build; this is asserted by
`cargo test -p bee-merge --release --test perf` (the debug build runs the same test without
the time bound).

## 3. Three-way line merge

Inputs: base `B`, ours `O`, theirs `T`, each split into lines.

1. Diff `B → O` and `B → T` (§2). Each hunk has a **base range** `[old_start, old_end)`; an
   insertion has an empty base range `[p, p)`.
2. **Touching.** Two hunks (from either side) are connected when their base ranges, taken as
   closed intervals `[old_start, old_end]`, intersect:
   `h1.old_start <= h2.old_end && h2.old_start <= h1.old_end`. So hunks connect when they
   overlap, when they are adjacent (`h1.old_end == h2.old_start`), when an insertion point lies
   at either end of or inside another hunk's base range, and when two insertions are at the same
   point. Hunks of one side are never connected to each other directly (an unchanged line
   separates them) but can be connected through a hunk of the other side.
3. **Regions** are the connected components, in base order. Computed by sorting all hunks by
   `(old_start, old_end)` and sweeping: a hunk joins the current region iff
   `old_start <= region_end`, then `region_end = max(region_end, old_end)`. A region covers base
   range `[rs, re)`, the start of its first hunk to the furthest end.
4. A side's range for a region: if the side has hunks in it, from the first such hunk `f` and
   the last `l`: `[f.new_start - (f.old_start - rs), l.new_end + (re - l.old_end))` (outside
   its hunks a side copies base lines one to one).
5. Each region resolves:
   - changed by one side only → take that side's lines (**clean**);
   - changed by both and the two side ranges hold identical lines → take them (**clean**);
   - otherwise → **conflict** `{base_start: rs, base_len: re - rs, ours_start, ours_len,
     theirs_start, theirs_len}` (the wire `ConflictHunk`).
6. Base lines outside every region are copied as they are.

The result is a sequence of regions, each `Clean {source: Base|Ours|Theirs, start, end}` (never
empty; identical two-sided changes report `Ours`) or `Conflict(hunk)`. The merged text, when
there is no conflict, is the concatenation of the clean regions. Conflict ranges are never
trimmed: lines common to both sides at the edges of a conflict stay inside it.

Properties (asserted by `tests/properties.rs`): `merge(b, x, x) = x`; `merge(b, b, t) = t` and
`merge(b, o, b) = o`; swapping ours and theirs gives the same clean result and mirrored conflict
ranges; every conflict carries the complete ours, base and theirs ranges of its region; a clean
result `m` is a fixed point (`merge(b, m, m) = m`). Like every diff3, `diff3-v1` is **not**
idempotent in the strong sense: re-merging a clean result with one input over the original base
can conflict. The smallest known case: base `a c d a a`, ours `a d`, theirs `c a c d` merge
cleanly to `c a d`, but `merge(base, ours, c a d)` conflicts (one line per letter).

## 4. Two-way merge of independent adds

When a path is added by both sides with different text content and no base applies (§5 rule 3,
§6.4), the payload is a two-way merge: diff `O → T` (§2); the unchanged runs are clean
(`source: Ours`) and every hunk is a conflict with an empty base range at line 0
(`base_start = 0, base_len = 0`) and the hunk's ours and theirs ranges. The path is still an
`add_add` conflict: two-way merge never resolves differing adds by itself, it only narrows the
conflict to the lines that differ.

## 5. Per-path rules

A side is absent or `{mode, content}`; mode is `file`, `executable`, `symlink` or `gitlink`;
content is bytes or *redacted* (bytes unavailable; compared by an opaque identity such as the
object id). Sides are equal when mode and content are equal. Rules apply in order:

1. ours = base → take theirs (including absent).
2. theirs = base, or ours = theirs → take ours.
3. base absent (so both sides present and different): identical content with different modes →
   `mode` conflict (content: ours); else `add_add`, whose payload is the two-way merge of §4 when
   both sides are readable, non-binary `file`/`executable` content.
4. base present and one side absent → `modify_delete`.
5. any side a symlink → `symlink`. Any side a gitlink → `binary` (gitlink content is an opaque id
   that cannot be line-merged).
6. Content and mode now merge separately, each by rules 1–2 (content: ours content = base
   content → theirs' content; theirs content = base content or ours = theirs → ours' content).
   Only when all three contents differ is a content merge needed, and then, in order:
   - any readable side is **binary** → `binary`. Binary means longer than
     `merge_max_text_bytes` (default 8 MiB = 8,388,608 bytes), or a NUL byte within the first
     8000 bytes. Boundary: a NUL at index 7999 is binary, at index 8000 is not;
   - any side redacted → `redacted` (a redacted side never blocks a merge that does not need its
     bytes, e.g. a mode-only change against a redacted base);
   - otherwise the line merge of §3; a conflict there is `content`.
7. Modes that cannot merge by rules 1–2 give a `mode` conflict when the content merged cleanly
   (the payload carries the merged content). A content conflict takes precedence over a mode
   conflict.

A clean result is *absent* or `{mode, content}` with content `Ours`, `Theirs` (the object can be
reused) or `Merged(bytes)`.

Rules 5–7 above are SPEC rules 5–8 read with one clarification: binary and redaction checks apply
only when a content merge is actually needed, so a mode-only change never conflicts with a
content change of a binary or redacted file. Rule 9 (structural) is in §6.5.

Conflict kinds: `content`, `add_add`, `modify_delete`, `symlink`, `binary`, `redacted`, `mode`,
`file_directory` (the wire `ConflictKind`), and `rename_rename` (§6.3; needs an additive wire
enum value when the server exposes it).

## 6. Tree merge: renames, copies and structure

Trees are flattened to maps from `/`-separated path to side (directories are implicit).

### 6.1 Rename detection (`base → side`)

Candidates are paths deleted from the base (`from`) and paths added on the side (`to`). Empty
files never take part. Modes must be compatible: `file` and `executable` with each other,
`symlink` and `gitlink` only with themselves.

1. **Exact.** For each `from` in bytewise order, the untaken `to` paths with identical content
   (redacted content: identical identity) are candidates. The first with the same basename (the
   bytes after the last `/`) wins, else the first in bytewise order.
2. **Similarity**, for the remaining readable, non-binary `file`/`executable` pairs. Similarity
   is the Jaccard index of the two sets of distinct lines: `shared / union`. A pair qualifies when
   `shared × den >= union × num` for the threshold `num/den` (default 1/2), in exact integer
   arithmetic. Qualifying pairs are ranked by similarity (compared as `s1 × u2` vs `s2 × u1`),
   higher first, then same basename first, then `from` bytewise, then `to` bytewise, and accepted
   greedily when neither path is already taken. If `|from| × |to|` exceeds `max_pairs` (default
   1,000,000) the similarity phase is skipped (exact renames are still found).

### 6.2 Following renames

Let `Ro` and `Rt` be the renames detected for ours and theirs.

1. A rename `b → p` of one side is **not followed** when the other side has an entry at `p` that
   is not its own rename of the same `b` to `p`. Both paths then merge in place, so nothing is
   lost: `p` typically becomes `add_add`, and `b` a deletion or `modify_delete`.
2. For each base path `b` with a followed rename:
   - renamed by ours only, to `p`: merge `(base[b], ours[p], theirs[b])` at `p`;
   - renamed by theirs only, to `q`: merge `(base[b], ours[b], theirs[q])` at `q`;
   - renamed by both to the same `p`: merge `(base[b], ours[p], theirs[p])` at `p`;
   - renamed to different paths `p ≠ q`: `rename_rename` conflict at both `p` and `q`, each
     carrying base `b`, ours `p` and theirs `q`.
   If the side that did not rename has deleted `b`, the result is `modify_delete` at the new path
   (a rename counts as a modification, even with unchanged content).
3. Every other path merges in place with the per-path rules. Each conflict side records its own
   path, which differs from the result path after a rename; clean content that is an unchanged
   input entry records that entry's side and path so its object can be reused.

### 6.3 Copies

A path added by both sides with different content (`add_add`) is merged three-way when both added
files derive from the same base file: the **copy source** of an added file is a base file with
identical content (same tie-break as §6.1 step 1), else the base text file of highest similarity
meeting the threshold (ties: same basename, then bytewise). When the copy sources of both sides
are the same base path `s`, the path is merged with `base[s]` as its base, and the conflict (if
any) carries `s` as its base path. Copies made by one side only are ordinary adds; changes to the
source are not propagated into copies.

### 6.4 Result

Clean entries and conflicts keyed by result path, plus the renames followed (sorted by side, then
base path).

### 6.5 Rule 9: `file_directory`

The candidate result is every clean entry path plus every conflict path. A path `p` such that some
other candidate starts with `p/` is a file where the result also needs a directory. This covers a
file added on one side where the other side has a directory, and a directory one side replaced by a
file while the other side added or kept entries under it. Every involved path (`p` and every
candidate under `p/`) becomes a `file_directory` conflict; a path that already conflicted keeps its
sides and hunks but takes the kind `file_directory`. `file_directory_conflicts(paths)` reports, in
bytewise order, each such `p` with the sorted paths under it.

## 7. Criss-cross merges: the virtual base

When a merge has several best merge bases, the merge base is a **virtual base** built from them:

1. No bases: the empty tree. One base: its tree.
2. Otherwise sort the base ids ascending and fold left. The accumulator starts as the first
   base's tree. For each next base `n`: compute the merge bases of (all bases folded so far) and
   `n`, build their virtual base recursively, and merge the accumulator (ours) with `n`'s tree
   (theirs) over it using §6.
3. Inner conflicts never fail the fold; they resolve into the virtual tree:
   - `content`, and `add_add` with a two-way payload: the marker rendering of §8 in `merge`
     style (ours and theirs sections, no base section), with ours' mode (theirs' if ours is
     absent);
   - `modify_delete`: the side that is present;
   - any other kind: ours' side at that path, or absent if ours has none.
   Every path resolved this way, and every path whose content was carried over from such a path,
   is listed as **conflicted** in the virtual base.
4. Nesting deeper than 64 levels uses the empty tree as the innermost base (only malformed
   histories get there).

The final merge is then `merge(virtual base, ours, theirs)`; conflict hunks refer to the virtual
base's lines, which may contain marker text when the path is listed as conflicted. When the final
merge touches a conflicted region of the virtual base differently on both sides it conflicts as
usual, so marker text never reaches a clean result unless one side already contained it.

## 8. Conflict markers (working copies only)

The server never writes markers (SPEC §7.3.2). For working copies, a conflict renders as:

```text
<<<<<<< ours
<ours lines>
||||||| base
<base lines>
=======
<theirs lines>
>>>>>>> theirs
```

- Every marker line is exactly the marker text followed by `\n`; there are no other labels.
- `merge` style omits the `||||||| base` line and the base lines; `diff3` style (the default)
  includes them.
- Before writing a marker, if the output so far is non-empty and does not end in `\n`, one `\n`
  is written first. This only happens when a section ends with a final line that has no
  terminator.
- Clean regions are written as they are between conflict blocks.

## 9. Determinism and versioning

- Every function of this contract is a pure function of its inputs.
- Ordering decisions are fixed: hunk order, region order, bytewise path order, id order for
  virtual bases, and the tie-breaks of §2.2 and §6.1.
- The algorithm id `diff3-v1` names this whole document. A server records the id with any
  derived merge result; rebase determinism (SPEC §7.3) is defined for a fixed id.

## 10. Differences from `git merge-file`

`crates/bee-merge/tests/git_differential.rs` runs `git merge-file -p --diff3 -L ours -L base -L
theirs` when `git` is on `PATH` (skipped otherwise) against every merge example in §12 and against
generated inputs whose lines are unique (so both tools find the same matched pairs). For those
inputs the outputs are identical byte for byte, and git's exit status equals the conflict count.
The known, intentional differences are:

1. **Diff choice on repeated lines.** git's xdiff uses its own Myers variant with heuristics
   (and, by default, the indent and sliding heuristics in other commands). On inputs with
   repeated lines it may choose a different (sometimes non-minimal) set of matched pairs, so
   region boundaries can differ. `diff3-v1` is exactly §2.
2. **Conflict refinement.** In its default `merge` style, git refines a conflict by diffing ours
   against theirs and moving common lines out of it ("zealous" merge), and merges conflicts
   separated only by lines without letters or digits. `diff3-v1` never trims or joins conflicts
   (§3 step 5). With `--diff3` git also does not refine, which is why the differential test uses
   it.
3. **Binary files.** git refuses to merge binary inputs; `diff3-v1` reports a `binary` conflict
   (§5), and the 8 MiB text limit has no git equivalent.
4. **Labels.** git writes the labels it is given; `diff3-v1` markers always say `ours`, `base`
   and `theirs`.

The differential test fails if any §12 example other than those listed here as differing
disagrees with git; currently none differ.

## 11. Worked diff examples

Each block lists `old` and `new` (strings with `\n`, `\r`, `\\` and `\"` escapes), the unified
`context`, the hunks as `old_start,old_len,new_start,new_len` (0-based), the unified rendering,
and the statistics as `hunks added removed`.

D1. Both empty: no hunks, nothing rendered.

```example
id: D1
old: ""
new: ""
context: 3
hunks: none
unified: ""
stats: 0 0 0
```

D2. Insert into an empty text: an empty old range renders as `0,0`.

```example
id: D2
old: ""
new: "a\nb\n"
context: 3
hunks: 0,0,0,2
unified: "@@ -0,0 +1,2 @@\n+a\n+b\n"
stats: 1 2 0
```

D3. Delete everything.

```example
id: D3
old: "a\nb\n"
new: ""
context: 3
hunks: 0,2,0,0
unified: "@@ -1,2 +0,0 @@\n-a\n-b\n"
stats: 1 0 2
```

D4. Replace one line: one hunk with one deletion then one insertion.

```example
id: D4
old: "a\nb\nc\n"
new: "a\nB\nc\n"
context: 3
hunks: 1,1,1,1
unified: "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n"
stats: 1 1 1
```

D5. Insert in the middle.

```example
id: D5
old: "a\nb\nc\n"
new: "a\nb\nx\nc\n"
context: 3
hunks: 2,0,2,1
unified: "@@ -1,3 +1,4 @@\n a\n b\n+x\n c\n"
stats: 1 1 0
```

D6. Delete the last line.

```example
id: D6
old: "a\nb\nc\n"
new: "a\nb\n"
context: 3
hunks: 2,1,2,0
unified: "@@ -1,3 +1,2 @@\n a\n b\n-c\n"
stats: 1 0 1
```

D7. Swap two lines. Two scripts of length 2 exist; the tie rule keeps `b` (delete `a` first,
insert `a` after `b`).

```example
id: D7
old: "a\nb\n"
new: "b\na\n"
context: 3
hunks: 0,1,0,0 2,0,1,1
unified: "@@ -1,2 +1,2 @@\n-a\n b\n+a\n"
stats: 2 1 1
```

D8. Duplicate a line: the original matches the first copy and the second is inserted.

```example
id: D8
old: "a\n"
new: "a\na\n"
context: 3
hunks: 1,0,1,1
unified: "@@ -1 +1,2 @@\n a\n+a\n"
stats: 1 1 0
```

D9. Remove a duplicate: the second copy is the one deleted.

```example
id: D9
old: "a\na\n"
new: "a\n"
context: 3
hunks: 1,1,1,0
unified: "@@ -1,2 +1 @@\n a\n-a\n"
stats: 1 0 1
```

D10. CRLF and LF lines differ; nothing is normalized.

```example
id: D10
old: "a\r\nb\r\n"
new: "a\nb\n"
context: 3
hunks: 0,2,0,2
unified: "@@ -1,2 +1,2 @@\n-a\r\n-b\r\n+a\n+b\n"
stats: 1 2 2
```

D11. Adding the final newline changes the last line.

```example
id: D11
old: "a\nb"
new: "a\nb\n"
context: 3
hunks: 1,1,1,1
unified: "@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n"
stats: 1 1 1
```

D12. Two changes five unchanged lines apart: separate unified hunks at context 1.

```example
id: D12
old: "1\n2\n3\n4\n5\n6\n7\n8\n9\n"
new: "1\nX\n3\n4\n5\n6\n7\nY\n9\n"
context: 1
hunks: 1,1,1,1 7,1,7,1
unified: "@@ -1,3 +1,3 @@\n 1\n-2\n+X\n 3\n@@ -7,3 +7,3 @@\n 7\n-8\n+Y\n 9\n"
stats: 2 2 2
```

D13. The same texts at context 2: still separate, because the five-line gap exceeds 2 × 2 = 4.

```example
id: D13
old: "1\n2\n3\n4\n5\n6\n7\n8\n9\n"
new: "1\nX\n3\n4\n5\n6\n7\nY\n9\n"
context: 2
hunks: 1,1,1,1 7,1,7,1
unified: "@@ -1,4 +1,4 @@\n 1\n-2\n+X\n 3\n 4\n@@ -6,4 +6,4 @@\n 6\n 7\n-8\n+Y\n 9\n"
stats: 2 2 2
```

D14. The same texts at context 3: one unified hunk.

```example
id: D14
old: "1\n2\n3\n4\n5\n6\n7\n8\n9\n"
new: "1\nX\n3\n4\n5\n6\n7\nY\n9\n"
context: 3
hunks: 1,1,1,1 7,1,7,1
unified: "@@ -1,9 +1,9 @@\n 1\n-2\n+X\n 3\n 4\n 5\n 6\n 7\n-8\n+Y\n 9\n"
stats: 2 2 2
```

D15. Repeated lines: of the minimal scripts, the forward greedy path keeps `a x`.

```example
id: D15
old: "x\na\nx\n"
new: "a\nx\na\n"
context: 3
hunks: 0,1,0,0 3,0,2,1
unified: "@@ -1,3 +1,3 @@\n-x\n a\n x\n+a\n"
stats: 2 1 1
```

D16. A moved block at context 0: deletion at the top, insertion at the end.

```example
id: D16
old: "a\nb\nc\nd\n"
new: "c\nd\na\nb\n"
context: 0
hunks: 0,2,0,0 4,0,2,2
unified: "@@ -1,2 +0,0 @@\n-a\n-b\n@@ -4,0 +3,2 @@\n+a\n+b\n"
stats: 2 2 2
```

## 12. Worked merge examples

Each block lists `base`, `ours`, `theirs`, the conflicts as
`base_start,base_len,ours_start,ours_len,theirs_start,theirs_len` (or `none`), and `output`: the
merged text when clean, else the `diff3`-style rendering of §8. A block with `mode: two-way` has
no base and shows the §4 two-way merge.

M1. Both sides made the same change: taken once.

```example
id: M1
base: "a\nb\nc\n"
ours: "a\nB\nc\n"
theirs: "a\nB\nc\n"
conflicts: none
output: "a\nB\nc\n"
```

M2. Changes separated by unchanged lines merge.

```example
id: M2
base: "a\nb\nc\nd\ne\n"
ours: "A\nb\nc\nd\ne\n"
theirs: "a\nb\nc\nd\nE\n"
conflicts: none
output: "A\nb\nc\nd\nE\n"
```

M3. The same line changed differently.

```example
id: M3
base: "a\nb\nc\n"
ours: "a\nX\nc\n"
theirs: "a\nY\nc\n"
conflicts: 1,1,1,1,1,1
output: "a\n<<<<<<< ours\nX\n||||||| base\nb\n=======\nY\n>>>>>>> theirs\nc\n"
```

M4. Adjacent lines changed by different sides touch (`[1,2)` and `[2,3)`) and conflict as one
region.

```example
id: M4
base: "a\nb\nc\nd\n"
ours: "a\nB\nc\nd\n"
theirs: "a\nb\nC\nd\n"
conflicts: 1,2,1,2,1,2
output: "a\n<<<<<<< ours\nB\nc\n||||||| base\nb\nc\n=======\nb\nC\n>>>>>>> theirs\nd\n"
```

M5. An insertion at the start of the other side's changed range touches it.

```example
id: M5
base: "a\nb\nc\n"
ours: "a\nX\nb\nc\n"
theirs: "a\nB\nc\n"
conflicts: 1,1,1,2,1,1
output: "a\n<<<<<<< ours\nX\nb\n||||||| base\nb\n=======\nB\n>>>>>>> theirs\nc\n"
```

M6. An insertion at the end of the other side's changed range touches it.

```example
id: M6
base: "a\nb\nc\n"
ours: "a\nb\nX\nc\n"
theirs: "a\nB\nc\n"
conflicts: 1,1,1,2,1,1
output: "a\n<<<<<<< ours\nb\nX\n||||||| base\nb\n=======\nB\n>>>>>>> theirs\nc\n"
```

M7. Different insertions at the same point: a conflict with an empty base range.

```example
id: M7
base: "a\nb\n"
ours: "a\nX\nb\n"
theirs: "a\nY\nb\n"
conflicts: 1,0,1,1,1,1
output: "a\n<<<<<<< ours\nX\n||||||| base\n=======\nY\n>>>>>>> theirs\nb\n"
```

M8. Identical insertions at the same point merge.

```example
id: M8
base: "a\nb\n"
ours: "a\nX\nb\n"
theirs: "a\nX\nb\n"
conflicts: none
output: "a\nX\nb\n"
```

M9. A deletion on one side and an unrelated edit on the other.

```example
id: M9
base: "a\nb\nc\nd\ne\n"
ours: "a\nc\nd\ne\n"
theirs: "a\nb\nc\nd\nE\n"
conflicts: none
output: "a\nc\nd\nE\n"
```

M10. Delete against modify of the same line: the ours section is empty.

```example
id: M10
base: "a\nb\nc\n"
ours: "a\nc\n"
theirs: "a\nB\nc\n"
conflicts: 1,1,1,0,1,1
output: "a\n<<<<<<< ours\n||||||| base\nb\n=======\nB\n>>>>>>> theirs\nc\n"
```

M11. Unterminated final lines: a `\n` is inserted before each following marker.

```example
id: M11
base: "a\nb"
ours: "a\nB"
theirs: "a\nC"
conflicts: 1,1,1,1,1,1
output: "a\n<<<<<<< ours\nB\n||||||| base\nb\n=======\nC\n>>>>>>> theirs\n"
```

M12. Ours unchanged: theirs is taken entirely.

```example
id: M12
base: "a\nb\n"
ours: "a\nb\n"
theirs: "x\ny\n"
conflicts: none
output: "x\ny\n"
```

M13. Regions join through the other side: ours changes lines 1 and 3, theirs changes line 2,
which touches both, so all three form one conflict.

```example
id: M13
base: "1\n2\n3\n4\n5\n"
ours: "1\nX\n3\nY\n5\n"
theirs: "1\n2\nZ\n4\n5\n"
conflicts: 1,3,1,3,1,3
output: "1\n<<<<<<< ours\nX\n3\nY\n||||||| base\n2\n3\n4\n=======\n2\nZ\n4\n>>>>>>> theirs\n5\n"
```

M14. Line endings are content: a CRLF→LF change touching an edit conflicts.

```example
id: M14
base: "a\r\nb\r\n"
ours: "a\nb\r\n"
theirs: "a\r\nB\r\n"
conflicts: 0,2,0,2,0,2
output: "<<<<<<< ours\na\nb\r\n||||||| base\na\r\nb\r\n=======\na\r\nB\r\n>>>>>>> theirs\n"
```

M15. An identical change and a conflicting change in one file: the identical one is clean.

```example
id: M15
base: "a\nb\nc\nd\ne\nf\n"
ours: "a\nB\nc\nd\nE\nf\n"
theirs: "a\nB\nc\nd\nF\nf\n"
conflicts: 4,1,4,1,4,1
output: "a\nB\nc\nd\n<<<<<<< ours\nE\n||||||| base\ne\n=======\nF\n>>>>>>> theirs\nf\n"
```

M16. Two-way merge of independent adds (§4): the common lines are clean, each difference is a
conflict with an empty base range at 0.

```example
id: M16
mode: two-way
ours: "a\nb\nc\n"
theirs: "a\nX\nc\nd\n"
conflicts: 0,0,1,1,1,1 0,0,3,0,3,1
output: "a\n<<<<<<< ours\nb\n||||||| base\n=======\nX\n>>>>>>> theirs\nc\n<<<<<<< ours\n||||||| base\n=======\nd\n>>>>>>> theirs\n"
```

## 13. Path, tree and virtual-base examples

These are asserted by unit tests in `crates/bee-merge/tests/tree.rs` and `src/path.rs` rather
than parsed from this file.

| Case | base | ours | theirs | Result |
|---|---|---|---|---|
| P1 | absent | `f: a\n` | `f: b\n` | `add_add`, two-way hunk `0,0,0,1,0,1` |
| P2 | `f: a\n` | absent | `f: b\n` | `modify_delete` |
| P3 | `f: a\n` | `f(exec): a\n` | `f: b\n` | clean: executable, theirs' content |
| P4 | `f: a\n` | `f(exec): a\n` | `f(symlink): a\n` | `symlink` |
| P5 | `f: a\n` | `f: \0…` | `f: b\n` | `binary` |
| P6 | `f: redacted` | `f(exec): redacted` | `f: b\n` | clean: executable, theirs' content |
| T1 | `a.txt: 1..5` | `b.txt: 1..5` | `a.txt: 1..4,FIVE` | clean: `b.txt` = theirs' `a.txt` |
| T2 | `a: x` | `b: x` | `c: x` | `rename_rename` at `b` and `c` |
| T3 | `a: x` | `b: x` | absent | `modify_delete` at `b` |
| T4 | `d/x` | `d` (file) | `d/x`, `d/y` | `file_directory` at `d` and `d/y` |
| T5 | `src: S` | `src: S`, `cp: S+o` | `src: S`, `cp: S+t` | `cp` merged three-way over `src` |
| V1 | bases `f: b\n`, `f: c\n` over `f: a\n` | | | virtual `f` holds merge-style markers |
