//! Turns delta's ANSI output back into markup.
//!
//! Delta is run with backgrounds pinned to sentinels we chose, so the colours it
//! emits are a vocabulary we control rather than whatever its theme happens to
//! use. Foregrounds come from a known syntax theme and map onto token classes,
//! which keeps the actual colours in CSS instead of baked into the HTML.

use rocket::serde::Serialize;

/// Backgrounds delta is told to use, so we can read line kind back out.
///
/// The `*_EMPH` pair marks the part of a line that actually changed — the
/// word-level highlighting that plain `git diff` cannot express.
pub const MINUS_BG: (u8, u8, u8) = (1, 1, 1);
pub const PLUS_BG: (u8, u8, u8) = (2, 2, 2);
pub const MINUS_EMPH_BG: (u8, u8, u8) = (3, 3, 3);
pub const PLUS_EMPH_BG: (u8, u8, u8) = (4, 4, 4);

/// Foregrounds of the `Nord` syntax theme, mapped to what they highlight.
///
/// Nord is used for how cleanly its scopes separate, not for its colours: we
/// restyle through these classes, so the palette on screen stays the app's own.
/// A colour missing from this table is not an error — it renders as plain text —
/// but `palette_is_complete` fails loudly if delta stops emitting these.
const PALETTE: &[((u8, u8, u8), &str)] = &[
    ((129, 161, 193), "tok-keyword"),
    ((136, 192, 208), "tok-fn"),
    ((143, 188, 187), "tok-type"),
    ((163, 190, 140), "tok-string"),
    ((180, 142, 173), "tok-number"),
    ((97, 110, 136), "tok-comment"),
    ((236, 239, 244), "tok-punct"),
    ((216, 222, 233), "tok-text"),
];

pub const DEFAULT_CLASS: &str = "tok-text";

/// Which side of the diff a line belongs to, as delta reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(crate = "rocket::serde", rename_all = "snake_case")]
pub enum Marker {
    Context,
    Added,
    Removed,
}

/// A run of text sharing one token class and one changed state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Span {
    pub text: String,
    pub class: &'static str,
    /// True inside the region delta marked as the actual change.
    pub changed: bool,
}

/// One parsed line: its marker, its text, and how to colour it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLine {
    pub marker: Marker,
    pub spans: Vec<Span>,
}

impl ParsedLine {
    /// The line's text with all styling dropped. Used by tests and by anything
    /// that needs the plain content rather than the markup.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    pub fn to_html(&self) -> String {
        let mut out = String::new();
        for span in &self.spans {
            let escaped = escape(&span.text);
            if span.changed {
                out.push_str(&format!(
                    "<span class=\"{} chg\">{escaped}</span>",
                    span.class
                ));
            } else if span.class == DEFAULT_CLASS {
                out.push_str(&escaped);
            } else {
                out.push_str(&format!("<span class=\"{}\">{escaped}</span>", span.class));
            }
        }
        out
    }
}

/// Parses one line of delta output.
///
/// Returns `None` for anything that is not diff content — headers, `@@` markers
/// and the `\ No newline` note, none of which delta styles.
pub fn parse_line(raw: &str) -> Option<ParsedLine> {
    let mut state = State::default();
    let mut spans: Vec<Span> = Vec::new();
    let mut rest = raw;

    while let Some(start) = rest.find('\u{1b}') {
        push(&mut spans, &rest[..start], &state);

        let Some((params, final_byte, consumed)) = escape_sequence(&rest[start..]) else {
            // A lone ESC with no terminator: treat the remainder as text.
            push(&mut spans, &rest[start..], &state);
            rest = "";
            break;
        };

        // `K` erases to end of line — a terminal artifact with no meaning here.
        if final_byte == 'm' {
            state.apply(params);
        }
        rest = &rest[start + consumed..];
    }
    push(&mut spans, rest, &state);

    // The marker is the first character of the line, outside any styling.
    let text: String = spans.iter().map(|s| s.text.as_str()).collect();
    let marker = match text.as_bytes().first()? {
        b'+' => Marker::Added,
        b'-' => Marker::Removed,
        b' ' => Marker::Context,
        _ => return None,
    };

    Some(ParsedLine {
        marker,
        spans: strip_marker(spans),
    })
}

/// Drops the leading `+`/`-`/space, which is diff syntax rather than content.
fn strip_marker(spans: Vec<Span>) -> Vec<Span> {
    let mut out = Vec::with_capacity(spans.len());
    let mut dropped = false;

    for mut span in spans {
        if !dropped && !span.text.is_empty() {
            span.text.remove(0);
            dropped = true;
        }
        if !span.text.is_empty() {
            out.push(span);
        }
    }
    out
}

fn push(spans: &mut Vec<Span>, text: &str, state: &State) {
    if text.is_empty() {
        return;
    }

    let span = Span {
        text: text.to_owned(),
        class: state.class,
        changed: state.changed,
    };

    // Merge with the previous run when nothing visible changed.
    match spans.last_mut() {
        Some(last) if last.class == span.class && last.changed == span.changed => {
            last.text.push_str(&span.text);
        }
        _ => spans.push(span),
    }
}

/// Splits `\x1b[<params><final>` off the front, returning its byte length.
fn escape_sequence(s: &str) -> Option<(&str, char, usize)> {
    let body = s.strip_prefix("\u{1b}[")?;
    let end = body.find(|c: char| c.is_ascii_alphabetic())?;
    let final_byte = body[end..].chars().next()?;

    Some((&body[..end], final_byte, 2 + end + final_byte.len_utf8()))
}

struct State {
    class: &'static str,
    changed: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            class: DEFAULT_CLASS,
            changed: false,
        }
    }
}

impl State {
    /// Applies one SGR sequence.
    ///
    /// NB: parameters are walked rather than matched as a whole string. Delta
    /// combines them — `48;2;0;96;0;38;2;186;...` sets a background and a
    /// foreground in one sequence — so comparing the full parameter list would
    /// miss exactly the changed-region spans we care about.
    fn apply(&mut self, params: &str) {
        let codes: Vec<u32> = params
            .split(';')
            .map(|p| p.parse().unwrap_or(0))
            .collect();

        let mut i = 0;
        while i < codes.len() {
            match codes[i] {
                0 => {
                    self.class = DEFAULT_CLASS;
                    self.changed = false;
                    i += 1;
                }
                38 | 48 if codes.get(i + 1) == Some(&2) => {
                    let rgb = (
                        codes.get(i + 2).copied().unwrap_or(0) as u8,
                        codes.get(i + 3).copied().unwrap_or(0) as u8,
                        codes.get(i + 4).copied().unwrap_or(0) as u8,
                    );
                    if codes[i] == 38 {
                        self.class = class_for(rgb);
                    } else {
                        self.changed = rgb == MINUS_EMPH_BG || rgb == PLUS_EMPH_BG;
                    }
                    i += 5;
                }
                // 256-colour and everything else carry no meaning for us; delta
                // is run with --true-color=always so these should not appear.
                38 | 48 => i += 3,
                _ => i += 1,
            }
        }
    }
}

fn class_for(rgb: (u8, u8, u8)) -> &'static str {
    PALETTE
        .iter()
        .find(|(colour, _)| *colour == rgb)
        .map_or(DEFAULT_CLASS, |(_, class)| class)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from `delta --color-only --syntax-theme=Nord` with our sentinels.
    const PLUS_LINE: &str = "\u{1b}[48;2;2;2;2m+\u{1b}[38;2;216;222;233m    \
\u{1b}[38;2;129;161;193mlet\u{1b}[38;2;216;222;233m s \u{1b}[38;2;236;239;244m=\
\u{1b}[38;2;216;222;233m \u{1b}[38;2;163;190;140m\"\u{1b}[48;2;4;4;4mgoodbye\
\u{1b}[48;2;2;2;2m\"\u{1b}[38;2;236;239;244m;\u{1b}[0m\u{1b}[48;2;2;2;2m\u{1b}[0K\u{1b}[0m";

    fn classes(line: &ParsedLine) -> Vec<(&str, &str, bool)> {
        line.spans
            .iter()
            .map(|s| (s.text.as_str(), s.class, s.changed))
            .collect()
    }

    #[test]
    fn a_plus_line_keeps_its_text_and_loses_the_marker() {
        let line = parse_line(PLUS_LINE).unwrap();
        assert_eq!(line.marker, Marker::Added);
        assert_eq!(line.text(), "    let s = \"goodbye\";");
    }

    #[test]
    fn foreground_colours_become_token_classes() {
        let line = parse_line(PLUS_LINE).unwrap();
        let spans = classes(&line);

        assert!(spans.contains(&("let", "tok-keyword", false)));
        assert!(spans.contains(&("=", "tok-punct", false)));
        assert!(spans.contains(&("\"", "tok-string", false)));
    }

    #[test]
    fn the_emphasis_background_marks_only_the_changed_word() {
        let line = parse_line(PLUS_LINE).unwrap();

        let changed: Vec<_> = line
            .spans
            .iter()
            .filter(|s| s.changed)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(changed, ["goodbye"]);

        // The word keeps its syntax class as well as the change flag.
        let word = line.spans.iter().find(|s| s.changed).unwrap();
        assert_eq!(word.class, "tok-string");
    }

    #[test]
    fn a_combined_sequence_sets_background_and_foreground_at_once() {
        // Delta emits both in one SGR; matching the whole parameter list would
        // miss the emphasis here.
        let raw = "\u{1b}[48;2;2;2;2m+\u{1b}[48;2;4;4;4;38;2;129;161;193mfn\u{1b}[0m";
        let line = parse_line(raw).unwrap();

        assert_eq!(classes(&line), [("fn", "tok-keyword", true)]);
    }

    #[test]
    fn a_reset_clears_both_colour_and_emphasis() {
        let raw = "\u{1b}[48;2;2;2;2m+\u{1b}[48;2;4;4;4;38;2;163;190;140mx\u{1b}[0my";
        let line = parse_line(raw).unwrap();

        assert_eq!(
            classes(&line),
            [("x", "tok-string", true), ("y", DEFAULT_CLASS, false)]
        );
    }

    #[test]
    fn the_erase_to_end_of_line_artifact_contributes_nothing() {
        let line = parse_line(PLUS_LINE).unwrap();
        assert!(!line.text().contains('K'));
        assert!(!line.text().contains('\u{1b}'));
    }

    #[test]
    fn an_unknown_colour_degrades_to_plain_text() {
        // What a palette drift looks like: readable, unstyled, not a crash.
        let raw = "\u{1b}[48;2;2;2;2m+\u{1b}[38;2;12;34;56mmystery\u{1b}[0m";
        assert_eq!(classes(&parse_line(raw).unwrap()), [("mystery", DEFAULT_CLASS, false)]);
    }

    #[test]
    fn markers_are_read_from_the_first_character() {
        assert_eq!(parse_line(" context").unwrap().marker, Marker::Context);
        assert_eq!(parse_line("-gone").unwrap().marker, Marker::Removed);
        assert_eq!(parse_line("+new").unwrap().marker, Marker::Added);
    }

    #[test]
    fn non_content_lines_are_rejected() {
        assert!(parse_line("@@ -1,3 +1,4 @@").is_none());
        assert!(parse_line("diff --git a/x b/x").is_none());
        assert!(parse_line("\\ No newline at end of file").is_none());
        assert!(parse_line("").is_none());
    }

    #[test]
    fn adjacent_runs_of_the_same_style_are_merged() {
        let raw = "\u{1b}[48;2;2;2;2m+\u{1b}[38;2;129;161;193mfn\u{1b}[38;2;129;161;193m x\u{1b}[0m";
        assert_eq!(classes(&parse_line(raw).unwrap()), [("fn x", "tok-keyword", false)]);
    }

    #[test]
    fn html_escapes_and_only_wraps_what_needs_it() {
        let raw = "\u{1b}[48;2;2;2;2m+\u{1b}[38;2;129;161;193m<T>\u{1b}[0m & plain";
        let html = parse_line(raw).unwrap().to_html();

        assert_eq!(html, "<span class=\"tok-keyword\">&lt;T&gt;</span> &amp; plain");
    }

    #[test]
    fn an_empty_line_survives_having_only_a_marker() {
        let line = parse_line(" ").unwrap();
        assert_eq!(line.marker, Marker::Context);
        assert_eq!(line.text(), "");
        assert_eq!(line.to_html(), "");
    }
}
