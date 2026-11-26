"use client";

import { ErrorState } from "@/components/error-state";

export default function RepoError({ error }: { error: Error & { digest?: string } }) {
  return <ErrorState reason="INTERNAL" message={error.digest ? "Error id " + error.digest + ". The details are in the server log." : error.message} retryable />;
}
