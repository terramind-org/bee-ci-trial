---
name: writing-for-people
description: Use this when writing or revising a Beehive title, pull request summary, task hand-back or post for a person to read.
---

# Writing for people

1. **Lead with what changed.** In the first line, name the thing that works
   differently and why anyone cares. Use a short action for a title. Use one
   sentence about the result for a summary. Keep the branch or file name if it
   helps the reader find the change. Put plumbing details later in the brief.

2. **Make decisions and evidence easy to find.** Say what you checked and what
   is still uncertain. If you need an answer, name the decision and what it
   affects. In a hand-back, record done, deviations, concerns and follow-ups. Name
   the pull request and its actual state. In a post, open with the result and put
   details in the Markdown body or article. Attach the work it describes.

3. **Use the words people already know.** Write pull request, commit, branch,
   merge and checks. Keep the theme to three terms in user-facing copy. Use
   Swarm for coordinated work, Queen · lead for its lead, and `bee land` when
   you need to name the command. Use exact identifiers in commands and logs.
   Read your first two lines aloud. If the reader cannot tell what changed or
   what you need from them, put that information there.

4. **Use these five good examples as models.** The examples are fictional.

   1. Title. "Keep saved filters when people switch projects."
   2. Summary. "The invoice includes tax before payment. The checkout check passed."
   3. Decision. "Approve the one-day export delay by Friday so Support can update customers."
   4. Hand-back. "Pull request #42 fixes duplicate reminders. Tests passed. Delivery timing still needs a staging check."
   5. Post. "You can now download receipts from the order page."

5. **Rewrite these five weak examples.**

   1. "Refactor persistence layer." → "Remember filters after a page reload."
   2. "Everything is green." → "Checkout and tax checks passed. The refund check is still running."
   3. "Please advise." → "Choose which branch gets the fix first. The support release cuts on Friday."
   4. "Completed the integration work." → "Pull request #61 adds billing retries. The payment sandbox was unavailable, so the end-to-end check is still open."
   5. "An improved export experience is here." → "CSV exports now include the currency next to each amount."
