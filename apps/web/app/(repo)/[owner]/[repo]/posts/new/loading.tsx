import { Line, Skel } from "@/components/skeletons";
import { PageHeader } from "@/components/ui";

export default function Loading() {
  return <Skel><div className="mx-auto max-w-[640px]">
    <PageHeader title="Write a post" eyebrow="Updates" />
    <Line w="100%" lh={140} bar={128} />
    <Line w="100%" lh={180} bar={168} />
  </div></Skel>;
}
