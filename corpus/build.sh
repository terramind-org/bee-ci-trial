#!/usr/bin/env bash
# Builds the importer test corpus: small, deterministic git repositories made with the git CLI.
#
#   corpus/build.sh <out-dir>
#
# Creates, under <out-dir>:
#   main/          SHA-1 repo exercising everything the importer must carry exactly: merges, an
#                  octopus merge, renames, binary and executable files, symlinks, a submodule
#                  gitlink, non-UTF-8 file names and commit messages, an LFS pointer, annotated,
#                  lightweight and nested tags, notes, a second root, odd trees (non-canonical
#                  modes, unsorted entries, empty directories) and ref names bee must rename
#                  (feature@x, an id-shaped name, a non-UTF-8 name).
#   main.pack      every object of main/ as one pack, and
#   main.refs      its ref list (`<id> <refname>` lines), as a fetch would deliver them.
#   sha256/        a small repo in git's SHA-256 object format.
#   edge/          history the importer cannot carry with identical ids, to exercise the
#                  fidelity report: a 17-parent merge (whose new tree a valid commit reuses),
#                  a `.bee` path, a tag of a tree.
#
# Every commit has fixed author and committer identities and dates, and no user or system git
# configuration is read, so the same git version always produces the same object ids.
set -euo pipefail

out="${1:?usage: corpus/build.sh <out-dir>}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
export HOME="$out"
export GIT_AUTHOR_NAME="Ada Author" GIT_AUTHOR_EMAIL="ada@example.com"
export GIT_COMMITTER_NAME="Cy Committer" GIT_COMMITTER_EMAIL="cy@example.com"
export TZ=UTC LC_ALL=C

T=1700000000
tick() {
  T=$((T + 60))
  export GIT_AUTHOR_DATE="@$T +0130" GIT_COMMITTER_DATE="@$T -0700"
}

g() { git -c init.defaultBranch=main -c core.autocrlf=false -c commit.gpgsign=false \
  -c tag.gpgsign=false -c core.symlinks=true "$@"; }

commit() { tick; g commit -q --allow-empty -m "$1"; }

# --- main ------------------------------------------------------------------------------
rm -rf "$out/main" "$out/main.pack" "$out/main.refs"
g init -q "$out/main"
cd "$out/main"

printf 'hello\n' > README.md
mkdir -p src/lib docs
printf 'fn main() {}\n' > src/main.rs
printf 'pub fn lib() {}\n' > src/lib/mod.rs
printf 'guide\n' > docs/guide.md
g add -A && commit "Initial commit"

printf '#!/bin/sh\necho run\n' > run.sh && chmod +x run.sh
ln -s README.md link-to-readme
printf '\000\001\002\377binary\000data' > blob.bin
g add -A && commit "Add executable, symlink and binary"

# A Git LFS pointer file.
printf 'version https://git-lfs.github.com/spec/v1\noid sha256:%s\nsize 4096\n' \
  4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393 > big.dat
g add -A && commit "Add an LFS pointer"

# Rename, and replace a directory with a file.
g mv docs/guide.md GUIDE.md
g rm -q -r src/lib && printf 'now a file\n' > src/lib
g add -A && commit "Rename guide; directory becomes a file"

# Submodule pointer (gitlink) without fetching anything.
printf '[submodule "vendor/lib"]\n\tpath = vendor/lib\n\turl = https://example.com/lib.git\n' > .gitmodules
g add .gitmodules
g update-index --add --cacheinfo 160000,1111111111111111111111111111111111111111,vendor/lib
commit "Add a submodule"

# A file whose name is not UTF-8 (written through the index; some filesystems refuse it).
blob=$(printf 'latin-1 name\n' | g hash-object -w --stdin)
g update-index --add --cacheinfo "100644,$blob,caf$(printf '\351').txt"
commit "Add a file with a non-UTF-8 name"
g update-index --skip-worktree "caf$(printf '\351').txt"

# A non-UTF-8 commit message with an encoding header.
tick
printf 'Caf\351 au lait\n\nLatin-1 body.\n' > "$out/msg.txt"
g -c i18n.commitEncoding=ISO-8859-1 commit -q --allow-empty -F "$out/msg.txt"
rm "$out/msg.txt"

g tag -a v1.0 -m "Release 1.0"
g tag light-v1
tick
g tag -a v1.0-nested -m "Tag of a tag" v1.0 2>/dev/null

# Branches and merges.
g checkout -q -b feature
printf 'feature work\n' > feature.txt
g add -A && commit "Feature work"
printf 'more feature work\n' >> feature.txt
g add -A && commit "More feature work"
g checkout -q main
printf 'main work\n' >> README.md
g add -A && commit "Main work"
tick
g merge -q --no-ff -m "Merge branch 'feature'" feature

# Octopus merge of three side branches.
for b in o1 o2 o3; do
  g checkout -q -b "$b" main
  printf '%s\n' "$b" > "$b.txt"
  g add -A && commit "Branch $b"
done
g checkout -q main
tick
g merge -q --no-ff -m "Octopus merge" o1 o2 o3 >/dev/null

# An odd tree git itself would never write: non-canonical mode digits, unsorted entries and
# an empty directory holding an empty directory.
empty=$(g hash-object -t tree -w --literally /dev/null)
hollow=$( (printf '40000 e\000'; printf '%s' "$empty" | xxd -r -p) | g hash-object -t tree -w --literally --stdin)
a=$(printf 'a\n' | g hash-object -w --stdin)
odd=$( (printf '100664 z\000'; printf '%s' "$a" | xxd -r -p
        printf '100644 b\000'; printf '%s' "$a" | xxd -r -p
        printf '40000 hollow\000'; printf '%s' "$hollow" | xxd -r -p) |
      g hash-object -t tree -w --literally --stdin)
tick
oddc=$(printf 'Odd tree\n' | g commit-tree "$odd" -p HEAD)
g branch odd-tree "$oddc"

# Ref names bee's grammar rejects.
g branch 'feature@x' feature
g branch c1_abcdef0123 o1

# Notes.
tick
g notes add -m "A note" HEAD

# A second root (orphan) branch, built without touching the working tree.
blob=$(printf 'orphan\n' | g hash-object -w --stdin)
otree=$(printf '100644 blob %s\torphan.txt\n' "$blob" | g mktree)
tick
g branch orphan "$(printf 'Orphan root\n' | g commit-tree "$otree")"

g pack-refs --all
# A branch whose name is not UTF-8. Some filesystems refuse such file names, so it goes
# straight into packed-refs, at its sorted place: the file's header declares it sorted, and
# git 2.56 aborts ("packed-refs backend yielded reference preceding its prefix") on a file
# that is not.
odd_ref="refs/heads/caf$(printf '\351')"
odd_line="$(g rev-parse o2) $odd_ref"
awk -v line="$odd_line" -v name="$odd_ref" '
  !done && /^[0-9a-f]+ / && $2 > name { print line; done = 1 }
  { print }
  END { if (!done) print line }
' .git/packed-refs > .git/packed-refs.sorted
mv .git/packed-refs.sorted .git/packed-refs
git rev-list --objects --all | cut -d' ' -f1 | g pack-objects -q --stdout > "$out/main.pack"
g for-each-ref --format='%(objectname) %(refname)' > "$out/main.refs"
cd "$out"

# --- sha256 ----------------------------------------------------------------------------
rm -rf "$out/sha256"
g init -q --object-format=sha256 "$out/sha256"
cd "$out/sha256"
printf 'one\n' > a.txt && g add -A && commit "First"
g checkout -q -b side
printf 'side\n' > b.txt && g add -A && commit "Side"
g checkout -q main
printf 'two\n' >> a.txt && g add -A && commit "Second"
tick
g merge -q --no-ff -m "Merge side" side
g tag -a v1 -m "SHA-256 tag"
cd "$out"

# --- edge ------------------------------------------------------------------------------
rm -rf "$out/edge"
g init -q "$out/edge"
cd "$out/edge"
printf 'base\n' > base.txt && g add -A && commit "Base"
base=$(g rev-parse HEAD)
# 17 parents: more than bee allows (SPEC 4.6).
parents=()
for i in $(seq 1 17); do
  tick
  c=$(printf 'Side %s\n' "$i" | g commit-tree "$(g rev-parse HEAD^{tree})" -p "$base")
  parents+=(-p "$c")
done
# The big merge introduces a new directory and file. The importer converts them before it
# finds the parent count too large; a later valid commit reuses the same tree, so the
# importer must still store objects first converted for a dropped commit.
mkdir -p merged && printf 'merged\n' > merged/file.txt && g add -A
mtree=$(g write-tree)
tick
big=$(printf 'Seventeen parents\n' | g commit-tree "$mtree" "${parents[@]}")
tick
child=$(printf 'Child of the big merge\n' | g commit-tree "$mtree" -p "$big")
g branch too-many-parents "$child"
tick
# The importer walks ref tips in id order; pick a message so this tip sorts after the big
# merge's child and is therefore converted second.
n=0
while :; do
  reuse=$(printf 'Same tree, one parent (%s)\n' "$n" | g commit-tree "$mtree" -p "$base")
  [[ "$child" < "$reuse" ]] && break
  n=$((n + 1))
done
g branch reuses-tree "$reuse"
# A reserved path name.
blob=$(printf 'reserved\n' | g hash-object -w --stdin)
reserved=$( (printf '100644 .bee\000'; printf '%s' "$blob" | xxd -r -p) |
           g hash-object -t tree -w --literally --stdin)
tick
g branch reserved-name "$(printf 'Reserved name\n' | g commit-tree "$reserved" -p "$base")"
# A tag of a tree.
g tag -a tree-tag -m "Tags a tree" "$(g rev-parse HEAD^{tree})"
cd "$out"

echo "corpus: built main, sha256 and edge in $out"
