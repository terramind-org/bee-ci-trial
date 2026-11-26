import { CrumbSkeleton, DetailHeaderSkeleton, SectSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <CrumbSkeleton w={65} />
      <DetailHeaderSkeleton height={110} titleW={220} />
      <div className="flex flex-col gap-10">
        <SectSkeleton title="Recent work" />
        <SectSkeleton title="Working now" />
        <SectSkeleton title="Follows" />
      </div>
    </Skel>
  );
}
