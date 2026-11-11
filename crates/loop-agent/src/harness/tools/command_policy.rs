//! The bash safety block list: commands refused whatever the user allows.

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
/// target is the filesystem root or the home directory (`/`, `/*`, `~`,
/// `$HOME`, …), so `chmod -R u+w ./src` and `rm -rf /tmp/x` run.
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

    for words in &parts {
        if depth < MAX_NESTING {
            for script in nested_scripts(words) {
                check_nested(&script, extra_blocked, depth + 1)?;
            }
        }
        if let Some(blocked) = recursive_on_root(words) {
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

/// Scripts that `words` run through a shell: the argument of `sh -c`
/// (`bash`, `zsh`, …) and the words after `eval`.
fn nested_scripts(words: &[String]) -> Vec<String> {
    let mut scripts = Vec::new();
    for (i, word) in words.iter().enumerate() {
        let name = word.rsplit('/').next().unwrap_or(word);
        if name == "eval" {
            scripts.push(words[i + 1..].join(" "));
        } else if SHELLS.contains(&name) {
            let mut args = words[i + 1..].iter();
            // `-c`, or combined flags such as `-lc`, then the script.
            if args.any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c')) {
                scripts.extend(args.find(|a| !a.starts_with('-')).cloned());
            }
        }
    }
    scripts
}

/// Shells whose `-c` script is checked like the command itself.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

/// The program, when `words` (lowercased) run `rm`, `chmod`, `chown` or
/// `chgrp` recursively on a root or home target. Any word naming one of them
/// starts its arguments, so wrappers (`sudo`, `env`, `xargs`, …) do not hide it.
fn recursive_on_root(words: &[String]) -> Option<&'static str> {
    for (i, word) in words.iter().enumerate() {
        let name = word.rsplit('/').next().unwrap_or(word);
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
        if recursive && targets.iter().any(|t| is_root_or_home(t)) {
            return Some(program);
        }
    }
    None
}

/// Whether `target` (lowercased) names the filesystem root or the home
/// directory, or everything in one: `/`, `//`, `/.`, `/*`, `~`, `~/`, `~/*`,
/// `$home`, `${home}/*`, ….
fn is_root_or_home(target: &str) -> bool {
    let mut t = target.trim();
    for home in ["~", "$home", "${home}"] {
        if let Some(rest) = t.strip_prefix(home) {
            if rest.is_empty() || rest.starts_with('/') {
                t = rest;
                break;
            }
        }
    }
    // What is left must be only separators, `.` and `*`: the directory itself
    // or every entry in it.
    t.split('/').all(|seg| matches!(seg, "" | "." | "*" | ".*"))
        && (t.is_empty() || t.starts_with('/'))
}
