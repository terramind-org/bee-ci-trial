/**
 * Form controls, on UI SDK's Field, Input, Textarea and Switch. A `Field` ties its control to
 * the label, the hint and the error, so a screen reader hears all three and an agent finds the
 * control by its label.
 *
 * `Select` is the browser's own `<select>`: the app's selects are short lists written as
 * `<option>` children, which the native control already does well on every device. It is styled
 * here and wired to its `Field` by `htmlFor`.
 */
import { Field as UiField, FieldDescription, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input as UiInput } from "@ui-sdk/react/input";
import { Switch as UiSwitch, SwitchThumb } from "@ui-sdk/react/switch";
import { Textarea as UiTextarea } from "@ui-sdk/react/textarea";
import type { ComponentProps, ReactNode, SelectHTMLAttributes } from "react";
import { tw } from "./cx";

export function Field({ label, hint, error, children, htmlFor }: { label: string; hint?: ReactNode; error?: string; children: ReactNode; htmlFor: string }) {
  return (
    <UiField invalid={error ? true : undefined} className="flex flex-col gap-1.5">
      {/* `for` is set on the rendered element: the part's own would point at an id it generates,
          and the app's controls carry their own ids (a native select has no other tie to its label). */}
      <FieldLabel render={<label htmlFor={htmlFor} />} className="text-xs font-medium text-ink-2">
        {label}
      </FieldLabel>
      {children}
      {error ? (
        <FieldError match role="alert" className="text-sm text-bad">
          {error}
        </FieldError>
      ) : hint ? (
        <FieldDescription className="text-sm text-ink-3">{hint}</FieldDescription>
      ) : null}
    </UiField>
  );
}

/** One look for every text control: the field surface inside a hairline, ink on focus. */
const CONTROL =
  "w-full bg-input text-base text-ink shadow-[inset_0_0_0_1px_var(--line)] outline-none transition-shadow duration-[160ms] placeholder:text-ink-3 hover:shadow-[inset_0_0_0_1px_var(--line-strong)] focus-visible:shadow-[inset_0_0_0_2px_var(--focus)] disabled:cursor-not-allowed disabled:opacity-50 aria-[invalid=true]:shadow-[inset_0_0_0_2px_var(--bad)]";

type UiInputProps = ComponentProps<typeof UiInput>;
type UiTextareaProps = ComponentProps<typeof UiTextarea>;

/** `value` may be a number, as on a native input; the control holds it as text. */
export type InputProps = Omit<UiInputProps, "className" | "value" | "defaultValue"> & { className?: string; value?: string | number; defaultValue?: string | number };
export type TextareaProps = Omit<UiTextareaProps, "className"> & { className?: string };

const text = (v: string | number | undefined) => (v === undefined ? undefined : String(v));

export function Input({ className, value, defaultValue, ...rest }: InputProps) {
  return <UiInput className={tw(CONTROL + " h-[46px] rounded-full px-5", className)} value={text(value)} defaultValue={text(defaultValue)} {...rest} />;
}

export function Textarea({ className, rows = 3, ...rest }: TextareaProps) {
  return <UiTextarea rows={rows} className={tw(CONTROL + " rounded-2xl px-5 py-3 leading-6", className)} {...rest} />;
}

export function Select({ className, children, ...rest }: SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <select className={tw(CONTROL + " h-[46px] rounded-full pr-9 pl-5", className)} {...rest}>
      {children}
    </select>
  );
}

type UiSwitchProps = ComponentProps<typeof UiSwitch>;

/** On or off, applied at once. Name it with a `Field` label or `aria-label`. */
export function Switch({ className, ...rest }: Omit<UiSwitchProps, "className" | "children"> & { className?: string }) {
  return (
    <UiSwitch
      className={tw(
        "inline-flex h-6 w-11 shrink-0 cursor-pointer items-center rounded-full bg-transparent p-0.5 shadow-[inset_0_0_0_1px_var(--line-strong)] transition-colors duration-[160ms] data-[checked]:bg-ink data-[checked]:shadow-none data-[disabled]:cursor-not-allowed data-[disabled]:opacity-50",
        className,
      )}
      {...rest}
    >
      <SwitchThumb className="block size-5 rounded-full bg-ink-2 transition-transform duration-[160ms] data-[checked]:translate-x-5 data-[checked]:bg-bg" />
    </UiSwitch>
  );
}
