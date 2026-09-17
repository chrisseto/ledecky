use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use rocket::serde::Serialize;

use crate::review::ansi::{self, Marker, MINUS_BG, MINUS_EMPH_BG, PLUS_BG, PLUS_EMPH_BG};
use crate::review::Expansion;

/// Context large enough to cover any file, so delta sees the whole thing.
///
/// Highlighter state is only correct when the opening `/*` or `"` is visible, so
/// the diff has to carry the entire file rather than islands around each change.
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

/// What a rendered hunk is surrounded by: which unexpanded hunks it spans, and
/// how many lines are still folded away either side of it.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Gaps {
    pub first: usize,
    pub last: usize,
    pub above: usize,
    pub below: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<Line>,
    pub gaps: Gaps,
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

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub binary: bool,
    pub additions: u32,
    pub deletions: u32,
    pub hunks: Vec<Hunk>,
}

impl ParsedFile {
    /// Slices the file down to the changed regions plus [`CONTEXT`] lines either
    /// side, widened by whatever the reader has opened up.
    pub fn hunks(&self, expansion: &Expansion) -> FileDiff {
        FileDiff {
            path: self.path.clone(),
            old_path: self.old_path.clone(),
            binary: self.binary,
            additions: self.additions,
            deletions: self.deletions,
            hunks: self.windows(expansion),
        }
    }

    /// The ranges to render, in order, each carrying the unopened lines on
    /// either side of it.
    ///
    /// Expansion is keyed by the index of the *unexpanded* hunk so that a key
    /// stays valid as neighbours grow into each other: when two hunks merge, the
    /// surviving run is still bounded by the first hunk's gap above and the last
    /// hunk's gap below.
    fn windows(&self, expansion: &Expansion) -> Vec<Hunk> {
        let total = self.lines.len();

        let mut runs: Vec<(usize, usize, usize, usize)> = Vec::new();
        for (index, (start, end)) in ranges(&self.lines, CONTEXT).into_iter().enumerate() {
            let (up, down) = expansion.of(index);
            let start = start.saturating_sub(up);
            let end = end.saturating_add(down).min(total);

            match runs.last_mut() {
                Some(last) if start <= last.1 => {
                    last.1 = last.1.max(end);
                    last.3 = index;
                }
                _ => runs.push((start, end, index, index)),
            }
        }

        runs.iter()
            .enumerate()
            .map(|(i, &(start, end, first, last))| {
                let above = start - runs[..i].last().map_or(0, |previous| previous.1);
                let below = runs.get(i + 1).map_or(total, |next| next.0) - end;
                self.hunk(
                    start,
                    end,
                    Gaps {
                        first,
                        last,
                        above,
                        below,
                    },
                )
            })
            .collect()
    }

    fn hunk(&self, start: usize, end: usize, gaps: Gaps) -> Hunk {
        let lines = &self.lines[start..end];

        let count = |pick: fn(&Line) -> Option<u32>| {
            let numbered: Vec<u32> = lines.iter().filter_map(pick).collect();
            (numbered.first().copied().unwrap_or(0), numbered.len())
        };
        let (old_start, old_len) = count(|l| l.old_line);
        let (new_start, new_len) = count(|l| l.new_line);

        Hunk {
            header: format!("@@ -{old_start},{old_len} +{new_start},{new_len} @@"),
            lines: lines.to_vec(),
            gaps,
        }
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

/// Runs `git diff | delta` and parses the result.
///
/// git writes straight into delta through an OS pipe, so a large diff cannot
/// deadlock the way it would if we buffered it ourselves.
pub fn between(repo: &Path, from: &str, to: &str) -> Result<Vec<ParsedFile>> {
    let mut git = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "diff",
            FULL_CONTEXT,
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            from,
            to,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git diff")?;

    let stdout = git.stdout.take().expect("stdout was piped");
    let delta = Command::new("delta")
        .args(delta_args())
        .stdin(stdout)
        .output()
        .context("running delta — it is provided by the flake's dev shell")?;

    let status = git.wait().context("waiting for git diff")?;
    if !status.success() {
        bail!("git diff {from} {to} failed");
    }
    if !delta.status.success() {
        bail!(
            "delta failed: {}",
            String::from_utf8_lossy(&delta.stderr).trim()
        );
    }

    Ok(parse(&String::from_utf8_lossy(&delta.stdout)))
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
        let lines = &parsed.hunks(&Expansion::default()).hunks[0].lines;

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
        assert_eq!(parsed.hunks(&Expansion::default()).hunks[0].lines.len(), 5);
    }

    #[test]
    fn ignores_the_no_newline_marker() {
        let parsed =
            parse("diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file\n")
                .pop()
                .unwrap();
        assert_eq!(parsed.hunks(&Expansion::default()).hunks[0].lines.len(), 2);
    }

    #[test]
    fn a_binary_file_is_flagged_and_has_no_lines() {
        let parsed = parse(
            "diff --git a/b.bin b/b.bin\nindex 1b35392..8765ab8 100644\nBinary files a/b.bin and b/b.bin differ\n",
        )
        .pop()
        .unwrap();

        assert!(parsed.binary);
        assert!(parsed.hunks(&Expansion::default()).hunks.is_empty());
    }

    #[test]
    fn each_file_restarts_its_line_numbering() {
        let files = parse(&format!(
            "{SAMPLE}diff --git a/other.rs b/other.rs\n@@ -1,1 +1,1 @@\n+only\n"
        ));

        assert_eq!(files.len(), 2);
        let second = files[1].hunks(&Expansion::default()).hunks[0].lines[0].new_line;
        assert_eq!(second, Some(1));
    }

    // ---- hunk windows -------------------------------------------------------

    /// Two changes with a six-line gap: far enough apart to stay separate at
    /// `CONTEXT`, close enough to merge once a little is opened up.
    fn pair() -> ParsedFile {
        sketch("acccccccccca")
    }

    #[test]
    fn a_single_change_is_padded_on_both_sides() {
        let parsed = sketch("cccccaccccc");
        let hunks = parsed.hunks(&Expansion::default()).hunks;

        assert_eq!(hunks.len(), 1);
        // CONTEXT lines either side of the change at index 5.
        assert_eq!(hunks[0].lines.len(), 2 * CONTEXT + 1);
    }

    #[test]
    fn distant_changes_stay_separate() {
        assert_eq!(pair().hunks(&Expansion::default()).hunks.len(), 2);
    }

    #[test]
    fn the_window_clamps_at_the_edges_of_the_file() {
        let parsed = sketch("accca");
        let hunks = parsed.hunks(&Expansion::default()).hunks;

        // More padding than the file has lines yields the whole file, once.
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].lines.len(), 5);
        assert_eq!((hunks[0].gaps.above, hunks[0].gaps.below), (0, 0));
    }

    #[test]
    fn a_file_with_no_changes_has_no_hunks() {
        assert!(sketch("cccc").hunks(&Expansion::default()).hunks.is_empty());
    }

    // ---- expansion ----------------------------------------------------------

    #[test]
    fn the_gaps_report_what_is_still_folded_away() {
        let hunks = pair().hunks(&Expansion::default()).hunks;

        // Nothing above the first hunk or below the last; the six lines between
        // them are split between the two facing gaps.
        assert_eq!((hunks[0].gaps.above, hunks[0].gaps.below), (0, 4));
        assert_eq!((hunks[1].gaps.above, hunks[1].gaps.below), (4, 0));
    }

    #[test]
    fn expanding_one_hunk_takes_lines_from_its_gap() {
        let hunks = pair()
            .hunks(&Expansion::default().plus(0, Dir::Down, 2))
            .hunks;

        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].lines.len(), CONTEXT + 1 + 2);
        assert_eq!(hunks[0].gaps.below, 2);
        assert_eq!(hunks[1].gaps.above, 2);
    }

    #[test]
    fn hunks_merge_once_expansion_closes_the_gap() {
        let hunks = pair()
            .hunks(&Expansion::default().plus(0, Dir::Down, 4))
            .hunks;

        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].lines.len(), 12);
        // The run still names the hunks it swallowed, so the buttons either side
        // of it keep expanding the right ones.
        assert_eq!((hunks[0].gaps.first, hunks[0].gaps.last), (0, 1));
    }

    #[test]
    fn the_whole_file_expansion_does_not_overflow() {
        // Every hunk opens by usize::MAX, which naive padding would wrap.
        let hunks = pair()
            .hunks(&Expansion::parse(Some(Expansion::WHOLE_FILE)))
            .hunks;

        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].lines.len(), 12);
        assert_eq!((hunks[0].gaps.above, hunks[0].gaps.below), (0, 0));
    }

    #[test]
    fn an_expansion_naming_a_hunk_that_does_not_exist_is_ignored() {
        // Keys outlive the file they were made against — a stale one from
        // another file must not change what this one shows.
        let stale = Expansion::default().plus(9, Dir::Up, 40);
        assert_eq!(pair().hunks(&stale).hunks.len(), 2);
    }

    // ---- against a real git and delta ---------------------------------------

    /// Runs the actual pipeline over a scratch repository.
    ///
    /// This is the palette drift detector. Delta is pinned by `flake.lock`, so
    /// its colours can only move on a deliberate update — at which point this
    /// fails loudly instead of the diff quietly losing its highlighting.
    #[test]
    fn every_palette_colour_still_resolves_to_its_class() {
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

        let files = between(&repo, "HEAD~1", "HEAD").expect("the pipeline ran");
        let html: String = files[0]
            .hunks(&Expansion::parse(Some(Expansion::WHOLE_FILE)))
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
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

    #[test]
    fn a_changed_word_is_marked_inside_its_line() {
        let repo = scratch_repo(
            "word",
            "sample.rs",
            "fn main() {\n    let greeting = \"hello there\";\n}\n",
            "hello there",
            "hello world",
        );

        let files = between(&repo, "HEAD~1", "HEAD").expect("the pipeline ran");
        let added = files[0]
            .hunks(&Expansion::default())
            .hunks
            .iter()
            .flat_map(|h| h.lines.clone())
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
    #[test]
    fn a_change_inside_a_multi_line_comment_stays_a_comment() {
        let mut body = String::from("fn main() {\n    /* opener far above\n");
        for i in 0..40 {
            body.push_str(&format!("    filler {i}\n"));
        }
        body.push_str("    target OLD\n    closing */\n    let x = 1;\n}\n");

        let repo = scratch_repo("comment", "sample.rs", &body, "target OLD", "target NEW");

        let files = between(&repo, "HEAD~1", "HEAD").expect("the pipeline ran");
        let added = files[0]
            .hunks(&Expansion::default())
            .hunks
            .iter()
            .flat_map(|h| h.lines.clone())
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

        let git = |args: &[&str]| {
            Command::new("git")
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
    fn the_header_counts_each_side_separately() {
        // Five context lines around one addition: all five have an old number,
        // the added line does not, so the sides disagree — which is the point of
        // the format.
        let parsed = sketch("ccaccc");
        let header = &parsed.hunks(&Expansion::default()).hunks[0].header;

        assert_eq!(header, "@@ -1,5 +1,6 @@");
    }
}
