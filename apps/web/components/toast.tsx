"use client";

import { createContext, useCallback, useContext, useState, type ReactNode } from "react";
import { BeeError } from "@/lib/api/transport";
import { DEGRADED_EVENT, isOutageError } from "@/lib/infra";
import { Icon } from "./icons";
import { cx } from "./ui";

interface Toast {
  id: number;
  tone: "ok" | "bad" | "neutral";
  title: string;
  detail?: string;
}

const Ctx = createContext<{ push: (t: Omit<Toast, "id">) => void; error: (e: unknown, title?: string) => void }>({ push: () => {}, error: () => {} });

let next = 1;

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const push = useCallback((t: Omit<Toast, "id">) => {
    const id = next++;
    setToasts((xs) => [...xs.slice(-3), { ...t, id }]);
    setTimeout(() => setToasts((xs) => xs.filter((x) => x.id !== id)), t.tone === "bad" ? 9000 : 4500);
  }, []);
  const error = useCallback(
    (e: unknown, title = "That did not work") => {
      const se = e instanceof BeeError ? e : null;
      if (isOutageError(e)) window.dispatchEvent(new Event(DEGRADED_EVENT));
      const missing = se?.detail.policyDenial?.missing;
      push({ tone: "bad", title: title + (se ? " · " + se.reason : ""), detail: missing?.length ? "Missing: " + missing.join(", ") : (e as Error)?.message });
    },
    [push],
  );
  return (
    <Ctx.Provider value={{ push, error }}>
      {children}
      <div className="pointer-events-none fixed right-3 bottom-3 z-[60] flex w-[min(380px,calc(100vw-24px))] flex-col gap-2" aria-live="polite" aria-relevant="additions">
        {toasts.map((t) => (
          <div key={t.id} role={t.tone === "bad" ? "alert" : "status"} className="pointer-events-auto flex gap-2.5 rounded-lg border border-line bg-popover p-3 shadow-overlay animate-enter">
            <span className={cx("mt-0.5", t.tone === "ok" ? "text-ok" : t.tone === "bad" ? "text-bad" : "text-ink-3")}>
              <Icon name={t.tone === "ok" ? "check" : t.tone === "bad" ? "alert" : "dot"} />
            </span>
            <div className="min-w-0">
              <p className="text-base font-medium">{t.title}</p>
              {t.detail ? <p className="mt-0.5 text-sm text-ink-2 [overflow-wrap:anywhere]">{t.detail}</p> : null}
            </div>
          </div>
        ))}
      </div>
    </Ctx.Provider>
  );
}

export function useToast() {
  return useContext(Ctx);
}
