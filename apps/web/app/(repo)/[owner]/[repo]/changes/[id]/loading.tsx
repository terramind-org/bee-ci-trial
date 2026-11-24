import { Line, PanelSkeleton, Skel } from "@/components/skeletons";

/** A tab loading inside the change layout: the header, tabs and rail are already there. */
export default function Loading() {
  return (
    <Skel>
      <div className="flex max-w-[760px] flex-col gap-10">
        <div className="flex flex-col gap-3">
          <Line w="90%" lh={32} bar={18} />
          <Line w="60%" lh={32} bar={18} />
        </div>
        <span aria-hidden="true" className="block h-[76px] rounded-xl bg-sunken" />
        <PanelSkeleton title="Needs you" rows={2} rowHeight={44} lines={1} />
        <PanelSkeleton title="Does it work?" rows={3} rowHeight={120} lines={1} />
      </div>
    </Skel>
  );
}
