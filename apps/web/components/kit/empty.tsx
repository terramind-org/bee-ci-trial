/**
 * Nothing here yet: what this place is for in a sentence, and the one next step that fills it
 * (`action`: a ButtonLink, a CLI line, a sign-in link). Without an action the copy must say why
 * there is nothing to do. No picture and no invented numbers: an empty place says it is empty.
 */
import type { ReactNode } from "react";
import { Icon, type IconName } from "../icons";

export function EmptyState({ title, children, icon, action }: { title: string; children?: ReactNode; icon?: IconName; action?: ReactNode }) {
  return (
    <div className="flex flex-col items-start gap-3 border-t border-line py-10" role="status">
      {icon ? <Icon name={icon} size={22} className="text-ink-3" /> : null}
      <div className="flex max-w-xl flex-col gap-1.5">
        <p className="text-lg font-light tracking-tight text-ink">{title}</p>
        {children ? <div className="text-base text-ink-2">{children}</div> : null}
      </div>
      {action ? <div className="mt-2 flex flex-wrap items-center gap-2.5">{action}</div> : null}
    </div>
  );
}
