import { RepoName } from "@/components/discover/rank-row";
import { State, StateShape } from "@/components/parts";
import { Time } from "@/components/time";
import type { Visibility } from "@/lib/api/types";
import { plural } from "@/lib/format";
import { repoHref } from "@/lib/routes";

export interface RepositoryRow {
  fullName: string;
  description?: string;
  visibility?: Visibility;
  activity?: string;
  agents?: number;
  needsYou?: number;
}

const visibilityLabel: Record<Visibility, string> = {
  VISIBILITY_PUBLIC: "Public",
  VISIBILITY_PRIVATE: "Private",
  VISIBILITY_INTERNAL: "Internal",
  VISIBILITY_UNSPECIFIED: "",
};

const editing = (n: number) => plural(n, "agent") + " editing now";
const needsYou = (n: number) => plural(n, "pull request") + (n === 1 ? " needs you" : " need you");

export function repositoryLine(row: RepositoryRow): string {
  return [
    row.fullName, row.visibility && visibilityLabel[row.visibility], row.description,
    row.agents !== undefined && editing(row.agents),
    row.needsYou !== undefined && needsYou(row.needsYou),
    row.activity && "updated " + row.activity, repoHref(row.fullName),
  ].filter(Boolean).join(" · ");
}

export function RepositoryList({ rows }: { rows: RepositoryRow[] }) {
  return (
    <ul className="rule divide-y divide-line" aria-label="Repositories">
      {rows.map((row) => (
        <li key={row.fullName} className="py-4" data-repository={row.fullName}>
          <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
            <RepoName fullName={row.fullName} className="min-h-6 max-w-full break-all whitespace-normal text-lg" />
            {row.visibility && visibilityLabel[row.visibility] ? (
              <State shape={row.visibility === "VISIBILITY_PUBLIC" ? "open" : "closed"} className="text-sm text-ink-2">{visibilityLabel[row.visibility]}</State>
            ) : null}
            {row.activity ? <span className="ml-auto text-sm text-ink-3">Updated <Time iso={row.activity} /></span> : null}
          </div>
          {row.description ? <p className="mt-1 truncate text-base text-ink-2" title={row.description}>{row.description}</p> : null}
          {row.agents !== undefined || row.needsYou !== undefined ? (
            <p className="mt-1 flex flex-wrap items-center gap-x-4 gap-y-1 text-sm text-ink-3">
              {row.agents !== undefined ? <span>{editing(row.agents)}</span> : null}
              {row.needsYou !== undefined ? (
                <span className="inline-flex items-center gap-1.5">
                  {row.needsYou > 0 ? <StateShape shape="open" /> : null}
                  {needsYou(row.needsYou)}
                </span>
              ) : null}
            </p>
          ) : null}
        </li>
      ))}
    </ul>
  );
}
