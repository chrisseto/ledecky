use std::collections::BTreeMap;
use std::fmt::Write as _;

/// How far each hunk of the selected file has been opened up, in lines.
///
/// The diff always renders a few lines of context; everything beyond that is
/// asked for one gap at a time. Keeping that in the URL rather than on the
/// server means the pane re-renders from the query alone, and a stale key from
/// another file is harmless — it names hunks that no longer exist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expansion {
    whole_file: bool,
    /// hunk index -> (lines above, lines below)
    by_hunk: BTreeMap<usize, (usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
}

impl Expansion {
    pub const WHOLE_FILE: &'static str = "file";

    /// `file`, or `0u10,2d40`. Anything unparseable reads as no expansion.
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Self::default();
        };
        if raw == Self::WHOLE_FILE {
            return Self {
                whole_file: true,
                by_hunk: BTreeMap::new(),
            };
        }

        let mut by_hunk = BTreeMap::new();
        for term in raw.split(',') {
            let split = term.find(['u', 'd']);
            let Some(at) = split else { continue };
            let (Ok(index), Ok(lines)) = (term[..at].parse(), term[at + 1..].parse()) else {
                continue;
            };

            let entry: &mut (usize, usize) = by_hunk.entry(index).or_default();
            match &term[at..at + 1] {
                "u" => entry.0 = lines,
                _ => entry.1 = lines,
            }
        }

        Self {
            whole_file: false,
            by_hunk,
        }
    }

    pub fn key(&self) -> String {
        if self.whole_file {
            return Self::WHOLE_FILE.to_owned();
        }

        let mut key = String::new();
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

    /// Lines opened above and below hunk `index`.
    pub fn of(&self, index: usize) -> (usize, usize) {
        if self.whole_file {
            return (usize::MAX, usize::MAX);
        }
        self.by_hunk.get(&index).copied().unwrap_or_default()
    }

    /// The same expansion with `lines` more opened on one side of one hunk.
    pub fn plus(&self, index: usize, dir: Dir, lines: usize) -> Self {
        // Nothing is left to open, and the per-hunk counts would be meaningless.
        if self.whole_file {
            return self.clone();
        }

        let mut next = self.clone();
        let (up, down) = self.of(index);
        let entry = next.by_hunk.entry(index).or_default();
        *entry = match dir {
            Dir::Up => (up.saturating_add(lines), down),
            Dir::Down => (up, down.saturating_add(lines)),
        };
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_expanded_by_default() {
        let expansion = Expansion::parse(None);
        assert_eq!(expansion.of(0), (0, 0));
        assert_eq!(expansion.key(), "");
    }

    #[test]
    fn expansions_round_trip_through_their_key() {
        let expansion = Expansion::parse(Some("0u10,2d40"));
        assert_eq!(expansion.of(0), (10, 0));
        assert_eq!(expansion.of(2), (0, 40));
        assert_eq!(expansion.key(), "0u10,2d40");
    }

    #[test]
    fn the_whole_file_opens_every_hunk() {
        let expansion = Expansion::parse(Some(Expansion::WHOLE_FILE));
        assert_eq!(expansion.of(7), (usize::MAX, usize::MAX));
        assert_eq!(expansion.key(), "file");
    }

    #[test]
    fn junk_terms_are_dropped_rather_than_failing() {
        let expansion = Expansion::parse(Some("nonsense,1u5,xuy"));
        assert_eq!(expansion.of(1), (5, 0));
        assert_eq!(expansion.key(), "1u5");
    }

    #[test]
    fn expanding_accumulates_on_one_side() {
        let expansion = Expansion::parse(Some("0u10"))
            .plus(0, Dir::Up, 10)
            .plus(0, Dir::Down, 3);

        assert_eq!(expansion.of(0), (20, 3));
        assert_eq!(expansion.key(), "0u20,0d3");
    }

    #[test]
    fn a_whole_file_expansion_has_nothing_left_to_open() {
        let expansion = Expansion::parse(Some(Expansion::WHOLE_FILE)).plus(0, Dir::Up, 10);
        assert_eq!(expansion.key(), Expansion::WHOLE_FILE);
    }
}
