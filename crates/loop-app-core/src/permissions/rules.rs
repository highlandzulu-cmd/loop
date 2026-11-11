//! `toolPermissions` rules: pattern parsing and matching, and bash command
//! splitting.

use std::cell::{OnceCell, RefCell};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use super::policy::resolve;
use super::Call;
use crate::hooks_load::glob_match;
pub use loop_agent::harness::tools::shell::split_command;
use loop_agent::harness::tools::shell::split_words;

/// Per-tool default from `~/.loop/agent/settings.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPermission {
    /// Prompt when approval policy says so.
    Ask,
    /// Never prompt.
    Allow,
    /// Always block.
    Deny,
}

impl ToolPermission {
    /// Parse `ask` / `allow` / `deny`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow" | "auto" | "yes" => Self::Allow,
            "deny" | "block" | "no" => Self::Deny,
            _ => Self::Ask,
        }
    }

    /// Settings label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }

    /// `allow` < `ask` < `deny`.
    pub fn strictness(self) -> u8 {
        match self {
            Self::Allow => 0,
            Self::Ask => 1,
            Self::Deny => 2,
        }
    }
}

/// What `toolPermissions` says about one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    /// The permission.
    pub permission: ToolPermission,
    /// Whether a pattern rule (`bash(git *)`) decided, not the plain tool rule.
    pub specific: bool,
}

impl Rule {
    /// A plain tool rule.
    pub fn plain(permission: ToolPermission) -> Self {
        Self {
            permission,
            specific: false,
        }
    }
}

/// Split `tool(pattern)` keys; plain keys have no pattern.
fn split_key(key: &str) -> (&str, Option<&str>) {
    match key.split_once('(') {
        Some((tool, rest)) if rest.ends_with(')') => (tool.trim(), Some(&rest[..rest.len() - 1])),
        _ => (key, None),
    }
}

/// The call argument patterns match: the command, path, URL or query. A
/// search tool's subject is its search root, `.` when it names none.
fn subject(tool: &str, args: &Value) -> Option<String> {
    if is_search(tool) {
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        return Some(path.to_owned());
    }
    let key = match tool {
        "bash" => "command",
        "read" | "write" | "edit" => "path",
        "web_fetch" => "url",
        "web_search" => "query",
        _ => return Some(args.to_string()),
    };
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Loop's read-only search tools, whose patterns match a path.
fn is_search(tool: &str) -> bool {
    matches!(tool, "grep" | "glob" | "ls")
}

/// Whether a search tool (`grep`, `glob`, `ls`) may show or read the file
/// at `path`: not when a `read` rule, or the tool's own rule, denies that
/// path. The rules see the path as the walk built it and the file it names
/// ([`PathForms`]), so a symlink or `..` into a denied tree is hidden too.
/// The search root is checked separately, as the call's subject.
pub fn search_may_show(
    rules: &BTreeMap<String, ToolPermission>,
    tool: &str,
    path: &Path,
    workspace: &Path,
    home: Option<&Path>,
) -> bool {
    // Inside the workspace, relative patterns such as `secret/*` match the
    // relative path; `PathForms` also tries it joined to the workspace.
    let subject = path.strip_prefix(workspace).unwrap_or(path);
    let args = serde_json::json!({ "path": subject.to_string_lossy() });
    // Both tools share one resolution of the path.
    let forms = OnceCell::new();
    ["read", tool].iter().all(|tool| {
        let call = Call {
            home: home.map(Path::to_path_buf),
            ..Call::new(tool, args.clone(), workspace)
        };
        rule_with(rules, &call, &forms).permission != ToolPermission::Deny
    })
}

/// Commands `read-only` runs: they print what they read and change nothing
/// (with the options [`read_only_part`] refuses).
const READ_ONLY_COMMANDS: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "grep", "rg", "find", "tree", "file", "stat", "du",
    "which", "echo", "sort", "uniq", "diff",
];

/// `git` subcommands `read-only` runs.
const READ_ONLY_GIT: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "branch",
    "blame",
    "ls-files",
    "rev-parse",
];

/// Whether every part of `command` only reads: a command from
/// [`READ_ONLY_COMMANDS`] or a read-only `git` subcommand, with no output
/// redirect, command substitution or variable assignment.
pub fn read_only_command(command: &str) -> bool {
    split_command(command).is_some_and(|parts| {
        parts
            .iter()
            .all(|part| words(part).is_some_and(|w| read_only_part(&w)))
    })
}

/// The words of one command part, with quotes removed. `None` when it
/// redirects output (an unquoted `>`).
fn words(part: &str) -> Option<Vec<String>> {
    split_words(part, false)
}

/// One command part as the shell runs it: quotes and escapes removed and
/// words joined with single spaces, so `\rm`, `r\m` and `'rm'` read as
/// `rm`. Deny and ask patterns match this as well as the part as written.
fn normalise(part: &str) -> String {
    split_words(part, true).unwrap_or_default().join(" ")
}

fn read_only_part(words: &[String]) -> bool {
    let Some((program, args)) = words.split_first() else {
        return false;
    };
    let has = |options: &[&str]| {
        args.iter().any(|a| {
            options
                .iter()
                .any(|o| a == o || (o.starts_with("--") && a.starts_with(&format!("{o}="))))
        })
    };
    let positional = || args.iter().filter(|a| !a.starts_with('-')).count();
    match program.as_str() {
        "git" => {
            let Some((sub, rest)) = args.split_first() else {
                return false;
            };
            if !READ_ONLY_GIT.contains(&sub.as_str()) || has(&["--output"]) {
                return false;
            }
            // `git branch NAME` creates a branch; only listing is a read.
            sub != "branch"
                || rest.iter().all(|a| {
                    a.starts_with('-')
                        && !matches!(
                            a.as_str(),
                            "-d" | "-D"
                                | "-m"
                                | "-M"
                                | "-c"
                                | "-C"
                                | "-f"
                                | "-u"
                                | "--delete"
                                | "--move"
                                | "--copy"
                                | "--force"
                                | "--edit-description"
                                | "--unset-upstream"
                        )
                        && !a.starts_with("--set-upstream-to")
                })
        }
        "find" => !has(&[
            "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf",
            "-fls",
        ]),
        // `-o FILE` writes FILE, also inside a cluster such as `-uo`.
        // `--compress-program` runs a program when sort spills to disk.
        "sort" => !args.iter().any(|a| {
            a.starts_with("--output")
                || a.starts_with("--compress-program")
                || (a.starts_with('-') && !a.starts_with("--") && a.contains('o'))
        }),
        // `file -C` compiles a magic file.
        "file" => !has(&["-C", "--compile"]),
        "rg" => !has(&["--pre", "--hostname-bin"]),
        "tree" => !has(&["-o"]),
        // `uniq IN OUT` writes OUT.
        "uniq" => positional() <= 1,
        program => READ_ONLY_COMMANDS.contains(&program),
    }
}

/// `p` with a leading `~/` replaced by `home`.
fn expand_home(p: &str, home: Option<&Path>) -> String {
    match (p.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ => p.to_owned(),
    }
}

/// `path` with `.` parts removed and each `..` taking off the part before
/// it, without looking at the filesystem.
fn normalise_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push(part);
                }
            }
            part => out.push(part),
        }
    }
    out
}

/// The names of one path that path patterns are matched against.
///
/// A rule's pattern names files, but a call can reach a file through a
/// symlink or a `..`. So the path is matched as written and also as the
/// file it names: lexically normalised, and resolved through symlinks
/// (from the path as written, as the kernel does, and from the normalised
/// path). A `deny` or `ask` pattern applies when it matches any of them;
/// an `allow` pattern only when it matches the path as written and every
/// resolved name.
#[derive(Debug)]
pub(crate) struct PathForms {
    /// The path as written (`~` expanded), its absolute form and, inside
    /// the workspace, its relative form.
    written: Vec<String>,
    /// Each other name of the file: the absolute form and, inside the
    /// workspace, the relative one. `None` when the path cannot be resolved
    /// (a symlink loop): deny and ask then apply, allow does not.
    resolved: Option<Vec<Vec<String>>>,
}

impl PathForms {
    pub(crate) fn new(path: &str, workspace: &Path, home: Option<&Path>) -> Self {
        let path = expand_home(path, home);
        let joined = workspace.join(&path);
        // Components drop `.` parts, so `.` is the workspace itself.
        let absolute: PathBuf = joined.components().collect();
        let root = resolve_cached(workspace);
        let names = |p: &Path| {
            let mut names = vec![p.to_string_lossy().into_owned()];
            for base in std::iter::once(workspace).chain(root.as_deref()) {
                if let Ok(rel) = p.strip_prefix(base) {
                    if !rel.as_os_str().is_empty() {
                        names.push(rel.to_string_lossy().into_owned());
                    }
                }
            }
            names
        };
        let mut written = names(&absolute);
        written.push(path);
        let lexical = normalise_path(&joined);
        let resolved = resolve(&lexical).map(|real| {
            let mut groups = vec![names(&lexical), names(&real)];
            // `resolve` refuses `..` below a missing part; the kernel would
            // not open such a path either.
            // With no `..` in the path, this is the same file as `real`.
            if lexical != absolute {
                if let Some(kernel) = resolve(&joined) {
                    if kernel != real {
                        groups.push(names(&kernel));
                    }
                }
            }
            groups
        });
        Self { written, resolved }
    }

    /// Whether `patterns` (one rule's forms) apply to this path for a rule
    /// of `permission`.
    fn hit(&self, patterns: &[String], permission: ToolPermission) -> bool {
        let any = |names: &[String]| {
            names
                .iter()
                .any(|name| patterns.iter().any(|p| glob_match(p, name)))
        };
        let written = any(&self.written);
        match (permission, &self.resolved) {
            (ToolPermission::Allow, Some(groups)) => written && groups.iter().all(|g| any(g)),
            (ToolPermission::Allow, None) => false,
            (_, Some(groups)) => written || groups.iter().any(|g| any(g)),
            (_, None) => true,
        }
    }
}

/// A path pattern as written (`~` expanded) and, when its fixed directory
/// part is absolute, with that part resolved through symlinks, so that
/// `/var/x/*` also names `/private/var/x/*` and a home behind a link still
/// matches.
fn pattern_forms(pattern: &str, home: Option<&Path>) -> Vec<String> {
    let pattern = expand_home(pattern, home);
    let mut forms = vec![pattern.clone()];
    let (prefix, rest) = match pattern.find(['*', '?']) {
        None => (pattern.as_str(), ""),
        Some(cut) => match pattern[..cut].rfind(['/', '\\']) {
            Some(end) => pattern.split_at(end),
            None => return forms,
        },
    };
    if !Path::new(prefix).is_absolute() {
        return forms;
    }
    if let Some(real) = resolve_cached(&normalise_path(Path::new(prefix))) {
        let real = format!("{}{rest}", real.display());
        if real != pattern {
            forms.push(real);
        }
    }
    forms
}

/// [`resolve`] for pattern prefixes and the workspace, remembered for a
/// moment: a search checks every file it walks against the same few
/// patterns.
fn resolve_cached(path: &Path) -> Option<PathBuf> {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    const FRESH: Duration = Duration::from_secs(2);
    thread_local! {
        static CACHE: RefCell<HashMap<PathBuf, (Instant, Option<PathBuf>)>> =
            RefCell::new(HashMap::new());
    }
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((at, real)) = cache.get(path) {
            if at.elapsed() < FRESH {
                return real.clone();
            }
        }
        if cache.len() > 256 {
            cache.clear();
        }
        let real = resolve(path);
        cache.insert(path.to_path_buf(), (Instant::now(), real.clone()));
        real
    })
}

/// The `toolPermissions` rule for one call; tools with no rule are allowed.
///
/// A path pattern (`read`, `write`, `edit`, `grep`, `glob`, `ls`) sees the
/// path as written and the file it names through symlinks and `..`
/// ([`PathForms`]); only then does this look at the filesystem.
pub fn rule_for(rules: &BTreeMap<String, ToolPermission>, call: &Call) -> Rule {
    rule_with(rules, call, &OnceCell::new())
}

/// [`rule_for`] with the call path's [`PathForms`] worked out at most once.
fn rule_with(
    rules: &BTreeMap<String, ToolPermission>,
    call: &Call,
    forms: &OnceCell<PathForms>,
) -> Rule {
    let tool = call.tool.as_str();
    let subject = subject(tool, &call.args);
    let mut specific: Option<ToolPermission> = None;
    let mut bash_allows = Vec::new();
    for (key, permission) in rules {
        let (tool_pattern, Some(pattern)) = split_key(key) else {
            continue;
        };
        if !glob_match(tool_pattern, tool) {
            continue;
        }
        let Some(subject) = subject.as_deref() else {
            continue;
        };
        let hit = if tool == "bash" {
            if *permission == ToolPermission::Allow {
                bash_allows.push(pattern);
                false
            } else {
                // Deny and ask also match the part with quotes and escapes
                // removed; allow (above) matches only the part as written.
                let matches =
                    |part: &str| glob_match(pattern, part) || glob_match(pattern, &normalise(part));
                match split_command(subject) {
                    Some(parts) => parts.iter().any(|part| matches(part)),
                    None => {
                        matches(subject)
                            || subject.contains(pattern.trim_matches('*'))
                            || normalise(subject).contains(pattern.trim_matches('*'))
                    }
                }
            }
        } else if matches!(tool, "read" | "write" | "edit") || is_search(tool) {
            let home = call.home.as_deref();
            let forms = forms.get_or_init(|| PathForms::new(subject, &call.workspace, home));
            forms.hit(&pattern_forms(pattern, home), *permission)
        } else {
            glob_match(pattern, subject)
        };
        if hit && specific.is_none_or(|s| permission.strictness() > s.strictness()) {
            specific = Some(*permission);
        }
    }
    if specific.is_none() && !bash_allows.is_empty() {
        let all_allowed = subject
            .as_deref()
            .and_then(split_command)
            .is_some_and(|parts| {
                parts
                    .iter()
                    .all(|part| bash_allows.iter().any(|pattern| glob_match(pattern, part)))
            });
        if all_allowed {
            specific = Some(ToolPermission::Allow);
        }
    }
    if let Some(permission) = specific {
        return Rule {
            permission,
            specific: true,
        };
    }
    if let Some(permission) = rules.get(tool) {
        return Rule::plain(*permission);
    }
    // Plain glob keys such as `mcp__github__*`; the strictest match wins.
    let plain = rules
        .iter()
        .filter(|(key, _)| split_key(key).1.is_none() && key.contains('*') && glob_match(key, tool))
        .map(|(_, p)| *p)
        .max_by_key(|p| p.strictness());
    Rule::plain(plain.unwrap_or(ToolPermission::Allow))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn rules(entries: &[(&str, &str)]) -> BTreeMap<String, ToolPermission> {
        crate::tool_approval::permissions_from_settings(
            &entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    fn call(tool: &str, args: Value) -> Call {
        Call {
            home: Some("/home/me".into()),
            ..Call::new(tool, args, "/ws")
        }
    }

    fn bash_rule(rules: &BTreeMap<String, ToolPermission>, cmd: &str) -> Rule {
        rule_for(rules, &call("bash", json!({ "command": cmd })))
    }

    #[test]
    fn read_only_commands() {
        let allowed = [
            "ls -la",
            "ls | grep x",
            "cat a.txt",
            "grep -rn 'a > b' src",
            "rg --files | wc -l",
            "find . -name '*.rs'",
            "git status && git log --oneline -5",
            "git diff HEAD~1 -- src",
            "git branch -a",
            "head -20 README.md; tail -n 5 log.txt",
            "sort -u names.txt",
            "uniq names.txt",
        ];
        for command in allowed {
            assert!(read_only_command(command), "{command}");
        }
        let refused = [
            "cat a > b",
            "echo hi >> notes",
            "find . -delete",
            "find . -name x -exec rm {} ;",
            "git log && rm x",
            "git push",
            "git branch new-branch",
            "git branch -D old",
            "git -c core.pager=evil log",
            "git log --output=out.txt",
            "sort -o out.txt in.txt",
            "sort -uo out.txt in.txt",
            "file -C -m magic",
            "rg --pre ./script x",
            "rg --hostname-bin=./script x",
            "sort -S 1 --compress-program=./script big.txt",
            "tree -o out.txt",
            "uniq in.txt out.txt",
            "ls $(pwd)",
            "cat `which ls`",
            "FOO=1 ls",
            "/bin/ls",
            "cargo test",
            "tee out.txt",
            "",
        ];
        for command in refused {
            assert!(!read_only_command(command), "{command}");
        }
    }

    #[test]
    fn splitting_commands() {
        assert_eq!(
            split_command("git status && rm -rf x; ls | wc -l").unwrap(),
            ["git status", "rm -rf x", "ls", "wc -l"]
        );
        assert_eq!(
            split_command("echo 'a && b' \"c;d\"").unwrap(),
            ["echo 'a && b' \"c;d\""]
        );
        assert!(split_command("echo $(whoami)").is_none());
        assert!(split_command("echo `whoami`").is_none());
        assert!(split_command("echo 'unclosed").is_none());
        assert_eq!(
            split_command("echo '$(literal)'").unwrap(),
            ["echo '$(literal)'"]
        );
    }

    #[test]
    fn bash_allow_patterns_need_every_part() {
        let r = rules(&[
            ("bash", "ask"),
            ("bash(git status*)", "allow"),
            ("bash(ls*)", "allow"),
        ]);
        let allow = Rule {
            permission: ToolPermission::Allow,
            specific: true,
        };
        assert_eq!(bash_rule(&r, "git status --short"), allow);
        assert_eq!(bash_rule(&r, "git status && ls -la"), allow);
        assert_eq!(
            bash_rule(&r, "git status && rm -rf x"),
            Rule::plain(ToolPermission::Ask)
        );
        assert_eq!(
            bash_rule(&r, "git status $(rm -rf x)"),
            Rule::plain(ToolPermission::Ask)
        );
    }

    #[test]
    fn deny_and_ask_patterns_see_through_quotes_and_escapes() {
        let r = rules(&[("bash(rm *)", "deny"), ("bash(curl *)", "ask")]);
        for cmd in [
            "\\rm -rf x",
            "r\\m -rf x",
            "'rm' -rf x",
            "\"r\"m -rf x",
            "ls && \\rm -rf x",
            "rm  -rf x",
        ] {
            assert_eq!(bash_rule(&r, cmd).permission, ToolPermission::Deny, "{cmd}");
        }
        assert_eq!(bash_rule(&r, "c\\url x").permission, ToolPermission::Ask);
        // Allow patterns are not widened by the normalisation.
        let allow = rules(&[("bash(git status)", "allow"), ("bash", "ask")]);
        assert_eq!(
            bash_rule(&allow, "git status").permission,
            ToolPermission::Allow
        );
        assert_eq!(
            bash_rule(&allow, "'git' status").permission,
            ToolPermission::Ask
        );
    }

    #[test]
    fn deny_patterns_match_any_part_and_beat_allow() {
        let r = rules(&[("bash(git *)", "allow"), ("bash(rm -rf *)", "deny")]);
        assert_eq!(
            bash_rule(&r, "git status; rm -rf build").permission,
            ToolPermission::Deny
        );
        assert_eq!(
            bash_rule(&r, "echo $(rm -rf build)").permission,
            ToolPermission::Deny,
            "an unparseable command is still checked against deny patterns"
        );
    }

    #[test]
    fn path_url_and_glob_tool_rules() {
        let r = rules(&[
            ("write(~/secrets/*)", "deny"),
            ("write(src/generated/*)", "ask"),
            ("web_fetch(https://docs.rs/*)", "allow"),
            ("mcp__github__*", "ask"),
        ]);
        let permission = |tool: &str, args: Value| rule_for(&r, &call(tool, args)).permission;
        // Built the way `~/` expands, so the separators match on Windows too.
        let secret = Path::new("/home/me").join("secrets/key");
        assert_eq!(
            permission("write", json!({ "path": secret.to_string_lossy() })),
            ToolPermission::Deny
        );
        assert_eq!(
            permission("write", json!({"path": "~/secrets/key"})),
            ToolPermission::Deny
        );
        let generated = rule_for(&r, &call("write", json!({"path": "src/generated/a.rs"})));
        assert_eq!(generated.permission, ToolPermission::Ask);
        assert!(generated.specific);
        #[cfg(unix)]
        assert_eq!(
            permission("write", json!({"path": "/ws/src/generated/a.rs"})),
            ToolPermission::Ask,
            "an absolute path in the workspace matches a relative pattern"
        );
        assert_eq!(
            permission("web_fetch", json!({"url": "https://docs.rs/serde"})),
            ToolPermission::Allow
        );
        assert_eq!(
            permission("web_fetch", json!({"url": "https://evil.example"})),
            ToolPermission::Allow,
            "the default web_fetch rule"
        );
        assert_eq!(
            permission("mcp__github__create_issue", json!({})),
            ToolPermission::Ask
        );
        assert_eq!(
            permission("mcp__slack__post", json!({})),
            ToolPermission::Allow
        );
    }

    #[test]
    fn search_tool_rules_match_the_search_root() {
        let r = rules(&[("grep(~/secrets/*)", "deny"), ("ls(/ws)", "deny")]);
        let permission = |tool: &str, args: Value| rule_for(&r, &call(tool, args)).permission;
        assert_eq!(
            permission("grep", json!({"pattern": "key", "path": "~/secrets/a"})),
            ToolPermission::Deny
        );
        assert_eq!(
            permission("grep", json!({"pattern": "key", "path": "src"})),
            ToolPermission::Allow
        );
        // No `path` searches the working directory.
        #[cfg(unix)]
        assert_eq!(permission("ls", json!({})), ToolPermission::Deny);
        assert_eq!(
            permission("glob", json!({"pattern": "*.rs"})),
            ToolPermission::Allow
        );
    }

    // Home patterns expand with the platform's separator, as for `read`.
    #[cfg(unix)]
    #[test]
    fn search_tools_skip_files_a_read_or_search_deny_rule_refuses() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let r = rules(&[("read(~/.ssh/*)", "deny"), ("grep(~/private/*)", "deny")]);
        let shown =
            |tool: &str, path: &Path| search_may_show(&r, tool, path, Path::new("/ws"), Some(home));
        let key = home.join(".ssh").join("id_rsa");
        let diary = home.join("private").join("diary.txt");
        let notes = home.join("notes.txt");
        for tool in ["grep", "glob", "ls"] {
            assert!(!shown(tool, &key), "{tool}: a read deny hides the file");
            assert!(shown(tool, &notes), "{tool}");
        }
        assert!(!shown("grep", &diary), "the tool's own pattern deny");
        assert!(shown("ls", &diary), "another tool's pattern doesn't apply");

        let plain = rules(&[("read", "deny")]);
        assert!(
            !search_may_show(&plain, "grep", &notes, Path::new("/ws"), Some(home)),
            "denying read denies searching file contents too"
        );
    }

    /// A workspace reached through a link (`/var` on macOS is one), with a
    /// link to a secret outside it.
    #[cfg(unix)]
    fn linked_tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(real.join("ws/src")).unwrap();
        std::fs::create_dir_all(real.join("home/.secret")).unwrap();
        std::fs::write(real.join("home/.secret/key"), "k").unwrap();
        std::fs::write(real.join("ws/src/a.rs"), "").unwrap();
        std::os::unix::fs::symlink(real.join("ws"), real.join("ws-link")).unwrap();
        std::os::unix::fs::symlink(real.join("home"), real.join("home-link")).unwrap();
        std::os::unix::fs::symlink(real.join("home/.secret/key"), real.join("ws/src/key")).unwrap();
        std::os::unix::fs::symlink(real.join("home/.secret"), real.join("ws/sec")).unwrap();
        (dir, real.join("ws-link"), real)
    }

    #[cfg(unix)]
    #[test]
    fn path_rules_see_through_links_and_dot_dot() {
        let (_d, ws, real) = linked_tree();
        // The rule names the home through a link; the calls reach the file
        // by its real path, a file link, a directory link and `..`.
        let r = rules(&[(
            &format!("read({}/.secret/*)", real.join("home-link").display()),
            "deny",
        )]);
        let home = real.join("home-link");
        let permission = |path: &str| {
            let call = Call {
                home: Some(home.clone()),
                ..Call::new("read", json!({ "path": path }), &ws)
            };
            rule_for(&r, &call).permission
        };
        let direct = real.join("home/.secret/key");
        for path in [
            direct.to_str().unwrap(),
            "~/.secret/key",
            "src/key",
            "sec/key",
            "../home/.secret/key",
            "sec/../.secret/key",
        ] {
            assert_eq!(permission(path), ToolPermission::Deny, "{path}");
        }
        assert_eq!(permission("src/a.rs"), ToolPermission::Allow);
        for path in ["src/key", "sec/key", "src/a.rs"] {
            assert_eq!(
                search_may_show(&r, "grep", &ws.join(path), &ws, Some(&home)),
                path == "src/a.rs",
                "{path}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn allow_patterns_need_the_written_and_the_resolved_path() {
        let (_d, ws, _real) = linked_tree();
        let r = rules(&[("write(src/*)", "allow"), ("write", "ask")]);
        let permission =
            |path: &str| rule_for(&r, &Call::new("write", json!({ "path": path }), &ws)).permission;
        // A workspace behind a link still matches relative allow patterns.
        assert_eq!(permission("src/a.rs"), ToolPermission::Allow);
        let absolute = ws.join("src/a.rs");
        assert_eq!(
            permission(absolute.to_str().unwrap()),
            ToolPermission::Allow
        );
        // A link or `..` out of `src` is not widened into the allow.
        assert_eq!(permission("src/key"), ToolPermission::Ask);
        assert_eq!(permission("src/../outside.txt"), ToolPermission::Ask);
        // An absolute allow pattern through the link matches the real path.
        let r = rules(&[
            (&format!("write({}/src/*)", ws.display()), "allow"),
            ("write", "ask"),
        ]);
        let real = ws.canonicalize().unwrap().join("src/a.rs");
        assert_eq!(
            rule_for(
                &r,
                &Call::new("write", json!({ "path": real.to_str().unwrap() }), &ws)
            )
            .permission,
            ToolPermission::Allow
        );
    }

    #[test]
    fn normalising_paths() {
        let n = |p: &str| normalise_path(Path::new(p));
        assert_eq!(n("/a/./b/../c"), Path::new("/a/c"));
        assert_eq!(n("/../a"), Path::new("/a"));
        assert_eq!(n("a/../../b"), Path::new("../b"));
    }

    #[test]
    fn strictness_orders_permissions() {
        use ToolPermission::*;
        assert!(Allow.strictness() < Ask.strictness() && Ask.strictness() < Deny.strictness());
    }
}
