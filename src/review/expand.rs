use std::collections::BTreeMap;
use std::fmt::Write as _;

/// How far each hunk of one file has been opened up, in lines.
///
/// The diff always renders a few lines of context; everything beyond that is
/// asked for one gap at a time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileExpansion {
    whole_file: bool,
    /// Set on a file the pane held back for its size, to ask for it anyway.
    shown: bool,
    /// hunk index -> (lines above, lines below)
    by_hunk: BTreeMap<usize, (usize, usize)>,
}

/// What the reader has opened up, across every file in the diff.
///
/// Keeping this in the URL rather than on the server means the pane re-renders
/// from the query alone, and a stale key is harmless — it names files and hunks
/// that no longer exist.
///
/// Files are keyed by their position in the diff, which is also what the DOM ids
/// and the tree's jump links use. Positions shift when the scope changes, but a
/// key that outlives its scope was already only a rendering hint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expansion {
    by_file: BTreeMap<usize, FileExpansion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
}

impl FileExpansion {
    pub const WHOLE_FILE: &'static str = "file";
    pub const SHOW: &'static str = "show";

    /// `file`, `show`, or `0u10,2d40`. Anything unparseable reads as nothing.
    pub fn parse(raw: &str) -> Self {
        let mut expansion = Self::default();

        for term in raw.split(',') {
            match term.trim() {
                Self::WHOLE_FILE => expansion.whole_file = true,
                Self::SHOW => expansion.shown = true,
                term => {
                    let Some(at) = term.find(['u', 'd']) else {
                        continue;
                    };
                    let (Ok(index), Ok(lines)) = (term[..at].parse(), term[at + 1..].parse())
                    else {
                        continue;
                    };

                    let entry: &mut (usize, usize) = expansion.by_hunk.entry(index).or_default();
                    match &term[at..at + 1] {
                        "u" => entry.0 = lines,
                        _ => entry.1 = lines,
                    }
                }
            }
        }
        expansion
    }

    fn terms(&self) -> String {
        if self.whole_file {
            return Self::WHOLE_FILE.to_owned();
        }

        let mut key = String::new();
        if self.shown {
            key.push_str(Self::SHOW);
        }

        for (index, (up, down)) in &self.by_hunk {
            for (lines, letter) in [(up, 'u'), (down, 'd')] {
                if *lines > 0 {
                    if !key.is_empty() {
                        key.push(',');
                    }
                    let _ = write!(key, "{index}{letter}{lines}");
                }
            }
        }
        key
    }

    fn is_empty(&self) -> bool {
        self.terms().is_empty()
    }

    /// Lines opened above and below hunk `index`.
    pub fn of(&self, index: usize) -> (usize, usize) {
        if self.whole_file {
            return (usize::MAX, usize::MAX);
        }
        self.by_hunk.get(&index).copied().unwrap_or_default()
    }

    /// Whether a file held back for its size was asked for anyway.
    pub fn shown(&self) -> bool {
        self.shown || self.whole_file || !self.by_hunk.is_empty()
    }

    /// The same expansion with `lines` more opened on one side of one hunk.
    pub fn plus(&self, hunk: usize, dir: Dir, lines: usize) -> Self {
        // Nothing is left to open, and the per-hunk counts would be meaningless.
        if self.whole_file {
            return self.clone();
        }

        let mut next = self.clone();
        let (up, down) = self.of(hunk);
        let entry = next.by_hunk.entry(hunk).or_default();
        *entry = match dir {
            Dir::Up => (up.saturating_add(lines), down),
            Dir::Down => (up, down.saturating_add(lines)),
        };
        next
    }
}

impl Expansion {
    /// `0:file;3:0u10,2d40`. Anything unparseable reads as no expansion.
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Self::default();
        };

        let mut by_file = BTreeMap::new();
        for term in raw.split(';') {
            let Some((index, terms)) = term.split_once(':') else {
                continue;
            };
            let Ok(index) = index.trim().parse() else {
                continue;
            };

            let expansion = FileExpansion::parse(terms);
            if !expansion.is_empty() {
                by_file.insert(index, expansion);
            }
        }

        Self { by_file }
    }

    pub fn key(&self) -> String {
        let mut key = String::new();
        for (index, expansion) in &self.by_file {
            if !key.is_empty() {
                key.push(';');
            }
            let _ = write!(key, "{index}:{}", expansion.terms());
        }
        key
    }

    /// What has been opened in the file at `index`.
    pub fn file(&self, index: usize) -> FileExpansion {
        self.by_file.get(&index).cloned().unwrap_or_default()
    }

    /// The same expansion with `lines` more opened on one side of one hunk of
    /// one file.
    pub fn plus(&self, file: usize, hunk: usize, dir: Dir, lines: usize) -> Self {
        let expanded = self.file(file).plus(hunk, dir, lines);
        let mut next = self.clone();
        if !expanded.is_empty() {
            next.by_file.insert(file, expanded);
        }
        next
    }

    /// The same expansion with every hunk of one file opened.
    pub fn whole_file(&self, file: usize) -> Self {
        let mut next = self.clone();
        let entry = next.by_file.entry(file).or_default();
        entry.whole_file = true;
        entry.by_hunk.clear();
        next
    }

    /// The same expansion with one over-long file asked for anyway.
    pub fn showing(&self, file: usize) -> Self {
        let mut next = self.clone();
        next.by_file.entry(file).or_default().shown = true;
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_expanded_by_default() {
        let expansion = Expansion::parse(None);
        assert_eq!(expansion.file(0).of(0), (0, 0));
        assert_eq!(expansion.key(), "");
    }

    #[test]
    fn expansions_round_trip_through_their_key() {
        let expansion = Expansion::parse(Some("2:0u10,2d40"));
        assert_eq!(expansion.file(2).of(0), (10, 0));
        assert_eq!(expansion.file(2).of(2), (0, 40));
        assert_eq!(expansion.key(), "2:0u10,2d40");
    }

    #[test]
    fn files_do_not_share_what_has_been_opened() {
        let expansion = Expansion::parse(Some("0:1u10;3:file"));

        assert_eq!(expansion.file(0).of(1), (10, 0));
        assert_eq!(expansion.file(3).of(1), (usize::MAX, usize::MAX));
        // The file nobody has touched is untouched.
        assert_eq!(expansion.file(1).of(1), (0, 0));
    }

    #[test]
    fn the_whole_file_opens_every_hunk() {
        let expansion = Expansion::parse(Some("4:file"));
        assert_eq!(expansion.file(4).of(7), (usize::MAX, usize::MAX));
        assert_eq!(expansion.key(), "4:file");
    }

    #[test]
    fn junk_terms_are_dropped_rather_than_failing() {
        let expansion = Expansion::parse(Some("nonsense;1:xuy,1u5;:9;2"));
        assert_eq!(expansion.file(1).of(1), (5, 0));
        assert_eq!(expansion.key(), "1:1u5");
    }

    #[test]
    fn expanding_accumulates_on_one_side() {
        let expansion = Expansion::parse(Some("0:0u10"))
            .plus(0, 0, Dir::Up, 10)
            .plus(0, 0, Dir::Down, 3);

        assert_eq!(expansion.file(0).of(0), (20, 3));
        assert_eq!(expansion.key(), "0:0u20,0d3");
    }

    #[test]
    fn expanding_one_file_leaves_the_others_alone() {
        let expansion = Expansion::parse(Some("0:0u10")).plus(5, 1, Dir::Down, 20);

        assert_eq!(expansion.file(0).of(0), (10, 0));
        assert_eq!(expansion.key(), "0:0u10;5:1d20");
    }

    #[test]
    fn a_whole_file_expansion_has_nothing_left_to_open() {
        let expansion = Expansion::parse(Some("3:file")).plus(3, 0, Dir::Up, 10);
        assert_eq!(expansion.key(), "3:file");
    }

    #[test]
    fn opening_a_whole_file_supersedes_its_hunks() {
        let expansion = Expansion::parse(Some("3:0u10,1d5")).whole_file(3);
        assert_eq!(expansion.key(), "3:file");
    }

    #[test]
    fn a_file_held_back_for_its_size_can_be_asked_for() {
        let expansion = Expansion::default().showing(2);

        assert_eq!(expansion.key(), "2:show");
        assert!(expansion.file(2).shown());
        assert!(!expansion.file(0).shown());
        // Asking for it opens nothing beyond the default context.
        assert_eq!(expansion.file(2).of(0), (0, 0));
    }

    #[test]
    fn expanding_a_hunk_is_itself_a_reason_to_show_the_file() {
        assert!(Expansion::parse(Some("2:0u10")).file(2).shown());
        assert!(Expansion::parse(Some("2:file")).file(2).shown());
    }
}
