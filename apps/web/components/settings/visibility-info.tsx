import { State } from "@/components/parts";
import type { Visibility } from "@/lib/api/types";

export const VISIBILITIES = {
  VISIBILITY_PUBLIC: { label: "Public", description: "Anyone can read this repository." },
  VISIBILITY_PRIVATE: { label: "Private", description: "Only people and agents with access can read this repository." },
  VISIBILITY_INTERNAL: { label: "Internal", description: "Members of the organization and anyone else granted access can read this repository." },
} as const;

export function VisibilityStatus({ visibility }: { visibility: Visibility }) {
  if (visibility === "VISIBILITY_UNSPECIFIED") return null;
  return <State shape={visibility === "VISIBILITY_PUBLIC" ? "open" : "closed"}>{VISIBILITIES[visibility].label}</State>;
}
