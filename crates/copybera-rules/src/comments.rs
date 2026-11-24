//! `strip_comments`: a per-language lexer finds every comment and removes the ones that are not kept. It is off unless a workflow lists
//! it.
//!
//! Kept: a shebang, an SPDX header anywhere, a licence or copyright comment before the first code, tool directives
//! (`@ts-expect-error`, `eslint-disable`, `# type:`, `# noqa`, `swiftlint:` and the like), and with `keep_docs` (the
//! default) doc comments (`///`, `//!`, `/** */`, `/*! */`). Never touched: strings, raw strings, template literals,
//! regex literals, character literals, f-string expressions. A file the lexer cannot read to its end (an unterminated
//! string or comment) is left as it was. A line left blank by a removal goes whole; a removal between two tokens
//! leaves one space so they do not join.

/// The languages the lexer reads, chosen by suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lang {
    Rust,
    /// TypeScript and JavaScript (`.ts`, `.mts`, `.cts`, `.js`, `.mjs`, `.cjs`).
    Script,
    /// `.tsx` and `.jsx`: JSX text is not code, so only comments that are whole lines, or that end a line holding
    /// code, are read there.
    Jsx,
    Python,
    Swift,
}

impl Lang {
    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Script => "typescript",
            Lang::Jsx => "tsx",
            Lang::Python => "python",
            Lang::Swift => "swift",
        }
    }

    pub fn from_name(name: &str) -> Option<Lang> {
        [
            Lang::Rust,
            Lang::Script,
            Lang::Jsx,
            Lang::Python,
            Lang::Swift,
        ]
        .into_iter()
        .find(|l| l.name() == name)
    }

    pub fn of_path(path: &str) -> Option<Lang> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let suffix = name.rsplit_once('.').map(|(_, s)| s)?;
        Some(match suffix {
            "rs" => Lang::Rust,
            "ts" | "mts" | "cts" | "js" | "mjs" | "cjs" => Lang::Script,
            "tsx" | "jsx" => Lang::Jsx,
            "py" => Lang::Python,
            "swift" => Lang::Swift,
            _ => return None,
        })
    }
}

/// What one strip removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommentStats {
    pub comments: usize,
    pub kept: usize,
    pub lines: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
    doc: bool,
    header: bool,
}

/// The text with its comments stripped, or `None` when the lexer cannot read the file to its end (it is then left as
/// it was) or nothing is removed.
pub fn strip_comments(text: &str, lang: Lang, keep_docs: bool) -> Option<(String, CommentStats)> {
    let spans = match lang {
        Lang::Python => lex_python(text.as_bytes())?,
        _ => lex_c(text.as_bytes(), lang)?,
    };
    let mut stats = CommentStats::default();
    let mut mask = vec![false; text.len()];
    for span in spans {
        let body = &text[span.start..span.end];
        if keeps(body, span, lang, keep_docs) {
            stats.kept += 1;
            continue;
        }
        if lang == Lang::Jsx && !jsx_safe(text, span) {
            stats.kept += 1;
            continue;
        }
        stats.comments += 1;
        mask[span.start..span.end].fill(true);
    }
    if stats.comments == 0 {
        return None;
    }
    let (out, lines) = rebuild(text, &mask);
    stats.lines = lines;
    stats.bytes = text.len() - out.len();
    Some((out, stats))
}

/// In JSX a `//` or `/*` inside element children is text. A comment is read only when it is a whole line that does
/// not sit between a `>` and a `<` (an element's children), or when it ends a line whose code ends in `;`, `,`, `{`,
/// `(` or `)`.
fn jsx_safe(text: &str, span: Span) -> bool {
    let line_start = text[..span.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[span.end..]
        .find('\n')
        .map_or(text.len(), |i| span.end + i);
    let before = text[line_start..span.start].trim();
    let after = text[span.end..line_end].trim();
    if before.is_empty() && after.is_empty() {
        let prev = text[..line_start].trim_end().chars().next_back();
        let next = text[line_end..].trim_start().chars().next();
        return !(prev == Some('>') || next == Some('<'));
    }
    after.is_empty() && before.ends_with([';', ',', '{', '(', ')'])
}

const SCRIPT_DIRECTIVES: &[&str] = &[
    "/ <reference",
    "@ts-",
    "eslint",
    "oxlint",
    "biome-ignore",
    "prettier-ignore",
    "istanbul ",
    "c8 ",
    "#__PURE__",
    "@__PURE__",
    "#__NO_SIDE_EFFECTS__",
    "webpack",
    "@vite-ignore",
    "@jsx",
    "@refresh",
    "global ",
    "jshint",
    "deno-",
    "# sourceMappingURL",
    "@license",
    "@preserve",
    "!",
];
const PYTHON_DIRECTIVES: &[&str] = &[
    "type:", "noqa", "pragma", "fmt:", "pyright:", "mypy:", "ruff:", "isort:", "pylint:", "-*-",
    "nosec", "pyre-", "!",
];
const SWIFT_DIRECTIVES: &[&str] = &["swiftlint:", "swift-format-ignore", "sourcery:"];

fn keeps(body: &str, span: Span, lang: Lang, keep_docs: bool) -> bool {
    if span.doc && keep_docs {
        return true;
    }
    if body.contains("SPDX-License-Identifier") {
        return true;
    }
    let lower = body.to_lowercase();
    if span.header
        && ["copyright", "license", "licence", "@generated"]
            .iter()
            .any(|w| lower.contains(w))
    {
        return true;
    }
    let inner = body
        .trim_start_matches(['/', '*', '#'])
        .trim_start_matches([' ', '\t']);
    let raw = body
        .strip_prefix("//")
        .or_else(|| body.strip_prefix("/*"))
        .or_else(|| body.strip_prefix('#'))
        .unwrap_or(body);
    match lang {
        Lang::Script | Lang::Jsx => SCRIPT_DIRECTIVES
            .iter()
            .any(|d| inner.starts_with(d) || raw.starts_with(d)),
        Lang::Python => {
            PYTHON_DIRECTIVES
                .iter()
                .any(|d| inner.starts_with(d) || raw.starts_with(d))
                || lower.contains("noqa")
                || lower.contains("type: ignore")
                || lower.contains("pragma: no")
        }
        Lang::Swift => SWIFT_DIRECTIVES.iter().any(|d| inner.starts_with(d)),
        Lang::Rust => false,
    }
}

/// The text with the masked bytes removed, and the number of lines dropped whole. A line whose every non-blank byte
/// is masked goes with its newline; a masked run that ends a line takes the blanks before it; a masked run between two
/// tokens leaves one space.
fn rebuild(text: &str, mask: &[bool]) -> (String, usize) {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    let mut dropped = 0;
    let mut start = 0;
    while start < bytes.len() {
        let nl = bytes[start..].iter().position(|b| *b == b'\n');
        let end = nl.map_or(bytes.len(), |i| start + i); // content end (exclusive), newline at `end` if any
        let next = nl.map_or(bytes.len(), |i| start + i + 1);
        let line = &bytes[start..end];
        let masked = &mask[start..end];
        if !masked.iter().any(|m| *m) {
            out.extend_from_slice(&bytes[start..next]);
            start = next;
            continue;
        }
        let blank_or_masked = line
            .iter()
            .zip(masked)
            .all(|(b, m)| *m || b.is_ascii_whitespace());
        if blank_or_masked {
            dropped += 1;
            start = next;
            continue;
        }
        let mut i = 0;
        let mut piece: Vec<u8> = Vec::with_capacity(line.len());
        while i < line.len() {
            if !masked[i] {
                piece.push(line[i]);
                i += 1;
                continue;
            }
            let run_start = i;
            while i < line.len() && masked[i] {
                i += 1;
            }
            if i == line.len() {
                while piece.last().is_some_and(|b| *b == b' ' || *b == b'\t') {
                    piece.pop();
                }
            } else if piece.last().is_none_or(|b| b.is_ascii_whitespace()) {
                // Blanks on both sides of the removal: one side's are enough.
                while i < line.len() && !masked[i] && (line[i] == b' ' || line[i] == b'\t') {
                    if run_start == 0 {
                        piece.push(line[i]);
                    }
                    i += 1;
                }
            } else if !line[i].is_ascii_whitespace() {
                piece.push(b' ');
            }
        }
        out.extend_from_slice(&piece);
        out.extend_from_slice(&bytes[end..next]);
        start = next;
    }
    (
        String::from_utf8(out).expect("only whole ASCII-delimited comment spans are removed"),
        dropped,
    )
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Rust, TypeScript / JavaScript and Swift.
fn lex_c(b: &[u8], lang: Lang) -> Option<Vec<Span>> {
    let n = b.len();
    let mut spans = Vec::new();
    let mut i = 0;
    let mut code_seen = false;
    // A shebang line (a Rust `#![` is an inner attribute, not a shebang).
    if b.starts_with(b"#!") && !(lang == Lang::Rust && b.starts_with(b"#![")) {
        i = b.iter().position(|c| *c == b'\n').unwrap_or(n);
    }
    let script = matches!(lang, Lang::Script | Lang::Jsx);
    // Template literals: the brace depth at which each open `${` resumes its template.
    let mut templates: Vec<usize> = Vec::new();
    let mut depth = 0usize;
    // For a regex literal: whether a `/` here starts an expression.
    let mut expr_start = true;
    while i < n {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            let end = b[i..].iter().position(|x| *x == b'\n').map_or(n, |p| i + p);
            let doc = match lang {
                Lang::Rust => {
                    (b[i..].starts_with(b"///") && !b[i..].starts_with(b"////"))
                        || b[i..].starts_with(b"//!")
                }
                Lang::Swift => b[i..].starts_with(b"///") && !b[i..].starts_with(b"////"),
                _ => false,
            };
            spans.push(Span {
                start: i,
                end,
                doc,
                header: !code_seen,
            });
            i = end;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let nested = matches!(lang, Lang::Rust | Lang::Swift);
            let mut j = i + 2;
            let mut level = 1;
            while j < n && level > 0 {
                if nested && b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                    level += 1;
                    j += 2;
                } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                    level -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            if level > 0 {
                return None;
            }
            let text = &b[i..j];
            let doc = (text.starts_with(b"/**")
                && !text.starts_with(b"/**/")
                && !text.starts_with(b"/***"))
                || (lang == Lang::Rust && text.starts_with(b"/*!"));
            spans.push(Span {
                start: i,
                end: j,
                doc,
                header: !code_seen,
            });
            i = j;
            continue;
        }
        code_seen = true;
        match lang {
            Lang::Rust => {
                if is_ident(c) && !c.is_ascii_digit() {
                    let s = i;
                    while i < n && is_ident(b[i]) {
                        i += 1;
                    }
                    let word = &b[s..i];
                    if matches!(word, b"r" | b"br" | b"cr") && matches!(b.get(i), Some(b'"' | b'#'))
                    {
                        let mut j = i;
                        while j < n && b[j] == b'#' {
                            j += 1;
                        }
                        if b.get(j) == Some(&b'"') {
                            let hashes = j - i;
                            let mut k = j + 1;
                            loop {
                                if k >= n {
                                    return None;
                                }
                                if b[k] == b'"'
                                    && b[k + 1..]
                                        .iter()
                                        .take(hashes)
                                        .filter(|x| **x == b'#')
                                        .count()
                                        == hashes
                                {
                                    i = k + 1 + hashes;
                                    break;
                                }
                                k += 1;
                            }
                        }
                        // else a raw identifier (`r#type`): the next pass reads `#` as punctuation
                    } else if matches!(word, b"b" | b"c") && b.get(i) == Some(&b'"') {
                        i = skip_quoted(b, i, b'"')?;
                    } else if word == b"b" && b.get(i) == Some(&b'\'') {
                        i = skip_quoted(b, i, b'\'')?;
                    }
                    continue;
                }
                if c == b'"' {
                    i = skip_quoted(b, i, b'"')?;
                    continue;
                }
                if c == b'\'' {
                    if b.get(i + 1) == Some(&b'\\') {
                        i = skip_quoted(b, i, b'\'')?;
                        continue;
                    }
                    let len = utf8_len(b.get(i + 1).copied().unwrap_or(0));
                    if b.get(i + 1 + len) == Some(&b'\'') {
                        i += len + 2;
                    } else {
                        i += 1; // a lifetime or a label
                    }
                    continue;
                }
                i += 1;
            }
            Lang::Swift => {
                if c == b'#' {
                    let mut j = i;
                    while j < n && b[j] == b'#' {
                        j += 1;
                    }
                    if b.get(j) == Some(&b'"') {
                        i = skip_swift_string(b, j, j - i)?;
                        continue;
                    }
                    i += 1;
                    continue;
                }
                if c == b'"' {
                    i = skip_swift_string(b, i, 0)?;
                    continue;
                }
                i += 1;
            }
            Lang::Script | Lang::Jsx => {
                debug_assert!(script);
                match c {
                    b'"' | b'\'' => {
                        // A quote that does not close on its line is JSX text (`don't`), not a string.
                        match skip_quoted_line(b, i, c) {
                            Some(next) => i = next,
                            None => i += 1,
                        }
                        expr_start = false;
                    }
                    b'`' => {
                        let (next, ended) = scan_template(b, i + 1)?;
                        i = next;
                        if !ended {
                            templates.push(depth);
                            depth += 1;
                            expr_start = true;
                        } else {
                            expr_start = false;
                        }
                    }
                    b'{' => {
                        depth += 1;
                        i += 1;
                        expr_start = true;
                    }
                    b'}' => {
                        depth = depth.saturating_sub(1);
                        if templates.last() == Some(&depth) {
                            templates.pop();
                            let (next, ended) = scan_template(b, i + 1)?;
                            i = next;
                            if !ended {
                                templates.push(depth);
                                depth += 1;
                                expr_start = true;
                            } else {
                                expr_start = false;
                            }
                        } else {
                            i += 1;
                            expr_start = true;
                        }
                    }
                    b'/' if expr_start => {
                        // A slash that does not close a regex on its line is division (or a JSX closing tag).
                        i = skip_regex(b, i).unwrap_or(i + 1);
                        expr_start = false;
                    }
                    _ if is_ident(c) => {
                        let s = i;
                        while i < n && (is_ident(b[i]) || b[i] == b'$') {
                            i += 1;
                        }
                        let word = &b[s..i];
                        expr_start = matches!(
                            word,
                            b"return"
                                | b"typeof"
                                | b"case"
                                | b"do"
                                | b"else"
                                | b"in"
                                | b"of"
                                | b"new"
                                | b"delete"
                                | b"void"
                                | b"throw"
                                | b"yield"
                                | b"await"
                                | b"instanceof"
                        );
                    }
                    b')' | b']' | b'$' => {
                        i += 1;
                        expr_start = false;
                    }
                    _ => {
                        i += 1;
                        expr_start = true;
                    }
                }
            }
            Lang::Python => unreachable!("python has its own lexer"),
        }
    }
    if !templates.is_empty() {
        return None;
    }
    Some(spans)
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

/// Past a quoted literal starting at `i` (the quote), backslash escapes honoured.
fn skip_quoted(b: &[u8], i: usize, quote: u8) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            x if x == quote => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// [`skip_quoted`] that stops at a line end: a JavaScript string never holds a raw newline.
fn skip_quoted_line(b: &[u8], i: usize, quote: u8) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'\n' => return None,
            x if x == quote => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// A template literal's text from `i`: `(after, true)` past the closing backtick, `(after, false)` past a `${`.
fn scan_template(b: &[u8], mut i: usize) -> Option<(usize, bool)> {
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'`' => return Some((i + 1, true)),
            b'$' if b.get(i + 1) == Some(&b'{') => return Some((i + 2, false)),
            _ => i += 1,
        }
    }
    None
}

/// Past a regex literal starting at `i` (the slash) and its flags.
fn skip_regex(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    let mut class = false;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'[' => {
                class = true;
                j += 1
            }
            b']' => {
                class = false;
                j += 1
            }
            b'/' if !class => {
                j += 1;
                while j < b.len() && b[j].is_ascii_alphabetic() {
                    j += 1;
                }
                return Some(j);
            }
            b'\n' => return None,
            _ => j += 1,
        }
    }
    None
}

/// Past a Swift string at `i` (its first quote) with `hashes` raw delimiters, `\(...)` interpolations included.
fn skip_swift_string(b: &[u8], i: usize, hashes: usize) -> Option<usize> {
    let multi = b[i..].starts_with(b"\"\"\"");
    let open = if multi { 3 } else { 1 };
    let mut j = i + open;
    let closes = |j: usize| {
        let q = if multi { &b"\"\"\""[..] } else { &b"\""[..] };
        b[j..].starts_with(q)
            && b[j + q.len()..]
                .iter()
                .take(hashes)
                .filter(|x| **x == b'#')
                .count()
                == hashes
    };
    while j < b.len() {
        if closes(j) {
            return Some(j + open + hashes);
        }
        if b[j] == b'\\'
            && b[j + 1..]
                .iter()
                .take(hashes)
                .filter(|x| **x == b'#')
                .count()
                == hashes
        {
            let after = j + 1 + hashes;
            if b.get(after) == Some(&b'(') {
                // An interpolation: balanced parentheses, nested strings skipped.
                let mut level = 0usize;
                let mut k = after;
                while k < b.len() {
                    match b[k] {
                        b'(' => level += 1,
                        b')' => {
                            level -= 1;
                            if level == 0 {
                                break;
                            }
                        }
                        b'"' => {
                            k = skip_swift_string(b, k, 0)?;
                            continue;
                        }
                        _ => {}
                    }
                    k += 1;
                }
                j = k + 1;
                continue;
            }
            j = after + 1;
            continue;
        }
        if !multi && b[j] == b'\n' {
            return None;
        }
        j += 1;
    }
    None
}

/// Python: `#` comments outside strings; string prefixes, triple quotes and f-string expressions read.
fn lex_python(b: &[u8]) -> Option<Vec<Span>> {
    let n = b.len();
    let mut spans = Vec::new();
    let mut i = 0;
    let mut code_seen = false;
    while i < n {
        let c = b[i];
        if c.is_ascii_whitespace() || c == b'\\' {
            i += 1;
            continue;
        }
        if c == b'#' {
            let end = b[i..].iter().position(|x| *x == b'\n').map_or(n, |p| i + p);
            spans.push(Span {
                start: i,
                end,
                doc: false,
                header: !code_seen,
            });
            i = end;
            continue;
        }
        code_seen = true;
        if is_ident(c) {
            let s = i;
            while i < n && is_ident(b[i]) {
                i += 1;
            }
            let word = b[s..i].to_ascii_lowercase();
            if matches!(b.get(i), Some(b'"' | b'\''))
                && word.len() <= 2
                && word.iter().all(|x| b"rbuf".contains(x))
            {
                i = skip_py_string(b, i, word.contains(&b'f'))?;
            }
            continue;
        }
        if c == b'"' || c == b'\'' {
            i = skip_py_string(b, i, false)?;
            continue;
        }
        i += 1;
    }
    Some(spans)
}

fn skip_py_string(b: &[u8], i: usize, fstring: bool) -> Option<usize> {
    let q = b[i];
    let triple = b[i..].starts_with(&[q, q, q]);
    let mut j = i + if triple { 3 } else { 1 };
    while j < b.len() {
        let x = b[j];
        if x == b'\\' {
            j += 2;
            continue;
        }
        if triple {
            if b[j..].starts_with(&[q, q, q]) {
                return Some(j + 3);
            }
        } else if x == q {
            return Some(j + 1);
        } else if x == b'\n' {
            return None;
        }
        if fstring && x == b'{' {
            if b.get(j + 1) == Some(&b'{') {
                j += 2;
                continue;
            }
            // An expression: balanced brackets, nested strings skipped.
            let mut level = 0usize;
            let mut k = j;
            while k < b.len() {
                match b[k] {
                    b'{' | b'(' | b'[' => level += 1,
                    b'}' | b')' | b']' => {
                        level -= 1;
                        if level == 0 {
                            break;
                        }
                    }
                    b'"' | b'\'' => {
                        k = skip_py_string(b, k, false)?;
                        continue;
                    }
                    _ => {}
                }
                k += 1;
            }
            j = k + 1;
            continue;
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(text: &str, lang: Lang) -> String {
        strip_comments(text, lang, true).map_or_else(|| text.to_string(), |(t, _)| t)
    }

    #[test]
    fn rust_comments_go_and_strings_docs_and_headers_stay() {
        let src = "// SPDX-License-Identifier: MIT\n//! Crate docs.\nuse x; // trailing\n\n/// Item docs.\n\
                   fn f<'a>(s: &'a str) -> char {\n    // a whole line\n    let u = \"http://x // no\";\n    \
                   let r = r#\"/* raw */\"#;\n    let c = '/'; /* inline */ let d = '\"';\n    /* outer /* nested */ \
                   still */\n    a/**/b\n}\n";
        let want = "// SPDX-License-Identifier: MIT\n//! Crate docs.\nuse x;\n\n/// Item docs.\n\
                    fn f<'a>(s: &'a str) -> char {\n    let u = \"http://x // no\";\n    \
                    let r = r#\"/* raw */\"#;\n    let c = '/'; let d = '\"';\n    a b\n}\n";
        assert_eq!(strip(src, Lang::Rust), want);
    }

    #[test]
    fn script_comments_go_and_templates_regexes_and_directives_stay() {
        let src = "#!/usr/bin/env node\n// @ts-expect-error a directive\nconst a = `x ${b /* in */ + `y // ${c}`} //`; // gone\n\
                   const r = /\\/\\/[/]/g.test(s); // gone\nconst d = a / b; /** jsdoc */\n/* block\n   lines */\nreturn /re/;\n";
        let want = "#!/usr/bin/env node\n// @ts-expect-error a directive\nconst a = `x ${b + `y // ${c}`} //`;\n\
                    const r = /\\/\\/[/]/g.test(s);\nconst d = a / b; /** jsdoc */\nreturn /re/;\n";
        assert_eq!(strip(src, Lang::Script), want);
    }

    #[test]
    fn jsx_children_text_is_never_read_as_a_comment() {
        let src = "const x = (\n  <a href=\"/\">\n    // text, not a comment\n  </a>\n); // gone\n// gone too\nf();\n";
        let want = "const x = (\n  <a href=\"/\">\n    // text, not a comment\n  </a>\n);\nf();\n";
        assert_eq!(strip(src, Lang::Jsx), want);
    }

    #[test]
    fn python_comments_go_and_strings_fstrings_and_directives_stay() {
        let src = "#!/usr/bin/env python3\n# Copyright the authors, MIT licence\n\"\"\"Module doc # not a comment.\"\"\"\n\
                   import x  # noqa: F401\nx = f\"{d['#']} # no\"  # gone\n# gone\ny = '''\n# kept in a string\n'''\n";
        let want = "#!/usr/bin/env python3\n# Copyright the authors, MIT licence\n\"\"\"Module doc # not a comment.\"\"\"\n\
                    import x  # noqa: F401\nx = f\"{d['#']} # no\"\ny = '''\n# kept in a string\n'''\n";
        assert_eq!(strip(src, Lang::Python), want);
    }

    #[test]
    fn swift_comments_go_and_interpolations_stay() {
        let src = "/// Docs.\nlet s = \"a \\(f(\"// x\")) b\" // gone\nlet r = #\"/* raw */\"#\n/* a /* b */ c */\nlet m = \"\"\"\n// kept\n\"\"\"\n";
        let want = "/// Docs.\nlet s = \"a \\(f(\"// x\")) b\"\nlet r = #\"/* raw */\"#\nlet m = \"\"\"\n// kept\n\"\"\"\n";
        assert_eq!(strip(src, Lang::Swift), want);
    }

    #[test]
    fn an_unterminated_literal_leaves_the_file_alone_and_stats_count() {
        assert!(strip_comments("let s = \"open // x\n", Lang::Rust, true).is_none());
        let (_, stats) = strip_comments("a(); // one\n// two\nb();\n", Lang::Script, true).unwrap();
        assert_eq!(
            stats,
            CommentStats {
                comments: 2,
                kept: 0,
                lines: 1,
                bytes: 14
            }
        );
        let (out, _) = strip_comments("/// docs\nfn f() {}\n", Lang::Rust, false).unwrap();
        assert_eq!(out, "fn f() {}\n");
    }
}
