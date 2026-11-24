"use client";

import { useState } from "react";
import type { EnterprisePolicies } from "@/lib/api/types";
import { CopyAs } from "../copy-as";
import { Button, Field, Select, Textarea } from "../ui";
import { ActionError, Check, actionsRow, changed, formClass, useAdminAction } from "./kit";

type Opts = readonly (readonly [value: string, label: string])[];

const REPO_CREATION: Opts = [
  ["ENTERPRISE_REPO_CREATION_POLICY_NO_POLICY", "Each organization decides"],
  ["ENTERPRISE_REPO_CREATION_POLICY_DISABLED", "Only organization owners"],
  ["ENTERPRISE_REPO_CREATION_POLICY_PRIVATE", "Members, private repositories"],
  ["ENTERPRISE_REPO_CREATION_POLICY_PRIVATE_INTERNAL", "Members, private and internal"],
  ["ENTERPRISE_REPO_CREATION_POLICY_ALL", "Members, private, internal and public"],
];
const FORKING: Opts = [
  ["ENTERPRISE_FORKING_POLICY_NO_POLICY", "Each organization decides"],
  ["ENTERPRISE_FORKING_POLICY_DISABLED", "No forking"],
  ["ENTERPRISE_FORKING_POLICY_WITHIN_ORG", "Within the organization"],
  ["ENTERPRISE_FORKING_POLICY_WITHIN_ENTERPRISE", "Within the enterprise"],
  ["ENTERPRISE_FORKING_POLICY_ANYWHERE", "Anywhere, including user accounts"],
];
const BASE: Opts = [
  ["ENTERPRISE_BASE_PERMISSION_NO_POLICY", "Each organization decides"],
  ["ENTERPRISE_BASE_PERMISSION_NONE", "None"],
  ["ENTERPRISE_BASE_PERMISSION_READ", "Read"],
  ["ENTERPRISE_BASE_PERMISSION_WRITE", "Write"],
  ["ENTERPRISE_BASE_PERMISSION_ADMIN", "Admin"],
];
const TOGGLE: Opts = [
  ["ENTERPRISE_POLICY_TOGGLE_NO_POLICY", "Each organization decides"],
  ["ENTERPRISE_POLICY_TOGGLE_ALLOWED", "Allowed"],
  ["ENTERPRISE_POLICY_TOGGLE_DISABLED", "Not allowed"],
];
const ENABLEMENT: Opts = [
  ["ENTERPRISE_ACTIONS_ENABLEMENT_NO_POLICY", "Each organization decides"],
  ["ENTERPRISE_ACTIONS_ENABLEMENT_ALL_ORGS", "All organizations"],
  ["ENTERPRISE_ACTIONS_ENABLEMENT_SELECTED_ORGS", "Selected organizations"],
  ["ENTERPRISE_ACTIONS_ENABLEMENT_DISABLED", "Disabled"],
];
const ALLOWED: Opts = [
  ["ENTERPRISE_ALLOWED_ACTIONS_NO_POLICY", "Each organization decides"],
  ["ENTERPRISE_ALLOWED_ACTIONS_ALL", "All actions and workflows"],
  ["ENTERPRISE_ALLOWED_ACTIONS_LOCAL_ONLY", "Only from this enterprise"],
  ["ENTERPRISE_ALLOWED_ACTIONS_SELECTED", "Only the patterns below"],
];

const TOGGLES = [
  ["membersCanDeleteRepos", "Members can delete repositories"],
  ["membersCanChangeRepoVisibility", "Members can change repository visibility"],
  ["membersCanInviteOutsideCollaborators", "Members can invite outside collaborators"],
  ["membersCanCreateTeams", "Members can create teams"],
  ["membersCanInstallApps", "Members can install apps"],
  ["agentsCanLandWithoutHumanApproval", "Agents can land without a human approval"],
] as const;

const lines = (v: string) => v.split("\n").map((s) => s.trim()).filter(Boolean);

function Pick({ id, label, value, options, onChange, hint }: { id: string; label: string; value: string; options: Opts; onChange: (v: string) => void; hint?: string }) {
  // A value the server holds that this list does not know is shown, not hidden.
  const known = options.some(([v]) => v === value);
  return (
    <Field label={label} htmlFor={id} hint={hint}>
      <Select id={id} value={value} onChange={(e) => onChange(e.target.value)}>
        {known ? null : <option value={value}>{value || "Unset"}</option>}
        {options.map(([v, l]) => (
          <option key={v} value={v}>
            {l}
          </option>
        ))}
      </Select>
    </Field>
  );
}

/** Enterprise policies: what members of every organization may do. */
export function PoliciesForm({ enterprise, policies }: { enterprise: string; policies: EnterprisePolicies }) {
  const a = policies.actions;
  const initial = {
    repoCreation: policies.repoCreation as string,
    forking: policies.forking as string,
    basePermission: policies.basePermission as string,
    membersCanDeleteRepos: policies.membersCanDeleteRepos as string,
    membersCanChangeRepoVisibility: policies.membersCanChangeRepoVisibility as string,
    membersCanInviteOutsideCollaborators: policies.membersCanInviteOutsideCollaborators as string,
    membersCanCreateTeams: policies.membersCanCreateTeams as string,
    membersCanInstallApps: policies.membersCanInstallApps as string,
    agentsCanLandWithoutHumanApproval: policies.agentsCanLandWithoutHumanApproval as string,
    enablement: (a?.enablement ?? "ENTERPRISE_ACTIONS_ENABLEMENT_NO_POLICY") as string,
    allowedActions: (a?.allowedActions ?? "ENTERPRISE_ALLOWED_ACTIONS_NO_POLICY") as string,
    allowVerifiedCreators: a?.allowVerifiedCreators ?? false,
    enabledOrgs: (a?.enabledOrgs ?? []).join("\n"),
    allowedActionPatterns: (a?.allowedActionPatterns ?? []).join("\n"),
  };
  type F = typeof initial;
  const [f, setF] = useState<F>(initial);
  const set = <K extends keyof F>(k: K) => (v: F[K]) => setF((x) => ({ ...x, [k]: v }));
  const { run, busy, error } = useAdminAction();
  const d = changed(initial, f);
  const { enablement, allowedActions, allowVerifiedCreators, enabledOrgs, allowedActionPatterns, ...top } = d;
  const actions = {
    ...(enablement !== undefined ? { enablement } : {}),
    ...(allowedActions !== undefined ? { allowedActions } : {}),
    ...(allowVerifiedCreators !== undefined ? { allowVerifiedCreators } : {}),
    ...(enabledOrgs !== undefined ? { enabledOrgs: lines(enabledOrgs) } : {}),
    ...(allowedActionPatterns !== undefined ? { allowedActionPatterns: lines(allowedActionPatterns) } : {}),
  };
  // The select values come from the lists above, which hold the proto's enum names.
  const request = { enterprise, ...top, ...(Object.keys(actions).length ? { actions } : {}) } as { enterprise: string } & Partial<EnterprisePolicies>;
  const dirty = Object.keys(d).length > 0;

  return (
    <form
      className={formClass}
      onSubmit={async (e) => {
        e.preventDefault();
        await run("updateEnterprisePolicies", request as never, "Policies saved");
      }}
    >
      <h3 className="text-sm font-medium">Repositories</h3>
      <div className="grid grid-cols-1 gap-3.5 md:grid-cols-3">
        <Pick id="pol-create" label="Who can create repositories" value={f.repoCreation} options={REPO_CREATION} onChange={set("repoCreation")} />
        <Pick id="pol-fork" label="Forking" value={f.forking} options={FORKING} onChange={set("forking")} />
        <Pick id="pol-base" label="Base permission" value={f.basePermission} options={BASE} onChange={set("basePermission")} hint="What every member gets on every repository." />
      </div>
      <h3 className="mt-1 text-sm font-medium">Members</h3>
      <div className="grid grid-cols-1 gap-3.5 md:grid-cols-2">
        {TOGGLES.map(([k, label]) => (
          <Pick key={k} id={"pol-" + k} label={label} value={f[k]} options={TOGGLE} onChange={set(k)} />
        ))}
      </div>
      <h3 className="mt-1 text-sm font-medium">Workflows</h3>
      <div className="grid grid-cols-1 gap-3.5 md:grid-cols-2">
        <Pick id="pol-enable" label="Where workflows run" value={f.enablement} options={ENABLEMENT} onChange={set("enablement")} />
        <Pick id="pol-allowed" label="Which actions may run" value={f.allowedActions} options={ALLOWED} onChange={set("allowedActions")} />
      </div>
      <div className="grid grid-cols-1 gap-3.5 md:grid-cols-2">
        <Field label="Selected organizations" htmlFor="pol-orgs" hint="One organization per line. Used when workflows run in selected organizations.">
          <Textarea id="pol-orgs" rows={3} className="mono text-xs" value={f.enabledOrgs} onChange={(e) => set("enabledOrgs")(e.target.value)} />
        </Field>
        <Field label="Allowed action patterns" htmlFor="pol-patterns" hint="One pattern per line. Used when only selected actions may run.">
          <Textarea id="pol-patterns" rows={3} className="mono text-xs" value={f.allowedActionPatterns} onChange={(e) => set("allowedActionPatterns")(e.target.value)} />
        </Field>
      </div>
      <Check id="pol-verified" label="Allow actions from verified creators" checked={f.allowVerifiedCreators} onChange={set("allowVerifiedCreators")} />
      <ActionError error={error} />
      <div className={actionsRow}>
        <Button type="submit" variant="primary" disabled={busy || !dirty}>
          Save
        </Button>
        <CopyAs op={{ service: "EnterpriseService", method: "UpdateEnterprisePolicies", request, label: "Update enterprise policies" }} />
      </div>
    </form>
  );
}
