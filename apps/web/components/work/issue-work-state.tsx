import { Mark, Sep, State } from "@/components/parts";
import type { Issue } from "@/lib/api/types";
import type { IssueWork } from "@/lib/issues";

export function IssueWorkState({ issue, work }: { issue?: Pick<Issue, "state">; work: IssueWork }) {
  const text = work.personIsAssignee ? <Mark>{work.text}</Mark> : work.text;
  return (
    <>
      {issue ? <State shape={issue.state === "closed" ? "closed" : work.taken ? "running" : "open"}>{text}</State> : <span>{text}</span>}
      {work.person && !work.personIsAssignee ? (
        <>
          <Sep />
          <span className="inline-flex items-center gap-1.5">
            for <Mark>{work.person.display || work.person.id}</Mark>
          </span>
        </>
      ) : null}
    </>
  );
}
