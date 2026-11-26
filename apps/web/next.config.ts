import type { NextConfig } from "next";

// The UI makes no third-party requests: it uses the system fonts and there are no trackers.
const config: NextConfig = {
  reactStrictMode: true,
  poweredByHeader: false,
  // Self-contained server for the Docker image (apps/web/Dockerfile).
  output: "standalone",
  // Do not write AGENTS.md into the app; the repo's own AGENTS.md is the contributor guide.
  agentRules: false,
  async headers() {
    return [
      {
        source: "/:path*",
        headers: [
          { key: "X-Content-Type-Options", value: "nosniff" },
          { key: "Referrer-Policy", value: "same-origin" },
        ],
      },
    ];
  },
};

export default config;
