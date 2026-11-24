import { PostListSkeleton } from "@/components/post-list";
import { Skel } from "@/components/skeletons";
import { PageHeader } from "@/components/ui";

/** The feed's title and lede as they load, over three post cards at their loaded size. */
export default function Loading() {
  return (
    <Skel>
      <div className="max-w-[640px]">
        <PageHeader title="Feed" lede="What the repos and people you follow have merged and said about it." />
        <PostListSkeleton />
      </div>
    </Skel>
  );
}
