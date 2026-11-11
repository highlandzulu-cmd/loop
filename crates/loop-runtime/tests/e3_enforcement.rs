//! Probe E3, enforcement arm, "Off means unavailable" (H4).
//!
//! Each case builds a fresh agent directory and workspace, writes the
//! settings layers it needs (user `settings.json`, a trusted project
//! `.loop/settings.json`, user or project hook files, the `--permission-mode`
//! flag, a Session Policy layer), and then lets a scripted `faux` model make
//! tool calls on one execution path:
//!
//! - `embedded`: `SessionFactory::open` plus `AgentHarness::prompt`, with no
//!   approver. This is the stack `--print` and the TUI-embedded Session use
//!   (`loop_app_core::runtime::bootstrap`).
//! - `runtime`: a Runtime Session (`Runtime::new_session` and `prompt`).
//! - `subagent`: a Runtime parent calls the `subagent` tool, and the child
//!   makes the calls.
//! - `workflow`: `AgentHarness::start_workflow` with shell and agent tasks
//!   (only with `--features orchestration`; `loop-cli` does not enable it by
//!   default).
//! - `acp`: an external agent's permission request through `GatedHost`; a
//!   fake agent runs the command when Loop picks an allow option.
//!
//! Every case checks a real side effect (a marker file, file content, or a
//! token in a tool result), that a denial is reported (an error tool result,
//! a failed workflow task, a reject option), and the control cases check that
//! an allowed tool still works.
//!
//! ```text
//! E3_SEED=42 cargo test -p loop-runtime --features orchestration \
//!     --test e3_enforcement -- --ignored --nocapture --test-threads=4 e3_enforcement
//! ```
//!
//! `E3_FILTER` runs only the cases whose id contains it.
#![cfg(unix)]

use async_trait::async_trait;
use loop_ai::{
    providers::{faux_provider_fn, FauxResponse},
    Context, Message, ToolCall, ToolResultContent, UserContent, UserMessageContent,
};
use loop_app_core::permissions::{Capability, Layer, LayerSource, Setting, State};
use loop_app_core::runtime::{BootstrapOpts, GlobalResources, SessionFactory};
use loop_protocol::{
    AgentBackend, ContentBlock, HostServices, InteractionId, InteractionOutcome,
    InteractionRequest, PermissionKind, PermissionOption, SessionSpec, ToolCallId, ToolCallUpdate,
    ToolKind,
};
use loop_runtime::{log::MemoryEventLog, Runtime, RuntimeLimits};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

// ---------------------------------------------------------------- utilities

/// SplitMix64, as in E1.
#[derive(Clone)]
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            items.swap(i, j);
        }
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

const TURN_LIMIT: Duration = Duration::from_secs(8);

// ---------------------------------------------------------------- cases

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Route {
    Embedded,
    Runtime,
    Subagent,
    Workflow,
    Acp,
}
impl Route {
    fn name(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Runtime => "runtime",
            Self::Subagent => "subagent",
            Self::Workflow => "workflow",
            Self::Acp => "acp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    /// The effect must not happen and the refusal must be reported.
    Denied,
    /// Control: the effect must happen.
    Allowed,
}

/// Fixtures a cloned repository could carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prep {
    None,
    /// `{WS}/lnk -> {OUT}`.
    LinkOut,
    /// `{WS}/cfg -> {WS}/.loop`.
    LinkLoop,
    /// `{WS}/f.txt -> {OUT}/m.txt`, a dangling link.
    Dangling,
}

#[derive(Clone, Debug)]
enum Check {
    /// The templated path exists.
    Exists(String),
    /// The templated path contains the text.
    Contains(String, String),
    /// The text shows up in a tool result the model saw.
    Result(String),
}

#[derive(Clone, Debug)]
enum Work {
    /// The model's tool calls, one model response per entry.
    Calls(Vec<(String, Value)>),
    /// A workflow shell task.
    Shell(String),
    /// An ACP agent's permission request: the kind and the command it runs
    /// once allowed.
    Agent(ToolKind, String),
}

/// A second Session after the first turn: the agent's own changes to
/// settings or hooks must not widen it.
#[derive(Clone, Debug)]
struct Then {
    mode: &'static str,
    calls: Vec<(String, Value)>,
}

#[derive(Clone)]
struct Case {
    id: String,
    group: &'static str,
    route: Route,
    /// `--permission-mode`.
    mode: &'static str,
    /// Extra keys for the user `settings.json`.
    user: Value,
    /// A trusted project `.loop/settings.json`.
    project: Option<Value>,
    user_hook: Option<Value>,
    project_hook: Option<Value>,
    /// A Session Policy layer that turns a Capability off (embedded only).
    policy_off: Option<Capability>,
    /// Trust the workspace.
    trust: bool,
    prep: Prep,
    work: Work,
    then: Option<Then>,
    /// Revoke (switch to read-only) after the first model response.
    revoke: bool,
    check: Check,
    expect: Expect,
    note: &'static str,
}

impl Case {
    fn new(group: &'static str, route: Route, mode: &'static str, work: Work) -> Self {
        Self {
            id: String::new(),
            group,
            route,
            mode,
            user: json!({}),
            project: None,
            user_hook: None,
            project_hook: None,
            policy_off: None,
            trust: false,
            prep: Prep::None,
            work,
            then: None,
            revoke: false,
            check: Check::Exists("{M}".into()),
            expect: Expect::Denied,
            note: "",
        }
    }
    fn user(mut self, v: Value) -> Self {
        self.user = v;
        self
    }
    fn project(mut self, v: Value) -> Self {
        self.project = Some(v);
        self.trust = true;
        self
    }
    fn user_hook(mut self, v: Value) -> Self {
        self.user_hook = Some(v);
        self
    }
    fn project_hook(mut self, v: Value) -> Self {
        self.project_hook = Some(v);
        self.trust = true;
        self
    }
    fn policy_off(mut self, c: Capability) -> Self {
        self.policy_off = Some(c);
        self
    }
    fn trust(mut self) -> Self {
        self.trust = true;
        self
    }
    fn prep(mut self, p: Prep) -> Self {
        self.prep = p;
        self
    }
    fn then(mut self, t: Then) -> Self {
        self.then = Some(t);
        self
    }
    fn revoke(mut self) -> Self {
        self.revoke = true;
        self
    }
    fn check(mut self, c: Check) -> Self {
        self.check = c;
        self
    }
    fn allowed(mut self) -> Self {
        self.expect = Expect::Allowed;
        self
    }
    fn note(mut self, n: &'static str) -> Self {
        self.note = n;
        self
    }
}

fn bash(cmd: &str) -> Work {
    Work::Calls(vec![("bash".into(), json!({ "command": cmd }))])
}
fn write(path: &str) -> Work {
    Work::Calls(vec![(
        "write".into(),
        json!({ "path": path, "content": "E3MARK" }),
    )])
}
fn write_text(path: &str, content: Value) -> Work {
    Work::Calls(vec![(
        "write".into(),
        json!({ "path": path, "content": content.to_string() }),
    )])
}
fn read(path: &str) -> Work {
    Work::Calls(vec![("read".into(), json!({ "path": path }))])
}
fn touch() -> Work {
    bash("touch {M}")
}

/// Hook files: block every execute call; block writes; allow everything.
fn hook_block_execute() -> Value {
    json!({"name": "e3-no-exec", "on": "tool_call", "match": {"kind": "execute"},
           "action": {"block": true, "reason": "E3 hook blocks commands"}})
}
fn hook_block_write() -> Value {
    json!({"name": "e3-no-write", "on": "tool_call", "match": {"tool": "write|edit"},
           "action": {"block": true, "reason": "E3 hook blocks writes"}})
}
fn hook_block_touch() -> Value {
    json!({"name": "e3-no-touch", "on": "tool_call",
           "match": {"tool": "bash", "args": {"command": "*touch*"}},
           "action": {"block": true, "reason": "E3 hook blocks touch"}})
}
fn hook_allow_all() -> Value {
    json!({"name": "e3-allow", "on": "tool_call", "action": {"allow": true}})
}

/// The cases. About 100; the order is shuffled by the seed.
fn cases() -> Vec<Case> {
    use Route::*;
    let mut v: Vec<Case> = Vec::new();
    let deny_bash = json!({"toolPermissions": {"bash": "deny"}});
    let deny_write = json!({"toolPermissions": {"write": "deny", "edit": "deny"}});
    let deny_touch = json!({"toolPermissions": {"bash(touch*)": "deny"}});

    // ---- A. Mode ceilings, on the three chat paths.
    for route in [Embedded, Runtime, Subagent] {
        for mode in ["read-only", "plan"] {
            v.push(Case::new("mode", route, mode, touch()).note("shell off"));
            v.push(
                Case::new("mode", route, mode, write("{WS}/m.txt"))
                    .check(Check::Exists("{WS}/m.txt".into())),
            );
            v.push(
                Case::new("mode", route, mode, read("{WS}/token.txt"))
                    .check(Check::Result("E3TOKEN".into()))
                    .allowed()
                    .note("control: read works"),
            );
        }
        v.push(
            Case::new("mode", route, "accept-edits", write("{WS}/m.txt"))
                .check(Check::Exists("{WS}/m.txt".into()))
                .allowed()
                .note("control: edit in workspace"),
        );
        v.push(
            Case::new("mode", route, "full-access", touch())
                .allowed()
                .note("control: shell"),
        );
    }
    // Ask and accept-edits headless refusals (embedded = --print).
    v.push(Case::new("mode", Embedded, "ask", touch()).note("ask, nobody to ask"));
    v.push(
        Case::new("mode", Embedded, "ask", write("{WS}/m.txt"))
            .check(Check::Exists("{WS}/m.txt".into())),
    );
    v.push(Case::new("mode", Embedded, "accept-edits", touch()).note("bash asks in accept-edits"));
    v.push(
        Case::new(
            "mode",
            Embedded,
            "plan",
            Work::Calls(vec![
                ("exit_plan".into(), json!({"plan": "x"})),
                ("bash".into(), json!({"command": "touch {M}"})),
            ]),
        )
        .note("exit_plan has nobody to approve, then a command"),
    );

    // ---- B. Path tricks for file tools in accept-edits (embedded, runtime).
    for route in [Embedded, Runtime] {
        let m = "accept-edits";
        v.push(
            Case::new("path", route, m, write("{OUT}/m.txt"))
                .check(Check::Exists("{OUT}/m.txt".into()))
                .note("absolute outside"),
        );
        v.push(
            Case::new("path", route, m, write("../outside/m.txt"))
                .check(Check::Exists("{OUT}/m.txt".into()))
                .note("`..` outside"),
        );
        v.push(
            Case::new("path", route, m, write("{WS}/sub/../../outside/m.txt"))
                .check(Check::Exists("{OUT}/m.txt".into()))
                .note("`..` after a missing dir"),
        );
        v.push(
            Case::new("path", route, m, write("lnk/m.txt"))
                .prep(Prep::LinkOut)
                .check(Check::Exists("{OUT}/m.txt".into()))
                .note("dir symlink to outside"),
        );
        v.push(
            Case::new("path", route, m, write("f.txt"))
                .prep(Prep::Dangling)
                .check(Check::Exists("{OUT}/m.txt".into()))
                .note("dangling file symlink to outside"),
        );
        v.push(
            Case::new("path", route, m, write(".loop/settings.json"))
                .check(Check::Exists("{WS}/.loop/settings.json".into()))
                .note("protected .loop"),
        );
        v.push(
            Case::new("path", route, m, write(".git/hooks/pre-commit"))
                .check(Check::Exists("{WS}/.git/hooks/pre-commit".into()))
                .note("protected .git"),
        );
        v.push(
            Case::new("path", route, m, write(".env"))
                .check(Check::Exists("{WS}/.env".into()))
                .note("protected .env"),
        );
        v.push(
            Case::new("path", route, m, write("cfg/settings.json"))
                .prep(Prep::LinkLoop)
                .check(Check::Exists("{WS}/.loop/settings.json".into()))
                .note("symlink into .loop"),
        );
        v.push(
            Case::new("path", route, m, write(".GIT/hooks/pre-commit"))
                .check(Check::Exists("{WS}/.git/hooks/pre-commit".into()))
                .note("case: .GIT on a case-insensitive disk"),
        );
        v.push(
            Case::new("path", route, m, write(".LOOP/settings.json"))
                .check(Check::Exists("{WS}/.loop/settings.json".into()))
                .note("case: .LOOP on a case-insensitive disk"),
        );
    }

    // ---- C. Deny rules and switches, every layer (full-access, so only
    // the rule stops the call).
    for route in [Embedded, Runtime, Subagent] {
        v.push(
            Case::new("rule", route, "full-access", touch())
                .user(deny_bash.clone())
                .note("user bash deny"),
        );
        v.push(
            Case::new("rule", route, "full-access", write("{WS}/m.txt"))
                .user(deny_write.clone())
                .check(Check::Exists("{WS}/m.txt".into()))
                .note("user write deny"),
        );
        v.push(
            Case::new("rule", route, "full-access", touch())
                .user(deny_write.clone())
                .allowed()
                .note("control: bash with write denied"),
        );
        v.push(
            Case::new("rule", route, "full-access", touch())
                .project(deny_bash.clone())
                .note("project bash deny"),
        );
        v.push(
            Case::new("rule", route, "full-access", touch())
                .user(deny_touch.clone())
                .note("pattern deny, plain"),
        );
    }
    v.push(
        Case::new("rule", Embedded, "ask", touch())
            .project(json!({"toolPermissions": {"bash": "allow"}}))
            .note("project allow cannot loosen"),
    );
    v.push(
        Case::new("rule", Embedded, "ask", touch())
            .project(json!({"permissionMode": "full-access"}))
            .note("project mode cannot loosen"),
    );
    v.push(
        Case::new("rule", Embedded, "full-access", touch())
            .user(json!({"permissionMode": "read-only"}))
            .allowed()
            .note("CLI flag beats the user setting (control)"),
    );
    v.push(Case::new("rule", Embedded, "full-access", touch()).project(json!({"permissionMode": "read-only"})).allowed()
        .note("project read-only vs CLI full-access: the flag wins (documented: project only tightens the setting)"));
    v.push(
        Case::new("rule", Embedded, "full-access", touch())
            .policy_off(Capability::Shell)
            .note("Session Policy turns the shell off"),
    );
    v.push(
        Case::new("rule", Embedded, "full-access", write("{WS}/m.txt"))
            .policy_off(Capability::WriteFiles)
            .check(Check::Exists("{WS}/m.txt".into()))
            .note("Session Policy turns writes off"),
    );
    v.push(
        Case::new("rule", Embedded, "full-access", write("{WS}/m.txt"))
            .policy_off(Capability::Shell)
            .check(Check::Exists("{WS}/m.txt".into()))
            .allowed()
            .note("control: write with shell off"),
    );
    v.push(
        Case::new("rule", Embedded, "full-access", touch())
            .policy_off(Capability::Shell)
            .user_hook(hook_allow_all())
            .note("hook allow vs Policy off"),
    );
    v.push(
        Case::new(
            "rule",
            Subagent,
            "full-access",
            Work::Calls(vec![("subagent".into(), json!({"task": "E3CHILD"}))]),
        )
        .user(json!({"toolPermissions": {"subagent": "deny"}}))
        .note("subagent tool denied (child would touch)"),
    );

    // ---- D. Shell wrappers around a pattern deny `bash(touch*)` and a
    // hook that blocks `*touch*` (full-access, embedded).
    let wrappers: &[(&str, &str)] = &[
        ("bash -c 'touch {M}'", "bash -c"),
        ("sh -c \"touch {M}\"", "sh -c"),
        ("env touch {M}", "env"),
        ("command touch {M}", "command"),
        ("/usr/bin/touch {M}", "absolute program path"),
        ("TOUCH {M}", "upper case program (case-insensitive disk)"),
        ("tou''ch {M}", "empty quotes in the word"),
        ("t\\ouch {M}", "backslash in the word"),
        ("echo x && touch {M}", "second part"),
        (
            "echo \\' ; touch {M} ; echo \\'",
            "escaped quote hides the split",
        ),
        ("printf 'touch {M}' | sh", "pipe into sh"),
        ("$(printf tou)ch {M}", "command substitution"),
    ];
    for (cmd, note) in wrappers {
        v.push(
            Case::new("wrap-rule", Embedded, "full-access", bash(cmd))
                .user(deny_touch.clone())
                .note(note),
        );
    }
    for (cmd, note) in [wrappers[6], wrappers[7], wrappers[10], wrappers[11]] {
        v.push(
            Case::new("wrap-hook", Embedded, "full-access", bash(cmd))
                .user_hook(hook_block_touch())
                .note(note),
        );
    }
    v.push(
        Case::new("wrap-rule", Embedded, "full-access", bash("tou''ch {M}"))
            .user(json!({"toolPermissions": {"bash(*touch*)": "deny"}}))
            .note("pattern *touch* vs empty quotes"),
    );

    // ---- E. read-only shell classifier (read-only / plan, three paths).
    let ro: &[(&str, &str)] = &[
        (
            "echo \\' ; touch {M} ; echo \\'",
            "escaped single quote hides `;`",
        ),
        (
            "echo \\\" ; touch {M} ; echo \\\"",
            "escaped double quote hides `;`",
        ),
        ("ls; touch {M}", "second part"),
        ("ls\ntouch {M}", "newline"),
        ("echo x > {M}", "redirect"),
        ("echo $(touch {M})", "substitution"),
        ("find . -maxdepth 0 -fprint {M}", "find -fprint"),
        ("sort -o {M} token.txt", "sort -o"),
        ("git diff --output={M}", "git --output"),
        ("git -c core.pager='touch {M}' log", "git -c"),
        ("cat token.txt | tee {M}", "tee"),
        ("tree -o {M}", "tree -o"),
    ];
    for (cmd, note) in ro {
        v.push(Case::new("readonly-sh", Embedded, "read-only", bash(cmd)).note(note));
    }
    for route in [Runtime, Subagent] {
        v.push(Case::new("readonly-sh", route, "read-only", bash(ro[0].0)).note(ro[0].1));
        v.push(Case::new("readonly-sh", route, "plan", bash(ro[1].0)).note(ro[1].1));
        v.push(Case::new("readonly-sh", route, "read-only", bash(ro[2].0)).note(ro[2].1));
    }
    v.push(
        Case::new("readonly-sh", Embedded, "read-only", bash("cat token.txt"))
            .check(Check::Result("E3TOKEN".into()))
            .allowed()
            .note("control: read-only command runs"),
    );

    // ---- F. Hooks that deny.
    for route in [Embedded, Runtime, Subagent] {
        v.push(
            Case::new("hook", route, "full-access", touch())
                .user_hook(hook_block_execute())
                .note("user hook blocks execute"),
        );
        v.push(
            Case::new("hook", route, "full-access", write("{WS}/m.txt"))
                .user_hook(hook_block_write())
                .check(Check::Exists("{WS}/m.txt".into()))
                .note("user hook blocks write"),
        );
    }
    v.push(
        Case::new("hook", Embedded, "full-access", touch())
            .project_hook(hook_block_execute())
            .note("project hook blocks execute"),
    );
    v.push(
        Case::new("hook", Embedded, "full-access", write("{WS}/m.txt"))
            .user_hook(hook_block_execute())
            .check(Check::Exists("{WS}/m.txt".into()))
            .allowed()
            .note("control: write with exec hook"),
    );
    v.push(
        Case::new("hook", Embedded, "read-only", touch())
            .user_hook(hook_allow_all())
            .note("hook allow vs read-only"),
    );
    v.push(
        Case::new("hook", Embedded, "full-access", touch())
            .user(deny_bash.clone())
            .user_hook(hook_allow_all())
            .note("hook allow vs deny rule"),
    );
    v.push(
        Case::new("hook", Embedded, "ask", touch())
            .user_hook(hook_allow_all())
            .allowed()
            .note("control: hook allow lifts ask"),
    );

    // ---- G. The agent changes its own settings, then a new Session.
    let next_touch = Then {
        mode: "full-access",
        calls: vec![("bash".into(), json!({"command": "touch {M}"}))],
    };
    let next_read = Then {
        mode: "full-access",
        calls: vec![("read".into(), json!({"path": "{WS}/token.txt"}))],
    };
    v.push(
        Case::new(
            "self",
            Embedded,
            "full-access",
            write_text(
                "{WS}/.loop/settings.json",
                json!({"toolPermissions": {"bash": "allow"}, "permissionMode": "full-access"}),
            ),
        )
        .user(deny_bash.clone())
        .trust()
        .then(next_touch.clone())
        .note("project settings re-allow bash"),
    );
    v.push(
        Case::new(
            "self",
            Embedded,
            "full-access",
            write_text(
                "{AG}/settings.json",
                json!({"toolPermissions": {"bash": "allow"}}),
            ),
        )
        .user(deny_bash.clone())
        .then(next_touch.clone())
        .note("user settings rewritten by the write tool"),
    );
    v.push(Case::new("self", Embedded, "full-access",
        write_text("{WS}/.loop/hooks/e3.json", json!({"name": "e3", "on": "tool_call", "action": {"command": ["sh", "-c", "touch {M}"]}})))
        .user(deny_bash.clone()).trust().then(next_read.clone()).note("project command hook runs a shell"));
    v.push(
        Case::new(
            "self",
            Embedded,
            "full-access",
            write_text(
                "{WS}/.loop/settings.json",
                json!({"mcpServers": {"e3": {"command": "sh", "args": ["-c", "touch {M}"]}}}),
            ),
        )
        .user(deny_bash.clone())
        .trust()
        .then(next_read.clone())
        .note("project MCP server command"),
    );
    v.push(Case::new("self", Embedded, "full-access",
        write_text("{AG}/hooks/e3.json", json!({"name": "e3", "on": "tool_call", "action": {"command": ["sh", "-c", "touch {M}"]}})))
        .user(deny_bash.clone()).then(next_read.clone()).note("user command hook runs a shell"));
    v.push(
        Case::new(
            "self",
            Embedded,
            "accept-edits",
            write_text(
                ".loop/settings.json",
                json!({"toolPermissions": {"bash": "allow"}}),
            ),
        )
        .trust()
        .check(Check::Exists("{WS}/.loop/settings.json".into()))
        .note("accept-edits cannot write .loop"),
    );
    v.push(
        Case::new(
            "self",
            Embedded,
            "accept-edits",
            write_text(
                "{AG}/settings.json",
                json!({"toolPermissions": {"bash": "allow"}}),
            ),
        )
        .check(Check::Contains("{AG}/settings.json".into(), "allow".into()))
        .note("accept-edits cannot write user settings"),
    );

    // ---- H. Revocation during work.
    let two = Work::Calls(vec![
        ("bash".into(), json!({"command": "touch {WS}/first"})),
        ("bash".into(), json!({"command": "touch {M}"})),
    ]);
    v.push(
        Case::new("revoke", Embedded, "full-access", two.clone())
            .revoke()
            .note("mode to read-only mid-turn"),
    );
    v.push(
        Case::new("revoke", Runtime, "full-access", two.clone())
            .revoke()
            .note("mode to read-only mid-turn"),
    );

    // ---- I. Workflow tasks.
    v.push(
        Case::new(
            "workflow",
            Workflow,
            "read-only",
            Work::Shell("touch {M}".into()),
        )
        .note("shell task in read-only"),
    );
    v.push(
        Case::new(
            "workflow",
            Workflow,
            "full-access",
            Work::Shell("touch {M}".into()),
        )
        .user(deny_bash.clone())
        .note("shell task, bash deny"),
    );
    v.push(
        Case::new(
            "workflow",
            Workflow,
            "full-access",
            Work::Shell("touch {M}".into()),
        )
        .user_hook(hook_block_execute())
        .note("shell task, hook"),
    );
    v.push(
        Case::new(
            "workflow",
            Workflow,
            "read-only",
            Work::Shell("echo \\' ; touch {M} ; echo \\'".into()),
        )
        .note("shell task, escaped quote"),
    );
    v.push(Case::new("workflow", Workflow, "read-only", touch()).note("agent task in read-only"));
    v.push(
        Case::new(
            "workflow",
            Workflow,
            "full-access",
            Work::Shell("touch {M}".into()),
        )
        .allowed()
        .note("control: shell task"),
    );

    // ---- J. ACP agent permission requests.
    v.push(
        Case::new(
            "acp",
            Acp,
            "read-only",
            Work::Agent(ToolKind::Execute, "touch {M}".into()),
        )
        .note("read-only rejects"),
    );
    v.push(
        Case::new(
            "acp",
            Acp,
            "plan",
            Work::Agent(ToolKind::Edit, "touch {M}".into()),
        )
        .note("plan rejects edits"),
    );
    v.push(
        Case::new(
            "acp",
            Acp,
            "full-access",
            Work::Agent(ToolKind::Execute, "touch {M}".into()),
        )
        .user_hook(hook_block_execute())
        .note("hook blocks"),
    );
    v.push(
        Case::new(
            "acp",
            Acp,
            "full-access",
            Work::Agent(ToolKind::Execute, "touch {M}".into()),
        )
        .user(deny_bash.clone())
        .note("bash deny rule"),
    );
    v.push(
        Case::new(
            "acp",
            Acp,
            "read-only",
            Work::Agent(ToolKind::Execute, "touch {M}".into()),
        )
        .user_hook(hook_allow_all())
        .note("hook allow vs read-only"),
    );
    v.push(
        Case::new(
            "acp",
            Acp,
            "full-access",
            Work::Agent(ToolKind::Execute, "touch {M}".into()),
        )
        .policy_off(Capability::Shell)
        .note("Policy shell off"),
    );
    v.push(
        Case::new(
            "acp",
            Acp,
            "full-access",
            Work::Agent(ToolKind::Execute, "touch {M}".into()),
        )
        .allowed()
        .note("control: full-access allows once"),
    );

    for (i, case) in v.iter_mut().enumerate() {
        case.id = format!(
            "{:03}-{}-{}-{}",
            i,
            case.group,
            case.route.name(),
            case.mode
        );
    }
    v
}

// ---------------------------------------------------------------- fixture

struct Dirs {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    agent: PathBuf,
    ws: PathBuf,
    out: PathBuf,
    marker: PathBuf,
}

impl Dirs {
    fn new(base: &Path, case: &Case) -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("e3-")
            .tempdir_in(base)
            .unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let agent = root.join("agent");
        let ws = root.join("ws");
        let out = root.join("outside");
        for d in [&agent, &ws, &out, &agent.join("hooks")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&ws)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            std::fs::create_dir_all(ws.join(".git")).unwrap();
        }
        std::fs::write(ws.join("token.txt"), "E3TOKEN\n").unwrap();
        let marker = out.join(format!("mark-{}", &case.id[..3]));
        let dirs = Self {
            _tmp: tmp,
            root,
            agent,
            ws,
            out,
            marker,
        };
        match case.prep {
            Prep::None => {}
            Prep::LinkOut => std::os::unix::fs::symlink(&dirs.out, dirs.ws.join("lnk")).unwrap(),
            Prep::LinkLoop => {
                std::fs::create_dir_all(dirs.ws.join(".loop")).unwrap();
                std::os::unix::fs::symlink(dirs.ws.join(".loop"), dirs.ws.join("cfg")).unwrap()
            }
            Prep::Dangling => {
                std::os::unix::fs::symlink(dirs.out.join("m.txt"), dirs.ws.join("f.txt")).unwrap()
            }
        }
        // A no-op diff editor: the Runtime approver must never launch a GUI.
        let mut user = json!({"diffEditor": "/usr/bin/true"});
        if let Some(extra) = case.user.as_object() {
            for (k, v) in extra {
                user[k] = dirs.fill(v);
            }
        }
        std::fs::write(dirs.agent.join("settings.json"), user.to_string()).unwrap();
        if let Some(project) = &case.project {
            std::fs::create_dir_all(dirs.ws.join(".loop")).unwrap();
            std::fs::write(
                dirs.ws.join(".loop/settings.json"),
                dirs.fill(project).to_string(),
            )
            .unwrap();
        }
        if let Some(hook) = &case.user_hook {
            std::fs::write(
                dirs.agent.join("hooks/e3.json"),
                dirs.fill(hook).to_string(),
            )
            .unwrap();
        }
        if let Some(hook) = &case.project_hook {
            std::fs::create_dir_all(dirs.ws.join(".loop/hooks")).unwrap();
            std::fs::write(
                dirs.ws.join(".loop/hooks/e3.json"),
                dirs.fill(hook).to_string(),
            )
            .unwrap();
        }
        if case.trust {
            let mut trust = loop_app_core::config::TrustStore::load(
                loop_app_core::config::paths::trust_path(&dirs.agent),
            )
            .unwrap();
            trust.set(&dirs.ws, true).unwrap();
        }
        dirs
    }

    fn text(&self, s: &str) -> String {
        s.replace("{M}", &self.marker.to_string_lossy())
            .replace("{WS}", &self.ws.to_string_lossy())
            .replace("{OUT}", &self.out.to_string_lossy())
            .replace("{AG}", &self.agent.to_string_lossy())
    }

    fn fill(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.fill(x)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (self.text(k), self.fill(x)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    fn calls(&self, calls: &[(String, Value)]) -> Vec<(String, Value)> {
        calls
            .iter()
            .map(|(t, a)| (t.clone(), self.fill(a)))
            .collect()
    }

    fn opts(&self, mode: &str) -> BootstrapOpts {
        BootstrapOpts {
            cwd: self.ws.clone(),
            provider: Some("faux".into()),
            model: Some("faux-model".into()),
            theme: None,
            system_prompt: Some("E3".into()),
            append_system_prompt: None,
            no_context_files: true,
            interactive: false,
            session_id: None,
            no_web: true,
            permission_mode: Some(mode.into()),
            agent: None,
        }
    }
}

// ---------------------------------------------------------------- faux model

/// What one task (named by its first prompt) saw.
#[derive(Default, Clone, Debug)]
struct Seen {
    tools: Vec<String>,
    /// (tool, is_error, text), newest last.
    results: Vec<(String, bool, String)>,
}

type SeenMap = Arc<Mutex<HashMap<String, Seen>>>;
type Hook = Arc<dyn Fn(&str, usize) + Send + Sync>;

fn user_text(message: &Message) -> Option<String> {
    let Message::User(user) = message else {
        return None;
    };
    Some(match &user.content {
        UserMessageContent::Text(text) => text.clone(),
        UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                UserContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect(),
    })
}

/// A model scripted by task: `script[task][n]` is the n-th response. The
/// task is the script key the first user message contains.
fn model(
    script: BTreeMap<String, Vec<Vec<(String, Value)>>>,
    seen: SeenMap,
    on_call: Option<Hook>,
) -> loop_ai::providers::FauxResponder {
    Arc::new(move |context: &Context| {
        let first = context
            .messages
            .iter()
            .find_map(user_text)
            .unwrap_or_default();
        let task = script
            .keys()
            .filter(|k| first.contains(k.as_str()))
            .max_by_key(|k| k.len())
            .cloned()
            .unwrap_or_default();
        let n = context
            .messages
            .iter()
            .filter(|m| matches!(m, Message::Assistant(_)))
            .count();
        {
            let mut seen = seen.lock().unwrap();
            let entry = seen.entry(task.clone()).or_default();
            entry.tools = context
                .tools
                .iter()
                .flatten()
                .map(|t| t.name.clone())
                .collect();
            entry.results = context
                .messages
                .iter()
                .filter_map(|m| match m {
                    Message::ToolResult(r) => Some((
                        r.tool_name.clone(),
                        r.is_error,
                        r.content
                            .iter()
                            .filter_map(|c| match c {
                                ToolResultContent::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(""),
                    )),
                    _ => None,
                })
                .collect();
        }
        if let Some(hook) = &on_call {
            hook(&task, n);
        }
        let response = match script.get(&task).and_then(|s| s.get(n)) {
            Some(calls) => FauxResponse::ToolCalls(
                calls
                    .iter()
                    .enumerate()
                    .map(|(i, (name, arguments))| ToolCall {
                        id: format!("e3-{n}-{i}"),
                        name: name.clone(),
                        arguments: arguments.clone(),
                        thought_signature: None,
                    })
                    .collect(),
            ),
            None => FauxResponse::Text(format!("done {task}")),
        };
        Box::pin(std::future::ready(response))
    })
}

fn globals(dirs: &Dirs, responder: loop_ai::providers::FauxResponder) -> GlobalResources {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(responder));
    GlobalResources::with_models(dirs.agent.clone(), models).unwrap()
}

// ---------------------------------------------------------------- runs

#[derive(Debug, Default)]
struct Outcome {
    /// The forbidden (or, for a control, the wanted) effect happened.
    effect: bool,
    /// A refusal was reported to the model (or the workflow, or the agent).
    reported: bool,
    /// The off tool was still offered to the model.
    offered: Option<bool>,
    /// Milliseconds from revocation to the refused call.
    revoke_ms: Option<u128>,
    detail: String,
}

fn effect(dirs: &Dirs, check: &Check, seen: &HashMap<String, Seen>) -> bool {
    match check {
        Check::Exists(p) => {
            let p = PathBuf::from(dirs.text(p));
            std::fs::symlink_metadata(&p).is_ok() && std::fs::metadata(&p).is_ok()
                // A dangling symlink planted by Prep is not the effect.
                && !(p.is_symlink() && std::fs::read_to_string(&p).map(|s| s.is_empty()).unwrap_or(true))
        }
        Check::Contains(p, text) => std::fs::read_to_string(dirs.text(p))
            .map(|s| s.contains(text.as_str()))
            .unwrap_or(false),
        Check::Result(text) => seen
            .values()
            .flat_map(|s| s.results.iter())
            .any(|(_, err, t)| !err && t.contains(text.as_str())),
    }
}

fn reported(seen: &HashMap<String, Seen>, task: &str) -> (bool, String) {
    let Some(s) = seen.get(task) else {
        return (false, "task never called the model".into());
    };
    let errors: Vec<&(String, bool, String)> = s.results.iter().filter(|r| r.1).collect();
    let detail = s
        .results
        .iter()
        .map(|(t, e, x)| {
            format!(
                "{t}{}: {}",
                if *e { "!" } else { "" },
                x.chars().take(140).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    (!errors.is_empty(), detail)
}

async fn run_case(base: &Path, case: &Case) -> Outcome {
    let dirs = Dirs::new(base, case);
    let seen: SeenMap = Arc::default();
    let task = if case.route == Route::Subagent {
        "E3CHILD"
    } else {
        "E3TASK"
    };
    let mut script: BTreeMap<String, Vec<Vec<(String, Value)>>> = BTreeMap::new();
    let calls = match &case.work {
        Work::Calls(c) => dirs.calls(c),
        _ => vec![],
    };
    // One response per call, so a refusal of the first is seen before the
    // second is made.
    script.insert(
        task.into(),
        calls.iter().cloned().map(|c| vec![c]).collect(),
    );
    if case.route == Route::Subagent {
        script.insert(
            "E3ROOT".into(),
            vec![vec![(
                "subagent".into(),
                json!({"task": "E3CHILD do the work"}),
            )]],
        );
    }
    if case.id.contains("-rule-subagent-") && case.note.starts_with("subagent tool denied") {
        // The child would run `touch`; the parent's call is the denied one.
        script.insert(
            "E3CHILD".into(),
            vec![vec![(
                "bash".into(),
                json!({"command": dirs.text("touch {M}")}),
            )]],
        );
        script.insert(
            "E3TASK".into(),
            vec![vec![(
                "subagent".into(),
                json!({"task": "E3CHILD do the work"}),
            )]],
        );
    }
    if let Some(then) = &case.then {
        script.insert(
            "E3NEXT".into(),
            dirs.calls(&then.calls)
                .into_iter()
                .map(|c| vec![c])
                .collect(),
        );
    }

    let mut out = Outcome::default();
    let revoked_at: Arc<Mutex<Option<Instant>>> = Arc::default();
    match case.route {
        Route::Embedded => {
            let gate_slot: Arc<Mutex<Option<Arc<loop_app_core::tool_gate::ToolGate>>>> =
                Arc::default();
            let hook: Option<Hook> = case.revoke.then(|| {
                let slot = gate_slot.clone();
                let at = revoked_at.clone();
                Arc::new(move |_task: &str, n: usize| {
                    if n == 1 {
                        if let Some(gate) = slot.lock().unwrap().as_ref() {
                            gate.set_mode(loop_app_core::permissions::PermissionMode::ReadOnly);
                            *at.lock().unwrap() = Some(Instant::now());
                        }
                    }
                }) as Hook
            });
            let factory = SessionFactory::new(Arc::new(globals(
                &dirs,
                model(script.clone(), seen.clone(), hook),
            )));
            let opts = dirs.opts(case.mode);
            match open_embedded(&factory, &opts, case).await {
                Ok(opened) => {
                    *gate_slot.lock().unwrap() = Some(opened.tool_gate.clone());
                    let r =
                        tokio::time::timeout(TURN_LIMIT, opened.harness.prompt("E3TASK go")).await;
                    if r.is_err() {
                        out.detail.push_str("[turn timed out] ");
                    }
                }
                Err(e) => out.detail.push_str(&format!("[open failed: {e}] ")),
            }
            if let Some(then) = &case.then {
                let opts = dirs.opts(then.mode);
                // A new process: settings and hooks are read again.
                let factory = SessionFactory::new(Arc::new(globals(
                    &dirs,
                    model(script.clone(), seen.clone(), None),
                )));
                match open_embedded(
                    &factory,
                    &opts,
                    &Case {
                        policy_off: None,
                        ..case.clone()
                    },
                )
                .await
                {
                    Ok(opened) => {
                        let _ =
                            tokio::time::timeout(TURN_LIMIT, opened.harness.prompt("E3NEXT go"))
                                .await;
                    }
                    Err(e) => out.detail.push_str(&format!("[second open failed: {e}] ")),
                }
            }
        }
        Route::Runtime | Route::Subagent => {
            let rt_slot: RuntimeSlot = Arc::default();
            let hook: Option<Hook> = case.revoke.then(|| {
                let slot = rt_slot.clone();
                let at = revoked_at.clone();
                Arc::new(move |_task: &str, n: usize| {
                    if n == 1 {
                        if let Some((rt, s)) = slot.lock().unwrap().clone() {
                            let at = at.clone();
                            // The model call waits for the switch.
                            let handle = tokio::runtime::Handle::current();
                            std::thread::spawn(move || {
                                let r = handle.block_on(loop_wire::RuntimeAdmin::permission_mode(
                                    rt.as_ref(),
                                    loop_protocol::admin::PermissionModeParams {
                                        session: s,
                                        mode: Some("read-only".into()),
                                        expected_revision: None,
                                    },
                                ));
                                if r.is_ok() {
                                    *at.lock().unwrap() = Some(Instant::now());
                                } else {
                                    eprintln!("E3|revoke refused: {r:?}");
                                }
                            })
                            .join()
                            .unwrap();
                        }
                    }
                }) as Hook
            });
            let g = globals(&dirs, model(script.clone(), seen.clone(), hook));
            let log = Arc::new(MemoryEventLog::new());
            match Runtime::new(g, dirs.opts(case.mode), log, RuntimeLimits::default()).await {
                Ok(rt) => {
                    let rt = rt.into_shared();
                    let spec = SessionSpec {
                        cwd: Some(dirs.ws.clone()),
                        ..Default::default()
                    };
                    match rt.new_session(spec).await {
                        Ok(session) => {
                            *rt_slot.lock().unwrap() = Some((rt.clone(), session.clone()));
                            let first = if case.route == Route::Subagent {
                                "E3ROOT go"
                            } else {
                                "E3TASK go"
                            };
                            let turn = rt.prompt(&session, vec![ContentBlock::text(first)]).await;
                            match turn {
                                Ok(turn) => {
                                    if tokio::time::timeout(TURN_LIMIT, turn.outcome())
                                        .await
                                        .is_err()
                                    {
                                        out.detail
                                            .push_str("[turn timed out: held for approval?] ");
                                        rt.cancel(&session);
                                    }
                                }
                                Err(e) => out.detail.push_str(&format!("[prompt failed: {e}] ")),
                            }
                        }
                        Err(e) => out.detail.push_str(&format!("[session failed: {e}] ")),
                    }
                    *rt_slot.lock().unwrap() = None;
                    let _ = tokio::time::timeout(Duration::from_secs(5), rt.shutdown()).await;
                }
                Err(e) => out.detail.push_str(&format!("[runtime failed: {e}] ")),
            }
        }
        Route::Workflow => {
            #[cfg(feature = "orchestration")]
            {
                let factory = SessionFactory::new(Arc::new(globals(
                    &dirs,
                    model(script.clone(), seen.clone(), None),
                )));
                let opts = dirs.opts(case.mode);
                match open_embedded(&factory, &opts, case).await {
                    Ok(opened) => {
                        let mut planner = loop_orchestration::planner::ManualPlanner::new();
                        match &case.work {
                            Work::Shell(cmd) => {
                                planner.add_shell_command("e3 shell", dirs.text(cmd));
                            }
                            _ => {
                                planner.add_agent_turn("e3 agent", "E3TASK go");
                            }
                        }
                        let graph = planner.build().unwrap();
                        match tokio::time::timeout(
                            TURN_LIMIT,
                            opened.harness.start_workflow(graph, None, None),
                        )
                        .await
                        {
                            Ok(Ok(result)) => {
                                if matches!(case.work, Work::Shell(_)) {
                                    out.reported = !result.failed_tasks.is_empty();
                                    out.detail
                                        .push_str(&format!("failed={:?} ", result.failed_tasks));
                                }
                            }
                            Ok(Err(e)) => out.detail.push_str(&format!("[workflow error: {e}] ")),
                            Err(_) => out.detail.push_str("[workflow timed out] "),
                        }
                    }
                    Err(e) => out.detail.push_str(&format!("[open failed: {e}] ")),
                }
            }
            #[cfg(not(feature = "orchestration"))]
            out.detail
                .push_str("[skipped: build without --features orchestration] ");
        }
        Route::Acp => {
            let factory = SessionFactory::new(Arc::new(globals(
                &dirs,
                model(script.clone(), seen.clone(), None),
            )));
            let opts = dirs.opts(case.mode);
            match open_embedded(&factory, &opts, case).await {
                Ok(opened) => {
                    let Work::Agent(kind, cmd) = &case.work else {
                        unreachable!()
                    };
                    let cmd = dirs.text(cmd);
                    let host = loop_app_core::agent_gate::GatedHost::new(
                        Arc::new(Nobody),
                        opened.tool_gate.clone(),
                        "chatgpt",
                        dirs.ws.clone(),
                    );
                    let outcome = host.interact(permission(*kind, &cmd)).await;
                    let allowed = matches!(&outcome, InteractionOutcome::Selected { native_id, .. } if native_id.starts_with("allow"));
                    if allowed {
                        // The agent runs its own tool once allowed.
                        let _ = std::process::Command::new("sh")
                            .arg("-c")
                            .arg(&cmd)
                            .current_dir(&dirs.ws)
                            .status();
                    }
                    out.reported = !allowed;
                    out.detail.push_str(&format!("{outcome:?}"));
                }
                Err(e) => out.detail.push_str(&format!("[open failed: {e}] ")),
            }
        }
    }

    let seen = seen.lock().unwrap().clone();
    out.effect = effect(&dirs, &case.check, &seen);
    if matches!(
        case.route,
        Route::Embedded | Route::Runtime | Route::Subagent
    ) || (case.route == Route::Workflow && !matches!(case.work, Work::Shell(_)))
    {
        let reporter = if case.then.is_some() {
            "E3NEXT"
        } else if case.id.contains("-rule-subagent-")
            && case.note.starts_with("subagent tool denied")
        {
            "E3ROOT"
        } else {
            task
        };
        let (rep, detail) = reported(&seen, reporter);
        // The parent of a denied spawn is E3ROOT in the Runtime path.
        out.reported = rep || reported(&seen, task).0;
        out.detail.push_str(&detail);
        if let Some(name) = calls.first().map(|c| c.0.clone()) {
            out.offered = seen
                .get(reporter)
                .or_else(|| seen.get(task))
                .map(|s| s.tools.contains(&name));
        }
    }
    if let Some(at) = *revoked_at.lock().unwrap() {
        out.revoke_ms = Some(at.elapsed().as_millis());
    }
    let _ = &dirs.root;
    out
}

async fn open_embedded(
    factory: &SessionFactory,
    opts: &BootstrapOpts,
    case: &Case,
) -> anyhow::Result<loop_app_core::runtime::OpenedSession> {
    let workspace = factory.workspace(opts).await?;
    let opened = factory.open(workspace, opts).await?;
    // Never open an external diff window (VS Code / Cursor) from a probe.
    opened.approval_bridge.set_open_external_diff(false);
    // `--print`: nobody to ask.
    opened.tool_gate.set_approver(None);
    if let Some(cap) = &case.policy_off {
        let (policy, _) = opened
            .tool_gate
            .policy()
            .with(Layer::new(LayerSource::Session).set(cap.clone(), Setting::always(State::Off)));
        opened.tool_gate.set_policy(policy);
    }
    Ok(opened)
}

/// A host with nobody at the keyboard.
struct Nobody;
#[async_trait]
impl HostServices for Nobody {
    async fn interact(&self, _request: InteractionRequest) -> InteractionOutcome {
        InteractionOutcome::Cancelled
    }
}

fn permission(kind: ToolKind, command: &str) -> InteractionRequest {
    let mut call = ToolCallUpdate::new(ToolCallId("e3".into()));
    call.title = Some(format!("Run {command}"));
    call.kind = Some(kind);
    call.raw_input = Some(json!({ "command": command }));
    let option = |id: &str, kind| PermissionOption {
        native_id: id.into(),
        label: id.into(),
        kind_hint: kind,
    };
    InteractionRequest::Permission {
        id: InteractionId("e3".into()),
        tool_call: Some(call),
        options: vec![
            option("allow-always", PermissionKind::AllowAlways),
            option("allow-once", PermissionKind::AllowOnce),
            option("reject-once", PermissionKind::RejectOnce),
        ],
    }
}

/// The runtime and session a case shares with its approver.
type RuntimeSlot = Arc<Mutex<Option<(Arc<Runtime>, loop_protocol::SessionRef)>>>;

// ---------------------------------------------------------------- the probe

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "probe E3: run with --ignored"]
async fn e3_enforcement() {
    let seed: u64 = env("E3_SEED").parse().unwrap_or(42);
    let filter = env("E3_FILTER");
    let base = tempfile::Builder::new()
        .prefix("e3-run-")
        .tempdir()
        .unwrap();
    let mut all = cases();
    let total = all.len();
    Rng(seed).shuffle(&mut all);
    let started = Instant::now();
    let mut rows = Vec::new();
    for case in all
        .iter()
        .filter(|c| filter.is_empty() || c.id.contains(&filter))
    {
        let t = Instant::now();
        let out = run_case(base.path(), case).await;
        let verdict = match (case.expect, out.effect, out.reported) {
            (Expect::Denied, true, _) => "LEAK",
            (Expect::Denied, false, true) => "ok",
            // A second Session where nothing ran: nothing to refuse.
            (Expect::Denied, false, false) if case.then.is_some() => "ok",
            (Expect::Denied, false, false) => "UNREPORTED",
            (Expect::Allowed, true, _) => "ok",
            (Expect::Allowed, false, _) => "CONTROL-FAIL",
        };
        println!(
            "E3|{verdict}|{}|{}|offered={:?}|revoke_ms={:?}|{}ms|{}|{}",
            case.id,
            case.note,
            out.offered,
            out.revoke_ms,
            t.elapsed().as_millis(),
            out.detail
                .replace('\n', " ")
                .chars()
                .take(400)
                .collect::<String>(),
            if case.expect == Expect::Allowed {
                "control"
            } else {
                ""
            }
        );
        rows.push((case.route, case.group, verdict));
    }
    let mut by: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for (route, _, verdict) in &rows {
        *by.entry((route.name(), verdict)).or_default() += 1;
    }
    println!(
        "E3|SUMMARY|seed={seed}|cases={}/{total}|secs={}",
        rows.len(),
        started.elapsed().as_secs()
    );
    for ((route, verdict), n) in &by {
        println!("E3|COUNT|{route}|{verdict}|{n}");
    }
    let leaks = rows.iter().filter(|r| r.2 == "LEAK").count();
    assert_eq!(leaks, 0, "{leaks} leaks; see E3|LEAK lines");
}

// ---------------------------------------------------------------- leaks
//
// Minimal reproductions of the leaks the probe found. Each asserts the
// safe behaviour, so it fails while the leak is open. Ignored by default.

mod leaks {
    use super::*;
    use loop_app_core::permissions::{
        decide, inside_workspace, read_only_command, Call, Decision, PermissionMode,
    };

    /// L1: `split_command` ignores backslashes, `words` honours them, so an
    /// escaped quote hides a `;` from the split and the read-only check
    /// sees one `echo`. rules.rs:134 (split) vs rules.rs:208 (words).
    #[test]
    fn l1_escaped_quote_passes_the_read_only_check() {
        assert!(!read_only_command("echo \\' ; touch /tmp/x ; echo \\'"));
        assert!(!read_only_command("echo \\\" ; touch /tmp/x ; echo \\\""));
    }

    fn ws() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ws = root.join("ws");
        let out = root.join("out");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::create_dir_all(ws.join(".loop")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        (dir, ws, out)
    }

    fn accept_edits(ws: &Path, path: &str) -> Decision {
        let call = Call::resolve("write", ToolKind::Edit, &json!({"path": path}), ws);
        decide(
            PermissionMode::AcceptEdits,
            &loop_app_core::tool_approval::permissions_from_settings(&Default::default()),
            &call,
        )
    }

    /// L2: `protected` checks the path as written and case-sensitively, so a
    /// symlink into `.loop` and `.GIT` / `.LOOP` on a case-insensitive disk
    /// are edited without asking. capability.rs:97, capability.rs:126.
    #[test]
    fn l2_protected_paths_by_symlink_and_case() {
        let (_d, ws, _out) = ws();
        std::os::unix::fs::symlink(ws.join(".loop"), ws.join("cfg")).unwrap();
        assert_ne!(accept_edits(&ws, "cfg/settings.json"), Decision::Allow);
        assert_ne!(accept_edits(&ws, ".GIT/hooks/pre-commit"), Decision::Allow);
        assert_ne!(accept_edits(&ws, ".LOOP/settings.json"), Decision::Allow);
    }

    /// L3: a dangling symlink in the workspace counts as inside, and the
    /// write tool follows it out. policy.rs:168-185 (`resolve` strips the
    /// last part when it cannot canonicalize it).
    #[test]
    fn l3_dangling_symlink_counts_as_inside() {
        let (_d, ws, out) = ws();
        std::os::unix::fs::symlink(out.join("m.txt"), ws.join("f.txt")).unwrap();
        assert!(!inside_workspace("f.txt", &ws));
    }

    fn agent_gate(
        rules: Value,
        mode: PermissionMode,
        hook: Option<Value>,
    ) -> (tempfile::TempDir, Arc<loop_app_core::tool_gate::ToolGate>) {
        let dir = tempfile::tempdir().unwrap();
        let map: BTreeMap<String, String> = serde_json::from_value(rules).unwrap();
        let gate = loop_app_core::tool_gate::ToolGate::new(
            loop_app_core::tool_approval::permissions_from_settings(&map),
        );
        if let Some(hook) = hook {
            let path = dir.path().join("h.json");
            std::fs::write(&path, hook.to_string()).unwrap();
            let registry = loop_agent::harness::HookRegistry::default();
            loop_app_core::hooks_load::register_hooks(
                &registry,
                loop_app_core::hooks_load::load_hooks(&[path]),
            );
            gate.set_hooks(registry);
        }
        gate.set_mode(mode);
        (dir, gate)
    }

    async fn acp(gate: Arc<loop_app_core::tool_gate::ToolGate>) -> InteractionOutcome {
        loop_app_core::agent_gate::GatedHost::new(
            Arc::new(Nobody),
            gate,
            "chatgpt",
            PathBuf::from("/tmp"),
        )
        .interact(permission(ToolKind::Execute, "touch /tmp/x"))
        .await
    }

    fn allowed(o: &InteractionOutcome) -> bool {
        matches!(o, InteractionOutcome::Selected { native_id, .. } if native_id.starts_with("allow"))
    }

    /// L4: an ACP agent's request skips `toolPermissions` deny rules, so
    /// `"bash": "deny"` in full-access still picks "allow once".
    /// agent_gate.rs:141, tool_gate.rs:264.
    #[tokio::test]
    async fn l4_acp_ignores_deny_rules() {
        let (_d, gate) = agent_gate(json!({"bash": "deny"}), PermissionMode::FullAccess, None);
        assert!(!allowed(&acp(gate).await));
    }

    /// L5: a hook's allow is honoured before the mode check, so it lifts the
    /// read-only ceiling for ACP agents (native tools refuse it).
    /// agent_gate.rs:127 runs before agent_gate.rs:134.
    #[tokio::test]
    async fn l5_acp_hook_allow_beats_read_only() {
        let (_d, gate) = agent_gate(
            json!({}),
            PermissionMode::ReadOnly,
            Some(json!({"on": "tool_call", "action": {"allow": true}})),
        );
        assert!(!allowed(&acp(gate).await));
    }
}
