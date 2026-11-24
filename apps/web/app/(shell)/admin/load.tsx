import type { ReactNode } from "react";
import { ErrorState, Unavailable } from "@/components/error-state";
import { TabLinks } from "@/components/ui";
import { edition as serverEdition, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { Enterprise, EnterpriseScope } from "@/lib/api/types";
import type { SearchParams } from "@/lib/page";
import { param } from "@/lib/page";

export type Loaded<T> = { ok: true; value: T } | { ok: false; error: BeeError };

/** Awaits a call and keeps a server error as a value, so one failing panel does not take the page down. */
export async function settle<T>(p: Promise<T>): Promise<Loaded<T>> {
  try {
    return { ok: true, value: await p };
  } catch (err) {
    if (err instanceof BeeError) return { ok: false, error: err };
    throw err;
  }
}

/** What a panel shows when its call failed: "not available on this server" for UNIMPLEMENTED, else the reason. */
export function Failed({ what, error }: { what: string; error: BeeError }): ReactNode {
  if (error.code === "unimplemented") return <Unavailable what={what} message={error.message} reason={error.reason} />;
  return <ErrorState reason={error.reason} message={error.message} retryable={error.retryable} />;
}

/** The enterprise's slug on this server. A self-hosted instance has one; `default` is the alias for it. */
export const enterpriseSlug = () => process.env.BEE_ENTERPRISE || "default";

export interface AdminScope {
  /** `enterprise` or `org:<name>`, as it appears in `?scope=`. */
  key: string;
  label: string;
  scope: EnterpriseScope;
}

export interface ScopeChoice {
  enterprise: Enterprise | null;
  enterpriseError: BeeError | null;
  options: AdminScope[];
  selected: AdminScope | null;
}

/**
 * Where identity and security settings apply: the enterprise when the server has one, else each
 * organization on it. `?scope=` picks one; the default is the enterprise, then the first org.
 */
export async function loadScope(sp: Awaited<SearchParams>): Promise<ScopeChoice> {
  const api = await serverClient();
  const [ent, orgs] = await Promise.all([settle(api.enterprise.getEnterprise({ enterprise: enterpriseSlug() })), settle(api.org.listOrgs({}))]);
  const enterprise = ent.ok ? ent.value : null;
  const options: AdminScope[] = [];
  if (enterprise) options.push({ key: "enterprise", label: enterprise.name || enterprise.slug, scope: { enterprise: enterprise.slug } });
  if (orgs.ok) for (const o of orgs.value.orgs) options.push({ key: "org:" + o.name, label: o.display || o.name, scope: { org: o.name } });
  const wanted = param(sp, "scope");
  return { enterprise, enterpriseError: ent.ok ? null : ent.error, options, selected: options.find((o) => o.key === wanted) ?? options[0] ?? null };
}

/** Which scope a page is showing, with links to the others. */
export function ScopeBar({ path, choice }: { path: string; choice: ScopeChoice }) {
  if (!choice.selected) return null;
  return (
    <div className="mb-4">
      {choice.options.length > 1 ? (
        <TabLinks label="Applies to" items={choice.options.map((o) => ({ href: path + "?scope=" + encodeURIComponent(o.key), label: o.label, active: o.key === choice.selected?.key }))} />
      ) : null}
      {!choice.enterprise ? (
        <p className="text-sm text-ink-3">
          This server has no enterprise account, so these settings apply to {choice.options.length > 1 ? "the organization you pick" : "the organization"} {choice.options.length > 1 ? "" : choice.selected.label}.
        </p>
      ) : null}
    </div>
  );
}

export { serverClient, serverEdition };
