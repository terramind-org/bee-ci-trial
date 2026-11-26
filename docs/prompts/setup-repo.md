---
id: setup-repo
title: Set up Bee for a repository
when: First time working in a repository on this Beehive server. Connects the CLI, git and your MCP client, then proves it with a read.
---
You are going to connect to the Beehive server at {{endpoint}} and work in the repository {{repo}}. Do the steps in order and stop at the first one that fails: read the error, fix its cause, then continue.

You need a Beehive token from the person who asked you to do this. Put it in the environment and keep it out of files, logs and commit messages:

```sh
export BEE_ENDPOINT={{endpoint}}
export BEE_TOKEN=<the token you were given>
```

1. Install the CLI. Skip this if `bee --version` already prints a version.

   ```sh
   cargo install --locked --git https://github.com/terramind-org/bee.git bee-cli
   bee --version
   ```

   If you have a checkout of the Bee source, `cargo install --locked --path crates/bee-cli` does the same.

2. Check the endpoint scheme. The `bee` CLI speaks plain HTTP only and rejects an `https://` endpoint with the subcode `bad_endpoint`. If `{{endpoint}}` starts with `https://`, skip steps 3 to 5, use your MCP client (step 6) or the HTTP/JSON API, and go straight to step 7. The HTTP/JSON API is one POST per call:

   ```sh
   curl -sS -X POST "{{endpoint}}/bee.v1.AuthService/WhoAmI" \
     -H "Authorization: Bearer $BEE_TOKEN" -H "Content-Type: application/json" -d '{}'
   ```

3. Store the credentials so every later command finds them, then confirm who you are:

   ```sh
   printf %s "$BEE_TOKEN" | bee login --token - --endpoint {{endpoint}}
   bee whoami --json
   ```

   `whoami` prints your principal, the scope of the token (repos, paths, refs, actions), its expiry and what is left of its budget. If it fails with `UNAUTHENTICATED`, `TOKEN_EXPIRED` or `TOKEN_REVOKED`, ask for a new token; do not retry.

4. Point git at the repository, if you want to use plain git. Run this inside a git repository:

   ```sh
   bee git setup --repo {{repo}} --json
   ```

   It adds the remote `origin` at `{{endpoint}}/{{repo}}.git`, installs `bee git credential` as the credential helper for this endpoint so the token never appears in the remote URL, sets `protocol.version=2` and runs a preflight. Read `preflight.problems` in the output and fix each one. Use `--remote <name>` to keep your existing `origin`, and `--global` to write the helper to your global git config.

5. Or get a Bee working copy, which keeps no history on disk and does its writes on the server:

   ```sh
   bee clone {{repo}} work
   cd work
   ```

   For a large repository, add `--sparse <path-prefix>` (repeatable) to fetch only the paths you need. Use `bee init --repo {{repo}}` instead to bind a directory that already has files.

6. Connect your MCP client. Beehive serves MCP at `{{endpoint}}/mcp` over streamable HTTP. Add a server with that URL and the header `Authorization: Bearer <your token>`; the exact file or command depends on your MCP client. A typical entry looks like this:

   ```json
   {
     "mcpServers": {
       "bee": {
         "url": "{{endpoint}}/mcp",
         "headers": { "Authorization": "Bearer <your token>" }
       }
     }
   }
   ```

   If your client only launches local processes, register the command `bee mcp` instead (it needs an `http://` endpoint) and give it `BEE_ENDPOINT` and `BEE_TOKEN` in its environment. Every tool is named `<service>_<method>` in snake_case, for example `auth_who_am_i`, `change_open_change`, `lease_acquire_lease`. A session lists the `core` toolset (objects, queries, writes, refs, change requests, checks, leases, events, issues, repos). The rest are one call away: `search_tools` finds any tool by keyword, `list_toolsets` shows the groups, and `enable_toolset` adds a group such as `ci`. Arguments are the proto3-JSON request (lowerCamelCase fields, 64-bit integers as strings).

7. Verify with a read. Any one of these proves the token, the endpoint and your access to {{repo}}:

   ```sh
   bee log --repo {{repo}} --json
   bee cr list --repo {{repo}} --json
   ```

   Over MCP, call `query_log` with `{"repo": {"fullName": "{{repo}}"}}`. If the repository is public and you have no token, the same reads work without an `Authorization` header.

Done when `whoami` names the principal you expect and one read of {{repo}} succeeded. Report the principal, the token's expiry and the first line of the log.
