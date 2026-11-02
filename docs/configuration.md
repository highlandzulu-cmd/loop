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

A project can add `.loop/settings.json` in its root. Project values override global values. Arrays replace, they do not merge.

Common settings in `settings.json`:

| Key | Default | Purpose |
|---|---|---|
| `defaultProvider`, `defaultModel` | none | Provider and model at startup |
| `permissionMode` | `ask` | `read-only`, `ask`, `accept-edits`, `full-access` or `plan`; see [permissions](permissions.md). A project may only choose a stricter mode |
| `toolPermissions` | `write`, `edit`, `bash`: `ask`; `read`, `grep`, `glob`, `ls`, `web_fetch`, `web_search`: `allow` | Per-tool rules, `allow`, `ask` or `deny`, with optional patterns such as `bash(git status*)`. `deny` applies in every mode; with nobody to ask, `ask` refuses the call. A project may only add `ask`/`deny` rules |
| `bash.defaultTimeoutMs`, `bash.maxTimeoutMs` | `120000`, `600000` | `bash` time limits; a call may ask for up to the maximum (global only) |
| `fileEditReview` | `newSession` | Kept for old settings files: with no `permissionMode`, `never` starts in `accept-edits`. Other values change nothing |
| `sandbox.mode` | `off` | `off` or `local`. Linux only (macOS support is planned): the local sandbox runs on Linux (Podman) |
| `sandbox.isolation` | platform default | `full` or `partial`. Unset means `full` on Linux and `partial` on macOS. `full` on macOS is an error. Any other value is an error when `mode` is `local`. A project value overrides the global one only when the project sets it |
| `sandbox.runtime` | platform default | `runc`, `crun`, `runsc` or `krun`. Unset means `runc` on Linux. These are Linux container runtimes: setting one on macOS is an error. Any other value is an error when `mode` is `local`. A project value overrides the global one only when the project sets it. Older versions saved `"isolation": "full", "runtime": "runc"` into every settings file; exactly that pair is read as unset |
| `sandbox.network` | `false` | Allow network access from sandboxed commands. Stored for the macOS sandbox; the Linux sandbox does not enforce it yet. A project may turn it off but not on |
| `sandbox.allowWrite` | empty | Extra paths sandboxed commands may write, such as `~/.cargo`. Stored for the macOS sandbox; the Linux sandbox does not enforce it yet (global only) |
| `compaction` | enabled | `reserveTokens`, `keepRecentTokens` for `/compact` |
| `skills`, `disabledSkills` | empty | Extra skill paths, and skills to hide |
| `mcpServers`, `disabledMcpServers` | empty | MCP servers (stdio `command` or remote `url`) |
| `providers` | empty | Custom providers added with `/login custom` |
| `runtime.maxSessions`, `runtime.maxActiveTurns` | `16`, `8` | Limits of `loop runtime start`; see [the Runtime](runtime.md#start-the-runtime). Each must be a whole number of at least 1. A command-line flag overrides it (global only) |

On macOS, use `--permission-mode read-only` or `--permission-mode ask` to control tool permissions. On platforms other than Linux, setting `sandbox.mode` to `local` has different startup outcomes: standalone Loop leaves the sandbox off with a warning, while a Runtime started with `loop runtime start` refuses to admit the Session with `required workspace sandbox failed`. Runtime Sessions require the configured sandbox to start successfully.

Environment variables:

| Variable | Purpose |
|---|---|
| `SOKET_API_KEY`, `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` | Provider API keys |
| `GEMINI_API_KEY`, `GOOGLE_API_KEY` | Gemini API key (the first one set wins) |
| `LOOP_CODING_AGENT_DIR` | Use a different config directory |
| `LOOP_CODING_AGENT_SESSION_DIR` | Use a different directory for `sessions.db` |
| `LOOP_DEBUG=1` | Write debug logs (same as `--debug`) |
