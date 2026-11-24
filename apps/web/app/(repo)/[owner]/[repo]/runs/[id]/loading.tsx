import { Line, PanelSkeleton, Skel } from "@/components/skeletons";
import { PageHeader } from "@/components/ui";

export default function Loading() {
  return (
    <Skel>
      <PageHeader eyebrow={<Line w={140} lh={24} bar={11} />} title={<Line w={300} lh={32} bar={18} />} meta={<Line w={420} lh={24} bar={10} />} />
      <section className="mb-10">
        <h2 className="mb-2 text-xl font-semibold tracking-tight text-ink">What happened</h2>
        <Line w={360} lh={28} bar={14} />
      </section>
      <div className="grid grid-cols-1 gap-5 lg:grid-cols-[260px_minmax(0,1fr)]">
        <PanelSkeleton title="Jobs" rows={3} rowHeight={39} lines={1} />
        <PanelSkeleton title="Steps" rows={8} rowHeight={36} lines={1} />
      </div>
    </Skel>
  );
}
