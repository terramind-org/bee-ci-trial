/** The ways to clone a repo. The header's Clone menu renders them; the Agent tab is a prompt, so it has no command. */

export interface CloneTabsProps {
  fullName: string;
  /** `git clone` URL over HTTPS; empty when the server does not know its public address. */
  https: string;
  /** Present only when the server advertises an SSH endpoint. */
  ssh?: string;
}

export interface CloneTab {
  id: "https" | "ssh" | "cli" | "agent";
  label: string;
  /** The command to copy; the Agent tab has none (its prompt card copies itself). */
  command?: string;
  hint?: string;
}

/** The tabs a repo offers: HTTPS and SSH only when the server gave a URL for them. */
export function cloneTabs({ fullName, https, ssh }: Pick<CloneTabsProps, "fullName" | "https" | "ssh">): CloneTab[] {
  const tabs: CloneTab[] = [];
  if (https) tabs.push({ id: "https", label: "HTTPS", command: "git clone " + https });
  if (ssh) tabs.push({ id: "ssh", label: "SSH", command: "git clone " + ssh });
  tabs.push({ id: "cli", label: "Bee CLI", command: "bee git setup --repo " + fullName, hint: "Run it inside a git repository: it points the remote and the credential helper at this repo." });
  tabs.push({ id: "agent", label: "Agent" });
  return tabs;
}
