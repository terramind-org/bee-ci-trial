#!/usr/bin/env node
// Generates TypeScript for the wire API with protobuf-es (protoc + the pinned protoc-gen-es).
//
//   node scripts/gen.mjs           regenerate lib/gen/
//   node scripts/gen.mjs --check   exit 1 when lib/gen/ differs from a fresh generation
//
// Input: every file in proto/bee/v1, the wire contract. protoc must be on PATH, as for the
// rest of the repo.
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, readdirSync, rmSync, mkdirSync, cpSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const web = join(dirname(fileURLToPath(import.meta.url)), "..");
const repo = join(web, "..", "..");
const out = join(web, "lib", "gen");
const plugin = join(web, "node_modules", ".bin", process.platform === "win32" ? "protoc-gen-es.cmd" : "protoc-gen-es");
const roots = [join(repo, "proto")];

function protoFiles(root) {
  const found = [];
  const walk = (d) => {
    for (const name of readdirSync(d).sort()) {
      const p = join(d, name);
      if (statSync(p).isDirectory()) walk(p);
      else if (name.endsWith(".proto")) found.push(relative(root, p));
    }
  };
  walk(root);
  return found;
}

function generate(dest) {
  mkdirSync(dest, { recursive: true });
  for (const root of roots) {
    const files = protoFiles(root);
    if (!files.length) continue;
    execFileSync(
      "protoc",
      [
        "--plugin=protoc-gen-es=" + plugin,
        "--es_out=" + dest,
        "--es_opt=target=ts,json_types=true",
        ...roots.map((r) => "-I" + r),
        ...files.map((f) => join(root, f)),
      ],
      { stdio: ["ignore", "inherit", "inherit"] },
    );
  }
}

function listing(dir) {
  const files = new Map();
  const walk = (d) => {
    for (const name of readdirSync(d).sort()) {
      const p = join(d, name);
      if (statSync(p).isDirectory()) walk(p);
      else files.set(relative(dir, p), readFileSync(p, "utf8"));
    }
  };
  walk(dir);
  return files;
}

const tmp = mkdtempSync(join(tmpdir(), "bee-web-gen-"));
try {
  generate(tmp);
  if (process.argv.includes("--check")) {
    const want = listing(tmp);
    let have;
    try {
      have = listing(out);
    } catch {
      have = new Map();
    }
    const stale = [...new Set([...want.keys(), ...have.keys()])].filter((f) => want.get(f) !== have.get(f)).sort();
    if (stale.length) {
      console.error("lib/gen is stale; run `bun run gen`. Differs: " + stale.join(", "));
      process.exit(1);
    }
    console.log("lib/gen is up to date (" + want.size + " files)");
  } else {
    rmSync(out, { recursive: true, force: true });
    cpSync(tmp, out, { recursive: true });
    console.log("generated " + listing(out).size + " files into lib/gen");
  }
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
