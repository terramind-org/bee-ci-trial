import type { AgentOwnerBudget } from "@/lib/api/types";

const MICROS_PER_DOLLAR = 1_000_000n;
const MAX_U64 = 18_446_744_073_709_551_615n;

export function dollars(micros: string): string {
  const n = BigInt(micros);
  const fractional = String(n % MICROS_PER_DOLLAR).padStart(6, "0").replace(/0{1,4}$/, "");
  return String(n / MICROS_PER_DOLLAR) + "." + fractional;
}

export function asMicros(value: string): string | null {
  if (!/^\d{1,20}(\.\d{1,6})?$/.test(value)) return null;
  const [whole = "", cents = ""] = value.split(".");
  const micros = BigInt(whole) * MICROS_PER_DOLLAR + BigInt(cents.padEnd(6, "0"));
  return micros <= MAX_U64 ? String(micros) : null;
}

export function asCount(value: string): string | null {
  if (!/^\d{1,20}$/.test(value)) return null;
  const count = BigInt(value);
  return count <= MAX_U64 ? String(count) : null;
}

export const BUDGET_LIMITS = [
  { field: "maxUsdMicros", label: "Spending limit (USD)", unit: "Spend", meter: "usd" },
  { field: "maxInputTokens", label: "Input token limit", unit: "Input tokens", meter: "input_tokens" },
  { field: "maxOutputTokens", label: "Output token limit", unit: "Output tokens", meter: "output_tokens" },
  { field: "maxRuns", label: "Run limit", unit: "Runs", meter: "runs" },
] as const;

export function ownerBudgetFacts(budget: AgentOwnerBudget) {
  const usage = [budget.spent?.usdMicros, budget.spent?.inputTokens, budget.spent?.outputTokens, budget.runsStarted];
  return BUDGET_LIMITS.map(({ field, unit, meter }, i) => {
    const format = (n: string) => field === "maxUsdMicros" ? "$" + dollars(n) : BigInt(n).toLocaleString("en-US");
    const max = budget[field] || "0";
    const used = usage[i];
    return {
      label: unit,
      value: [used !== undefined ? format(used) + " used" : "", max === "0" ? "unlimited" : format(max) + " limit"].filter(Boolean).join(" · "),
      exhausted: budget.exhausted === meter,
    };
  });
}
