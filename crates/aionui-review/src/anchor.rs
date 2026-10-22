//! Line anchors for diff annotations and lazy re-anchoring.
//!
//! Design: `E4-diff-annotation-commit-pr.md` §5.2. Everything here is a pure
//! function over text so Core and UI see the same result (the UI never
//! re-anchors on its own; it reads the outcome from the API).
//!
//! Line endings are normalized before any comparison: a file that flips
//! between CRLF and LF (or mixes them) is the same content for anchoring, and
//! [`content_sha`] hashes the normalized form.
//!
//! [`reanchor`] tries, in order:
//! 1. `content_sha` unchanged → keep the line numbers;
//! 2. exact match of the selected lines (several hits → best `before`/`after`
//!    context similarity, then nearest to the old position);
//! 3. the same with leading/trailing whitespace ignored;
//! 4. a `before` … `after` sandwich around a non-empty region (the selection
//!    was rewritten) → `fuzzy`;
//! 5. otherwise → outdated (the original snapshot stays in the anchor).

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

/// Anchor JSON schema version.
pub const ANCHOR_VERSION: u32 = 1;
/// Context lines captured above and below the selection.
pub const CONTEXT_LINES: usize = 3;
/// How much longer than the original selection a rewritten region may grow
/// before the sandwich match is considered unrelated code.
const MAX_FUZZY_GROWTH: usize = 50;

/// Persisted anchor (`review_threads.anchor`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub v: u32,
    /// Base the side was read from: a commit, `INDEX` or `WORKTREE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// sha1 of the side's normalized text when the anchor was (re)built.
    /// `None` when the server could not read the side (context-only anchor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hunk_header: Option<String>,
    #[serde(default)]
    pub before: Vec<String>,
    pub lines: Vec<String>,
    #[serde(default)]
    pub after: Vec<String>,
    /// The selected lines were rewritten at least once since the annotation
    /// was made; the thread now points at the region that replaced them.
    #[serde(default, skip_serializing_if = "is_false")]
    pub moved_fuzzy: bool,
    /// Selected lines as the user saw them, kept once the selection changed
    /// (fuzzy move). Sent along with the annotation so the agent sees what
    /// the comment was about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_lines: Option<Vec<String>>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Why an anchor could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnchorError {
    #[error("line range {start}-{end} is outside the file ({len} lines)")]
    OutOfRange { start: u32, end: u32, len: usize },
    #[error("invalid line range {start}-{end}")]
    InvalidRange { start: u32, end: u32 },
}

/// Result of [`reanchor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Content is unchanged; keep the stored line numbers.
    Unchanged,
    /// The selection now lives at `start_line..=end_line` (1-based).
    /// `fuzzy` = found only by its surrounding context (the lines changed).
    Moved {
        start_line: u32,
        end_line: u32,
        fuzzy: bool,
    },
    /// The selection cannot be located any more.
    Outdated,
}

/// Split text into lines with line endings removed. `\r\n`, `\n` and a lone
/// trailing `\r` are all accepted, so CRLF, LF and mixed files compare equal.
/// A final line terminator does not produce an extra empty line.
pub fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    body.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// Normalize one line coming from a client (diff text may carry `\r`).
pub fn normalize_line(line: &str) -> String {
    line.trim_end_matches(['\r', '\n']).to_owned()
}

/// sha1 (hex) of the line-ending-normalized text.
pub fn content_sha(text: &str) -> String {
    let mut hasher = Sha1::new();
    for (index, line) in split_lines(text).iter().enumerate() {
        if index > 0 {
            hasher.update(b"\n");
        }
        hasher.update(line.as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn check_range(start: u32, end: u32, len: usize) -> Result<(usize, usize), AnchorError> {
    if start == 0 || end < start {
        return Err(AnchorError::InvalidRange { start, end });
    }
    if end as usize > len {
        return Err(AnchorError::OutOfRange { start, end, len });
    }
    Ok((start as usize - 1, end as usize))
}

impl Anchor {
    /// Build an anchor from the full side text (server-side, authoritative).
    pub fn from_text(
        text: &str,
        start_line: u32,
        end_line: u32,
        base: Option<String>,
        hunk_header: Option<String>,
    ) -> Result<Self, AnchorError> {
        let lines = split_lines(text);
        let (from, to) = check_range(start_line, end_line, lines.len())?;
        Ok(Self {
            v: ANCHOR_VERSION,
            base,
            content_sha: Some(content_sha(text)),
            hunk_header,
            before: owned(&lines[from.saturating_sub(CONTEXT_LINES)..from]),
            lines: owned(&lines[from..to]),
            after: owned(&lines[to..(to + CONTEXT_LINES).min(lines.len())]),
            moved_fuzzy: false,
            original_lines: None,
        })
    }

    /// Build an anchor from diff context only (the side is not readable on
    /// the server, e.g. a retained TaskRun diff). No `content_sha`.
    pub fn from_context(
        before: &[String],
        lines: &[String],
        after: &[String],
        base: Option<String>,
        hunk_header: Option<String>,
    ) -> Self {
        let before: Vec<String> = before.iter().map(|l| normalize_line(l)).collect();
        let after: Vec<String> = after.iter().map(|l| normalize_line(l)).collect();
        Self {
            v: ANCHOR_VERSION,
            base,
            content_sha: None,
            hunk_header,
            before: before[before.len().saturating_sub(CONTEXT_LINES)..].to_vec(),
            lines: lines.iter().map(|l| normalize_line(l)).collect(),
            after: after[..after.len().min(CONTEXT_LINES)].to_vec(),
            moved_fuzzy: false,
            original_lines: None,
        }
    }

    /// The anchor rebuilt at its new position in `text` after a move.
    /// `fuzzy` marks the selection as rewritten and keeps the first snapshot
    /// of the user's selection in `original_lines`.
    pub fn rebased(&self, text: &str, start_line: u32, end_line: u32, fuzzy: bool) -> Result<Self, AnchorError> {
        let mut next = Self::from_text(text, start_line, end_line, self.base.clone(), self.hunk_header.clone())?;
        next.moved_fuzzy = self.moved_fuzzy || fuzzy;
        next.original_lines = match (&self.original_lines, fuzzy) {
            (Some(original), _) => Some(original.clone()),
            (None, true) => Some(self.lines.clone()),
            (None, false) => None,
        };
        Ok(next)
    }

    /// Whether the selected text still equals `text[start..=end]` exactly.
    pub fn matches_at(&self, text: &str, start_line: u32, end_line: u32) -> bool {
        let lines = split_lines(text);
        match check_range(start_line, end_line, lines.len()) {
            Ok((from, to)) => eq_seq(&lines[from..to], &self.lines, false),
            Err(_) => false,
        }
    }
}

fn owned(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| (*line).to_owned()).collect()
}

fn line_eq(a: &str, b: &str, trimmed: bool) -> bool {
    if trimmed { a.trim() == b.trim() } else { a == b }
}

fn eq_seq(window: &[&str], wanted: &[String], trimmed: bool) -> bool {
    window.len() == wanted.len() && window.iter().zip(wanted).all(|(a, b)| line_eq(a, b, trimmed))
}

/// 0-based starts of every window equal to `wanted`.
fn find_all(lines: &[&str], wanted: &[String], trimmed: bool) -> Vec<usize> {
    let n = wanted.len();
    if n == 0 || n > lines.len() {
        return Vec::new();
    }
    (0..=lines.len() - n)
        .filter(|&i| eq_seq(&lines[i..i + n], wanted, trimmed))
        .collect()
}

/// How many `before`/`after` context lines agree around a candidate at
/// `start..end` (0-based, exclusive end). Whitespace-insensitive.
fn context_score(lines: &[&str], start: usize, end: usize, anchor: &Anchor) -> usize {
    let before = anchor
        .before
        .iter()
        .rev()
        .enumerate()
        .filter(|(k, want)| start > *k && line_eq(lines[start - 1 - k], want, true))
        .count();
    let after = anchor
        .after
        .iter()
        .enumerate()
        .filter(|(k, want)| lines.get(end + k).is_some_and(|line| line_eq(line, want, true)))
        .count();
    before + after
}

fn distance(candidate: usize, original_start: u32) -> usize {
    (candidate + 1).abs_diff(original_start as usize)
}

fn pick_exact(lines: &[&str], anchor: &Anchor, original_start: u32, trimmed: bool) -> Option<usize> {
    let n = anchor.lines.len();
    find_all(lines, &anchor.lines, trimmed).into_iter().max_by(|&a, &b| {
        let sa = context_score(lines, a, a + n, anchor);
        let sb = context_score(lines, b, b + n, anchor);
        // Higher score wins; on a tie the candidate nearer the old position
        // wins (max_by keeps the later element on Equal, so compare reversed
        // distance and break remaining ties towards the earlier index).
        sa.cmp(&sb)
            .then_with(|| distance(b, original_start).cmp(&distance(a, original_start)))
            .then_with(|| b.cmp(&a))
    })
}

/// Region `(start, end)` (0-based, exclusive end) between a `before` match
/// and an `after` match. Empty contexts bind to the file boundary they were
/// taken against (selection at the top / bottom of the file).
fn pick_sandwich(lines: &[&str], anchor: &Anchor, original_start: u32) -> Option<(usize, usize)> {
    if anchor.before.is_empty() && anchor.after.is_empty() {
        return None;
    }
    let starts: Vec<usize> = if anchor.before.is_empty() {
        vec![0]
    } else {
        find_all(lines, &anchor.before, true)
            .into_iter()
            .map(|i| i + anchor.before.len())
            .collect()
    };
    let ends: Vec<usize> = if anchor.after.is_empty() {
        vec![lines.len()]
    } else {
        find_all(lines, &anchor.after, true)
    };
    let max_span = anchor.lines.len() + MAX_FUZZY_GROWTH;
    let mut best: Option<(usize, usize)> = None;
    for &start in &starts {
        // The nearest `after` at or below this `before` closes the region.
        let Some(&end) = ends.iter().find(|&&end| end >= start) else {
            continue;
        };
        // An empty region means the selected block was removed outright.
        if end == start || end - start > max_span {
            continue;
        }
        let better = match best {
            None => true,
            Some((bs, be)) => {
                let (d, bd) = (distance(start, original_start), distance(bs, original_start));
                d < bd
                    || (d == bd && (end - start).abs_diff(anchor.lines.len()) < (be - bs).abs_diff(anchor.lines.len()))
            }
        };
        if better {
            best = Some((start, end));
        }
    }
    best
}

/// Locate a stored selection in the side's current text (design §5.2).
///
/// `start_line` is the stored (1-based) start; it only breaks ties between
/// equally good candidates.
pub fn reanchor(anchor: &Anchor, start_line: u32, new_text: &str) -> Outcome {
    if anchor.lines.is_empty() {
        return Outcome::Outdated;
    }
    if anchor.content_sha.as_deref() == Some(content_sha(new_text).as_str()) {
        return Outcome::Unchanged;
    }
    let lines = split_lines(new_text);
    let n = anchor.lines.len();
    let moved = |start: usize, end: usize, fuzzy: bool| Outcome::Moved {
        start_line: start as u32 + 1,
        end_line: end as u32,
        fuzzy,
    };

    for trimmed in [false, true] {
        if let Some(start) = pick_exact(&lines, anchor, start_line, trimmed) {
            // Also reported when the position is the same but the file
            // changed elsewhere, so the caller refreshes sha and context.
            return moved(start, start + n, false);
        }
    }
    if let Some((start, end)) = pick_sandwich(&lines, anchor, start_line) {
        return moved(start, end, true);
    }
    Outcome::Outdated
}

#[cfg(test)]
#[path = "anchor_test.rs"]
mod tests;
