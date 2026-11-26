/**
 * Mounts the Post panel for a change request: works out, on the server, who is looking (signed in,
 * the change's author, Write on the repo) and hands the panel that viewer. The change's own
 * `post` carries the server's per-post permissions, which win (`panelViewer`).
 *
 *   <PostPanelSlot repo={fullName} change={cr} />
 */
import { canWriteRepo, currentPrincipal } from "@/lib/api/server";
import type { ChangeRequest } from "@/lib/api/types";
import { panelViewer } from "@/lib/post-compose";
import { PostPanel } from "./post-panel";

export async function PostPanelSlot({ repo, change }: { repo: string; change: ChangeRequest }) {
  const [me, canWrite] = await Promise.all([currentPrincipal(), canWriteRepo(repo)]);
  const viewer = panelViewer(change, me, canWrite);
  return <PostPanel key={change.id + ":" + (change.post?.version ?? "0") + ":" + (change.post?.state ?? "")} repo={repo} change={change} viewer={viewer} me={me} />;
}
