// `harness describe <Method|Message>...`: prints the proto definitions a scenario author needs
// (an RPC's request and response messages, with the messages and enums they reference, one
// level deep).

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import type { Catalog } from "./catalog";

function blocks(protoDir: string): Map<string, string> {
  const out = new Map<string, string>();
  for (const file of readdirSync(protoDir).filter((f) => f.endsWith(".proto"))) {
    const lines = readFileSync(join(protoDir, file), "utf8").split("\n");
    let name = "";
    let depth = 0;
    let buf: string[] = [];
    for (const line of lines) {
      if (!name) {
        const m = /^(message|enum)\s+(\w+)\s*\{/.exec(line);
        if (m) {
          name = m[2]!;
          depth = 0;
          buf = [];
        }
      }
      if (name) {
        buf.push(line);
        depth += (line.match(/\{/g) ?? []).length - (line.match(/\}/g) ?? []).length;
        if (depth === 0) {
          out.set(name, buf.join("\n"));
          name = "";
        }
      }
    }
  }
  return out;
}

export function describe(catalog: Catalog, names: string[], depth = 1): string {
  const defs = blocks(catalog.protoDir);
  const seen = new Set<string>();
  const out: string[] = [];
  const emit = (name: string, level: number) => {
    const short = name.replace(/^bee\.v1\./, "");
    if (seen.has(short)) return;
    const text = defs.get(short);
    if (!text) return;
    seen.add(short);
    out.push(text);
    if (level >= depth) return;
    for (const m of text.matchAll(/^\s*(?:repeated\s+|optional\s+)?(?:map<\s*\w+\s*,\s*)?([A-Z]\w*)/gm)) emit(m[1]!, level + 1);
  };
  for (const n of names) {
    const method = catalog.resolve(n);
    if (method) {
      out.push(
        `// ${method.service}.${method.method}  (${method.file})${method.clientStreaming ? "  client-streaming" : ""}${method.serverStreaming ? "  server-streaming" : ""}`,
      );
      emit(method.input, 0);
      emit(method.output, 0);
    } else if (defs.has(n)) {
      emit(n, 0);
    } else {
      out.push(`// ${n}: no such RPC or message`);
    }
  }
  return out.join("\n\n");
}
