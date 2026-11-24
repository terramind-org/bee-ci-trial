import type { Metadata } from "next";
import { PageMenu } from "@/components/menu";
import { ImportWizard } from "@/components/import-wizard";
import { Time } from "@/components/time";
import { Badge, PageHeader, Panel } from "@/components/ui";
import { currentOrg, serverClient } from "@/lib/api/server";
import { guard } from "@/lib/page";

export const metadata: Metadata = { title: "Import" };

export default async function ImportPage() {
  return guard(async () => {
    const api = await serverClient();
    const org = await currentOrg();
    const req = { targetOrg: org };
    const { imports } = await api.import.listImports(req);
    return (
      <>
        <PageHeader title="Import from GitHub" lede="Do a dry run first. Your Git history arrives with the same commit ids, and issues and pull requests keep their numbers." actions={<PageMenu op={{ service: "ImportService", method: "ListImports", request: req, label: "List imports" }} />} />
        <div className="grid grid-cols-1 gap-10 xl:grid-cols-[minmax(0,1fr)_320px]">
          <ImportWizard org={org} />
          <Panel title="Previous imports" id="prev-imports">
            {imports.length === 0 ? <p className="text-base text-ink-2">Nothing imported yet.</p> : null}
            <ul className="divide-y divide-line">
              {imports.map((i) => (
                <li key={i.id} className="py-3 text-base">
                  <p className="mono truncate">{i.source}</p>
                  <p className="mt-0.5 flex items-center gap-2 text-sm text-ink-3">
                    <Badge tone={i.liveSince ? "ok" : "warn"}>{i.phase.replace("IMPORT_PHASE_", "").toLowerCase().replace("_", " ")}</Badge>
                    <Time iso={i.startedAt} />
                  </p>
                </li>
              ))}
            </ul>
          </Panel>
        </div>
      </>
    );
  });
}
