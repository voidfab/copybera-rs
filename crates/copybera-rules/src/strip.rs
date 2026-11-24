//! Reference stripping that takes a reference's punctuation with it.
//!
//! A plain `replace_all` leaves `(, seen on v2.1)`, `; it replaced`, a squash subject ending in ` ()`
//! and every docs header's `owner= `. Here each match is taken with its context:
//!
//! - the id that is the whole of a parenthetical takes the parentheses (and the space before them);
//! - the id that is the whole of a list item (`,` or `;` separated, in a parenthetical or a line)
//!   takes its separator;
//! - a docs header's `owner=<id>` field goes whole, with its `;` separator;
//! - the id that is the whole of an inline code span or of a bracket takes the backticks or the brackets, and a
//!   markdown link's `(target)` after the bracket, then the rules above apply to what is left around it;
//! - otherwise the id goes alone, and a space it leaves before `,;:.)!?` goes with it, as does the second of two
//!   spaces it leaves between words.
//!
//! A second pass reads every deletion site and names any `()`, `(,`, `; ;`, `owner=`, and on the same line an
//! empty code span or bracket (`` `` ``, `[]`) left there; the caller refuses the run on one. It reads only
//! deletion sites, so a `()` the origin wrote (`fn keep()`) is never a finding.

use regex::Regex;

/// One strip of a text: the result, how many references it took, and the leftovers the second
/// pass found at its deletion sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stripped {
    pub text: String,
    pub hits: usize,
    pub leftovers: Vec<&'static str>,
}

/// Every regex in turn, each on the text the one before it left.
pub fn strip_references(text: &str, regexes: &[Regex]) -> Stripped {
    let mut current = text.to_string();
    let mut hits = 0;
    let mut leftovers = Vec::new();
    for regex in regexes {
        let pass = strip_one(&current, regex);
        hits += pass.hits;
        leftovers.extend(pass.leftovers);
        current = pass.text;
    }
    Stripped {
        text: current,
        hits,
        leftovers,
    }
}

fn strip_one(text: &str, regex: &Regex) -> Stripped {
    let mut out = Sites::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut hits = 0usize;
    for m in regex.find_iter(text) {
        if m.end() <= cursor {
            // Inside a span an earlier match already took (a skipped separator).
            continue;
        }
        let (start, end) = enclosing(text, m.start().max(cursor), m.end(), cursor);
        hits += 1;
        out.push_str(&text[cursor..start]);
        cursor = end;

        if out.take_owner_field() {
            if !out.pop_separator(&[';']) {
                cursor = skip_separator(text, cursor, ';');
            }
            out.mark();
            continue;
        }

        let left = out.last_mark();
        let (right, right_at) = next_mark(text, cursor);
        // A parenthetical that wraps a line end: read the marks across the newline, and when the id is all
        // that is left between `(` and `)`, the parenthetical goes whole with the line break inside it.
        // A blank line on either side is a paragraph break: the second pass still refuses it.
        if left.is_none() || right.is_none() {
            let kept = out.text.trim_end();
            let wide_left = kept.chars().next_back();
            let rest = &text[cursor..];
            let wide_right_at = cursor + (rest.len() - rest.trim_start().len());
            let blank = out.text[kept.len()..].matches('\n').count() > 1
                || text[cursor..wide_right_at].matches('\n').count() > 1;
            if !blank && text[wide_right_at..].starts_with(')') {
                let len = kept.len();
                match wide_left {
                    Some('(') => {
                        out.truncate(len);
                        out.pop_char('(');
                        out.trim_spaces();
                        cursor = wide_right_at + 1;
                        out.mark();
                        continue;
                    }
                    Some(',' | ';') => {
                        // The last item of a wrapped list: its separator goes, the close stays.
                        out.truncate(len);
                        out.pop_any(&[',', ';']);
                        out.trim_spaces();
                        cursor = wide_right_at;
                        out.mark();
                        continue;
                    }
                    _ => {}
                }
            }
        }
        match (left, right) {
            (Some('('), Some(')')) => {
                // The id is the whole parenthetical.
                out.pop_char('(');
                out.trim_spaces();
                cursor = right_at + 1;
                if out.at_line_start() {
                    cursor = skip_spaces(text, cursor);
                }
            }
            (Some('('), Some(',' | ';')) => {
                // The first item of a parenthetical list.
                cursor = skip_spaces(text, right_at + 1);
            }
            (Some(',' | ';'), right) if !right.is_some_and(is_word) => {
                // A later list item: its separator goes, the next item or the close stays.
                out.trim_spaces();
                out.pop_any(&[',', ';']);
                cursor = right_at;
            }
            (None, Some(',' | ';')) => {
                // The first item of a line.
                cursor = skip_spaces(text, right_at + 1);
            }
            _ => {
                let next = text[cursor..].chars().next();
                if next == Some(':') && out.opens_line_item_or_comment() {
                    cursor = skip_spaces(text, cursor + ':'.len_utf8());
                } else if out.ends_with('(') || out.at_line_start() {
                    cursor = skip_spaces(text, cursor);
                } else if out.ends_with(' ')
                    && next.is_some_and(|c| matches!(c, ',' | ';' | ':' | '.' | ')' | '!' | '?'))
                {
                    out.trim_spaces();
                } else if out.ends_with(' ') && next == Some(' ') {
                    // `the slice PROJ-abc123 landed`: one space stays between the words.
                    cursor = skip_spaces(text, cursor);
                }
            }
        }
        out.mark();
    }
    out.push_str(&text[cursor..]);
    let leftovers = out.leftovers();
    Stripped {
        text: out.text,
        hits,
        leftovers,
    }
}

/// The output so far and the deletion sites in it. A truncation clamps every site past it.
struct Sites {
    text: String,
    sites: Vec<usize>,
}

impl Sites {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            text: String::with_capacity(capacity),
            sites: Vec::new(),
        }
    }

    fn push_str(&mut self, s: &str) {
        self.text.push_str(s);
    }

    fn mark(&mut self) {
        self.sites.push(self.text.len());
    }

    fn truncate(&mut self, len: usize) {
        self.text.truncate(len);
        for site in &mut self.sites {
            *site = (*site).min(len);
        }
    }

    fn ends_with(&self, c: char) -> bool {
        self.text.ends_with(c)
    }

    /// The last character before trailing spaces, or `None` at the start of a line.
    fn last_mark(&self) -> Option<char> {
        match self.text.trim_end_matches([' ', '\t']).chars().next_back() {
            None | Some('\n') | Some('\r') => None,
            Some(c) => Some(c),
        }
    }

    fn at_line_start(&self) -> bool {
        matches!(self.text.chars().next_back(), None | Some('\n'))
    }

    /// Whether the text before a match contains only a line-opening comment or Markdown list marker.
    fn opens_line_item_or_comment(&self) -> bool {
        let line = self.text.rsplit('\n').next().unwrap_or("").trim();
        matches!(line, "" | "//" | "///" | "//!" | "#" | "*" | "-" | "+")
            || line.strip_suffix('.').is_some_and(|ordinal| {
                !ordinal.is_empty() && ordinal.bytes().all(|byte| byte.is_ascii_digit())
            })
    }

    fn trim_spaces(&mut self) {
        let len = self.text.trim_end_matches([' ', '\t']).len();
        self.truncate(len);
    }

    fn pop_char(&mut self, c: char) {
        if self.text.ends_with(c) {
            self.truncate(self.text.len() - c.len_utf8());
        }
    }

    fn pop_any(&mut self, chars: &[char]) {
        if let Some(last) = self.text.chars().next_back() {
            if chars.contains(&last) {
                self.truncate(self.text.len() - last.len_utf8());
            }
        }
    }

    /// Pops trailing spaces and one separator when the text ends with one; says whether it did.
    fn pop_separator(&mut self, separators: &[char]) -> bool {
        let trimmed = self.text.trim_end_matches([' ', '\t']);
        match trimmed.chars().next_back() {
            Some(c) if separators.contains(&c) => {
                let len = trimmed.len() - c.len_utf8();
                self.truncate(len);
                true
            }
            _ => false,
        }
    }

    /// A docs header's `owner=` key in front of the match: pops it and says so.
    fn take_owner_field(&mut self) -> bool {
        const KEY: &str = "owner=";
        if !self.text.ends_with(KEY) {
            return false;
        }
        let before = &self.text[..self.text.len() - KEY.len()];
        if before.chars().next_back().is_some_and(is_word) {
            return false;
        }
        self.truncate(before.len());
        true
    }

    /// The second pass: the punctuation a deletion left behind, by site.
    fn leftovers(&self) -> Vec<&'static str> {
        let mut found = Vec::new();
        for &site in &self.sites {
            let site = floor_boundary(&self.text, site);
            let before = self.text[..site].trim_end();
            let after = self.text[site..].trim_start();
            let open = before.ends_with('(');
            let close = after.starts_with(')');
            if open && close {
                found.push("()");
            } else if open && (after.starts_with(',') || after.starts_with(';')) {
                found.push("(,");
            } else if before.ends_with(';') && after.starts_with(';') {
                found.push("; ;");
            } else if before.ends_with(',') && after.starts_with(',') {
                found.push(", ,");
            } else if before.ends_with("owner=") {
                found.push("owner=");
            } else {
                // An empty code span or bracket, read on the deletion's own line (a fence on the next line is not one).
                // The backtick before the site opens a span only when the line holds an odd number of backticks up to
                // it: `a` (id) `b` joins two spans, which is no leftover.
                let before = self.text[..site].trim_end_matches([' ', '\t']);
                let after = self.text[site..].trim_start_matches([' ', '\t']);
                let line = &before[before.rfind('\n').map_or(0, |at| at + 1)..];
                let opens = line.matches('`').count() % 2 == 1;
                if opens && before.ends_with('`') && after.starts_with('`') {
                    found.push("``");
                } else if before.ends_with('[') && after.starts_with(']') {
                    found.push("[]");
                }
            }
        }
        found
    }
}

/// The span a match takes: the match itself, or the inline code span or bracket it is the whole of (a bracket takes a
/// markdown link's `(target)` on the same line with it). `floor` is where the text not yet copied starts.
fn enclosing(text: &str, start: usize, end: usize, floor: usize) -> (usize, usize) {
    let before = &text[floor..start];
    let after = &text[end..];
    if before.ends_with('`') && after.starts_with('`') {
        return (start - 1, end + 1);
    }
    if before.ends_with('[') && after.starts_with(']') {
        let close = end + 1;
        let rest = &text[close..];
        if rest.starts_with('(') {
            if let Some(at) = rest.find([')', '\n']) {
                if rest.as_bytes()[at] == b')' {
                    return (start - 1, close + at + 1);
                }
            }
        }
        return (start - 1, close);
    }
    (start, end)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The next character at or after `from` that is not a space or tab, with its index; `None` at a
/// line end or the end of the text (the index is then that line end).
fn next_mark(text: &str, from: usize) -> (Option<char>, usize) {
    let at = skip_spaces(text, from);
    match text[at..].chars().next() {
        None | Some('\n') | Some('\r') => (None, at),
        Some(c) => (Some(c), at),
    }
}

fn skip_spaces(text: &str, from: usize) -> usize {
    let rest = &text[from..];
    from + (rest.len() - rest.trim_start_matches([' ', '\t']).len())
}

fn skip_separator(text: &str, from: usize, separator: char) -> usize {
    let at = skip_spaces(text, from);
    if text[at..].starts_with(separator) {
        skip_spaces(text, at + separator.len_utf8())
    } else {
        from
    }
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(text: &str) -> Stripped {
        let regexes = [
            Regex::new("PROJ-[0-9a-f]{6}").unwrap(),
            Regex::new("logbook").unwrap(),
        ];
        strip_references(text, &regexes)
    }

    // The punctuation edge cases: an id that is the whole of an inline code
    // span or a bracket (a markdown link's text) takes them with it, and a deletion never leaves a double space.
    #[test]
    fn a_reference_takes_its_code_span_and_brackets() {
        for (input, want) in [
            ("the rule (`PROJ-abc123`) holds", "the rule holds"),
            ("(`PROJ-c0ffee`, seen on v2.1)", "(seen on v2.1)"),
            ("the slice `PROJ-abc123` landed", "the slice landed"),
            ("[PROJ-abc123] fixed", "fixed"),
            (
                "see [PROJ-abc123](../x/PROJ-abc123.md) and go",
                "see and go",
            ),
            ("a `logbook` note", "a note"),
            ("the slice PROJ-abc123 landed", "the slice landed"),
        ] {
            let got = strip(input);
            assert_eq!(got.text, want, "input {input:?}");
            assert!(
                got.leftovers.is_empty(),
                "input {input:?}: {:?}",
                got.leftovers
            );
        }
    }

    #[test]
    fn a_reference_heading_a_comment_or_a_list_item_takes_its_colon() {
        for (input, want) in [
            ("PROJ-abc123: text", "text"),
            ("// PROJ-abc123: text", "// text"),
            ("/// PROJ-abc123: text", "/// text"),
            ("//! PROJ-abc123: text", "//! text"),
            ("# PROJ-abc123: text", "# text"),
            ("* PROJ-abc123: text", "* text"),
            ("- PROJ-abc123: text", "- text"),
            ("+ PROJ-abc123: text", "+ text"),
            ("1. PROJ-abc123: text", "1. text"),
            ("a PROJ-abc123: detail", "a: detail"),
        ] {
            assert_eq!(strip(input).text, want, "input {input:?}");
        }
    }

    #[test]
    fn the_second_pass_refuses_an_empty_code_span_or_bracket() {
        let got = strip("code `PROJ-abc123 ` here");
        assert_eq!(got.leftovers, vec!["``"], "{got:?}");
        let got = strip("link [PROJ-abc123 ] here");
        assert_eq!(got.leftovers, vec!["[]"], "{got:?}");
        // A fence on the next line is not a leftover: the check never crosses a line end.
        let got = strip("```\nPROJ-abc123\n```\n");
        assert!(got.leftovers.is_empty(), "{got:?}");
        // Two code spans that a removed parenthetical brings together are not an empty one: the closing backtick of
        // the first and the opening one of the second are now neighbours, not a span.
        let got = strip("with `strict: true` (PROJ-888bbb) `parse_args`, `run`");
        assert_eq!(got.text, "with `strict: true` `parse_args`, `run`");
        assert!(got.leftovers.is_empty(), "{got:?}");
    }

    // A parenthetical that wraps across one line end goes whole when the ids are all it holds.
    #[test]
    fn a_parenthetical_of_ids_that_wraps_a_line_goes_whole() {
        for (input, want) in [
            ("after step one (PROJ-111aaa) and step two (\nPROJ-222bbb). The call", "after step one and step two. The call"),
            (
                "\"\"\"the parser and the printer (PROJ-333ccc, PROJ-444ddd,\n    PROJ-555eee, PROJ-666fff).\n\"\"\"",
                "\"\"\"the parser and the printer.\n\"\"\"",
            ),
            ("rules (\n  PROJ-777aaa\n) hold", "rules hold"),
            ("kept (beta,\nPROJ-777aaa) here", "kept (beta) here"),
        ] {
            let got = strip(input);
            assert_eq!(got.text, want, "input {input:?}");
            assert!(got.leftovers.is_empty(), "input {input:?}: {:?}", got.leftovers);
        }
    }
}
