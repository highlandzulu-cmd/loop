# CI

This page is for maintainers. Loop supports Linux and macOS only, so no
workflow has a Windows job or step.

## Workflows

| Workflow | File | Runs on | Does |
|----------|------|---------|------|
| CI | `.github/workflows/ci.yml` | push to `main` and `staging`; PR into `main` or `staging`; merge queue; manual | Format, lint, tests, desktop check |
| Probes | `.github/workflows/probes.yml` | nightly (default branch); manual | The `#[ignore]` E1 and E3 probe tests |
| Release | `.github/workflows/release.yml` | tag `v*.*.*`; manual | Builds, signs, publishes. See [Releasing](releasing.md) |

## CI jobs

| Job | Runner | What it runs |
|-----|--------|--------------|
| `Changes` | Linux | Reads the changed files of a PR and sets `rust` and `desktop` |
| `Lint (fmt + clippy)` | Linux | `cargo fmt`, ShellCheck, clippy for: the workspace, loop-cli without telemetry, loop-cli without the usage view, and **with `orchestration`** |
| `Test (ubuntu-latest)` | Linux | `cargo test --locked` (default members), `bash -n install.sh` |
| `Test (macos-latest)` | macOS | `cargo test` for the crates that have OS-specific code (see below), `bash -n install.sh` |
| `Test (ubuntu-latest, feature sets)` | Linux | Tests without telemetry, and tests with `orchestration` |
| `Check desktop (nightly)` | Linux | `cargo check -p loop-desktop` on nightly |
| `CI gate` | Linux | Passes when all jobs passed or were skipped. **Require this one check.** |

All jobs run in parallel. There is no separate `cargo build` step: `cargo test`
builds the same binaries.

### Which jobs run

- Push to `main` or `staging`, merge queue, manual run: all jobs.
- PR: `Changes` looks at the files.
  - Only docs (`docs/`, root and `.github/` Markdown files, `resources/`,
    `LICENSE`, issue templates): the Rust jobs are skipped. The gate still
    passes, because skipped jobs count as passed.
  - The desktop job runs only when a crate in the desktop dependency tree
    changes (`loop-desktop`, `loop-app-core`, `loop-agent`, `loop-ai`,
    `loop-mcp`, `loop-orchestration`, `loop-telemetry`, `loop-test-support`,
    `core/*`, `extend/loop-acp`), or `Cargo.toml`, `Cargo.lock` or `ci.yml`
    change. If the desktop app starts to depend on another crate, add it to the
    `desk` pattern in `ci.yml`.
  - Anything else: all jobs.
- A new push to a PR cancels the older run of that PR. Runs on `main` and
  `staging` are never cancelled.

### Why these feature sets

`orchestration` is off by default, so the default clippy and test runs never
compile the code behind it. A defect in that code passed CI for this reason.
`--features loop-cli/orchestration` turns it on in loop-agent and loop-runtime
too. These sets run on Linux only: they have no OS-specific parts.

### What runs on macOS, and why

macOS minutes cost 10 times a Linux minute. macOS runs the tests of the crates
that have `cfg(target_os)` or `cfg(unix)` code: loop-agent (seatbelt sandbox,
host environment), loop-app-core, loop-cli, loop-runtime, loop-wire,
loop-telemetry, loop-mcp, loop-update (the update lifecycle) and
loop-test-support. loop-ai, loop-orchestration, loop-protocol, loop-acp and
loop-dashboard have no such code, so only Linux runs their tests. They still
compile on macOS as dependencies. A new crate runs on macOS until you add it
to the exclude list in `ci.yml`. Clippy is not run on macOS, so the
`cfg(target_os = "macos")` code is compiled (by the tests) but not linted.

### Caches

- `Swatinem/rust-cache`. Only `main` and `staging` save caches. PRs restore
  them, so per-PR copies do not push the shared caches out of the 10 GB limit.
- Lint and test have separate keys (`lint`, `test-<os>`): clippy and test
  builds make different files, and a cache key can be saved only once.
- The feature-set job and the probes restore `test-ubuntu-latest` and never
  save. Their dependencies are the same.
- `CARGO_PROFILE_DEV_DEBUG=line-tables-only` keeps `target/` small. rust-cache
  puts `CARGO_*` variables in the key, so `probes.yml` must keep the same
  values as `ci.yml`.
- `cache-on-failure` saves the cache of a failed run on `main` and `staging`.

### Other settings

- `permissions: contents: read` for every workflow. Only the release job can
  write.
- Actions are pinned to commit SHAs. `.github/dependabot.yml` opens weekly PRs
  to update them.
- Every job has `timeout-minutes`.

## Probes

`e1_recovery` and `e3_enforcement` are `#[ignore]` in the normal test run
(1 to 2 minutes each). They use the `faux` provider: no model tokens, no
network. E3 needs `--features orchestration`. The E3 harness switches off
external diff windows, so no probe opens a GUI.

`probes.yml` runs them nightly on the default branch, and on demand (Actions,
Probes, Run workflow, choose a branch; inputs: E1 trial count and seed). The
commands are the ones in [E1](probes/e1-results.md) and [E3](probes/e3-results.md).
They are not part of the PR gate. An E3 `LEAK` fails the job by design: the
probe fails while a leak is open.

## Release workflow

Release semantics, artifact names and the signing steps and secrets did not
change. A tag run cannot restore the cache of an older tag (caches belong to a
ref, plus the default branch), so the release builds save a cache only on
`main`. To make a tag build warm, first run Release by hand on `main` without
"create_release".

## Not done, and why

- **cargo-nextest.** It can cut test wall time, but it does not run doctests,
  and it runs each test in its own process, so shared in-process state in
  tests behaves differently. Try it on a branch: install with
  `taiki-e/install-action`, run `cargo nextest run --locked` plus
  `cargo test --doc --locked`, and compare the times.
- **Shared cache for lint and test.** See Caches.
- **Clippy on loop-desktop.** `CONTRIBUTING.md` asks for it by hand. Nightly clippy changes every night and could turn unrelated PRs red, so CI only compiles the app.
- **Clippy on macOS.** About one extra macOS minute per run (10 Linux minutes).
