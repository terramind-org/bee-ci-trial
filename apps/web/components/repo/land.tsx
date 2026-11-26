/** A landed changeset as one line: who, what, which id, when. Shared by the latest-land line and the recent list. */
import { Mono, PrincipalMark } from "@/components/domain";
import { Time } from "@/components/time";
import { objectIdText } from "@/lib/api/ids";
import type { Changeset, Principal } from "@/lib/api/types";

/** The committing principal as the code browser derives it: agents carry provenance. */
export function landAuthor(c: Changeset): Principal {
  return {
    id: c.meta?.creator ?? "",
    kind: c.meta?.provenance ? "PRINCIPAL_KIND_AGENT" : "PRINCIPAL_KIND_HUMAN",
    display: c.meta?.author ?? "",
    ownerId: "",
    runtime: c.meta?.provenance?.runtime.split("/")[0] ?? "",
    externalIds: [],
  };
}

export function LandLine({ changeset, className, stacked = false }: { changeset: Changeset; className?: string; stacked?: boolean }) {
  const author = landAuthor(changeset);
  const message = (changeset.meta?.message ?? "").split("\n")[0] || "(no message)";
  const id = (
    <Mono className="shrink-0 text-xs text-ink-3" title={objectIdText(changeset.id)}>
      {objectIdText(changeset.id, true).replace(/^c\d_/, "")}
    </Mono>
  );
  const when = <Time iso={changeset.meta?.committerTime?.time ?? changeset.meta?.authorTime?.time} className="shrink-0 text-xs text-ink-3" />;
  if (stacked) {
    // Two lines for narrow columns: the message, then who, which id and when.
    return (
      <div className={"flex min-w-0 flex-col gap-1 text-sm " + (className ?? "")}>
        <span className="truncate" title={message}>
          {message}
        </span>
        <span className="flex min-w-0 items-center gap-2 text-xs text-ink-3">
          <PrincipalMark principal={author} size={16} />
          <span className="min-w-0 truncate text-ink-2">{author.display}</span>
          {id}
          {when}
        </span>
      </div>
    );
  }
  return (
    <div className={"flex min-w-0 items-center gap-2.5 text-sm " + (className ?? "")}>
      <PrincipalMark principal={author} size={20} />
      <span className="max-w-[7rem] shrink-0 truncate font-medium sm:max-w-[12rem]">{author.display}</span>
      <span className="min-w-0 flex-1 truncate text-ink-2" title={message}>
        {message}
      </span>
      {id}
      {when}
    </div>
  );
}
