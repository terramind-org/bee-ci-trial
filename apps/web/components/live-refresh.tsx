"use client";

import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";
import { browserClient } from "@/lib/api/browser";
import type { Event } from "@/lib/api/types";
import { refreshDelay } from "@/lib/presence";

/**
 * Keeps a server-rendered screen current without polling: one EventService.Subscribe stream for
 * the repo; once the retained events have replayed, every event that concerns `ids` (or any event
 * of `kinds` when no ids are given) re-renders the page from the server.
 */
export function LiveRefresh({ repo, kinds, ids = [], onNewEvent }: { repo: string; kinds: string[]; ids?: string[]; onNewEvent?: (event: Event) => void }) {
  // When this client mounted, less a margin for clock skew between the bee server and here.
  const [since] = useState(() => Date.now() - 5000);
  const router = useRouter();
  const key = ids.join(",");
  const kindKey = kinds.join(",");
  useEffect(() => {
    const ctl = new AbortController();
    const watch = key ? key.split(",") : [];
    const concerns = (e: Event) =>
      !watch.length ||
      watch.some((id) => e.change?.id === id || e.review?.changeId === id || (e.note?.target ?? "").endsWith(":" + id)) ||
      // Checks name the changeset, not the change request: any check may be this change's.
      e.kind.startsWith("check.");
    let live = false;
    let quiet: ReturnType<typeof setTimeout> = setTimeout(() => (live = true), 600);
    let pending: ReturnType<typeof setTimeout> | null = null;
    let firstPending = 0;
    (async () => {
      try {
        const api = browserClient().event;
        const scope = { repo: { fullName: repo } };
        const options = { signal: ctl.signal };
        // An event picture needs an exact history boundary. The refresh below can still use its
        // existing recent-event replay to reconcile server data read before this stream opened.
        const after = onNewEvent ? await api.getEventLogInfo(scope, options).then((info) => BigInt(info.latestSeq), () => null) : null;
        for await (const e of api.subscribe({ ...scope, fromSeq: "0", kinds: kindKey.split(",") }, options)) {
          if (after !== null && onNewEvent && BigInt(e.seq) > after && concerns(e)) onNewEvent(e);
          // History replays first; an event stamped after the page was rendered is live even mid-replay.
          if (!live && !(e.time && Date.parse(e.time) >= since)) {
            // Still replaying history: wait for the stream to go quiet before reacting.
            clearTimeout(quiet);
            quiet = setTimeout(() => (live = true), 150);
            continue;
          }
          if (!concerns(e)) continue;
          // A burst of related events becomes one refresh: wait for 250ms of quiet, but never more
          // than 1s after the first one, so a steady stream still updates the page promptly.
          if (pending) clearTimeout(pending);
          if (!firstPending) firstPending = Date.now();
          pending = setTimeout(() => {
            pending = null;
            firstPending = 0;
            router.refresh();
          }, refreshDelay(firstPending, Date.now()));
        }
      } catch {
        // The page stays as rendered; the activity stream shows connection state.
      }
    })();
    return () => {
      ctl.abort();
      clearTimeout(quiet);
      if (pending) clearTimeout(pending);
    };
  }, [repo, key, kindKey, router, since, onNewEvent]);
  return null;
}
