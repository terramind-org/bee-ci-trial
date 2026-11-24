import { PostListSkeleton } from "@/components/post-list";
import { Line, Skel } from "@/components/skeletons";
import { PageHeader } from "@/components/ui";

/** The Updates title over three post cards at their loaded size; the lede names the repo, so it waits. */
export default function Loading() {
  return (
    <Skel>
      <div className="max-w-[640px]">
        <PageHeader title="Updates" lede={<Line w="80%" lh={26} bar={11} />} />
        <PostListSkeleton />
      </div>
    </Skel>
  );
}
