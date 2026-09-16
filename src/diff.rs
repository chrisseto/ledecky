use std::path::Path;
use std::sync::OnceLock;

use serde::Serialize;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::html::{styled_line_to_highlighted_html, IncludeBackground};
use syntect::parsing::SyntaxSet;

use crate::git;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Serialize)]
pub struct Line {
    pub kind: LineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    /// Pre-highlighted HTML; the template inserts it verbatim.
    pub html: String,
    /// `<side>:<line>`, the key a comment anchors to.
    pub anchor: String,
}

#[derive(Debug, Serialize)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<Line>,
}

#[derive(Debug, Serialize)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub binary: bool,
    pub additions: u32,
    pub deletions: u32,
    pub hunks: Vec<Hunk>,
}

/// Renders `git diff <from> <to>` into something a template can walk.
pub fn between(repo: &Path, from: &str, to: &str) -> anyhow::Result<Vec<FileDiff>> {
    let raw = git::run(
        repo,
        &[
            "diff",
            "-U3",
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            from,
            to,
        ],
    )?;
    Ok(parse(&raw))
}

pub fn parse(raw: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut old_no = 0u32;
    let mut new_no = 0u32;

    for line in raw.lines() {
        if let Some(paths) = line.strip_prefix("diff --git ") {
            files.push(FileDiff {
                path: git_path(paths, 'b').unwrap_or_else(|| paths.to_owned()),
                old_path: git_path(paths, 'a'),
                binary: false,
                additions: 0,
                deletions: 0,
                hunks: Vec::new(),
            });
            continue;
        }

        let Some(file) = files.last_mut() else { continue };

        if line.starts_with("Binary files ") {
            file.binary = true;
            continue;
        }
        if line.starts_with("@@") {
            let (start_old, start_new) = hunk_starts(line);
            old_no = start_old;
            new_no = start_new;
            file.hunks.push(Hunk {
                header: line.to_owned(),
                lines: Vec::new(),
            });
            continue;
        }

        // Everything before the first @@ is header noise (index/mode/--- /+++).
        let Some(hunk) = file.hunks.last_mut() else {
            continue;
        };

        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            // "\ No newline at end of file" and friends.
            _ => continue,
        };

        let (old_line, new_line, anchor) = match kind {
            LineKind::Added => {
                new_no += 1;
                file.additions += 1;
                (None, Some(new_no), format!("new:{new_no}"))
            }
            LineKind::Removed => {
                old_no += 1;
                file.deletions += 1;
                (Some(old_no), None, format!("old:{old_no}"))
            }
            LineKind::Context => {
                old_no += 1;
                new_no += 1;
                (Some(old_no), Some(new_no), format!("new:{new_no}"))
            }
        };

        hunk.lines.push(Line {
            kind,
            old_line,
            new_line,
            html: text.to_owned(),
            anchor,
        });
    }

    for file in &mut files {
        highlight(file);
    }
    files
}

/// Colours each file's diff lines with syntect.
///
/// NB: hunks are discontiguous, so highlighting state carried across a gap can be
/// wrong for constructs that span the skipped lines. Reconstructing both full
/// blobs to avoid that costs far more than the occasional mis-tinted string.
fn highlight(file: &mut FileDiff) {
    let (syntaxes, theme) = assets();
    let syntax = Path::new(&file.path)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| syntaxes.find_syntax_by_extension(e))
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text());

    let mut highlighter = HighlightLines::new(syntax, theme);
    for hunk in &mut file.hunks {
        for line in &mut hunk.lines {
            let source = format!("{}\n", line.html);
            line.html = match highlighter.highlight_line(&source, syntaxes) {
                Ok(regions) => {
                    styled_line_to_highlighted_html(&regions, IncludeBackground::No)
                        .unwrap_or_else(|_| escape(&line.html))
                }
                Err(_) => escape(&line.html),
            };
        }
    }
}

fn assets() -> (&'static SyntaxSet, &'static Theme) {
    static ASSETS: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    let (syntaxes, theme) = ASSETS.get_or_init(|| {
        let themes = ThemeSet::load_defaults();
        let theme = themes.themes["base16-ocean.dark"].clone();
        (SyntaxSet::load_defaults_newlines(), theme)
    });
    (syntaxes, theme)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Pulls one side out of `a/src/foo.rs b/src/foo.rs`.
fn git_path(paths: &str, side: char) -> Option<String> {
    let prefix = format!("{side}/");
    paths
        .split(' ')
        .find(|p| p.starts_with(&prefix))
        .map(|p| p[2..].to_owned())
}

/// `@@ -12,7 +12,9 @@` -> the two start lines, minus one so the first line of the
/// hunk increments into place.
fn hunk_starts(header: &str) -> (u32, u32) {
    let mut old: u32 = 0;
    let mut new: u32 = 0;
    for token in header.split_whitespace() {
        let number = |t: &str| t[1..].split(',').next().and_then(|n| n.parse().ok());
        match token.as_bytes().first() {
            Some(b'-') => old = number(token).unwrap_or(1),
            Some(b'+') => new = number(token).unwrap_or(1),
            _ => {}
        }
    }
    (old.saturating_sub(1), new.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/main.rs b/main.rs
index 7b16f1f..333b15b 100644
--- a/main.rs
+++ b/main.rs
@@ -1,3 +1,4 @@
 fn main() {
-    println!(\"hi\");
+    println!(\"hello\");
+    println!(\"extra\");
 }
";

    #[test]
    fn parses_paths_and_counts() {
        let files = parse(SAMPLE);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "main.rs");
        assert_eq!(files[0].old_path.as_deref(), Some("main.rs"));
        assert_eq!((files[0].additions, files[0].deletions), (2, 1));
    }

    #[test]
    fn numbers_lines_from_the_hunk_header() {
        let files = parse(SAMPLE);
        let lines = &files[0].hunks[0].lines;

        assert_eq!(lines[0].kind, LineKind::Context);
        assert_eq!((lines[0].old_line, lines[0].new_line), (Some(1), Some(1)));

        assert_eq!(lines[1].kind, LineKind::Removed);
        assert_eq!((lines[1].old_line, lines[1].new_line), (Some(2), None));
        assert_eq!(lines[1].anchor, "old:2");

        assert_eq!(lines[2].kind, LineKind::Added);
        assert_eq!((lines[2].old_line, lines[2].new_line), (None, Some(2)));
        assert_eq!(lines[3].anchor, "new:3");

        // The trailing context line resumes from both counters.
        assert_eq!((lines[4].old_line, lines[4].new_line), (Some(3), Some(4)));
    }

    #[test]
    fn ignores_the_no_newline_marker() {
        let files = parse("diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file\n");
        assert_eq!(files[0].hunks[0].lines.len(), 2);
    }
}
