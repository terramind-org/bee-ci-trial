import { Line, SectSkeleton, Skel } from "@/components/skeletons";

/** A tab loading inside the pull request's layout: the title, the decisions and the tabs are already there. */
export default function Loading() {
  return (
    <Skel>
      <div className="flex max-w-[860px] flex-col gap-10">
        <div className="flex flex-col gap-2">
          <Line w="88%" lh={28} bar={14} />
          <Line w="52%" lh={22} bar={10} />
        </div>
        <SectSkeleton title="Why you" lines={2} />
        <SectSkeleton title="Checks" lines={1} />
        <SectSkeleton title="Files changed" lines={4} />
      </div>
    </Skel>
  );
}
