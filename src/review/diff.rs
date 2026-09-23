use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

use anyhow::{bail, Context, Result};
use rocket::serde::Serialize;

use crate::review::ansi::{self, Marker, MINUS_BG, MINUS_EMPH_BG, PLUS_BG, PLUS_EMPH_BG};
use crate::review::expand::{FileExpansion, Fold};

/// Context large enough to cover any file, so delta sees the whole thing.
///
/// Highlighter state is only correct when the opening `/*` or `"` is visible, so
/// the diff has to carry the entire file rather than islands around each change.
/// The bill is syntect's throughput — about 0.45s per five thousand lines,
/// whatever the change was — which is why [`DiffCache`] keys on blobs and
/// re-highlights only the files that moved.
///
/// [`DiffCache`]: crate::review::DiffCache
const FULL_CONTEXT: &str = "-U1000000";

/// Lines of context either side of a change before anything is opened up.
pub const CONTEXT: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(crate = "rocket::serde", rename_all = "snake_case")]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

impl From<Marker> for LineKind {
    fn from(marker: Marker) -> Self {
        match marker {
            Marker::Context => Self::Context,
            Marker::Added => Self::Added,
            Marker::Removed => Self::Removed,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Line {
    pub kind: LineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    /// Pre-rendered markup; the template inserts it verbatim.
    pub html: String,
    /// `<side>:<line>`, the key a comment anchors to.
    pub anchor: String,
}

/// One stretch of a windowed file, top to bottom.
#[derive(Debug, Clone)]
pub enum Segment {
    Lines(Vec<Line>),
    Fold(Fold),
}

/// Every line of one file's diff, before any window is applied.
///
/// Held whole so that opening up a hunk is a re-slice rather than another run of
/// git and delta.
#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub path: String,
    pub old_path: Option<String>,
    pub binary: bool,
    pub additions: u32,
    pub deletions: u32,
    lines: Vec<Line>,
}

/// What of one file is on screen.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub segments: Vec<Segment>,
}

impl FileDiff {
    /// Every line on screen, folds skipped.
    pub fn lines(&self) -> impl Iterator<Item = &Line> {
        self.segments.iter().flat_map(|segment| match segment {
            Segment::Lines(lines) => lines.as_slice(),
            Segment::Fold(_) => &[],
        })
    }
}

impl ParsedFile {
    /// Slices the file down to the changed regions plus [`CONTEXT`] lines either
    /// side, widened by whatever the reader has opened up.
    pub fn window(&self, expansion: &FileExpansion) -> FileDiff {
        FileDiff {
            segments: self.segments(expansion),
        }
    }

    /// Roughly the bytes this holds, for bounding a cache of them.
    pub fn weight(&self) -> usize {
        self.lines
            .iter()
            .map(|line| std::mem::size_of::<Line>() + line.html.len() + line.anchor.len())
            .sum()
    }

    /// The shown runs in order, with a fold wherever lines are left hidden.
    ///
    /// Expansion is keyed by the index of the *unexpanded* hunk so that a key
    /// stays valid as neighbours grow into each other: when two hunks merge, the
    /// fold above the run still opens from the first of them and the fold below
    /// from the last.
    fn segments(&self, expansion: &FileExpansion) -> Vec<Segment> {
        let total = self.lines.len();

        let mut runs: Vec<(usize, usize, usize, usize)> = Vec::new();
        for (index, (start, end)) in ranges(&self.lines, CONTEXT).into_iter().enumerate() {
            let (up, down) = expansion.of(index);
            let start = start.saturating_sub(up);
            let end = end.saturating_add(down).min(total);

            match runs.last_mut() {
                Some(last) if !Fold::hides(start.saturating_sub(last.1)) => {
                    last.1 = last.1.max(end);
                    last.3 = index;
                }
                Some(_) => runs.push((start, end, index, index)),
                None => {
                    let start = if Fold::hides(start) { start } else { 0 };
                    runs.push((start, end, index, index));
                }
            }
        }
        if let Some(last) = runs.last_mut() {
            if !Fold::hides(total - last.1) {
                last.1 = total;
            }
        }

        let mut segments = Vec::new();
        let mut shown = 0;
        let mut above = None;
        for (start, end, first, last) in runs {
            if start > shown {
                segments.push(Segment::Fold(Fold {
                    lines: start - shown,
                    above,
                    below: Some(first),
                }));
            }
            segments.push(Segment::Lines(self.lines[start..end].to_vec()));
            shown = end;
            above = Some(last);
        }
        if above.is_some() && total > shown {
            segments.push(Segment::Fold(Fold {
                lines: total - shown,
                above,
                below: None,
            }));
        }
        segments
    }
}

/// The index ranges worth showing: every run of changed lines, padded by
/// `context` and merged where the padding overlaps.
fn ranges(lines: &[Line], context: usize) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if line.kind == LineKind::Context {
            continue;
        }

        // Saturating so a change near the top of the file cannot wrap.
        let start = i.saturating_sub(context);
        let end = i.saturating_add(context).saturating_add(1).min(lines.len());

        match ranges.last_mut() {
            Some(last) if start <= last.1 => last.1 = end,
            _ => ranges.push((start, end)),
        }
    }
    ranges
}

/// One entry of `git diff --raw`: a file as it stood at each end of a range.
///
/// Everything its rendered diff depends on, so two ranges that agree on it
/// render that file identically.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Change {
    pub old_mode: String,
    pub new_mode: String,
    pub old_blob: String,
    pub new_blob: String,
    pub old_path: String,
    pub new_path: String,
}

impl Change {
    /// What to hand `git diff` to reproduce just this file, rename and all.
    pub fn paths(&self) -> Vec<String> {
        match self.old_path == self.new_path {
            true => vec![self.new_path.clone()],
            false => vec![self.old_path.clone(), self.new_path.clone()],
        }
    }
}

/// The files that differ between two revisions, without reading any of them.
pub async fn changes(repo: &Path, from: &str, to: &str) -> Result<Vec<Change>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "diff",
            "--raw",
            "-z",
            "--no-abbrev",
            "--no-ext-diff",
            "--find-renames",
            from,
            to,
        ])
        .output()
        .await
        .context("running git diff --raw")?;

    if !out.status.success() {
        bail!(
            "git diff --raw {from} {to} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_raw(&String::from_utf8_lossy(&out.stdout)))
}

/// `:<mode> <mode> <blob> <blob> <status>\0<path>\0`, with a second path for a
/// rename or copy.
fn parse_raw(raw: &str) -> Vec<Change> {
    let mut fields = raw.split('\0');
    let mut changes = Vec::new();

    while let Some(meta) = fields.next() {
        let Some(meta) = meta.strip_prefix(':') else {
            continue;
        };
        let parts: Vec<&str> = meta.split(' ').collect();
        let [old_mode, new_mode, old_blob, new_blob, status] = parts[..] else {
            continue;
        };
        let Some(old_path) = fields.next() else {
            break;
        };
        let new_path = match status.starts_with(['R', 'C']) {
            true => match fields.next() {
                Some(path) => path,
                None => break,
            },
            false => old_path,
        };

        changes.push(Change {
            old_mode: old_mode.to_owned(),
            new_mode: new_mode.to_owned(),
            old_blob: old_blob.to_owned(),
            new_blob: new_blob.to_owned(),
            old_path: old_path.to_owned(),
            new_path: new_path.to_owned(),
        });
    }
    changes
}

/// Runs `git diff | delta` over `paths`, or over everything when there are
/// none, and parses the result.
///
/// git writes straight into delta through an OS pipe, so a large diff cannot
/// deadlock the way it would if we buffered it ourselves.
pub async fn between(
    repo: &Path,
    from: &str,
    to: &str,
    paths: &[String],
) -> Result<Vec<ParsedFile>> {
    let mut git = Command::new("git")
        .arg("-C")
        .arg(repo)
        // A path is a path, not a glob that might match its neighbours.
        .arg("--literal-pathspecs")
        .args([
            "diff",
            FULL_CONTEXT,
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            from,
            to,
            "--",
        ])
        .args(paths)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git diff")?;

    // NB: git's stdout becomes delta's stdin as a raw descriptor, so the two
    // talk through the kernel rather than through us. Nothing here holds a
    // whole diff in memory, and neither can wedge on the other filling a pipe.
    let stdout = git.stdout.take().expect("stdout was piped");
    let stdout: Stdio = stdout.try_into().context("handing git's output to delta")?;

    let delta = Command::new("delta")
        .args(delta_args())
        .stdin(stdout)
        .output()
        .await
        .context("running delta — it is provided by the flake's dev shell")?;

    let status = git.wait().await.context("waiting for git diff")?;
    if !status.success() {
        bail!("git diff {from} {to} failed");
    }
    if !delta.status.success() {
        bail!(
            "delta failed: {}",
            String::from_utf8_lossy(&delta.stderr).trim()
        );
    }

    // NB: tens of milliseconds per thousand lines, which is too long to hold
    // an async worker for.
    tokio::task::spawn_blocking(move || parse(&String::from_utf8_lossy(&delta.stdout)))
        .await
        .context("parsing the diff")
}

fn delta_args() -> Vec<String> {
    let sentinel = |(r, g, b): (u8, u8, u8)| format!("syntax \"#{r:02x}{g:02x}{b:02x}\"");

    vec![
        // Annotate the diff rather than restructuring it, so the line and hunk
        // structure stays exactly what git produced.
        "--color-only".into(),
        // Without this the user's own delta configuration changes our output.
        "--no-gitconfig".into(),
        "--paging=never".into(),
        // Forces 38;2;r;g;b, so the palette never arrives as 256-colour indices.
        "--true-color=always".into(),
        // Byte offsets have to match the file; expanded tabs would shift them.
        "--tabs".into(),
        "0".into(),
        "--syntax-theme=Nord".into(),
        // `syntax` keeps the theme's foreground; `normal` would drop it.
        "--minus-style".into(),
        sentinel(MINUS_BG),
        "--plus-style".into(),
        sentinel(PLUS_BG),
        "--minus-emph-style".into(),
        sentinel(MINUS_EMPH_BG),
        "--plus-emph-style".into(),
        sentinel(PLUS_EMPH_BG),
    ]
}

pub fn parse(raw: &str) -> Vec<ParsedFile> {
    let mut files: Vec<ParsedFile> = Vec::new();
    let mut old_no = 0u32;
    let mut new_no = 0u32;

    for raw_line in raw.lines() {
        // Delta leaves the structural headers unstyled, so they are matched on
        // the raw text.
        if let Some(paths) = raw_line.strip_prefix("diff --git ") {
            old_no = 0;
            new_no = 0;
            files.push(ParsedFile {
                path: git_path(paths, 'b').unwrap_or_else(|| paths.to_owned()),
                old_path: git_path(paths, 'a'),
                binary: false,
                additions: 0,
                deletions: 0,
                lines: Vec::new(),
            });
            continue;
        }

        let Some(file) = files.last_mut() else {
            continue;
        };

        if raw_line.starts_with("Binary files ") {
            file.binary = true;
            continue;
        }
        if raw_line.starts_with("@@") {
            let (start_old, start_new) = hunk_starts(raw_line);
            old_no = start_old;
            new_no = start_new;
            continue;
        }
        // `--- a/x` and `+++ b/x` arrive before any hunk and would otherwise read
        // as content, so skip them explicitly.
        if raw_line.starts_with("--- ") || raw_line.starts_with("+++ ") {
            continue;
        }

        let Some(parsed) = ansi::parse_line(raw_line) else {
            continue;
        };
        let kind = LineKind::from(parsed.marker);

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

        file.lines.push(Line {
            kind,
            old_line,
            new_line,
            html: parsed.to_html(),
            anchor,
        });
    }

    files
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
    use crate::review::expand::Dir;

    /// Plain-text stand-in for delta output: the parser only needs the markers,
    /// and ANSI handling is covered in `ansi.rs`.
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

    fn file(body: &str) -> ParsedFile {
        parse(&format!(
            "diff --git a/f.rs b/f.rs\n@@ -1,9 +1,9 @@\n{body}"
        ))
        .pop()
        .unwrap()
    }

    /// One line per character: `c` context, `a` added, `r` removed.
    fn sketch(shape: &str) -> ParsedFile {
        let body: String = shape
            .chars()
            .map(|c| match c {
                'a' => "+x\n",
                'r' => "-x\n",
                _ => " x\n",
            })
            .collect();
        file(&body)
    }

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
        let parsed = parse(SAMPLE).pop().unwrap();
        let lines = shown(&parsed);

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
    fn the_headers_never_read_as_content() {
        // `--- a/x` and `+++ b/x` start with diff markers but are not lines.
        let parsed = parse(SAMPLE).pop().unwrap();
        assert_eq!(shown(&parsed).len(), 5);
    }

    #[test]
    fn ignores_the_no_newline_marker() {
        let parsed =
            parse("diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file\n")
                .pop()
                .unwrap();
        assert_eq!(shown(&parsed).len(), 2);
    }

    #[test]
    fn a_binary_file_is_flagged_and_has_no_lines() {
        let parsed = parse(
            "diff --git a/b.bin b/b.bin\nindex 1b35392..8765ab8 100644\nBinary files a/b.bin and b/b.bin differ\n",
        )
        .pop()
        .unwrap();

        assert!(parsed.binary);
        assert!(shown(&parsed).is_empty());
    }

    #[test]
    fn each_file_restarts_its_line_numbering() {
        let files = parse(&format!(
            "{SAMPLE}diff --git a/other.rs b/other.rs\n@@ -1,1 +1,1 @@\n+only\n"
        ));

        assert_eq!(files.len(), 2);
        let second = shown(&files[1])[0].new_line;
        assert_eq!(second, Some(1));
    }

    // ---- windows ------------------------------------------------------------

    /// What is on screen by default.
    fn shown(parsed: &ParsedFile) -> Vec<Line> {
        parsed
            .window(&FileExpansion::default())
            .lines()
            .cloned()
            .collect()
    }

    /// `7 ~14 7`: each shown run by its length, each fold by `~` and its size.
    fn shape(parsed: &ParsedFile, expansion: &FileExpansion) -> String {
        parsed
            .window(expansion)
            .segments
            .iter()
            .map(|segment| match segment {
                Segment::Lines(lines) => lines.len().to_string(),
                Segment::Fold(fold) => format!("~{}", fold.lines),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn folds(parsed: &ParsedFile, expansion: &FileExpansion) -> Vec<Fold> {
        parsed
            .window(expansion)
            .segments
            .into_iter()
            .filter_map(|segment| match segment {
                Segment::Fold(fold) => Some(fold),
                Segment::Lines(_) => None,
            })
            .collect()
    }

    /// Two changes with 14 lines folded between them: enough to stay folded
    /// after a little is opened up.
    fn pair() -> ParsedFile {
        sketch(&format!("a{}a", "c".repeat(2 * CONTEXT + 14)))
    }

    #[test]
    fn a_single_change_is_padded_on_both_sides() {
        let pad = "c".repeat(CONTEXT + 10);
        let parsed = sketch(&format!("{pad}a{pad}"));

        assert_eq!(shape(&parsed, &FileExpansion::default()), "~10 7 ~10");
    }

    #[test]
    fn a_short_gap_is_shown_rather_than_folded() {
        let short = "c".repeat(CONTEXT + 9);
        let parsed = sketch(&format!("{short}a{short}a{short}"));

        assert_eq!(shape(&parsed, &FileExpansion::default()), "38");
    }

    #[test]
    fn the_window_clamps_at_the_edges_of_the_file() {
        // More padding than the file has lines yields the whole file, once.
        assert_eq!(shape(&sketch("accca"), &FileExpansion::default()), "5");
    }

    #[test]
    fn a_file_with_no_changes_has_nothing_to_show() {
        assert_eq!(shape(&sketch("cccc"), &FileExpansion::default()), "");
    }

    #[test]
    fn a_gap_between_changes_is_one_fold_opening_from_both_sides() {
        assert_eq!(shape(&pair(), &FileExpansion::default()), "4 ~14 4");
        assert_eq!(
            folds(&pair(), &FileExpansion::default()),
            [Fold {
                lines: 14,
                above: Some(0),
                below: Some(1),
            }]
        );
    }

    #[test]
    fn a_fold_at_an_edge_opens_from_one_side() {
        let pad = "c".repeat(CONTEXT + 10);
        let parsed = sketch(&format!("{pad}a{pad}"));
        let [top, bottom] = folds(&parsed, &FileExpansion::default())[..] else {
            panic!("two folds");
        };

        assert_eq!((top.above, top.below), (None, Some(0)));
        assert_eq!((bottom.above, bottom.below), (Some(0), None));
    }

    // ---- expansion ----------------------------------------------------------

    #[test]
    fn expanding_one_hunk_takes_lines_from_its_fold() {
        let opened = FileExpansion::default().plus(0, Dir::Down, 2);
        assert_eq!(shape(&pair(), &opened), "6 ~12 4");
    }

    #[test]
    fn hunks_merge_once_expansion_closes_the_fold() {
        let opened = FileExpansion::default().plus(0, Dir::Down, 14);
        assert_eq!(shape(&pair(), &opened), "22");
    }

    #[test]
    fn a_merged_run_is_bounded_by_its_first_and_last_hunks() {
        // Three changes; the first two merge, so the fold below the run must
        // still open from the second hunk, not the first.
        let gap = "c".repeat(2 * CONTEXT + 14);
        let parsed = sketch(&format!("a{gap}a{gap}a"));
        let opened = FileExpansion::default().plus(0, Dir::Down, 14);

        assert_eq!(shape(&parsed, &opened), "25 ~14 4");
        let fold = folds(&parsed, &opened)[0];
        assert_eq!((fold.above, fold.below), (Some(1), Some(2)));
    }

    #[test]
    fn the_whole_file_expansion_does_not_overflow() {
        // Every hunk opens by usize::MAX, which naive padding would wrap.
        let whole = FileExpansion::parse(FileExpansion::WHOLE_FILE);
        assert_eq!(shape(&pair(), &whole), "22");
    }

    #[test]
    fn an_expansion_naming_a_hunk_that_does_not_exist_is_ignored() {
        // Keys outlive the file they were made against — a stale one from
        // another file must not change what this one shows.
        let stale = FileExpansion::default().plus(9, Dir::Up, 40);
        assert_eq!(shape(&pair(), &stale), "4 ~14 4");
    }

    // ---- against a real git and delta ---------------------------------------

    /// Runs the actual pipeline over a scratch repository.
    ///
    /// This is the palette drift detector. Delta is pinned by `flake.lock`, so
    /// its colours can only move on a deliberate update — at which point this
    /// fails loudly instead of the diff quietly losing its highlighting.
    #[tokio::test]
    async fn every_palette_colour_still_resolves_to_its_class() {
        let repo = scratch_repo(
            "palette",
            "sample.rs",
            "/// A doc comment.\n\
             pub struct Thing { name: String }\n\
             impl Thing {\n\
             \x20   pub fn new() -> Self {\n\
             \x20       let count = 42;\n\
             \x20       Self { name: \"hello\".to_owned() }\n\
             \x20   }\n\
             }\n",
            "let count = 42;",
            "let count = 43;",
        );

        let files = between(&repo, "HEAD~1", "HEAD", &[])
            .await
            .expect("the pipeline ran");
        let html: String = files[0]
            .window(&FileExpansion::parse(FileExpansion::WHOLE_FILE))
            .lines()
            .map(|l| l.html.clone())
            .collect();

        for class in [
            "tok-keyword",
            "tok-fn",
            "tok-type",
            "tok-string",
            "tok-number",
            "tok-comment",
            "tok-punct",
        ] {
            assert!(
                html.contains(class),
                "{class} never appeared — delta's palette has moved, or the theme changed.\n{html}"
            );
        }
    }

    #[tokio::test]
    async fn a_changed_word_is_marked_inside_its_line() {
        let repo = scratch_repo(
            "word",
            "sample.rs",
            "fn main() {\n    let greeting = \"hello there\";\n}\n",
            "hello there",
            "hello world",
        );

        let files = between(&repo, "HEAD~1", "HEAD", &[])
            .await
            .expect("the pipeline ran");
        let added = shown(&files[0])
            .into_iter()
            .find(|l| l.kind == LineKind::Added)
            .expect("an added line");

        // Only the word that changed carries the marker, not the whole line.
        assert!(added.html.contains("chg"), "no change span: {}", added.html);
        assert!(added.html.contains("world"));
        assert!(
            !added.html.contains("chg\">    let"),
            "the untouched start of the line was marked: {}",
            added.html
        );
    }

    /// Highlighting is only correct when the construct's opening is visible, so
    /// this is the case full context exists for.
    #[tokio::test]
    async fn a_change_inside_a_multi_line_comment_stays_a_comment() {
        let mut body = String::from("fn main() {\n    /* opener far above\n");
        for i in 0..40 {
            body.push_str(&format!("    filler {i}\n"));
        }
        body.push_str("    target OLD\n    closing */\n    let x = 1;\n}\n");

        let repo = scratch_repo("comment", "sample.rs", &body, "target OLD", "target NEW");

        let files = between(&repo, "HEAD~1", "HEAD", &[])
            .await
            .expect("the pipeline ran");
        let added = shown(&files[0])
            .into_iter()
            .find(|l| l.kind == LineKind::Added)
            .expect("an added line");

        // The `/*` is forty lines up and would be outside any ordinary hunk.
        assert!(
            added.html.contains("tok-comment"),
            "the line lost its comment styling: {}",
            added.html
        );
    }

    /// A repository with one commit and an uncommitted edit.
    fn scratch_repo(
        name: &str,
        file: &str,
        body: &str,
        from: &str,
        to: &str,
    ) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ledecky-diff-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // NB: `std`, not the `tokio` `Command` this module builds its pipeline
        // with. Setting a scratch repository up is not what is under test, and
        // a plain blocking call keeps the helper a closure.
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .expect("git ran");
        };

        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@ledecky"]);
        git(&["config", "user.name", "ledecky test"]);
        std::fs::write(dir.join(file), body).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);

        std::fs::write(dir.join(file), body.replace(from, to)).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "change"]);

        dir
    }

    #[test]
    fn raw_entries_carry_both_paths_of_a_rename() {
        let raw = ":100644 100644 aaa bbb M\0a b.rs\0:100644 100644 ccc ddd R086\0old.rs\0new.rs\0";
        let changes = parse_raw(raw);

        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].paths(), ["a b.rs"]);
        assert_eq!(
            (changes[1].old_blob.as_str(), changes[1].new_blob.as_str()),
            ("ccc", "ddd")
        );
        assert_eq!(changes[1].paths(), ["old.rs", "new.rs"]);
    }
}
