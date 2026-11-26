import type { BeeClient } from "@/lib/api/client";
import type { PostAttachment, RepoRef } from "@/lib/api/types";
import { optionalRead } from "./post-data";

async function changesFor(api: BeeClient, repo: RepoRef): Promise<PostAttachment[]> {
  const attachments: PostAttachment[] = [];
  let pageToken: string | undefined;
  do {
    const page = await optionalRead(api.change.listChanges({ repo, page: { pageSize: 100, pageToken } }));
    if (!page) break;
    attachments.push(...page.changes.map((change): PostAttachment => ({
      kind: "POST_ATTACHMENT_KIND_CHANGE_REQUEST", id: change.id, number: change.number, title: change.title,
    })));
    pageToken = page.page?.nextPageToken;
  } while (pageToken);
  return attachments;
}

export async function composeChoices(api: BeeClient, repo: RepoRef): Promise<PostAttachment[]> {
  const [pulls, issues, releases, runs] = await Promise.all([
    changesFor(api, repo),
    optionalRead(api.issue.listIssues({ repo })),
    optionalRead(api.release.listReleases({ repo })),
    optionalRead(api.agent.listAdoptRuns({ repo, limit: 100 })),
  ]);
  return [
    ...pulls,
    ...(releases?.releases ?? []).map((r): PostAttachment => ({
      kind: "POST_ATTACHMENT_KIND_RELEASE", id: r.id, title: r.name || r.tag,
    })),
    ...(issues?.issues ?? []).map((i): PostAttachment => ({
      kind: "POST_ATTACHMENT_KIND_ISSUE", id: i.id, number: String(i.number), title: i.title,
    })),
    ...(runs?.runs ?? []).map((r): PostAttachment => ({
      kind: "POST_ATTACHMENT_KIND_AGENT_RUN", id: r.id, title: [r.agent?.display, r.post?.changeTitle].filter(Boolean).join(" · ") || r.id,
    })),
  ];
}
