"use client";

import { useRouter } from "next/navigation";
import { useTransition } from "react";
import { Button } from "@/components/kit";

export function RefreshWork() {
  const router = useRouter();
  const [pending, start] = useTransition();
  return <Button size="sm" onClick={() => start(() => router.refresh())} disabled={pending}>{pending ? "Refreshing…" : "Refresh"}</Button>;
}
