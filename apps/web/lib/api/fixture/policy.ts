/**
 * A deliberately small reader for the `.bee/policy.toml` subset the demo uses, and the land
 * evaluation the fixture runs for EvaluateLanding and EvaluatePolicy. The server owns the real
 * policy engine (SPEC §9.4); this exists so the UI has honest data to show.
 */
import { DIFF_CLASSES } from "../types";
import type { PolicyError } from "../types";

export interface ApprovalRule {
  cls: string;
  need: number;
  from: "owner" | "human" | "any";
}

export interface LandRules {
  requiredChecks: string[];
  agentsMayLand: string[];
  approvals: ApprovalRule[];
  exclusiveRequired: string[];
  owners: { prefix: string; who: string[] }[];
  groups: Record<string, string[]>;
  sensitive: string[];
}

export interface ParseResult {
  rules: LandRules;
  errors: PolicyError[];
}

const KNOWN_SECTIONS = /^(groups|access|owners|sensitive|leases|land\."[^"]+")$/;

function strings(value: string): string[] {
  return [...value.matchAll(/"([^"]*)"/g)].map((m) => m[1] ?? "");
}

export function parsePolicy(source: string): ParseResult {
  const errors: PolicyError[] = [];
  const rules: LandRules = {
    requiredChecks: [],
    agentsMayLand: [],
    approvals: [],
    exclusiveRequired: [],
    owners: [],
    groups: {},
    sensitive: [],
  };
  const lines = source.split("\n");
  let section = "";
  let depth = 0;
  let pendingKey = "";
  let pendingValue = "";
  let pendingLine = 0;

  const finish = (key: string, value: string, line: number) => {
    const known = (cls: string) => (DIFF_CLASSES as readonly string[]).includes(cls);
    if (section.startsWith("land.")) {
      if (key === "required_checks") rules.requiredChecks = strings(value);
      else if (key === "agents_may_land") {
        rules.agentsMayLand = strings(value);
        for (const c of rules.agentsMayLand) if (!known(c)) errors.push({ line, message: 'unknown diff class "' + c + '"' });
      } else if (key === "approvals") {
        for (const m of value.matchAll(/\{([^}]*)\}/g)) {
          const body = m[1] ?? "";
          const cls = /class\s*=\s*"([^"]*)"/.exec(body)?.[1] ?? "";
          const need = Number(/need\s*=\s*(\d+)/.exec(body)?.[1] ?? "NaN");
          const from = /from\s*=\s*"([^"]*)"/.exec(body)?.[1] ?? "any";
          if (!known(cls)) errors.push({ line, message: 'unknown diff class "' + cls + '" in approvals' });
          if (!Number.isFinite(need) || need < 0) errors.push({ line, message: "approvals need a non-negative `need`" });
          if (from !== "owner" && from !== "human" && from !== "any")
            errors.push({ line, message: '`from` must be "owner", "human" or "any", got "' + from + '"' });
          rules.approvals.push({ cls, need: Number.isFinite(need) ? need : 0, from: from as ApprovalRule["from"] });
        }
      } else if (key !== "train") errors.push({ line, message: 'unknown key "' + key + '" in [' + section + "]" });
    } else if (section === "leases" && key === "exclusive_required") {
      rules.exclusiveRequired = strings(value);
    } else if (section === "owners") {
      rules.owners.push({ prefix: key.replace(/"/g, ""), who: strings(value) });
    } else if (section === "groups") {
      rules.groups[key] = strings(value);
    } else if (section === "sensitive" && key === "paths") {
      rules.sensitive = strings(value);
    }
  };

  lines.forEach((raw, idx) => {
    const lineNo = idx + 1;
    const line = raw.replace(/#.*$/, "").trim();
    if (depth > 0) {
      pendingValue += " " + line;
      depth += count(line, "[") - count(line, "]");
      if (depth <= 0) {
        depth = 0;
        finish(pendingKey, pendingValue, pendingLine);
      }
      return;
    }
    if (line === "") return;
    const sec = /^\[([^\]]+)\]$/.exec(line);
    if (sec) {
      section = (sec[1] ?? "").trim();
      if (!KNOWN_SECTIONS.test(section)) errors.push({ line: lineNo, message: 'unknown section "[' + section + ']"' });
      return;
    }
    const kv = /^("?[\w./-]+"?)\s*=\s*(.+)$/.exec(line);
    if (!kv) {
      errors.push({ line: lineNo, message: "expected `key = value`" });
      return;
    }
    const key = kv[1] ?? "";
    const value = kv[2] ?? "";
    if (count(value, '"') % 2 !== 0) {
      errors.push({ line: lineNo, message: "unterminated string" });
      return;
    }
    const d = count(value, "[") - count(value, "]");
    if (d > 0) {
      depth = d;
      pendingKey = key;
      pendingValue = value;
      pendingLine = lineNo;
      return;
    }
    if (key === "version" && value !== "1") errors.push({ line: lineNo, message: "only policy version 1 is supported" });
    finish(key, value, lineNo);
  });
  if (depth > 0) errors.push({ line: pendingLine, message: "array is never closed" });
  return { rules, errors };
}

function count(s: string, ch: string): number {
  let n = 0;
  for (const c of s) if (c === ch) n++;
  return n;
}

/** Human-readable summary lines, as RepoContext.land_rules shows them. */
export function describeRules(r: LandRules): string[] {
  const out: string[] = [];
  if (r.requiredChecks.length) out.push("Required checks: " + r.requiredChecks.join(", "));
  if (r.agentsMayLand.length) out.push("Agents may land without a human: " + r.agentsMayLand.join(", "));
  for (const a of r.approvals) out.push(a.cls + ": " + a.need + " approval" + (a.need === 1 ? "" : "s") + " from " + (a.from === "any" ? "anyone" : a.from + "s"));
  if (r.exclusiveRequired.length) out.push("Exclusive lease required under " + r.exclusiveRequired.join(", "));
  return out;
}

export interface LandSubject {
  diffClass: string;
  authorKind: "human" | "agent" | "service";
  authorHandle: string;
  paths: string[];
  checks: { name: string; status: string }[];
  approvals: { handle: string; kind: "human" | "agent" | "service" }[];
  exclusiveLeases: { holderHandle: string; paths: string[] }[];
}

export interface LandResult {
  allowed: boolean;
  satisfied: string[];
  missing: string[];
}

function under(path: string, prefix: string): boolean {
  const p = prefix.replace(/^\//, "").replace(/\/$/, "");
  return p === "" || path === p || path.startsWith(p + "/");
}

export function ownersFor(rules: LandRules, path: string): string[] {
  let best: { prefix: string; who: string[] } | undefined;
  for (const o of rules.owners) if (under(path, o.prefix) && (!best || o.prefix.length > best.prefix.length)) best = o;
  if (!best) return [];
  return best.who.flatMap((w) => (w.startsWith("group:") ? (rules.groups[w.slice(6)] ?? []) : [w]));
}

export function evaluateLand(rules: LandRules, s: LandSubject): LandResult {
  const satisfied: string[] = [];
  const missing: string[] = [];
  for (const name of rules.requiredChecks) {
    const c = s.checks.find((x) => x.name === name);
    if (c?.status === "CHECK_STATUS_PASSED") satisfied.push("check:" + name);
    else missing.push("check:" + name + (c ? " (" + c.status.replace("CHECK_STATUS_", "").toLowerCase() + ")" : " (not reported)"));
  }
  const agentOk = s.authorKind !== "human" && rules.agentsMayLand.includes(s.diffClass);
  if (agentOk) satisfied.push("agent-may-land:" + s.diffClass);
  const rule = rules.approvals.find((a) => a.cls === s.diffClass);
  if (rule && !agentOk) {
    if (rule.from === "owner") {
      const prefixes = [...new Set(s.paths.map((p) => topOwnedPrefix(rules, p)).filter((x): x is string => !!x))];
      for (const prefix of prefixes) {
        const owners = ownersFor(rules, prefix);
        const got = s.approvals.filter((a) => owners.includes(a.handle) && a.handle !== s.authorHandle).length;
        const label = "approval:owner:" + prefix.replace(/^\//, "");
        if (got >= rule.need) satisfied.push(label);
        else missing.push(label + " (" + got + " of " + rule.need + ")");
      }
      if (prefixes.length === 0) {
        const got = s.approvals.filter((a) => a.handle !== s.authorHandle).length;
        if (got >= rule.need) satisfied.push("approval:any");
        else missing.push("approval:any (" + got + " of " + rule.need + ")");
      }
    } else {
      const got = s.approvals.filter((a) => (rule.from === "any" || a.kind === "human") && a.handle !== s.authorHandle).length;
      const label = "approval:" + rule.from + ":" + s.diffClass;
      if (got >= rule.need) satisfied.push(label + " (" + got + " of " + rule.need + ")");
      else missing.push(label + " (" + got + " of " + rule.need + ")");
    }
  } else if (!rule && !agentOk && s.authorKind !== "human" && s.diffClass !== "") {
    // An agent change in a class that is neither agent-landable nor covered by a rule needs a human.
    const got = s.approvals.filter((a) => a.kind === "human").length;
    if (got >= 1) satisfied.push("approval:human:default");
    else missing.push("approval:human:default (0 of 1)");
  }
  for (const prefix of rules.exclusiveRequired) {
    if (!s.paths.some((p) => under(p, prefix))) continue;
    const held = s.exclusiveLeases.some((l) => l.holderHandle === s.authorHandle && l.paths.some((lp) => under(prefix.replace(/^\//, ""), lp) || under(lp, prefix)));
    const label = "lease:exclusive:" + prefix.replace(/^\//, "");
    if (held) satisfied.push(label);
    else missing.push(label);
  }
  return { allowed: missing.length === 0, satisfied, missing };
}

function topOwnedPrefix(rules: LandRules, path: string): string | undefined {
  let best: string | undefined;
  for (const o of rules.owners) if (under(path, o.prefix) && (!best || o.prefix.length > best.length)) best = o.prefix;
  return best;
}
