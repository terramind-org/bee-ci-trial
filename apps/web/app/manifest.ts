import type { MetadataRoute } from "next";

/** Installable PWA for inbox, review and merge on phones (PRODUCT §4.24). */
export default function manifest(): MetadataRoute.Manifest {
  return {
    name: "Beehive",
    short_name: "Beehive",
    description: "Source control and a forge built for AI agents.",
    start_url: "/",
    display: "standalone",
    background_color: "#09090b",
    theme_color: "#09090b",
    icons: [{ src: "/icon.svg", sizes: "any", type: "image/svg+xml" }],
  };
}
