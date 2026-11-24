import { PostListSkeleton } from "@/components/post-list";
import { Line, Skel } from "@/components/skeletons";
import { PageHeader } from "@/components/ui";

/** The permalink's crumb and title over one post card at its loaded size. */
export default function Loading() {
  return (
    <Skel>
      <div className="max-w-[640px]">
        <PageHeader eyebrow="Updates" title={<Line w={360} lh={34} bar={16} />} />
        <PostListSkeleton count={1} />
      </div>
    </Skel>
  );
}
