# Configuration

Global configuration is in `~/.loop/agent/`. Run `loop config` to print the paths.

| Path | Content |
|---|---|
| `settings.json` | Settings (see below) |
| `auth.json` | API keys (file mode `0600`) |
| `models.json` | Extra OpenAI-compatible providers, written by hand |
| `sessions.db` | Saved sessions (SQLite) |
| `skills/`, `prompts/`, `hooks/`, `extensions/`, `themes/` | Skills (`SKILL.md`), prompt templates, [hooks](hooks.md), extensions, themes |

In `models.json`, custom provider `models` can be string ids (listed with metadata from `/models`) or objects that pin details the listing omits. Object entries require `id`; `name`, `contextWindow`, `maxTokens`, and `reasoning` are optional. The context and token fields also accept snake_case names.

```json
{
  "providers": [{
    "id": "local",
    "baseUrl": "http://localhost:8000/v1",
    "models": [{ "id": "my-model", "name": "My model", "contextWindow": 131072, "maxTokens": 8192, "reasoning": true }]
  }]
}
```

Custom providers added with `/login custom` can also save per-model thinking metadata in the global `settings.json`:

```json
{
  "providers": [{
    "id": "my-gateway",
    "name": "My Gateway",
    "baseUrl": "https://gateway.example/v1",
    "models": [
      { "id": "glm-5.3", "reasoning": true, "thinkingFormat": "zai" },
      { "id": "another-model", "reasoning": true, "thinkingFormat": "openai" }
    ]
  }]
}
```

`models` is optional, so existing provider settings still work. Each entry pins an exact model id and its capabilities. `reasoning` defaults to `false`; `thinkingFormat` is optional and otherwise uses automatic compatibility detection. `openai` sends `reasoning_effort` for each enabled effort level and omits it for off. `zai` sends `thinking: {"type": "enabled"}` for every enabled level and `thinking: {"type": "disabled"}` for off. Zai does not distinguish the enabled effort levels.

After the name, URL and key, `/login custom` asks which model ids support thinking and which format they use (default `openai`). Enter comma-separated ids, or leave the answer blank to keep saved metadata on reconnect and use server defaults on first login. Enter `none` to clear saved metadata. For mixed capabilities or formats, edit individual entries in settings. Reconnect, restart and catalog refresh retain the metadata.

Precedence for a matching model id, from highest to lowest: a `models.json` entry, a saved provider `models` entry, server-published metadata (including `supported_parameters`), then built-in defaults. Entries are pinned as complete model specifications; unspecified context and output limits use the custom-provider defaults (128,000 and 16,384 tokens). Other models remain discoverable from `/models`. `models.json` overrides also accept `thinkingFormat` (or `thinking_format`) alongside the existing model fields; its provider URL and key environment settings take precedence when that provider id appears in both files. These overrides apply consistently during login and at startup.

A project can add `.loop/settings.json` in its root. Project values override global values. Arrays replace, they do not merge.

Common settings in `settings.json`:

| Key | Default | Purpose |
|---|---|---|
| `defaultProvider`, `defaultModel` | none | Provider and model at startup |
| `permissionMode` | `ask` | `read-only`, `ask`, `accept-edits`, `full-access` or `plan`; see [permissions](permissions.md). A project may only choose a stricter mode |
| `toolPermissions` | `write`, `edit`, `bash`: `ask`; `read`, `grep`, `glob`, `ls`, `web_fetch`, `web_search`: `allow` | Per-tool rules, `allow`, `ask` or `deny`, with optional patterns such as `bash(git status*)`. `deny` applies in every mode; with nobody to ask, `ask` refuses the call. A project may only add `ask`/`deny` rules |
| `bash.defaultTimeoutMs`, `bash.maxTimeoutMs` | `120000`, `600000` | `bash` time limits; a call may ask for up to the maximum (global only) |
| `fileEditReview` | `newSession` | Kept for old settings files: with no `permissionMode`, `never` starts in `accept-edits`. Other values change nothing |
| `sandbox.mode` | `off` | `off` or `local`. Podman on Linux, Seatbelt partial isolation on macOS. A project may enable the sandbox, never turn it off |
| `sandbox.isolation` | platform default | `full` or `partial`. Unset means `full` on Linux and `partial` on macOS. `full` on macOS is an error. Any other value is an error when `mode` is `local`. A project value overrides the global one only when the project sets it |
| `sandbox.runtime` | platform default | `runc`, `crun`, `runsc` or `krun`. Unset means `runc` on Linux. These are Linux container runtimes: setting one on macOS is an error. Any other value is an error when `mode` is `local`. A project value overrides the global one only when the project sets it. Older versions saved `"isolation": "full", "runtime": "runc"` into every settings file; exactly that pair is read as unset |
| `sandbox.network` | `false` | Allow network access from sandboxed commands. Enforced for macOS shell commands; the Linux sandbox does not enforce it yet. Unix sockets remain denied on macOS. A project may turn it off but not on |
| `sandbox.allowWrite` | empty | Extra paths macOS sandboxed commands and file tools may write, such as `~/.cargo`. Secret paths remain denied under broad grants. The Linux sandbox does not enforce this setting yet (global only) |
| `sandbox.denyHomeReads` | `false` | macOS: deny reading files under the home directory, except the project, `allowWrite`, `allowRead` and git/Rust toolchain settings. Credential files stay denied either way. A project may turn it on but not off |
| `sandbox.allowRead` | empty | macOS, with `denyHomeReads`: paths under the home directory sandboxed commands and file tools may still read, such as `~/.npm` (global only) |
| `compaction` | enabled | `reserveTokens`, `keepRecentTokens` for `/compact` |
| `skills`, `disabledSkills` | empty | Extra skill paths, and skills to hide |
| `mcpServers`, `disabledMcpServers` | empty | MCP servers (stdio `command` or remote `url`) |
| `providers` | empty | Custom providers added with `/login custom` |
| `runtime.maxSessions`, `runtime.maxActiveTurns` | `16`, `8` | Limits of `loop runtime start`; see [the Runtime](runtime.md#start-the-runtime). Each must be a whole number of at least 1. A command-line flag overrides it (global only) |
| `runtime.idleShutdownMinutes` | `10` | Minutes an auto-started Runtime waits with nothing attached or running before it exits. `0` disables it. An owner from `loop runtime start` never idles out (global only) |
| `runtime.forwardEnv` | empty | Extra environment variable names a Surface forwards to a Runtime over WebSocket, on top of the built-in allowlist. Unix sockets forward the full environment. Values are never logged (global only) |

On macOS, `/sandbox local` starts Seatbelt after a preflight. Shell commands may
write the real project, private sandbox temp directory and `sandbox.allowWrite`; file tools
may write the project and `allowWrite`. Reads are allowed except for credential
paths, Keychains, and the resolved agent directory's `auth.json` and `run/`.
Project `.loop`, the entire agent directory and Git hooks, config and pointers
in every repository (submodules and nested ones included) are write-protected,
and commands cannot create or replace a `.git` entry: run `git clone` and `git
init` outside the sandbox. Git worktrees and submodule checkouts work;
a project whose `.git` is a symlink or any other outside pointer cannot start
the sandbox. OAuth/ACP credentials, databases, history, logs and common home
credential files are also unreadable. Provider keys and Runtime tokens are
removed from child environments. Child TMPDIR is a private Loop-created 0700
directory; environment overrides cannot replace it. Network
settings apply to shell subprocesses; host web/MCP services remain separate
capabilities. See [the macOS sandbox decision](design/macos-sandbox.md).

Loop supports Linux and macOS only. If sandbox preflight fails, standalone Loop leaves
the sandbox off with a warning, while a Runtime started with `loop runtime start`
refuses to admit the Session with `required workspace sandbox failed`. Runtime
Sessions require the configured sandbox to start successfully.

Environment variables:

| Variable | Purpose |
|---|---|
| `SOKET_API_KEY`, `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` | Provider API keys |
| `GEMINI_API_KEY`, `GOOGLE_API_KEY` | Gemini API key (the first one set wins) |
| `LOOP_CODING_AGENT_DIR` | Use a different config directory |
| `LOOP_CODING_AGENT_SESSION_DIR` | Use a different directory for `sessions.db` |
| `LOOP_DEBUG=1` | Write debug logs (same as `--debug`) |
