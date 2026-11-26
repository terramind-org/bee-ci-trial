/**
 * A deliberately small reader for the `.beehive/policy.toml` subset the demo uses, and the land
 * evaluation the fixture runs for EvaluateLanding and EvaluatePolicy. The server owns the real
 * policy engine (SPEC §9.4); this exists so the UI has honest data to show.
 */
import { DIFF_CLASSES } from "../types";
import type { PolicyError } from "../types";

export interface ApprovalRule {
  classes: string[];
  paths: string[];
  need: number;
  from: "owners" | "any" | string[];
  includeAgents: boolean;
}

export interface LandRules {
  ref: string;
  requiredChecks: string[];
  agentsMayLand: string[];
  approvals: ApprovalRule[];
  exclusiveRequired: string[];
  owners: { prefix: string; who: string[] }[];
  groups: Record<string, string[]>;
  roles: Record<string, { base: string | null; actions: string[] }>;
  members: Record<string, string[]>;
  sensitive: string[];
  access: Record<string, { read: string[] | null; write: string[] | null }>;
}

export interface ParseResult {
  rules: LandRules;
  errors: PolicyError[];
}

export const POLICY_PATH = ".beehive/policy.toml";

function strings(value: string): string[] {
  return [...value.matchAll(/"([^"]*)"/g)].map((m) => m[1] ?? "");
}

const TABLES: Record<string, readonly string[]> = {
  "": ["version"],
  groups: [],
  members: [],
  "roles.*": ["base", "actions"],
  "[[access]]": ["path", "read", "write"],
  "[[owners]]": ["path", "owners"],
  "[[land]]": ["ref", "required_checks", "agents_may_land_without_human"],
  "[[land.approvals]]": ["classes", "paths", "count", "from", "include_agents"],
  paths: ["sensitive", "exclusive_lease"],
};

export function parsePolicy(source: string): ParseResult {
  const errors: PolicyError[] = [];
  const rules: LandRules = {
    ref: "",
    requiredChecks: [],
    agentsMayLand: [],
    approvals: [],
    exclusiveRequired: [],
    owners: [],
    groups: {},
    roles: {},
    members: {},
    sensitive: [],
    access: {},
  };
  const knownClass = (cls: string) => (DIFF_CLASSES as readonly string[]).includes(cls);
  const invalid = (line: number, message: string) => errors.push({ line, message });
  const values = new Map<string, { value: string; line: number }>();
  let section = "";
  let sectionLine = 1;
  let depth = 0;
  let pendingKey = "";
  let pendingValue = "";
  let pendingLine = 0;

  function finishTable() {
    const val = (key: string) => values.get(key)?.value;
    const arr = (key: string) => strings(val(key) ?? "");
    const requireString = (key: string) => {
      const parsed = /^"([^"]+)"$/.exec(val(key) ?? "")?.[1];
      if (!parsed) invalid(values.get(key)?.line ?? sectionLine, `expected a string for ${key}`);
      return parsed;
    };
    const classes = (key: string) => {
      const choices = arr(key);
      for (const cls of choices) {
        if (!knownClass(cls)) invalid(values.get(key)?.line ?? sectionLine, 'unknown diff class "' + cls + '"');
      }
      return choices;
    };
    switch (section) {
      case "":
        if (val("version") !== "1" && source.trim()) invalid(values.get("version")?.line ?? sectionLine, "only policy version 1 is supported");
        break;
      case "groups":
        for (const [key, { value }] of values) rules.groups[key] = strings(value);
        break;
      case "members":
        rules.members = Object.fromEntries([...values].map(([key, { value }]) => [key, strings(value)]));
        break;
      case "[[access]]": {
        const path = requireString("path");
        if (path) rules.access[prefix(path)] = { read: val("read") ? arr("read") : null, write: val("write") ? arr("write") : null };
        break;
      }
      case "[[owners]]": {
        const path = requireString("path");
        if (path) rules.owners.push({ prefix: prefix(path), who: arr("owners") });
        break;
      }
      case "[[land]]":
        if (rules.ref) invalid(sectionLine, "the fixture evaluates one target ref");
        rules.ref = requireString("ref") ?? "";
        rules.requiredChecks = arr("required_checks");
        rules.agentsMayLand = classes("agents_may_land_without_human");
        break;
      case "[[land.approvals]]": {
        if (!rules.ref) invalid(sectionLine, "an approval rule needs a preceding [[land]] table");
        const need = Number(val("count") ?? "1");
        const from = val("from") ?? '"any"';
        if (!Number.isSafeInteger(need) || need < 1) invalid(values.get("count")?.line ?? sectionLine, "approvals need a positive count");
        if (!/^(?:"any"|"owners"|\[.*\])$/.test(from)) invalid(values.get("from")?.line ?? sectionLine, 'from must be "any", "owners" or a list of subjects');
        const includeAgents = val("include_agents");
        if (includeAgents !== undefined && includeAgents !== "true" && includeAgents !== "false") invalid(values.get("include_agents")?.line ?? sectionLine, "include_agents must be true or false");
        rules.approvals.push({
          classes: classes("classes"),
          paths: arr("paths").map((p) => prefix(p).slice(1)),
          need,
          from: from === '"owners"' ? "owners" : from === '"any"' ? "any" : strings(from),
          includeAgents: includeAgents === "true",
        });
        break;
      }
      case "paths":
        rules.sensitive = arr("sensitive");
        rules.exclusiveRequired = arr("exclusive_lease");
        break;
      default:
        if (section.startsWith("roles.")) {
          const name = section.slice("roles.".length);
          const base = val("base") ? requireString("base") ?? null : null;
          rules.roles = { ...rules.roles, [name]: { base, actions: arr("actions") } };
        }
        break;
    }
    values.clear();
  }

  function add(key: string, value: string, line: number) {
    const allowed = TABLES[section.startsWith("roles.") ? "roles.*" : section];
    if (section !== "groups" && section !== "members" && !allowed?.includes(key)) {
      invalid(line, 'unknown key "' + key + '" in ' + (section || "the policy header"));
      return;
    }
    if (values.has(key)) invalid(line, 'duplicate key "' + key + '"');
    values.set(key, { value, line });
  }

  source.split("\n").forEach((raw, idx) => {
    const lineNo = idx + 1;
    const line = raw.replace(/#(?=(?:[^"]*"[^"]*")*[^"]*$).*$/, "").trim();
    if (depth > 0) {
      pendingValue += " " + line;
      depth += count(line, "[") - count(line, "]");
      if (depth <= 0) add(pendingKey, pendingValue, pendingLine);
      return;
    }
    if (line === "") return;
    if (/^\[\[[\w.-]+\]\]$|^\[[\w.-]+\]$/.test(line)) {
      finishTable();
      section = line.startsWith("[[") ? line : line.slice(1, -1);
      sectionLine = lineNo;
      if (!Object.hasOwn(TABLES, section) && !section.startsWith("roles.")) invalid(lineNo, 'unknown section "' + line + '"');
      return;
    }
    const kv = /^([\w-]+)\s*=\s*(.+)$/.exec(line);
    const key = kv?.[1];
    const value = kv?.[2];
    if (!key || !value) {
      invalid(lineNo, "expected `key = value`");
      return;
    }
    if (count(value, '"') % 2 !== 0) {
      invalid(lineNo, "unterminated string");
      return;
    }
    depth = count(value, "[") - count(value, "]");
    if (depth > 0) {
      pendingKey = key;
      pendingValue = value;
      pendingLine = lineNo;
      return;
    }
    add(key, value, lineNo);
  });
  finishTable();
  if (depth > 0) invalid(pendingLine, "array is never closed");
  return { rules, errors };
}

function prefix(path: string): string {
  return "/" + path.replace(/^\/+|\/+$/g, "");
}

/** The wire shape from the server's policy_json serializer, rather than the demo evaluator internals. */
export function normalizedPolicy(rules: LandRules): string {
  const push = { deny_paths: [], deny_extensions: [], max_file_size: null, max_path_length: null };
  return JSON.stringify({
    codeowners: null,
    groups: rules.groups,
    roles: rules.roles,
    members: rules.members,
    access: rules.access,
    refs: {},
    land: rules.ref ? {
      [rules.ref]: {
        allow: null,
        required_checks: rules.requiredChecks,
        agents_may_land_without_human: rules.agentsMayLand,
        approvals: rules.approvals.map(({ classes, paths, need, from, includeAgents }) => ({
          classes, paths, count: need, from, include_agents: includeAgents,
        })),
        require_signed: false,
        require_linear: false,
        dismiss_stale_approvals: false,
        require_last_push_approval: false,
        push,
      },
    } : {},
    owners: Object.fromEntries(rules.owners.map(({ prefix, who }) => [prefix, who])),
    push,
    paths: { sensitive: rules.sensitive, generated: [], exclusive_lease: rules.exclusiveRequired },
    rulesets: {},
  });
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
  for (const a of r.approvals) out.push((a.classes.join(", ") || "Any change") + ": " + a.need + " approval" + (a.need === 1 ? "" : "s") + " from " + (a.from === "any" ? a.includeAgents ? "anyone" : "people" : a.from));
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

function groupMembers(rules: LandRules, subjects: string[], seen = new Set<string>()): string[] {
  return subjects.flatMap((subject) => {
    if (!subject.startsWith("@")) return [subject];
    if (seen.has(subject)) return [];
    return groupMembers(rules, rules.groups[subject.slice(1)] ?? [], new Set([...seen, subject]));
  });
}

/** The repo-wide admission action in the fixture policy's role and group tables. */
export function mayJoinSwarm(rules: LandRules, subjects: string[]): boolean {
  const membership = Object.entries(rules.members);
  if (!membership.length) return true;
  const builtInRoles = new Set(["read", "triage", "write", "maintain", "admin"]);
  function grants(role: string, seen = new Set<string>()): boolean {
    if (builtInRoles.has(role)) return true;
    if (!Object.hasOwn(rules.roles, role) || seen.has(role)) return false;
    const definition = rules.roles[role]!;
    return definition.actions.includes("swarm_join") || (!!definition.base && grants(definition.base, new Set([...seen, role])));
  }

  const names = new Set(subjects);
  function matches(subject: string, seen = new Set<string>()): boolean {
    if (names.has(subject)) return true;
    if (!subject.startsWith("@") || seen.has(subject)) return false;
    const group = subject.slice(1);
    if (!Object.hasOwn(rules.groups, group)) return false;
    return rules.groups[group]!.some((p) => matches(p, new Set([...seen, subject])));
  }
  return membership.some(([role, members]) => grants(role) && members.some((p) => matches(p)));
}

export function ownersFor(rules: LandRules, path: string): string[] {
  let best: { prefix: string; who: string[] } | undefined;
  for (const o of rules.owners) if (under(path, o.prefix) && (!best || o.prefix.length > best.prefix.length)) best = o;
  if (!best) return [];
  return groupMembers(rules, best.who);
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
  const approvals = rules.approvals.filter((a) => (!a.classes.length || a.classes.includes(s.diffClass)) &&
    (!a.paths.length || s.paths.some((path) => a.paths.some((p) => under(path, p)))));
  if (!agentOk) {
    for (const rule of approvals) {
      const eligible = s.approvals.filter((a) => a.handle !== s.authorHandle && (rule.includeAgents || a.kind === "human"));
      const report = (label: string, got: number) => {
        const result = label + " (" + got + " of " + rule.need + ")";
        (got >= rule.need ? satisfied : missing).push(result);
      };
      if (rule.from === "owners") {
        const paths = s.paths.filter((path) => !rule.paths.length || rule.paths.some((p) => under(path, p)));
        const prefixes = [...new Set(paths.map((p) => topOwnedPrefix(rules, p)).filter((x): x is string => !!x))];
        for (const prefix of prefixes) {
          const owners = ownersFor(rules, prefix.slice(1));
          report("approval:owner:" + prefix.slice(1), eligible.filter((a) => owners.includes(a.handle)).length);
        }
        if (!prefixes.length) report("approval:any", eligible.length);
      } else {
        const subjects = rule.from === "any" ? undefined : groupMembers(rules, rule.from);
        const got = subjects ? eligible.filter((a) => subjects.includes(a.handle)).length : eligible.length;
        report("approval:" + (rule.includeAgents ? "any" : "human") + ":" + s.diffClass, got);
      }
    }
    if (!approvals.length && s.authorKind !== "human" && s.diffClass !== "") {
      // An agent change not eligible to merge independently still needs a person.
      const got = s.approvals.filter((a) => a.kind === "human" && a.handle !== s.authorHandle).length;
      if (got >= 1) satisfied.push("approval:human:default");
      else missing.push("approval:human:default (0 of 1)");
    }
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
