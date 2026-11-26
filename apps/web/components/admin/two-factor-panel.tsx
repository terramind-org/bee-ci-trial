"use client";

import { useState } from "react";
import type { EnterpriseScope, TwoFactorRequirement } from "@/lib/api/types";
import { CopyAs } from "../copy-as";
import { Time } from "../time";
import { Button } from "../ui";
import { ActionError, Check, ConfirmButton, actionsRow, changed, formClass, useAdminAction } from "./kit";

/** The two-factor requirement: on or off, secure methods only, and whether to remove who is not compliant. */
export function TwoFactorPanel({ scope, requirement }: { scope: EnterpriseScope; requirement: TwoFactorRequirement }) {
  const initial = { required: requirement.required, secureMethodsOnly: requirement.secureMethodsOnly };
  const [f, setF] = useState(initial);
  const [remove, setRemove] = useState(false);
  const { run, busy, error } = useAdminAction();
  const patch = changed(initial, f);
  const request = { scope, ...patch, ...(remove ? { removeNoncompliant: true } : {}) };
  const dirty = Object.keys(patch).length > 0;
  const risky = (f.required && !requirement.required && requirement.noncompliantCount > 0) || remove;
  const save = async () => void (await run("updateTwoFactorRequirement", request, f.required ? "Two-factor is required" : "Two-factor is optional"));

  return (
    <form
      className={formClass}
      onSubmit={async (e) => {
        e.preventDefault();
        if (!risky) await save();
      }}
    >
      <p className="text-sm text-ink-2" data-testid="two-factor-state">
        {requirement.required ? "Required" : "Not required"}
        {requirement.requiredSince ? (
          <>
            {" "}
            since <Time iso={requirement.requiredSince} />
          </>
        ) : null}
        . {requirement.noncompliantCount} {requirement.noncompliantCount === 1 ? "member has" : "members have"} not set up two-factor.
      </p>
      <Check id="2fa-required" label="Require two-factor authentication" checked={f.required} onChange={(v) => setF((x) => ({ ...x, required: v }))} />
      <Check id="2fa-secure" label="Secure methods only" hint="Reject SMS codes; allow authenticator apps and security keys." checked={f.secureMethodsOnly} onChange={(v) => setF((x) => ({ ...x, secureMethodsOnly: v }))} />
      <Check id="2fa-remove" label="Remove members who are not compliant" hint="They lose access until they set up two-factor and are invited back." checked={remove} onChange={setRemove} />
      <ActionError error={error} />
      <div className={actionsRow}>
        {risky ? (
          <ConfirmButton
            size="md"
            variant="primary"
            label="Save"
            title={remove ? "Remove non-compliant members?" : "Require two-factor?"}
            description={remove ? `${requirement.noncompliantCount} members without two-factor lose access to this scope.` : `${requirement.noncompliantCount} members without two-factor are asked to set it up.`}
            confirmLabel={remove ? "Remove and save" : "Require and save"}
            disabled={busy || (!dirty && !remove)}
            onConfirm={save}
          />
        ) : (
          <Button type="submit" variant="primary" disabled={busy || !dirty}>
            Save
          </Button>
        )}
        <CopyAs op={{ service: "EnterpriseService", method: "UpdateTwoFactorRequirement", request, label: "Update the two-factor requirement" }} />
      </div>
    </form>
  );
}
