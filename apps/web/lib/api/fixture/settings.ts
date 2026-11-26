/** Repository settings at the fixture's wire boundary. */
import type { AgentService, AgentInstructionFileJson } from "../../gen/bee/v1/agents_pb";
import type { RepoRoleJson, RepoService } from "../../gen/bee/v1/forge_pb";
import { bytesToBase64, pathBytes } from "../ids";
import { BeeError } from "../transport";
import type { Role } from "../types";
import type { Ctx, Impl } from "./router";
import { digest, objectId, ulid } from "./seed";
import type { FixtureStore, RepoState } from "./store";

function isAdmin(role: Role | undefined): boolean {
  return role === "ROLE_OWNER" || role === "ROLE_ADMIN";
}

function requireAdmin(s: FixtureStore, repo: RepoState, ctx: Ctx): string {
  const actor = s.caller(ctx.token);
  const tokenId = ctx.token && s.tokenSecrets.get(ctx.token);
  const scope = tokenId ? s.tokens.find((token) => token.tokenId === tokenId)?.scope : undefined;
  if (!isAdmin(s.orgRecord(repo.owner).members.get(actor)) || (scope && !scope.actions.includes("manage_settings") && !scope.actions.includes("admin"))) {
    throw new BeeError("permission_denied", {
      reason: "POLICY_DENIED",
      message: "Repository admin access is required.",
    });
  }
  return actor;
}

export const repoSettingsImpl = {
  listCollaborators: (s, req) => {
    const repo = s.repo(req.repo);
    const rows = new Map<string, RepoRoleJson>();
    if (repo.repo.fullName === "acme/api") rows.set("triage", "REPO_ROLE_WRITE");
    if (!req.directOnly) {
      for (const [member, role] of s.orgRecord(repo.owner).members) {
        rows.set(member, isAdmin(role) ? "REPO_ROLE_ADMIN" : "REPO_ROLE_READ");
      }
    }
    const size = Math.max(1, Math.min(200, req.page?.pageSize || 50));
    const collaborators = [...rows]
      .map(([handle, role]) => ({ principal: s.p(handle), role, outsideCollaborator: !s.orgRecord(repo.owner).members.has(handle) }))
      .sort((a, b) => a.principal.id.localeCompare(b.principal.id))
      .filter((row) => row.principal.id > (req.page?.pageToken || ""));
    const items = collaborators.slice(0, size);
    return { collaborators: items, page: { nextPageToken: collaborators.length > size ? items.at(-1)?.principal.id : "" } };
  },
  listDeployKeys: (s, req, ctx) => {
    const repo = s.repo(req.repo);
    requireAdmin(s, repo, ctx);
    if (repo.repo.fullName !== "acme/api") return { keys: [] };
    // OpenSSH's two length-prefixed fields, with a deterministic demo public key.
    const ssh = new Uint8Array(51);
    ssh.set([0, 0, 0, 11], 0);
    ssh.set(new TextEncoder().encode("ssh-ed25519"), 4);
    ssh.set([0, 0, 0, 32], 15);
    ssh.set(digest("acme api deployment"), 19);
    return { keys: [{
      id: ulid(s.ago(7 * 1440), "deploy-key:acme/api:ci"),
      title: "CI checkout",
      publicKey: "ssh-ed25519 " + bytesToBase64(ssh),
      readOnly: true,
      principalId: s.p("ci").id,
      createdAt: s.iso(s.ago(7 * 1440)),
    }] };
  },
  updateRepo: (s, req, ctx) => {
    const repo = s.repo(req.repo);
    const actor = requireAdmin(s, repo, ctx);
    const version = repo.repo.version ?? "1";
    if (req.expectedVersion && req.expectedVersion !== "0" && req.expectedVersion !== version) {
      throw new BeeError("aborted", {
        reason: "VERSION_MISMATCH",
        message: "expected version " + req.expectedVersion + ", current version is " + version,
      });
    }
    if (repo.archived) {
      throw new BeeError("failed_precondition", { reason: "ARCHIVED", message: "repo " + repo.repo.fullName + " is archived" });
    }
    if (req.visibility && req.visibility !== "VISIBILITY_UNSPECIFIED") repo.repo.visibility = req.visibility;
    repo.repo.version = String(BigInt(version) + 1n);
    s.appendAudit(actor, "repo.update", repo.repo.fullName, repo.name, true, "");
    return s.repoView(repo);
  },
} satisfies Impl<typeof RepoService>;

export const instructionImpl = {
  resolveAgentInstructions: (s, req) => {
    const repo = s.repo(req.repo);
    const commit = s.resolve(repo, req.revision?.expr ?? "");
    // Settings requests the repository root and uses the default runtime.
    const files: AgentInstructionFileJson[] = [];
    const sources = [
      [".github/copilot-instructions.md", "AGENT_INSTRUCTION_SOURCE_COPILOT_INSTRUCTIONS"],
      ["AGENTS.md", "AGENT_INSTRUCTION_SOURCE_AGENTS_MD"],
    ] as const;
    for (const [path, source] of sources) {
      const content = commit.files.get(path);
      if (content !== undefined) files.push({ path: pathBytes(path), source, content, blob: objectId("blob", content) });
    }
    const combined = sources.flatMap(([path]) => {
      const content = commit.files.get(path);
      return content === undefined ? [] : ["<!-- " + path + " -->\n" + content.trimEnd()];
    }).join("\n\n");
    return { files, combined, changeset: commit.id };
  },
} satisfies Impl<typeof AgentService>;
