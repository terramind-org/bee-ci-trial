/**
 * The number beside "Needs you" in the rail: what needs you, read from the same scan the Inbox's
 * "Needs you" list renders (`loadNeeds`, cached per request), so the two always agree. It streams
 * into the rail inside a Suspense boundary, so no page waits for it, and it renders nothing at
 * zero or on any error (the Inbox itself reports errors).
 */
import { loadNeeds, needCount } from "@/lib/inbox/load";

export async function InboxCount() {
  const n = await loadNeeds()
    .then(needCount)
    .catch(() => 0);
  if (!n) return null;
  return (
    <>
      <span aria-hidden="true" className="num text-ink">
        {n}
      </span>
      <span className="sr-only">{", " + n + " waiting"}</span>
    </>
  );
}
