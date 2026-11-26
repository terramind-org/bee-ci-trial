"use client";

import { useState } from "react";
import type { EnterpriseScope, VerifiedDomain } from "@/lib/api/types";
import { CopyAs } from "../copy-as";
import { Time } from "../time";
import { Badge, Button, EmptyState, Field, Input } from "../ui";
import { ActionError, ConfirmButton, CopyValue, actionsRow, useAdminAction } from "./kit";

const STATE: Record<string, { tone: "ok" | "warn" | "bad" | "neutral"; text: string }> = {
  VERIFIED_DOMAIN_STATE_VERIFIED: { tone: "ok", text: "Verified" },
  VERIFIED_DOMAIN_STATE_APPROVED: { tone: "ok", text: "Approved" },
  VERIFIED_DOMAIN_STATE_PENDING: { tone: "warn", text: "Pending" },
  VERIFIED_DOMAIN_STATE_FAILED: { tone: "bad", text: "Failed" },
};

/** What to publish in DNS to prove the domain is yours. */
function DnsRecord({ domain }: { domain: VerifiedDomain }) {
  return (
    <div data-testid="dns-record" className="mt-2 rounded-md border border-line px-3 py-2.5">
      <p className="mb-1.5 text-xs text-ink-3">Create this DNS TXT record, then check the domain.</p>
      <div className="flex flex-col gap-1.5">
        <CopyValue label="TXT record name" value={domain.dnsTxtRecordName} />
        <CopyValue label="TXT record value" value={domain.dnsTxtRecordValue} />
      </div>
      {domain.challengeExpiresAt ? (
        <p className="mt-1.5 text-xs text-ink-3">
          The challenge expires <Time iso={domain.challengeExpiresAt} />.
        </p>
      ) : null}
    </div>
  );
}

/** Verified domains: list, add, check against DNS, delete. */
export function DomainsPanel({ scope, domains }: { scope: EnterpriseScope; domains: VerifiedDomain[] }) {
  const [name, setName] = useState("");
  const [added, setAdded] = useState<VerifiedDomain | null>(null);
  const add = useAdminAction();
  const row = useAdminAction();
  const addRequest = { scope, domain: name.trim() };
  
  return (
    <div>
      <form
        className="flex flex-col gap-3 px-3.5 py-3.5"
        onSubmit={async (e) => {
          e.preventDefault();
          const res = await add.run("addVerifiedDomain", addRequest, "Domain added", { detail: "Create the TXT record shown below, then check it." });
          if (res) {
            setAdded(res.data);
            setName("");
          }
        }}
      >
        <Field label="Domain" htmlFor="domain-name" hint="For example example.com. Members with a verified email at the domain are recognised.">
          <Input id="domain-name" placeholder="example.com" value={name} onChange={(e) => setName(e.target.value)} required autoComplete="off" />
        </Field>
        <ActionError error={add.error} />
        <div className={actionsRow}>
          <Button type="submit" variant="primary" disabled={add.busy || !name.trim()}>
            Add domain
          </Button>
          <CopyAs op={{ service: "EnterpriseService", method: "AddVerifiedDomain", request: addRequest, label: "Add a verified domain" }} />
        </div>
        {added ? (
          <div data-testid="added-domain">
            <p className="text-sm">
              <span className="mono font-medium">{added.domain}</span> <Badge tone={STATE[added.state]?.tone ?? "neutral"}>{STATE[added.state]?.text ?? added.state}</Badge>
            </p>
            {added.dnsTxtRecordName ? <DnsRecord domain={added} /> : null}
          </div>
        ) : null}
      </form>
      <div className="border-t border-line">
        {domains.length === 0 ? (
          <EmptyState title="No domains yet" icon="pages">
            Add a domain to prove your organization owns it.
          </EmptyState>
        ) : (
          <ul aria-label="Verified domains" className="divide-y divide-line">
            {domains.map((d) => {
              const checkRequest = { scope, domainId: d.id };
              const deleteRequest = { scope, domainId: d.id };
              const st = STATE[d.state] ?? { tone: "neutral" as const, text: d.state };
              const open = d.state === "VERIFIED_DOMAIN_STATE_PENDING" || d.state === "VERIFIED_DOMAIN_STATE_FAILED";
              return (
                <li key={d.id} className="px-3.5 py-3">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="mono text-sm font-medium">{d.domain}</span>
                    <Badge tone={st.tone}>{st.text}</Badge>
                    {d.verifiedAt ? (
                      <span className="text-xs text-ink-3">
                        verified <Time iso={d.verifiedAt} />
                      </span>
                    ) : d.lastCheckedAt ? (
                      <span className="text-xs text-ink-3">
                        checked <Time iso={d.lastCheckedAt} />
                      </span>
                    ) : null}
                    <span className="ml-auto flex items-center gap-1">
                      {open ? (
                        <>
                          <Button size="sm" disabled={row.busy} onClick={() => row.run("checkVerifiedDomain", checkRequest, "Checked " + d.domain)}>
                            Check
                          </Button>
                          <CopyAs compact op={{ service: "EnterpriseService", method: "CheckVerifiedDomain", request: checkRequest, label: "Check this domain" }} />
                        </>
                      ) : null}
                      <ConfirmButton label="Delete" title={"Delete " + d.domain + "?"} description="The domain stops counting as verified. Members are not removed." confirmLabel="Delete domain" disabled={row.busy} onConfirm={() => void row.run("deleteVerifiedDomain", deleteRequest, "Deleted " + d.domain)} />
                      <CopyAs compact op={{ service: "EnterpriseService", method: "DeleteVerifiedDomain", request: deleteRequest, label: "Delete this domain" }} />
                    </span>
                  </div>
                  {open && d.dnsTxtRecordName && d.id !== added?.id ? <DnsRecord domain={d} /> : null}
                </li>
              );
            })}
          </ul>
        )}
        {row.error ? (
          <div className="px-3.5 pb-3">
            <ActionError error={row.error} />
          </div>
        ) : null}
      </div>
    </div>
  );
}
