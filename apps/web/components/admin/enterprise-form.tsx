"use client";

import { useState } from "react";
import type { Enterprise } from "@/lib/api/types";
import { CopyAs } from "../copy-as";
import { Button, Field, Input, Textarea } from "../ui";
import { ActionError, actionsRow, changed, formClass, useAdminAction } from "./kit";

/** The enterprise's name, description and website. */
export function EnterpriseForm({ enterprise }: { enterprise: Enterprise }) {
  const [name, setName] = useState(enterprise.name);
  const [description, setDescription] = useState(enterprise.description);
  const [websiteUrl, setWebsiteUrl] = useState(enterprise.websiteUrl);
  const { run, busy, error } = useAdminAction();
  const patch = changed({ name: enterprise.name, description: enterprise.description, websiteUrl: enterprise.websiteUrl }, { name, description, websiteUrl });
  const request = { enterprise: enterprise.slug, ...patch };
  const dirty = Object.keys(patch).length > 0;

  return (
    <form
      className={formClass}
      onSubmit={async (e) => {
        e.preventDefault();
        await run("updateEnterprise", request, "Enterprise saved");
      }}
    >
      <Field label="Name" htmlFor="ent-name" hint={<>Slug <span className="mono">{enterprise.slug}</span> does not change.</>}>
        <Input id="ent-name" value={name} onChange={(e) => setName(e.target.value)} required />
      </Field>
      <Field label="Description" htmlFor="ent-description">
        <Textarea id="ent-description" value={description} onChange={(e) => setDescription(e.target.value)} />
      </Field>
      <Field label="Website" htmlFor="ent-website">
        <Input id="ent-website" type="url" placeholder="https://" value={websiteUrl} onChange={(e) => setWebsiteUrl(e.target.value)} />
      </Field>
      <ActionError error={error} />
      <div className={actionsRow}>
        <Button type="submit" variant="primary" disabled={busy || !dirty}>
          Save
        </Button>
        <CopyAs op={{ service: "EnterpriseService", method: "UpdateEnterprise", request, label: "Update the enterprise" }} />
      </div>
    </form>
  );
}
