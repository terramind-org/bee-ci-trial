/**
 * The number beside "Inbox" in the rail: what needs you, read from the same scan the Inbox's
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
      <span aria-hidden="true" className="num min-w-6 rounded-full bg-accent px-1.5 text-center text-sm leading-6 font-semibold text-on-accent">
        {n}
      </span>
      <span className="sr-only">{n === 1 ? ", 1 needs you" : ", " + n + " need you"}</span>
    </>
  );
}
