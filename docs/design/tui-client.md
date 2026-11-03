# S3 · The TUI as a Runtime client: design

Status: Approved 2026-11-03 (answers to the open questions are recorded in section 10).

Current-code references were checked at
`dc98af089ab23240a901a140f8f143df6cc232a4` on `s3/design-doc`.
This document describes behavior to build. It does not report completed S3 work.
The revised S3 plan and its 17 review findings set the scope.

Goal: a **Surface** controls a **Session** through one `SurfaceClient` interface.
The TUI first uses this interface with the current native backend. It then gains
`--connect`. Plain `loop` uses an embedded **Runtime** only after ACP, trust and
ownership work there. A connected Session keeps working when its Surface leaves.

## 1. Scope and current foundations

S3 supports Linux and macOS. It keeps existing TUI commands, pickers, approvals,
attachments and configurable keys. It adds model control, tree operations,
workspace administration and usage queries over the wire. ACP startup moves to
the Runtime. The macOS sandbox and custom-provider reasoning fixes run in named
parallel lanes. Their contracts are included here; their implementation is separate.

Not in S3: the desktop migration, the S4 Policy gate, cloud hosting, Windows,
Event Log retention or timed idle unloading. S2 already unloads unneeded Sessions
at capacity. Non-loopback WebSocket serving and URL/QR control wait for S3.8b with
TLS. They are not part of S3.8's loopback listener.

Use the existing terms Session, Runtime, Surface, Event Log, Escalation, Policy
and Capability. A workspace is a host directory with a canonical identity. Its
wire id is assigned by the Runtime. A client path is never a workspace id.
The four groups below describe who owns state. They do not grant authorization.

| Current foundation | Checked source | Consequence |
|---|---|---|
| Application bootstrap owns one harness and UI state | `crates/loop-app-core/src/runtime.rs:1367` | Extract the client interface before replacing bootstrap. |
| Optional backend services | `crates/core/loop-protocol/src/backend.rs:170` | Native compact/stats/shell exist. Runtime and Remote need routing and wire methods. |
| Native model and effort changes save global settings | `crates/loop-app-core/src/runtime.rs:257`, `crates/loop-app-core/src/runtime.rs:934` | Split Session state from defaults. |
| Workspace settings and connections are cached by cwd | `crates/loop-app-core/src/session_factory.rs:82`, `crates/loop-app-core/src/session_factory.rs:313` | Admin requests need a workspace id and cache revisions. |
| Fork replaces the harness's current Session | `crates/loop-agent/src/harness/agent_harness.rs:416` | Do not use this helper unchanged in Runtime routing. |
| Runtime owns fixed Session references | `crates/loop-runtime/src/runtime.rs:660` | A fork creates another live Session and lease. |
| Attach returns only a sequence and generation | `crates/core/loop-protocol/src/rpc.rs:196` | Add a consistent state snapshot without changing cursor meaning. |
| Wire requests run independently | `crates/core/loop-wire/src/server.rs:241` | Put mutation ordering in the Session owner. |
| Current recovery scans earlier generations | `crates/loop-runtime/src/runtime.rs:307` | Scope recovery before several embedded owners share the log. |
| WebSocket transport checks an Authorization header | `crates/core/loop-wire/src/socket.rs:85` | Add listener policy and private token storage. TLS is not enabled at `Cargo.toml:76`. |

## 2. Ownership and the client interface

```text
TUI → SurfaceClient
      ├── Native adapter first
      ├── RemoteBackend over Unix socket or loopback WebSocket
      └── Embedded Runtime adapter last

Runtime → workspace definitions and connections
        → leased Sessions → native or ACP backend
        → Event Log → ordered updates → every attached Surface
```

`SurfaceClient` exposes typed backend and control services. It also supplies
immutable view data. It does not return a harness, gate, credential store or
mutable settings object. Separate local UI preferences from host state.
`app::run` and its child modules must take this interface instead of `CliRuntime`.
Their imports and argument types must prevent access to `loop_app_core::Runtime`.
Moving a direct harness call into a helper is not completion.

| Group | Owner | Wire policy | Representative operations |
|---|---|---|---|
| Session-scoped | The Session owner | Authenticated, capability checked | Model, effort, mode, stats, compact, shell, tree, forks, skills, workflows |
| Workspace-scoped admin | Runtime workspace | Workspace id required; trust changes also require host-local authority | MCP, web search, skills/templates, reload, trust, effective settings |
| Host-only | Runtime host | Unix socket or trusted embedded adapter only; never WebSocket | Provider credentials, MCP OAuth credentials, tracing, timeout defaults, usage history |
| Surface-local | TUI process | No host mutation | Theme, keys, composer, clipboard, editor, rendering, local diagnostics |

A 0600 Unix socket is inside a user-owned 0700 directory. The transport, not a
request field, supplies host-local authority. A WebSocket token authorizes the
Session and workspace operations listed here, including shell execution and
permission changes. It is a host-control credential, not a read-only share link.
Host-only methods stay forbidden even when that token is valid.

### 2.1 Slash command inventory

This is the complete built-in command set, including aliases and dynamic commands.
The source registry is `crates/loop-cli/src/commands/mod.rs:17`.
Dispatch is at `crates/loop-cli/src/commands/mod.rs:544`; effects start at
`crates/loop-cli/src/app.rs:5123`. Table references below are in `app.rs`.
All method names not in the existing RPC registry are proposals in section 3.
A local picker uses the same service calls as its slash command.

| Command and forms | Group | Wire method or local action | Current source |
|---|---|---|---|
| `/model [provider/model]` | Session-scoped | `session/models`, `session/set_model`; switching backend uses `session/new` | `crates/loop-cli/src/app.rs:5159` |
| `/effort [level]` | Session-scoped | `session/efforts`, `session/set_effort` | `crates/loop-cli/src/app.rs:5252` |
| `/permissions [read-only/ask/accept-edits/full-access/plan]`, `/plan` | Session-scoped | `session/permission_mode`; ACP choices use `session/modes`, `session/set_mode` | `crates/loop-cli/src/app.rs:5187` |
| `/sandbox [status/off/local ...]` | Session-scoped | `session/sandbox`; platform builder validates flags on the host | `crates/loop-cli/src/app.rs:5272` |
| `/new` | Session-scoped | `session/new`, then attach to the returned reference | `crates/loop-cli/src/app.rs:5405` |
| `/compact [instructions]` | Session-scoped | `session/compact` | `crates/loop-cli/src/app.rs:5510` |
| `/session` | Session-scoped | `session/stats`, `session/config`; local theme/connection details are labeled separately | `crates/loop-cli/src/app.rs:5553` |
| `/review [legacy argument]` | Session-scoped | `session/review` returns grants and rules; a legacy argument only prints migration help | `crates/loop-cli/src/app.rs:5431` |
| `/resume [id]`, `/sessions` | Session-scoped | `session/list`, then `session/attach`; attach loads when needed | `crates/loop-cli/src/app.rs:5703` |
| `/tree` | Session-scoped | `session/tree`, `session/navigate`; choosing a related Session attaches to it | `crates/loop-cli/src/app.rs:5766` |
| `/name <name>` | Session-scoped | `session/rename`; blank input prints usage | `crates/loop-cli/src/app.rs:5769` |
| `/fork` | Session-scoped | `session/tree` supplies fork points; `session/fork` creates a destination | `crates/loop-cli/src/app.rs:5872` |
| `/clone` | Session-scoped | `session/clone` creates a destination | `crates/loop-cli/src/app.rs:5875` |
| `/export [--force] [path]` | Session-scoped | `session/export` returns payloads; path choice, overwrite check and writes are Surface-local | `crates/loop-cli/src/app.rs:5776` |
| `/import <path>` | Session-scoped | Surface reads its file; `session/import` accepts a payload and returns a new reference | `crates/loop-cli/src/app.rs:5805` |
| `/skill:<name> [args]`, `/<skill-name> [args]` | Session-scoped | `session/skill` activates; args enter the local composer, without starting a turn | `crates/loop-cli/src/app.rs:5968` |
| `/workflow [--concurrency N/-c N] <goal>` | Session-scoped | `session/workflow`, then workflow progress updates | `crates/loop-cli/src/app.rs:6026` |
| `/websearch [on/off/soket/tavily/tinyfish/brave]` | Workspace-scoped admin | `workspace/web_search`; credential setup uses host-only provider flow | `crates/loop-cli/src/app.rs:5240` |
| `/skills [list/ls/enable/on/disable/off <name>]` | Workspace-scoped admin | `workspace/skills`; no args opens a local toggle picker | `crates/loop-cli/src/app.rs:5646` |
| `/<template-name> [args]` | Workspace-scoped admin | `workspace/templates` lists/renders; resulting text uses `session/prompt`, or waits in local queue | `crates/loop-cli/src/app.rs:5999` |
| `/mcp [list/ls/reload/enable/on/disable/off <name>]` | Workspace-scoped admin | `workspace/mcp`; no args opens a local toggle picker | `crates/loop-cli/src/app.rs:5898` |
| `/reload` | Workspace-scoped admin | `workspace/reload`; TUI also reloads its local theme definitions | `crates/loop-cli/src/app.rs:5628` |
| `/trust [yes/y/no/n]` | Workspace-scoped admin | `workspace/trust`; mutation is host-local only; no argument toggles after confirmation | `crates/loop-cli/src/app.rs:5613` |
| `/settings` | Workspace-scoped admin | `workspace/settings` plus `session/config`; display effective host values without secrets | `crates/loop-cli/src/app.rs:5586` |
| `/scoped-models` | Workspace-scoped admin | `workspace/settings` returns `enabled_models`; preserve current read-only help | `crates/loop-cli/src/app.rs:5866` |
| `/login [provider/custom/agent]`, `/logout [provider]` | Host-only | `runtime/providers`; host saves credentials, then refreshes catalog and affected workspace tools | `crates/loop-cli/src/app.rs:5388` |
| `/mcp login <name>`, `/mcp logout <name>` | Host-only | `runtime/mcp_auth` with workspace id; OAuth browser/callback and tokens stay on host | `crates/loop-cli/src/app.rs:5940` |
| `/tracing [status/enable/on/disable/off/setup [langfuse/otlp]]` | Host-only | `runtime/tracing`; configure Runtime telemetry, not the remote TUI | `crates/loop-cli/src/app.rs:5380` |
| `/ttfb [off/on/duration]` | Session-scoped | `session/ttfb` reads/sets the current Session override; allowed over WebSocket | `crates/loop-cli/src/app.rs:5476` |
| `/ttfb`: explicit save for new Sessions | Host-only | `runtime/defaults` saves the timeout default after confirmation; forbidden over WebSocket | `crates/loop-cli/src/app.rs:5476` |
| `/usage` | Session-scoped + Host-only | Live: `session/stats`; history: host-only `runtime/usage`; Runtime panel: `runtime/sessions` | `crates/loop-cli/src/app.rs:5378` |
| `/theme [name]` | Surface-local | Read/set local theme; no RPC | `crates/loop-cli/src/app.rs:5139` |
| `/copy` | Surface-local | Copy last assistant text to local clipboard | `crates/loop-cli/src/app.rs:5540` |
| `/hotkeys`, `/help` | Surface-local | Render local bindings and commands filtered by capabilities | `crates/loop-cli/src/app.rs:5541` |
| `/changelog` | Surface-local | Fetch release notes for the TUI version | `crates/loop-cli/src/app.rs:5848` |
| `/quit`, `/exit`, `/q` | Surface-local | Leave Surface; connected adapter calls `session/detach`, never `runtime/stop` | `crates/loop-cli/src/app.rs:5123` |

`usage`, `telemetry` and `orchestration` build features still gate their commands.
Skill aliases lose to built-in commands. Skills precede templates with the same
name (`crates/loop-cli/src/commands/mod.rs:663`). Preserve that precedence.
`/theme` with no name currently prints help. This design does not claim it has a
picker. `/review` does not change permission mode.

### 2.2 Keys and other input

The complete configurable action list and defaults are at
`crates/loop-cli/src/keybindings.rs:10` and
`crates/loop-cli/src/keybindings.rs:57`. Custom bindings keep the action's group.
A dash in the wire column means no RPC. Local navigation does not become host
input until the user confirms a command or submits a prompt.

| Binding or action | Group | Wire method or local action | Current source |
|---|---|---|---|
| Enter / `Submit` | Session-scoped | `session/prompt`; slash commands use table above; `!command` uses `session/shell` | `crates/loop-cli/src/app.rs:3224` |
| Escape / `Interrupt` during work | Session-scoped | `session/cancel`; clear only this Surface's unsent queue | `crates/loop-cli/src/app.rs:3184` |
| Double Escape when idle | Session-scoped | Configured tree/fork action: `session/tree`, then navigate/fork | `crates/loop-cli/src/app.rs:3209` |
| Ctrl+L / `ModelSelect` | Session-scoped | `session/models`, `session/set_model` | `crates/loop-cli/src/app.rs:3373` |
| Ctrl+P, Ctrl+Shift+P / `ModelCycleForward`, `ModelCycleBackward` | Session-scoped | `session/models`, `workspace/settings` cycle filter, `session/set_model` | `crates/loop-cli/src/app.rs:3389` |
| Shift+Tab / `ThinkingCycle` | Session-scoped | `session/efforts`, `session/set_effort` | `crates/loop-cli/src/app.rs:3399` |
| `SessionTree`, `SessionFork` (no default keys) | Session-scoped | Same methods as `/tree`, `/fork` | `crates/loop-cli/src/app.rs:3381` |
| `SessionNew`, `SessionResume` (no default keys) | Session-scoped | Proposed `session/new` / list+attach; currently declared but fall through the app handler | `crates/loop-cli/src/app.rs:3474` |
| Alt+Enter / `FollowUp` | Session-scoped | Local queue while busy, later `session/prompt`; no automatic resend after disconnect | `crates/loop-cli/src/app.rs:3439` |
| Approval Enter; Escape outside reason field | Session-scoped | Answer the pending `host/interact`; Escape rejects only when explicitly used as review rejection | `crates/loop-cli/src/app.rs:2553` |
| Question wizard Enter | Session-scoped | Answer pending `host/interact` after final step | `crates/loop-cli/src/app.rs:3108` |
| Model/fork/session/tree picker Enter | Session-scoped | `session/set_model`, `session/fork`, attach, or navigate | `crates/loop-cli/src/app.rs:2661` |
| Skill picker Enter/Space | Workspace-scoped admin | `workspace/skills` toggle | `crates/loop-cli/src/app.rs:2770` |
| MCP picker Enter/Space | Workspace-scoped admin | `workspace/mcp` toggle | `crates/loop-cli/src/app.rs:2946` |
| Provider/tracing setup Enter | Host-only | Complete `runtime/providers` / `runtime/tracing` request; secrets never enter Session history | `crates/loop-cli/src/app.rs:3075` |
| Ctrl+D / `Exit` with empty composer; `/quit`; double Ctrl+C | Surface-local | Detach for connected Runtime; shut down owned embedded Runtime on exit | `crates/loop-cli/src/app.rs:3163` |
| Ctrl+C / `Clear` once | Surface-local | Clear composer/history browse state; does not cancel a turn | `crates/loop-cli/src/app.rs:3167` |
| Shift+Enter, Ctrl+Enter, Ctrl+J / `NewLine` | Surface-local | Insert newline; terminal newline aliases are kept | `crates/loop-cli/src/app.rs:3301` |
| Ctrl+T / `ThinkingToggle` | Surface-local | Hide/show reasoning; does not change effort | `crates/loop-cli/src/app.rs:3409` |
| Ctrl+O / `ToolsExpand` | Surface-local | Expand/collapse tool and reasoning details | `crates/loop-cli/src/app.rs:3419` |
| Ctrl+X / `MessageCopy` | Surface-local | Clipboard copy | `crates/loop-cli/src/app.rs:3429` |
| Alt+Up / `Dequeue` | Surface-local | Remove last unsent queued message | `crates/loop-cli/src/app.rs:3461` |
| Ctrl+G / `ExternalEditor` | Surface-local | Edit composer in local editor | `crates/loop-cli/src/app.rs:3433` |
| Left/Right / `MoveLeft`, `MoveRight` | Surface-local | Move composer cursor | `crates/loop-cli/src/app.rs:3305` |
| Up/Down / `MoveUp`, `MoveDown` | Surface-local | Move by line, then browse local command history | `crates/loop-cli/src/app.rs:3313` |
| Alt+Left/Right, Ctrl+Left/Right / `MoveWordLeft`, `MoveWordRight` | Surface-local | Move by word | `crates/loop-cli/src/app.rs:3329` |
| Home/Ctrl+A, End/Ctrl+E / `MoveLineStart`, `MoveLineEnd` | Surface-local | Move to line boundary | `crates/loop-cli/src/app.rs:3337` |
| Backspace/Ctrl+H, Delete / `DeleteBackward`, `DeleteForward` | Surface-local | Delete character | `crates/loop-cli/src/app.rs:3345` |
| Ctrl+W, Alt+Backspace, Ctrl+Backspace / `DeleteWordBackward` | Surface-local | Delete previous word | `crates/loop-cli/src/app.rs:3353` |
| Alt+D, Alt+Delete / `DeleteWordForward` | Surface-local | Delete next word | `crates/loop-cli/src/app.rs:3357` |
| Ctrl+U, Super+Backspace / `DeleteToLineStart` | Surface-local | Delete to line start | `crates/loop-cli/src/app.rs:3361` |
| Ctrl+K / `DeleteToLineEnd` | Surface-local | Delete to line end | `crates/loop-cli/src/app.rs:3365` |
| Super+Delete, Ctrl+Shift+Backspace, Ctrl+Shift+U / `DeleteLine` | Surface-local | Delete whole line | `crates/loop-cli/src/app.rs:3369` |
| Ctrl+V; image path paste; bracketed/text paste | Surface-local | Attach local image bytes or insert text; bytes go through `session/prompt` on submit | `crates/loop-cli/src/app.rs:2436`, `crates/loop-cli/src/app.rs:3151` |
| `/` autocomplete: Up/Down, Tab, Enter | Surface-local | Select/complete; Enter then dispatches selected command | `crates/loop-cli/src/app.rs:2972` |
| `@file` autocomplete: Up/Down, Tab, Enter | Surface-local | Local file list; insert client path; no implicit host file read | `crates/loop-cli/src/app.rs:3006` |
| Plain characters, Tab fallback, ignored Ctrl/Super characters | Surface-local | Composer input/completion only | `crates/loop-cli/src/app.rs:3478` |
| Picker Up/Down, text/Backspace filter, Escape | Surface-local | Selection/filter/dismissal only; tree picker has no text filter | `crates/loop-cli/src/app.rs:2704`, `crates/loop-cli/src/app.rs:2789`, `crates/loop-cli/src/app.rs:2883` |
| Approval arrows; A/S/R; Tab; reason text/Backspace; Escape in reason field | Surface-local | Choose accept/session/reject or edit reason; Enter sends outcome | `crates/loop-cli/src/app.rs:2564` |
| Setup Up/Down, text editing; Escape/Ctrl+C/Interrupt | Surface-local | Edit/cancel wizard; initial required provider setup may exit | `crates/loop-cli/src/app.rs:3044` |
| Dashboard keys, resize and redraw | Surface-local | Change views locally; read APIs supply data | `crates/loop-cli/src/app/dashboard.rs:88` |

The connection-level detach action takes priority over approval and setup key
handlers. Ctrl+D must not submit a rejection or an empty wizard answer. Closing
the transport removes the recipient; first-answer arbitration and withdrawal
remain in the Runtime. Escape inside a review retains its explicit reject action.

Local `@file` completion describes the client's files. A remote workspace may
have different paths. Label that distinction. Local image attachments send bytes,
not paths for the host to open. Remote file browsing is an open question, not an
implicit unrestricted filesystem RPC. External diff review also needs a payload
or host-local editor policy; a remote TUI must not open a host path locally.

## 3. Wire contract and capabilities

Keep protocol version 1. Add fields with defaults. Missing capability flags mean
false. Keep unknown-method errors at `-32601`. The current capability contract is
`crates/core/loop-protocol/src/capabilities.rs:5`. Existing RPC names are at
`crates/core/loop-protocol/src/rpc.rs:19`.

Advertise transport-wide support at initialize. Attach also returns the actual
Session service flags. ACP Sessions may support fewer operations than native
Sessions. The effective flags are their intersection with transport authority.
The in-process Native adapter must supply the same event and snapshot contract;
it cannot bypass serialization or persistence by calling a harness setter directly.
A flag is feature discovery, not a substitute for server authorization.

Add `ClientCapabilities.branch_reset` to the client's `initialize` request.
It defaults to false when absent. This is the client's promise that it can replace
its transcript on BranchReset. It is distinct from the server's `branch_snapshot`
service flag. Forward BranchReset only to connections that advertised
`client_capabilities.branch_reset = true`.

Use `s` for a full `SessionRef`; use `w` for a Runtime-issued workspace id.
No method accepts a caller-supplied host-local boolean. All calls below are
client-to-server requests. Setters return a committed revision and whether a
change is pending. JSON spellings use snake_case, as the existing wire types do.

| Method | Params | Result | Required capability |
|---|---|---|---|
| `session/models` | `{session:s}` | `{current,models:[{id,name,description,backend}],revision}` | `set_model` (existing flag) |
| `session/set_model` | `{session:s,model,expected_revision?}` | `MutationResult` | `set_model` |
| `session/efforts` | `{session:s}` | `{current,choices:[{id,name}],revision}` | `efforts` |
| `session/set_effort` | `{session:s,effort,expected_revision?}` | `MutationResult` | `efforts` |
| `session/modes` | `{session:s}` | `{current,choices:[{id,name}],revision}` | `modes` (existing flag) |
| `session/set_mode` | `{session:s,mode,expected_revision?}` | `MutationResult` | `modes` |
| `session/config` | `{session:s}` | `SessionSnapshot` | `session_config` |
| `session/compact` | `{session:s,instructions?}` | `{tokens_before?,summary,branch_revision,seq}` | `compaction` |
| `session/stats` | `{session:s}` | `{total_tokens,context_tokens?,context_window?,report,turn_state,usage?,taken_at_ms}` | `session_stats` |
| `session/ttfb` | `{session:s,timeout_ms?,clear_override:false,expected_revision?}` | `{override_ms?,effective_ms,revision,seq?,pending}` | `session_ttfb` |
| `session/shell` | `{session:s,command,timeout_ms?}` | `{stdout,stderr,exit_code,truncated}` | `local_shell` |
| `session/tree` | `{session:s,cursor?,limit?}` | `{entries,fork_points,family,branch_revision,next_cursor?}` | `session_tree` |
| `session/navigate` | `{session:s,target_entry_id?,summarize:false,expected_revision?}` | `{branch_revision,seq}` | `session_tree` |
| `session/view` | `{session:s,view_id,cursor?,limit?}` | `{updates,next_cursor?,branch_revision,at_seq}` | `branch_snapshot` |
| `session/rename` | `{session:s,name,expected_revision?}` | `MutationResult`; emits ConfigChanged metadata, never BranchReset | `session_rename` |
| `session/fork` | `{session:s,before_entry_id,name?,request_id}` | `{session:s,workspace_id,source_branch_revision}` | `session_fork` |
| `session/clone` | `{session:s,name?,request_id}` | Same as fork, with the full active branch | `session_fork` |
| `session/export` | `{session:s}` | `{format_version,markdown,jsonl,branch_revision}` | `session_transfer` |
| `session/import` | `{workspace_id:w,jsonl,request_id}` | `{session:s,workspace_id}` | `session_transfer` |
| `session/sandbox` | `{session:s,action:status/set,config?,expected_revision?}` | `{config,info,revision,seq?}` | `session_sandbox` |
| `session/permission_mode` | `{session:s,mode?,expected_revision?}` | `{mode,effective_policy,revision,seq?,pending}` | `permission_mode` |
| `session/review` | `{session:s}` | `{mode,grants,tool_rules,diff_review}`; no credential values | `session_review` |
| `session/skill` | `{session:s,name,active:true,expected_revision?}` | `MutationResult`; active names in config | `session_skills` |
| `session/workflow` | `{session:s,goal,concurrency?,request_id}` | `{workflow_id}`; progress is streamed | `workflow` |
| `workspace/settings` | `{workspace_id:w}` | `{revision,effective,enabled_models,resource_counts}`; redacted | `workspace_settings` |
| `workspace/mcp` | `{workspace_id:w,action:list/enable/disable/reload,name?,expected_revision?}` | `{revision,servers:[{name,enabled,connected,tools,error?}],pending_sessions}` | `workspace_mcp` |
| `workspace/web_search` | `{workspace_id:w,enabled?,provider?,expected_revision?}` | `{revision,enabled,provider,status,pending_sessions}` | `workspace_web_search` |
| `workspace/skills` | `{workspace_id:w,action:list/enable/disable,name?,expected_revision?}` | `{revision,skills:[{name,description,enabled}],pending_sessions}` | `workspace_skills` |
| `workspace/templates` | `{workspace_id:w,name?,args?}` | `{revision,templates}` or `{revision,text}` for render | `workspace_templates` |
| `workspace/reload` | `{workspace_id:w,expected_revision?}` | `{revision,notices,pending_sessions}` | `workspace_reload` |
| `workspace/trust` | `{workspace_id:w,trusted?,expected_revision?}` | `{revision,trusted,pending_sessions}`; writes host-local only | `workspace_trust` |
| `runtime/providers` | `{action:list/connect/disconnect,provider?,login?}` | `{providers:[{id,name,connected}],notices}`; never returns keys | `host_providers` |
| `runtime/mcp_auth` | `{workspace_id:w,action:login/logout,name}` | `{status,notices}` | `host_mcp_auth` |
| `runtime/tracing` | `{action:status/enable/disable/setup,config?}` | `{enabled,backend,status}`; redacted | `host_tracing` |
| `runtime/defaults` | `{workspace_id:w,patch?,expected_revision?}`; patch may set `response_header_timeout_ms` | `{revision,model?,effort?,mode?,response_header_timeout_ms?}` | `host_defaults` |
| `runtime/usage` | `{filter?,cursor?,limit?}` | `{rows,totals,next_cursor?,taken_at_ms,log_status}` | `host_usage` |
| `runtime/sessions` | `{}` | `{sessions:[{session,workspace_id,state,model,tokens}],totals,taken_at_ms}` | `runtime_sessions` |

`MutationResult = {revision,seq,pending,applies_after_turn?}`. Validate choice ids,
limits and expected revisions before changing state. A revision conflict returns
a typed conflict error with the current revision. Unsupported services return a
typed Unsupported error. Host-only calls over WebSocket return forbidden, even
if invoked manually. A stopped/closing Session refuses new mutations.

Extend existing `session/list` results with optional summary fields: workspace id,
backend, first prompt, message count, selected model, owner kind and availability.
Keep `parent` for Subagents. Add `fork_source` separately. Listing must not acquire
a writer lease. A Session owned by another embedded process cannot be attached
through this daemon; report that owner instead of pretending it is loadable.
Existing load, prompt, transcript, cancel and detach methods stay. Extend
`session/new` params to `{cwd?,name?,backend?,model?,effort?}` with result
`SessionRef`. The existing `multi_session` flag covers creation; new
`backend_selection` gates the optional backend selector. The host resolves cwd
and returns its workspace id in the attach snapshot. Default to the host's
startup cwd if none is supplied. A local Unix client can offer its own cwd;
a WebSocket client must choose a host workspace, not send its local cwd by default.
`session/set_model` changes a model within the Session's backend. A choice that
switches native/ACP backend creates or loads the target backend's Session and
attaches to that reference. It must not replace a live Session's routing identity.
Native/Remote service accessors and advertised flags must agree. The new
`attach_snapshot` flag gates the extended attach result. `branch_snapshot` gates
immutable views; tree mutation and compaction support require it on S3-capable
Surfaces. The S3 TUI requires both server flags before offering branch-changing
controls, and advertises its own `branch_reset` support at initialize. Keep legacy
attach replay working. Do not rely on a legacy watcher ignoring BranchReset:
that would leave its transcript stale. Apply the detach rule in section 3.1.

Bound pages to 100 items by default and 1,000 maximum. Bound a Session transfer
to 8 MiB of UTF-8 JSONL and the whole encoded response to 16 MiB. Bound shell
output to 1 MiB per stream, with truncation marked. Enforce limits before JSON
allocation where possible. These are proposed S3 limits. The existing transport
ceiling is 64 MiB (`crates/core/loop-wire/src/transport.rs:13`). Reject an oversized
transfer explicitly; never silently truncate Session context. Tune limits only
with matching schema and boundary tests.

### 3.1 State updates and attach snapshot

Add typed `SessionUpdate` variants. Current variants are at
`crates/core/loop-protocol/src/update.rs:77`.

| Update | Payload and effect |
|---|---|
| `ConfigChanged` / `config_changed` | `{revision,metadata:{name},config,effective,pending,applies_after_turn?,workspace_revision}`. Carries Session metadata and the complete public config, including model, effort, backend mode, permission mode, sandbox, response-timeout override and active skills. Replaces the previous metadata/config view. Rename changes `metadata.name`, not the branch. |
| `BranchReset` / `branch_reset` | `{branch_revision,view_id,reason}`. Replaces the chat projection using the immutable paged view. Reasons include navigate and compact. Never append the replacement transcript to the old one. |
| `WorkflowProgress` / `workflow_progress` | `{workflow_id,state,task_id?,description?,outline?,mermaid?,output?,error?}`. States cover planned, task_started, task_completed, task_failed and ended. Shares Session ordering and replay. |

All use the existing durable `UpdateEnvelope`. They consume contiguous
`session_seq` values. They are committed before broadcast or RPC success.
Do not bypass the pump with a direct Surface notification.

When a Session's branch resets, detach every attached connection that did not
advertise `branch_reset` at initialize. Deliver a visible detach reason:
“this Session changed branch; attach again”. Stop that subscription before any
update from the replacement branch reaches it. Never send BranchReset to that
connection or leave it attached with a stale transcript. Detach only the affected
Session; preserve other Session subscriptions on the connection. Remove its
Escalation recipient without answering an approval. Reset-capable watchers stay
attached and receive the ordered BranchReset. Apply the same rule when replay
encounters a reset; do not silently skip it for a legacy client. Reattach starts
a fresh transcript view at the current branch. Keep sequence cursors truthful.

Rename uses ConfigChanged's `metadata.name` field. It leaves `branch_revision`
and the transcript view unchanged. It does not detach legacy watchers. This
**deliberately deviates from the revised plan text**, which groups
`session/tree|navigate|rename` under BranchReset. Rename changes metadata, not
the active branch.

Extend `session/attach` with optional `snapshot` in its result and advertise
`attach_snapshot`. Preserve `{latest_seq,generation}` for old clients.
`SessionSnapshot` contains `{session,workspace_id,at_seq,revision,branch_revision,
view_id,metadata,config,effective,pending,capabilities,turn_state,stats,workspace_revision}`.
The snapshot excludes secrets. Stats may contain unknown values for ACP.
Pending Escalation ids are display state only; actionable requests still arrive
through `host/interact` after recipient registration. Never answer an id recovered
from historical replay.

Subscribe before capture. Under the Session lifecycle lock, after the update pump has passed a barrier,
capture a committed
high-water mark H, the config at H and an immutable transcript view at H. Reply
with the snapshot, replay cursor+1 through H, then send buffered live updates
above H. Deduplicate overlap by sequence. The snapshot's `at_seq` equals H and
`latest_seq`. It is not the client's processed cursor.

A fresh TUI builds its chat from the snapshot's paged view. It processes replay
for cursor/audit purposes without appending the same historical chat again.
A resumed view can reduce replay, then reconcile to the snapshot at H. It must
not let a replayed old config overwrite the snapshot's newer config. Buffer live
updates while view pages load, then apply updates above H in order. BranchReset
uses the same bounded view mechanism. Keep referenced views valid during paging;
expire abandoned views explicitly. Never fall back to a moving transcript query.
Snapshot, page-load failure and reconnect parity need dedicated tests.

### 3.2 Mutation serialization

Use the existing per-Session lifecycle lock
(`crates/loop-runtime/src/runtime.rs:179`) for admission and revision changes.
Do not hold it while waiting for a model, subprocess, user answer or network RPC.
Reserve the operation under the lock, then run it with cancellation and a tracked
busy state. Close/unload must wait for settlement or cancel it safely.

| Operation | While a turn or exclusive operation runs |
|---|---|
| Model selection, sandbox replacement, navigate, compact, skill activation, workflow start, shell | Refuse Busy. No silent cancellation. |
| Effort, backend mode, permission mode and response-timeout override | Commit desired state as pending for the next turn. Current turn keeps its captured state. Recheck Policy ceilings and Locks. |
| Rename | Serialize the metadata write and emit ConfigChanged with `metadata.name`; allowed without changing current execution or branch revision. |
| Tree/export/fork/clone | Read a committed branch snapshot. Do not include uncommitted turn output. Do not interrupt the source turn. |
| Stats/config/list | Read consistent public state without changing ownership. |
| Cancel/approval answer | Remain responsive; cancellation must not wait on a long operation holding the lifecycle lock. |

At turn end, apply pending config before admitting another prompt. Emit another
ConfigChanged when effective state changes. Latest serialized setter wins unless
its expected revision conflicts. The response names when a pending change takes
effect. The current turn must not observe a half-changed mode. Explicit Policy
revocation may still cancel work; S3 does not implement a new Policy engine.

Workspace administration has a separate revision lock. Rebuild definitions and
connections once. Update idle Sessions immediately. Busy Sessions record the new
workspace revision and refresh before their next turn. Return pending Session ids.
Do not hold the workspace lock while acquiring Session locks or waiting for I/O.
Trust revocation immediately blocks new project execution and cancels affected
work before replacing executable resources. Do not apply a revoked configuration
only after another project hook has run.

Tree state and Event Log use separate databases. Use mutation ids and recovery
markers for state-changing operations. Write tree state, then commit its ordered
update, then expose success. A log failure stops that Session's new work. On
reopen, reconcile an unannounced committed mutation before capture or prompt.
Do not claim an atomic cross-database transaction. Tests must inject failure
between these steps and prove that two watchers converge.

## 4. Session state, defaults and workspace reload

A Session's selected model, effort and supported mode belong to that Session.
Write explicit selections to its tree, even without a prompt. Restore them on
load, navigate and restart. An absent effort entry means inherit the default;
an explicit `off` means off. The current non-Off restore check at
`crates/loop-app-core/src/session_factory.rs:447` must change.

New-Session defaults are separate saved host settings. A Session setter never
silently changes another Session or the new-Session default. The TUI offers a
separate explicit “save for new Sessions” action through `runtime/defaults`.
That action is host-local. Existing commands currently save defaults
(`crates/loop-app-core/src/runtime.rs:255`, `crates/loop-app-core/src/runtime.rs:947`);
characterization tests and release notes must call out this deliberate UX change.
`/ttfb` reads or sets the current Session response-timeout override through
`session/ttfb`. This is Session-scoped and allowed over authenticated WebSocket
when `session_ttfb` is supported. Without `timeout_ms` or a clear request, read
state. A value of
zero means unlimited. `clear_override = true` removes the override and inherits
the default; reject a request that also supplies `timeout_ms`. A change during a
turn applies to the next turn and emits ConfigChanged. It never saves a default.
Only an explicit host-local `runtime/defaults` write saves
`response_header_timeout_ms` for new Sessions. Refuse that write over WebSocket,
without preventing the Session read or override change.

Keep effective permission Policy separate from the backend's mode id. Native
`session/set_mode` and `session/permission_mode` must share one transition path
where they address the same setting. ACP modes can mean different things. Neither
method may bypass a Policy ceiling or a Lock. Approval grants belong to the
Session. Fork/import do not copy active approvals or grants as new authority.

Workspace reload rereads settings, models, skills, templates, hooks and context.
It invalidates the canonical-cwd cache and rebuilds affected tool lists/system
prompts. Host provider login/logout refreshes the shared catalog and credentials,
then refreshes all affected workspaces. A theme reload stays local. MCP OAuth
credentials stay on the host. Credentials are not sent in model/provider lists,
usage rows, errors, snapshots or logs.

Trust is persisted on the host. A same-host TUI can confirm a trust change through
the protected socket. A WebSocket Surface can read trust status but cannot grant
trust. Startup prompts happen before project definitions or commands load.
Current startup resolves trust at `crates/loop-app-core/src/runtime.rs:1379`;
Runtime workspace loading is noninteractive at
`crates/loop-app-core/src/session_factory.rs:113`. The embedded adapter must join
these two steps without a second stdin reader or a daemon prompt.

## 5. Fork, clone and payload transfer

Fork and clone read a committed source snapshot. Create the destination tree
atomically, acquire its own lease, then admit its backend and pump. Return a new
SessionRef. Failure rolls back or removes an unadmitted destination and releases
its reservation. A capacity or lease error leaves the source untouched.
A request id makes destination creation idempotent after a lost response; do not
create duplicate Sessions when a client checks the same operation again.

The initiating Surface attaches to the destination, then detaches from the source.
Other watchers stay on the source. A fork before a user message returns that
message to the initiating Surface's draft. Clone copies the whole active branch.
Neither operation calls the current mutating harness helper unchanged
(`crates/loop-agent/src/harness/agent_harness.rs:413`).

Export returns Markdown and versioned JSONL from a consistent branch snapshot.
The Surface chooses paths and enforces overwrite rules on its own machine.
Import reads the Surface's local file and sends bounded JSONL. The host validates
the format and creates a separately leased destination in the chosen workspace.
Ignore imported host paths and executable grants as authority. Validate parent
links and ids. Never follow a path embedded in imported data to read host files.
Current host file I/O at `crates/loop-app-core/src/runtime.rs:391` and
`crates/loop-app-core/src/runtime.rs:404` is not the remote implementation.

## 6. Embedded ownership and recovery

Add an explicit Runtime ownership mode. Daemon mode keeps `owner = "runtime"`
and its instance/database locks. Embedded mode uses `owner = "embedded"`, the
current pid and a fresh fenced generation. It takes neither Runtime instance lock
nor the daemon's Session-database lock. Current hardcoded owner/generation setup
is at `crates/loop-runtime/src/runtime.rs:243`.

Every embedded Session still acquires and renews its own writer lease. Fence all
writes/releases by owner, pid and generation. Use the same 5 s renewal and 30 s
staleness rules. The existing takeover guard only allows lock-based immediate
reclaim from runtime to runtime
(`crates/loop-agent/src/harness/session/sqlite/leases.rs:238`). Preserve that guard.
An embedded owner is never reclaimed under a daemon lock. Normal exit cancels
and settles owned work, drains updates and releases its own leases. Ctrl+D on a
connected Surface only detaches; Ctrl+D on an embedded Surface also ends the
Runtime that process owns.

A dead embedded owner can be swept or taken over only when its heartbeat is
stale AND its pid is proven dead. Unknown pid liveness is not proof. A daemon
restart must preserve live embedded owners. Two embedded TUIs and a daemon must
run concurrently, with different Sessions, against the same configured stores.
Opening a Session with another live owner returns an actionable ownership error.

Owner labels alone are insufficient for Event Log recovery. Current `recover`
scans earlier generations and marks a log-wide recovery watermark
(`crates/loop-runtime/src/runtime.rs:332`, `crates/loop-runtime/src/runtime.rs:377`).
Before default cutover, record generation ownership and scope both recovery scans
and watermarks to proven-dead owners or a successfully acquired orphan Session.
Never end a turn or withdraw an Escalation from a live embedded generation merely
because a newer daemon generation starts. Do not let a newer recovery watermark
hide an older embedded generation that dies later. Recovery must be idempotent.

Use the existing shared Event Log path with unique transactional generations.
An owner mode must reach generation allocation, lease identity and recovery,
not just the lease row's label. On dead-owner recovery, record interrupted work as
uncertain and withdraw old Escalations. Never retry a prompt or reuse a historical
approval. Session admission occurs only after that Session's recovery commits.

Embedded mode may own multiple Sessions after `/new`, fork or resume. It is not a
hard one-Session limit. It also must leave capacity for required Subagent children.
Exit closes all work owned by that embedded process. S2's child cancellation,
Policy inheritance and lease cleanup rules still apply.

## 7. WebSocket security and sandbox boundary

S3.8 listens on loopback only. Require a random nonempty bearer token even there.
Reject missing or wrong tokens with 401 using the existing constant-time check.
Keep the token in a user-owned 0700 run directory and a regular 0600 file. Reject
symlinks and unsafe ownership on creation/read. Do not print the token in status,
URLs, QR codes or diagnostics. Read it from `LOOP_RUNTIME_TOKEN` or `--token-file`.
Do not pass it to agent subprocesses.

Refuse every non-loopback bind until S3.8b supplies `wss://` with certificate
validation. A token alone does not permit plaintext remote exposure. In S3 use an
SSH tunnel to the host's loopback listener. Host-only checks still treat the
WebSocket endpoint as remote, including traffic arriving through a tunnel.
Do not reuse HTTP MCP's weaker loopback authentication policy. Fix its `==` token
comparison at `crates/loop-cli/src/mcp_serve.rs:30` in the listener slice.

The connection carries authenticated transport authority into dispatch. Reject
host-only methods before invoking services. Protect `runtime/stop` as host-only
as well. Apply message/page/output bounds and handshake deadlines. Disconnect a
lagging client visibly so it can replay; do not allocate unlimited queued output.
Untrusted input never becomes a host file path or a claimed transport identity.

SB1a supplies one platform sandbox builder before S3.4. It adds `network` and
`allowWrite` settings. Linux defaults remain Full/runc. macOS defaults select
Seatbelt/partial. Distinguish inherited defaults from explicitly supplied Linux
flags. Current settings only contain mode/isolation/runtime
(`crates/loop-app-core/src/config/settings.rs:50`).

SB1b wraps shell commands in Seatbelt and confines built-in filesystem tools.
`ExecutionEnv` includes both interfaces
(`crates/loop-agent/src/harness/types.rs:222`). A `JailedHostFs` must reject escapes,
symlinks into denied areas and unsafe creation paths. Canonicalization alone is
not a defense against a concurrent symlink replacement. Search currently uses a
host walker (`crates/loop-agent/src/harness/tools/search.rs:337`); changing only
`absolute_path` is insufficient. Every walked/read path must obey the jail, or
search must run in a confined helper.

Deny secrets, the resolved agent directory's auth file, run directory, token and
control sockets. Resolve agent-directory overrides. Clean provider/token values
from child environments. Keep denied paths denied even under a broad allowWrite
root. Shell network is off by default. Workspace web/MCP services are separate
host capabilities and must not be described as confined by a shell-only profile.
Validate each execution lane before claiming whole-agent network isolation.

The macOS lane must prove read/write/edit/search confinement, token protection,
control-socket denial, symlink safety, process-tree cleanup, git commit and cargo
build with required cache paths explicitly allowed. Its own design records the
Seatbelt choice and later Apple container work.

GLM metadata work lands before S3.1. Preserve pinned reasoning metadata through
login, restart and catalog refresh. Model support and request format are separate:
`CustomModelSpec` has compatibility overrides
(`crates/loop-ai/src/providers/custom.rs:26`), and OpenAI/Zai encode thinking
differently (`crates/loop-ai/src/api/openai_completions.rs:829`). Test actual on/off
payloads and returned thinking, not just picker choices. This document does not
claim a reasoning flag proves every endpoint supports every effort level.

## 8. Connected usage

Keep today's dashboard layout and add a Runtime panel. Current dashboard opening
reads a local usage log (`crates/loop-cli/src/app/dashboard.rs:65`), and live status
reads the current application Runtime (`crates/loop-cli/src/app/dashboard.rs:174`).
Replace those data dependencies through SurfaceClient.

| Panel | Scope and source | Connected behavior |
|---|---|---|
| Live / Now and token bar | Attached Session, from `session/stats`, config and ordered updates | Never add another Session's context tokens to this bar. Show unknown ACP values as unknown. |
| History | Host's complete usage log, through paged `runtime/usage` | Includes all logged Sessions on that host. Unix socket/embedded only. WebSocket shows “run this on the Runtime host”; never substitutes the client's local history. |
| Runtime (new) | Loaded Sessions, through `runtime/sessions` | One row per loaded Session: reference, workspace, idle/busy/parked state, model and tokens so far. Show totals for these rows only. |

History is billed usage, not the sum of live context sizes. Runtime totals count
each loaded Session once, including children as separate rows. Mark parked when
work waits for an Escalation. Include a capture time and unknown counts. Do not
mix snapshots from different hosts. Selecting a row attaches through the normal
Session flow; it does not transfer a lease from another process. In embedded mode
the Runtime panel covers that process's Sessions, not every independent TUI.

## 9. Slices and dependencies

S3.0 is documentation only. Approval is required before spine implementation.
Each implementation slice starts with characterization or regression tests.
Do not switch the default early to simplify an intermediate slice.

| Slice | Deliverable | Prerequisites |
|---|---|---|
| S3.1 | Native ModelControl, model/effort/mode wire methods, capabilities, ConfigChanged, state/default split and explicit-Off restore | Approved S3.0; GLM lane merged first |
| S3.2 | Compact/stats/shell and Session timeout routing, service flags, bounded output and cancellation | S3.1; coordinates BranchReset/view types with S3.3 |
| S3.3 | Tree/view/navigate, metadata-only rename, snapshot contract, reset capability and legacy detach, leased fork/clone, payload export/import, list summaries | S3.1; shared event/snapshot types land once before parallel consumers |
| S3.4 | Workspace admin, host-only methods, reload/trust, usage history and Runtime panel data | S3.1; SB1a platform builder; snapshot/type agreement with S3.3 |
| S3.5 | SurfaceClient over current NativeBackend; app and dashboard take only facade/view types; skills/templates/workflow API and progress | S3.2–S3.4; SB1a base for sandbox migration |
| S3.6 | Unix-socket `--connect`, Session choice, detach priority, replay/snapshot rendering and capability parity | S3.5 |
| S3.7 | ACP admission/startup inside Runtime, selection, model/mode/effort controls, host file/terminal authority | S3.6 and S3.1; required before default cutover |
| S3.8 | Loopback WebSocket listener, token files, transport authority and dispatch enforcement | S3.6; can run beside S3.7 |
| S3.9 | Embedded owner mode, scoped recovery, trust startup and default `loop` cutover | S3.7, S3.6 and S3.4; concurrent-owner/recovery checks pass; coordinate S3.8 security checks |
| S3.10 | User docs, protocol docs and manual live check across native/ACP, embedded/socket/WebSocket | S3.1–S3.9 plus sandbox/GLM lanes in release scope |

S3.8b is a later TLS/URL/QR slice. It is not a dependency for embedded startup.
S3.5 merges one group at a time: model/effort; Session operations; sandbox/MCP/
permissions/trust; skills/templates/workflows; dashboards; login/tracing. Existing
native and ACP paths remain usable during those merges. Characterize each first.

SB0 owns Linux-only error text in podman and the relevant docs. SB1a owns builder
extraction and settings, including the old app sandbox call site. It lands before
S3.4 and before S3.5 edits that group. SB1b owns Seatbelt and filesystem confinement
and runs beside S3.5 after rebasing on SB1a. GLM owns custom-provider metadata and
login setup; S3.1 rebases on it. Separate worktrees do not remove file dependencies.

Tests must cover schema/goldens, missing capabilities, old v1 clients, native and
Remote parity, two watchers, concurrent setters, pending changes, explicit off,
branch reset during replay, bounded snapshot pages, fork failure/rollback, payload
limits, forbidden host methods, token files and lost connections. Never retry a
prompt after an uncertain outcome. Exercise live embedded/daemon coexistence and
SIGKILL recovery, not only clean exits. Async tests use bounded timeouts and wait
for outcomes. Implementation gates follow the repo's required Cargo checks.
Required regression scenarios for these contracts:

- Attach a reset-capable TUI and a legacy client to one Session. Omit the legacy
  client's `branch_reset` field at initialize. Navigate the branch, then repeat
  with compaction. Only the capable client receives BranchReset and replaces its
  transcript. The legacy client renders “this Session changed branch; attach
  again”, is detached before replacement-branch updates, and can attach again to
  the current branch. Other Session subscriptions and approval arbitration remain
  valid. Repeat with explicit false and a reset encountered during cursor replay.
- Over an authenticated WebSocket, read and set `session/ttfb`, including explicit
  zero and clearing the override. Check current/next-turn behavior. Verify that
  neither operation changes new-Session defaults. A `runtime/defaults` timeout
  write is forbidden there and succeeds only with host-local authority.
- Rename with two watchers, including a legacy watcher. Both observe the metadata
  change. Assert ConfigChanged contains `metadata.name`; branch revision and
  transcript view are unchanged. Assert no BranchReset or detach occurs. Check
  the name in a fresh attach snapshot and after restart.

No Cargo checks are required for this documentation-only slice.

## 10. Open questions for approval

Answered 2026-11-03:

- After a model or effort change, the TUI shows a one-line hint to also save the
  value as the default for new Sessions. The Session change always applies.
- Host usage history stays hidden over WebSocket. Remote Surfaces see the live
  Session panel and the Runtime panel only.
- The remaining questions take the defaults proposed in this document.

Original questions:


- Should the TUI offer “save for new Sessions” after each model/effort change,
  or expose it in a settings action? Session setters and default writes remain
  separate in either case.
- Confirm the proposed transfer, shell-output and page limits. Large exports
  need an explicit chunked transfer design if the fixed limits prove too small.
- Should remote users browse host workspace files through a future confined API?
  S3 keeps local path completion explicit and does not add unrestricted file RPCs.
- Confirm remote diff-review UX: inline bounded diff payload or host-only editor.
  Do not treat a host path as a usable client path.
- Which ACP agents can persist mode/effort and provide accurate stats? Advertise
  only supported per-Session services. Do not fabricate restored backend state.
- Confirm the persisted mutation/recovery marker representation across the two
  databases and the per-owner recovery watermark schema before S3.9. Safety rules
  in sections 3 and 6 are required regardless of that representation.
- Confirm live-mode changes that must cancel work for a locked Policy revocation.
  Ordinary requested effort/mode changes take effect next turn.
- The revision plan adds host usage history to a host-only group. This design
  therefore disables that history over WebSocket. Confirm that boundary before
  later TLS sharing introduces any narrower read-only authority.

## Decision record

Approved by the maintainer on 2026-11-03. The TUI will use one SurfaceClient code
path. The facade first adapts the current NativeBackend and then RemoteBackend.
An embedded Runtime becomes the default only after ACP, host trust and embedded
ownership/recovery pass their gates. This sequence keeps existing subscription
and standalone users working while the client boundary is built.

We considered switching every TUI to Runtime first. That would make API migration
look simpler, but current Runtime ownership and ACP startup do not preserve those
users' behavior. Keeping two permanent TUI implementations would avoid the early
cutover risk but duplicate command and security behavior. The staged facade keeps
one UI path while making each backend transition reviewable.

The Runtime owns Session execution and workspace definitions. Credentials and
host telemetry stay host-only. The Surface owns its display, local files and
composer. Forks get new leases. Embedded owners have no daemon lock and no
lock-based recovery privilege. WebSockets require tokens on loopback and cannot
bind remotely before TLS. These boundaries are part of the proposed decision,
not incidental details for each implementation slice to redefine.
