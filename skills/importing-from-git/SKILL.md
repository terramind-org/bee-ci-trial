---
name: importing-from-git
description: Use this to bring git history or a GitHub repository into Beehive, verify what carried over, or keep a git mirror current.
---

# Importing from git

1. **Agree on the destination and the source of truth.** Get the existing
   destination organization and the approved destination repository name from the
   person who owns the migration. Start with a new or dedicated mirror repository.
   Importing into an existing repository can move its refs. Confirm write access
   and the source credential before starting. Keep tokens out of URLs and reports.

2. **Bring in git history.** For a local source, have the server operator confirm
   a full git repository or bare mirror under its allowed import roots. Use that
   server-side filesystem path for `<server-git-path>` and the agreed destination
   `owner/name` for `<destination-repo>`. A path visible only to your CLI will fail.

   ```sh
   bee import start-import --source-kind git --source "<destination-repo>" --source-url "<server-git-path>" --json
   ```

   This queues the history import and returns its ID. A plain git import targets
   one repository and has no dry run. If the server cannot read the path, get a
   permitted source from the operator. For a remote git URL, use the pull mirror
   in step 5. A history import carries git objects and refs.

3. **For GitHub, estimate before importing forge records.** Use the source
   `owner/name` as `<github-repo>` and the approved destination organization as
   `<destination-org>`. The destination repository takes the source's short
   name. Install the GitHub importer beside the Bee executable or set
   `BEE_GITHUB_BIN` to its absolute path. Supply an authorized GitHub token
   through `GH_TOKEN` or `GITHUB_TOKEN`; a configured GitHub CLI login is also
   supported. Use your Beehive credential from [Start here](../start-here/SKILL.md).

   ```sh
   bee import github "<github-repo>" --org "<destination-org>" --dry-run --json
   ```

   This estimates pull request and issue counts, requests and time at the current
   rate limit. If it fails, check importer installation and access to the source.
   Once the destination and plan are approved, start the import.

   ```sh
   bee import github "<github-repo>" --org "<destination-org>" --json
   ```

   A server-backed import returns `data.import_id`. A local fallback returns
   the phase, fidelity report and state directory. Keep that directory if the run
   was interrupted. Check the report for metadata the target could not store.
   If GitHub throttles the import, wait for the reported reset before continuing.

4. **Verify what arrived.** Use `<import-id>` from `data.id` for a server git
   import or `data.import_id` for a server-backed GitHub import. For a local fallback,
   read the returned report instead.

   ```sh
   bee import get-import --id "<import-id>" --json
   bee import get-fidelity-report --id "<import-id>" --json
   ```

   The first call returns state and progress. The second returns counts and reasons
   for imported, archived and dropped records. If the report is unavailable while
   import is active, check progress and read the report after the import settles.
   Investigate failed or cancelled imports before treating an incomplete result
   as a successful migration.

   - Imported commits retain their git IDs, including merge commits. Branches
     become branches of the same name, with unsupported names escaped and
     reported. Tags are under `tags/`. Unsupported refs and shallow or
     unreachable history can be dropped.
   - GitHub pull requests and issues keep their numbers where imported. Pull
     request pushes become revisions where the old GitHub heads are still
     available. Imported merged pull requests are recorded as closed with their
     merge commit, since the source commits are already in history.
   - GitHub workflow files stay under `.github/workflows/`. Beehive also reads
     workflows under `.beehive/workflows/` for native CI. Verify that the expected
     workflows run on the destination and configure their required secrets and
     checks there. Archived results are separate from a new CI run.
   - Review generated policy before adopting it. Expect a manual checklist for
     secrets, deploy keys, app installs and webhook setup. Inspect the report for
     unsupported LFS or metadata and for substitutions such as archived review
     comments.

5. **Keep git refs in step with a pull mirror.** As a repository admin, use the
   approved source URL without credentials for `<remote-url>` and an approved
   source-of-truth branch or ref pattern for `<ref-pattern>`. Repeat
   `--ref-patterns` to add a pattern. Use a remote the server can already read.
   For a private source, have the admin supply its credential through the
   authorized configuration flow.

   ```sh
   bee repo create-repo-mirror --repo "<destination-repo>" --remote-url "<remote-url>" --direction pull --ref-patterns "<ref-pattern>" --interval-seconds 3600 --json
   bee repo sync-repo-mirror --repo "<destination-repo>" --id "<mirror-id>" --json
   ```

   Creation returns the mirror record and `data.id` for `<mirror-id>`. Sync
   returns the record with the last sync time and any recorded error. The server
   schedules future syncs. Check `lastError` and compare representative remote
   and destination refs and commit IDs. If the worker is unavailable, ask the
   server operator about mirror support. Pull mirrors keep refs deleted upstream.
   Keep destination edits off the mirrored branches until cutover, since the
   next sync can move their tips.

6. **Verify metadata sync separately before cutover.** A git mirror carries
   refs and history. Use the GitHub integration's webhook sync and reconciliation
   for ongoing GitHub metadata when that deployment is configured for them.
   Verify it with an issue or pull request update and check the fidelity report.
   GitHub deletions after import are not mirrored by that integration. Report
   which refs and records were verified, what was archived or dropped, and which
   actions need the migration owner's decision.
