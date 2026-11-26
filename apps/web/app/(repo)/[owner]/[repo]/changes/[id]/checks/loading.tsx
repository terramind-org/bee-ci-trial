import { Line, Skel } from "@/components/skeletons";

/** The Checks tab loading inside the change layout: the heading, its sentence, then one entry per check. */
export default function Loading() {
  return (
    <Skel>
      <div className="flex min-w-0 flex-col gap-6">
        <div className="flex flex-col gap-2">
          <Line w={180} lh={28} bar={16} />
          <Line w="70%" lh={28} bar={12} />
        </div>
        {[0, 1, 2].map((i) => (
          <div key={i} className="flex flex-col gap-2 border-t border-line pt-6">
            <Line w={["40%", "32%", "46%"][i]!} lh={28} bar={14} />
            <Line w={["58%", "50%", "62%"][i]!} lh={20} bar={9} />
          </div>
        ))}
      </div>
    </Skel>
  );
}
