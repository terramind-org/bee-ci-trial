/** BillingService for the fixture: plans, the account's plan, seats and metered usage. */
import {
  BillingService,
  type BillingAccountPlanJson,
  type BillingPlanJson,
  type BillingProductJson,
  type BillingSeatJson,
  type BillingUsageLineItemJson,
} from "../../gen/bee/v1/billing_pb";
import type { PrincipalJson } from "../../gen/bee/v1/common_pb";
import { BeeError } from "../transport";
import { adminState, authorizeWrite, DAY, ORGS, scopeOf, SEAT_POOLS, signedIn, stamp, targetOf, type Account, type Scope } from "./admin-state";
import { defineService } from "./router";
import { fnv1a, prng, ulid } from "./seed";
import type { FixtureStore } from "./store";
import { page } from "./util";

export const FREE_SEAT_CAP = 5;
const MAX_SEATS = 10_000;

const q = (product: BillingProductJson, quantity: number, unit: string) => ({ product, quantity, unit });

/** The plan catalogue. Prices are micro-USD per seat; the yearly price is ten months of the monthly one. */
export const PLANS: BillingPlanJson[] = [
  {
    id: "free",
    name: "Free",
    description: "For individuals and small teams getting started.",
    seatMonthlyPriceMicroUsd: "0",
    seatYearlyPriceMicroUsd: "0",
    included: [
      q("BILLING_PRODUCT_ACTIONS_MINUTES", 2_000, "minute"),
      q("BILLING_PRODUCT_STORAGE_GB", 1, "GB-month"),
      q("BILLING_PRODUCT_AGENT_TOKENS", 500_000, "token"),
      q("BILLING_PRODUCT_WORKSPACES_HOURS", 60, "hour"),
    ],
    features: ["Up to " + FREE_SEAT_CAP + " seats", "Unlimited public and private repositories", "Change requests with required checks", "Community support"],
    selfServe: true,
  },
  {
    id: "team",
    name: "Team",
    description: "For teams that ship with agents and need policy, audit and support.",
    seatMonthlyPriceMicroUsd: "4000000",
    seatYearlyPriceMicroUsd: "40000000",
    included: [
      q("BILLING_PRODUCT_ACTIONS_MINUTES", 3_000, "minute"),
      q("BILLING_PRODUCT_STORAGE_GB", 50, "GB-month"),
      q("BILLING_PRODUCT_AGENT_TOKENS", 5_000_000, "token"),
      q("BILLING_PRODUCT_WORKSPACES_HOURS", 120, "hour"),
    ],
    features: [
      "Pay per seat, no minimum",
      "Landing policy, leases and merge queues",
      "Audit log with 1 year of retention",
      "Email support with a one business day response",
      "Free for verified students, teachers and nonprofits: ask us",
    ],
    selfServe: true,
  },
  {
    id: "enterprise",
    name: "Enterprise",
    description: "For organisations that need SSO, provisioning and a contract.",
    seatMonthlyPriceMicroUsd: "0",
    seatYearlyPriceMicroUsd: "0",
    included: [
      q("BILLING_PRODUCT_ACTIONS_MINUTES", 50_000, "minute"),
      q("BILLING_PRODUCT_STORAGE_GB", 1_000, "GB-month"),
      q("BILLING_PRODUCT_AGENT_TOKENS", 100_000_000, "token"),
      q("BILLING_PRODUCT_WORKSPACES_HOURS", 2_000, "hour"),
    ],
    features: ["SAML single sign-on and SCIM provisioning", "IP allow lists and enforced two-factor authentication", "Enterprise policies across every org", "Dedicated support and a service level agreement", "Contact us for pricing"],
    selfServe: false,
  },
];

const RANK: Record<string, number> = { free: 0, team: 1, enterprise: 2 };

const planOf = (id: string): BillingPlanJson => {
  const p = PLANS.find((x) => x.id === id);
  if (!p) throw new BeeError("not_found", { reason: "PLAN_NOT_FOUND", message: "no plan " + id });
  return p;
};

function accountOf(s: FixtureStore, sc: Scope): Account {
  const a = adminState(s).accounts.get(sc.key);
  if (!a) throw new BeeError("not_found", { reason: "ORG_NOT_FOUND", message: "no billing account for " + sc.key });
  return a;
}

function accountJson(s: FixtureStore, sc: Scope): BillingAccountPlanJson {
  const a = accountOf(s, sc);
  return {
    scope: sc.json,
    plan: planOf(a.planId),
    cycle: a.cycle,
    seatsPurchased: a.purchased,
    seatsConsumed: a.consumed,
    cycleStartedAt: s.iso(a.cycleStart),
    nextBillingAt: s.iso(a.nextBilling),
    pendingPlanId: a.pendingPlanId,
    pendingSeats: a.pendingSeats,
    billingDisabled: false,
  };
}

const cycleLength = (cycle: string) => (cycle === "BILLING_CYCLE_YEARLY" ? 365 : 30) * DAY;

// ---------------------------------------------------------------- seats

const OUTSIDE: Record<string, string[]> = {
  acme: ["Ines Alvarez", "Tomasz Kowal", "Priya Nair", "Jonas Berg", "Wen Tan"],
  lattice: ["Odile Marchand", "Kofi Mensah"],
};
const LATTICE_MEMBERS = ["Rhea Kapoor", "Mateo Ruiz", "Hana Sato"];

function externalPrincipal(s: FixtureStore, name: string): PrincipalJson {
  return { id: ulid(s.anchor - 250 * DAY, "principal:ext:" + name), kind: "PRINCIPAL_KIND_HUMAN", display: name };
}

/** Every seat the org consumes: humans first, then outside collaborators. */
function orgSeats(s: FixtureStore, org: string): BillingSeatJson[] {
  const humans: { principal: PrincipalJson; outside: boolean }[] =
    org === "acme"
      ? [...s.principals.values()].filter((p) => p.kind === "PRINCIPAL_KIND_HUMAN").slice(0, SEAT_POOLS.acme.members).map((principal) => ({ principal, outside: false }))
      : LATTICE_MEMBERS.map((n) => ({ principal: externalPrincipal(s, n), outside: false }));
  const outside = (OUTSIDE[org] ?? []).map((n) => ({ principal: externalPrincipal(s, n), outside: true }));
  return [...humans, ...outside].map(({ principal, outside: out }, i) => {
    const rnd = prng(fnv1a("seat:" + org + ":" + principal.id));
    const since = s.anchor - Math.floor(30 + rnd() * 250) * DAY;
    const active = Math.min(s.anchor - Math.floor(rnd() * 30 * 3_600_000) - i * 60_000, s.anchor);
    return {
      principal,
      org,
      outsideCollaborator: out,
      costCenterId: "",
      consumedSince: s.iso(since),
      lastActivityAt: s.iso(Math.max(since, active)),
    };
  });
}

function seatsOf(s: FixtureStore, sc: Scope): BillingSeatJson[] {
  return sc.kind === "org" ? orgSeats(s, sc.org) : ORGS.flatMap((o) => orgSeats(s, o.name));
}

// ---------------------------------------------------------------- usage

interface ProductDef {
  product: BillingProductJson;
  sku: string;
  unit: string;
  /** Micro-USD per unit. */
  price: number;
  /** Typical daily quantity per scope key. */
  base: Record<string, number>;
}

const USAGE_PRODUCTS: ProductDef[] = [
  { product: "BILLING_PRODUCT_ACTIONS_MINUTES", sku: "actions_linux_2core", unit: "minute", price: 8_000, base: { "org:acme": 140, "org:lattice": 55 } },
  { product: "BILLING_PRODUCT_STORAGE_GB", sku: "storage_artifacts", unit: "GB-day", price: 2_300, base: { "org:acme": 38, "org:lattice": 11 } },
  { product: "BILLING_PRODUCT_AGENT_TOKENS", sku: "agent_tokens_input", unit: "token", price: 3, base: { "org:acme": 260_000, "org:lattice": 40_000 } },
  { product: "BILLING_PRODUCT_WORKSPACES_HOURS", sku: "workspaces_4core", unit: "hour", price: 90_000, base: { "org:acme": 8, "org:lattice": 2 } },
];

const HISTORY_DAYS = 90;

interface DayLine {
  day: number;
  def: ProductDef;
  quantity: number;
  gross: number;
  discount: number;
}

const dayKey = (ms: number) => new Date(ms).toISOString().slice(0, 10);
const daysInMonth = (ms: number) => {
  const d = new Date(ms);
  return new Date(Date.UTC(d.getUTCFullYear(), d.getUTCMonth() + 1, 0)).getUTCDate();
};

/** Daily priced lines for the 90 whole days before the anchor, with the plan's monthly allowance applied in order. */
function usageLines(s: FixtureStore, sc: Scope): DayLine[] {
  const acct = accountOf(s, sc);
  const plan = planOf(acct.planId);
  const today = Math.floor(s.anchor / DAY) * DAY;
  const remaining = new Map<string, number>();
  const out: DayLine[] = [];
  for (let k = HISTORY_DAYS; k >= 1; k--) {
    const day = today - k * DAY;
    const weekday = new Date(day).getUTCDay();
    const damp = weekday === 0 || weekday === 6 ? 0.3 : 1;
    for (const def of USAGE_PRODUCTS) {
      const base = sc.kind === "org" ? (def.base[sc.key] ?? 0) : (def.base["org:acme"] ?? 0) + (def.base["org:lattice"] ?? 0);
      const rnd = prng(fnv1a("usage:" + sc.key + ":" + def.product + ":" + dayKey(day)))();
      const storage = def.product === "BILLING_PRODUCT_STORAGE_GB";
      const quantity = storage ? Math.round(base * (0.9 + 0.2 * ((HISTORY_DAYS - k) / HISTORY_DAYS)) * (0.97 + 0.06 * rnd)) : Math.round(base * (0.55 + 0.9 * rnd) * damp);
      const gross = quantity * def.price;
      const month = dayKey(day).slice(0, 7);
      const slot = month + ":" + def.product;
      const allowance = (plan.included?.find((i) => i.product === def.product)?.quantity as number | undefined) ?? 0;
      const left = remaining.get(slot) ?? allowance * (storage ? daysInMonth(day) : 1);
      const used = Math.min(quantity, left);
      remaining.set(slot, left - used);
      out.push({ day, def, quantity, gross, discount: used * def.price });
    }
  }
  return out;
}

const GROUP_DAY = "BILLING_USAGE_GROUPING_DAY";
const GROUP_MONTH = "BILLING_USAGE_GROUPING_MONTH";
const GROUP_PRODUCT = "BILLING_USAGE_GROUPING_PRODUCT";

function timeOf(v: string | undefined, fallback: number): number {
  if (!v) return fallback;
  const t = Date.parse(v);
  if (Number.isNaN(t)) throw new BeeError("invalid_argument", { reason: "INVALID_TIMESTAMP", message: "not a timestamp: " + v });
  return t;
}

function lineItem(date: string, def: ProductDef, quantity: number, gross: number, discount: number): BillingUsageLineItemJson {
  return {
    date,
    product: def.product,
    sku: def.sku,
    quantity,
    unit: def.unit,
    unitPriceMicroUsd: String(def.price),
    grossMicroUsd: String(gross),
    discountMicroUsd: String(discount),
    netMicroUsd: String(gross - discount),
  };
}

export const billing = defineService(BillingService, {
  listBillingPlans: (s, req, ctx) => {
    signedIn(s, ctx);
    if (req.scope) scopeOf(s, req.scope);
    return { plans: PLANS };
  },

  getAccountBillingPlan: (s, req, ctx) => {
    signedIn(s, ctx);
    return accountJson(s, scopeOf(s, req.scope));
  },

  changeBillingPlan: (s, req, ctx) => {
    const sc = scopeOf(s, req.scope);
    const acct = accountOf(s, sc);
    const actor = authorizeWrite(s, ctx, sc, "billing.plan.change");
    const target = planOf(req.planId ?? "");
    const targetId = target.id ?? "";
    if (!target.selfServe) throw new BeeError("failed_precondition", { reason: "PLAN_NOT_SELF_SERVE", message: "the " + target.name + " plan is by contract: contact us to switch" });
    const asked = req.seats ?? 0;
    if (asked > MAX_SEATS) throw new BeeError("invalid_argument", { reason: "INVALID_SEATS", message: "seats must be at most " + MAX_SEATS });
    if (asked > 0 && asked < acct.consumed)
      throw new BeeError("failed_precondition", { reason: "SEATS_BELOW_CONSUMED", message: `${acct.consumed} seats are in use: remove members before buying fewer than that` });
    let seats = asked || Math.max(acct.purchased, acct.consumed, 1);
    if (targetId === "free") {
      if (acct.consumed > FREE_SEAT_CAP)
        throw new BeeError("failed_precondition", { reason: "FREE_SEAT_CAP_EXCEEDED", message: `the Free plan allows ${FREE_SEAT_CAP} seats and ${acct.consumed} are in use: remove members first` });
      if (asked > FREE_SEAT_CAP) throw new BeeError("invalid_argument", { reason: "SEATS_ABOVE_PLAN_LIMIT", message: `the Free plan allows at most ${FREE_SEAT_CAP} seats` });
      seats = FREE_SEAT_CAP;
    }
    const cycle = req.cycle && req.cycle !== "BILLING_CYCLE_UNSPECIFIED" ? req.cycle : acct.cycle;
    const now = s.now();

    if (cycle !== acct.cycle) {
      acct.cycle = cycle;
      acct.nextBilling = Math.max(acct.cycleStart + cycleLength(cycle), now + DAY);
    }
    const from = RANK[acct.planId] ?? 0;
    const to = RANK[targetId] ?? 0;
    if (to > from) {
      acct.planId = targetId;
      acct.purchased = seats;
      acct.pendingPlanId = "";
      acct.pendingSeats = 0;
    } else if (to < from) {
      acct.pendingPlanId = targetId;
      acct.pendingSeats = seats;
    } else if (seats > acct.purchased) {
      acct.purchased = seats;
      acct.pendingPlanId = "";
      acct.pendingSeats = 0;
    } else if (seats < acct.purchased) {
      acct.pendingPlanId = targetId;
      acct.pendingSeats = seats;
    } else {
      acct.pendingPlanId = "";
      acct.pendingSeats = 0;
    }
    s.appendAudit(actor, "billing.plan.change", `${targetOf(sc)} -> ${targetId} x${seats} ${cycle === "BILLING_CYCLE_YEARLY" ? "yearly" : "monthly"}`, "", true, "");
    return accountJson(s, sc);
  },

  listBillingSeats: (s, req, ctx) => {
    signedIn(s, ctx);
    const sc = scopeOf(s, req.scope);
    const acct = accountOf(s, sc);
    const all = req.costCenterId ? [] : seatsOf(s, sc);
    const p = page(all, req.page);
    return { seats: p.items, totalConsumed: acct.consumed, page: { nextPageToken: p.next } };
  },

  queryBillingUsage: (s, req, ctx) => {
    signedIn(s, ctx);
    const sc = scopeOf(s, req.scope);
    if (req.org && sc.kind === "enterprise") scopeOf(s, { org: req.org });
    const today = Math.floor(s.anchor / DAY) * DAY;
    const start = timeOf(req.startAt, today - 30 * DAY);
    const end = timeOf(req.endAt, today);
    const wanted = new Set<string>((req.products ?? []).filter((p) => p !== "BILLING_PRODUCT_UNSPECIFIED"));
    const lines = usageLines(s, sc).filter((l) => l.day >= start && l.day < end && (!wanted.size || wanted.has(l.def.product)));

    const groups = new Set(req.groupBy ?? []);
    const byMonth = groups.has(GROUP_MONTH) && !groups.has(GROUP_DAY);
    const byProductOnly = groups.has(GROUP_PRODUCT) && !groups.has(GROUP_DAY) && !groups.has(GROUP_MONTH);
    const merged = new Map<string, { date: string; def: ProductDef; quantity: number; gross: number; discount: number }>();
    for (const l of lines) {
      const date = byProductOnly ? "" : byMonth ? dayKey(l.day).slice(0, 7) : dayKey(l.day);
      const key = date + "|" + l.def.product;
      const hit = merged.get(key);
      if (hit) {
        hit.quantity += l.quantity;
        hit.gross += l.gross;
        hit.discount += l.discount;
      } else merged.set(key, { date, def: l.def, quantity: l.quantity, gross: l.gross, discount: l.discount });
    }
    const order = USAGE_PRODUCTS.map((d) => d.product);
    const items = [...merged.values()].sort((a, b) => (a.date < b.date ? -1 : a.date > b.date ? 1 : order.indexOf(a.def.product) - order.indexOf(b.def.product)));
    const gross = items.reduce((n, i) => n + BigInt(i.gross), 0n);
    const discount = items.reduce((n, i) => n + BigInt(i.discount), 0n);
    const p = page(items, req.page, 100);
    return {
      lineItems: p.items.map((i) => lineItem(i.date, i.def, i.quantity, i.gross, i.discount)),
      totalGrossMicroUsd: gross.toString(),
      totalNetMicroUsd: (gross - discount).toString(),
      dataCompleteUntil: stamp(s, s.anchor),
      page: { nextPageToken: p.next },
    };
  },
});
