import { PanelSkeleton, Skel } from "@/components/skeletons";

export default function Loading() {
  return (
    <Skel>
      <div className="grid grid-cols-1 gap-5 xl:grid-cols-2">
        <PanelSkeleton title="SAML single sign-on" rows={7} rowHeight={72} lines={1} />
        <div className="flex flex-col gap-5">
          <PanelSkeleton title="SCIM provisioning" rows={3} rowHeight={72} lines={1} />
          <PanelSkeleton title="Two-factor authentication" rows={2} rowHeight={72} lines={1} />
        </div>
      </div>
      <div className="mt-5">
        <PanelSkeleton title="Verified domains" rows={3} rowHeight={64} />
      </div>
    </Skel>
  );
}
