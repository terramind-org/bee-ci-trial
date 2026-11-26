"use client";

import { useRouter } from "next/navigation";
import { useId, useState } from "react";
import { askPerson, ConfirmButton, Field, Input, Select } from "@/components/kit";
import { browserClient } from "@/lib/api/browser";
import type { Role } from "@/lib/api/types";
import { BeeError } from "@/lib/api/transport";
import { ROLES, roleName } from "./roles";

function RoleOptions({ canGrantOwner }: { canGrantOwner: boolean }) {
  return Object.entries(ROLES).filter(([role]) => canGrantOwner || role !== "ROLE_OWNER").map(([role, label]) => (
    <option key={role} value={role}>{label}</option>
  ));
}

function useMemberWrite() {
  const router = useRouter();
  const [error, setError] = useState("");
  async function write(action: () => Promise<unknown>) {
    setError("");
    try {
      await action();
      router.refresh();
    } catch (err) {
      setError(err instanceof BeeError ? err.reason + " · " + err.message : "The request could not be completed.");
    }
  }
  return { error, write };
}

export function InviteMember({ org, canGrantOwner }: { org: string; canGrantOwner: boolean }) {
  const id = useId();
  const [type, setType] = useState("login");
  const [invitee, setInvitee] = useState("");
  const [role, setRole] = useState<Role>("ROLE_MEMBER");
  const [sent, setSent] = useState("");
  const { write, error } = useMemberWrite();

  async function invite() {
    const to = invitee.trim();
    if (!to) return;
    if (!(await askPerson({ title: "Invite " + to + "?", description: "They can join as " + roleName(role).toLowerCase() + ".", confirmLabel: "Send invitation" }))) return;
    setSent("");
    await write(async () => {
      const result = await browserClient().org.createOrgInvitation({
        org,
        role,
        ...(type === "email" ? { inviteeEmail: to } : type === "id" ? { inviteePrincipalId: to } : { inviteeLogin: to }),
      });
      setSent("Invitation created for " + (result.inviteeEmail || result.inviteeLogin || to) + ".");
      setInvitee("");
    });
  }

  return (
    <section aria-labelledby="org-invite" className="mt-8 space-y-3 border-t border-line pt-5">
      <h2 id="org-invite" className="text-lg font-medium">Invite a person</h2>
      <div className="grid items-end gap-3 sm:grid-cols-[1fr_2fr_1fr_auto]">
        <Field label="Find by" htmlFor={id + "-type"}>
          <Select id={id + "-type"} value={type} onChange={(event) => setType(event.target.value)}>
            <option value="login">Login</option>
            <option value="email">Email</option>
            <option value="id">Principal ID</option>
          </Select>
        </Field>
        <Field label="Invitee" htmlFor={id + "-invitee"}>
          <Input id={id + "-invitee"} value={invitee} onChange={(event) => setInvitee(event.target.value)} type={type === "email" ? "email" : "text"} />
        </Field>
        <Field label="Invitation role" htmlFor={id + "-role"}>
          <Select id={id + "-role"} value={role} onChange={(event) => setRole(event.target.value as Role)}>
            <RoleOptions canGrantOwner={canGrantOwner} />
          </Select>
        </Field>
        <ConfirmButton agentConfirm="Invites this person to the organisation" consequence="approving" disabled={!invitee.trim()} onClick={invite}>Invite</ConfirmButton>
      </div>
      {sent ? <p role="status" className="text-sm">{sent}</p> : null}
      {error ? <p role="alert" className="text-sm text-bad [overflow-wrap:anywhere]">{error}</p> : null}
    </section>
  );
}

export function ManageMember({ org, principalId, name, role, canGrantOwner }: { org: string; principalId: string; name: string; role: Role; canGrantOwner: boolean }) {
  const [nextRole, setNextRole] = useState(role);
  const { write, error } = useMemberWrite();

  async function setMember(roleToSet: Role) {
    const removing = roleToSet === "ROLE_UNSPECIFIED";
    const title = removing ? "Remove " + name + "?" : "Change " + name + "’s role to " + roleName(roleToSet).toLowerCase() + "?";
    if (!(await askPerson({ title, confirmLabel: removing ? "Remove member" : "Change role", destructive: removing }))) return;
    await write(() => browserClient().org.setMember({ org, principalId, role: roleToSet }));
  }

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-2">
        <Select aria-label={"New role for " + name} className="h-[34px] w-auto text-xs" value={nextRole} onChange={(event) => setNextRole(event.target.value as Role)}>
          {role === "ROLE_UNSPECIFIED" ? <option value={role}>{roleName(role)}</option> : null}
          <RoleOptions canGrantOwner={canGrantOwner} />
        </Select>
        <ConfirmButton agentConfirm={"Changes the organisation role for " + name} consequence="approving" variant="secondary" size="sm" disabled={nextRole === role || nextRole === "ROLE_UNSPECIFIED"} onClick={() => setMember(nextRole)}>Change role</ConfirmButton>
        <ConfirmButton agentConfirm={"Removes " + name + " from the organisation"} consequence="destructive" size="sm" onClick={() => setMember("ROLE_UNSPECIFIED")}>Remove</ConfirmButton>
      </div>
      {error ? <p role="alert" className="text-sm text-bad [overflow-wrap:anywhere]">{error}</p> : null}
    </div>
  );
}
