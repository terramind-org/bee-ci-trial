import type { Metadata } from "next";
import { PageFacts } from "@/components/bee/page-facts";
import { State } from "@/components/parts";
import { PageMenu } from "@/components/menu";
import { ImportWizard } from "@/components/import-wizard";
import { Time } from "@/components/time";
import { EmptyState, PageHeader, Panel } from "@/components/ui";
import { currentOrg, serverClient } from "@/lib/api/server";
import { toolFor } from "@/lib/tools";
import { requestMode } from "../../request-mode";
import { guard } from "@/lib/page";

export const metadata: Metadata = { title: "Import" };

export default async function ImportPage() {
  return guard(async () => {
    const api = await serverClient();
    const [org, { mode }] = await Promise.all([currentOrg(), requestMode()]);
    const req = { targetOrg: org };
    const { imports } = await api.import.listImports(req);
    if (mode === "bee") {
      return <PageFacts page="import" title="Import from GitHub" tools={[
        toolFor("ImportService/ListImports", "List imports for the organization"),
        toolFor("ImportService/GetImport", "Read the status of an import"),
        toolFor("ImportService/GetFidelityReport", "Read what was imported and what was dropped"),
        toolFor("ImportService/StartImport", "Start with a dry run, then mirror the repository"),
      ]} groups={[
        { title: "destination", facts: [{ label: "organization", value: org }] },
        { title: "previous imports", lines: imports.map((i) => [i.id, i.source, phase(i.phase), i.startedAt].filter(Boolean).join(" · ")), empty: "Nothing imported yet." },
      ]} />;
    }
    return (
      <>
        <PageHeader title="Import from GitHub" lede={"Run an estimate before mirroring into " + org + "."} actions={<PageMenu op={{ service: "ImportService", method: "ListImports", request: req, label: "List imports" }} />} />
        <div className="grid grid-cols-1 gap-12 lg:grid-cols-[minmax(0,1fr)_minmax(0,250px)]">
          <ImportWizard org={org} />
          <Panel title="Previous imports" id="prev-imports">
            {imports.length === 0 ? <EmptyState title="Nothing imported yet" /> : null}
            <ul className="rule divide-y divide-line">
              {imports.map((i) => (
                <li key={i.id} className="py-3 text-base">
                  <p className="mono break-all">{i.source}</p>
                  <p className="mt-0.5 flex items-center gap-2 text-sm text-ink-3">
                    <State shape={i.liveSince ? "passed" : "open"} className="text-ink-2">{phase(i.phase)}</State>
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

function phase(value: string) {
  return value.replace("IMPORT_PHASE_", "").toLowerCase().replaceAll("_", " ");
}
