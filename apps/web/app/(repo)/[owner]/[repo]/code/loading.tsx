import { Line, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <header className="mt-8 mb-6 flex h-9 items-center justify-between">
        <Line w={180} lh={20} bar={11} />
        <span className="h-9 w-56 rounded-md bg-sunken" />
      </header>
      <div className="mb-6 flex gap-6 border-b border-line pb-3">
        <Line w={48} lh={20} bar={11} />
        <Line w={64} lh={20} bar={11} />
        <Line w={64} lh={20} bar={11} />
      </div>
      <div className="grid gap-x-16 gap-y-12 xl:grid-cols-[minmax(0,1fr)_280px]">
        <div className="flex flex-col gap-3">
          {Array.from({ length: 9 }, (_, i) => (
            <Line key={i} w={i % 2 ? 360 : 280} lh={24} bar={12} />
          ))}
        </div>
        <div className="flex flex-col gap-3">
          <Line w={160} lh={28} bar={14} />
          <Line w={220} lh={20} bar={11} />
          <Line w={200} lh={20} bar={11} />
        </div>
      </div>
    </Skel>
  );
}
