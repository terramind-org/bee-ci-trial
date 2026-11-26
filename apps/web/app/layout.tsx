import type { Metadata, Viewport } from "next";
import Script from "next/script";
import type { ReactNode } from "react";
import { AgentBridge } from "@/components/agent-bridge";
import { ConfirmHost } from "@/components/kit/confirm";
import { THEME_SCRIPT } from "@/components/theme";
import { ToastProvider } from "@/components/toast";
import "./globals.css";
import { requestMode } from "./request-mode";

export const metadata: Metadata = {
  title: { default: "Beehive", template: "%s · Beehive" },
  description: "Source control and a forge built for AI agents.",
  manifest: "/manifest.webmanifest",
  robots: { index: false, follow: false },
};

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
  // Paper and near-black, the two page grounds (app/globals.css).
  themeColor: [
    { media: "(prefers-color-scheme: light)", color: "#f4f2ec" },
    { media: "(prefers-color-scheme: dark)", color: "#0f0f0e" },
  ],
};

/**
 * The document. `data-mode` is decided here, on the server, from who is signed in (lib/mode.ts):
 * an agent gets the agent's view, a person the person's unless they asked to preview the other.
 * `data-theme` is left to the script below, which runs before paint; without it the colours
 * still follow the system (`light-dark()` in globals.css).
 */
export default async function RootLayout({ children }: { children: ReactNode }) {
  const { mode, kind } = await requestMode();
  return (
    <html lang="en" data-mode={mode} suppressHydrationWarning>
      <body>
        <Script id="bee-theme" strategy="beforeInteractive">
          {THEME_SCRIPT}
        </Script>
        <ToastProvider>{children}</ToastProvider>
        <ConfirmHost />
        <AgentBridge kind={kind} />
      </body>
    </html>
  );
}
