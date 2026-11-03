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
can execute a trivial command under the complete policy.

Writes are allowed under the real project path, the real system `$TMPDIR`, and
`sandbox.allowWrite`. Reads and writes are denied under the user's `.ssh`,
`.aws`, `.gnupg`, `.config/gh`, user/system Keychains, and the resolved agent
directory's `auth.json` and `run/`. The agent directory honors
`LOOP_CODING_AGENT_DIR`, including relative paths and symlinks. The short Runtime
socket fallback directory is protected too. Protected ancestors cannot be
renamed out of the pathname denies, even under a broad writable grant.

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
files are refused. Directory children, ignore files and grep contents all pass
through the same interface. Search never falls back to a host walker. Edit review
snapshots also use the confined filesystem.

The shell uses the host execution runner's process groups, timeout/cancellation
cleanup and output draining. Output retention is bounded to 1 MiB per stream and
combined output; excess bytes are drained without being retained. Normal shell
exit also stops background descendants so inherited pipes cannot evade a timeout.

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
git commits, and an offline Cargo build with explicit Rust cache grants. Unix
unit tests also stress concurrent symlink replacement against file writes.
