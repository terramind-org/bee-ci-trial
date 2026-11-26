import { Line, SectSkeleton, Skel } from "@/components/skeletons";

/** A repository's home while it loads: the README's lines, then Files and Commits under their labels. */
export default function Loading() {
  return (
    <Skel>
      <div className="flex max-w-[820px] min-w-0 flex-col gap-10">
        <div className="flex flex-col gap-1">
          <Line w="46%" lh={32} bar={18} />
          <Line w="92%" lh={24} bar={10} />
          <Line w="84%" lh={24} bar={10} />
          <Line w="60%" lh={24} bar={10} />
        </div>
        <SectSkeleton title="Files" lines={6} />
        <SectSkeleton title="Commits" lines={4} />
      </div>
    </Skel>
  );
}
