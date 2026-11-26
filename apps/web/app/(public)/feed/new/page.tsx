import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { requestMode } from "@/app/request-mode";
import { PageFacts } from "@/components/bee/page-facts";
import { Button, Field, Input } from "@/components/kit";
import { PageHeader } from "@/components/ui";
import { sessionState } from "@/lib/api/server";
import { param, type SearchParams } from "@/lib/page";
import { isRepoFullName, repoHref } from "@/lib/routes";
import { pageTools } from "@/lib/tools";

export const metadata: Metadata = { title: "Write a post" };

export default async function ChoosePostRepository({ searchParams }: { searchParams: SearchParams }) {
  const [{ me }, { mode }, query] = await Promise.all([sessionState(), requestMode(), searchParams]);
  const fullName = param(query, "repo").trim();
  const human = me?.kind === "PRINCIPAL_KIND_HUMAN";
  if (!me && mode === "keeper") redirect("/signin?next=" + encodeURIComponent("/feed/new"));
  if (human && isRepoFullName(fullName)) redirect(repoHref(fullName, "posts/new"));
  if (mode === "bee") return <PageFacts page="compose" title="Write a post" groups={[
    { title: "scope", lines: ["Choose a repository you can write to.", "Use its owner/name in the repository field of the request."] },
  ]} tools={pageTools("compose").filter((tool) => tool.op === "PostService/CreatePost")} />;
  return (
    <div className="mx-auto w-full max-w-[640px]">
      <PageHeader title="Write a post" />
      {human ? (
        <form action="/feed/new" className="space-y-4">
          <Field label="Repository" htmlFor="post-repository" hint="Enter owner/name of a repository you can write to."
            error={fullName ? "INVALID_ARGUMENT · Enter a repository in owner/name form." : undefined}>
            <Input id="post-repository" name="repo" defaultValue={fullName} required />
          </Field>
          <Button type="submit" variant="primary">Continue</Button>
        </form>
      ) : <p className="text-ink-2">A person with write access can publish a post.</p>}
    </div>
  );
}
