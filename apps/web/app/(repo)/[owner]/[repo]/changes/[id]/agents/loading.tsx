import { Line, PanelSkeleton, Skel } from "@/components/skeletons";

/** The For agents tab loading inside the change layout: the fetch line, the actions, then change.md. */
export default function Loading() {
  return (
    <Skel>
      <div className="flex min-w-0 flex-col gap-14">
        <section className="flex flex-col gap-4">
          <h2 className="text-xl font-semibold tracking-tight">Everything in one file</h2>
          <Line w="85%" lh={26} bar={11} />
          <span aria-hidden="true" className="block h-[46px] rounded-lg bg-sunken" />
        </section>
        <PanelSkeleton title="Act on it" rows={4} rowHeight={44} lines={1} />
        <section className="flex flex-col gap-4">
          <h2 className="text-xl font-semibold tracking-tight">change.md, rendered</h2>
          <span aria-hidden="true" className="block h-[320px] rounded-lg bg-sunken" />
        </section>
      </div>
    </Skel>
  );
}
