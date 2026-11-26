// The organisation People page uses the real invitations and membership message shapes.
import type { OrgInvitationJson, OrgService } from "../../gen/bee/v1/forge_pb";
import { BeeError } from "../transport";
import type { Impl } from "./router";
import { ulid } from "./seed";
import type { FixtureStore, OrgRecord } from "./store";
import { invalid } from "./util";

const invitations = new WeakMap<FixtureStore, OrgInvitationJson[]>();

function canManage(org: OrgRecord, actor: string, ownerOnly: boolean) {
  const role = org.members.get(actor);
  if (role !== "ROLE_OWNER" && (ownerOnly || role !== "ROLE_ADMIN")) {
    throw new BeeError("permission_denied", { reason: "POLICY_DENIED", message: "You do not have permission to manage this membership." });
  }
}

export const setMember: NonNullable<Impl<typeof OrgService>["setMember"]> = (s, req, ctx) => {
  const org = s.orgRecord(req.org);
  const actor = s.caller(ctx.token);
  const target = s.handleOf(req.principalId ?? "");
  const role = req.role ?? "ROLE_UNSPECIFIED";
  const previous = org.members.get(target);
  canManage(org, actor, previous === "ROLE_OWNER" || role === "ROLE_OWNER");
  const principal = s.principals.get(target);
  if (!principal || (role === "ROLE_UNSPECIFIED" && !previous)) {
    throw new BeeError("not_found", { reason: "NOT_FOUND", message: "This member could not be found." });
  }
  if (previous === "ROLE_OWNER" && role !== "ROLE_OWNER" && [...org.members.values()].filter((r) => r === "ROLE_OWNER").length === 1) {
    throw new BeeError("failed_precondition", { reason: "INVALID_STATE", message: "An organisation needs at least one owner." });
  }
  if (role === "ROLE_UNSPECIFIED") org.members.delete(target);
  else org.members.set(target, role);
  if (org.org.name === s.org.name) {
    s.appendAudit(actor, role === "ROLE_UNSPECIFIED" ? "member.remove" : previous ? "member.role_change" : "member.add", principal.display, "", true, "");
  }
  return { principal, role };
};

export const createOrgInvitation: NonNullable<Impl<typeof OrgService>["createOrgInvitation"]> = (s, req, ctx) => {
  const org = s.orgRecord(req.org);
  const actor = s.caller(ctx.token);
  const role = !req.role || req.role === "ROLE_UNSPECIFIED" ? "ROLE_MEMBER" : req.role;
  canManage(org, actor, role === "ROLE_OWNER");
  const login = req.inviteeLogin?.trim() || "";
  const email = req.inviteeEmail?.trim() || "";
  const id = req.inviteePrincipalId || "";
  if (!login && !email && !id) throw invalid("An invitation needs a login, email or principal ID.");
  const target = id ? s.principals.get(s.handleOf(id)) : s.principals.get(login);
  if (id && !target) throw new BeeError("not_found", { reason: "NOT_FOUND", message: "This principal could not be found." });
  const key = target?.id || (login ? "login:" + login.toLowerCase() : "email:" + email.toLowerCase());
  const known = invitations.get(s) ?? [];
  const now = s.now();
  const alreadyInvited = known.some((invite) =>
    invite.orgId === org.org.id && Date.parse(invite.expiresAt ?? "") > now &&
    (invite.inviteePrincipalId || (invite.inviteeLogin ? "login:" + invite.inviteeLogin.toLowerCase() : "email:" + invite.inviteeEmail?.toLowerCase())) === key,
  );
  if ((target && org.members.has(s.handleOf(target.id))) || alreadyInvited) {
    throw new BeeError("already_exists", { reason: "ALREADY_EXISTS", message: "This person is already a member or has a pending invitation." });
  }
  const invite: OrgInvitationJson = {
    id: ulid(now, "org-invitation:" + org.org.id + ":" + key),
    orgId: org.org.id,
    inviteeLogin: login,
    inviteeEmail: email,
    inviteePrincipalId: target?.id || "",
    role,
    teamIds: req.teamIds ?? [],
    inviter: s.p(actor),
    state: "INVITATION_STATE_PENDING",
    createdAt: s.iso(now),
    expiresAt: s.iso(now + 7 * 86_400_000),
  };
  known.push(invite);
  invitations.set(s, known);
  if (org.org.name === s.org.name) s.appendAudit(actor, "org_invitation.create", login || email || id, "", true, "");
  return invite;
};
