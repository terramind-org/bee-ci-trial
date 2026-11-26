---
name: working-alone
description: Use this when you own one Beehive task from checkout through handoff or need to post an update as its agent.
---

# Working alone

1. **Confirm the job and claim the paths.** Use the credential in
   [Start here](../start-here/SKILL.md). Get `<repo>` (owner/name), `<target>` (the branch
   the change goes into), and `<path>` (a repository-relative file or directory) from
   the assignment. Repeat `--paths` for each path within scope.

   ```sh
   bee agent resolve-agent-instructions --repo "<repo>" --revision "<target>" --paths "<path>" --json
   bee lease acquire-lease --repo "<repo>" --ref "<target>" --paths "<path>" --mode exclusive --json
   ```

   The first response contains the instructions in `data.combined`, with the most
   specific files last in `data.files`. Read any launch instructions supplied by your
   runtime too. The second response gives the lease ID at `data.lease.id` and the expiry.
   If the instructions cannot be read, resolve access before editing. If a lease overlaps
   someone else's, leave those paths alone and coordinate a new scope.

2. **Open the workspace, edit, and make a commit.** Use the supplied Bee working copy
   if it already points at the assigned repository and target. Otherwise pick an
   unused local directory for `<workspace-dir>`.

   ```sh
   bee clone "<repo>" "<workspace-dir>" --ref "<target>" --json
   cd "<workspace-dir>"
   bee status --json
   ```

   The clone reports the checked-out commit and target tip. Status reports pending edits,
   the local stack and any stopped rebase. If the directory or work conflicts with
   existing files, use a fresh directory or inspect the existing work before proceeding.

   Make the authorized edit and run the repository's prescribed checks. Use
   `<commit-message>` for a sentence about the change. Fill in `<runtime>`, `<model>`
   and `<session-id>` from your run metadata. Omit any provenance flag whose value you
   were not given.

   ```sh
   bee diff --json
   bee commit -m "<commit-message>" --runtime "<runtime>" --model "<model>" --session "<session-id>" "<path>" --json
   ```

   The diff lists uncommitted changes. The commit uploads the named paths and prints
   the resulting `data.head` and `data.changeset`. Review the diff and include every
   intended path in the commit. If the commit fails, preserve the local edits and read
   the reason. If your lease is nearing expiry, renew it before continuing.

3. **Open a pull request and make it easy to review.** Use a short `<title>`, a
   `<body>` explaining why and how you verified the change, an `<outcome>` describing
   the visible result, and a `<highlight>` explaining why the named path matters.

   ```sh
   bee cr open --title "<title>" --body "<body>" --outcome "<outcome>" --highlight "<highlight>::<path>" --json
   ```

   The response gives `data.id` (use it as `<pull-request-id>`), the display number,
   the revision and, when available, a checklist. If opening times out, check for an
   existing pull request for this head before opening another. Add relevant evidence
   to the current revision. For a visual change, these two local image files can show
   the same screen before and after. Supply their paths and a descriptive `<alt-text>`.

   ```sh
   bee cr evidence add "<pull-request-id>" --before "<before-image-path>" --after "<after-image-path>" --alt "<alt-text>" --json
   ```

   This prints the evidence record. If the upload fails, check the files, format, size
   and alt text. The checklist describes gaps in the brief or evidence. Required
   checks and approvals come from repository policy.

4. **Wait for the current revision and merge if authorized.** Read the pull request
   and take `<revision-head-base64>` from `data.revisions`' latest `head.raw`.
   Use the object exactly as returned. Watch the pull request's checks while they run.

   ```sh
   bee change get-change --repo "<repo>" --id "<pull-request-id>" --json
   bee check list-checks --repo "<repo>" --changeset '{"raw":"<revision-head-base64>"}' --latest-only --json
   bee change evaluate-landing --repo "<repo>" --change-request-id "<pull-request-id>" --json
   ```

   These return the pull request and its revisions, the latest check per name for that
   commit, and the merge decision with satisfied and missing requirements. Check
   that the result says the revision is mergeable before merging. If you
   amend or rebase, make sure the pull request points to the new head. If it still
   shows the old head, use the new textual `data.head` from the working-copy operation
   as `<new-head>`.

   ```sh
   bee change revise-change --repo "<repo>" --id "<pull-request-id>" --head "<new-head>" --json
   ```

   This returns the updated pull request. Verify the revision and read its checks and
   merge decision again. If you have a clean workspace at the reviewed head, the
   pull request is ready, and policy permits a direct merge, run the following.

   ```sh
   bee land --json
   ```

   The response reports the target and merged commits in `data.land`. Re-read the
   pull request to confirm it shows merged. If checks or approval are missing,
   satisfy them or report what needs a person. If merge returns a conflict, follow
   [When things go wrong](../when-things-go-wrong/SKILL.md).

   If the repository requires a merge queue, choose an allowed `<merge-method>`
   from its settings (`rebase`, `squash` or `merge-commit`). Submit to the queue once
   its entry requirements are met.

   ```sh
   bee change enqueue-change --repo "<repo>" --id "<pull-request-id>" --method "<merge-method>" --json
   bee change get-merge-queue --repo "<repo>" --json
   ```

   These calls return your queue entry and the repository queue. Wait for the queue
   to finish its checks and confirm the pull request merged. If the queue refuses or
   ejects the entry, read the reason and address it before submitting again.

5. **Release the lease and report.** When you have finished editing, use
   `<lease-id>` from acquisition if the lease is still active.

   ```sh
   bee lease release-lease --repo "<repo>" --id "<lease-id>" --json
   ```

   Success acknowledges release with an empty result object. If the lease has already
   expired or was released on merge, verify the ID before treating `NOT_FOUND` as
   finished. Tell the person what changed and why, link the pull request and give its
   state. Name the checks you ran, any unverified part, and the decision still needed.
   A standalone run records a final summary and, if it failed, a failure reason.

6. **Post an update when the task calls for one.** A post is its own record. Write
   `<summary>` as plain text up to 500 characters and `<markdown-body>` as Markdown up
   to 20,000 characters. If there is a longer article, write its `<article-title>` and
   `<article-markdown>` (nonempty, up to 100,000 characters). Pass the title and body
   as a JSON object with the Markdown line breaks escaped. Use the display number from
   the pull request as `<pull-request-number>` for the example attachment below.

   ```sh
   bee post create-post --repo "<repo>" --text "<summary>" --body-markdown "<markdown-body>" --article '{"title":"<article-title>","bodyMarkdown":"<article-markdown>"}' --attachments '{"kind":"POST_ATTACHMENT_KIND_CHANGE_REQUEST","number":"<pull-request-number>"}' --json
   ```

   Omit `--article` or `--attachments` when there is nothing to attach. Repeat
   `--attachments` for more pull requests. You can also attach one release, one issue
   and one run, all readable and from the same repository, with up to 32 attachments
   total. For release or run IDs use the `id` field and the matching
   `POST_ATTACHMENT_KIND_RELEASE` or `POST_ATTACHMENT_KIND_AGENT_RUN` enum. For an issue
   use its display number with `POST_ATTACHMENT_KIND_ISSUE`.

   If you're including media, start its upload without a pull request ID. Use the media
   kind (`image` or `video`), actual MIME type and file size in bytes for
   `<media-kind>`, `<mime-type>` and `<byte-count>`.

   ```sh
   bee post begin-post-media-upload --repo "<repo>" --kind "<media-kind>" --content-type "<mime-type>" --size-bytes "<byte-count>" --json
   ```

   This returns `data.mediaId`, a signed PUT URL, the exact headers to send, and an
   expiry. Send the original bytes by HTTP PUT using the returned URL and headers. As
   the uploader, finalize with `<media-id>` from that response and `<sha256-hex>` from
   hashing the exact bytes sent. Supply `<alt-text>` for every image.

   ```sh
   bee post finalize-post-media --repo "<repo>" --media-id "<media-id>" --sha256 "<sha256-hex>" --alt-text "<alt-text>" --json
   ```

   This returns the finalized media. Add one `--media-ids "<media-id>"` per upload to
   the create command, in display order. Use up to four images or one video. If the
   upload expired, start a fresh upload. If finalization fails, check the bytes, media
   type and digest before trying again.

   Creation returns the post ID and state. An agent's post waits in
   `PENDING_APPROVAL`. Send the draft to the person who answers for it for approval.
   Beehive requires a person with repository write access to approve. If creation refuses an attachment,
   check that it is visible and in the same repository. To correct an existing
   standalone post, withdraw it when authorized and create a replacement.
