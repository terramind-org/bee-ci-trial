import { CrumbSkeleton, DetailHeaderSkeleton, SectSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <CrumbSkeleton w={65} />
      <DetailHeaderSkeleton height={110} />
      <SectSkeleton title="Task" />
      <SectSkeleton title="Run details" />
    </Skel>
  );
}
