# Shared Seatline integration

Seatline is installed once for the user's account. The default TabBeam native
build routes real provider execution through that shared companion. TabBeam's
conversation policy, protocol and product state remain in this repository.
Conclave and future neutral-protocol clients use the same broker without loading
their app engines into Seatline.

For this development integration, first build/install `seatline-companion`
following Seatline's companion README. Build TabBeam's own native integration:

```sh
cargo build --release --locked --manifest-path native/Cargo.toml -p tabbeam-host
```

Keep that binary at a permanent location, then authorize TabBeam's exact Chrome
extension ID and register the compatibility adapter:

```sh
seatline-companion install
seatline-companion authorize tabbeam codex,claude,gemini,grok chrome-extension://EXTENSION_ID/ --cache-title=TabBeam --allow-provider-default
seatline-companion register-native tabbeam /absolute/path/to/tabbeam-host
```

`--allow-provider-default` is required for TabBeam's plain questions. A question
with no page context asks the provider to apply its own tool configuration
(`ToolPolicy::ProviderDefault`), which Seatline refuses unless the grant allows
it; without the option those questions fail with `PROVIDER_DEFAULT_TOOLS_DENIED`
(the extension shows what to do). Questions that carry page context, and web
search, never need it.

Use the absolute path to `tabbeam-host.exe` on Windows. The setup page fills in
the current extension ID. Chrome connects to `com.seatline.host`; Seatline selects
the approved TabBeam integration by the exact caller origin. Its own provider
requests use authenticated IPC with the shared broker. There is no local HTTP
listener. App adapters are an optional compatibility path for existing native
protocols; neutral-protocol extensions need no extra executable.

Do not register TabBeam's `--print-manifest` output over Seatline's shared Chrome
registration. Let Seatline's `install` and `authorize` commands manage that host
and the full set of approved extension origins.

`seatline-companion revoke tabbeam` removes TabBeam's authorization without
uninstalling Seatline or affecting other apps. Reauthorizing rotates credentials
and resets adapter registration; repeat `register-native` afterward. Provider
CLIs and their sign-ins are still managed separately.

## Two builds that do not interoperate yet

This branch produces two different products, and neither works with the other:

| | Extension connects to | Native side |
| --- | --- | --- |
| Shared-host build (default) | `com.seatline.host` | Seatline companion, plus the registered `tabbeam-host` adapter |
| Standalone packages (macOS pkg, Windows installer) | `com.tabbeam.host` | The self-contained `tabbeam-host`, built with `--no-default-features` |

An extension built from this branch does not talk to a companion installed from
a TabBeam release, and the released installers do not talk to this extension.
The shared-host build is a development installation until a signed installer
that registers `com.seatline.host` exists, and it should not be offered as the
default distribution path before then. The contract records both names
(`host_name`, `legacy_host_name` in `docs/protocol/native-messaging-v1.json`),
and CI tests both configurations.

The existing macOS/Windows packaging scripts explicitly build the legacy
standalone host with `--no-default-features`. Those packages serve extensions
using `com.tabbeam.host`; they are not installers for this shared-host extension
build. Signed shared-integration packaging and distribution remain release work.
Provider warming is unchanged: adapters continue to launch one process per turn.

## One connection per host

A `tabbeam-host` process serves its extension port for as long as Chrome keeps
the port open, so it answers many requests, often several at once. The host
makes one `seatline_companion::remote::RemoteClient` for TabBeam
(`Providers::installed`) and gives every provider a
`RemoteProvider::with_client` over it. Every request, for any provider, is a
request on that one authenticated connection to the broker, which Seatline's
client keeps on one runtime thread and opens when the first request needs it. It
replaces the earlier arrangement, in which each request started a thread and a
runtime of its own and made, authenticated and closed a connection of its own.

What follows from it, as Seatline's client defines it (see Seatline's
`docs/performance-implementation-tracker.md`, items D-01 to D-03):

- Request IDs are unique on the connection, and a cancel names one request, so
  cancelling one request never ends another.
- A client holds at most 64 requests in flight, and a request that has output
  waiting unread beyond 4 MiB is cancelled with `CONSUMER_TOO_SLOW`, so one
  stalled request cannot grow the host's memory. The broker itself runs two
  turns of one app at a time and queues up to eight, whatever the connection;
  the rest are refused with `QUEUE_FULL`.
- Authorizing TabBeam again rotates its credentials, and revoking it removes
  them. The broker closes the connections of the old grant within about a
  second (it looks once a second) and ends the requests in flight on them once
  with `COMPANION_DISCONNECTED`; they are not replayed, because the broker may
  have started a turn for them. The host's next request then connects afresh
  and reads the grant again, so the host needs no restart: the request completes
  with the new grant, or is refused with `APP_NOT_AUTHORIZED` after a revoke. A
  question asked in that first second, on the old connection, fails once with
  `COMPANION_DISCONNECTED` (retryable) and the next works. The earlier
  per-request arrangement had no such window.
- A request made while TabBeam has no grant is refused with
  `APP_NOT_AUTHORIZED` a moment later than before, from the connection's own
  thread; the message is the same.

The host's wire protocol with the extension is unchanged. Seatline's readiness
record (`ProviderState.readiness`) is the runtime's and is dropped before
`provider.status` is written.

### Checks

- `scripts/validate-brokered-concurrency.mjs` (CI job "Extension to built host
  round trip") keeps one host open against a real broker with a fake `codex`:
  four quick questions finish in about 100 ms beside a slow one that holds the
  other of the broker's two slots; cancelling that one ends only it; the
  second slow question and a later one complete; and on Linux the host's
  connection count, read from `/proc` and sampled every 10 ms, is exactly one
  throughout. It then authorizes TabBeam again and revokes it, and checks that
  the host's next question connects afresh and completes, or names the missing
  grant, with no restart. The same script fails on a host that connects per
  request.
- `scripts/validate-brokered-ask.mjs` is unchanged and still passes: plain and
  context questions complete, and a missing grant is named.
- `native/host/tests/cli.rs` keeps the host's input open until the refusal for a
  missing grant arrives (input that ends first cancels the request, and the
  refusal now takes a few milliseconds).

### Measured effect, and what it is not

`scripts/measure-brokered-latency.mjs` compares two builds of the host against
one real broker and a fake `codex` that answers at once: this branch, and the
same Seatline revision with the earlier per-request connection. Raw numbers:
`docs/performance/g02-shared-client.json` (8 rounds of 20 questions per build,
a fresh host process per round, on a 4-core Linux container).

| Build | First text, first question of a host (median / p95) | First text, later questions (median / p95) | With six questions in flight |
|---|---|---|---|
| shared connection | 21.6 / 22.2 ms | 21.5 / 26.0 ms | 1 connection, 3 threads |
| connection per request | 22.0 / 32.2 ms | 21.7 / 22.2 ms | 4 connections, 6 threads |

The adoption does not change first-text latency measurably here (the medians
differ by 0.2 to 0.4 ms, and the p95s, which one slow sample moves, differ in
both directions). That fits a local connection and its authentication costing
little next to what these figures mostly consist of and the change does not
touch: the host's own 10 ms polling tick and the adapter's process start (not
isolated by a separate experiment). Its gain is structural: the host holds one
connection and one runtime thread however many requests are in flight, where it
held a thread, a runtime and a connection for each request in flight (six in
flight showed four connections and six threads). An idle host holds one more
thread than before (the shared connection's).

**This is not a live latency measurement.** A real Codex, Claude, Gemini or
Grok start-up and model time are not in these numbers, nobody has run TabBeam
against a signed-in provider for this change, and the figures above must not be
read as what a user sees. A live comparison needs a provider account and the
opt-in live workflow (`.github/workflows/live-codex.yml`); it has not been run.

Reproduce:

```sh
cargo build --release --locked --manifest-path native/Cargo.toml -p tabbeam-host
node scripts/measure-brokered-latency.mjs --companion <seatline-companion> \
  --host shared=native/target/release/tabbeam-host \
  --host per-request=<a host built with RemoteProvider::new> --rounds 8 --asks 20
```

### Updating the Seatline revision

The Seatline crates are pinned to one exact revision in `native/Cargo.toml`, and
CI installs the broker from the same revision (`scripts/check-seatline-pin.mjs`
checks that they agree). This change moved the pin from `dc10865` to
`0cb105e4c4d753abf8fb305d8ccedeeb64dd0ef4`, which is Seatline's merge of the
remote client. The one source change the new revision forced is
`ProviderState.readiness`, which the fake provider and one test set to `None`
and `Some`. Update the pin deliberately, with the Seatline workspace tests and
this repository's checks passing at the new revision (the "External Seatline
revision" CI job runs the former).

To go back to a connection per request, change `installed_provider` in
`native/host/src/providers/mod.rs` to build `RemoteProvider::new(APP, &metadata)`
and drop the `Link`; nothing else depends on the shared connection.
