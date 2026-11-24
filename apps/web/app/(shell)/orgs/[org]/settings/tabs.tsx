import { TabLinks } from "@/components/ui";
import { edition } from "@/lib/api/server";
import { orgSettingsHref } from "@/lib/routes";

export type SettingsSection = "general" | "audit" | "members" | "teams" | "apps" | "billing";

/**
 * The org settings sections. Repo policy lives with each repo, at `/{owner}/{repo}/settings`.
 * Billing is a section of the hosted (cloud) edition only.
 */
export async function SettingsTabs({ org, active }: { org: string; active: SettingsSection }) {
  const cloud = (await edition()) === "cloud";
  return (
    <TabLinks
      label="Settings sections"
      items={[
        { href: orgSettingsHref(org), label: "General", active: active === "general" },
        { href: orgSettingsHref(org, "members"), label: "Members", active: active === "members" },
        { href: orgSettingsHref(org, "teams"), label: "Teams", active: active === "teams" },
        { href: orgSettingsHref(org, "apps"), label: "Apps", active: active === "apps" },
        { href: orgSettingsHref(org, "audit"), label: "Audit log", active: active === "audit" },
        ...(cloud ? [{ href: orgSettingsHref(org, "billing"), label: "Billing", active: active === "billing" }] : []),
      ]}
    />
  );
}
