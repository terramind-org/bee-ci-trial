import { PostListSkeleton } from "@/components/posts/post-list";
import { Skel } from "@/components/skeletons";
import { PageHeader } from "@/components/ui";

export default function Loading() {
  return <Skel><div className="mx-auto w-full max-w-[640px]">
    <PageHeader title="Updates" />
    <PostListSkeleton />
  </div></Skel>;
}
