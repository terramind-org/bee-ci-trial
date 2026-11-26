import { Line, PanelSkeleton, Skel } from "@/components/skeletons";

/** The Code tab loading inside the change layout: the revision heading, the diff, then reviews. */
export default function Loading() {
  return (
    <Skel>
      <div className="flex min-w-0 flex-col gap-12">
        <section className="min-w-0">
          <Line w={120} lh={28} bar={16} className="mb-3" />
          <span aria-hidden="true" className="block h-[420px] rounded-xl bg-sunken" />
        </section>
        <PanelSkeleton title="Reviews" rows={2} rowHeight={56} />
        <PanelSkeleton title="Provenance" rows={2} rowHeight={40} lines={1} />
      </div>
    </Skel>
  );
}
