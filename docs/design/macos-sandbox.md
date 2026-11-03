# macOS local sandbox

Decision: use Seatbelt for `/sandbox local` on macOS now. It provides partial
isolation for shell commands and the built-in filesystem tools. Linux keeps
Podman and its existing full/partial options. A macOS request for `--full` or a
Linux container runtime fails with a platform-specific message.

## Boundary

Each shell command starts through `/usr/bin/sandbox-exec -p PROFILE -D NAME=VALUE
/bin/sh -c COMMAND`. The profile starts with `(deny default)` and grants process
execution/forking, system reads, selected system Mach lookups, and filesystem
reads outside protected areas. The profile source contains no filesystem paths: only
parameter names. Paths are passed as separate arguments, never shell-expanded
or inserted into profile syntax. Startup checks that sandbox-exec exists and
can execute a trivial command under the complete policy, then proves that a
denied read and a denied write both fail with host-created canaries.

Writes are allowed under the real project path, a Loop-created private temporary
directory, and `sandbox.allowWrite`. Project `.loop`, Git hooks/config and the
whole resolved agent directory are always write-protected, even under broad
allowWrite grants. Git worktree pointers and shared hooks/config are protected
as well. An existing repository supports commits; initialize repositories outside
the sandbox because initialization writes the protected Git configuration.
Project settings may enable sandbox mode, but cannot disable a required sandbox.

Reads are denied for SSH/AWS/GPG/GitHub credentials, Keychains, `.netrc`, Docker,
Kubernetes, npm/PyPI and gcloud credentials, browser Cookies, and native ACP login
folders including Codex, Claude, Gemini, OpenCode, Goose and Amp. The resolved
agent directory's `auth.json`, `run/`, `mcp-auth/`, `agents/` subscription state,
`sessions.db*`, `events.db*`, history and logs are also denied. The agent directory
honors `LOOP_CODING_AGENT_DIR`, including relative paths and symlinks. The short
Runtime socket fallback directory is protected too. Protected ancestors cannot
be renamed out of the pathname denies.

Each sandbox gets an exclusively created, owner-only 0700 directory under the
system `/tmp`, independent of the ambient TMPDIR. The path is canonicalized and
checked for owner, permissions and UTF-8 before granting access. Child TMPDIR
always points there, overriding tool-provided values. Commands cannot loosen the
root's mode or remove it. Destroy cleans it after command cleanup finishes.

Shell networking is off by default. `sandbox.network: true` permits TCP/UDP
networking; pathname Unix sockets remain denied, including custom Runtime
socket locations, except for the system DNS resolver endpoint. Network-enabled
commands may also use the system DNS and certificate trust services. The profile does not grant Keychain services. A child receives
only ordinary environment variables (PATH, HOME, locale, terminal, temp and Rust
installation locations); provider keys, Runtime tokens, loader injection and
shell startup variables are dropped, including tool-supplied overrides.

`JailedHostFs` confines built-in read/write/edit/grep/glob/ls operations. Reads
outside the project are permitted except for the protected paths above. File-tool
writes stay in the project and explicit allowWrite paths. Existing ancestors are
canonicalized even for a new nested file. File access then walks from `/` with
no-follow, descriptor-relative opens, so replacing an ancestor with a symlink
between checking and opening fails closed. Nonregular files and multiply linked
files and `/dev` paths (including `/dev/fd`) are refused. Every opened component
is checked by device/inode against protected roots, retaining identities across
renames and refreshing identities for newly created protected paths. Directory children, ignore files and grep contents all pass
through the same interface. Search never falls back to a host walker. Edit review
snapshots also use the confined filesystem.

The shell uses the host execution runner's bounded output collection and
cancellation. Seatbelt commands start in a new session without a controlling tty;
`/dev/tty` is not writable. Output retention is bounded to 1 MiB per stream and
combined output; excess bytes are drained without being retained. After killing,
child reaping and pipe draining have explicit bounds; dropping the execution
future also aborts its reader tasks.

Cleanup identifies descendants with a unique, inherited Seatbelt policy marker,
not their removable environment or mutable session/process-group IDs. It uses
macOS process information and the same private `sandbox_check` interface used by
[WebKit](https://github.com/WebKit/WebKit/blob/main/Source/WTF/wtf/spi/darwin/SandboxSPI.h).
The command denies one unique marker path and allows its neighboring probe;
cleanup finds the matching policy even after setsid, double fork, reparenting or
exec with an empty environment. Applying a nested profile is explicitly denied
so a command cannot hide its policy marker. PID birth times guard against PID reuse. It
freezes matching processes to a fixed point before killing them. Process-query
failure is reported rather than silently falling back to process groups. Normal
return, timeout, cancellation and dropped futures clean up descendants. Stop and
destroy cancel all active commands and wait for cleanup before returning.

This is **partial** isolation: commands still execute native host binaries and
can read ordinary host files. Host web tools, MCP services, hooks and other
external integrations are separate capabilities; the shell network setting does
not claim to isolate their traffic. Granting writes to an installation/cache
allows commands to modify it. Prefer narrowly scoped allowWrite paths.

## Alternatives and follow-up

Apple's [container project](https://github.com/apple/container) is the intended
follow-up for macOS `--full`: a Linux VM boundary would provide a separate system
and a clearer whole-environment isolation model. It needs image, mount, networking
and lifecycle work; it is not selected silently for this lane.

[App Sandbox](https://developer.apple.com/documentation/security/protecting-user-data-with-app-sandbox)
is built around signed app entitlements and inherited helper permissions. Loop
needs per-command writable roots and arbitrary development subprocesses in a
terminal application. App Sandbox is a poor fit for that dynamic policy. Apple's
[helper documentation](https://developer.apple.com/documentation/xcode/embedding-a-helper-tool-in-a-sandboxed-app)
also requires sandbox inheritance/signing arrangements. Seatbelt fits the current
native command lane, while its deprecated interface is a reason to retain the
preflight and plan the container backend.

## Validation

Cross-platform unit tests cover profile parameters, denied-root precedence,
missing-path resolution and child-environment selection. macOS integration tests
exercise built-in tools and shell denies, symlink escapes, hard-link/rename
aliases, network off/on, Unix control sockets, process-tree timeout cleanup,
protected configuration writes, credential/descriptor aliases, private TMPDIR,
setsid/empty-environment escape cleanup, git commits, and an offline Cargo build with explicit Rust cache grants. Unix
unit tests also stress concurrent symlink replacement against file writes.
