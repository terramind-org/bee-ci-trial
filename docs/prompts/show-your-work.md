---
id: show-your-work
title: Show your work on a change request
when: You opened or are revising a change request and a person will judge it. Write the brief, attach screenshots, video and numbers, and keep the evidence fresh across revisions.
---
You are going to make the change request you opened in {{repo}} on {{endpoint}} readable by a busy person who will not read the code. They decide from the page: what is different, whether it works, whether it is safe and what you need from them. You decide what goes on that page, so write it for them and keep the detail for agents. If you have not connected yet, follow the "Set up Bee for a repository" prompt first. Nothing here blocks landing: the page and the checklist are advice, and a change request with no brief still lands when policy allows. A brief and evidence are how a person comes to trust it.

1. Ask what is still owed. This is a read, so run it as often as you like. It looks at the files you changed and says what a reader would want to see:

   ```sh
   bee cr checklist <id> --json
   ```

   `items` lists each ask in words (`detail`), the `kinds` of evidence that settle it and whether it is `satisfied`. `brief` wants an outcome and a highlight (asked for logic changes by agents), `brief:risk` wants a risk or a rollback plan (asked when a sensitive path changed) and `evidence:ui` wants a screenshot, a before and after or a video (asked when a UI file changed). The same list is printed by `bee cr open` when you open with a brief. You can pass a change request number instead of an id (`12` or `#12`).

2. Write the brief. One command, which replaces the previous brief:

   ```sh
   bee cr brief <id> \
     --outcome "Checkout shows the final price before the customer pays" \
     --highlight "Show the total with tax on the payment step::src/checkout/Payment.tsx,src/checkout/total.ts" \
     --highlight "Remove the old summary page::src/checkout/Summary.tsx" \
     --risk "Prices could round differently from the invoice::Rounding now shares one function with invoices" \
     --rollback "Revert this change; no data moves" \
     --ask "Show prices with tax included?::yes|no, show them before tax" \
     --try-it https://preview.example.com/checkout --json
   ```

   Write for someone who does not know the code. The outcome is one line (280 characters at most) about what is different for users, not what you edited. A highlight is one thing the change does, followed by `::` and the paths that do it. Name a risk you really see and what covers it; do not pad the list. Say how to undo the change. Use `--ask` for a decision you want a person to make, with two to six options split by `|`; their answer arrives as a comment `Decision on "<question>": <option>` (step 6). `--from FILE` takes the whole brief as JSON, which is handy when you build it in a script. You can also write the brief when you open the change: `bee cr open --title ... --outcome ... --highlight ...`.

3. Capture the change, then attach it. Prefer what a person can see to what you can say:
   - A UI change: a before and after of the same screen, captured the same way at 1440 by 900. The slider on the page only helps if both images line up.

     ```sh
     bee cr evidence add <id> --before before.png --after after.png \
       --alt "The payment step" --after-alt "The payment step with the total and tax shown" \
       --title "Payment step" --hero --json
     ```

     One image with no `--before` is a screenshot (`bee cr evidence add <id> shot.png --alt "..."`); several images make an image set.
   - A flow or an animation: a video of 30 seconds or less, with captions so it works with the sound off. Add a poster frame and say its size so the page can lay it out. Videos have no alt text requirement, but captions are how a person follows them.

     ```sh
     bee cr evidence add <id> demo.webm --captions demo.vtt --poster demo.png --alt "Poster of the demo" \
       --width 1440 --height 900 --duration-ms 24000 --title "Pay with a saved card" --json
     ```

   - A speed or size claim: the numbers against the target branch, not a sentence. Each `--bench` is `name:unit:baseline:value:lower|higher`, where the last word says which direction is better:

     ```sh
     bee cr evidence add <id> --bench "p95 latency:ms:120:85:lower" --bench "bundle size:kB:312:290:lower" --json
     ```

   - Tests: `--tests passed:failed:skipped[:duration_ms]` with `--summary "All 214 billing tests pass"`, and one `--failure "name::message"` for each failing test. A preview URL goes in with `--url https://...`, and a document (PDF, text, Markdown or CSV) as a file.

   Every image needs `--alt`: say what it shows, in words. Use the `--hero` flag on the one item that proves the change, so it leads the page and its thumbnail shows in the review queue. Keep files small: images up to 10 MiB, video up to 100 MiB; files over 8 MiB are uploaded in separate steps for you. Never attach an SVG or HTML file (they are refused), and never put a secret on screen.

4. Prefer evidence a machine verified. A person trusts "Verified by CI" over "From the agent". When your workflow already produces a screenshot, a recording or a test report, attach it from CI instead of uploading your own copy:

   ```sh
   bee cr evidence add <id> --artifact <artifact id> --kind screenshot --alt "The home page" shots/home.png
   bee cr evidence add <id> --check-run <check run id> --tests 214:0:3 --summary "All 214 billing tests pass"
   ```

   `--check-run` needs you to be the reporter of that check run, and the run must have tested a revision head of the change request. Evidence you upload yourself is labelled "From the agent".

5. Re-capture after you revise. Evidence belongs to the revision it was captured for; when you publish a new revision (`bee change revise-change`), earlier evidence is flagged stale on the page. List what you have, then add fresh evidence and remove what no longer shows the change:

   ```sh
   bee cr evidence list <id> --json     # look for "stale": true
   bee cr evidence add <id> --before before.png --after after.png --alt "..." --hero
   bee cr evidence rm <evidence id>
   ```

   Evidence about code you did not touch in the new revision is still true, so remove only what is wrong. Run `bee cr checklist <id>` again and update the brief with `bee cr brief` when what the change does moved.

6. Read the answers. People answer your `--ask` questions and leave comments. Read them before you continue, and act on them:

   ```sh
   bee change list-change-comments --repo {{repo}} --change-request-id <id> --json
   ```

   Look for comments that start with `Decision on "`: the question, then the option they chose. Do what the decision says, then update the brief. Over MCP, the brief and evidence tools are `brief_set_change_brief`, `brief_get_brief_checklist`, `brief_add_evidence`, `brief_begin_evidence_upload`, `brief_finalize_evidence`, `brief_list_evidence` and `brief_delete_evidence`, in the default `core` toolset; `brief_add_evidence` takes files inline as base64 (up to 8 MiB in total).

Errors you will meet: `INVALID_ARGUMENT` with the subcode `brief_too_long` (shorten the outcome or the list), `missing_alt` (add `--alt`), `bad_media` (the file is not the type its name says, or is SVG or HTML), `bad_evidence` (the files do not fit the kind, for example a screenshot with two images), `bad_url` (a try-it or preview URL must be https); `TOO_LARGE` (a file or the change request is over its limit: 64 items and 512 MiB; remove old evidence or shrink the file); `PERMISSION_DENIED` (only the author, or someone who may commit, edits the brief and evidence); `VERSION_MISMATCH` (the change request moved; read it again and retry). Do not retry these unchanged.

Report the change request id, what the checklist says now, the outcome line you wrote, the evidence you attached with its source, and any `Decision on` answers you acted on.
