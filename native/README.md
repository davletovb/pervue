# TabBeam Native Host

Reusable provider-runtime primitives now live in the standalone [Seatline](https://github.com/davletovb/seatline) repository, pinned by this workspace to one exact revision (see `native/Cargo.toml`; `scripts/check-seatline-pin.mjs` keeps CI's installs on the same one). TabBeam owns browser policy, conversations, Native Messaging, packaging, and product-facing diagnostics.

This directory contains the native Rust companion/host.

The host serves the Chrome extension over Native Messaging. It validates each protocol-v1 request strictly, runs it through a provider adapter, and streams the answer back as protocol events. Codex and Claude are the first real providers.

## Layout

```text
native/
├── Cargo.toml       Cargo workspace: shared version, Rust 1.85+, `unsafe` forbidden
│
│   Seatline is an external Git dependency pinned in Cargo.toml.
│   TabBeam: the application over the runtime.
├── host/            tabbeam-host: the Native Messaging host (binary + library)
│   ├── src/
│   │   ├── diagnostics.rs  structured lifecycle diagnostics (JSON lines on stderr)
│   │   ├── limits.rs    browser input bounds; reexports the core frame limit
│   │   ├── manifest.rs  caller-origin checks and the Native Messaging manifest
│   │   ├── conversations/  conversation IDs, session store, recovery and forget over a provider
│   │   ├── providers/   the conversation-level provider contract, the registry of an installed host, and the `fake` scaffold
│   │   ├── protocol/    strict request validation and event emission
│   │   ├── search.rs    the search request's options (result normalization is in seatline-core)
│   │   ├── host.rs      request loop: requests side by side, cancellation, timeouts
│   │   └── main.rs      command-line entry point
│   └── tests/       command-line tests and the opt-in live Codex test
├── test_provider/   tabbeam-fake-provider: TabBeam's integration tests (conversations over each adapter, the host's hostile matrix), and the fake provider binary they run
└── fuzz/            cargo-fuzz targets of the host (`frame_reader`, `protocol`) and their seed-corpus generators
```

## Requirements

- Rust 1.85 or newer (install with [rustup](https://rustup.rs))

## Development build

```bash
cd native
cargo build
cargo test --workspace
```

`cargo build` builds only the host (`target/debug/tabbeam-host`); the fake provider is a test fixture, so `cargo test --workspace` builds and tests it. The runtime's own tests run in the standalone Seatline repository. TabBeam CI also checks out the exact pinned Seatline revision and runs its standalone workspace tests as an integration gate.

All project crates forbid `unsafe` code. TabBeam's local Clippy policy applies to application crates; Seatline carries the provider-process enforcement and its own Clippy policy in the pinned external repository (see `docs/security/trust-boundaries.md`). CI treats compiler and Clippy warnings as errors and checks formatting:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Native Messaging framing

TabBeam uses Chrome Native Messaging framing:

- 4-byte unsigned payload length in the platform's native byte order;
- followed by exactly that many payload bytes;
- zero-length payloads are valid;
- inbound and outbound frames are capped at `MAX_FRAME_SIZE` (1 MiB) defined in `host/src/framing.rs` and reexported by `host/src/limits.rs`, which tests keep equal to `docs/protocol/native-messaging-v1.json`, the copy the extension is tested against;
- oversized lengths are rejected before allocation;
- EOF before any prefix byte is clean end-of-stream;
- partial prefix/payload EOF is a truncated-frame error;
- short reads and writes are retried until the frame is complete or the stream fails.

NAT-02 validates and transports frames. NAT-03 validates protocol-v1 JSON envelopes/payloads and emits normalized protocol failures. NAT-04 and NAT-05 run provider processes and read their output, and PRO-01 to PRO-04 add the provider adapters, starting with Codex (see [Providers](#providers)).

A framing failure ends the host with a deterministic exit status, and the host's last diagnostics record (`host.stopped`, see [Diagnostics](#diagnostics)) names the reason and the exit status.

| Exit status | Meaning |
|---|---|
| 0 | Clean end of stream, or `--version` |
| 2 | I/O error while reading a request, writing an event, or printing the manifest |
| 3 | The stream ended inside a frame |
| 4 | A frame length exceeded the 1 MiB cap |
| 5 | A frame buffer could not be allocated |
| 64 | Missing or unexpected command-line arguments, including a caller origin or extension ID that isn't exact |

## Host behavior at this milestone

```bash
./target/debug/tabbeam-host --version
```

prints the host version and exits with status 0.

Chrome launches Native Messaging hosts with the caller origin as the first positional argument. TabBeam accepts the production launch shape:

```bash
./target/debug/tabbeam-host chrome-extension://<extension-id>/
```

On Windows, Chrome also passes the calling window's handle as a second argument, `--parent-window=<decimal handle>` (0 when the caller is a service worker). The host accepts that shape and ignores the handle.

The caller origin is required and must be exactly `chrome-extension://<extension-id>/`, where the ID is 32 characters from `a` to `p`. With no arguments, any other origin, unknown flags, or extra positional arguments, the host exits with usage status 64 before reading a frame. To drive the host by hand, pass a well-formed origin such as `chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/`.

Normal execution first emits exactly one `host.ready` event, then reads bounded Native Messaging frames until EOF. Each frame must contain exactly one valid protocol-v1 JSON request object.

Malformed JSON, invalid envelopes/payloads, unsupported versions, and unknown methods produce normalized `response.failed` events. Malformed requests do not terminate an otherwise usable host stream.

No object in a request may repeat a member name, including members the host does not interpret; names are compared after decoding escapes, so `"a"` and `"\u0061"` collide. A repeat in the envelope is `INVALID_ENVELOPE` and anywhere inside a method payload is `INVALID_PAYLOAD`; syntax errors still take precedence.

Requests are validated by a strict pull reader (`host/src/protocol/json.rs`) rather than a serde deserializer, because protocol v1 depends on details serde hides: duplicate member names must be rejected, member order decides whether a request ID was recovered before a syntax error, request IDs are echoed byte-for-byte, and depth overflow is classified by where it happens. Outbound event payloads are serialized with serde_json.

Two providers answer requests: `fake`, a deterministic scaffold that answers in process and that protocol tests and the golden fixtures use, and `codex`, the Codex CLI adapter (see [Providers](#providers)). Any other provider ID fails as `PROVIDER_NOT_FOUND` / `PROVIDER_NOT_INSTALLED`. Rust's standard streams pass bytes through unchanged, including Windows pipes, so no binary-mode switch is needed before framing.

## Requests in flight

The host serves requests side by side. A reader thread hands over one frame at a time, and the request loop (`host/src/host.rs`) drives every running request without blocking on any of them, so it reads a `request.cancel` while the target is still streaming. A request that can be answered at once, such as the fake provider's, is answered before the next frame is read.

- **Request IDs.** A request ID must be unique among the requests in flight, including a `request.cancel` still waiting for its target. A request that reuses one fails with `INVALID_REQUEST` / `DUPLICATE_REQUEST_ID`, and the request already running is unaffected. After a request ends, its ID can be used again.
- **Cancellation.** `request.cancel` stops its target, which ends with `response.failed` / `REQUEST_CANCELLED` / `USER_CANCELLED`. The cancellation then ends with `request.cancelled`, naming the target (protocol v1 §7.6). From the moment it is cancelled, nothing more is sent for the target but that failure, not even output its provider had already produced. Every cancellation of the same target is confirmed. A cancellation whose target isn't in flight, has ended, or is already stopping after a timeout fails with `INVALID_REQUEST` / `UNKNOWN_TARGET_REQUEST`.
- **Timeouts.** Each provider sets how long a `conversation.send` may take to start answering (`REQUEST_TIMEOUT` / `PROVIDER_START_TIMEOUT`) and how long its answer may then go without progress (`REQUEST_TIMEOUT` / `PROVIDER_RESPONSE_TIMEOUT`). A request that times out is stopped like a cancelled one. Adapters bound their own status checks.
- **Stopping.** A stopped request's provider gets the provider's grace period to exit before it is killed. If the adapter still hasn't ended the request one second after that, the host ends it anyway and drops the adapter, which kills its processes.
- **End of input.** When the extension closes the stream, each request still running gets 250 ms to stop and ends with `REQUEST_CANCELLED` / `INPUT_CLOSED`; the host exits once all have ended. If stdout closes instead, the host can't answer anyone: it records the running requests as aborted, kills their processes, and exits with status 2.
- **Frame size.** Long answers are split into `response.delta` events of at most 64 KiB, cut between characters, so every event fits in a frame whatever JSON escaping adds.
- **Fairness.** The loop delivers one request's updates for at most 5 ms before it turns to the other requests and to new frames, and an adapter stops consuming output that gives it nothing to return 5 ms past its deadline (`BUSY_LIMIT`). A provider that floods its output, with progress events, lines nobody acts on, or stderr, can't hold up other requests, new frames, or its own cancellation and timeouts (TST-04).

## Registering the host

Chrome starts the host only for the extensions its Native Messaging manifest lists in `allowed_origins`. The host prints that manifest for exact extension IDs:

```bash
./target/debug/tabbeam-host --print-manifest <extension-id> [<extension-id>...]
```

The output names the host the build registers: `com.seatline.host` by default, or `com.tabbeam.host` for the standalone configuration built with `--no-default-features` (the packaged macOS and Windows hosts). It points `path` at the absolute path the host was run from, and lists one `chrome-extension://<id>/` origin per ID. Anything that isn't a 32-character `a`–`p` ID, such as a wildcard or a full origin, is refused with status 64. The path isn't canonicalized: on macOS a symlink or `..` is kept as typed, while Linux reports the resolved path. That is deliberate, because a stable symlink can be the right path to register, where its versioned target would break on upgrade. Installers should run the host from the path they want registered (PKG-01). `extension/README.md` shows where to save the manifest for development; installers register it later (PKG-01).

## Diagnostics

While it serves a session, the host writes structured diagnostics to stderr, one JSON object per line (OBS-01). stdout carries only Native Messaging frames, so diagnostics can't corrupt them. If writing to stderr fails, for example because it's closed, the host ignores the failure and carries on. A write that blocks isn't skipped, though: a stderr pipe that nobody reads would eventually stall the host. Chrome passes a host's stderr through to its own, so that only happens when Chrome's own stderr is such a pipe, and starting Chrome with `--enable-logging=stderr` shows the records.

```text
{"ts":"2026-09-25T01:21:49.903Z","event":"host.started","host_version":"0.1.0-dev","pid":1764}
{"ts":"2026-09-25T01:21:49.903Z","event":"request.completed","request_id":"req_a","method":"conversation.send","provider_id":"fake","conversation_id":"conv_1","duration_ms":0}
{"ts":"2026-09-25T01:21:49.903Z","event":"request.failed","request_id":"req_b","method":"conversation.send","provider_id":"codex","duration_ms":0,"error":{"code":"PROVIDER_NOT_FOUND","reason":"EXECUTABLE_NOT_FOUND"}}
{"ts":"2026-09-25T01:21:49.903Z","event":"request.rejected","error":{"code":"INVALID_REQUEST","reason":"MALFORMED_MESSAGE"}}
{"ts":"2026-09-25T01:21:49.903Z","event":"host.stopped","duration_ms":0,"reason":"end_of_input","exit_code":0,"requests":2,"rejected":1}
```

Every record has `ts` (RFC 3339 UTC, with milliseconds) and `event`. A field that doesn't apply is left out. Every request the host reads gets exactly one `request.*` record, including a request it was answering when it stopped. A completed or failed request's `conversation_id` is the conversation it continued or, when it started one, the ID its `conversation.created` event announced, so the first request of a conversation correlates with the ones that follow.

| Event | Written when | Fields |
|---|---|---|
| `host.started` | Before `host.ready` | `host_version`, `pid` |
| `request.completed` | The request ended successfully: `response.completed`, or `request.cancelled` for a cancellation | `request_id`, `method`, `provider_id`, `conversation_id`, `target_request_id` (for `request.cancel`), `duration_ms` |
| `request.failed` | The request ended with `response.failed` | The same fields, plus `error` (`code` and `reason`) |
| `request.aborted` | The host stopped while answering the request, such as when stdout closed, so the extension got no terminal event | The same fields as `request.completed`, plus `reason` (why the host stopped). Its `conversation_id` is the one the request continued or, if the provider had already reported it, the one the request created |
| `request.rejected` | The request failed validation, or reused the ID of a request in flight, and never reached a provider | `request_id` if one was recovered, `method` for a reused ID, and `error` (`INVALID_REQUEST` and its reason) |
| `host.stopped` | The host is about to exit | `reason` (`end_of_input`, `io_error`, `frame_truncated`, `frame_too_large` or `allocation_failed`), `exit_code`, `duration_ms` (uptime), `requests` (requests that passed validation and were served) and `rejected` (requests that failed validation or reused an ID in flight) |

A record never contains request content: no prompt text, page context, other payload members, raw frame bytes, or error messages. Nor does it contain provider output: a provider's stderr and error messages are discarded (see [Codex](#codex)). A record copies only identifiers TabBeam made itself (SEC-02): a request ID in the shape the extension gives every request, `req_` and a UUID in lowercase hex (`req_4f1c2a7e-9b3d-4c21-8e0f-2a6b5c7d8e9f`); a provider the host serves; and a conversation this host process created. Any other identifier a request carries, such as a hand-written `req-1`, an unknown provider, or a conversation the host never created, is written as `[redacted]`. So is a conversation an earlier host process created and this one continues from its stored mapping (CON-03). So a record holds no free text: not a secret sent where an ID belongs, whatever its format, and nothing that could forge or split a record. To find their requests in the log, tools that talk to the host directly should use IDs of the extension's shape.

Command-line errors, such as a usage error or an invalid `--print-manifest` ID, are plain text on stderr, because no session is running.

## Web search

TabBeam uses the selected AI provider's own authenticated native web-search capability. There is no separate search API key, search HTTP client, or external search-provider process.

- **Auto (default).** `"search": {}` or `"backend_id": "auto"` asks the selected provider to perform native web search in the same answer turn.
- **Provider native.** `"backend_id": "provider"` is an explicit alias for the same behavior. If the provider does not advertise `web_search: true`, the host returns `SEARCH_FAILED / NATIVE_SEARCH_UNSUPPORTED`.
- **Codex.** A native-search turn enables live web search while shell, images, apps/plugins/hooks, MCP/orchestrator, and subagents remain disabled.
- **Claude.** A native-search turn exposes and auto-approves only `WebSearch`; `WebFetch` remains unavailable, and MCP loading/use remains blocked.
- **Context isolation.** Search and browser context cannot be combined. The host returns `SEARCH_FAILED / SEARCH_WITH_CONTEXT_UNSUPPORTED` before a provider runs.
- **Grounding requirement.** A native-search turn must produce at least one usable normalized source or it fails with `NATIVE_SEARCH_NO_SOURCES` instead of silently completing with an ungrounded answer.

Claude's real CLI emits WebSearch results as a `tool_result` text payload with a `Links:` JSON array; the adapter correlates it with the matching `WebSearch` tool-use ID. Current Codex `web_search` items expose only query/action metadata, so the adapter normalizes HTTP(S) links cited in completed agent messages. Both become the same `response.source` shape. Both providers decide for themselves whether to search and cite, so a search turn's prompt starts with the same instructions (`SEARCH_INSTRUCTIONS` in `seatline-core/src/prompt.rs`): search before answering, cite every page used as a Markdown link, answer the likely meanings of an ambiguous question instead of asking which was meant, and don't narrate the search. Narration a provider writes before searching anyway is dropped: Codex's message just before a `web_search` item, and Claude's text in a message that goes on to call a tool. Provider authentication, rate limits, cancellation, and timeouts stay in the existing provider/request error categories because retrieval and synthesis are one provider turn.


## Providers

Provider support is four layers, each with its own tests: the normalized adapter contract the request loop drives (PRO-01/PRO-07), the Codex and Claude adapters behind it, the stream manager that reads provider output as lines (NAT-05), and the process manager that runs provider processes (NAT-04).

### Adapter contract

Two contracts sit one on top of the other (ADR-0002): the runtime `Provider` in `providers/src/lib.rs`, with the adapters that implement it, and `ConversationProvider` in `host/src/providers/mod.rs`, which is TabBeam's. PRO-07 reconciled the first from the two working adapters, Codex and Claude; it contains only behaviors every adapter can express, and differences such as web search remain capability values rather than provider-specific methods.

**The runtime `Provider`** is what an adapter implements. It runs one neutral `Turn` (an optional system prompt, messages, model, tool policy, session policy, an optional session to continue) and knows nothing of conversations, browsers, or TabBeam's protocol.

- A `Provider` has an ID, which requests name, and `Timeouts`: how long a turn may take to start answering, how long it may then go without progress, an absolute bound for the whole turn, and how long a stopped turn's process gets to exit. Its `capabilities()` are the runtime's (streaming, continuation, web search, model selection, cancellation, and `tool_isolation`, whether it can run a turn with no tools). TabBeam's wire capabilities add page context and attachments on top; page context needs `tool_isolation`.
- `status()` and `send(turn)` start an `Exchange`: a state machine the request loop drives. `next(deadline)` returns the next `Update`, or `None` once the deadline passes, and never blocks longer, so one slow provider can't hold up other requests. Output that keeps arriving without an update can keep it working at most `BUSY_LIMIT` (5 ms) past the deadline, so a flooding provider can't either.
- Updates are the runtime's own: `Launched`, `Session` (the opaque native session a persistent turn runs in, before `Started`, and again if a result names another), `SessionLost` (a turn asked to resume a session it can't: `Confirmed` when the provider said so, `Suspected` when the run merely ended before the turn began), `Usage`, `Started`, `Delta`, `Source`, `Status`, and `Activity` (progress with nothing to show, which counts for the idle timeout), then one terminal update, `Completed`, `Failed`, or `Stopped`. Command lines, output formats, and the meaning of a session handle stay inside the adapter.
- `cancel(grace)` stops the work. The exchange then ends with `Stopped`, or with the terminal update it had already reached, and kills any process still running after `grace`.
- `cleanup_sessions(sessions)` and `cleanup_group(group)` give the file work that removes what a provider saved: the transcripts of native sessions, only those the provider provably wrote for TabBeam (the transcript must name the session and record TabBeam's private workspace as the directory it ran in; symbolic links are removed, never followed), and the per-turn cleanup records a turn's `cleanup_group` names. An adapter that saves nothing uses the defaults.

- A turn's `system` prompt is the application's own instructions, as opposed to what its user said. Every adapter sends it on a channel that is not a command line, each in the way its model was seen to follow it. Codex and Claude: first in the prompt on stdin, after an introduction that tells the model to follow the application's instructions for the whole conversation (`seatline_core::prompt::SYSTEM_INTRO`). Gemini: in the system prompt of the agent the adapter writes for the turn, because Antigravity's model refused instructions in the prompt that claimed precedence over the messages, taking them for a prompt injection. Grok: first in the prompt, under an introduction of its own that says the instructions come before, and take precedence over, the messages (`grok::SYSTEM_INTRO`); real runs followed it 16 of 16 times, a plainer header 1 of 7, and the same text in Grok's agent file, which Grok ignores, 2 of 5. It goes with every turn that carries it, so an application that resumes a native session sends it on the first turn only, and one that sets it and asks for web search means the two to agree. TabBeam sets none.

**`ConversationProvider`** is what the request loop drives: `send(SendRequest)` and `forget(conversation_id)`, in terms of the conversations the extension names, with history and browser context. `conversations::Conversations` implements it once for every runtime provider, and owns what the runtime doesn't:

- framing history and browser context into the turn's messages, and choosing its tool policy: search for a native-search turn, none for a page-context turn, and the provider's own configuration otherwise;
- the `conv_` IDs, and the map from each conversation to the native session its provider keeps for it (`conversations/store.rs`), so a conversation resumes after a restart. A request that carries no usable session but carries history starts a new conversation from that history; a search turn never resumes a session that may hold page text, and replaces it;
- the sessions a conversation leaves behind, recorded durably before they are replaced and removed in the background, so a removal that fails is retried;
- recovering when a provider says a session is gone (see each provider);
- `forget(conversation_id)`, which serves `conversation.forget` (v1 §5.4), which the extension sends when the user deletes a conversation. It removes the conversation's mapping, and asks the provider to remove its saved transcripts of every session the conversation used and to retry its per-turn cleanups. The file work runs on a thread of its own, so a large provider directory never holds up other requests. The mapping goes last, so a removal that fails (`SESSION_FORGET_FAILED`, retryable) can be retried; an unknown conversation completes. The `fake` scaffold implements `ConversationProvider` directly and keeps nothing, so its forget completes at once.

The layer tells the request loop which conversation a request serves, and whether it just created it, through a `ConversationSlot` the loop puts in the request. The loop announces a new conversation (`conversation.created`) right before the `response.started` that names it.

`Providers::installed(layout)` is the registry of an installed host: `fake`, `codex`, `claude`, `gemini`, and `grok`. `provider.status` can query any real adapter through the same request shape. `Providers::scaffold()` holds only `fake`, which starts no processes, for fuzzing and protocol tests. The `layout` names the application's namespace: every workspace, mapping and cleanup record lives under it (`platform/src/layout.rs`), and `tabbeam` resolves to TabBeam's clean-break directories established before the first release.

### Codex

`providers/src/codex/` is the Codex CLI adapter. It was written against Codex CLI 0.156.1 and verified against that release.

- **Discovery.** The adapter looks for an executable named `codex` in the host's `PATH` and then in the usual install locations that Chrome's minimal `PATH` can leave out: `/opt/homebrew/bin` (macOS), `/usr/local/bin`, `~/.local/bin`, `~/.npm-global/bin`, `~/.volta/bin`, `~/.bun/bin`, `~/bin`, and the `bin` directory of each Node version nvm installed, newest first. On Windows it looks for `codex.exe`, then `codex.cmd`, in `PATH` and `%APPDATA%\npm`. `TABBEAM_PROVIDER_PATH`, a list of directories in `PATH` form, replaces all of these, for unusual installs and hermetic tests. Relative directories are skipped, and nothing in a request affects the lookup (`core/src/discovery.rs`; the host override is applied in `platform/src/discovery.rs`).
- **Status.** `provider.status` runs `codex login status` and reads only its exit status: 0 is `authenticated` and 1 `unauthenticated`. Any other status, or no answer within 10 seconds, is `unknown`. The command's output names the account and a masked key, so it is never read. If no executable is found, the availability is `not_found`; if it can't be started, `unavailable`.
- **Requests.** A signed-out `codex exec` keeps retrying instead of failing, so each `conversation.send` first checks the sign-in the same way and fails at once if Codex is signed out. Then it runs:

  ```text
  codex exec --json --skip-git-repo-check --sandbox read-only -C <work dir> [resume <thread id>] -
  ```

  The question goes on stdin, never in an argument. `<work dir>` is Codex's workspace, which is also where both commands run: an empty directory in the user's own cache, `~/Library/Caches/TabBeam/codex-workspace` on macOS, `$XDG_CACHE_HOME/tabbeam/codex-workspace` or `~/.cache/tabbeam/codex-workspace` on other POSIX systems, and `%LOCALAPPDATA%\TabBeam\codex-workspace` on Windows. The read-only sandbox keeps Codex from changing files.
- **Workspace.** Codex follows instructions it finds where it runs: `AGENTS.md` in its working directory and, when a directory above it holds `.git`, in each directory from that one down (checked with Codex CLI 0.156.1). So before every launch the host checks that nobody but the user can change the workspace or anything above it (SEC-02, `platform/src/workspace.rs`). On POSIX, the workspace must be a directory of the user's own, not a link, and the host sets it to mode 0700. Every directory above it, both as named and with links resolved, must belong to the user or root and be writable by its owner alone. A sticky directory such as `/tmp` doesn't qualify, since anyone can still add `.git` and `AGENTS.md` to it. A directory of the user's may also be writable by the user's private group, which most Linux distributions give each user, with a umask of 002: the user's primary group, named after the user, with no other members listed. A shared group, such as macOS's `staff`, doesn't qualify. Every link on the way must belong to the user or root, and Codex gets the resolved path. Only owners and permission bits are read, not access-control lists. On Windows, the workspace must be a directory, not a link or junction, in `%LOCALAPPDATA%`, which Windows keeps private to the user. Without a cache directory, the workspace is a new directory with a random name in the temporary directory, which passes only where that is private to the user, as on macOS and Windows. A workspace that can't be created, or that other users could change, stops Codex from starting: the status is `unavailable`, and a question fails with `WORKSPACE_UNAVAILABLE`.
- **Environment.** Codex gets a minimal environment (SEC-02): the variables every provider gets (see [Provider processes](#provider-processes)), Codex's own `CODEX_HOME`, `CODEX_SQLITE_HOME`, and `CODEX_CA_CERTIFICATE`, and a `PATH` that starts with the executable's own directory, because npm installs `codex` as a Node script that finds `node` there. So Codex signs in with its stored login, the one its status reports, even when Chrome was started from a terminal holding `OPENAI_API_KEY` or `CODEX_API_KEY`.
- **Answers.** Codex prints one JSON event per line (`providers/src/codex/output.rs`). The response starts at `turn.started`. Each completed agent message is a `response.delta`, with a blank line before each message after the first. Other items, such as reasoning and tool calls, count as progress for the idle timeout. `turn.completed` completes the request. Exec mode reports each message whole when it completes, not token by token. In a native-search turn each message is held until the next event: a `web_search` item after it marks it as narration, which is dropped; anything else shows it.
- **Conversations.** A new native session gets a random opaque `conv_` ID with 16 hex digits. The conversation layer saves its mapping to the Codex thread, which the adapter reports as an opaque handle, before announcing it, under the user's data directory (`$XDG_DATA_HOME/tabbeam/codex-sessions`, `~/.local/share/tabbeam/codex-sessions`, or `%LOCALAPPDATA%\\tabbeam\\codex-sessions`). Files are private to the user on Unix. A fresh host recovers the mapping and resumes the thread. If a mapping was lost but the request includes prior dialogue, Codex starts a new thread with that bounded history; the extension keeps its own stable conversation ID and updates the native session metadata. The same happens when a resumed `codex exec` ends before its turn begins: why isn't known (the thread may be gone, or Codex may have crashed), so the old mapping is kept and the dialogue starts a new conversation; without history, the failure is reported as it is. If neither a mapping nor a history is available, the request fails with `UNKNOWN_CONVERSATION`. When no private user data directory can be found, the installed host refuses to persist new sessions.
- **Deleting.** Forgetting a conversation removes its mapping and Codex's saved sessions of its thread: `rollout-…-<thread>.jsonl` files under `$CODEX_HOME/sessions` (by date) and `$CODEX_HOME/archived_sessions` whose `session_meta` names the thread and TabBeam's `codex-workspace`. Codex's own state database is left untouched, so it may keep a reference to the thread.
- **Limits.** Codex gets 60 seconds to start answering and 5 minutes without progress, because a model can think for minutes without any output. A stopped request's Codex gets 2 seconds to exit before it is killed. After the turn ends, Codex gets 5 seconds to save its session and exit before it is stopped; the answer stands either way. A line of output over 8 MiB ends the request.
- **Capabilities.** `streaming`, `continuation`, `web_search`, `page_context`, and `cancellation` are `true`. `attachments` and `model_selection` remain `false`. Selection/page context is validated at the native boundary and framed in the Codex prompt as untrusted reference data, separate from the user question. Context turns are answer-only: TabBeam disables Codex shell/image/apps/plugins/hooks/web-search/orchestrator-MCP/subagent surfaces (`features.multi_agent=false` and `features.multi_agent_v2=false`). Normal plugin cache/install artifacts do not block context. TabBeam refuses only user-level standalone `mcp_servers` configuration that it cannot yet deterministically disable (`PAGE_CONTEXT_TOOLS_ENABLED`).

Failures map to the normalized errors of `docs/protocol/errors-and-capabilities-v1.md`. Codex's own messages and stderr can hold URLs, account details, and masked keys, so they are never forwarded or logged: every failure carries a fixed message.

| Situation | `code` / `reason` | Retryable |
|---|---|---|
| No `codex` executable | `PROVIDER_NOT_FOUND` / `EXECUTABLE_NOT_FOUND` | no |
| `codex login status` says signed out | `PROVIDER_NOT_AUTHENTICATED` / `LOGIN_REQUIRED` | no |
| A turn failed on a 401 or 403 status or a rejected API key | `PROVIDER_NOT_AUTHENTICATED` / `AUTH_REJECTED` | no |
| A turn failed on a 429 status, a rate or usage limit, or a quota | `PROVIDER_FAILED` / `PROVIDER_RATE_LIMITED` | yes |
| Any other failed turn | `PROVIDER_FAILED` / `PROVIDER_UNAVAILABLE` | yes |
| Codex exited with an error before the turn ended, as in a crash or a lost session | `PROVIDER_FAILED` / `PROCESS_EXITED` | yes |
| Output that isn't Codex's event stream: a line that isn't an event, events out of order, a line over 8 MiB, or a clean exit before the turn ended | `PROVIDER_FAILED` / `MALFORMED_PROVIDER_OUTPUT` | no |
| Codex couldn't be started | `PROVIDER_FAILED` / `PROVIDER_UNAVAILABLE` | no |
| Its workspace couldn't be created, or other users could change it | `PROVIDER_FAILED` / `WORKSPACE_UNAVAILABLE` | no |
| A `conversation_id` the adapter doesn't know | `INVALID_REQUEST` / `UNKNOWN_CONVERSATION` | no |
| Native session mapping cannot be stored | `INTERNAL_ERROR` / `SESSION_STORE_FAILED` | yes |
| Browser context with user-configured standalone MCP servers | `INVALID_REQUEST` / `PAGE_CONTEXT_TOOLS_ENABLED` | no |

[Seatline's `seatline-tests/tests/codex_provider.rs`](https://github.com/davletovb/seatline/blob/e021c2acf05132073d82bf3e2149f1ff64f1f49f/seatline-tests/tests/codex_provider.rs) runs the adapter against a fake `codex`: the fake provider binary, linked under that name (see `fake-provider/README.md`), at the runtime's level, where a `Turn` goes in and `Update`s come out. The tests cover discovery and sign-in status, a Codex that can't start, a workspace that can't be made or that other users could change, a workspace reached through a link, the exact command line and the question on stdin, the exact environment and working directory Codex gets, streaming, resuming the thread a turn reported, how much of Codex's own configuration may apply to a turn, browser context delivered as untrusted reference data, failed turns, crashes, malformed and oversized output, cancellation, both timeouts, and what cleanup removes. `test_provider/tests/codex_adapter.rs` covers what TabBeam adds, through the whole host: the thread a conversation continues, a thread a new host recovers without exposing it, the bounded dialogue that rebuilds a lost one, conversation IDs that never reach the command line, and deleting a conversation. [Seatline's `providers/src/codex/fixtures/`](https://github.com/davletovb/seatline/blob/e021c2acf05132073d82bf3e2149f1ff64f1f49f/providers/src/codex/fixtures/) holds `codex exec --json` output captured from Codex CLI 0.156.1, and `output.rs`'s tests parse it.

`host/tests/live_codex.rs` is the opt-in smoke test against the real Codex (TST-05). Through the built host, it checks Codex's status, asks it one question, and checks that the answer streams to completion: discovery → send → stream → completion. `TABBEAM_LIVE_CODEX` turns it on:

```bash
cd native
TABBEAM_LIVE_CODEX=1 cargo test -p tabbeam-host --test live_codex -- --nocapture
```

Unset, as in CI's usual runs, the test passes at once. Set to `1`, it is skipped, and passes, when Codex isn't installed or isn't signed in. Set to `required`, those fail it instead. Before printing anything, it checks the events and the host's diagnostics for the values of `OPENAI_API_KEY` and `CODEX_API_KEY` and for anything shaped like an API key. The **Live Codex smoke test** workflow (`.github/workflows/live-codex.yml`) runs it only when started by hand: with a repository secret `OPENAI_API_KEY`, Codex signs in with it, reading it on stdin, and the test runs with `TABBEAM_LIVE_CODEX=required`; without the secret, the test is skipped.

### Claude

`providers/src/claude/` is the Claude Code CLI adapter (PRO-05/06).

- **Discovery.** Claude uses the same platform-controlled `SearchPath` rules as Codex, but searches for the fixed executable name `claude`. No request or webpage can choose an executable path.
- **Status.** `provider.status` runs `claude auth status` and uses only its exit status: 0 means authenticated, 1 unauthenticated, and anything else is unknown. stdout/stderr are discarded because the command can identify the account.
- **Requests.** TabBeam runs Claude in non-interactive print mode with `--output-format stream-json --input-format stream-json --verbose --include-partial-messages --permission-mode default --strict-mcp-config --disallowedTools "mcp__*"`. Plain turns add `--tools ""`; native-search turns use `--tools WebSearch --allowedTools WebSearch`. No other built-in tool, including `WebFetch`, is available. The user prompt is a structured JSON user message on stdin, never an argv value. The process runs in its own private empty `claude-workspace`, using the same workspace trust checks as Codex, and automatic updating is disabled while TabBeam owns the process.
- **Answers.** `stream_event` text deltas become `response.delta`; separate assistant messages are separated by a blank line. In a native-search turn the prompt first asks Claude to search, cite each page as a Markdown link, and answer without narrating (`SEARCH_INSTRUCTIONS`), and each message's text is held until it's clear it isn't narration before a search: a `tool_use` block starting in the same message drops it, while the message ending, or the text passing 600 bytes, shows it and streams the rest live. If a compatible Claude build produces no partial text, the final `result.result` is used as a fallback answer. Unknown progress events remain provider-neutral `Activity` updates. After a successful `result`, Claude gets a short finish grace to shut down, even if it keeps writing output; a lingering process is then stopped without turning the completed answer into a timeout.
- **Conversations.** Claude's `system/init` session ID reaches the host only as an opaque handle. TabBeam exposes an opaque `conv_…` ID and resumes the mapped Claude session with `--resume`. The conversation layer persists the conversation→session mapping in TabBeam's user-data directory and a fresh host recovers it; with no user-data directory it is kept in memory for the host's lifetime. A mapping is rewritten only when Claude reports a different session, and a failed rewrite after an answer has streamed doesn't fail that answer. If the mapping is missing and bounded dialogue history is available, the layer starts a fresh Claude session under a new conversation. If Claude says the mapped session no longer exists (in its `result`, or on stderr before `init`), the adapter reports the session lost, and the layer drops the stale mapping and, with history, retries once as a fresh Claude session under the same conversation ID; without history the request fails as `UNKNOWN_CONVERSATION`. Any other failure of a resumed run keeps the mapping, so a crash or a rejected flag never discards the native session.
- **Deleting.** Forgetting a conversation removes its mapping and Claude Code's saved files for its session, under `$CLAUDE_CONFIG_DIR` or `~/.claude`: `projects/<project>/<session>.jsonl` transcripts whose records name the session and TabBeam's `claude-workspace`, the directory beside each, and the session's `session-env`, `tasks`, and `file-history` directories. A transcript Claude recorded elsewhere, even with the same session ID, is left alone.
- **Hooks.** Claude Code runs the hooks in the user's own Claude settings for TabBeam's questions too, just as it does in a terminal: a hook that logs or forwards prompts sees TabBeam's questions, and a hook's output can add to what Claude sees. TabBeam leaves hooks on deliberately; they are the user's own configuration, and organization-managed hooks can't be turned off from here anyway. Tool-use hooks can fire on native-search turns because `WebSearch` is deliberately enabled there; plain turns expose no built-in tools, and MCP servers remain blocked.
- **Cancellation and failures.** Cancellation uses the shared process/stream manager. Authentication, rate-limit, process-exit, malformed-output, and availability failures map into the same normalized error vocabulary as Codex.
- **Capabilities.** `streaming`, `continuation`, `web_search`, `page_context`, `model_selection`, and `cancellation` are `true`; `attachments` is `false`. Selection/page context is framed in the prompt as untrusted JSON reference data, separate from the question, exactly as for Codex. A context turn is a plain turn: `--tools ""` gives Claude no tools, MCP stays blocked, and the host refuses context combined with search, so page text can only inform an answer, never make Claude act. Like any prompt, it is visible to the user's own prompt hooks.

[Seatline's `seatline-tests/tests/claude_provider.rs`](https://github.com/davletovb/seatline/blob/e021c2acf05132073d82bf3e2149f1ff64f1f49f/seatline-tests/tests/claude_provider.rs) runs the adapter against a fake `claude` at the runtime's level, and `test_provider/tests/claude_adapter.rs` covers TabBeam's conversations over it. The provider-neutral contract (TST-10) runs against every adapter twice: [Seatline's `seatline-tests/tests/provider_contract.rs`](https://github.com/davletovb/seatline/blob/e021c2acf05132073d82bf3e2149f1ff64f1f49f/seatline-tests/tests/provider_contract.rs) through the runtime's `Provider` alone, for Codex, Claude, Gemini and Grok (lifecycle and ordering of updates, sessions, search, refusals, cancellation, deadlines, cleanup), and `test_provider/tests/provider_contract.rs` as TabBeam serves them, over conversations.

The opt-in live smoke test exercises the built host against the installed Claude CLI:

```bash
cd native
TABBEAM_LIVE_CLAUDE=1 cargo test -p tabbeam-host --test live_claude -- --nocapture
```

Set `TABBEAM_LIVE_CLAUDE=required` when Claude is expected to be installed and authenticated.

The opt-in live smoke tests of the four providers run at the runtime's level in the standalone Seatline repository, through the adapter and nothing of TabBeam's; the two host-level tests above cover TabBeam's own layer on top of the same Codex and Claude adapters:

```bash
git clone https://github.com/davletovb/seatline.git
cd seatline
git checkout e021c2acf05132073d82bf3e2149f1ff64f1f49f
SEATLINE_LIVE_GEMINI=1 cargo test --locked -p seatline-tests --test live_gemini -- --nocapture
SEATLINE_LIVE_GROK=1 cargo test --locked -p seatline-tests --test live_grok -- --nocapture
SEATLINE_LIVE_CODEX=1 cargo test --locked -p seatline-tests --test live_codex -- --nocapture
SEATLINE_LIVE_CLAUDE=1 cargo test --locked -p seatline-tests --test live_claude -- --nocapture
```

Unset, each passes at once; `1` runs it when the CLI is installed and signed in and skips it otherwise; `required` fails when it isn't. Each checks the status and sign-in, a plain answer, that a system prompt is followed, and a web search with its sources (Gemini, Codex, Claude) or the refusal of search (Grok); and that nothing holding the prompt outlives an ephemeral turn, in the private workspaces and in what the CLI keeps of its own: Antigravity's `~/.gemini/antigravity-cli` (its transcript under `brain` and the conversation database under `conversations`), Grok's `~/.grok`, Codex's `CODEX_HOME` (`~/.codex`) and Claude's `CLAUDE_CONFIG_DIR` (`~/.claude`). The search of those directories reads the files modified since the run began (a file that holds the run's marker was written during it), so a home of any size is searched in full; it reads at most 20,000 of them, skips any over 16 MiB, and says so when it left files unread. What a model says is checked for credentials before it is printed, a system-prompt check gets a second try (a model's compliance is not certain), and `required` Grok needs a fresh cached sign-in (`grok models` refreshes an expired one). Codex's plain and system-prompt turns run with tools off, so a user's own Codex configuration that exposes tools Codex can't switch off makes the test skip (or fail, in `required`) with that reason. Seatline's manual `live-gemini.yml`, `live-grok.yml`, `live-codex.yml`, and `live-claude.yml` workflows run those runtime-level checks. TabBeam's `live-codex.yml` now runs only the host-level Codex check. The owner has run all four for real (`agy` 1.2.13, `grok` 1.0.41, `codex` 0.154.0, `claude` 2.1.236, macOS, signed in) and they pass: Codex and Claude follow the shared introduction on the first attempt, Gemini follows its agent file, and Grok follows its own introduction (19 of 20 attempts, every run passing).

### Stream manager

[Seatline's `seatline-core/src/stream.rs`](https://github.com/davletovb/seatline/blob/e021c2acf05132073d82bf3e2149f1ff64f1f49f/seatline-core/src/stream.rs) is the stream manager (NAT-05). `LineStream` reads a provider process's stdout as lines. `next(deadline)` returns one complete line at a time, and then exactly one terminal state, which later calls repeat:

- `Final(exit)` when stdout ended and the process exited. A last line without a line ending is still delivered.
- `Error` when a line grew past the stream's limit or wasn't UTF-8. The process is killed at once.
- `Stopped(exit)` after `cancel(grace)`.

The process manager's chunks end wherever a read did, even inside a UTF-8 character. The stream manager reassembles lines across any number of chunks, drops a `\r` before a `\n`, and checks each line is UTF-8 once it is complete. The limit counts a line as delivered, without its line ending. It holds at most one line of up to the limit and the lines of one chunk, and the process manager reads at most 16 chunks ahead, so a provider that floods its output waits on its own writes instead of growing the host's memory. stderr is counted and discarded, because it is written for people and can hold secrets. `cancel(grace)` drops everything not yet delivered, asks the process to stop (`request_stop`), and kills it once `grace` passes.

Output that arrives without completing a line, such as stderr, the start of a long line, or anything after a cancel, keeps `next(deadline)` working at most `BUSY_LIMIT` (5 ms) past its deadline; a stopped process is killed when its grace period ends even while it floods.

`split_text` cuts outgoing text into pieces of bounded size, never inside a character; the host uses it for `response.delta`.

The unit tests in [Seatline's `seatline-core/src/stream.rs`](https://github.com/davletovb/seatline/blob/e021c2acf05132073d82bf3e2149f1ff64f1f49f/seatline-core/src/stream.rs) pin the line splitting: characters cut between chunks, CRLF endings, a last line without an ending, the limit, and invalid UTF-8. [Seatline's `seatline-tests/tests/stream_manager.rs`](https://github.com/davletovb/seatline/blob/e021c2acf05132073d82bf3e2149f1ff64f1f49f/seatline-tests/tests/stream_manager.rs) runs the stream manager on real processes: 2,000 lines full of multi-byte characters, stderr, a line over the limit, a 2 MiB line read slowly, a crash, and cancelling between lines, mid-line, and against a provider that ignores SIGTERM.

### Provider processes

`seatline-core/src/process.rs` is the provider process manager (NAT-04). It is the only code in the native workspace that starts a provider process; the Codex adapter reaches it through the stream manager.

```rust
let mut process = Process::spawn(&ProcessSpec::new(executable).args(["exec", "--json"]))?;
process.write(prompt.as_bytes())?;
process.close_stdin();

let deadline = Instant::now() + timeout;
while let Some(event) = process.next_event(deadline) {
    match event {
        Event::Stdout(bytes) => { /* provider output */ }
        Event::Stderr(bytes) => { /* provider diagnostics */ }
        Event::Exited(exit) => return Ok(exit),
    }
}
let exit = process.terminate(Duration::from_secs(2)); // the deadline passed
```

- **Starting.** The program must be an absolute path, and each argument is its own argv element: there is no shell and no `PATH` search. stdin, stdout, and stderr are always three separate pipes, so a provider never gets the host's own Native Messaging streams.
- **Environment.** A provider's environment starts empty: it gets only the variables its spec sets (`env`, `envs`), none of the host's, and runs in the spec's `current_dir` when it sets one (SEC-02). `platform/src/environment.rs` lists what every provider gets from the host's environment, when set: `HOME`, `USER`, `LOGNAME`, `TMPDIR`, `LANG`, `LC_ALL`, `LC_CTYPE`, `DBUS_SESSION_BUS_ADDRESS` and `XDG_RUNTIME_DIR` (where Linux keyrings are found), the `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, and `NO_PROXY` proxies in either case, and `SSL_CERT_FILE` and `SSL_CERT_DIR`. On Windows it is what programs, `cmd.exe`, and Node need to run (`SystemRoot`, `ComSpec`, `PATHEXT`, `TEMP`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, and the like), with the proxies and CA certificates. Each adapter adds its provider's own settings and sets `PATH`. Everything else stays behind: credentials such as `OPENAI_API_KEY`, `AWS_SECRET_ACCESS_KEY`, or `GITHUB_TOKEN`, variables that change how programs load code such as `NODE_OPTIONS`, `LD_PRELOAD`, and `DYLD_INSERT_LIBRARIES`, and TabBeam's own settings.
- **Input.** `write` queues bytes for a helper thread and returns at once, so a provider that isn't reading can't block the host. `close_stdin` sends end of file after the queued input.
- **Output.** `next_event` returns stdout and stderr chunks of at most 8 KiB (`MAX_CHUNK_BYTES`) as the provider writes them, then one final `Exited`. At most 16 chunks are read ahead of the caller; beyond that the provider waits on its own writes, so a flood can't grow the host's memory. Chunks end wherever a read did, so they can split lines and UTF-8 sequences; reassembling them is the stream manager's job (NAT-05).
- **Timeouts.** `next_event` returns `None` once its deadline passes, and the caller decides what happens next.
- **Stopping.** `terminate(grace)` closes stdin, sends SIGTERM to the provider's process group, waits up to `grace`, and then kills the group with SIGKILL. `kill()` sends SIGKILL at once. Both reap the provider and return its `Exit`: the status, whether it exited on its own (`Natural`), after the request (`Stopped`), or was `Killed`, and whether its output closed. `request_stop()` closes stdin and sends SIGTERM without waiting, for callers that keep reading events until the provider exits, as the stream manager does.
- **Cleanup.** Dropping a `Process` kills and reaps it, so no provider outlives its `Process`, not even as a zombie. The provider leads its own process group, so stopping it stops everything it started, and when it exits on its own the host kills whatever it left in the group, which would otherwise live on as an orphan and could hold its output open. The host sees the exit before it reaps the provider, with `waitid` and `WNOWAIT` on Linux and kqueue's `NOTE_EXIT` on macOS, and kills the group first: until the provider is reaped, its process ID, which is also the group's, can't be reused, so the kill can't reach another group.

What it can't do yet:

- A descendant that leaves the process group, for example with `setsid`, is out of reach. If it holds the output open, the host stops waiting one second after the provider exits and reports `output_closed: false`.
- On POSIX systems other than Linux, Android, FreeBSD, and macOS, the host can't see an exit before reaping, so it kills the group right after. If the provider left nothing behind, the group ID is free again by then, and another group that took it within those microseconds would be hit.
- On Windows only the provider process itself is stopped; stopping its descendants too needs a Job Object (ADR-0001). Windows has no SIGTERM, so closing stdin is the only stop request there.
- If the host itself is killed outright, it can't clean up. A provider that reads stdin sees end of file.

The package that builds the fake provider tests the manager, because only it can locate the fake provider binary: [Seatline's `seatline-tests/tests/process_manager.rs`](https://github.com/davletovb/seatline/blob/e021c2acf05132073d82bf3e2149f1ff64f1f49f/seatline-tests/tests/process_manager.rs) covers success, a nonzero exit, a crash, a timeout, a graceful stop, an ignored stop escalated to SIGKILL, input written while output flows, a process that sees only the environment its spec sets and runs where its spec says, arguments holding spaces, quotes, and shell syntax that each arrive whole, and descendants that stay in the group, outlive the provider, or leave the group. [Seatline's `seatline-tests/tests/process_stress.rs`](https://github.com/davletovb/seatline/blob/e021c2acf05132073d82bf3e2149f1ff64f1f49f/seatline-tests/tests/process_stress.rs) spawns and stops 120 providers at different points and checks that each was reaped and that no pipe or thread was left behind.

### Hostile providers

`test_provider/tests/hostile_matrix.rs` is the hostile fake-process matrix (TST-04). It runs the whole host (the request loop, the Codex adapter, the stream manager, and the process manager) against a fake `codex` that misbehaves in one way per case, and checks that each request ends in its normalized outcome within a time bound:

| Case | Outcome |
|---|---|
| Every line arrives a few bytes at a time | the whole answer |
| 128 MiB of stderr while answering | the whole answer |
| stderr without end, and no progress | `REQUEST_TIMEOUT` / `PROVIDER_RESPONSE_TIMEOUT` |
| 100,000 progress events, then the answer | the whole answer |
| Progress without end, cancelled | `REQUEST_CANCELLED`, then `request.cancelled` |
| Unknown events without end | `REQUEST_TIMEOUT` / `PROVIDER_RESPONSE_TIMEOUT` |
| Nonzero exit, or a crash, mid-turn | `PROVIDER_FAILED` / `PROCESS_EXITED` |
| A hang before the turn starts | `REQUEST_TIMEOUT` / `PROVIDER_START_TIMEOUT` |
| A hang mid-turn | `REQUEST_TIMEOUT` / `PROVIDER_RESPONSE_TIMEOUT` |
| Cancellation ignored, quietly or while flooding | `REQUEST_CANCELLED` once the grace period ends |
| A line that isn't JSON, or isn't UTF-8 | `PROVIDER_FAILED` / `MALFORMED_PROVIDER_OUTPUT` |
| A 330 KB answer | the whole answer, in deltas of at most 64 KiB |
| A 9 MiB line, or a line without end | `PROVIDER_FAILED` / `MALFORMED_PROVIDER_OUTPUT` |
| A sign-in check that floods | given up on at its time limit: the status is `unknown`, and the question is asked anyway |
| Four of these at once, next to a plain question and a cancel | each its own outcome; the answer and the cancel within 2 seconds |

Every request ends exactly once, with a matching diagnostics record, every process is reaped, and neither the provider's stderr nor any question reaches the events or the diagnostics. On Linux the test also checks that the host's threads and file descriptors return to where they were after each case, and that its peak memory grows by at most 64 MiB, far less than the floods write; the longest line held is 8 MiB. The matrix found that a provider writing fast enough could keep the loop busy indefinitely, holding up other requests, cancellations, and timeouts, which the fairness limits above now prevent. A stopped process, or a sign-in check past its time limit, is also cut off on time however fast it writes.

[Seatline's `seatline-tests/tests/hostile_matrix.rs`](https://github.com/davletovb/seatline/blob/e021c2acf05132073d82bf3e2149f1ff64f1f49f/seatline-tests/tests/hostile_matrix.rs) runs the same misbehaviour through none of TabBeam's code. Each case runs an adapter (Codex for every case above, and Claude for silence, unknown events, malformed output, an exit without a result, and ignored cancellation) under the scheduler's supervisor, which owns the lifecycle limits and the panic boundary. The endings are the scheduler's: `Completed` with the whole answer, `Failed` with the normalized code and reason, `Cancelled`, and `Timeout` naming the limit (`Start` or `Idle`). Each turn ends exactly once, every process is reaped, nothing the provider wrote to stderr reaches the events, and on Linux the same thread, file-descriptor and peak-memory bounds hold (`seatline-fake-provider::resources` measures them for both matrices). [Seatline's `seatline-tests/tests/service.rs`](https://github.com/davletovb/seatline/blob/e021c2acf05132073d82bf3e2149f1ff64f1f49f/seatline-tests/tests/service.rs) drives real adapters through the threaded service: an answer, a refused turn, a cancelled turn, a dropped turn and a stopped service each stop their provider process, and one silent turn holds up no other.

## Fuzz targets

With Node 22, nightly Rust and [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) (`cargo install cargo-fuzz`), the host's targets run from the default fuzz directory, and the runtime's from its own:

```bash
cd native
node fuzz/fuzz-support.mjs frame-corpus fuzz/corpus/frame_reader
node fuzz/fuzz-support.mjs protocol-corpus fuzz/corpus/protocol
cargo +nightly fuzz run frame_reader fuzz/corpus/frame_reader -- -runs=1000 -max_len="$(node fuzz/fuzz-support.mjs max-len frame_reader)"
cargo +nightly fuzz run protocol fuzz/corpus/protocol -- -runs=2000 -max_len="$(node fuzz/fuzz-support.mjs max-len protocol)"

# In a Seatline checkout at e021c2acf05132073d82bf3e2149f1ff64f1f49f:
node seatline-fuzz/fuzz-support.mjs stream-corpus seatline-fuzz/corpus/stream_lines
cargo +nightly fuzz run --fuzz-dir seatline-fuzz stream_lines seatline-fuzz/corpus/stream_lines -- -runs=1000 -max_len="$(node seatline-fuzz/fuzz-support.mjs max-len stream_lines)"
```

cargo-fuzz builds the targets with AddressSanitizer. `frame_reader` reads frames from memory until the first non-frame result. `protocol` runs each input through the whole host as one request frame and fails if any emitted frame is not a JSON object. Both belong to TabBeam (`fuzz/`, package `tabbeam-host-fuzz`). `stream_lines` belongs to the external Seatline runtime ([`seatline-fuzz/`](https://github.com/davletovb/seatline/blob/e021c2acf05132073d82bf3e2149f1ff64f1f49f/seatline-fuzz/), package `seatline-fuzz`, which depends on `seatline-core` alone): it splits its input into lines under a byte limit, once in chunks whose sizes the input chooses and once whole, and fails if the two disagree or a line breaks the limit.

The harnesses read directly from memory rather than creating a temporary file per input. The generated corpora seed empty, small valid, exact-maximum, oversized-prefix, truncated-prefix, and truncated-payload frames, plus requests derived from the golden protocol fixtures, and lines at, over and cut across the limit, so smoke runs start from structurally meaningful inputs.

### Grok

The Grok provider uses Grok Build's **one-shot headless** interface, not `grok agent stdio` and not a persistent app-server. TabBeam targets the currently shipped stable CLI surface: each turn launches one `grok` child with `--prompt-file`, `--output-format streaming-messages-json`, `--include-partial-messages`, and `--agent <definition-file>`, then owns that child until it exits. Follow-ups resend TabBeam's bounded dialogue history instead of depending on Grok's saved-session continuation.

TabBeam separates authentication from customization. Each child gets a private per-turn `GROK_HOME`; the cached account credential is located using Grok's normal precedence — an existing `GROK_AUTH_PATH`, otherwise `$GROK_HOME/auth.json`, otherwise `~/.grok/auth.json` — and is passed back only as `GROK_AUTH_PATH`. Direct API-key billing is disabled with `GROK_DISABLE_API_KEY_AUTH=1`, and ambient xAI API keys/custom endpoints are not inherited. Auto-update, subagents, memory, workflows, web fetch, feedback, prompt suggestions, and provider telemetry upload are disabled.

Shipped Grok Build keeps the MCP discovery/dispatch umbrellas `search_tool` and `use_tool` available unless explicitly denied, even with a restricted agent definition. TabBeam therefore applies a session-level clamp and explicitly denies `Agent,search_tool,use_tool`; it also clamps built-ins to `web_search` and then disables web search, yielding a text-only ordinary/context turn. The adapter verifies the real `system/init` boundary before forwarding answer text: OAuth auth, a compatible Grok model (including a resolved versioned alias), the canonical private cwd, an empty effective tool set, empty skills, and no active MCP server. Missing boundary fields fail closed, and any later tool-bearing block stops the turn.

**Web search is currently reported unsupported for Grok.** Upstream Grok Build `main` documents a backend `web_search` stream, but the shipped stable CLI verified during PRO-09 does not expose that TabBeam-safe path; its search attempts use the `search_tool`/`use_tool` MCP umbrellas instead. TabBeam will not weaken the text-only boundary to simulate search. The extension therefore disables its Web switch for Grok until a shipped CLI exposes the structured backend search surface TabBeam can verify and normalize safely.

Grok output is intentionally reported as non-streaming at the protocol capability level for now. TabBeam still requests partial-message frames and converts thinking/partial/system activity into internal activity updates so long reasoning does not look idle; user-visible answer text is forwarded from complete assistant/result messages. Provider failures are normalized without exposing raw stderr, and model/workspace/tool/skills/MCP boundary failures have distinct diagnostic reasons.

Per-turn state lives under TabBeam's private Grok workspace and is removed off the host loop after the child exits. Workspaces carry a heartbeat marker named for the application's namespace (`.tabbeam-owner` for TabBeam; `grok::owner_file`); a second browser/profile leaves a live turn alone, while startup recovery schedules only stale directories that carry it for cleanup. This also recovers prompt/context files after a hard-killed host without deleting another host's in-flight turn.

### Gemini

The Gemini provider uses Google's Antigravity CLI (`agy`), not the legacy `gemini` CLI. `agy models` is the status probe: a successful probe reports the provider authenticated, a recognized sign-in failure reports unauthenticated, and other probe failures report unavailable. TabBeam never passes Google/API-key or ambient Antigravity variables from Chrome; Antigravity uses its own cached account sign-in, and auto-update is disabled inside provider runs.

Each answer is a one-shot `agy` process in a private per-turn workspace using `--input-format stream-json --output-format stream-json --sandbox`. TabBeam writes a workspace-local Markdown agent (named for the application's namespace: `tabbeam-text`, or `tabbeam-search` for a web turn; `gemini::agent_name`) with `inheritCustomizations: false`, no MCP, skills, plugins, rules, subagents, hooks, or command execution. Ordinary/context turns expose no tools; Web turns expose only `search_web`. The adapter verifies the selected agent (`PROVIDER_AGENT_NOT_USED`) and a safe permission mode (`request-review`, `proceed-in-sandbox`, or `strict`; `always-proceed` is `PROVIDER_PERMISSIONS_TOO_OPEN`) at `init`, fails closed if any unapproved tool or subagent actually runs or a step type Antigravity doesn't document appears, and treats a Web answer as grounded only after an observed `search_web` step plus at least one usable cited HTTP(S) source. Real `agy` (1.2.x) opens every turn by echoing the prompt as a `user_input` step and can add `system_message` and `unknown` steps: they count as activity, and their text is never shown. An `agent_response` step streams as ACTIVE fragments; its DONE update may name only its `step_index`, and one that repeats the whole text shows only what's new. Each agent-response step is held until the following step shows whether it was narration for another search; narration immediately before any `search_web` step is not forwarded or saved into later history.

Antigravity continuation is deliberately not used. TabBeam sends the protocol's bounded dialogue history on every follow-up, so a provider-side session disappearing cannot strand the conversation and no browser-supplied conversation ID reaches Antigravity argv. After `result`, the adapter gives `agy` a short finish grace, bounds busy output with the shared stream fairness limit, classifies stderr only into fixed normalized errors, and then removes both the private turn workspace and the TabBeam-launched Antigravity transcript under `~/.gemini/antigravity-cli/brain/<id>`. The transcript ID is recorded before the init boundary is accepted; if init was never consumed, cleanup can identify only transcripts that record that turn's unique private workspace. Cleanup failures do not discard an otherwise-complete answer: safe transcript IDs are retained in private TabBeam cleanup records across host restarts, and `conversation.forget` retries them.
