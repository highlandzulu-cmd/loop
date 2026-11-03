# Phase 0 Lane B · loop-protocol on the wire: design draft

Status: approved 2026-10-29 (method set, `loop-wire` crate, exit codes). Model control and the other TUI methods arrived in S3 (§12). All five PRs in §9 are built. User docs: [Running Loop from scripts](../print-mode.md).

Goal: `loop-protocol` runs over a real connection. This is S1, the first step
of the spine: the Runtime (S2) and the TUI as a Runtime client (S3) build on
it. This lane adds no Runtime command and no `--connect`. Its only user-facing
part is `--print --output-format`.

## 1. What exists today

| Piece | Where | State |
|---|---|---|
| Event model | `crates/core/loop-protocol` | `UpdateEnvelope` and `SessionUpdate` are versioned (`v`, `PROTOCOL_VERSION = 1`). They serialize as snake_case JSON with lenient readers (`Unknown`, `Other`), and `tests/wire.rs` pins the JSON. |
| Backend contract | `loop-protocol/src/backend.rs` | `AgentBackend` and `HostServices` are in-process only, over a tokio broadcast. `SessionRef`, `SessionSpec`, `BackendError`, `TurnOutcome` and `ModelChoices` have no serde. |
| JSON-RPC | `loop-acp/src/rpc.rs` | Line-delimited JSON-RPC 2.0 over `AsyncRead`/`AsyncWrite`. One ordered incoming channel, so a response arrives after every notification sent before it. Pending requests fail when the stream closes. |
| `--print` | `loop-cli/src/print_mode.rs` | Prints the last agent message as text. Exits 1 on any error. |

## 2. Layout

```
crates/core/
├── loop-protocol/     # event model + AgentBackend (unchanged deps: serde, async-trait, tokio sync)
│   └── src/rpc/       # NEW, behind no feature: message types, methods, initialize (serde only)
└── loop-wire/         # NEW crate: Connection, transports, RemoteBackend, serve()
    ├── connection.rs  # lifted from loop-acp/src/rpc.rs onto a message transport
    ├── transport/     # memory (mpsc), lines (stdio, Unix socket), websocket
    ├── client.rs      # RemoteBackend: impl AgentBackend over a Connection
    └── server.rs      # serve(backend, host, transport)
```

- **Why a new crate:** `loop-protocol` stays a dependency-light contract, as its README promises and the architecture review recommends. Transports need `tokio` io/net, `tokio-util` and a WebSocket library.
- **loop-acp moves onto `loop-wire::Connection`** with its line transport. Its behaviour doesn't change, and its tests must stay green.

## 3. Framing and transports

A transport sends and receives whole messages (`Stream<Item = String>` + `Sink<String>`).

| Transport | Framing | Use |
|---|---|---|
| memory | an mpsc pair | in-process; tests |
| lines | one JSON message per line | stdio, Unix sockets (Linux and macOS) |
| websocket | one JSON message per text frame | remote Surfaces, later the Web app |

Security (see `tui-client.md` §7):
- **Unix socket:** created with mode 0600, under the agent directory's private `run/` directory.
- **WebSocket:** `loop runtime start --listen IP:PORT` accepts loopback binds only until TLS exists. It always requires a freshly generated bearer token, even on loopback, because any web page can open `ws://127.0.0.1`. The token lives in `run/runtime.token` (0600) under a user-owned 0700 directory, outside sandbox-readable paths. WebSocket connections carry Remote authority, including through SSH tunnels. Handshakes have a five-second deadline. Each WebSocket transport queue holds at most 128 messages and 64 MiB; overflow disconnects the client for replay.

## 4. Methods

JSON-RPC 2.0. Params and results are snake_case, with the same lenient-reader rules as the event model.

| Method | Direction | Kind | Params → result |
|---|---|---|---|
| `initialize` | client → server | request | `{protocol_version, client_info, client_capabilities}` → `{protocol_version, server_info, capabilities}` |
| `session/new` | c → s | request | `SessionSpec` → `SessionRef` |
| `session/load` | c → s | request | `{session, spec}` → `SessionRef` (history arrives as replayed updates) |
| `session/transcript` | c → s | request | `{session}` → `[SessionUpdate]` |
| `session/prompt` | c → s | request | `{session, content: [ContentBlock]}` → `TurnOutcome` |
| `session/cancel` | c → s | notification | `{session}` |
| `session/update` | s → c | notification | `UpdateEnvelope` |
| `host/interact` | s → c | request | `InteractionRequest` → `InteractionOutcome` |
| `shutdown` | c → s | request | `{}` → `{}` |

- **Ordering:** the `session/prompt` response is sent only after that turn's `turn_ended` update. A client that has the outcome has therefore seen the whole turn.
  - Backends don't promise which comes first: ACP resolves the outcome before it publishes `turn_ended`. So one forwarder task sends everything to the client.
  - A prompt is registered with the forwarder before the backend starts it. Its turn is the session's next `turn_started`, and the response goes out once that turn's `turn_ended` has been sent and the outcome is known.
  - If the backend refused the prompt before any turn started, the response goes out 2 s after the outcome.
- **Busy:** a second `session/prompt` on a busy session fails with `BackendError::Busy`, as it does in-process.
  - `RemoteBackend` tracks running turns from the update stream and refuses locally.
  - A turn another client started first comes back as a `-32001` error, which `RemoteBackend` reports as a failed outcome, because by then `prompt()` has already returned its handle.
- **Optional services:** `ModelControl`, `Compaction`, `SessionStats` and `LocalShell`, and the host's file system and terminals. Each is advertised in `capabilities` and gets methods in a later version. In v1, `RemoteBackend` returns `None` for each.
- **Errors:** a `BackendError` maps to a JSON-RPC error. The code is `-32000 - variant` (busy, unsupported, session not found, failed), and `data` carries the `ErrorInfo`. Malformed requests use the standard codes.
- **Lost connection:** a prompt whose connection closes before its response resolves as `TurnOutcome::Uncertain`.
- **Lag:** if the server falls behind the backend's broadcast (`RecvError::Lagged`), it sends `session/update` with a `notice` saying how many updates were missed, then closes the connection. A client never silently misses part of a turn.

## 5. Versioning and negotiation

One constant, `PROTOCOL_VERSION`, covers the events and the methods.
- The client sends the highest version it speaks. The server answers with that version if it speaks it, or else its own highest. A client that can't use the answer closes the connection.
- Additive changes, such as a new method, field or update type, keep the version. A reader ignores what it doesn't know, and calling an unknown method returns `-32601`.
- `server_info` / `client_info` are `{name, version}` for logs, never for feature checks. Feature checks use `capabilities`.

## 6. JSON Schema

- A checked-in `crates/core/loop-protocol/schema/loop-protocol.json` covers every event type and every method's params and result.
- It's generated with `schemars`, and a test fails if the file is out of date.
- Tagged enums come out as unions (`anyOf`) whose variants carry a `const` tag, so the Phase 1 TypeScript SDK generator produces discriminated unions. Each union also has an open variant for the lenient reader's `Unknown`.
- `schemars` is an optional `schema` feature of `loop-protocol`, so other builds don't compile it. The freshness test lives in `loop-wire`, which turns the feature on for its tests, so it runs in normal CI.
- The test compares JSON values, not text: some crates in the workspace turn on `serde_json`'s `preserve_order`. The file is written with keys sorted.

## 7. First consumer: `--print --output-format`

`loop --print "…" --output-format text|json|stream-json`. The default is `text`, which keeps today's output.

- **`json`:** one object when the turn ends:
  `{"type":"result","session":"…","stop":"end_turn","text":"…","usage":{…},"error":null,"exit_code":0}`
- **`stream-json`:** every `UpdateEnvelope` of the turn as one line, then the same `result` object as the last line.
- **stdout carries only the selected format.** Notices and the trace id stay on stderr.

Exit codes:

| Code | Meaning |
|---|---|
| 0 | the turn completed |
| 1 | the turn failed, or Loop failed to start (unchanged) |
| 2 | usage error (clap) |
| 3 | lost contact with the agent mid-turn; edits or commands may have run (`TurnOutcome::Uncertain`) |
| 130 | the turn was cancelled |

## 8. Tests

- **Parity, the test that decides correctness:** drive `NativeBackend` with the faux provider, once directly and once through `RemoteBackend` over each transport (memory, lines over a Unix socket, websocket). Each run must give the same update sequence and the same `TurnOutcome`.
  - The outcome resolves only after `turn_ended`.
  - `host/interact` makes a round trip: a permission prompt answered by the client.
  - A second prompt on a busy session returns `Busy`.
  - A lagging client gets the notice and then the close.
- **Negotiation:** same version; a newer client against an older server; a server version the client refuses.
- **Wire:** golden JSON for every method and error, in the style of `tests/wire.rs`.
- **Schema:** the file is up to date, and the golden examples validate against it.
- **`--print`:** each format's stdout against the faux provider, and each exit code.

## 9. PR plan

1. `--print --output-format` and exit codes. This needs no transport.
2. `loop-wire` with `Connection` lifted onto message transports, and loop-acp moved onto it (no behaviour change).
3. Serde for the backend types, the methods and `initialize`, and `RemoteBackend` + `serve`, with the in-process parity test.
4. The Unix socket and WebSocket transports, running the same parity test.
5. The JSON Schema and its freshness test.

## 10. Open questions

- The method names: `session/*` follows ACP. Is that the right choice for Loop's own API, which the SDK will expose?
- Should v1 include `ModelControl`? The TUI uses it to switch models, but S3 can add it.
- The WebSocket library: `tokio-tungstenite`. axum's `ws` feature covers only the server side.

## 11. Runtime methods (S2)

S2 adds the shared Runtime (`loop runtime` and `loop session`, user guide in [`../runtime.md`](../runtime.md),
design in [`runtime.md`](runtime.md)). All additions keep `PROTOCOL_VERSION` at 1:
an older peer ignores the new fields, and an unknown method still returns `-32601`.

**Capabilities.** `Capabilities.multi_session` and `Capabilities.attach` say a
server offers the methods below. Both default to false when absent. The Runtime
sets both.

| Method | Direction | Kind | Params → result |
|---|---|---|---|
| `session/list` | c → s | request | `{}` → `[SessionInfo{ref, cwd, name, parent, live, busy, created_ms, last_active_ms}]` |
| `session/close` | c → s | request | `{session, force?}` → `{}`. `force` (default false) cancels a running turn; without it a busy Session returns Busy |
| `session/attach` | c → s | request | `{session, after_seq?}` → `{latest_seq, generation}`. The cursor defaults to 0 |
| `session/detach` | c → s | request | `{session}` → `{}`. Stops this connection's updates and approvals for the Session; the turn keeps running |
| `runtime/status` | c → s | request | `{}` → `RuntimeStatus{pid, socket, listen_url?, generation, loaded, busy, max_sessions, max_active_turns, max_sessions_from?, max_active_turns_from?}`. `listen_url` contains no credentials. The `_from` fields say where a limit came from: `flag`, `settings` or `default` |
| `runtime/stop` | c → s | request | `{}` → `{}`. The Runtime answers, then shuts down as on SIGTERM and closes every connection |
| `host/interact` | s → c | request | `InteractionRequest` plus optional `session` (a `SessionRef`) → `InteractionOutcome` |
| `host/interact_withdrawn` | s → c | notification | `{session, id}` |

- **Runtime control.** `runtime/status` is also available over authenticated
  WebSockets. `runtime/stop` is host-only, served on the Runtime's protected
  Unix socket.
  `loop runtime status` and `loop runtime stop` use them instead of the pid
  file. A server without them returns an error.
- **Session references.** `session` is always a full `SessionRef`
  (`instance`, `key`, `native_id`), never a title.
- **List.** `session/list` returns every stored Session, closed ones
  included. `parent` is set only for a Subagent child the Runtime opened. A
  fork also stores a parent, but the list reports `parent: null` for it.
  `name` of a Subagent child is its task, shortened.
- **Escalations from Subagents.** A child's `host/interact` goes to clients
  attached to the child and to any ancestor. Its `session` names the child,
  and `host/interact_withdrawn` names the child too.
- **Attach.** The result comes first, then the updates after `after_seq` up to
  `latest_seq` with `origin: "replay"`, then live updates.
  - `seq` is the Session's durable, contiguous update sequence from the Event Log.
    It never resets across reloads or Runtime restarts.
  - Within one attachment no `seq` is sent twice. A client stores the last `seq`
    it processed and reattaches from it.
  - Replayed updates carry the current `generation`.
- **Escalations.** A `host/interact` request goes to every interactive client
  attached to the Session.
  - The first valid answer wins. The other recipients get
    `host/interact_withdrawn`.
  - Cancelling the turn, or closing the Session, withdraws the request from
    every recipient.
  - A request raised while no interactive client is attached waits for one.
  - A client that sent `client_capabilities.interact: false` is an observer: it
    receives updates but never requests.
  - The interaction `id` names the shared Escalation. JSON-RPC request ids stay
    per connection.
- **Connection ownership.** A Runtime connection tracks only the Sessions it
  created, loaded or attached.
  - `session/list` does not subscribe to anything.
  - `shutdown` on a Runtime connection returns `{}` and ends only that
    connection.
  - Losing a connection leaves its work running. Its unresolved prompts resolve
    as `TurnOutcome::Uncertain` on the client.
- **Crash recovery.** When a Runtime starts, every turn an earlier Runtime left
  open gets a `turn_ended` update with `stop: "runtime_restarted"`. That means
  the turn was interrupted and its effects are uncertain. Clients must not
  retry it automatically.

```json
{"jsonrpc":"2.0","method":"session/update","params":{"v":1,"instance":"loop","generation":4,"session":"s1","turn":"turn-2","seq":43,"origin":"replay","update":{"type":"turn_ended","turn_id":"turn-2","stop":"runtime_restarted"}}}
```

## 12. Session and workspace methods (S3)

S3 makes the TUI a Runtime client, so the TUI's controls became wire methods.
`PROTOCOL_VERSION` stays 1. Each method needs the `Capabilities` flag shown;
without it the server returns `-32002` (unsupported). All are client → server
requests, and the Runtime sets every flag. `crates/core/loop-protocol/src/rpc.rs`
has the params and results, and the checked-in schema lists them.

**Host-only** methods are served only on the Runtime's protected Unix socket.
Over an authenticated WebSocket they are refused with `-32006` (forbidden).
The list is `methods::HOST_ONLY` in `rpc.rs`.

| Method | Capability | Host-only |
|---|---|---|
| `session/models`, `session/set_model` | `set_model` | no |
| `session/efforts`, `session/set_effort` | `efforts` | no |
| `session/modes`, `session/set_mode` | `modes` | no |
| `session/compact` | `compaction` | no |
| `session/stats` | `session_stats` | no |
| `session/shell` | `local_shell` | no |
| `session/ttfb` | `session_ttfb` | no |
| `session/config` | `session_config` | no |
| `session/skill` | `session_skills` | no |
| `session/workflow` (progress is streamed) | `workflow` | no |
| `session/tree`, `session/navigate` | `session_tree` | no |
| `session/view` (transcript pages) | `branch_snapshot` | no |
| `session/rename` | `session_rename` | no |
| `session/fork`, `session/clone` | `session_fork` | no |
| `session/export`, `session/import` | `session_transfer` | no |
| `session/sandbox` | `session_sandbox` | no |
| `session/permission_mode` | `permission_mode` | no |
| `workspace/settings` (redacted) | `workspace_settings` | no |
| `workspace/mcp` | `workspace_mcp` | no |
| `workspace/web_search` | `workspace_web_search` | no |
| `workspace/skills` | `workspace_skills` | no |
| `workspace/templates` | `workspace_templates` | no |
| `workspace/reload` | `workspace_reload` | no |
| `workspace/trust` | `workspace_trust` | writes only; reads work over WebSocket |
| `runtime/sessions` | `runtime_sessions` | no |
| `runtime/usage` | `host_usage` | yes |
| `runtime/defaults` | `host_defaults` | yes |
| `runtime/providers` (login and logout) | `host_providers` | yes |
| `runtime/mcp_auth` (MCP OAuth) | `host_mcp_auth` | yes |
| `runtime/tracing` | `host_tracing` | yes |
| `runtime/stop_empty` (stop only when no Session is loaded) | `runtime_stop_empty` | yes |

Other S3 additions:

- **`session/new` environment.** `session_environment` says `session/new`
  accepts the caller's environment. A Unix socket may send all of it. Over a
  WebSocket the client sends only an allowlist plus names the user listed in
  `runtime.forwardEnv`. The environment is never logged or stored. See
  [`../runtime.md`](../runtime.md).
- **`attach_snapshot`.** `session/attach` also returns a Session snapshot.
- **Size limits.** `session/export` and `session/import` payloads are limited to
  `SESSION_TRANSFER_LIMIT` (8 MiB), and any response to `SESSION_RESPONSE_LIMIT`
  (16 MiB).
- **New error codes.** `-32005` conflict and `-32006` forbidden join the S1 codes.
