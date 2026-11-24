import { HeaderSkeleton, Line, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <HeaderSkeleton eyebrow metaWidth={360} />
      <div className="grid gap-4 overflow-x-auto pb-2 md:grid-cols-4">
        {[3, 3, 5, 1].map((n, c) => (
          <section key={c} className="rounded-lg border border-line bg-sunken p-2.5">
            <div className="mb-2 flex items-center justify-between px-1">
              <Line w={70} lh={20} bar={10} />
            </div>
            <ul className="flex flex-col gap-2">
              {Array.from({ length: n }, (_, i) => (
                <li key={i} className="h-[88px] rounded-md border border-line bg-surface p-2.5">
                  <Line w={[80, 64, 72][i % 3] + "%"} lh={20} bar={10} />
                </li>
              ))}
            </ul>
          </section>
        ))}
      </div>
    </Skel>
  );
}
