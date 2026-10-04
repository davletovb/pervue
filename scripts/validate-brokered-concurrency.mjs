#!/usr/bin/env node
// Runs requests side by side through one host and a real Seatline broker.
//
// Chrome keeps one host process per extension port, so a host serves many
// requests over its life, often several at once. This keeps one host open
// against a real broker with a fake `codex` and checks what TabBeam depends on
// from the shared connection to the broker:
//
//   * quick questions finish while slow ones are still running;
//   * cancelling one request ends that request alone;
//   * through all of it the host holds exactly one connection to the broker
//     (Linux: counted from /proc), not one per request;
//   * after TabBeam is authorized again (its credentials rotate) or revoked, the
//     host connects afresh for its next question and needs no restart.
//
// Linux and macOS only (the broker listens on a Unix socket); the connection
// count is checked on Linux.
import assert from "node:assert/strict";
import { parseArgs } from "node:util";
import { BROKER_SWEEP_MS, Host, SLOW_SECONDS, TERMINAL, cancel, install, plainAsk } from "./brokered-support.mjs";

const { values } = parseArgs({ options: { companion: { type: "string" }, host: { type: "string" } } });
if (!values.companion || !values.host) {
  console.error("usage: validate-brokered-concurrency.mjs --companion <seatline-companion> --host <tabbeam-host>");
  process.exit(2);
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const installation = await install(values.companion);
const host = new Host(values.host, installation.env);
/**
 * Waits for the broker to close the connection of a grant that is gone: it
 * looks once a second. Where the connection can be seen (Linux) that is what is
 * waited for, so a slow machine does not make the next question race it.
 */
async function brokerLetsGo(what) {
  if (!host.resources()) return sleep(BROKER_SWEEP_MS);
  for (let waited = 0; host.resources().sockets !== 0 && waited < 15_000; waited += 25) await sleep(25);
  assert.equal(host.resources().sockets, 0, `the broker kept the connection after ${what}`);
}
const connections = (when) => {
  const resources = host.resources();
  if (resources) assert.equal(resources.sockets, 1, `the host should hold one connection to the broker ${when}, held ${resources.sockets}`);
  return resources;
};
try {
  await host.next((frame) => frame.event === "host.ready", "host.ready");

  // 1. A first question connects; the connection then stays for the next ones.
  host.send(plainAsk("warm"));
  assert.equal((await host.ended("warm")).frame.event, "response.completed");
  const warm = connections("after the first question");

  // 2. A slow question and four quick ones in flight together. The broker runs
  // two turns of one app at a time and queues the rest in order, so the quick
  // ones take the slot the slow one leaves free, one after another.
  const quick = ["quick_1", "quick_2", "quick_3", "quick_4"];
  const started = performance.now();
  host.send(plainAsk("slow_a", "SLOW question"));
  for (const id of quick) host.send(plainAsk(id));
  // Sampled the whole time, so a connection per request cannot slip between checks.
  let samples = 0;
  let threads = warm?.threads ?? 0;
  const sampler = setInterval(() => {
    const resources = connections("while five requests were in flight");
    if (resources) { samples++; threads = Math.max(threads, resources.threads); }
  }, 10);
  let answered;
  try {
    answered = await Promise.all(quick.map((id) => host.ended(id)));
  } finally {
    clearInterval(sampler);
  }
  for (const { frame } of answered) assert.equal(frame.event, "response.completed", JSON.stringify(frame));
  const quickMs = Math.max(...answered.map(({ at }) => at)) - started;
  assert.ok(quickMs < (SLOW_SECONDS * 1000) / 2, `quick questions waited ${quickMs.toFixed(0)} ms, behind the slow one`);
  assert.equal(host.of("slow_a").some((frame) => TERMINAL.has(frame.event)), false, "the slow question ended before the quick ones");

  // 3. A second slow question; cancelling the first ends that one alone.
  host.send(plainAsk("slow_b", "SLOW question"));
  host.send(cancel("cancel_a", "slow_a"));
  const cancelled = (await host.ended("slow_a")).frame;
  assert.equal(cancelled.event, "response.failed", JSON.stringify(cancelled));
  assert.equal(cancelled.payload.error.code, "REQUEST_CANCELLED");
  await host.next((frame) => frame.request_id === "cancel_a" && frame.event === "request.cancelled", "the cancel to be answered");
  assert.equal(host.of("slow_b").some((frame) => TERMINAL.has(frame.event)), false, "cancelling slow_a ended slow_b");

  // 4. The slot the cancelled one left serves a quick question while the second
  // slow one is still running; then that one completes with its answer.
  host.send(plainAsk("quick_5"));
  assert.equal((await host.ended("quick_5")).frame.event, "response.completed");
  assert.equal(host.of("slow_b").some((frame) => TERMINAL.has(frame.event)), false, "slow_b ended before quick_5");
  const busy = connections("while a slow question was still running");
  const finished = (await host.ended("slow_b", SLOW_SECONDS * 1000 + 20_000)).frame;
  assert.equal(finished.event, "response.completed", JSON.stringify(finished));
  assert.ok(host.of("slow_b").some((frame) => frame.event === "response.delta" && String(frame.payload?.text).includes("Brokered answer")));

  // 5. The same connection serves what comes next.
  host.send(plainAsk("after"));
  assert.equal((await host.ended("after")).frame.event, "response.completed");
  const after = connections("after the cancellation");

  // 6. Authorizing TabBeam again rotates its credentials. The broker closes the
  // connections of the old grant within a second, so after that the host's next
  // question connects afresh and completes, with no restart of the host.
  installation.authorize();
  await brokerLetsGo("the grant was rotated");
  host.send(plainAsk("rotated"));
  const rotated = (await host.ended("rotated")).frame;
  assert.equal(rotated.event, "response.completed", JSON.stringify(rotated));
  connections("after the grant was rotated");

  // 7. Revoking it is reported as what it is, and authorizing again recovers.
  installation.revoke();
  await brokerLetsGo("the grant was revoked");
  host.send(plainAsk("revoked"));
  const revoked = (await host.ended("revoked")).frame;
  assert.equal(revoked.event, "response.failed", JSON.stringify(revoked));
  assert.equal(revoked.payload.error.reason, "APP_NOT_AUTHORIZED");
  installation.authorize();
  host.send(plainAsk("authorized_again"));
  assert.equal((await host.ended("authorized_again")).frame.event, "response.completed");
  connections("after authorizing again");

  await host.close();
  const report = warm ? ` (one connection held at ${samples} samples through the concurrent run and after each step; threads ${warm.threads} idle, at most ${threads} under load, ${after.threads} after)` : "";
  console.log(`Brokered concurrency passed: four quick questions finished in ${quickMs.toFixed(0)} ms beside a slow one, one cancel ended one request, and the host carried on after its grant was rotated and revoked${report}`);
} catch (error) {
  host.kill();
  throw error;
} finally {
  await installation.remove();
}
