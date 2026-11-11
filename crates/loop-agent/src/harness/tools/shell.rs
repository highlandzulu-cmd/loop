//! Shell command splitting shared by the bash safety block list and the
//! permission rules: parts at `;`, `&&`, `||`, `|`, `&` and newlines, and
//! words with quotes and escapes removed, as the shell reads them.

/// The parts of a shell command, split at `;`, `&&`, `||`, `|`, `&` and
/// newlines outside quotes. `None` when it substitutes commands (`$(…)`,
/// backticks, `<(…)`) or has unbalanced quotes.
pub fn split_command(command: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let (mut single, mut double) = (false, false);
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // An escaped character never opens a quote or ends a part, as
            // in the shell (and in `words`).
            '\\' if !single => {
                current.push(c);
                current.extend(chars.next());
                continue;
            }
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '`' if !single => return None,
            '$' | '<' | '>' if !single && chars.peek() == Some(&'(') => return None,
            ';' | '\n' | '&' | '|' if !single && !double => {
                if matches!(chars.peek(), Some('&' | '|')) {
                    chars.next();
                }
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if single || double {
        return None;
    }
    parts.push(current);
    let parts: Vec<String> = parts
        .into_iter()
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect();
    (!parts.is_empty()).then_some(parts)
}

/// The words of one command part, with quotes and escapes removed. `None`
/// when it redirects output (an unquoted `>`), unless `redirects` is true:
/// then `>` is kept as a character.
pub fn split_words(part: &str, redirects: bool) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let (mut single, mut double, mut started) = (false, false, false);
    let mut chars = part.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => {
                single = !single;
                started = true;
            }
            '"' if !single => {
                double = !double;
                started = true;
            }
            '\\' if !single => current.extend(chars.next()),
            '>' if !single && !double && !redirects => return None,
            c if c.is_whitespace() && !single && !double => {
                if started || !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                started = false;
            }
            c => current.push(c),
        }
    }
    if started || !current.is_empty() {
        words.push(current);
    }
    Some(words)
}
