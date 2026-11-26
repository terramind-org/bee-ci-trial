/**
 * Buttons, on UI SDK's Button (docs/design/tokens.md, "Controls"). Pills: 46px, and 34px for
 * `sm`. One solid variant, ink (`primary`): a screen has at most one. `secondary` is the hairline
 * outline, `ghost` has no outline until hovered, and `danger` is the outline with the word in the
 * faint red: the word and where it sits say it is destructive, not the colour.
 *
 * A control that approves or destroys something does not use `Button`: it uses `ConfirmButton`
 * (./confirm.tsx), which marks it for agents. components/kit/confirm.test.ts holds that line.
 */
import { Button as UiButton, type ButtonActionDetails } from "@ui-sdk/react/button";
import Link from "next/link";
import type { ComponentProps, MouseEvent, ReactNode } from "react";
import { Icon, type IconName } from "../icons";
import { tw } from "./cx";

export type Variant = "primary" | "secondary" | "ghost" | "danger";
export type Size = "sm" | "md";

const BASE =
  "inline-flex shrink-0 items-center justify-center whitespace-nowrap rounded-full font-medium select-none transition-[background-color,color,box-shadow,opacity] duration-[160ms] ease-out disabled:pointer-events-none disabled:opacity-50 data-[disabled]:opacity-50 aria-disabled:cursor-not-allowed data-[pending]:opacity-70";

const VARIANTS: Record<Variant, string> = {
  primary: "bg-accent text-on-accent hover:bg-accent-hover",
  secondary: "bg-transparent text-ink shadow-[inset_0_0_0_1px_var(--line)] hover:bg-hover hover:shadow-[inset_0_0_0_1px_var(--line-strong)]",
  ghost: "bg-transparent text-ink-2 hover:bg-hover hover:text-ink",
  danger: "bg-transparent text-bad shadow-[inset_0_0_0_1px_var(--line)] hover:bg-bad-soft",
};

const SIZES: Record<Size, string> = {
  sm: "h-[34px] px-3.5 text-xs gap-1.5",
  md: "h-[46px] px-[22px] text-base gap-2",
};

export function buttonClass(variant: Variant = "secondary", size: Size = "md", extra?: string | false | null): string {
  return tw(BASE + " " + VARIANTS[variant] + " " + SIZES[size], extra);
}

type UiButtonProps = ComponentProps<typeof UiButton>;

export type ButtonProps = Omit<UiButtonProps, "className" | "children" | "onClick"> & {
  variant?: Variant;
  size?: Size;
  icon?: IconName;
  className?: string;
  children?: ReactNode;
  /** Return a promise to hold the button pending (and unpressable) until it settles. */
  onClick?: (event: MouseEvent<HTMLElement>, details: ButtonActionDetails) => unknown;
};

export function Button({ variant = "secondary", size = "md", icon, className, children, ...rest }: ButtonProps) {
  return (
    <UiButton className={buttonClass(variant, size, className)} {...rest}>
      {icon ? <Icon name={icon} size={size === "sm" ? 16 : 18} /> : null}
      {children}
    </UiButton>
  );
}

/** A link that looks like a button, for navigation that is the screen's next step. */
export function ButtonLink({ href, variant = "secondary", size = "md", icon, children, className }: { href: string; variant?: Variant; size?: Size; icon?: IconName; children: ReactNode; className?: string }) {
  return (
    <Link href={href} className={buttonClass(variant, size, className)}>
      {icon ? <Icon name={icon} size={size === "sm" ? 16 : 18} /> : null}
      {children}
    </Link>
  );
}
