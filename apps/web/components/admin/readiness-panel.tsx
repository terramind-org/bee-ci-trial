"use client";

import { useState } from "react";
import type { ValidateOpsConfigResponse } from "@/lib/api/types";
import { CopyAs } from "../copy-as";
import { Panel, Textarea, Button } from "../ui";
import { ActionError, useAdminAction } from "./kit";
import { ReadinessList, checksFrom, isChecklistItem, summarize } from "./readiness-list";

const editionOf = (toml: string): "cloud" | "enterprise" => (/^\s*edition\s*=\s*["']cloud["']/m.test(toml) ? "cloud" : "enterprise");

/**
 * The readiness checklist for a configuration. It opens on the config the server is running (secrets
 * redacted) and its result; Re-run checks the text as it stands, so a candidate change can be
 * tried before it is deployed.
 */
export function ReadinessPanel({ initialToml, initial, notice }: { initialToml: string; initial: ValidateOpsConfigResponse | null; notice?: string }) {
  const [toml, setToml] = useState(initialToml);
  const [result, setResult] = useState<ValidateOpsConfigResponse | null>(initial);
  const { run, busy, error } = useAdminAction();
  const request = { toml, component: "production" };

  async function rerun() {
    const res = await run("validateOpsConfig", request, "Checks re-run", { refresh: false });
    if (res) setResult(res.data);
  }

  const parseErrors = result ? result.errors.filter((e) => !isChecklistItem(e)) : [];
  const checks = result && parseErrors.length === 0 ? checksFrom(result, editionOf(toml)) : [];

  return (
    <div className="grid grid-cols-1 gap-5 xl:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
      <Panel
        title="Checklist"
        id="readiness-checks"
        actions={
          <>
            {result ? <span className="text-sm text-ink-3" data-testid="readiness-summary">{parseErrors.length ? "Configuration does not parse" : summarize(checks)}</span> : null}
            <Button size="sm" icon="refresh" disabled={busy} onClick={rerun}>
              Re-run
            </Button>
            <CopyAs compact op={{ service: "OpsService", method: "ValidateOpsConfig", request, label: "Run the production checklist" }} />
          </>
        }
      >
        {parseErrors.length ? (
          <ul aria-label="Configuration errors" className="divide-y divide-line">
            {parseErrors.map((e, i) => (
              <li key={i} className="px-3.5 py-3 text-sm">
                <p className="text-bad [overflow-wrap:anywhere]">{e.message}</p>
                <p className="mono text-xs text-ink-3">
                  {e.code}
                  {e.keyPath ? " · " + e.keyPath : ""}
                  {e.line ? " · line " + e.line + ", column " + e.column : ""}
                </p>
              </li>
            ))}
          </ul>
        ) : checks.length ? (
          <ReadinessList checks={checks} />
        ) : (
          <p className="px-3.5 py-6 text-sm text-ink-2">{notice ?? "Press Re-run to check the configuration on the right."}</p>
        )}
        {error ? (
          <div className="border-t border-line px-3.5 py-3">
            <ActionError error={error} />
          </div>
        ) : null}
      </Panel>
      <Panel title="Configuration" id="readiness-config" bodyClassName="p-3.5">
        <label htmlFor="readiness-toml" className="mb-1.5 block text-sm text-ink-2">
          server.toml as the server runs it. Secrets show as <span className="mono">&lt;redacted&gt;</span> and count as set. Edit it to try a change, then re-run.
        </label>
        <Textarea id="readiness-toml" rows={22} spellCheck={false} className="mono text-xs" value={toml} onChange={(e) => setToml(e.target.value)} />
      </Panel>
    </div>
  );
}
