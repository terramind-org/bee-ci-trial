"use client";

import { useCallback, useState } from "react";
import type { ChangeState, Event } from "@/lib/api/types";
import { MergeRefusal } from "../change-actions";
import { LiveRefresh } from "../live-refresh";

const KINDS = ["cr.*", "merge_queue.*", "review.*", "review_thread.*", "cr_comment.*", "check.*", "evidence.*", "post.*"];

/** Queue departure can also mean a successful merge or voluntary removal. Read the actual reason. */
export function isPolicyEjection(event: Pick<Event, "kind" | "payloadJson">): boolean {
  if (event.kind !== "merge_queue.left" || !event.payloadJson) return false;
  try {
    const payload: unknown = JSON.parse(event.payloadJson);
    return typeof payload === "object" && payload !== null && "eject_reason" in payload && payload.eject_reason === "POLICY_DENIED";
  } catch {
    return false;
  }
}

export function PullLive({ repo, id, state }: { repo: string; id: string; state: ChangeState }) {
  const [refusal, setRefusal] = useState<string | null>(null);
  const onNewEvent = useCallback((event: Event) => {
    if (isPolicyEjection(event)) setRefusal(repo + "/" + id + "/queue-refusal/" + event.seq);
    else if (event.kind === "merge_queue.entered" || event.kind === "cr.revised" || event.kind === "cr.closed") setRefusal(null);
  }, [repo, id]);
  return (
    <>
      <LiveRefresh repo={repo} ids={[id]} kinds={KINDS} onNewEvent={onNewEvent} />
      {state === "CHANGE_STATE_OPEN" && refusal
        ? <MergeRefusal eventKey={refusal} text="The rules refused this merge and removed the pull request from the merge queue." reason="POLICY_DENIED" />
        : null}
    </>
  );
}
