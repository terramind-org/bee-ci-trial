import { Icon } from "@/components/icons";
import type { AuditEntry, ListAuditResponse } from "@/lib/api/types";
import { absoluteTime } from "@/lib/format";

const ACTIONS = new Map([
  ["change.open", "open a pull request"],
  ["change.land", "merge a pull request"],
  ["change.close", "close a pull request"],
  ["changeset.create", "create a commit"],
  ["commit.create", "create a commit"],
  ["import.complete", "complete an import"],
  ["lease.acquire", "claim files"],
  ["lease.release", "release a file claim"],
  ["member.set", "change membership"],
  ["member.add", "add a member"],
  ["member.remove", "remove a member"],
  ["member.role_change", "change a member’s role"],
  ["org_invitation.create", "invite someone"],
  ["org_invitation.accept", "accept an invitation"],
  ["org_invitation.cancel", "cancel an invitation"],
  ["org_invitation.decline", "decline an invitation"],
  ["policy.update", "update rules"],
  ["release.create", "create a release"],
  ["release.publish", "publish a release"],
  ["review.submit", "submit a review"],
  ["token.mint", "issue a token"],
  ["token.revoke", "revoke a token"],
]);

const FAILURES = new Map([
  ["gap", "an entry is missing"],
  ["hash_mismatch", "the entry's hash does not match"],
  ["prev_hash_mismatch", "the link to the previous entry does not match"],
  ["head_mismatch", "the last entry's hash does not match the recorded head"],
]);

export function verificationLine(audit: ListAuditResponse): string {
  if (audit.chainVerified) {
    return audit.verifiedToSeq === "0" ? "The record is intact and has no entries." : "The record is intact up to entry " + audit.verifiedToSeq + ".";
  }
  const where = audit.firstBrokenSeq === "0" ? "" : " at entry " + audit.firstBrokenSeq;
  const why = FAILURES.get(audit.verificationFailure) ?? audit.verificationFailure;
  return "The record failed verification" + where + (why ? " because " + why : "") + ".";
}

export function RecordEntry({ entry, personMode = false }: { entry: AuditEntry; personMode?: boolean }) {
  const { actor, answerable, onBehalfOf, action, target, repo, allowed, rule, time, seq, hash, prevHash, source } = entry;
  const person = actor?.kind === "PRINCIPAL_KIND_AGENT" ? answerable : undefined;
  const actionWords = ACTIONS.get(action);
  const when = absoluteTime(time);
  return (
    <>
      <span className={personMode ? "text-md text-ink" : undefined}>
        <span className={personMode ? "font-medium" : undefined}>{actor?.display || actor?.id || "Unknown actor"}</span>
        {person ? (
          <>
            {" on behalf of "}
            <span className={personMode ? "inline-flex items-center gap-1.5" : undefined}>
              {personMode ? <span className="dot" aria-hidden="true" /> : null}
              {person.display || person.id}
            </span>
          </>
        ) : onBehalfOf.length ? " on behalf of " + onBehalfOf.join(" → ") : ""}
        {" requested "}
        {actionWords ? "to " + actionWords : <code className="mono">{action}</code>}
        {target ? <> for <span>“{target}”</span></> : null}
        {repo ? <> in <code className="mono">{repo}</code></> : null}
        {" and was "}
        <span className="inline-flex items-center gap-1">
          {personMode ? <Icon name={allowed ? "check" : "x"} size={14} /> : null}
          {allowed ? "allowed" : "refused"}
        </span>
        {rule ? <> under <code className="mono">{rule}</code></> : null}
        {when ? <> on <time dateTime={time}>{when}</time></> : null}
        {"."}
      </span>
      <span className="mt-1 block text-sm text-ink-2 [overflow-wrap:anywhere]">
        Entry <span className="num">{seq}</span>
        {actionWords ? <> · <code className="mono">{action}</code></> : null}
        {person && onBehalfOf.length > 1 ? <> · Delegation {onBehalfOf.join(" → ")}</> : null}
        {source ? <> · {source}</> : null}
        {hash ? <> · Hash <code className="mono">{hash}</code></> : null}
        {prevHash ? <> · Previous hash <code className="mono">{prevHash}</code></> : null}
      </span>
    </>
  );
}
