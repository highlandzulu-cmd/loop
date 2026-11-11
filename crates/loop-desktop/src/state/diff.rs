//! Pending file edits for diff review.

use std::path::PathBuf;

/// A file edit awaiting accept/reject in the diff panel.
#[derive(Debug, Clone)]
pub struct PendingFileChange {
    pub id: String,
    pub path: PathBuf,
    /// The previous text, for the diff view (`None` when it is not text).
    pub before: Option<String>,
    /// The previous bytes, which a reject writes back; `None` when they
    /// were not recorded, and a reject then refuses.
    pub original: Option<Vec<u8>>,
    pub after: String,
    pub added: usize,
    pub removed: usize,
    pub reviewed: bool,
}

impl PendingFileChange {
    pub fn from_paths(path: PathBuf, before: Option<String>, after: String) -> Self {
        let original = before.clone().map(String::into_bytes);
        Self::from_original(path, original, after)
    }

    /// A change whose previous contents were `original` (any bytes).
    pub fn from_original(path: PathBuf, original: Option<Vec<u8>>, after: String) -> Self {
        let before = original
            .as_ref()
            .and_then(|bytes| String::from_utf8(bytes.clone()).ok());
        let (added, removed) = diff_stats(before.as_deref().unwrap_or(""), &after);
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            path,
            before,
            original,
            after,
            added,
            removed,
            reviewed: false,
        }
    }
}

/// Compute line add/remove counts with similar.
pub fn diff_stats(before: &str, after: &str) -> (usize, usize) {
    let diff = similar::TextDiff::from_lines(before, after);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

/// One line in a chat-card diff preview.
#[derive(Debug, Clone)]
pub struct DiffPreviewLine {
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    pub tag: similar::ChangeTag,
    pub text: String,
}

/// Compact line-oriented preview favoring changed lines (with light equal context).
pub fn preview_diff_lines(before: &str, after: &str, max_lines: usize) -> Vec<DiffPreviewLine> {
    if max_lines == 0 {
        return Vec::new();
    }
    let diff = similar::TextDiff::from_lines(before, after);
    let mut out = Vec::new();
    let mut old_no = 1usize;
    let mut new_no = 1usize;
    let mut equal_run: Vec<DiffPreviewLine> = Vec::new();

    let flush_equal = |equal_run: &mut Vec<DiffPreviewLine>, out: &mut Vec<DiffPreviewLine>| {
        if equal_run.is_empty() {
            return;
        }
        // Keep a single trailing context line before changes.
        if let Some(last) = equal_run.pop() {
            out.push(last);
        }
        equal_run.clear();
    };

    for change in diff.iter_all_changes() {
        let text = change.value().trim_end_matches('\n').to_string();
        match change.tag() {
            similar::ChangeTag::Equal => {
                equal_run.push(DiffPreviewLine {
                    old_no: Some(old_no),
                    new_no: Some(new_no),
                    tag: similar::ChangeTag::Equal,
                    text,
                });
                old_no += 1;
                new_no += 1;
            }
            similar::ChangeTag::Delete => {
                flush_equal(&mut equal_run, &mut out);
                out.push(DiffPreviewLine {
                    old_no: Some(old_no),
                    new_no: None,
                    tag: similar::ChangeTag::Delete,
                    text,
                });
                old_no += 1;
            }
            similar::ChangeTag::Insert => {
                flush_equal(&mut equal_run, &mut out);
                out.push(DiffPreviewLine {
                    old_no: None,
                    new_no: Some(new_no),
                    tag: similar::ChangeTag::Insert,
                    text,
                });
                new_no += 1;
            }
        }
        if out.len() >= max_lines {
            break;
        }
    }

    // If the file was only equal / tiny, show the start of the after file.
    if out.is_empty() {
        for (i, line) in after.lines().take(max_lines).enumerate() {
            out.push(DiffPreviewLine {
                old_no: Some(i + 1),
                new_no: Some(i + 1),
                tag: similar::ChangeTag::Equal,
                text: line.to_string(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_writes_back_binary_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.docx");
        let original = vec![b'P', b'K', 3, 4, 0xff, 0x00, 0x80];
        std::fs::write(&path, b"rebuilt").unwrap();
        let change =
            PendingFileChange::from_original(path.clone(), Some(original.clone()), "# x".into());
        assert_eq!(change.before, None, "binary is not shown as text");
        reject_change(&change).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn reject_without_the_original_refuses_and_leaves_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"user edits").unwrap();
        let change = PendingFileChange::from_original(path.clone(), None, "x".into());
        assert!(reject_change(&change).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"user edits");
    }

    #[test]
    fn preview_prefers_changed_lines() {
        let before = "a\nb\nc\n";
        let after = "a\nB\nc\n";
        let lines = preview_diff_lines(before, after, 10);
        assert!(lines.iter().any(|l| l.tag == similar::ChangeTag::Delete));
        assert!(lines.iter().any(|l| l.tag == similar::ChangeTag::Insert));
    }
}

/// Reject a pending change by writing its previous bytes back. Without
/// them it refuses: a `git checkout` would drop untracked files and the
/// user's uncommitted edits, and a document cannot be rebuilt from text.
pub fn reject_change(change: &PendingFileChange) -> anyhow::Result<()> {
    let Some(original) = &change.original else {
        anyhow::bail!(
            "could not revert {}: its previous contents were not recorded",
            change.path.display()
        );
    };
    if let Some(parent) = change.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&change.path, original)?;
    Ok(())
}
