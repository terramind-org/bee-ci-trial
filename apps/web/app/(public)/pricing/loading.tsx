import { HeaderSkeleton, Skel } from "@/components/skeletons";

/** The plan grid at its rendered size: a header, three plan cards and the self-hosted card. */
export default function Loading() {
  return (
    <Skel>
      <div className="mx-auto flex max-w-[1120px] flex-col gap-10">
        <HeaderSkeleton title="Pricing" meta="Pay per seat, plus usage beyond what a plan includes." />
        <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
          {Array.from({ length: 3 }, (_, i) => (
            <div key={i} className="h-[520px] rounded-lg border border-line bg-surface" />
          ))}
        </div>
        <div className="h-[104px] rounded-lg border border-line bg-surface" />
      </div>
    </Skel>
  );
}
