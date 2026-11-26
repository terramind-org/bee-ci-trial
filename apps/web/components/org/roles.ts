import type { Role } from "@/lib/api/types";

export const ROLES = {
  ROLE_MEMBER: "Member",
  ROLE_ADMIN: "Admin",
  ROLE_OWNER: "Owner",
  ROLE_BILLING_MANAGER: "Billing manager",
  ROLE_SECURITY_MANAGER: "Security manager",
  ROLE_MODERATOR: "Moderator",
  ROLE_APP_MANAGER: "App manager",
} satisfies Record<Exclude<Role, "ROLE_UNSPECIFIED">, string>;

export function roleName(role?: Role) {
  return role && role !== "ROLE_UNSPECIFIED" ? ROLES[role] : "Role unspecified";
}
