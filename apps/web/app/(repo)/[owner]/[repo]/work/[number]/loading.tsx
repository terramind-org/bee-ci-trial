import { CrumbSkeleton, DetailHeaderSkeleton, PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <CrumbSkeleton w={60} />
      <DetailHeaderSkeleton height={54} titleW={380} />
      <div className="grid grid-cols-1 gap-5 lg:grid-cols-[minmax(0,1fr)_300px]">
        <div className="flex min-w-0 flex-col gap-5">
          <PanelSkeleton rows={1} rowHeight={48} lines={1} />
          <PanelSkeleton title="Conversation" rows={3} rowHeight={69} />
        </div>
        <div className="flex flex-col gap-5">
          <PanelSkeleton title="Assignment" rows={3} rowHeight={52} />
          <PanelSkeleton title="Linked changes" rows={2} rowHeight={31} lines={1} />
        </div>
      </div>
    </Skel>
  );
}
