"use client";

/**
 * Tabs, on UI SDK's Tabs: plain words over one hairline, the selected one underlined in ink.
 * These switch panels in place. Tabs that are pages of their own are links, not tabs: use
 * `TabLinks` (./tab-links.tsx), which is a `nav`.
 */
import { Tabs as UiTabs, TabsList as UiTabsList, TabsPanel as UiTabsPanel, TabsTab as UiTabsTab } from "@ui-sdk/react/tabs";
import type { ComponentProps, ReactNode } from "react";
import { tw } from "./cx";
import { TAB } from "./tabs-style";


export function Tabs({ className, ...rest }: Omit<ComponentProps<typeof UiTabs<string>>, "className"> & { className?: string }) {
  return <UiTabs<string> className={tw("flex flex-col gap-3", className)} {...rest} />;
}

export function TabsList({ label, className, children }: { label: string; className?: string; children: ReactNode }) {
  return (
    <UiTabsList aria-label={label} className={tw("flex gap-6 border-b border-line", className)}>
      {children}
    </UiTabsList>
  );
}

export function TabsTab({ value, className, children }: { value: string; className?: string; children: ReactNode }) {
  return (
    <UiTabsTab value={value} className={tw(TAB + " -mb-px data-[selected]:border-ink data-[selected]:text-ink", className)}>
      {children}
    </UiTabsTab>
  );
}

export function TabsPanel({ value, className, children }: { value: string; className?: string; children: ReactNode }) {
  return (
    <UiTabsPanel value={value} className={className}>
      {children}
    </UiTabsPanel>
  );
}
