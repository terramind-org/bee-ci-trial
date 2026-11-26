const USD = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" });

/** Billing prices keep cents when the amount is ten dollars or more. */
export function billingPrice(microUsd: string): string {
  return USD.format(Number(microUsd) / 1_000_000);
}
