/** A server refusal as the Post panel shows it: the SPEC §12.2 reason an agent matches on, then the message. */
import { BeeError } from "@/lib/api/transport";

export interface Refusal {
  code: string;
  reason: string;
  message: string;
}

export function refusalOf(err: unknown): Refusal {
  if (err instanceof BeeError) return { code: err.code, reason: err.reason, message: err.message };
  return { code: "internal", reason: "INTERNAL", message: err instanceof Error ? err.message : String(err) };
}
