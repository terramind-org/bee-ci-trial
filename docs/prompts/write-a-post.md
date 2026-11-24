---
id: write-a-post
title: Write a post for a change request
when: You opened a change request in a repository and want people who follow it to hear about it when it lands. Write the post text, attach one strong image or a short video, and preview it.
---
You are going to write the post for a change request in {{repo}} on {{endpoint}}. A post is the announcement of a change: it is a draft while the change request is open, and it publishes to the repo's feed and to the Following feed of everyone who follows the repo when the change lands. Each change request has at most one post. A change request without a post lands as before and announces nothing. If you have not connected yet, follow the "Set up Bee for a repository" prompt first, and open the change request with the "Open a change request and land it" prompt.

A good post says what is different for a person, not what you edited.
- **The user-visible change, in plain words.** "Checkout now shows the final price, tax included, before you pay." not "Refactor total computation". Under 500 characters, plain text, no markdown. Nothing is rendered as links or formatting, and newlines are the only control characters allowed.
- **One strong image, or a short video.** Prefer this change's own evidence: the "after" screenshot you attached with the "Show your work on a change request" prompt, captured at 1440 by 900. You may attach up to 4 images, or exactly one video and no images (png, jpeg, gif or webp up to 8 MiB each; mp4 or webm up to 64 MiB). A post with no visible change, such as a refactor, can be text alone.
- **Alt text on every image**, 1 to 1000 characters, describing what a person who cannot see it needs to know: what screen it is and what to notice.
- **No secrets, internal hostnames or private data.** A post on a public repository is public. Check the text, the alt text and every pixel of every image for tokens, customer data, internal URLs and addresses before you upload. A screenshot of a staging page may show a hostname in its address bar; crop it out.

1. Write the text when you open the change request, or afterwards. The first form validates the text before the change request opens, so a bad post fails the whole call and nothing is opened. `bee cr open` has no post flag; `bee change open-change` has `--post-text`:

   ```sh
   bee change open-change --repo {{repo}} --head <changeset id> --target-ref main \
     --title "Subject line" --post-text "Checkout now shows the final price, tax included, before you pay." --json
   ```

   For a change request that is already open (the usual case after `bee cr open`), set or replace the post with:

   ```sh
   bee post set-change-post --repo {{repo}} --change-id <id> \
     --text "Checkout now shows the final price, tax included, before you pay." --json
   ```

   `data.state` is `POST_STATE_DRAFT`, and `data.version` is the version of the post. Pass `--expected-version <version>` to refuse to overwrite an edit someone else made. Setting the text again clears an earlier approval. `INVALID_ARGUMENT` carries a subcode: `text_too_long`, `bad_text`, `empty_post` (no text and no media) or `bad_media`.

2. Attach media. Three calls per file: ask for an upload, send the bytes, then finalize with their hash. The server checks the hash and the real file type, so renaming a file does not get it through.

   ```sh
   bee post begin-post-media-upload --repo {{repo}} --change-id <id> \
     --kind image --content-type image/png --size-bytes <bytes> --json
   ```

   Keep `data.mediaId`, `data.uploadUrl` and `data.uploadHeaders`. The upload expires after 15 minutes, so do the next two steps straight away. Send the file with a plain HTTP `PUT` to `uploadUrl`, with the headers it lists and the file as the body (`curl -X PUT -H "<name>: <value>" --data-binary @after.png "<uploadUrl>"`). Then finalize:

   ```sh
   bee post finalize-post-media --repo {{repo}} --media-id <media id> \
     --sha256 <lowercase hex of the file> --alt-text "The payment step, with the total and tax shown" \
     --width 1440 --height 900 --json
   ```

   Compute the hash with `shasum -a 256 after.png`. For a video use `--kind video`, `--content-type video/mp4` (or `video/webm`) and `--duration-ms`; alt text is optional for a video but worth writing. Finalizing answers `INTEGRITY` when the bytes differ from the hash (upload again), `TOO_LARGE` above the limits and `INVALID_ARGUMENT` with `bad_media` or `alt_text_required`. Media that is never attached to a post is deleted after 24 hours.

3. Attach the finalized media to the post, in display order, by setting the post again with the same text:

   ```sh
   bee post set-change-post --repo {{repo}} --change-id <id> \
     --text "Checkout now shows the final price, tax included, before you pay." \
     --media-ids <media id> --json
   ```

   `--media-ids` replaces the set, so repeat every id you want to keep. Remove the whole post with `bee post clear-change-post --repo {{repo}} --change-id <id>`.

4. Preview it. Read the post back and look at what a follower will see:

   ```sh
   bee post get-change-post --repo {{repo}} --change-id <id> --json
   ```

   Check `text`, that every `media` entry has `altText` and a `url` that opens the image, and `state`. Open the change request page in the web UI to see the post panel as people will. Fix what is wrong with another `set-change-post`.

5. Ask for approval. A post written by an agent does not publish when the change lands: it waits as `POST_STATE_PENDING_APPROVAL` until a person with write access to the repository approves it, and that person cannot be you. Say so in the brief, where they are already looking, with a question they can answer in one click:

   ```sh
   bee cr brief <id> --ask "Publish the post for this change?::yes|no, keep it off the feed" --json
   ```

   Tell them the post is attached to the change request and what it says. Their answer comes back as a comment `Decision on "Publish the post for this change?": yes`. When the change has landed and the post is pending, a person approves it from the post panel or with `bee post approve-post --repo {{repo}} --post-id <post id>`; the same call from an agent is refused. A post a person wrote publishes at landing with no further step. If the change request closes without landing, the post is discarded, and it comes back as a draft if the change request reopens. To take a published post down, `bee post withdraw-post --repo {{repo}} --post-id <post id> --reason "..."` (the author or a repo admin).

Over MCP the same calls are the `post_set_change_post`, `post_begin_post_media_upload`, `post_finalize_post_media`, `post_get_change_post`, `post_clear_change_post`, `post_approve_post` and `post_withdraw_post` tools. `post_begin_post_media_upload` hands back a URL for the `PUT`; the bytes never travel through MCP.

Reasons you will meet, and the right move: `INVALID_STATE` (the post is already published or withdrawn; you can no longer edit it), `VERSION_MISMATCH` (someone changed the post; read it and retry), `PERMISSION_DENIED` (you are neither the change request's author nor a writer), `upload_expired` (begin again).

Report the change request id, the post text you wrote, the media you attached with its alt text, the post's state, and who you asked to approve it.
