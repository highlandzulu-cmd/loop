//! The bash safety block list: commands refused whatever the user allows.
//!
//! This is a best-effort guard against accidents, not a security boundary.
//! It reads the command as text and cannot see what a program, a variable or
//! a script does: `find / -delete`, `xargs rm -rf`, `rm -rf $DIR` and
//! `cd / && rm -rf *` all pass. The sandbox and the permission rules
//! (`toolPermissions`, modes, hooks) are the boundary.

use super::shell::{split_command, split_words};

/// Patterns blocked anywhere in the command (lowercased), as written or as
/// the shell reads it (quotes and escapes removed).
const DEFAULT_BLOCKED_PATTERNS: &[&str] = &[
    "mkfs.",
    ":(){ :|:& };:",
    "> /dev/sda",
    "dd if=/dev/zero of=/dev/",
    "format c:",
];

/// Commands blocked when they run recursively on a root or home target.
const RECURSIVE_PROGRAMS: &[&str] = &["rm", "chmod", "chown", "chgrp"];

/// Check a command against the blocklist. Returns `Err` with a rejection message
/// if the command matches a blocked pattern.
///
/// Recursive `rm`, `chmod`, `chown` and `chgrp` are blocked only when a
/// target is the filesystem root, the home directory or a top-level system
/// directory (`/`, `/*`, `~`, `$HOME`, `/usr`, …), so `chmod -R u+w ./src`
/// and `rm -rf /tmp/x` run. Scripts given to `sh -c`, `su -c`, `eval`,
/// `ssh`, a here-string or a shell's stdin are checked too.
pub fn check_command_policy(command: &str, extra_blocked: &[String]) -> Result<(), String> {
    check_nested(command, extra_blocked, 0)
}

/// How deep `sh -c '…'` and `eval '…'` scripts are checked.
const MAX_NESTING: usize = 4;

fn check_nested(command: &str, extra_blocked: &[String], depth: usize) -> Result<(), String> {
    let lower = command.to_lowercase();
    let trimmed = lower.trim();
    // A command that substitutes commands or has unbalanced quotes does not
    // split; cut it at every separator and bracket instead, so nothing in it
    // escapes the checks below.
    let parts = split_command(trimmed).unwrap_or_else(|| {
        trimmed
            .split([';', '&', '|', '\n', '`', '(', ')'])
            .map(str::to_owned)
            .collect()
    });
    let parts: Vec<Vec<String>> = parts
        .iter()
        .map(|part| split_words(part, true).unwrap_or_default())
        .collect();
    let normalised = parts
        .iter()
        .map(|words| words.join(" "))
        .collect::<Vec<_>>()
        .join(" ; ");

    for pattern in DEFAULT_BLOCKED_PATTERNS {
        if trimmed.contains(pattern) || normalised.contains(pattern) {
            return Err(format!(
                "Command blocked by safety policy: matches blocked pattern `{pattern}`"
            ));
        }
    }

    // A bare shell reads its script from stdin: check what earlier parts
    // print (`echo 'rm -rf /' | sh`).
    let mut scripts: Vec<String> = parts
        .iter()
        .flat_map(|words| nested_scripts(words))
        .collect();
    if parts.iter().any(|words| reads_stdin_script(words)) {
        scripts.extend(
            parts
                .iter()
                .filter(|w| w.len() > 1)
                .map(|w| w[1..].join(" ")),
        );
    }
    if !scripts.is_empty() && depth >= MAX_NESTING {
        return Err(
            "Command blocked by safety policy: shell scripts nested too deeply to check".into(),
        );
    }
    for script in &scripts {
        check_nested(script, extra_blocked, depth + 1)?;
    }

    let home = home_dir();
    for words in &parts {
        if let Some(blocked) = recursive_on_root(words, home.as_deref()) {
            return Err(format!(
                "Command blocked by safety policy: recursive `{blocked}` on the root or home directory"
            ));
        }
    }

    for pattern in extra_blocked {
        let pat_lower = pattern.to_lowercase();
        if trimmed.contains(&pat_lower) || normalised.contains(&pat_lower) {
            return Err(format!(
                "Command blocked by safety policy: matches custom blocked pattern `{pattern}`"
            ));
        }
    }

    Ok(())
}

/// Scripts that `words` hand to another shell: the argument of `sh -c`
/// (`bash`, `zsh`, `su`, …), a here-string fed to a shell, the words after
/// `eval`, and the remote command of `ssh`.
fn nested_scripts(words: &[String]) -> Vec<String> {
    let mut scripts = Vec::new();
    for (i, word) in words.iter().enumerate() {
        let name = program_name(word);
        let rest = &words[i + 1..];
        if name == "eval" {
            scripts.push(rest.join(" "));
        } else if name == "ssh" {
            // Each word, and all of them together: `ssh host 'cmd'` and
            // `ssh -p 22 host cmd args`.
            scripts.extend(rest.iter().cloned());
            scripts.push(rest.join(" "));
        } else if SHELLS.contains(&name) {
            let mut args = rest.iter();
            // `-c`, or combined flags such as `-lc`, then the script.
            if args
                .clone()
                .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
            {
                let mut after_c = args.clone().skip_while(|a| {
                    !(a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
                });
                after_c.next();
                scripts.extend(after_c.find(|a| !a.starts_with('-')).cloned());
            }
            // A here-string: `sh <<< 'cmd'` or `sh <<<'cmd'`.
            while let Some(arg) = args.next() {
                if arg == "<<<" {
                    scripts.extend(args.next().cloned());
                } else if let Some(script) = arg.strip_prefix("<<<") {
                    scripts.push(script.to_owned());
                }
            }
        }
    }
    scripts
}

/// Whether `words` run a shell that reads its script from stdin: a shell
/// with no script argument (`sh`, `bash -s`, `sudo bash`).
fn reads_stdin_script(words: &[String]) -> bool {
    words.iter().enumerate().any(|(i, word)| {
        SHELLS.contains(&program_name(word))
            && program_name(word) != "su"
            && words[i + 1..]
                .iter()
                .all(|a| a.starts_with('-') && !a.contains('c'))
    })
}

/// Shells whose script (`-c`, a here-string, stdin) is checked like the
/// command itself.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish", "su"];

/// A word's program name: `/bin/rm` reads as `rm`.
fn program_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Top-level directories whose loss breaks the system or every user.
const PROTECTED_DIRS: &[&str] = &[
    "/usr",
    "/etc",
    "/bin",
    "/sbin",
    "/lib",
    "/var",
    "/home",
    "/users",
    "/system",
    "/opt",
    "/private",
    "/applications",
    "/library",
];

/// The home directory, lowercased like the command, without a trailing `/`.
fn home_dir() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let home = normalise_path(&home.to_lowercase())?;
    (home != "/").then_some(home)
}

/// The program, when `words` (lowercased) run `rm`, `chmod`, `chown` or
/// `chgrp` recursively on a protected target ([`is_protected`]). Any word
/// naming one of them starts its arguments, so wrappers (`sudo`, `env`,
/// `xargs`, …) do not hide it.
fn recursive_on_root(words: &[String], home: Option<&str>) -> Option<&'static str> {
    for (i, word) in words.iter().enumerate() {
        let name = program_name(word);
        let Some(program) = RECURSIVE_PROGRAMS.iter().find(|p| **p == name) else {
            continue;
        };
        let mut recursive = false;
        let mut targets = Vec::new();
        let mut options_end = false;
        for arg in &words[i + 1..] {
            if options_end {
                targets.push(arg.as_str());
            } else if arg == "--" {
                options_end = true;
            } else if arg == "--recursive" {
                recursive = true;
            } else if let Some(flags) = arg.strip_prefix('-').filter(|f| !f.starts_with('-')) {
                // The command is lowercased: `-R` reads as `-r`.
                recursive |= flags.contains('r');
            } else if !arg.starts_with("--") {
                targets.push(arg.as_str());
            }
        }
        if recursive && targets.iter().any(|t| is_protected(t, home)) {
            return Some(program);
        }
    }
    None
}

/// Whether `target` (lowercased) names the filesystem root, the home
/// directory or one of its parents, or a [`PROTECTED_DIRS`] entry, after
/// `~`/`$HOME` expansion and `.`/`..` removal. A pattern (`*`, `?`, `[`, `{`)
/// counts when the fixed part before it is such a directory, so `/*`, `/{,}`
/// and `~/*` are blocked while `/tmp/build/*` runs. `~name` (another
/// user's home) is always blocked.
fn is_protected(target: &str, home: Option<&str>) -> bool {
    let t = target.trim();
    let home_or_tilde = home.unwrap_or("~");
    let mut expanded = None;
    for prefix in ["~", "$home", "${home}"] {
        if let Some(rest) = t.strip_prefix(prefix) {
            if rest.is_empty() || rest.starts_with('/') {
                expanded = Some(format!("{home_or_tilde}{rest}"));
                break;
            }
            if prefix == "~" {
                // `~root`, `~user/…`: another user's home directory.
                return true;
            }
        }
    }
    let t = expanded.unwrap_or_else(|| t.to_owned());
    if !t.starts_with('/') {
        // Relative paths (and `~` with no known home, already expanded to
        // itself) are checked only when they start at home.
        return t == "~"
            || (t.starts_with("~/") && fixed_prefix(&t[1..]).trim_matches('/').is_empty());
    }
    let fixed = fixed_prefix(&t);
    let Some(path) = normalise_path(fixed) else {
        return true;
    };
    path == "/"
        || PROTECTED_DIRS.contains(&path.as_str())
        || home.is_some_and(|home| home == path || home.starts_with(&format!("{path}/")))
}

/// The part of `path` before its first pattern character, cut back to the
/// last `/`: `/usr/l*` gives `/usr/`, `/tmp/x` gives itself.
fn fixed_prefix(path: &str) -> &str {
    match path.find(['*', '?', '[', '{']) {
        Some(at) => &path[..path[..at].rfind('/').map_or(0, |slash| slash + 1)],
        None => path,
    }
}

/// `path` with `.`, `..` and repeated or trailing `/` removed, lexically
/// (`..` at the root stays at the root). `None` when it is not absolute.
fn normalise_path(path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    Some(format!("/{}", parts.join("/")))
}
