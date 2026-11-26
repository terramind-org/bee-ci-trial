---
id: migrate-from-github
title: Migrate a repository from GitHub
when: Moving a GitHub repository to Beehive with the same commit ids, pull request numbers and issue numbers, then switching over.
---
You are going to move a repository from GitHub into the Beehive org {{org}} on {{endpoint}}. The importer keeps every commit id, gives pull requests the same numbers as change requests, imports issues, and writes a fidelity report that says what was carried, archived or dropped. Do not call the migration complete without reading that report.

You need two credentials, both in the environment and never in files or logs:

```sh
export BEE_ENDPOINT={{endpoint}}
export BEE_TOKEN=<a Beehive token that may create repos in {{org}}>
export GITHUB_TOKEN=<a GitHub token that can read the source repository>
```

The importer is the `bee-github` binary. If `bee-github --help` fails, install it with `cargo install --locked --git https://github.com/terramind-org/bee.git bee-github`. Replace `<owner>/<name>` below with the GitHub repository you were asked to migrate.

1. Dry run. It fetches nothing and prints the number of pull requests and issues, the API requests it will make and the time at the current rate limit:

   ```sh
   bee-github import <owner>/<name> --org {{org}} --dry-run --json
   ```

   If the estimate is longer than you expected, pass a GitHub App instead of a personal token with `--app-id`, `--app-key` and `--installation`: it has its own rate limit.

2. Import. The command is safe to re-run; it resumes from the state directory (`./.bee-github/<owner>/<name>` unless you set `--state`) and fetches only what is new.

   ```sh
   bee-github import <owner>/<name> --org {{org}} --json
   ```

   Add `--mirror-only` to bring in only git history first (branches, tags, pull refs), which makes the repository cloneable within minutes. `--no-wiki` and `--no-lfs` skip those parts. Add `--hub-url {{endpoint}}` if links inside imported text should point at this Beehive instance. When the server runs the import itself, the command prints `{"import_id": ..., "server": ...}` instead of a report; follow the import with `bee import get-import --id <import_id> --json`.

3. Read the fidelity report. A local import prints it under `report`. A server-side import has it at:

   ```sh
   bee import get-fidelity-report --id <import_id> --json
   ```

   For every category (commits, trees, blobs, refs, annotated tags, submodules, LFS pointers, and the metadata categories) compare imported, archived, dropped and excluded counts. Every dropped item has a reason; list the reasons. Treat the import as complete only when the report's `complete` flag is true. Otherwise say which categories are short and why, and ask whether that is acceptable. Check the migrated repository too: `bee log --repo {{org}}/<name> --json` should show the same head commit id as GitHub.

4. Keep it in sync while people and agents still use GitHub. One polling pass:

   ```sh
   bee-github sync <owner>/<name> --org {{org}} --json
   ```

   To follow GitHub continuously, serve webhooks and poll: add `--webhook-listen <addr:port> --webhook-secret <secret> --poll-secs 300`, and point a GitHub webhook at `POST /webhooks/github` on that address.

5. People appear as placeholder principals keyed by GitHub user id. A person claims theirs by signing in with GitHub; list the unclaimed ones with `bee import list-placeholder-principals --json`. `bee-github claim-token --help` shows how to print the claim token for one account.

6. Cutover, only after the person who asked you has said to go ahead. It freezes the source, runs a final sync, writes the redirect map and the final report:

   ```sh
   bee-github cutover <owner>/<name> --org {{org}} --json
   ```

   Add `--archive-github` to archive the GitHub repository first. After cutover, switch every clone and CI job to Beehive; `bee git setup --repo {{org}}/<name>` repoints a git checkout.

Report: the dry-run estimate, the fidelity report summary with each dropped category and its reason, whether `complete` was true, and the result of cutover if you ran it.
