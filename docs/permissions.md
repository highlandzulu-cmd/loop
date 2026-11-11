# Permissions

Every tool call Loop makes passes one check, whether it comes from the chat, `--print`, `mcp serve`, a `/workflow` task, or a subscription agent's permission request. The check runs in this order:

1. **Deny rules** (`toolPermissions` entries set to `deny`) and the `read-only` mode refuse the call.
2. **Hooks** (`tool_call`) may block the call or allow it without asking. See [hooks](hooks.md).
3. **The permission mode** allows the call or asks.
4. **Asking**: the TUI and desktop app show a prompt. With nobody to ask (`--print`, `mcp serve`), the call is refused, and the model is told which flag or rule would allow it.

## Modes

| Mode | read, search, web | write / edit | bash | MCP tools |
|---|---|---|---|---|
| `read-only` | allow | deny | read-only commands only | allow if the server marks the tool read-only, else deny |
| `ask` (default) | per `toolPermissions` | ask | ask | per `toolPermissions` |
| `accept-edits` | allow | allow inside the workspace; ask outside it and for `.git/`, `.loop/`, `.env*` (see below) | per `toolPermissions` | per `toolPermissions` |
| `full-access` | allow | allow | allow | allow |
| `plan` | as `read-only`, until you approve the plan ([below](#plan-mode)) | deny | read-only commands only | as `read-only` |

- **Search tools** (`grep`, `glob` and `ls`) only read, so every mode runs them without asking. `grep` and `glob` respect `.gitignore` and skip hidden files unless the call asks for them; `.git/` is never searched. They also skip every file a `read` deny rule (or the tool's own pattern deny) refuses, matched like a `read` call (see "Path patterns" under [Rules](#rules)): on the path the search reached and on the file it names through symlinks and `..`. So `"read(~/.ssh/*)": "deny"` keeps the files in `~/.ssh` out of a search of your home directory, and out of a search of a workspace that has a link into `~/.ssh`; the result says how many paths were hidden. The names of directories stay visible.
- **Protected names in `accept-edits`:** `.git`, `.loop` and `.env*` match without case (`.GIT` too), on the path as written and after symlinks are resolved. Only the part below the workspace counts. A path Loop cannot resolve asks.
- **The workspace** is the git root of the directory Loop runs in, or that directory itself. For the `accept-edits` workspace check, paths are resolved through symlinks, so a link that points outside the workspace counts as outside.
- **A write or edit through a symlink** shows where it lands in the approval prompt: `notes → /real/path/key`, in the title and in the detail.
- **`read-only` is a ceiling.** A hook can't allow a call that changes files or runs commands.
- **What `read-only` turns off is taken away, not just refused.** The model isn't offered `write`, `edit`, or MCP tools that their server doesn't mark read-only, in the agent loop or through `loop mcp serve`. `bash` stays, for read-only commands. Switching mode changes the toolset from the next turn.
- **Bash in `read-only`** runs a command only when every part of it reads: `ls`, `pwd`, `cat`, `head`, `tail`, `wc`, `grep`, `rg`, `find`, `tree`, `file`, `stat`, `du`, `which`, `echo`, `sort`, `uniq`, `diff`, and `git status`, `log`, `diff`, `show`, `blame`, `ls-files`, `rev-parse`, and `branch` when it only lists. Output redirects (`>`), `$(…)`, backticks, variable assignments, and options that write or run programs (`find -exec`/`-delete`, `sort -o`, `rg --pre`, `git --output`) are refused.
- **MCP tools in `read-only`** run when their server marks them read-only (`readOnlyHint`). The server sets that hint, so deny a tool in `toolPermissions` if you don't trust it.
- **Session approvals:** "Accept all … for this session" in a prompt stops further prompts for that group (file edits, bash or web) in this session, including after `--resume`.
- **Deny rules and hooks apply in every mode,** `full-access` included.
- **Calls the mode allows** (for example, edits in `accept-edits`) also skip the after-edit review.

### Choosing a mode

- **Default for every session:** `"permissionMode": "accept-edits"` in `~/.loop/agent/settings.json`. A project's `.loop/settings.json` can only choose a stricter mode.
- **One run:** `loop --permission-mode read-only`.
- **This session:** `/permissions` lists the modes; `/permissions accept-edits` switches. The footer shows the current mode.

### Plan mode

- **Turning it on:** `/plan` (or `/permissions plan`, `--permission-mode plan`). The footer shows `plan`.
- **While on:** the agent works as in `read-only`: it reads, searches and runs read-only commands, but can't edit files or run commands that change things. It writes its plan and calls `exit_plan` to ask you to approve it. The plan shows in the transcript.
- **Approving:**
  - **Approve the plan:** plan mode ends and the mode goes back to the one you had before `/plan`. The agent starts the work in the same turn.
  - **Approve and accept edits from now on:** the same, but the mode becomes `accept-edits`.
  - **Keep planning** (add a reason with **Tab**): the agent stays in plan mode and revises the plan.
- **Unattended runs** (`--print`): nobody can approve, so the agent gives the plan as its answer and changes nothing.
- **Subscription agents** get their `read-only` mode in plan mode; `exit_plan` is Loop's own tool.
- Plan mode isn't one of the four modes `/permissions` switches between; it's listed after them.

### `full-access`

- **Switching:** `/permissions full-access` asks you to repeat the command before it switches.
- **While on:** the footer shows a red `⚠ full access · tools run without approval` banner.
- **Startup:** starting in this mode prints a warning.
- **Unattended runs** (`--print`, `mcp serve`) need the flag `--permission-mode full-access`. The setting alone falls back to `ask` there, with a notice.
- **`mcp serve` without `--token`:** it warns, because any local process could then run tools without approval.

## Rules

`toolPermissions` maps a tool to `allow`, `ask` or `deny`. A key can carry a pattern on the call's main argument: the command for `bash`, the path for `read`/`write`/`edit`, the search root for `grep`/`glob`/`ls` (`.` when the call names none), the URL for `web_fetch`, the query for `web_search`.

```json
{
  "toolPermissions": {
    "bash": "ask",
    "bash(git status*)": "allow",
    "bash(cargo test*)": "allow",
    "bash(rm -rf *)": "deny",
    "write(~/secrets/*)": "deny",
    "grep(~/secrets/*)": "deny",
    "write(src/generated/*)": "ask",
    "web_fetch(https://docs.rs/*)": "allow",
    "mcp__slack__*": "deny"
  }
}
```

- **Which rule wins:** a pattern rule beats the plain tool rule. Among matching pattern rules, `deny` beats `ask`, which beats `allow`.
- **Globs:** `*` matches any characters, including `/`; `?` matches one character.
- **Bash commands** are split at `;`, `&&`, `||`, `|`, `&` and newlines that are outside quotes:
  - A `deny` or `ask` pattern applies if **any** part matches it.
  - An `allow` pattern applies only if **every** part matches an `allow` pattern. So `git status && rm -rf x` still asks.
  - A command that uses `$(…)`, backticks or `<(…)` never matches an `allow` pattern.
- **Path patterns** (`read`, `write`, `edit`, `grep`, `glob`, `ls`) match the path as written (`~/` expanded; a relative pattern also matches a path inside the workspace) and the file it names: the path with `.` and `..` removed, and the path resolved through symlinks, both as the system opens it and after `..` is removed. The fixed directory part of an absolute or `~/` pattern is resolved the same way, so `/var/x/*` also matches `/private/var/x/*` on macOS.
  - A `deny` or `ask` pattern applies if it matches **any** of these forms. A link or `..` into a denied directory is denied too.
  - An `allow` pattern applies only if it matches the path as written **and** every resolved form. A link out of an allowed directory is not allowed by that pattern.
  - A path Loop cannot resolve (a symlink loop) matches every `deny` and `ask` pattern and no `allow` pattern.
  - **Read rules do not cover the shell.** `"read(~/.ssh/*)": "deny"` stops the `read`, `grep`, `glob` and `ls` tools; `cat ~/.ssh/id_rsa` in `bash` is matched only by `bash` rules, which read the command as text. To keep commands out of files, use the sandbox (`sandbox.mode: local`; on macOS, `sandbox.denyHomeReads`). The sandbox has its own read limits; it does not read `toolPermissions`.
- **A pattern `ask` still asks in `accept-edits`.**
- **Loop has an approval prompt only for `write`, `edit`, `bash`, `web_fetch`, `web_search` and plan mode's `exit_plan`.** An `ask` rule on any other tool (for example `read(~/.ssh/*)` or an MCP tool) refuses the call. Use `allow` or `deny` for those.
- **A project's `.loop/settings.json`** can add `ask` and `deny` rules or make a rule stricter. It can't add `allow` rules or loosen a global rule.

## Subscription agents

Agents such as ChatGPT (codex-acp) run their own tools. Loop sets the agent's own mode to match:

| Loop mode | Codex mode |
|---|---|
| `read-only`, `ask` | `read-only` (Codex asks before edits and network use) |
| `accept-edits` | `workspace-write` |
| `full-access` | `agent-full-access` |

The agent's permission requests are checked in this order:

1. **Session Policy off** rejects.
2. **Deny rules** reject. A plain deny (`"bash": "deny"`) always applies. A pattern rule (`"bash(rm *)": "deny"`) matches only when the agent sends Loop's argument names: `command` as a string, `path`, `url` or `query`.
3. **The mode's off state** rejects (`read-only` rejects edits and commands).
4. **Hooks:** a block or allow chooses the agent's own "reject once" or "allow once" option, never an "always" one.
5. **`full-access`** allows once.
6. **Otherwise**, you are asked.

Codex runs some commands without asking: read-only commands in `read-only` mode, and sandboxed commands in `workspace-write`. Loop sees those only as tool-call updates, after they run.

## Upgrading from 0.4

- **`--print` and `mcp serve` refuse tools that need approval.** In `ask` mode that is `write`, `edit` and `bash`. The model is told why. For unattended edits use `--permission-mode accept-edits`. Bash still needs `full-access` or an allow rule such as `"bash(cargo test*)": "allow"`.
- **Resumed sessions ask too.** Before, `fileEditReview: newSession` (the default) turned prompts off after `--resume`. Now the mode decides; approvals you accepted for the session still apply.
- **`fileEditReview: never`** turned off every prompt. With no `permissionMode` set, it now starts Loop in `accept-edits`, with a notice at each start; setting `permissionMode` removes the notice. `newSession` and `always` no longer change anything, and `/review` only shows the session's approvals.
- **`web_fetch` and `web_search` are allowed by default.** A settings file that already says `ask` for them keeps asking. To be asked before each request, set `"web_fetch": "ask"` and `"web_search": "ask"` in `toolPermissions`.
- **A hook file with a `tool_call` block and no `match`** now blocks every tool call. Loop warns about such files at startup.
- **ChatGPT (Codex) in `ask` mode** runs in Codex's `read-only` mode, so it asks before each edit. Before, it ran in Codex's own default, which edits without asking.
- **A project's `.loop/settings.json`** can no longer loosen `toolPermissions` or `fileEditReview`.

## `bash` block list

Some `bash` commands are refused in every mode, `full-access` included: recursive `rm`, `chmod`, `chown` and `chgrp` on `/`, your home directory (`~`, `$HOME` or its path) or a top-level system directory (`/usr`, `/etc`, `/Users`, …), and `mkfs`, a fork bomb, `dd` onto a disk device, `> /dev/sda` and `format c:`. Loop also checks scripts given to `sh -c`, `su -c`, `eval`, `ssh`, a here-string or a shell's stdin. `rm -rf /tmp/x`, `rm -rf ./build` and `chmod -R u+w ./src` run.

The block list is a best-effort guard against accidents, **not a security boundary**. It reads the command as text, so it misses what programs, variables and scripts do: `find / -delete`, `xargs rm -rf`, `rm -rf "$DIR"` and `cd / && rm -rf *` all pass. The boundary is the sandbox and the permission rules above: deny rules, modes and hooks.

## `bash` time limits

A `bash` call may pass `timeoutMs` to get more time than the default. A command that times out tells the model how to ask for more.

| Setting | Default | Purpose |
|---|---|---|
| `bash.defaultTimeoutMs` | `120000` | Timeout when a call names none |
| `bash.maxTimeoutMs` | `600000` | The most a call may ask for |

Both are global-only. `/workflow` shell tasks use the default.
