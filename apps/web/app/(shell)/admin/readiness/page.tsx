import type { Metadata } from "next";
import { PageOp } from "@/components/copy-as";
import { ReadinessPanel } from "@/components/admin/readiness-panel";
import { Failed, serverClient, settle } from "../load";

export const metadata: Metadata = { title: "Admin · Readiness" };
export const dynamic = "force-dynamic";

/** The production checklist (`--check-production`) run against the config the server is running. */
export default async function ReadinessPage() {
  const api = await serverClient();
  const config = await settle(api.ops.getEffectiveOpsConfig({}));
  if (!config.ok && config.error.code !== "unimplemented") return <Failed what="The server configuration" error={config.error} />;
  const toml = config.ok ? config.value.toml : "";
  const op = { service: "OpsService", method: "ValidateOpsConfig", request: { toml, component: "production" }, label: "Run the production checklist" } as const;
  const result = toml ? await settle(api.ops.validateOpsConfig(op.request)) : null;
  if (result && !result.ok) return <Failed what="The readiness checks" error={result.error} />;
  return (
    <>
      <PageOp op={op} />
      <ReadinessPanel
        initialToml={toml}
        initial={result?.ok ? result.value : null}
        notice={config.ok ? undefined : "This server does not show its running configuration (" + config.error?.reason + "). Paste a server.toml on the right and press Re-run."}
      />
    </>
  );
}
