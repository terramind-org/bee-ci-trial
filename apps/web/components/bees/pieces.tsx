"use client";

import { useId, type ReactNode } from "react";
import styles from "./motion.module.css";
import { useOnce } from "./use-once";

type EventPiece = {
  /** Stable key for a confirmed event observed here. Omit to show the resting picture. */
  eventKey?: string;
  /** Plain sentence describing the event. The picture is hidden from assistive technology. */
  text: string;
  /** Show the resting pose, used by the review page. */
  still?: boolean;
};

function Bee({ x = 0, y = 0, marked = false }: { x?: number; y?: number; marked?: boolean }) {
  return (
    <g transform={`translate(${x} ${y})`}>
      <ellipse cx="5.8" cy="4.8" rx="2.15" ry="3.05" transform="rotate(-37 5.8 4.8)" fill="var(--ink)" fillOpacity=".32" />
      <ellipse cx="9" cy="4.6" rx="2" ry="2.9" transform="rotate(27 9 4.6)" fill="var(--ink)" fillOpacity=".46" />
      <path d="M2 10.4c1.1-2.5 3.4-3.7 6-3.7 2.6 0 4.2 1.4 4.2 3.3s-1.7 3.4-4.5 3.4c-2.5 0-4.5-1.2-5.7-3Z" fill="var(--ink)" />
      <path d="M6.2 7.2c-.8 1.7-.8 3.7 0 5.7m2.3-5.7c-.7 1.7-.7 3.7 0 5.7" fill="none" stroke="var(--surface)" strokeWidth=".8" />
      <circle cx="12.3" cy="9.7" r="1.95" fill="var(--ink)" />
      <path d="m13 8.5 1.3-1.6m-.6 2.2 1.5-.5" stroke="var(--ink)" strokeWidth=".65" strokeLinecap="round" />
      {marked && <circle cx="10.9" cy="10.4" r="1.5" fill="var(--mark)" />}
    </g>
  );
}

function Picture({ name, eventKey, still, width, height, className, children }: {
  name: string;
  eventKey?: string;
  still?: boolean;
  width: number;
  height: number;
  className?: string;
  children: ReactNode;
}) {
  const ref = useOnce(eventKey, still);
  return (
    <svg key={eventKey} ref={ref} data-motion={name} aria-hidden="true" focusable="false"
      className={`${styles.picture} ${className ?? ""}`} width={width} height={height} viewBox={`0 0 ${width} ${height}`}>
      {children}
    </svg>
  );
}

function EventText({ text }: { text: string }) {
  return <span className="sr-only" role="status" aria-live="polite" aria-atomic="true">{text}</span>;
}

/** One figure eight for the current loading mount. Pass a new key for a new wait in place. */
export function LoadingBee({ eventKey, still, decorative = false, text = "Loading" }: {
  eventKey?: string;
  still?: boolean;
  /** Use when surrounding content already names the status, such as a merge queue. */
  decorative?: boolean;
  text?: string;
}) {
  const instance = useId();
  return (
    <span className={decorative ? styles.inlineLoading : styles.loading} role={decorative ? undefined : "status"}>
      <Picture name="loading" eventKey={eventKey ?? `loading:${instance}`} still={still}
        width={16} height={16} className={styles.loadingPicture}><Bee /></Picture>
      {!decorative && <span>{text}</span>}
    </span>
  );
}

/** Place over the finished task's positioned cell to draw its fill and cap. */
export function Capping({ eventKey, text, still }: EventPiece) {
  return (
    <>
      <Picture name="capping" eventKey={eventKey} still={still} width={20} height={20} className={styles.cap}>
        <rect x="2" y="2" width="16" height="16" rx="4" fill="var(--ink-3)" fillOpacity=".45" />
        <path d="M4 6h12" stroke="var(--ink)" strokeLinecap="round" />
      </Picture>
      <EventText text={text} />
    </>
  );
}

/** Waggle beside the row whose task has started. */
export function Claiming({ eventKey, text, still }: EventPiece) {
  return (
    <>
      <Picture name="claiming" eventKey={eventKey} still={still} width={16} height={16}><Bee /></Picture>
      <EventText text={text} />
    </>
  );
}

/** Settle under the title once the merge has been confirmed. */
export function Landing({ eventKey, text, still }: EventPiece) {
  return (
    <>
      <Picture name="landing" eventKey={eventKey} still={still} width={48} height={24}>
        <path d="M1 21.5h46" stroke="var(--line-strong)" />
        <g className={styles.flight}><Bee x={29} y={7} /></g>
      </Picture>
      {/* A historical merge already has a state label. Announce only a merge observed here. */}
      <EventText text={eventKey ? text : ""} />
    </>
  );
}

/** Show alongside the immediate server refusal reason. */
export function Refusal({ eventKey, text, still }: EventPiece) {
  return (
    <>
      <Picture name="refusal" eventKey={eventKey} still={still} width={48} height={24}>
        <path d="M1 21.5h46" stroke="var(--line-strong)" />
        <path d="M34.5 10v8m4-10v10" stroke="var(--ink-2)" strokeLinecap="round" />
        <g className={styles.flight}><Bee x={5} y={7} /></g>
      </Picture>
      <EventText text={text} />
    </>
  );
}

/** Show beside Paused after the swarm enters that state. */
export function PauseSmoke({ eventKey, text, still }: EventPiece) {
  return (
    <>
      <Picture name="pause" eventKey={eventKey} still={still} width={24} height={20}>
        <g className={styles.flight} fill="none" stroke="var(--ink-2)" strokeWidth="1.1" strokeLinecap="round">
          <path d="M5 17c4-3-4-4 0-8" opacity=".65" />
          <path d="M12 15c5-4-4-6 0-12" opacity=".85" />
          <path d="M19 17c4-3-4-4 0-9" opacity=".65" />
        </g>
      </Picture>
      <EventText text={text} />
    </>
  );
}

/** A still illustration whenever the repository's working list is empty. */
export function QuietRepository({ text }: { text: string }) {
  return (
    <span role="status" className={styles.quiet}>
      <Picture name="quiet" width={42} height={24}>
        <Bee x={1} y={8} /><Bee x={13} y={1} /><Bee x={25} y={8} />
      </Picture>
      {text}
    </span>
  );
}

/** Fly in when the signed-in person's own request has been accepted. */
export function YourBeeArrives({ eventKey, text, still }: EventPiece) {
  return (
    <>
      <Picture name="arrival" eventKey={eventKey} still={still} width={16} height={16}><Bee marked /></Picture>
      <EventText text={text} />
    </>
  );
}
