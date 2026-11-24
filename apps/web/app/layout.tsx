import type { Metadata, Viewport } from "next";
import Script from "next/script";
import type { ReactNode } from "react";
import { THEME_SCRIPT } from "@/components/theme";
import { ToastProvider } from "@/components/toast";
import { mono, sans } from "./fonts";
import "./globals.css";

export const metadata: Metadata = {
  title: { default: "Beehive", template: "%s · Beehive" },
  description: "Source control and a forge built for AI agents.",
  manifest: "/manifest.webmanifest",
  robots: { index: false, follow: false },
};

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
  themeColor: "#09090b",
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en" data-theme="dark" className={sans.variable + " " + mono.variable} suppressHydrationWarning>
      <body>
        <Script id="bee-theme" strategy="beforeInteractive">
          {THEME_SCRIPT}
        </Script>
        <ToastProvider>{children}</ToastProvider>
      </body>
    </html>
  );
}
