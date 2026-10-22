use super::*;

/// Lines → LF text with a trailing newline.
fn lf<S: AsRef<str>>(lines: &[S]) -> String {
    let mut text = lines.iter().map(AsRef::as_ref).collect::<Vec<_>>().join("\n");
    text.push('\n');
    text
}

fn crlf<S: AsRef<str>>(lines: &[S]) -> String {
    let mut text = lines.iter().map(AsRef::as_ref).collect::<Vec<_>>().join("\r\n");
    text.push_str("\r\n");
    text
}

const BASE: &[&str] = &[
    "use std::io;",         // 1
    "",                     // 2
    "fn main() {",          // 3
    "    let a = 1;",       // 4
    "    let b = 2;",       // 5
    "    println!(a + b);", // 6
    "}",                    // 7
    "",                     // 8
    "fn helper() {}",       // 9
];

fn anchor_on(text: &str, start: u32, end: u32) -> Anchor {
    Anchor::from_text(text, start, end, Some("WORKTREE".into()), None).unwrap()
}

/// One table row: the original file, the selection, the revised file and
/// the expected outcome.
struct Case {
    name: &'static str,
    original: String,
    start: u32,
    end: u32,
    revised: String,
    expect: Outcome,
}

fn moved(start_line: u32, end_line: u32) -> Outcome {
    Outcome::Moved {
        start_line,
        end_line,
        fuzzy: false,
    }
}

fn fuzzy(start_line: u32, end_line: u32) -> Outcome {
    Outcome::Moved {
        start_line,
        end_line,
        fuzzy: true,
    }
}

fn with(lines: &[&'static str], edit: impl FnOnce(&mut Vec<&'static str>)) -> Vec<&'static str> {
    let mut v = lines.to_vec();
    edit(&mut v);
    v
}

fn cases() -> Vec<Case> {
    let dup = [
        "fn a() {",
        "    // section a",
        "    retry();",
        "    log();",
        "}",
        "",
        "fn b() {",
        "    // section b",
        "    retry();",
        "    log();",
        "}",
    ];
    let same_ctx = ["a();", "retry();", "b();", "c();", "retry();", "d();"];
    vec![
        Case {
            name: "unchanged content keeps lines",
            original: lf(BASE),
            start: 4,
            end: 5,
            revised: lf(BASE),
            expect: Outcome::Unchanged,
        },
        Case {
            name: "three lines inserted above",
            original: lf(BASE),
            start: 4,
            end: 5,
            revised: lf(&with(BASE, |v| {
                v.insert(1, "use std::fs;");
                v.insert(1, "use std::env;");
                v.insert(1, "use std::path;");
            })),
            expect: moved(7, 8),
        },
        Case {
            name: "two lines deleted above",
            original: lf(BASE),
            start: 4,
            end: 5,
            revised: lf(&with(BASE, |v| {
                v.drain(0..2);
            })),
            expect: moved(2, 3),
        },
        Case {
            name: "lines inserted below only refresh the anchor in place",
            original: lf(BASE),
            start: 4,
            end: 5,
            revised: lf(&with(BASE, |v| v.push("fn more() {}"))),
            expect: moved(4, 5),
        },
        Case {
            name: "line inserted inside the selection's context",
            original: lf(BASE),
            start: 5,
            end: 5,
            revised: lf(&with(BASE, |v| v.insert(4, "    let c = 3;"))),
            expect: moved(6, 6),
        },
        Case {
            name: "duplicate code: context picks the second block",
            original: lf(&dup),
            start: 9,
            end: 10,
            revised: lf(&with(&dup, |v| v.insert(0, "// header"))),
            expect: moved(10, 11),
        },
        Case {
            name: "duplicate code: context picks the first block even when it moved far",
            original: lf(&dup),
            start: 3,
            end: 4,
            revised: lf(&with(&dup, |v| {
                for _ in 0..10 {
                    v.insert(0, "// pad");
                }
            })),
            expect: moved(13, 14),
        },
        Case {
            name: "duplicate code, all context rewritten: nearest to old position",
            original: lf(&same_ctx),
            start: 2,
            end: 2,
            revised: lf(&["retry();", "q();", "q();", "q();", "retry();", "q();"]),
            expect: moved(1, 1),
        },
        Case {
            name: "duplicate code, all context rewritten: nearest picks the later copy",
            original: lf(&same_ctx),
            start: 5,
            end: 5,
            revised: lf(&["retry();", "q();", "q();", "q();", "retry();", "q();"]),
            expect: moved(5, 5),
        },
        Case {
            name: "duplicate code: the other copy was edited, ours stays",
            original: lf(&dup),
            start: 9,
            end: 9,
            revised: lf(&with(&dup, |v| v[2] = "    retry_twice();")),
            expect: moved(9, 9),
        },
        Case {
            name: "selected line rewritten: fuzzy by context",
            original: lf(BASE),
            start: 5,
            end: 5,
            revised: lf(&with(BASE, |v| v[4] = "    let b = compute_b();")),
            expect: fuzzy(5, 5),
        },
        Case {
            name: "selected lines rewritten into more lines",
            original: lf(BASE),
            start: 4,
            end: 5,
            revised: lf(&with(BASE, |v| {
                v.splice(
                    3..5,
                    ["    let a = load_a()?;", "    let b = load_b()?;", "    let c = a * b;"],
                );
            })),
            expect: fuzzy(4, 6),
        },
        Case {
            name: "rewritten and shifted down",
            original: lf(BASE),
            start: 6,
            end: 6,
            revised: lf(&with(BASE, |v| {
                v[5] = "    tracing::info!(sum = a + b);";
                v.insert(0, "#![allow(unused)]");
            })),
            expect: fuzzy(7, 7),
        },
        Case {
            name: "selected block deleted outright: outdated",
            original: lf(BASE),
            start: 4,
            end: 6,
            revised: lf(&with(BASE, |v| {
                v.drain(3..6);
            })),
            expect: Outcome::Outdated,
        },
        Case {
            name: "whole function deleted with its context: outdated",
            original: lf(BASE),
            start: 4,
            end: 5,
            revised: lf(&["use std::io;", "", "fn helper() {}"]),
            expect: Outcome::Outdated,
        },
        Case {
            name: "file emptied: outdated",
            original: lf(BASE),
            start: 4,
            end: 4,
            revised: String::new(),
            expect: Outcome::Outdated,
        },
        Case {
            name: "LF to CRLF conversion only: unchanged",
            original: lf(BASE),
            start: 4,
            end: 5,
            revised: crlf(BASE),
            expect: Outcome::Unchanged,
        },
        Case {
            name: "CRLF original, LF revision with lines inserted above",
            original: crlf(BASE),
            start: 4,
            end: 5,
            revised: lf(&with(BASE, |v| v.insert(0, "// new"))),
            expect: moved(5, 6),
        },
        Case {
            name: "mixed CRLF/LF file revised with CRLF lines above",
            original: "use std::io;\r\n\nfn main() {\r\n    let a = 1;\n    let b = 2;\r\n}\n".into(),
            start: 4,
            end: 5,
            revised: "// a\r\n// b\r\nuse std::io;\n\r\nfn main() {\n    let a = 1;\r\n    let b = 2;\n}\r\n".into(),
            expect: moved(6, 7),
        },
        Case {
            name: "indentation change only: whitespace-insensitive match",
            original: lf(BASE),
            start: 4,
            end: 5,
            revised: lf(&with(BASE, |v| {
                v[3] = "        let a = 1;";
                v[4] = "\tlet b = 2;";
                v.insert(0, "");
            })),
            expect: moved(5, 6),
        },
        Case {
            name: "selection at top of file rewritten (no before context)",
            original: lf(BASE),
            start: 1,
            end: 1,
            revised: lf(&with(BASE, |v| v[0] = "use std::io::{self, Write};")),
            expect: fuzzy(1, 1),
        },
        Case {
            name: "selection at end of file rewritten (no after context)",
            original: lf(BASE),
            start: 9,
            end: 9,
            revised: lf(&with(BASE, |v| v[8] = "fn helper() -> u8 { 0 }")),
            expect: fuzzy(9, 9),
        },
        Case {
            name: "block moved below another function",
            original: lf(BASE),
            start: 9,
            end: 9,
            revised: lf(&["fn helper() {}", "", "use std::io;", "", "fn main() {", "}"]),
            expect: moved(1, 1),
        },
        Case {
            name: "file without trailing newline gains one",
            original: BASE.join("\n"),
            start: 6,
            end: 6,
            revised: lf(BASE),
            expect: Outcome::Unchanged,
        },
    ]
}

#[test]
fn reanchor_table() {
    let mut failures = Vec::new();
    for case in cases() {
        let anchor = anchor_on(&case.original, case.start, case.end);
        let got = reanchor(&anchor, case.start, &case.revised);
        if got != case.expect {
            failures.push(format!("{}: expected {:?}, got {:?}", case.name, case.expect, got));
        }
    }
    assert!(failures.is_empty(), "reanchor failures:\n{}", failures.join("\n"));
    assert!(cases().len() >= 20);
}

#[test]
fn moved_outcome_points_at_the_same_text() {
    for case in cases() {
        let anchor = anchor_on(&case.original, case.start, case.end);
        if let Outcome::Moved {
            start_line,
            end_line,
            fuzzy: false,
        } = reanchor(&anchor, case.start, &case.revised)
        {
            let rebased = anchor.rebased(&case.revised, start_line, end_line, false).unwrap();
            let same = rebased
                .lines
                .iter()
                .zip(&anchor.lines)
                .all(|(a, b)| a.trim() == b.trim());
            assert!(same, "{}: rebased lines differ", case.name);
            assert_eq!(
                rebased.content_sha.as_deref(),
                Some(content_sha(&case.revised).as_str())
            );
        }
    }
}

#[test]
fn rebased_after_fuzzy_keeps_first_snapshot() {
    let original = lf(BASE);
    let anchor = anchor_on(&original, 5, 5);
    let rev1 = lf(&with(BASE, |v| v[4] = "    let b = two();"));
    let Outcome::Moved {
        start_line,
        end_line,
        fuzzy,
    } = reanchor(&anchor, 5, &rev1)
    else {
        panic!("expected a move");
    };
    assert!(fuzzy);
    let a1 = anchor.rebased(&rev1, start_line, end_line, fuzzy).unwrap();
    assert!(a1.moved_fuzzy);
    assert_eq!(a1.lines, vec!["    let b = two();"]);
    assert_eq!(a1.original_lines, Some(vec!["    let b = 2;".to_owned()]));

    // A later exact move keeps the flag and the very first snapshot.
    let rev2 = lf(&with(BASE, |v| {
        v[4] = "    let b = two();";
        v.insert(0, "// moved");
    }));
    let Outcome::Moved {
        start_line,
        end_line,
        fuzzy,
    } = reanchor(&a1, 5, &rev2)
    else {
        panic!("expected a move");
    };
    assert_eq!((start_line, end_line, fuzzy), (6, 6, false));
    let a2 = a1.rebased(&rev2, start_line, end_line, fuzzy).unwrap();
    assert!(a2.moved_fuzzy);
    assert_eq!(a2.original_lines, Some(vec!["    let b = 2;".to_owned()]));
}

#[test]
fn content_sha_ignores_line_endings_only() {
    assert_eq!(content_sha(&lf(BASE)), content_sha(&crlf(BASE)));
    assert_eq!(content_sha("a\r\nb\nc"), content_sha("a\nb\r\nc\n"));
    assert_ne!(content_sha("a\nb\n"), content_sha("a\nb \n"));
}

#[test]
fn split_lines_handles_crlf_lf_and_trailing_newline() {
    assert_eq!(split_lines(""), Vec::<&str>::new());
    assert_eq!(split_lines("a"), vec!["a"]);
    assert_eq!(split_lines("a\n"), vec!["a"]);
    assert_eq!(split_lines("a\r\nb\nc\r\n"), vec!["a", "b", "c"]);
    assert_eq!(split_lines("a\n\nb\n"), vec!["a", "", "b"]);
}

#[test]
fn from_text_captures_context_and_rejects_bad_ranges() {
    let text = lf(BASE);
    let anchor = anchor_on(&text, 4, 5);
    assert_eq!(anchor.before, vec!["use std::io;", "", "fn main() {"]);
    assert_eq!(anchor.lines, vec!["    let a = 1;", "    let b = 2;"]);
    assert_eq!(anchor.after, vec!["    println!(a + b);", "}", ""]);

    let top = anchor_on(&text, 1, 1);
    assert!(top.before.is_empty());
    let bottom = anchor_on(&text, 9, 9);
    assert!(bottom.after.is_empty());

    assert_eq!(
        Anchor::from_text(&text, 0, 1, None, None),
        Err(AnchorError::InvalidRange { start: 0, end: 1 })
    );
    assert_eq!(
        Anchor::from_text(&text, 3, 2, None, None),
        Err(AnchorError::InvalidRange { start: 3, end: 2 })
    );
    assert_eq!(
        Anchor::from_text(&text, 9, 10, None, None),
        Err(AnchorError::OutOfRange {
            start: 9,
            end: 10,
            len: 9
        })
    );
}

#[test]
fn context_only_anchor_normalizes_and_trims_context() {
    let before: Vec<String> = ["l1", "l2", "l3", "l4\r"].map(String::from).to_vec();
    let lines: Vec<String> = ["sel\r"].map(String::from).to_vec();
    let after: Vec<String> = ["a1\r", "a2", "a3", "a4"].map(String::from).to_vec();
    let anchor = Anchor::from_context(&before, &lines, &after, None, Some("@@ -1 +1 @@".into()));
    assert_eq!(anchor.before, vec!["l2", "l3", "l4"]);
    assert_eq!(anchor.lines, vec!["sel"]);
    assert_eq!(anchor.after, vec!["a1", "a2", "a3"]);
    assert!(anchor.content_sha.is_none());

    // A context-only anchor still re-anchors by content.
    let text = lf(&["l1", "l2", "l3", "l4", "new", "sel", "a1", "a2", "a3", "a4"]);
    assert_eq!(reanchor(&anchor, 5, &text), moved(6, 6));
}

#[test]
fn empty_selection_is_outdated() {
    let anchor = Anchor::from_context(&[], &[], &[], None, None);
    assert_eq!(reanchor(&anchor, 1, "anything\n"), Outcome::Outdated);
}

#[test]
fn matches_at_checks_exact_text() {
    let text = lf(BASE);
    let anchor = anchor_on(&text, 4, 5);
    assert!(anchor.matches_at(&text, 4, 5));
    assert!(anchor.matches_at(&crlf(BASE), 4, 5));
    assert!(!anchor.matches_at(&text, 5, 6));
    assert!(!anchor.matches_at(&text, 9, 10));
}

#[test]
fn anchor_json_round_trips_and_omits_defaults() {
    let anchor = anchor_on(&lf(BASE), 4, 5);
    let json = serde_json::to_value(&anchor).unwrap();
    assert_eq!(json["v"], 1);
    assert!(json.get("moved_fuzzy").is_none());
    assert!(json.get("original_lines").is_none());
    let back: Anchor = serde_json::from_value(json).unwrap();
    assert_eq!(back, anchor);
}
