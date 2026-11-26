type Values = Readonly<Record<string, string>>;

interface Snapshot {
  scope: string;
  values: Values;
  keys: Values;
  revision: number;
}

export function nextSnapshot(
  previous: Snapshot | undefined,
  scope: string,
  values: Values,
  instance: string,
  includeNew = false,
): Snapshot {
  if (!previous || previous.scope !== scope) return { scope, values, keys: {}, revision: previous?.revision ?? 0 };

  const entries = Object.entries(values);
  if (entries.length === Object.keys(previous.values).length &&
      entries.every(([id, value]) => previous.values[id] === value)) return previous;

  const revision = previous.revision + 1;
  const keys = Object.fromEntries(entries.flatMap(([id, value]) => {
    const hadValue = Object.hasOwn(previous.values, id);
    if (hadValue && previous.values[id] === value) {
      return Object.hasOwn(previous.keys, id) ? [[id, previous.keys[id]!]] : [];
    }
    return hadValue || includeNew ? [[id, JSON.stringify([scope, instance, revision, id, value])]] : [];
  }));
  return { scope, values, keys, revision };
}

/** Keep claimed keys for the lifetime of this document, across client navigation. */
export function createEventClaims() {
  const seen = new Set<string>();
  return {
    has: (eventKey: string) => seen.has(eventKey),
    claim: (eventKey: string) => {
      if (seen.has(eventKey)) return false;
      seen.add(eventKey);
      return true;
    },
  };
}

export function canAnimate(mode: string | undefined, reducedMotion: boolean, still: boolean) {
  return !still && !reducedMotion && mode !== "bee";
}
