import { Line, PanelSkeleton, Skel } from "@/components/skeletons";

/** Files and README beside About, Clone, Working now and Recent changes, at their loaded heights. */
export default function Loading() {
  return (
    <Skel>
      <div className="grid grid-cols-1 gap-5 xl:grid-cols-[minmax(0,1fr)_340px]">
        <div className="flex min-w-0 flex-col gap-5">
          <section className="rounded-lg border border-line bg-surface">
            <header className="flex min-h-10 items-center border-b border-line px-3.5 py-2">
              <h2 className="text-sm font-medium">Files</h2>
            </header>
            <div className="border-b border-line px-3.5 py-2.5">
              <Line w="70%" lh={20} bar={10} />
            </div>
            <ul className="grid sm:grid-cols-2">
              {Array.from({ length: 8 }, (_, i) => (
                <li key={i} className="flex h-8 items-center px-3.5">
                  <Line w={`${40 + ((i * 11) % 40)}%`} lh={16} bar={9} />
                </li>
              ))}
            </ul>
          </section>
          <PanelSkeleton rows={6} rowHeight={40} lines={1} title="README.md" />
        </div>
        <div className="flex flex-col gap-5">
          <PanelSkeleton title="About" rows={4} rowHeight={52} lines={1} />
          <PanelSkeleton title="Clone" rows={2} rowHeight={52} lines={1} />
          <PanelSkeleton title="Working now" rows={2} rowHeight={52} />
          <PanelSkeleton title="Recent changes" rows={5} rowHeight={52} />
        </div>
      </div>
    </Skel>
  );
}
