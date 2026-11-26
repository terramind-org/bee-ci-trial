import { CrumbSkeleton, DetailHeaderSkeleton, SectSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <CrumbSkeleton />
      <DetailHeaderSkeleton height={118} />
      <div className="flex max-w-[920px] flex-col gap-10">
        <SectSkeleton title="Tasks" lines={5} />
        <SectSkeleton title="Members" lines={3} />
      </div>
    </Skel>
  );
}
