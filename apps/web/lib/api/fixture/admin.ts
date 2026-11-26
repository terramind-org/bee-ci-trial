/** Fixture handlers for the cloud and admin surfaces: billing, enterprise and ops. */
import { billing } from "./admin-billing";
import { enterprise } from "./admin-enterprise";
import { ops } from "./admin-ops";
import type { ServiceDef } from "./router";

export const ADMIN_SERVICES: ServiceDef[] = [billing, ops, enterprise];
