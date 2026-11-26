// A scripted MCP client over stdio: spawns `bee mcp` and speaks JSON-RPC 2.0 to it, one JSON
// message per line. It plays the part of an agent runtime without a model: every call is fixed
// by the script (initialize, tools/list, tools/call).

import { fail } from "./rpc";

export interface ToolResult {
  isError: boolean;
  /** The tool's JSON result (structuredContent, else the first text block parsed as JSON). */
  data: any;
  text: string;
}

export class McpClient {
  private readonly proc: ReturnType<typeof Bun.spawn>;
  private nextId = 1;
  private readonly pending = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void }>();
  private buffer = "";
  private closed = false;
  stderr = "";

  constructor(command: string[], env: Record<string, string>) {
    this.proc = Bun.spawn(command, {
      env: { ...process.env, ...env },
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
    void this.pump(this.proc.stdout as ReadableStream<Uint8Array>, (chunk) => this.onStdout(chunk));
    void this.pump(this.proc.stderr as ReadableStream<Uint8Array>, (chunk) => (this.stderr += chunk));
    void this.proc.exited.then(() => {
      this.closed = true;
      for (const p of this.pending.values()) p.reject(new Error(`bee mcp exited: ${this.stderr.slice(-500)}`));
      this.pending.clear();
    });
  }

  private async pump(stream: ReadableStream<Uint8Array>, onText: (s: string) => void): Promise<void> {
    const dec = new TextDecoder();
    for await (const chunk of stream) onText(dec.decode(chunk, { stream: true }));
  }

  private onStdout(text: string): void {
    this.buffer += text;
    let nl: number;
    while ((nl = this.buffer.indexOf("\n")) >= 0) {
      const line = this.buffer.slice(0, nl).trim();
      this.buffer = this.buffer.slice(nl + 1);
      if (!line) continue;
      let msg: any;
      try {
        msg = JSON.parse(line);
      } catch {
        fail(`bee mcp wrote a line that is not JSON on stdout: ${line.slice(0, 200)}`);
      }
      if (msg.id !== undefined && this.pending.has(msg.id)) {
        const p = this.pending.get(msg.id)!;
        this.pending.delete(msg.id);
        if (msg.error) p.reject(new Error(`JSON-RPC error ${msg.error.code}: ${msg.error.message}`));
        else p.resolve(msg.result);
      }
    }
  }

  private send(message: unknown): void {
    if (this.closed) fail("bee mcp is not running");
    (this.proc.stdin as { write(s: string): number; flush(): unknown }).write(`${JSON.stringify(message)}\n`);
    (this.proc.stdin as { flush(): unknown }).flush();
  }

  request(method: string, params: unknown = {}, timeoutMs = 60_000): Promise<any> {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`MCP ${method} timed out after ${timeoutMs} ms`));
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (v) => (clearTimeout(timer), resolve(v)),
        reject: (e) => (clearTimeout(timer), reject(e)),
      });
      this.send({ jsonrpc: "2.0", id, method, params });
    });
  }

  notify(method: string, params: unknown = {}): void {
    this.send({ jsonrpc: "2.0", method, params });
  }

  /** The MCP handshake. Returns the server's `initialize` result. */
  async initialize(): Promise<any> {
    const result = await this.request("initialize", {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "bee-exit-test", version: "1" },
    });
    this.notify("notifications/initialized");
    return result;
  }

  async listTools(): Promise<{ name: string; description?: string; inputSchema?: unknown }[]> {
    const names: { name: string }[] = [];
    let cursor: string | undefined;
    do {
      const r = await this.request("tools/list", cursor ? { cursor } : {});
      names.push(...r.tools);
      cursor = r.nextCursor;
    } while (cursor);
    return names;
  }

  /** `tools/call`. A tool-level error comes back as `isError: true`, not as an exception. */
  async call(name: string, args: unknown): Promise<ToolResult> {
    const r = await this.request("tools/call", { name, arguments: args });
    const text = (r.content ?? []).filter((c: any) => c.type === "text").map((c: any) => c.text as string).join("\n");
    let data = r.structuredContent;
    if (data === undefined) {
      try {
        data = JSON.parse(text);
      } catch {
        data = undefined;
      }
    }
    return { isError: r.isError === true, data, text };
  }

  /** Closes stdin (the server returns when it closes) and waits for the process. */
  async close(): Promise<void> {
    if (!this.closed) {
      try {
        (this.proc.stdin as { end(): unknown }).end();
      } catch {
        /* already closed */
      }
      const timer = setTimeout(() => this.proc.kill(), 5000);
      await this.proc.exited;
      clearTimeout(timer);
    }
  }
}
