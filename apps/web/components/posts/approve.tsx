"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { askPerson, ConfirmButton } from "@/components/kit";
import { browserClient } from "@/lib/api/browser";
import { refusalOf } from "../post/refusal";

export function ApprovePost({ postId, fullName }: { postId: string; fullName: string }) {
  const router = useRouter();
  const [error, setError] = useState("");
  return (
    <div>
      <ConfirmButton consequence="approving" agentConfirm="Approves and publishes this post" onClick={async () => {
        if (!await askPerson({ title: "Approve this post?", description: "The post will be published in the repository and its followers’ feeds.", confirmLabel: "Approve" })) return;
        setError("");
        try {
          await browserClient().post.approvePost({ postId, repo: { fullName } });
          router.refresh();
        } catch (err) {
          const reason = refusalOf(err);
          setError(reason.reason + " · " + reason.message);
        }
      }}>Approve</ConfirmButton>
      {error ? <p className="mt-2 text-sm text-bad" role="alert">{error}</p> : null}
    </div>
  );
}
