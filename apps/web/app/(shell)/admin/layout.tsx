import { notFound } from "next/navigation";
import type { ReactNode } from "react";
import { AdminTabs } from "@/components/admin/tabs";
import { PageHeader } from "@/components/ui";
import { serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";

/**
 * The site operator console. The server decides who is an operator (`ops.operators` /
 * `forge.site_admins`) and answers the operator RPCs only to them. No profile or WhoAmI field
 * exposes the role, so the layout asks GetClusterStatus, the lightest RPC that is operator-only
 * (GetOpsHealth answers any signed-in caller). A caller it refuses gets the same 404 as a page
 * that does not exist.
 */
export default async function AdminLayout({ children }: { children: ReactNode }) {
  try {
    await (await serverClient()).ops.getClusterStatus({});
  } catch (err) {
    if (!(err instanceof BeeError)) throw err;
    if (err.code === "permission_denied" || err.code === "unauthenticated") notFound();
    // Anything else (unreachable, not served) is shown by the tab that needs it.
  }
  return (
    <>
      <PageHeader title="Admin" lede="Run this Beehive server: its health, how people sign in, and its security and policies." />
      <AdminTabs />
      {children}
    </>
  );
}
