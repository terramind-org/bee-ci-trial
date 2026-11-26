/**
 * The For agents tab (docs/spec/web-ui.md §3): the calls that act on this change, each with Copy
 * as, how to fetch `change.md` with a token, and the document itself, rendered. `change.md` is the
 * same bytes an agent gets from `…/change.md` (`lib/changemd.ts`).
 */
import { CopyAs, PageOp } from "@/components/copy-as";
import { Markdown } from "@/components/markdown";
import { Panel } from "@/components/ui";
import { scopeFromParams } from "@/lib/api/server";
import { changeActions, renderChangeMd } from "@/lib/changemd";
import { evidenceIndex } from "@/lib/evidence-view";
import { guard } from "@/lib/page";
import { requestOrigin } from "@/lib/prompt-endpoint";
import { repoHref } from "@/lib/routes";
import { api, changeMdInput } from "../data";

/** The document without its title line, which the page already shows as its heading. */
function body(md: string): string {
  return md.replace(/^# [^\n]*\n+/, "");
}

export default async function AgentsTab({ params }: { params: Promise<{ owner: string; repo: string; id: string }> }) {
  const { id } = await params;
  const { fullName } = await scopeFromParams(params);
  return guard(async () => {
    const origin = await requestOrigin();
    const input = await changeMdInput(await api(), fullName, id, origin);
    const md = renderChangeMd(input);
    const actions = changeActions({ repo: fullName, change: input.change, comments: input.comments });
    const url = origin + repoHref(fullName, "changes/" + id + "/change.md");
    const fetchLine = 'curl -H "Authorization: Bearer $BEE_TOKEN" ' + url;
    return (
      <div className="flex min-w-0 flex-col gap-14">
        <PageOp op={actions[0]?.op ?? { service: "ChangeService", method: "GetChange", request: { repo: { fullName }, id } }} />
        <section aria-labelledby="agents-fetch" className="flex flex-col gap-4">
          <h2 id="agents-fetch" className="text-xl font-semibold tracking-tight">
            Everything in one file
          </h2>
          <p className="max-w-[68ch] text-md text-ink-2">
            <a href={repoHref(fullName, "changes/" + id + "/change.md")} className="mono text-sm text-ink underline underline-offset-4 hover:decoration-ink">
              change.md
            </a>{" "}
            holds the brief, the checklist, the evidence with links, checks, reviews, threads, what changed, the file list and the full diff. It is the same for the same change, so an agent can compare it between reads. Fetch it with the token you already use:
          </p>
          <pre className="mono rounded-lg bg-sunken px-4 py-3 text-sm whitespace-pre-wrap text-ink [overflow-wrap:anywhere]" data-fetch-line>
            {fetchLine}
          </pre>
        </section>

        <Panel title="Act on it" id="agents-actions">
          <ol className="flex flex-col divide-y divide-line" data-actions>
            {actions.map((a) => (
              <li key={a.title} className="flex min-h-11 items-center justify-between gap-4 py-2">
                <span className="min-w-0 text-base text-ink">{a.title}</span>
                <CopyAs compact op={{ ...a.op, label: a.title }} />
              </li>
            ))}
          </ol>
        </Panel>

        <section aria-labelledby="agents-doc" className="flex min-w-0 flex-col gap-4">
          <h2 id="agents-doc" className="text-xl font-semibold tracking-tight">
            change.md, rendered
          </h2>
          <Markdown source={body(md)} repo={fullName} evidence={evidenceIndex(input.evidence ?? [])} className="[&_h2]:mt-10 [&_pre]:overflow-x-visible [&_pre]:whitespace-pre-wrap [&_pre]:[overflow-wrap:anywhere]" />
        </section>
      </div>
    );
  });
}
