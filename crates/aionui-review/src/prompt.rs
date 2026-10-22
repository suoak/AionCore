//! `[[AION_REVIEW]]` block sent to an agent for a review batch (design §6.3).
//!
//! Pure text assembly. Code snippets come from repository content and are
//! untrusted, so they always sit inside a code fence that is longer than any
//! backtick run they contain. Annotation bodies are user input and stay plain
//! text; the block's own closing marker is neutralized inside them. The block
//! stays under [`REVIEW_BLOCK_MAX_BYTES`]: when snippets would exceed it, only
//! file and line references are sent, and as a last resort the comment bodies
//! are shortened.

use crate::anchor::Anchor;
use crate::types::{ReviewCommentAuthor, ReviewSide, ReviewThread, ReviewThreadStatus};

/// Upper bound of a rendered block, in bytes.
pub const REVIEW_BLOCK_MAX_BYTES: usize = 24 * 1024;
/// Snippet lines kept per annotation before truncating.
pub const SNIPPET_MAX_LINES: usize = 40;
/// Comment length kept per comment when the block is still too long without
/// snippets.
const COMPACT_COMMENT_MAX_CHARS: usize = 400;

const OPEN_TAG: &str = "[[AION_REVIEW";
const CLOSE_TAG: &str = "[[/AION_REVIEW]]";
const DEFAULT_PREFACE: &str = "以下是用户对当前改动的逐行批注。请逐条处理；不同意或无法处理时说明原因。\n完成后按编号简要回报每条的处理结果（格式：`#n ✅/⚠️ 说明`）。";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Detail {
    Full,
    NoSnippets,
    Compact,
}

/// Render the block for `threads` (in the order given; numbering is 1-based).
pub fn render_review_block(batch_id: &str, threads: &[ReviewThread], preface: Option<&str>) -> String {
    for detail in [Detail::Full, Detail::NoSnippets] {
        let text = render(batch_id, threads, preface, detail);
        if text.len() <= REVIEW_BLOCK_MAX_BYTES {
            return text;
        }
    }
    let text = render(batch_id, threads, preface, Detail::Compact);
    if text.len() <= REVIEW_BLOCK_MAX_BYTES {
        return text;
    }
    // Pathological input (thousands of threads): cut whole lines and close the block.
    let note = "\n…（批注过多，其余内容已截断）\n";
    let budget = REVIEW_BLOCK_MAX_BYTES - note.len() - CLOSE_TAG.len() - 1;
    let mut cut = budget.min(text.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let cut = text[..cut].rfind('\n').unwrap_or(cut);
    format!("{}{note}{CLOSE_TAG}\n", &text[..cut])
}

fn render(batch_id: &str, threads: &[ReviewThread], preface: Option<&str>, detail: Detail) -> String {
    let mut out = format!("{OPEN_TAG} batch={batch_id} count={}]]\n", threads.len());
    let preface = preface
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(DEFAULT_PREFACE);
    out.push_str(&neutralize(preface));
    out.push('\n');
    if detail != Detail::Full {
        out.push_str("（内容较长，以下只列出文件和行号，请自行查看对应代码。）\n");
    }
    for (index, thread) in threads.iter().enumerate() {
        out.push('\n');
        render_thread(&mut out, index + 1, thread, detail);
    }
    out.push_str(CLOSE_TAG);
    out.push('\n');
    out
}

fn render_thread(out: &mut String, number: usize, thread: &ReviewThread, detail: Detail) {
    let side = match thread.side {
        ReviewSide::New => "新版本",
        ReviewSide::Old => "旧版本",
    };
    let range = if thread.start_line == thread.end_line {
        format!("L{}", thread.start_line)
    } else {
        format!("L{}–L{}", thread.start_line, thread.end_line)
    };
    let outdated = thread.status == ReviewThreadStatus::Outdated;
    let note = if outdated {
        "（已过期，原位置附近代码如下）"
    } else if thread.moved_fuzzy {
        "（代码已改动，批注时的原文如下）"
    } else {
        ""
    };
    out.push_str(&format!("#{number} {}  {range}（{side}）{note}\n", thread.file_path));
    if detail == Detail::Full {
        let lines = snippet_lines(&thread.anchor, outdated || thread.moved_fuzzy);
        if !lines.is_empty() {
            push_fenced(out, &lines, language_hint(&thread.file_path));
        }
    }
    let mut comments = thread.comments.iter();
    if let Some(first) = comments.next() {
        out.push_str("批注：");
        out.push_str(&comment_text(&first.body, detail));
        out.push('\n');
    }
    for reply in comments {
        let who = match reply.author {
            ReviewCommentAuthor::User => "用户回复",
            ReviewCommentAuthor::Agent => "Agent 回复",
        };
        out.push_str(&format!("{who}：{}\n", comment_text(&reply.body, detail)));
    }
}

/// Lines to show: what the user annotated (original text once it moved or
/// went stale), else the current anchored lines.
fn snippet_lines(anchor: &Anchor, prefer_original: bool) -> Vec<String> {
    let source = if prefer_original {
        anchor.original_lines.as_ref().unwrap_or(&anchor.lines)
    } else {
        &anchor.lines
    };
    source.clone()
}

fn push_fenced(out: &mut String, lines: &[String], language: &str) {
    let kept = &lines[..lines.len().min(SNIPPET_MAX_LINES)];
    let longest_run = kept.iter().map(|line| longest_backtick_run(line)).max().unwrap_or(0);
    let fence = "`".repeat((longest_run + 1).max(3));
    out.push_str(&fence);
    out.push_str(language);
    out.push('\n');
    for line in kept {
        out.push_str(line.trim_end_matches(['\r', '\n']));
        out.push('\n');
    }
    if lines.len() > kept.len() {
        out.push_str(&format!("…（另有 {} 行已截断）\n", lines.len() - kept.len()));
    }
    out.push_str(&fence);
    out.push('\n');
}

fn longest_backtick_run(line: &str) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for ch in line.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

fn comment_text(body: &str, detail: Detail) -> String {
    let body = neutralize(body.trim());
    if detail != Detail::Compact || body.chars().count() <= COMPACT_COMMENT_MAX_CHARS {
        return body;
    }
    let cut: String = body.chars().take(COMPACT_COMMENT_MAX_CHARS).collect();
    format!("{cut}…（已截断）")
}

/// Keep user text from opening or closing a review block of its own.
fn neutralize(text: &str) -> String {
    text.replace(CLOSE_TAG, "[ [/AION_REVIEW]]")
        .replace(OPEN_TAG, "[ [AION_REVIEW")
}

fn language_hint(path: &str) -> &'static str {
    let ext = path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "ts",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" => "js",
        "jsx" => "jsx",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" | "cxx" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "sh" | "bash" => "bash",
        "sql" => "sql",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" => "markdown",
        "css" => "css",
        "html" => "html",
        _ => "",
    }
}

#[cfg(test)]
#[path = "prompt_test.rs"]
mod tests;
