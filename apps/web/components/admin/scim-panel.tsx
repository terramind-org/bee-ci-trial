"use client";

import { useState } from "react";
import type { CreateScimTokenResponse, EnterpriseScope, ScimConfig } from "@/lib/api/types";
import { CopyAs } from "../copy-as";
import { Time } from "../time";
import { Button, Field, Input, Select } from "../ui";
import { ActionError, Check, CopyValue, actionsRow, changed, formClass, useAdminAction } from "./kit";

const TTLS: Record<string, number> = { "30 days": 30 * 86_400, "90 days": 90 * 86_400, "1 year": 365 * 86_400 };

/** SCIM provisioning: the endpoint, the switches, and one-time bearer tokens. */
export function ScimPanel({ scope, config }: { scope: EnterpriseScope; config: ScimConfig }) {
  const initial = { enabled: config.enabled, provisionsTeams: config.provisionsTeams };
  const [f, setF] = useState(initial);
  const patch = changed(initial, f);
  const request = { scope, ...patch };
  const save = useAdminAction();

  const [description, setDescription] = useState("");
  const [ttl, setTtl] = useState("90 days");
  const mint = useAdminAction();
  const [secret, setSecret] = useState<CreateScimTokenResponse | null>(null);
  const tokenRequest = { scope, description: description || undefined, ttlSeconds: TTLS[ttl] };

  return (
    <div className={formClass}>
      <div className="flex flex-col gap-1.5">
        <CopyValue label="Endpoint" value={config.endpointUrl} />
      </div>
      <p className="text-xs text-ink-3">
        {config.activeTokenCount} active {config.activeTokenCount === 1 ? "token" : "tokens"} · {config.provisionedCount} provisioned
        {config.lastRequestAt ? (
          <>
            {" "}
            · last request <Time iso={config.lastRequestAt} />
          </>
        ) : null}
      </p>
      <form
        className="flex flex-col gap-3"
        onSubmit={async (e) => {
          e.preventDefault();
          await save.run("updateScimConfig", request, "SCIM saved");
        }}
      >
        <div className="flex flex-wrap gap-x-6 gap-y-2">
          <Check id="scim-enabled" label="Enabled" checked={f.enabled} onChange={(v) => setF((x) => ({ ...x, enabled: v }))} />
          <Check id="scim-teams" label="Provision teams" checked={f.provisionsTeams} onChange={(v) => setF((x) => ({ ...x, provisionsTeams: v }))} />
        </div>
        <ActionError error={save.error} />
        <div className={actionsRow}>
          <Button type="submit" variant="primary" disabled={save.busy || Object.keys(patch).length === 0}>
            Save
          </Button>
          <CopyAs op={{ service: "EnterpriseService", method: "UpdateScimConfig", request, label: "Update SCIM" }} />
        </div>
      </form>
      <form
        className="flex flex-col gap-3 border-t border-line pt-3.5"
        onSubmit={async (e) => {
          e.preventDefault();
          const res = await mint.run("createScimToken", tokenRequest, "SCIM token created", { detail: "Copy it now: it is not shown again." });
          if (res) setSecret(res.data);
        }}
      >
        <h3 className="text-sm font-medium">New token</h3>
        <div className="grid grid-cols-1 gap-3.5 md:grid-cols-[minmax(0,1fr)_160px]">
          <Field label="Description" htmlFor="scim-desc">
            <Input id="scim-desc" placeholder="Okta production" value={description} onChange={(e) => setDescription(e.target.value)} />
          </Field>
          <Field label="Expires after" htmlFor="scim-ttl">
            <Select id="scim-ttl" value={ttl} onChange={(e) => setTtl(e.target.value)}>
              {Object.keys(TTLS).map((k) => (
                <option key={k}>{k}</option>
              ))}
            </Select>
          </Field>
        </div>
        <ActionError error={mint.error} />
        {secret ? (
          <div role="status" data-testid="scim-secret" className="rounded-md border border-line-strong bg-sunken px-3 py-2.5">
            <p className="text-sm font-medium">Copy this token now. It is shown once.</p>
            <p className="mono mt-1 text-xs [overflow-wrap:anywhere]">{secret.token}</p>
            <div className="mt-2 flex gap-2">
              <Button size="sm" onClick={() => navigator.clipboard.writeText(secret.token)}>
                Copy token
              </Button>
              <Button size="sm" variant="ghost" onClick={() => setSecret(null)}>
                I have saved it
              </Button>
            </div>
          </div>
        ) : null}
        <div className={actionsRow}>
          <Button type="submit" disabled={mint.busy}>
            Create token
          </Button>
          <CopyAs op={{ service: "EnterpriseService", method: "CreateScimToken", request: tokenRequest, label: "Create a SCIM token" }} />
        </div>
      </form>
    </div>
  );
}
