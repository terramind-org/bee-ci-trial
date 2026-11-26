import type { Metadata } from "next";
import Link from "next/link";
import { redirect } from "next/navigation";
import { requestMode } from "@/app/request-mode";
import { PageFacts } from "@/components/bee/page-facts";
import { attachmentLabel } from "@/components/posts/attachments";
import { ComposePost } from "@/components/posts/compose";
import { composeChoices } from "@/components/posts/compose-data";
import { PageHeader } from "@/components/ui";
import { canWriteRepo, scopeFromParams, serverClient, sessionState, type RepoParams } from "@/lib/api/server";
import { guard } from "@/lib/page";
import { repoHref } from "@/lib/routes";
import { pageTools } from "@/lib/tools";

export const metadata: Metadata = { title: "Write a post" };

export default async function ComposePostPage({ params }: { params: Promise<RepoParams> }) {
  return guard(async () => {
    const { fullName, ref } = await scopeFromParams(params);
    const [{ me }, { mode }, api, writable] = await Promise.all([sessionState(), requestMode(), serverClient(), canWriteRepo(fullName)]);
    if (!me && mode === "keeper") redirect("/signin?next=" + encodeURIComponent(repoHref(fullName, "posts/new")));
    const canPublish = me?.kind === "PRINCIPAL_KIND_HUMAN" && writable;
    const choices = writable ? await composeChoices(api, ref) : [];
    if (mode === "bee") return <PageFacts page="compose" title="Write a post" groups={[
      { title: "scope", facts: [{ label: "repository", value: fullName }, { label: "access", value: writable ? "Write access" : "Write access required" }] },
      { title: "post fields", lines: ["Post text (500 characters)", "Body Markdown (20,000 characters)", "Optional article title and Markdown", "Up to four images or one video", "Attachments from this repository"] },
      { title: "available attachments", lines: choices.map(attachmentLabel), empty: "There are no items available to attach." },
    ]} tools={pageTools("compose")} />;

    return (
      <div className="mx-auto w-full max-w-[640px]">
        <PageHeader title="Write a post" eyebrow={<Link className="link" href={repoHref(fullName, "updates")}>Updates</Link>} lede={fullName} />
        {canPublish ? <ComposePost fullName={fullName} choices={choices} /> : <p className="text-ink-2">A person with write access can publish a post in this repository.</p>}
      </div>
    );
  });
}
