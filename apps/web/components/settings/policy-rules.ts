import { countWord, listing } from "@/lib/checkwords";

type Rule = { text: string; raw?: boolean };
type Fields = Record<string, unknown>;

const object = (v: unknown): Fields | undefined => v !== null && typeof v === "object" && !Array.isArray(v) ? v as Fields : undefined;
const strings = (v: unknown): v is string[] => Array.isArray(v) && v.every((part) => typeof part === "string");
const sentence = (text: string): Rule => ({ text });
const raw = (key: string, value: unknown): Rule => ({ text: key + " = " + JSON.stringify(value, null, 2), raw: true });

const CLASS_WORDS = new Map([
  ["generated", "generated files"],
  ["docs", "docs"],
  ["comment_only", "comments"],
  ["test_only", "tests"],
  ["rename_only", "file moves"],
  ["dependency", "dependencies"],
  ["logic", "code behavior"],
  ["sensitive", "sensitive paths"],
]);

function classes(v: unknown) {
  if (!strings(v)) return undefined;
  const names = v.map((key) => CLASS_WORDS.get(key));
  return names.every((name) => name !== undefined) ? names : undefined;
}

function alternatives(items: string[]) {
  return items.length > 1 ? items.slice(0, -1).join(", ") + " or " + items.at(-1) : (items[0] ?? "");
}

function approval(value: unknown, branch: string): Rule {
  const rule = object(value);
  const key = "land." + branch + ".approvals";
  if (!rule || Object.keys(rule).some((field) => !["classes", "paths", "count", "from", "include_agents"].includes(field))) return raw(key, value);

  const names = classes(rule.classes);
  const { paths, count, from, include_agents: includeAgents } = rule;
  if (!names || !strings(paths) || typeof count !== "number" || !Number.isSafeInteger(count) || count < 1 || typeof includeAgents !== "boolean") return raw(key, rule);

  const where = paths.length > 0
    ? "A change to " + alternatives(paths) + (names.length ? " involving " + alternatives(names) : "")
    : names.length === 1 && names[0] === "sensitive paths"
      ? "A change to a path the rules mark sensitive"
      : names.length ? "A change involving " + alternatives(names) : "A change";
  const amount = countWord(count) + (count === 1 ? " approval" : " approvals");
  const who = from === "any" ? (includeAgents ? "eligible reviewers" : "a person")
    : from === "owners" ? (includeAgents ? "eligible path owners" : "people who own the affected paths")
      : strings(from) && from.length ? (includeAgents ? listing(from) : "people in " + listing(from))
        : undefined;
  return who ? sentence(where + " needs " + amount + " from " + who + " to merge into " + branch + ".") : raw(key, rule);
}

const MERGE_CONDITIONS: Record<string, string> = {
  require_signed: "Every commit must have a verified signature",
  require_linear: "History must be linear",
  dismiss_stale_approvals: "Only approvals on the latest revision count",
  require_last_push_approval: "Someone other than the author and the last pusher must approve the latest revision",
  require_agent_review: "An agent must review the pull request",
};

function mergeRule(branch: string, key: string, value: unknown): Rule[] {
  const field = "land." + branch + "." + key;
  if (key === "approvals" && Array.isArray(value)) return value.map((a) => approval(a, branch));
  if (key === "required_checks" && strings(value)) return value.length ? [sentence("Merges into " + branch + " require the " + listing(value) + (value.length === 1 ? " check." : " checks."))] : [];
  if (key === "agents_may_land_without_human") {
    const names = classes(value);
    if (names) return [sentence(names.length
      ? "Agents may merge " + listing(names) + " on their own when targeting " + branch + "."
      : "Agents need a person's approval to merge into " + branch + ".")];
  }
  if (key === "allow") {
    if (value === null) return [];
    if (strings(value)) return [sentence(value.length ? "Only " + listing(value) + " may merge into " + branch + "." : "Merging into " + branch + " is prohibited.")];
  }
  if (Object.hasOwn(MERGE_CONDITIONS, key) && typeof value === "boolean") {
    return value ? [sentence(MERGE_CONDITIONS[key] + " to merge into " + branch + ".")] : [];
  }
  if (key === "push") return pushRules(field, value, "Merges into " + branch);
  return [raw(field, value)];
}

const PATH_CONDITIONS: Record<string, string> = {
  sensitive: "The rules treat changes under {paths} as sensitive.",
  generated: "The rules classify matching files as generated under {paths}.",
  exclusive_lease: "Changes under {paths} need an exclusive lease to merge.",
};

function accessRules(value: unknown): Rule[] {
  const access = object(value);
  if (!access) return [raw("access", value)];
  return Object.entries(access).flatMap(([path, value]) => {
    const fields = object(value);
    if (!fields) return [raw("access." + path, value)];
    return Object.entries(fields).flatMap(([action, allowed]) => {
      if (action !== "read" && action !== "write") return [raw("access." + path + "." + action, allowed)];
      if (allowed === null) return [];
      if (!strings(allowed)) return [raw("access." + path + "." + action, allowed)];
      return [sentence(allowed.length
        ? "The rule for " + path + " limits " + action + " access to " + listing(allowed) + "."
        : "The rule for " + path + " denies " + action + " access.")];
    });
  });
}

function pushRules(key: string, value: unknown, subject: string): Rule[] {
  const fields = object(value);
  if (!fields) return [raw(key, value)];
  return Object.entries(fields).flatMap(([field, constraint]) => {
    if ((field === "deny_paths" || field === "deny_extensions") && strings(constraint)) {
      const label = field === "deny_paths" ? "files at " : "files with extensions ";
      return constraint.length ? [sentence(subject + " may not add or modify " + label + listing(constraint) + ".")] : [];
    }
    if (field === "max_file_size" || field === "max_path_length") {
      if (constraint === null) return [];
      if (typeof constraint === "number" && Number.isSafeInteger(constraint) && constraint >= 0) {
        return [sentence(subject + " limit " + (field === "max_file_size" ? "file size" : "path length") + " to " + constraint + " bytes.")];
      }
    }
    return [raw(key + "." + field, constraint)];
  });
}

/** Render the server's effective policy, including unfamiliar fields in their original form. */
export function policyRules(normalizedJson?: string): Rule[] {
  if (!normalizedJson) return [];
  let value: unknown;
  try {
    value = JSON.parse(normalizedJson);
  } catch {
    return [{ text: normalizedJson, raw: true }];
  }
  const doc = object(value);
  if (!doc) return [raw("policy", value)];

  // Merge requirements explain the decisions people make most often. The other policy
  // sections follow, including new fields this reader does not yet have words for.
  const land = object(doc.land);
  const merge = land ? Object.entries(land).flatMap(([branch, rule]) => {
    const fields = object(rule);
    return fields ? Object.entries(fields).flatMap(([key, v]) => mergeRule(branch, key, v)) : [raw("land." + branch, rule)];
  }) : doc.land === undefined ? [] : [raw("land", doc.land)];

  return [...merge, ...Object.entries(doc).flatMap(([key, entry]) => {
    if (key === "land") return [];
    if (key === "access") return accessRules(entry);
    if (key === "push") return pushRules(key, entry, "Changes");
    if (key === "codeowners") {
      if (entry === null) return [];
      if (typeof entry === "string") return [sentence("The rules use " + entry + " to look up path owners.")];
    }
    const fields = object(entry);
    if (fields && (key === "groups" || key === "owners" || key === "members")) {
      return Object.entries(fields).map(([name, members]) => {
        if (!strings(members) || !members.length) return raw(key + "." + name, members);
        if (key === "groups") return sentence("The group " + name + " includes " + listing(members) + ".");
        if (key === "owners") return sentence("The owners of " + name + " are " + listing(members) + ".");
        return sentence(listing(members) + " hold the " + name + " role.");
      });
    }
    if (fields && key === "paths") {
      return Object.entries(fields).flatMap(([name, paths]) => {
        if (!Object.hasOwn(PATH_CONDITIONS, name) || !strings(paths)) return [raw(key + "." + name, paths)];
        return paths.length ? [sentence((PATH_CONDITIONS[name] as string).replace("{paths}", () => listing(paths)))] : [];
      });
    }
    if (fields && ["refs", "roles", "rulesets"].includes(key)) {
      return Object.entries(fields).map(([name, rule]) => raw(key + "." + name, rule));
    }
    return [raw(key, entry)];
  })];
}
