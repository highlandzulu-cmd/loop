# E7 · Turn queue (scheduling liveness): results

Hypothesis H3. Probe run on 2026-11-11. Branch `probe/e7-turn-queue`.

## Result in one paragraph

E7 passes the task gate. The gate run is 1,000 random schedules, all at the
default limits (16 Sessions, 8 active turns), against an in-process Runtime
with the `faux` provider (`E7_LIMITS=default`, 553 s). It has 0 deadlocks, 0
lost permits (849 of 849 permit probes exact; 151 schedules shut down with
turns in flight and have no probe), 0 over-limit active turns, 0 over-limit
loaded Sessions, 0 prompts without exactly one terminal record, and 0 panics.
A second run of 1,000 schedules with mixed smaller limits (593 at 16/8, 407
smaller) adds contention. It also passes (437 s). The probe found one small defect
(D1): a cancel that arrives while the native backend prepares a prompt ended
the turn as `Failed` and not as `Cancelled`. It is fixed by a four-line change
in `runtime.rs` (separate commit). The first run found it 5 times in 1,000
schedules. The second run, with the fix, found it 0 times.

## Gate

| Question | Answer from E7 |
|---|---|
| Is the turn queue free of deadlock and lost permits (A4: "The turn queue has no deadlock")? | **Yes**, for the cases below. Liveness and terminal records: 1,000 of 1,000 schedules, in each of two runs, after the D1 fix. Permits: 849 of 849 probes (gate run), 842 of 842 (mixed run). The shutdown-in-flight schedules have no permit probe. |
| Does it block "Runtime as default"? | **No.** D1 is cosmetic (wrong outcome name, no lost state) and is fixed. |
| Permit release and reacquire rules, and admission | No defect found in the rules as built. The parent releases its permit while it waits for a Subagent and reacquires it after. The shutdown case is below (note N1). |

The task pass line is: "faux provider, 1,000 schedules: 0 deadlocks, 0 lost
permits". The longer line in `docs/stress-tests.md` (E7) asks for more. See
"What E7 does not cover".

## Method

Harness: `crates/loop-runtime/tests/e7_turn_queue.rs`. It is ignored by
default.

```text
E7_SCHEDULES=1000 E7_SEED=20261111 E7_PARALLEL=4 cargo test -p loop-runtime \
  --test e7_turn_queue -- --ignored --nocapture --test-threads=4 e7_turn_queue
```

Other settings: `E7_ONLY=<index>` runs one schedule and prints it.
`E7_LIMITS=default` uses 16 Sessions and 8 active turns in every schedule.
`E7_BUDGET_SECS` stops new schedules after that time. `E7_ROOT` names the
folder for schedule directories. A failing schedule keeps its directory and
prints the seed, the index, the violations, a prompt table, the full schedule
and a replay command.

- **Provider.** `faux` only (`faux_provider_fn`). No model tokens. The model
  answers by the token in the prompt (`E7[id;mode;delay;kids]`). It waits a
  random time (0 to 200 ms) per call. The modes are: text; `ls` tool;
  `bash` tool (`sleep 0.05`); injected model error; `subagent` tool (1 to 3
  parallel children); and nested Subagents (a child that spawns a child).
- **Runtime.** One Runtime per schedule, in one process, with one agent
  directory per schedule. Permission mode `full-access`, sandbox off.
- **Limits.** Gate run: every schedule uses the default limits (16 Sessions, 8
  active turns). Mixed run: 593 of 1,000 schedules use the defaults. The others
  use 2/1, 3/2, 4/2, 5/3, 8/4 (Sessions/turns) to create more contention.
- **Event Log.** `SlowLog` wraps `MemoryEventLog`. It delays every append by a
  seeded random time (0, 2, 10 or 30 ms maximum per schedule). This is the
  "slow event-log write" hook. It also counts open `turn_started` to
  `turn_ended` windows.
- **Schedule.** A pure function of (seed, index). 3 to 10 concurrent clients
  each run 15 to 45 operations: open Session (the schedule wants more than
  `max_sessions`, up to 40), prompt, cancel, close (plain or forced), load a closed Session,
  attach or detach a watching connection, and pause. 25 % of prompts have a
  cancel after 0 to 90 ms (queued or running). 10 % of prompts aim at a Session
  that already has a turn. 15 % of schedules call `shutdown` with turns in
  flight. 40 % of schedules include Subagent prompts.
- **Steer and follow-up.** The Runtime has no such API
  (`capabilities().steering` is false, and the harness asserts this). The
  nearest operation is a prompt on a busy Session, which the Runtime refuses
  with `Busy`. The harness does that.
- **Replay.** The schedule repeats from the seed. Thread timing does not, so a
  replay can take another interleaving.

### Checks per schedule

| Criterion | How it is checked |
|---|---|
| Every prompt ends (no hang) | A 30 s deadline on every `prompt` call, every outcome and every admin call. A 240 s deadline on the schedule. A dropped outcome sender (`Uncertain`) is a violation. |
| All permits free after quiescence | After all outcomes, with all watchers detached: `status().busy == 0`, no `is_busy`, `is_idle()`. Then a gated probe: `max_active + 1` idle Sessions each get a prompt whose model call waits on a gate. Exactly `max_active` calls must start, and the next must wait. Fewer means a lost permit. More means over-release. |
| No turn marked active | Same step: `busy` count and `is_busy` for every Session. |
| Active turns never exceed the limit | (a) The most model calls in flight for prompts that were never cancelled or force-closed (those can outlive their turn, because the faux stream is not cancelled). (b) In schedules without Subagents: the most open `turn_started`..`turn_ended` windows in the log. (c) The probe peak. |
| Loaded Sessions never exceed the limit | `status().loaded` sampled every 1 ms and after each admission operation. |
| One terminal record per acknowledged prompt | From `read_runtime_after(0)`: per Session, the log must read `prompt_sent`, then either `turn_started` + `turn_ended` or `prompt_settled`, and nothing else. Every acknowledged prompt id has exactly one `prompt_sent` and a terminal record. A refused prompt leaves no `prompt_sent`. `Completed` needs a turn and the same stop reason. Child Sessions are checked with the same state machine. |
| After shutdown | 0 loaded Sessions. No lease left in `sessions.db` for any Session in the log. |
| No panics | A counting panic hook (Tokio hides panics in spawned tasks). |
| Cancel latency | Measured from the cancel call to the outcome. Reported, not asserted (see below). |

### Negative controls

- **Permit probe.** `e7_negative_control` leaves one turn open. The probe must
  find 7 of 8. It does (`E7|CONTROL|... reached 7 of 8`).
- **Mutation.** I leaked one permit in the prompt task (`std::mem::forget` of
  the permit when a cancel raced the acquire) and ran 60 schedules. The probe
  failed 9 of 60 schedules with `LOST_PERMIT only 7 of 8 turns could start`.
  The change was reverted (`git checkout`). It is not in any commit.

## Numbers

### Gate run: default limits only (16/8), with the D1 fix

Command: `E7_LIMITS=default E7_SCHEDULES=1000 E7_SEED=20261111 E7_PARALLEL=4`,
commit `62fa2ba`, 553 s wall time (budget: 15 minutes). Result: **PASS**.

| Quantity | Gate run |
|---|---|
| Schedules / failed | 1,000 / 0 |
| Schedules with Subagents | 388 |
| Deadlocks or hangs | 0 |
| Permit probes exact (8 of 8 start, the 9th waits) | 849 of 849 |
| Peak loaded Sessions | 16 (limit 16) |
| Peak live model calls (never-cancelled prompts) | 8 (limit 8) |
| Peak acknowledged prompts at once (so prompts queued beyond 8) | 16 |
| Prompt calls / acknowledged | 103,552 / 44,743 |
| Refused `Busy` / refused for another expected reason | 46,736 / 12,073 |
| `prompt_sent` records (roots and children) | 56,675 |
| `turn_ended` / `prompt_settled` (cancelled or closed before a turn ran) | 55,096 / 1,579 |
| Prompts with no or two terminal records | 0 |
| Outcomes completed / cancelled / failed | 30,218 / 13,769 / 756 (47 of the failed: `semaphore closed` at shutdown, note N1; the rest injected model errors) |
| Child prompts (Subagents) | 4,291 |
| `new_session` accepted / refused `Busy` at the limit | 23,676 / 258 |
| Forced closes / plain closes accepted | 4,298 / 1,353 |
| Schedules that shut down with turns in flight | 151 |
| `SessionClosed` with `aborted: true` | 0 |
| Panics | 0 |
| Schedule time p50 / p99 / max | 1.95 s / 6.3 s / 12.3 s |
| Cancel latency, n=6,943: p50 / p99 / max / over 1 s | 59 ms / 472 ms / 2,115 ms / 15 |

### Mixed-limit run, with the D1 fix

Seed 20261111, 4 schedules in parallel, commit `62fa2ba`, 437 s. The first
run (harness `46fd40e`, without the fix, mixed limits) took 390 s.

| Quantity | Final run | First run (no fix) |
|---|---|---|
| Schedules | 1,000 | 1,000 |
| Schedules failed | **0** | 5 (all D1) |
| Schedules with default limits (16/8) | 593 | 593 |
| Schedules with Subagents | 392 | 392 |
| Deadlocks or hangs | **0** | 0 |
| Lost permits (probe runs exact: 842 of 842; no probe in 158 shutdown-in-flight schedules) | **0** | 0 |
| Over-release (more than the limit started) | **0** | 0 |
| Peak loaded Sessions | 16 (limit 16) | 16 |
| Peak live model calls (never-cancelled prompts) | 8 (limit 8) | 8 |
| Peak open turn windows, no-Subagent schedules | at most the limit | at most the limit |
| Peak acknowledged prompts at once | 16 | 16 |
| Prompt calls | 104,666 | 104,666 |
| Acknowledged prompts | 32,104 | 30,772 |
| Refused with `Busy` | 60,321 | 61,366 |
| Refused for another expected reason (Session not loaded, closing) | 12,241 | 12,528 |
| `prompt_sent` records (roots and children) | 40,342 | 39,096 |
| Terminal `turn_ended` | 38,281 | 36,935 |
| Terminal `prompt_settled` (never ran a turn) | 2,061 | 2,161 |
| Prompts with no or two terminal records | **0** | 0 |
| Outcomes: completed / cancelled / failed | 20,422 / 11,132 / 550 | 19,244 / 10,962 / 566 |
| Failed outcomes that are expected (injected model errors, and `semaphore closed` at shutdown: 70 and 67) | 550 | 561, plus 5 D1 |
| Child prompts (Subagents) logged | 2,652 | 2,738 |
| Model calls | 47,783 | 46,548 |
| `new_session` at the limit refused with `Busy` | 562 | 793 |
| `new_session` accepted | 18,430 | 18,331 |
| Forced closes / plain closes accepted | 3,463 / 961 | 3,540 / 892 |
| Close refused `Busy` (plain close of a busy Session) | 1,320 | 1,392 |
| Schedules that shut down with turns in flight | 158 | 158 |
| `SessionClosed` with `aborted: true` (abort path) | 0 | 0 |
| Panics | **0** | 0 |
| Schedule time p50 / p99 / max | 1.46 s / 5.2 s / 10.7 s | 1.33 s / 4.3 s / 6.3 s |

Cancel latency (cancel call to outcome, only prompts that ended `Cancelled`
after a timed cancel):

| Run | n | p50 | p99 | max | over 1 s |
|---|---|---|---|---|---|
| Final, 4 in parallel | 4,987 | 54 ms | 439 ms | 1,735 ms | 10 |
| 200 schedules, 1 at a time | 906 | 54 ms | 453 ms | 696 ms | 0 |
| First run, 4 in parallel | 4,684 | 53 ms | 414 ms | 903 ms | 0 |

The machine was busy (load average 7 to 13 from other agents' builds) during
every run, so these times are indicative only, not a clean measure. The latency includes the
random Event Log delay (up to 30 ms per append, several appends per cancel)
and the model call delay. The cases over 1 s (10 in the mixed run, 15 in the gate run) were seen only
with 4 schedules in parallel on a loaded machine. The E7 line in `stress-tests.md` asks for
"cancellation settles within 1 s". This is **not proved**: repeat it in a quiet
slot with `E7_PARALLEL=1` and no slow log.

## Defects

### D1 (low, fixed): a cancel during prompt preparation ends the turn as `Failed`

- **Where.** `crates/loop-agent/src/native_backend.rs:1036-1041`. If a cancel
  arrives while `prompt` prepares the turn, the backend returns
  `BackendError::failed(ErrorCategory::Cancelled, "the turn was cancelled before it started")`.
  The Runtime prompt task mapped every `BackendError::Failed` to
  `TurnOutcome::Failed` (`crates/loop-runtime/src/runtime.rs:2552`). A cancel
  that arrives while the prompt is queued, or while the turn runs, gives
  `TurnOutcome::Cancelled`. So a Surface showed "failed" for a cancel the user
  asked for.
- **Effect.** Wrong outcome name only. The log is correct: the prompt has a
  `prompt_settled` record. The permit and `busy` flag are released. No lost
  state, no hang.
- **Frequency.** 5 of 30,772 acknowledged prompts in the first run. With a
  cancel straight after the prompt, 77 of 3,000.
- **Failing test.** `e7_defect_cancel_during_prompt_start_is_not_failed`
  (ignored). It failed before the fix and passes after it (commit `21e7c54`
  adds it, `7e5ae2b` fixes).
- **Fix** (`7e5ae2b`, four lines at `runtime.rs:2549`). A `Failed` with category
  `Cancelled` becomes `TurnOutcome::Cancelled`. I did not change
  `native_backend.rs`. All other non-ignored `loop-runtime` tests pass. The
  fix changes the outcome a Surface sees, so the supervisor can drop the
  commit. In the repo, the ACP backend already maps a `Cancelled` error
  category to a `Cancelled` outcome (`crates/extend/loop-acp/src/backend.rs:1067`),
  so the fix makes the native path agree with it. I found no code that
  branches on a `Failed` outcome with that category (search of
  `ErrorCategory::Cancelled` in `crates`).

### Notes (not defects)

- **N1. Shutdown fails queued prompts.** `shutdown` closes the permit queue
  first (`runtime.rs:1544`). A prompt that waits for a permit then ends as
  `Failed("semaphore closed")` and not `Cancelled`. This happened 70 times in
  158 schedules that shut down with turns in flight. The log is correct
  (`prompt_settled`). This is the same class as D1: a prompt ends as `Failed`
  on a stop the caller asked for, with an internal word in the text. I left it
  unfixed, because a stop of the whole Runtime may deserve a distinct outcome.
  Suggest `Cancelled` or a "Runtime stopped" message.
- **N2. At the Session limit.** `new_session` and `load_session` return `Busy`
  when every loaded Session is busy, watched, a Subagent, or has a parent
  waiting. They unload the least recently active idle Session otherwise. At
  quiescence with no watchers, `new_session` never refused (842 probes).
- **N3. Pre-existing lint failure, not caused by this branch.**
  `cargo clippy -p loop-runtime --all-targets --locked -- -D warnings` fails,
  with and without `--features orchestration`, with one error:
  `crates/loop-runtime/tests/e3_enforcement.rs:980` (`explicit_auto_deref`,
  clippy 1.98). `git diff origin/staging -- crates/loop-runtime/tests/e3_enforcement.rs`
  is empty, so the file is as on `staging`. I did not touch it. No other error
  appears, so `e7_turn_queue.rs` and `runtime.rs` are clean. I read this output
  on commit `c92df7b`. `cargo fmt --all -- --check` passes.
- **N4. Stderr noise.** The Runtime prints one line per unloaded Session. Run
  with `2>&1 | grep -v "unloading idle"`.

## What E7 does not cover

The E7 line in `docs/stress-tests.md` is wider than the task gate. Not done:

- A run with exactly 1 permit and nested children, parked approvals and queued
  root turns together. The probe uses 1 permit in some schedules (limits 2/1)
  and nested children in 392 schedules, but no parked approval (`full-access`
  mode raises no Escalation).
- Cancel at every waiting boundary, one by one. Cancels land at random times
  (queued, in a model call, in a tool, while a parent waits for a child).
- A cancel settle time within 1 s (see above).
- A synthetic FIFO run: no root turn waits more than twice its ideal queue
  time. Fairness is not measured. The harness proves liveness, not order.
- Event Log write errors. Only delays are injected.
- `release_turn_permit` and `reacquire_turn_permit` called from outside a turn.
  They break the limit on purpose, so they are not in the mix. The Subagent
  tool uses its own release and reacquire in `subagent.rs`, which the nested
  schedules cover.
- A real SQLite Event Log. `MemoryEventLog` is used. The queue logic does not
  depend on it, but write latency has a different shape.
- The "active turns at most 8" check is a lower bound while turns are cancelled
  (see the criteria table). The gated probe is exact at quiescence.

## Decision

- Mark A4 ("The turn queue has no deadlock") as **passed** at the task gate:
  the default-limit run, 1,000 of 1,000 schedules, seed 20261111, commit
  `62fa2ba` (permits: 849 of 849 probes).
- Keep the D1 fix. Keep `e7_turn_queue.rs` in `staging` as a regression test
  (ignored by default; the full run takes 7 to 9 minutes).
- Before the multi-Surface release, run E7 again in a quiet slot, with
  `E7_PARALLEL=1`, to settle the 1 s cancel line, and add the FIFO and
  parked-approval cases from the full E7 line.

## Commits on `probe/e7-turn-queue`

| Commit | Content |
|---|---|
| `46fd40e` | Harness `crates/loop-runtime/tests/e7_turn_queue.rs` |
| `21e7c54`, `2e9ed76` | Failing test for D1 |
| `7e5ae2b` | Fix for D1 in `runtime.rs` |
| `62fa2ba` | D1 test becomes a regression test; strict check restored |
| (this file) | `docs/probes/e7-results.md` |
