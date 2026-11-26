"use client";

import { useState } from "react";
import type { EnterpriseScope, IpAllowEntry } from "@/lib/api/types";
import { CopyAs } from "../copy-as";
import { Badge, Button, EmptyState, Field, Input } from "../ui";
import { isCidr } from "./cidr";
import { ActionError, Check, ConfirmButton, actionsRow, useAdminAction } from "./kit";

/** The IP allow list: entries, add (CIDR checked here, the server has the last word) and delete. */
export function IpAllowPanel({ scope, entries }: { scope: EnterpriseScope; entries: IpAllowEntry[] }) {
  const [cidr, setCidr] = useState("");
  const [description, setDescription] = useState("");
  const [active, setActive] = useState(true);
  const [touched, setTouched] = useState(false);
  const add = useAdminAction();
  const row = useAdminAction();
  const invalid = touched && cidr.trim() !== "" && !isCidr(cidr);
  const request = { scope, cidr: cidr.trim(), description: description || undefined, active };

  return (
    <div>
      <form
        className="flex flex-col gap-3 px-3.5 py-3.5"
        noValidate
        onSubmit={async (e) => {
          e.preventDefault();
          setTouched(true);
          if (!isCidr(cidr)) return;
          const res = await add.run("createIpAllowListEntry", request, "Added " + cidr.trim());
          if (res) {
            setCidr("");
            setDescription("");
            setTouched(false);
          }
        }}
      >
        <div className="grid grid-cols-1 gap-3.5 md:grid-cols-[220px_minmax(0,1fr)]">
          <Field label="Address or CIDR" htmlFor="ip-cidr" error={invalid ? "Not an IPv4 or IPv6 address or CIDR block, like 203.0.113.0/24." : undefined} hint="For example 203.0.113.0/24 or 2001:db8::/32.">
            <Input id="ip-cidr" placeholder="203.0.113.0/24" value={cidr} aria-invalid={invalid} onChange={(e) => setCidr(e.target.value)} onBlur={() => setTouched(true)} autoComplete="off" />
          </Field>
          <Field label="Description" htmlFor="ip-desc">
            <Input id="ip-desc" placeholder="Office network" value={description} onChange={(e) => setDescription(e.target.value)} />
          </Field>
        </div>
        <Check id="ip-active" label="Active" hint="An inactive entry is kept but not applied." checked={active} onChange={setActive} />
        <ActionError error={add.error} />
        <div className={actionsRow}>
          <Button type="submit" variant="primary" disabled={add.busy || !cidr.trim()}>
            Add entry
          </Button>
          <CopyAs op={{ service: "EnterpriseService", method: "CreateIpAllowListEntry", request, label: "Add an allow-list entry" }} />
        </div>
      </form>
      <div className="border-t border-line">
        {entries.length === 0 ? (
          <EmptyState title="No entries" icon="lock">
            Add an address range above.
          </EmptyState>
        ) : (
          <ul aria-label="Allowed addresses" className="divide-y divide-line">
            {entries.map((e) => {
              const del = { scope, entryId: e.id };
              return (
                <li key={e.id} className="flex flex-col gap-1.5 px-3.5 py-3 md:flex-row md:items-center md:gap-3 md:py-2.5">
                  <span className="flex items-center gap-3">
                    <span className="mono text-sm font-medium">{e.cidr}</span>
                    <Badge tone={e.active ? "ok" : "neutral"}>{e.active ? "Active" : "Inactive"}</Badge>
                  </span>
                  <span className="min-w-0 text-sm text-ink-2 [overflow-wrap:anywhere] md:flex-1 md:truncate">{e.description}</span>
                  <span className="flex items-center gap-1">
                    <ConfirmButton label="Delete" title={"Delete " + e.cidr + "?"} description="Traffic from this range is no longer allowed by this entry." confirmLabel="Delete entry" disabled={row.busy} onConfirm={() => void row.run("deleteIpAllowListEntry", del, "Deleted " + e.cidr)} />
                    <CopyAs compact op={{ service: "EnterpriseService", method: "DeleteIpAllowListEntry", request: del, label: "Delete this entry" }} />
                  </span>
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
