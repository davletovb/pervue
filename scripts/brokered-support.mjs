// What the brokered scripts share: a scratch Seatline installation with a fake
// `codex`, a real broker, and a host process kept open the way Chrome keeps one,
// so several requests can be in flight on it at once.
//
// Linux and macOS only (the broker listens on a Unix socket).
import { spawn, execFileSync } from "node:child_process";
import { once } from "node:events";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, readlinkSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { NATIVE_LITTLE_ENDIAN, frameNativeMessage } from "./protocol-support.mjs";

export const ORIGIN = "chrome-extension://abcdefghijklmnopabcdefghijklmnop/";
export const TERMINAL = new Set(["response.completed", "response.failed"]);

/** How often the broker looks for connections whose grant is gone, plus a margin. */
export const BROKER_SWEEP_MS = 2200;

/** How long a prompt containing `SLOW` makes the fake `codex` wait, in seconds. */
export const SLOW_SECONDS = 3;

const FAKE_CODEX = `#!/bin/sh
if [ "$1" = "--version" ]; then echo 'codex-cli 0.1.0'; exit 0; fi
if [ "$1" = "login" ]; then echo 'Logged in using ChatGPT'; exit 0; fi
prompt=$(cat)
case "$prompt" in *SLOW*) sleep ${SLOW_SECONDS};; esac
echo '{"type":"thread.started","thread_id":"test-session"}'
echo '{"type":"turn.started"}'
echo '{"type":"item.completed","item":{"id":"message-1","type":"agent_message","text":"Brokered answer"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":2,"output_tokens":3}}'
`;

/** A scratch installation: a fake `codex`, TabBeam's grant, and a running broker. */
export async function install(companionPath) {
  const companion = resolve(companionPath);
  // Provider workspaces need trusted ancestors, and a Unix socket path must be
  // short, so the scratch directory lives under the working directory.
  const root = mkdtempSync(join(process.cwd(), ".brokered-"));
  const providers = join(root, "providers");
  mkdirSync(providers);
  const codex = join(providers, "codex");
  writeFileSync(codex, FAKE_CODEX);
  chmodSync(codex, 0o700);
  const env = {
    ...process.env,
    HOME: root,
    XDG_CACHE_HOME: join(root, "cache"),
    XDG_DATA_HOME: join(root, "data"),
    SEATLINE_DATA_DIR: join(root, "seatline"),
    TABBEAM_PROVIDER_PATH: providers,
  };
  for (const name of ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"]) delete env[name];
  const authorize = () => execFileSync(companion, ["authorize", "tabbeam", "codex", "--cache-title=TabBeam", ORIGIN, "--allow-provider-default"], { env, stdio: "ignore" });
  authorize();
  const broker = spawn(companion, ["serve"], { env, stdio: "ignore" });
  for (let i = 0; i < 100 && !existsSync(join(env.SEATLINE_DATA_DIR, "broker.sock")); i++) await new Promise((r) => setTimeout(r, 50));
  if (!existsSync(join(env.SEATLINE_DATA_DIR, "broker.sock"))) throw new Error("the broker did not start");
  return {
    env,
    broker,
    /** Authorizes TabBeam again, which rotates its credentials. */
    authorize,
    /** Removes TabBeam's authorization. */
    revoke: () => execFileSync(companion, ["revoke", "tabbeam"], { env, stdio: "ignore" }),
    async remove() {
      broker.kill();
      await once(broker, "exit").catch(() => {});
      rmSync(root, { recursive: true, force: true });
    },
  };
}

/** One host process whose stdin stays open until `close()`. */
export class Host {
  constructor(hostPath, env) {
    this.child = spawn(resolve(hostPath), [ORIGIN], { env, stdio: ["pipe", "pipe", "pipe"] });
    this.child.stdin.on("error", () => {});
    this.stderr = "";
    this.child.stderr.on("data", (chunk) => { this.stderr += chunk; });
    this.closed = once(this.child, "close");
    this.frames = [];
    this.waiters = [];
    this.pending = Buffer.alloc(0);
    this.child.stdout.on("data", (chunk) => this.#read(chunk));
  }

  get pid() { return this.child.pid; }

  #read(chunk) {
    this.pending = Buffer.concat([this.pending, chunk]);
    const at = performance.now();
    for (;;) {
      if (this.pending.length < 4) break;
      const length = NATIVE_LITTLE_ENDIAN ? this.pending.readUInt32LE(0) : this.pending.readUInt32BE(0);
      if (this.pending.length < 4 + length) break;
      const frame = JSON.parse(this.pending.subarray(4, 4 + length).toString("utf8"));
      this.pending = this.pending.subarray(4 + length);
      this.frames.push({ at, frame });
    }
    this.waiters = this.waiters.filter((waiter) => !waiter());
  }

  /** Sends one request; returns when it was written. */
  send(request) {
    const at = performance.now();
    this.child.stdin.write(frameNativeMessage(Buffer.from(JSON.stringify({ version: 1, type: "request", ...request }))));
    return at;
  }

  /** Resolves with the first frame matching `predicate`, now or later. */
  next(predicate, what, timeoutMs = 30_000) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`timed out waiting for ${what}\nframes: ${JSON.stringify(this.frames.map((f) => f.frame))}\n${this.stderr}`)), timeoutMs);
      const check = () => {
        const found = this.frames.find(({ frame }) => predicate(frame));
        if (!found) return false;
        clearTimeout(timer);
        resolve(found);
        return true;
      };
      if (!check()) this.waiters.push(check);
    });
  }

  /** The frames of one request, in order. */
  of(requestId) {
    return this.frames.filter(({ frame }) => frame.request_id === requestId).map(({ frame }) => frame);
  }

  ended(requestId, timeoutMs) {
    return this.next((frame) => frame.request_id === requestId && TERMINAL.has(frame.event), `${requestId} to end`, timeoutMs);
  }

  firstText(requestId, timeoutMs) {
    return this.next((frame) => frame.request_id === requestId && frame.event === "response.delta", `${requestId}'s first text`, timeoutMs);
  }

  /** Ends the host's input, as Chrome does, and waits for it to exit. */
  async close() {
    this.child.stdin.end();
    const timer = setTimeout(() => this.child.kill(), 10_000);
    await this.closed;
    clearTimeout(timer);
  }

  kill() { this.child.kill(); }

  /**
   * What the host holds right now, from /proc (Linux only): its sockets other
   * than its standard streams, which are its connections to the broker, and
   * its threads.
   */
  resources() {
    if (process.platform !== "linux") return null;
    // Not 0, 1 and 2: Node connects a child's standard streams as socket pairs.
    const sockets = readdirSync(`/proc/${this.pid}/fd`).filter((fd) => Number(fd) > 2).filter((fd) => {
      try { return readlinkSync(`/proc/${this.pid}/fd/${fd}`).startsWith("socket:"); } catch { return false; }
    }).length;
    const threads = Number(/Threads:\s+(\d+)/.exec(readFileSync(`/proc/${this.pid}/status`, "utf8"))[1]);
    return { sockets, threads };
  }
}

export const plainAsk = (id, text = "Hello") => ({ request_id: id, method: "conversation.send", payload: { provider_id: "codex", input: { text } } });
export const cancel = (id, target) => ({ request_id: id, method: "request.cancel", payload: { target_request_id: target } });

export function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)];
}
export function percentile(values, p) {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1)];
}
