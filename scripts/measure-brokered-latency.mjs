#!/usr/bin/env node
// Measures how long a question takes to produce its first text through a built
// host and a real Seatline broker, for one or more host builds side by side.
//
// What it measures: a fake `codex` (a shell script that answers at once), so
// the time is the host, the shared connection, the broker, the scheduler and
// the adapter's process start. It is NOT a measurement of a real provider's
// latency, which is dominated by the provider's own start-up and model; that
// needs a live run (see docs/architecture/shared-companion.md).
//
// Each round starts a fresh host process per build, as Chrome does for each
// extension port, and asks `--asks` plain questions in turn. The first
// question of a host process is reported apart from the rest: it pays for
// connecting to the broker, the rest do not when the connection is shared.
// Rounds alternate between the builds so drift in the machine's load hits all
// of them.
//
//   node scripts/measure-brokered-latency.mjs --companion <seatline-companion> \
//     --host shared=<tabbeam-host> --host per-exchange=<older tabbeam-host> \
//     [--rounds 8] [--asks 20] [--json]
import { cpus, platform, arch } from "node:os";
import { parseArgs } from "node:util";
import { Host, cancel, install, median, percentile, plainAsk } from "./brokered-support.mjs";

const { values } = parseArgs({
  options: {
    companion: { type: "string" },
    host: { type: "string", multiple: true },
    rounds: { type: "string", default: "8" },
    asks: { type: "string", default: "20" },
    json: { type: "boolean", default: false },
  },
});
if (!values.companion || !values.host?.length) {
  console.error("usage: measure-brokered-latency.mjs --companion <seatline-companion> --host [label=]<tabbeam-host> ... [--rounds N] [--asks N] [--json]");
  process.exit(2);
}
const builds = values.host.map((spec, index) => {
  const at = spec.indexOf("=");
  return at > 0 ? { label: spec.slice(0, at), path: spec.slice(at + 1) } : { label: `host${index + 1}`, path: spec };
});
const IN_FLIGHT = 6;
const rounds = Number(values.rounds);
const asks = Number(values.asks);
if (!(rounds >= 1 && asks >= 2)) {
  console.error("--rounds must be at least 1 and --asks at least 2");
  process.exit(2);
}

/**
 * What the host holds with six slow questions in flight (two run, the broker
 * queues the rest), measured once per build; then they are cancelled.
 */
async function loaded(host, round) {
  const ids = Array.from({ length: IN_FLIGHT }, (_, i) => `load${round}_${i}`);
  for (const id of ids) host.send(plainAsk(id, "SLOW question"));
  await host.next((frame) => frame.request_id === ids[0] && frame.event === "response.started", "the first slow question to start");
  await new Promise((resolve) => setTimeout(resolve, 200));
  const resources = host.resources();
  for (const id of ids) host.send(cancel(`stop_${id}`, id));
  await Promise.all(ids.map((id) => host.ended(id)));
  return resources;
}

const installation = await install(values.companion);
const samples = new Map(builds.map(({ label }) => [label, { cold: [], warm: [], threads: [], loaded: null }]));
try {
  for (let round = 0; round < rounds; round++) {
    for (const { label, path } of builds) {
      const host = new Host(path, installation.env);
      await host.next((frame) => frame.event === "host.ready", "host.ready");
      for (let ask = 0; ask < asks; ask++) {
        const id = `r${round}_a${ask}`;
        const sent = host.send(plainAsk(id));
        const text = await host.firstText(id);
        await host.ended(id);
        samples.get(label)[ask === 0 ? "cold" : "warm"].push(text.at - sent);
      }
      const resources = host.resources();
      if (resources) samples.get(label).threads.push(resources.threads);
      if (round === 0) samples.get(label).loaded = await loaded(host, round);
      await host.close();
    }
  }
} finally {
  await installation.remove();
}

const summary = (list) => ({ n: list.length, median_ms: round1(median(list)), p95_ms: round1(percentile(list, 95)), max_ms: round1(Math.max(...list)) });
const round1 = (value) => Math.round(value * 10) / 10;
const result = Object.fromEntries(builds.map(({ label }) => {
  const { cold, warm, threads, loaded } = samples.get(label);
  return [label, {
    first_ask_of_a_host: summary(cold),
    later_asks: summary(warm),
    threads_after_asks: threads.length ? median(threads) : null,
    with_six_in_flight: loaded,
  }];
}));
if (values.json) {
  console.log(JSON.stringify({ rounds, asks_per_host: asks, provider: "fake codex (shell script)", machine: `${platform()} ${arch()}, ${cpus().length} x ${cpus()[0]?.model}, node ${process.version}`, result }, null, 2));
} else {
  console.log(`First text, fake codex, ${rounds} rounds x ${asks} asks per host process (milliseconds from request written to first response.delta)`);
  for (const [label, row] of Object.entries(result)) {
    const { first_ask_of_a_host: cold, later_asks: warm } = row;
    console.log(`${label.padEnd(14)} first ask of a host: median ${cold.median_ms}, p95 ${cold.p95_ms} (n=${cold.n})   later asks: median ${warm.median_ms}, p95 ${warm.p95_ms} (n=${warm.n})` + (row.threads_after_asks === null ? "" : `   host threads after the asks: ${row.threads_after_asks}`) + (row.with_six_in_flight ? `   with six in flight: ${row.with_six_in_flight.threads} threads, ${row.with_six_in_flight.sockets} broker connection(s)` : ""));
  }
}
