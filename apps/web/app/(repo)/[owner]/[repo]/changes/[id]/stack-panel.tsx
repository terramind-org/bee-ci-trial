import { CopyAs } from "@/components/copy-as";
import { Icon } from "@/components/icons";
import { Badge, Panel } from "@/components/ui";
import type { Op } from "@/lib/copyas";
import type { StackMember } from "@/lib/infra";

/** Words shown for a member that merged with no commit written (SPEC §7.4 step 3, `allow_empty`). */
export const LANDED_EMPTY = "merged empty (no changes after rebase)";

/**
 * The changesets a change lands, first-parent chain, newest first. A merge changeset is marked
 * with the merge glyph, its label and the short ids of its other parents: those side branches land
 * with it. A member that rebased to nothing says so.
 */
export function StackList({ members }: { members: StackMember[] }) {
  return (
    <ol className="divide-y divide-line" data-testid="stack-list">
      {members.map((m) => (
        <li key={m.id} data-testid="stack-member" className="flex flex-wrap items-center gap-x-2 gap-y-1 px-3.5 py-2 text-sm">
          {m.merge ? <Icon name="branch" size={14} className="shrink-0 text-ink-2" /> : <Icon name="dot" size={14} className="shrink-0 text-ink-3" />}
          <span className="mono text-xs">{m.short}</span>
          {m.merge ? (
            <>
              <Badge tone="outline">merge</Badge>
              <span className="text-xs text-ink-3">
                also parents: <span className="mono">{m.otherParents.join(", ")}</span>
              </span>
            </>
          ) : null}
          {m.empty ? <span className="text-xs text-ink-2">{LANDED_EMPTY}</span> : null}
        </li>
      ))}
    </ol>
  );
}

export function StackPanel({ members, op }: { members: StackMember[]; op: Op }) {
  return (
    <Panel title="Stack" id="stack-title" actions={<CopyAs compact op={op} />}>
      <StackList members={members} />
    </Panel>
  );
}
