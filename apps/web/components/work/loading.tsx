import { LoadingBee } from "@/components/bees/pieces";

export function WorkLoading({ children }: { children: string }) {
  return <LoadingBee text={children} />;
}
