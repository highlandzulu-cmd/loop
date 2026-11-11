# E1 · Recoverable truth: results

Hypothesis H1 (ownership and recovery). Probe run on 2026-11-11.

## Result in one paragraph

E1 does **not** pass. Four of the five criteria pass with zero failures in
400 trials. The fifth criterion fails: 8 to 10 % of interrupted Runtime
turns are not marked uncertain (64 of 800 and 79 of 800 in the two trial
sets). The cause is two ordering defects between `events.db` and
`sessions.db` (D1, D2). The probe also found and fixed two smaller defects
(D0, D3). Recommendation: do not merge `staging` to `main` with the Runtime
as the default until D1 and D2 are fixed and E1 passes again. Make the
Session tree a projection of the Event Log in Phase 1.

## Gate

| Question | Answer from E1 |
|---|---|
| Merge `staging` to `main` with the Runtime as the default? | **Not yet.** Fix D1 and D2 first, then run E1 again (2 minutes). |
| Keep two stores with reconciliation, or make the tree a projection of the log? | **Projection** as the Phase 1 target. All Runtime failures are disagreements between the two stores. For the merge, apply two small ordering fixes in the two-store model now. |

## Pass criteria and numbers

Two trial sets of 200 trials each. Each trial has 2 SIGKILL rounds and 3
restarts (see Method). "Runtime" is the daemon; "embedded" is a standalone
owner (the in-process `loop` path).

| Criterion | Set A (seed 20261111) | Set B (seed 424242) | Pass? |
|---|---|---|---|
| Acknowledged mutations lost | 0 of 20,700 (6,952 Runtime + 13,748 embedded) | 0 of 21,220 (7,236 + 13,984) | Yes |
| Duplicate side effects | 0 | 0 | Yes |
| Duplicate recovery records | 0 (and 0 turns found by the extra start after a clean stop) | 0 (0) | Yes |
| Takeovers of a live owner | 0 of 4,800 takeover attempts (all refused), 0 lease rows changed | 0 of 4,800, 0 | Yes |
| Every interrupted turn marked uncertain (Runtime) | **64 of 800 not marked** (47 D1, 17 D2) | **79 of 800 not marked** (64 D1, 15 D2) | **No** |

What happened to the 800 interrupted Runtime turns of set A (set B in
brackets):

| Outcome | Count |
|---|---|
| Marked `runtime_restarted` by recovery | 556 (555) |
| Finished and durable in both stores, only the acknowledgement was lost | 44 (51) |
| Prompt never started: no turn, no side effect, no tree entry | 136 (115) |
| D1: log says `end_turn`, tree has no reply | 47 (64) |
| D2: tool ran, log has no turn | 17 (15) |

Embedded owners have no Event Log, so nothing can mark their interrupted
turns. Of 800 interrupted embedded turns in set A (set B): 226 (273)
finished in the tree but were not acknowledged, 319 (290) left a partial
turn in the tree, 149 (132) ran their tool but left no trace in the tree,
106 (105) left nothing. The design does not promise more for embedded
owners, but it is a real gap: a side effect with no record.

Other counts (set A): 378 SIGKILLs from the parent, 822 deaths at
failpoints, 44 deaths during recovery, 444 Runtime restarts, 0 Session load
failures, 0 resume failures, 0 prompt errors.

## Method

Harness: `crates/loop-runtime/tests/e1_recovery.rs` (ignored by default).

```text
E1_TRIALS=200 E1_SEED=20261111 E1_PARALLEL=6 E1_ROOT=<temp folder> \
  cargo test -p loop-runtime --test e1_recovery -- --ignored --nocapture e1_recoverable_truth
```

- **Provider.** `faux` only (`faux_provider_fn`). No model tokens.
- **Processes.** Each trial has its own agent directory
  (`LOOP_CODING_AGENT_DIR`). The test binary starts itself three times:
  - the daemon: `Runtime::new` + `recover` while it holds `runtime.lock` and
    the Session database lock, as in `runtime_cmd.rs`; two Sessions; SQLite
    Event Log; `full-access`; sandbox off;
  - two embedded owners: `AgentHarness` on the terminal path
    (`create_sqlite_session_store_for_resume`, standalone leases), one
    Session each.
- **Workload.** Each Session sends prompts in a loop. Each turn makes one
  tool call that appends the turn's token to `effects.log` (a real side
  effect: `bash` in the daemon, a custom tool in the embedded owners), then
  answers `done <token>`. A process prints `ACK` only after the prompt
  returns. For the daemon, that is the `TurnHandle` outcome (the same point
  where the wire sends the prompt result).
- **Failure injection.**
  - Daemon failpoints (seeded): SIGKILL itself before or after the Nth
    commit of `prompt_sent`, `turn_started`, `tool_call`,
    `tool_call_update`, `message_ended` or `turn_ended`, or before the Nth
    acknowledgement. In restarts: before or after the recovery batch, before
    or after `mark_recovered`.
  - Embedded failpoints: SIGKILL before or after the Nth tree write
    (`append_entry`), or before the Nth acknowledgement.
  - Then the parent SIGKILLs all three processes at seeded random times
    (0 to 1.2 s). 30 % of trials have a slow log writer (0 to 15 ms per
    append), as under load.
- **Rounds.** Round 0 and round 1 end with SIGKILL. Round 1 and round 2
  restart all owners (two restarts). Round 2 stops cleanly. A last daemon
  start then checks that recovery finds nothing more.
- **Live-owner takeover.** In each round, while all owners are alive, the
  parent makes every live lease look 60 s old. Then the daemon tries to load
  both embedded Sessions, and each embedded owner tries `loop --resume`
  (`takeover_lease`) on both daemon Sessions and on the other owner's
  Session. The parent also checks that the daemon's startup sweep keeps the
  rows of live owners. Before a restart, the parent ages only the leases of
  dead embedded owners, to simulate the 30 s timeout.
- **Checks.** After the trial, the parent reads `events.db`,
  `sessions.db` and `effects.log`, and compares them with what each process
  printed (`SENT`, `ACK`, `PROBE`).
- **Seeds.** The master seed gives each trial a seed; the run prints both.
  A seed fixes the plan (failpoints, kill times, slow writer), not the exact
  thread interleaving.
- **Run time.** Set A: 101 s wall time. Set B: 100 s. The 200 target fits
  easily in the 15–30 minute budget (6 trials in parallel, 12-core macOS 27).

Code under test: branch `probe/e1-recovery`, commit `da03ef4` (staging
`1dc7ef4` plus the fixes for D0 and D3 below). Existing `kill -9` binary
tests (`crates/loop-cli/tests/runtime_e2e.rs`, 15 tests) pass on this
commit.

### Limits of the method

- SIGKILL only. No power loss, so no test of `synchronous=NORMAL` in
  `events.db`.
- The daemon is driven in-process, not through the socket. The wire layer
  is not in the loop (the `runtime_e2e` tests cover `kill -9` through the
  binary).
- The workload has no Escalations (full-access) and no Subagents. The
  existing recovery tests cover those.
- The 30 s lease timeout is simulated by changing `heartbeat_ms` in the
  database.

## Defects

### D1 — `turn_ended` commits before the tree holds the turn (open)

- **Where.** The harness writes all tree entries of a turn after the turn
  ends (`crates/loop-agent/src/harness/agent_harness.rs:1391-1410`; entries
  are queued at `:1543`). The stream emits `TurnEnded` while the turn ends
  (`crates/loop-agent/src/native_backend.rs:633-642`), and the pump commits
  it to `events.db` at once (`crates/loop-runtime/src/runtime.rs:2553-2566`).
- **Effect.** A crash between the two commits leaves `turn_ended: end_turn`
  in the log and no reply (or a partial turn) in the tree. Recovery sees a
  finished turn and does not mark it uncertain. Attached Surfaces have
  already shown the turn as finished. The prompt result itself is safe: it
  waits for the tree writes.
- **Test.** `crates/loop-runtime/tests/e1_defects.rs`,
  `d1_a_logged_turn_end_implies_the_tree_holds_the_turn` (ignored, fails).
- **Fix direction.** Commit `turn_ended` only after the tree writes of the
  turn (hold it in the native backend until `harness.prompt` returns), or
  let recovery compare the tree with the log. Not done here: it changes the
  stream contract.

### D2 — a turn can run tools before its start is in the log (open)

- **Where.** Tools do not wait for the pump. Recovery reads only
  `turn_started`, `turn_ended` and Escalation records
  (`crates/loop-runtime/src/recovery.rs:35-60`,
  `crates/loop-runtime/src/log/sqlite.rs:373-381`).
- **Effect.** With a slow log writer, a crash can leave only `prompt_sent`
  in the log for a turn that already ran its tool (and sometimes wrote part
  of the tree). Recovery records nothing for that turn. All 32 cases were
  in trials with the slow writer; all 111 D1 cases were in trials without it.
- **Test.** `e1_defects.rs`,
  `d2_a_prompt_whose_turn_start_was_not_logged_is_recovered_as_uncertain`
  (ignored, fails).
- **Fix direction.** Recovery also ends, as uncertain, a `prompt_sent` that
  has no later `turn_started` in its generation (this needs a turn id for
  the synthetic end), or the Runtime commits `turn_started` before the
  first tool runs. Not done here: it needs a protocol choice.

### D3 — a failed tree write was acknowledged as completed (fixed)

- **Where.** `crates/loop-agent/src/harness/agent_harness.rs:1400-1410`.
  `run_turn` logged a failed `append_entry` and returned the reply.
- **Effect.** The prompt resolved `Completed` and the log said `end_turn`,
  but `sessions.db` had no trace of the turn. A lost acknowledged mutation.
  The trigger is a write that fails: another process holds the write lock
  for more than 6 s, a full disk, a lost lease. SIGKILL does not cause it,
  so the trial sets did not see it.
- **Fix.** Commit `da03ef4`: stop at the first failed write and return the
  error, so the turn reports a failure. Test `d3_a_completed_turn_is_in_the_tree`
  (passes now).

### D0 — two processes could not open a fresh Session database together (fixed)

- **Where.** `crates/loop-agent/src/harness/session/sqlite/store.rs:258-300`.
  The "already applied" check ran outside the IMMEDIATE transaction, and the
  switch to WAL reported BUSY at once.
- **Effect.** Two `loop` processes that start at the same time on a new
  agent directory: one fails with `table session_leases already exists` or
  `UNIQUE constraint failed: migrations.id`. E1 hit this in all trials of
  its first test runs.
- **Fix.** Commit `ef20c39`: check again inside the transaction; retry the
  WAL switch for up to the busy timeout. Test
  `concurrent_first_opens_of_a_fresh_database_all_succeed` in
  `crates/loop-agent/tests/session_sqlite.rs`.

## Recommendation

1. **Merge `staging` to `main`: no, not now.** Criteria 1 to 4 are strong
   (0 failures in about 42,000 acknowledged turns and 9,600 takeover
   attempts). Criterion 5 fails because of D1 and D2. Both have bounded
   fixes. Fix them, run E1 again with the same seeds, then merge if E3 also
   passes.
2. **Two stores or one: make the tree a projection of the Event Log
   (Phase 1, "One store for truth").** Every Runtime failure in E1 is a
   disagreement between `events.db` and `sessions.db`, not a defect in one
   store. The ordering fixes for D1 and D2 repair today's two paths, but
   each new write path needs the same care. Embedded owners write only the
   tree, so their interrupted turns can never be marked uncertain, and 40 %
   of them left a partial turn. One log with the tree as a projection gives
   one commit point and lets recovery see every turn.

## Files

- `crates/loop-runtime/tests/e1_recovery.rs` — the harness.
- `crates/loop-runtime/tests/e1_defects.rs` — tests for D1, D2, D3.
- `crates/loop-agent/tests/session_sqlite.rs` — test for D0.
