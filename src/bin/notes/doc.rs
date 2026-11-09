//! Markdown and the editor's text. The editor holds one block per line, its
//! Markdown markers kept as text and hidden, so that undo, selection and copy
//! work across blocks and nothing in a file is lost: a file's lines come back
//! as they were. Line breaks inside a block (a code block, a table, a soft
//! break) are U+2028, which the toolkit lays out as a break within the line.
//! The blank lines between blocks are remembered (see [`Layout`]).
// Lines hold lists of hidden ranges; one range is still a list, not a span of numbers.
#![allow(clippy::single_range_in_vec_init)]
use std::ops::Range;

/// A line break inside a block.
pub const SOFT: char = '\u{2028}';

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Para,
    H1,
    H2,
    H3,
    Bullet,
    Number,
    Todo(bool),
    Quote,
    Callout,
    Code,
    Divider,
}
impl Kind {
    pub fn is_list(self) -> bool {
        matches!(self, Kind::Bullet | Kind::Number | Kind::Todo(_))
    }
}

/// What a line of the editor is, in char offsets.
#[derive(Debug, PartialEq)]
pub struct Line {
    pub kind: Kind,
    /// Leading spaces of a list item: its nesting.
    pub spaces: usize,
    /// Where the text the reader sees starts and ends.
    pub start: usize,
    pub end: usize,
    /// Markers to hide: the prefix, a code block's fences, continuation prefixes.
    pub hidden: Vec<Range<usize>>,
}

fn fence(chars: &[char]) -> Option<(char, usize)> {
    let lead = chars.iter().take_while(|c| **c == ' ').count();
    let f = *chars.get(lead)?;
    let n = chars[lead..].iter().take_while(|c| **c == f).count();
    (lead <= 3 && (f == '`' || f == '~') && n >= 3).then_some((f, n))
}

fn is_divider(chars: &[char]) -> bool {
    let marks: Vec<_> = chars.iter().filter(|c| **c != ' ').collect();
    marks.len() >= 3
        && matches!(marks[0], '-' | '*' | '_')
        && marks.iter().all(|c| *c == marks[0])
        && !chars.contains(&SOFT)
}

/// A list marker after `spaces`: its width and the kind.
fn list_marker(chars: &[char], spaces: usize) -> Option<(usize, Kind)> {
    let rest = &chars[spaces..];
    let width = match rest.first()? {
        '-' | '*' | '+' if rest.get(1) == Some(&' ') => 2,
        c if c.is_ascii_digit() => {
            let d = rest.iter().take_while(|c| c.is_ascii_digit()).count();
            if d > 9 || !matches!(rest.get(d), Some('.' | ')')) || rest.get(d + 1) != Some(&' ') {
                return None;
            }
            return Some((d + 2, Kind::Number));
        }
        _ => return None,
    };
    let todo: String = rest.iter().skip(2).take(4).collect();
    Some(match todo.as_str() {
        "[ ] " => (6, Kind::Todo(false)),
        "[x] " | "[X] " => (6, Kind::Todo(true)),
        _ => (width, Kind::Bullet),
    })
}

fn heading(chars: &[char]) -> Option<usize> {
    let n = chars.iter().take_while(|c| **c == '#').count();
    ((1..=6).contains(&n) && chars.get(n) == Some(&' ')).then_some(n)
}

/// The leading spaces (or tabs) of a line.
fn indent(chars: &[char]) -> usize {
    chars
        .iter()
        .take_while(|c| **c == ' ' || **c == '\t')
        .count()
}

/// Classifies one line of the editor (not the title). Any block may be
/// indented: its leading spaces are its nesting, hidden, and so are the same
/// spaces at the start of each of its inner lines.
pub fn classify(text: &str) -> Line {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let spaces = indent(&chars);
    let rest = &chars[spaces..];
    let softs: Vec<usize> = (0..n).filter(|i| chars[*i] == SOFT).collect();
    // The block's own indent at the start of an inner line.
    let lead = |s: usize| {
        chars[s + 1..]
            .iter()
            .take(spaces)
            .take_while(|c| **c == ' ' || **c == '\t')
            .count()
    };
    let inner = |hidden: &mut Vec<Range<usize>>| {
        for &s in &softs {
            let l = lead(s);
            if l > 0 {
                hidden.push(s + 1..s + 1 + l);
            }
        }
    };
    let line = |kind, start: usize, end: usize, hidden: Vec<Range<usize>>| Line {
        kind,
        spaces,
        start,
        end,
        hidden: merge(hidden),
    };
    if let Some((f, k)) = fence(rest) {
        let Some(&first) = softs.first() else {
            return line(Kind::Code, n, n, vec![0..n]);
        };
        let mut hidden = vec![0..first + 1];
        inner(&mut hidden);
        let mut end = n;
        if let Some(&last) = softs.last().filter(|l| **l > first) {
            let closing: Vec<char> = chars[last + 1..]
                .iter()
                .copied()
                .filter(|c| *c != ' ' && *c != '\t')
                .collect();
            if closing.len() >= k && closing.iter().all(|c| *c == f) {
                hidden.push(last..n);
                end = last;
            }
        }
        let start = first + 1 + lead(first);
        return line(Kind::Code, start.min(end.max(first + 1)), end, hidden);
    }
    if is_divider(rest) {
        return line(Kind::Divider, n, n, vec![0..n]);
    }
    if let Some(h) = heading(rest) {
        let kind = [Kind::H1, Kind::H2, Kind::H3][(h - 1).min(2)];
        return line(kind, spaces + h + 1, n, vec![0..spaces + h + 1]);
    }
    if rest.first() == Some(&'>') {
        // `>` and the space after it, at `at`.
        let prefix = |at: usize| {
            if chars.get(at) != Some(&'>') {
                return 0;
            }
            1 + usize::from(chars.get(at + 1) == Some(&' '))
        };
        let mut hidden = vec![0..spaces + prefix(spaces)];
        let mut start = spaces + prefix(spaces);
        let mut kind = Kind::Quote;
        let first_seg: String = chars[start..softs.first().copied().unwrap_or(n)]
            .iter()
            .collect();
        let tag = first_seg.trim_end();
        if let Some(&s) = softs.first()
            && tag.starts_with("[!")
            && tag.ends_with(']')
            && tag.len() > 3
        {
            kind = Kind::Callout;
            hidden = vec![0..s + 1];
            start = s + 1;
        }
        for &s in &softs {
            let at = s + 1 + lead(s);
            let p = s + 1..at + prefix(at);
            if p.start == start {
                start = p.end;
            }
            if !p.is_empty() {
                hidden.push(p);
            }
        }
        return line(kind, start.min(n), n, hidden);
    }
    if let Some((width, kind)) = list_marker(&chars, spaces) {
        let mut hidden = vec![0..spaces + width];
        for &s in &softs {
            let lead = chars[s + 1..].iter().take_while(|c| **c == ' ').count();
            if lead > 0 {
                hidden.push(s + 1..s + 1 + lead);
            }
        }
        return line(kind, spaces + width, n, hidden);
    }
    let mut hidden = if spaces > 0 {
        vec![0..spaces]
    } else {
        Vec::new()
    };
    inner(&mut hidden);
    line(Kind::Para, spaces, n, hidden)
}

fn merge(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::new();
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// The visible text of a block, without its prefix or continuation markers.
pub fn plain(text: &str, line: &Line) -> String {
    text.chars()
        .enumerate()
        .filter(|(i, _)| {
            (line.start..line.end).contains(i) && !line.hidden.iter().any(|h| h.contains(i))
        })
        .map(|(_, c)| c)
        .collect()
}

/// A line as a reader sees it: no block or inline markers.
pub fn readable(text: &str) -> String {
    let line = classify(text);
    let chars: Vec<char> = plain(text, &line).chars().collect();
    let hidden: Vec<Range<usize>> = inline(&chars, 0, chars.len())
        .into_iter()
        .filter(|(_, m)| *m == Mark::Hide)
        .map(|(r, _)| r)
        .collect();
    (0..chars.len())
        .filter(|i| !hidden.iter().any(|r| r.contains(i)))
        .map(|i| chars[i])
        .collect()
}

/// A block of `kind` holding `plain`, indented by `spaces`.
pub fn wrap(kind: Kind, spaces: usize, plain: &str) -> String {
    let pad = " ".repeat(spaces);
    let (prefix, cont, suffix) = match kind {
        Kind::Para => (pad.clone(), pad.clone(), String::new()),
        Kind::H1 => (format!("{pad}# "), String::new(), String::new()),
        Kind::H2 => (format!("{pad}## "), String::new(), String::new()),
        Kind::H3 => (format!("{pad}### "), String::new(), String::new()),
        Kind::Bullet => (format!("{pad}- "), " ".repeat(spaces + 2), String::new()),
        Kind::Number => (format!("{pad}1. "), " ".repeat(spaces + 3), String::new()),
        Kind::Todo(done) => (
            format!("{pad}- [{}] ", if done { 'x' } else { ' ' }),
            " ".repeat(spaces + 2),
            String::new(),
        ),
        Kind::Quote => (format!("{pad}> "), format!("{pad}> "), String::new()),
        Kind::Callout => (
            format!("{pad}> [!NOTE]{SOFT}{pad}> "),
            format!("{pad}> "),
            String::new(),
        ),
        Kind::Code => (
            format!("{pad}```{SOFT}{pad}"),
            pad.clone(),
            format!("{SOFT}{pad}```"),
        ),
        Kind::Divider => return format!("{pad}---"),
    };
    let body = if matches!(kind, Kind::H1 | Kind::H2 | Kind::H3) {
        plain.replace(SOFT, " ")
    } else {
        plain.replace(SOFT, &format!("{SOFT}{cont}"))
    };
    format!("{prefix}{body}{suffix}")
}

/// What continues a block after a soft break typed inside it: CommonMark
/// wants a list item's lines indented to its text, which a checkbox is part of.
pub fn continuation(line: &Line) -> String {
    let pad = " ".repeat(line.spaces);
    match line.kind {
        Kind::Quote | Kind::Callout => format!("{pad}> "),
        Kind::Todo(_) => " ".repeat(line.start - 4),
        k if k.is_list() => " ".repeat(line.start),
        _ => pad,
    }
}

/// The width a block's children are indented to, past its own spaces: a list
/// item's text, else two spaces.
pub fn child_offset(line: &Line) -> usize {
    match line.kind {
        Kind::Todo(_) => line.start - 4,
        k if k.is_list() => line.start,
        _ => line.spaces + 2,
    }
}

/// The same block moved to `spaces` of indent, its inner lines with it.
pub fn reindent(text: &str, spaces: usize) -> String {
    let old = indent(&text.chars().collect::<Vec<_>>()) as isize;
    let delta = spaces as isize - old;
    text.split(SOFT)
        .map(|seg| {
            if delta >= 0 {
                format!("{}{seg}", " ".repeat(delta as usize))
            } else {
                let lead = seg.chars().take_while(|c| *c == ' ' || *c == '\t').count();
                seg.chars().skip(lead.min((-delta) as usize)).collect()
            }
        })
        .collect::<Vec<_>>()
        .join(&SOFT.to_string())
}

/// Splits front matter, with its fences and the blank line after it, from the body.
pub fn split_front_matter(raw: &str) -> (&str, &str) {
    if let Some(rest) = raw.strip_prefix("---\n") {
        let mut at = 4;
        for l in rest.split_inclusive('\n') {
            at += l.len();
            if matches!(l.trim_end(), "---" | "...") {
                if raw[at..].starts_with('\n') {
                    at += 1;
                }
                return (&raw[..at], &raw[at..]);
            }
        }
    }
    ("", raw)
}

/// Front matter with `key` set to `value`, or removed; the rest kept as is.
pub fn set_front(front: &str, key: &str, value: Option<&str>) -> String {
    let lines: Vec<&str> = front.split_inclusive('\n').collect();
    let Some(close) = (1..lines.len()).find(|i| matches!(lines[*i].trim_end(), "---" | "..."))
    else {
        return match value {
            Some(v) => format!("---\n{key}: {v}\n---\n\n{front}"),
            None => front.to_string(),
        };
    };
    let new = value.map(|v| format!("{key}: {v}\n"));
    let own = (1..close).find(|i| {
        lines[*i]
            .strip_prefix(key)
            .is_some_and(|r| r.starts_with(':'))
    });
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    match (own, new) {
        (Some(i), Some(n)) => out[i] = n,
        (Some(i), None) => {
            out.remove(i);
        }
        (None, Some(n)) => out.insert(close, n),
        (None, None) => {}
    }
    out.concat()
}

/// Whether a file line starts a block other than a paragraph.
fn starts_block(chars: &[char]) -> bool {
    let rest = &chars[indent(chars)..];
    fence(rest).is_some()
        || heading(rest).is_some()
        || rest.first() == Some(&'>')
        || list_marker(chars, indent(chars)).is_some()
        || (is_divider(rest) && !rest.iter().all(|c| *c == '-' || *c == ' '))
}

/// How a file laid out what the editor does not show: the blank lines between
/// blocks, the end of the file, its line endings. Saving puts them back, so a
/// file comes back byte for byte; new blocks get the usual layout.
#[derive(Default, Debug, Clone)]
pub struct Layout {
    /// Text after block `k`, where it is not the usual. The editor keeps
    /// these on its lines as they move (see `Editor::body`).
    pub gaps: std::collections::HashMap<usize, String>,
    /// The end of the file after its last block, where it is not one newline.
    tail: Option<String>,
    crlf: bool,
}

/// What usually goes between two blocks: list items of a kind sit together,
/// an empty block is itself the blank line.
fn usual_gap(prev: &str, next: &str) -> &'static str {
    let family = |t: &str| {
        let l = classify(t);
        (l.kind.is_list() && !t.is_empty()).then_some(l.kind == Kind::Number)
    };
    if prev.trim().is_empty() || (family(next).is_some() && family(prev) == family(next)) {
        "\n"
    } else {
        "\n\n"
    }
}

/// A Markdown body as editor lines, one block each, and how it was laid out.
/// Of a run of blank lines between blocks the first separates them and the
/// others become empty blocks.
pub fn to_editor(body: &str) -> (String, Layout) {
    let mut layout = Layout {
        crlf: body.contains("\r\n"),
        ..Layout::default()
    };
    let body = body.replace("\r\n", "\n");
    let (body, final_nl) = match body.strip_suffix('\n') {
        Some(b) => (b, true),
        None => (body.as_str(), false),
    };
    let lines: Vec<Vec<char>> = body.split('\n').map(|l| l.chars().collect()).collect();
    let blank = |l: &[char]| l.iter().all(|c| c.is_whitespace());
    let mut blocks: Vec<String> = Vec::new();
    // The gap after each block, as in the file.
    let mut gaps: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = &lines[i];
        if blank(l) {
            let text: String = l.iter().collect();
            match gaps.last_mut() {
                // The first blank line after a block separates it from the next.
                Some(g) if g == "\n" && !blocks.last().is_some_and(|b| b.trim().is_empty()) => {
                    g.push_str(&text);
                    g.push('\n');
                }
                _ => {
                    blocks.push(text);
                    gaps.push("\n".into());
                }
            }
            i += 1;
            continue;
        }
        let mut block = vec![l.clone()];
        i += 1;
        let rest = &l[indent(l)..];
        if let Some((f, k)) = fence(rest) {
            while i < lines.len() {
                let c = &lines[i];
                block.push(c.clone());
                i += 1;
                let r = &c[indent(c)..];
                if fence(r).is_some_and(|(g, m)| g == f && m >= k)
                    && r.iter().filter(|c| **c != ' ').all(|c| *c == f)
                {
                    break;
                }
            }
        } else if rest[0] == '>' {
            while i < lines.len() && lines[i][indent(&lines[i])..].first() == Some(&'>') {
                block.push(lines[i].clone());
                i += 1;
            }
        } else if heading(rest).is_none() && !is_divider(rest) {
            while i < lines.len() && !blank(&lines[i]) && !starts_block(&lines[i]) {
                block.push(lines[i].clone());
                i += 1;
            }
        }
        let joined: Vec<String> = block.iter().map(|l| l.iter().collect()).collect();
        blocks.push(joined.join(&SOFT.to_string()));
        gaps.push("\n".into());
    }
    if blocks.is_empty() {
        blocks.push(String::new());
        gaps.push(String::new());
    }
    for (k, gap) in gaps.iter().enumerate() {
        match blocks.get(k + 1) {
            Some(next) if gap != usual_gap(&blocks[k], next) => {
                layout.gaps.insert(k, gap.clone());
            }
            Some(_) => {}
            None => {
                // The end: what follows the last block, without the final newline.
                let mut tail = gap.clone();
                if !final_nl {
                    tail.pop();
                }
                if tail != "\n" {
                    layout.tail = Some(tail);
                }
            }
        }
    }
    (blocks.join("\n"), layout)
}

/// Editor lines (without the title) as a Markdown body, laid out as `layout` says.
pub fn from_editor<'a>(lines: impl IntoIterator<Item = &'a str>, layout: &Layout) -> String {
    let mut out = String::new();
    let mut prev: Option<&str> = None;
    for (k, text) in lines.into_iter().enumerate() {
        if let Some(p) = prev {
            out.push_str(
                layout
                    .gaps
                    .get(&(k - 1))
                    .map_or(usual_gap(p, text), |g| g.as_str()),
            );
        }
        out.push_str(&text.replace(SOFT, "\n"));
        prev = Some(text);
    }
    match &layout.tail {
        Some(tail) if prev.is_some() => out.push_str(tail),
        _ => out.push('\n'),
    }
    if layout.crlf {
        out = out.replace('\n', "\r\n");
    }
    out
}

/// The number shown for a numbered item: the first of its list's, counted on.
pub fn number_of<'a>(above: impl Iterator<Item = &'a str>, me: &Line, text: &str) -> u64 {
    // `above` runs upwards from the line before.
    let digits = |t: &str, l: &Line| -> u64 {
        t.chars()
            .skip(l.spaces)
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(1)
    };
    let mut count = 0;
    let mut start = digits(text, me);
    for t in above.take(2000) {
        if t.trim().is_empty() {
            continue;
        }
        let l = classify(t);
        if l.spaces > me.spaces {
            continue;
        }
        if l.spaces < me.spaces || l.kind != Kind::Number {
            break;
        }
        count += 1;
        start = digits(t, &l);
    }
    start + count
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mark {
    Hide,
    Bold,
    Italic,
    Code,
    /// The link text, and where its address is.
    Link(Range<usize>),
}

/// Inline formatting in `chars[from..to]`: what to hide and what to style.
pub fn inline(chars: &[char], from: usize, to: usize) -> Vec<(Range<usize>, Mark)> {
    let mut out = Vec::new();
    let mut taken = vec![false; to.max(from)];
    let mut i = from;
    while i < to {
        match chars[i] {
            '\\' => i += 1,
            '`' if !taken[i] => {
                if let Some(j) = (i + 1..to).find(|j| chars[*j] == '`')
                    && j > i + 1
                {
                    out.push((i..i + 1, Mark::Hide));
                    out.push((i + 1..j, Mark::Code));
                    out.push((j..j + 1, Mark::Hide));
                    taken[i..=j].iter_mut().for_each(|t| *t = true);
                    i = j;
                }
            }
            '[' if !taken[i] && (i == 0 || chars[i - 1] != '!') => {
                if let Some(k) = (i + 1..to).find(|k| chars[*k] == ']' && !taken[*k])
                    && chars.get(k + 1) == Some(&'(')
                    && let Some(m) = (k + 2..to).find(|m| chars[*m] == ')')
                {
                    out.push((i..i + 1, Mark::Hide));
                    out.push((i + 1..k, Mark::Link(k + 2..m)));
                    out.push((k..m + 1, Mark::Hide));
                    taken[i] = true;
                    taken[k..=m].iter_mut().for_each(|t| *t = true);
                }
            }
            _ => {}
        }
        i += 1;
    }
    for (delim, mark) in [("**", Mark::Bold), ("__", Mark::Bold)] {
        let d: Vec<char> = delim.chars().collect();
        let at = |i: usize, taken: &[bool]| {
            i + 1 < to && chars[i] == d[0] && chars[i + 1] == d[1] && !taken[i] && !taken[i + 1]
        };
        let mut i = from;
        while i < to {
            if at(i, &taken)
                && chars.get(i + 2).is_some_and(|c| !c.is_whitespace())
                && let Some(j) =
                    (i + 3..to).find(|j| at(*j, &taken) && !chars[j - 1].is_whitespace())
            {
                out.push((i..i + 2, Mark::Hide));
                out.push((i + 2..j, mark.clone()));
                out.push((j..j + 2, Mark::Hide));
                taken[i..i + 2].iter_mut().for_each(|t| *t = true);
                taken[j..j + 2].iter_mut().for_each(|t| *t = true);
                i = j + 2;
                continue;
            }
            i += 1;
        }
    }
    for d in ['*', '_'] {
        let word = |i: usize| chars.get(i).is_some_and(|c| c.is_alphanumeric());
        let open = |i: usize, taken: &[bool]| {
            chars[i] == d
                && !taken[i]
                && chars
                    .get(i + 1)
                    .is_some_and(|c| !c.is_whitespace() && *c != d)
                && (d == '*' || i == 0 || !word(i - 1))
        };
        let close = |j: usize, taken: &[bool]| {
            chars[j] == d
                && !taken[j]
                && !chars[j - 1].is_whitespace()
                && (d == '*' || !word(j + 1))
        };
        let mut i = from;
        while i < to {
            if open(i, &taken)
                && let Some(j) = (i + 2..to).find(|j| close(*j, &taken))
            {
                out.push((i..i + 1, Mark::Hide));
                out.push((i + 1..j, Mark::Italic));
                out.push((j..j + 1, Mark::Hide));
                taken[i] = true;
                taken[j] = true;
                i = j + 1;
                continue;
            }
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(md: &str) -> String {
        let (text, layout) = to_editor(md);
        from_editor(text.split('\n'), &layout)
    }

    #[test]
    fn canonical_files_come_back_byte_for_byte() {
        let md = "# Title\n\nSome *text* with **bold**, `code` and [a link](https://x.org).\nA soft second line.\n\n- one\n- two\n  - nested\n    continued\n- [ ] task\n- [x] done\n\n1. first\n2. second\n\n> a quote\n> on two lines\n\n> [!NOTE]\n> A callout.\n\n```rust\nfn main() {\n\n    println!(\"hi\");\n}\n```\n\n---\n\n\nAfter an empty block.\n";
        assert_eq!(round(md), md);
    }

    #[test]
    fn unknown_markdown_is_kept() {
        let md = "Intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n<details>\n<summary>x</summary>\n</details>\n\n[^1]: A footnote.\n\n![image](pic.png)\n\nSetext heading\n==============\n\nAnother\n---\n\n    indented code\n\n~~~\nraw ~~~ fence\n~~~\n\n* star\n+ plus\n3) paren\n\n#### Deep heading\n\n$$\nx^2\n$$\n";
        assert_eq!(round(md), md);
    }

    #[test]
    fn any_layout_comes_back_byte_for_byte() {
        for md in [
            "",
            "\n",
            "x",
            "x\n\n",
            "\n\n\nx\n",
            "# Tight\npara right under\n## Next\n",
            "a\n\n\n\n\nb\n- x\n\n- y\n\n\n",
            "- loose\n\n- list\n\n1. a\n3. b\n1. c\n",
            "line\n   \n  \t\nwhite space lines\n",
            "x\r\ny\r\n\r\n- z\r\n",
            "no final newline\n\n- item",
            "- a\n\n  nested paragraph\n\n  > nested quote\n\n  ```\n  code in item\n  ```\n",
        ] {
            assert_eq!(round(md), md, "{md:?}");
        }
    }

    #[test]
    fn a_new_page_types_into_its_one_block() {
        let (text, layout) = to_editor("\n");
        assert_eq!(text, "");
        assert_eq!(from_editor(["x"], &layout), "x\n");
        let raw = "---\ncreated: 2026-11-07\n---\n\n\n";
        let (front, body) = split_front_matter(raw);
        let (_, layout) = to_editor(body);
        assert_eq!(
            format!("{front}{}", from_editor(["x"], &layout)),
            "---\ncreated: 2026-11-07\n---\n\nx\n"
        );
    }

    #[test]
    fn new_blocks_get_the_usual_layout() {
        let (text, layout) = to_editor("# A\nb\n");
        let edited = text.replace('b', "c");
        assert_eq!(from_editor(edited.split('\n'), &layout), "# A\nc\n");
        let added = format!("{text}\n- d");
        assert_eq!(from_editor(added.split('\n'), &layout), "# A\nb\n\n- d\n");
        assert_eq!(
            from_editor(["- a", "- b", "", "c"], &Layout::default()),
            "- a\n- b\n\n\nc\n"
        );
    }

    #[test]
    fn any_block_nests() {
        for (text, kind) in [
            ("  para", Kind::Para),
            ("  # Heading", Kind::H1),
            ("  > quote", Kind::Quote),
            ("  ---", Kind::Divider),
        ] {
            let l = classify(text);
            assert_eq!((l.kind, l.spaces), (kind, 2), "{text}");
            assert_eq!(wrap(kind, 2, &plain(text, &l)), text);
        }
        let code = format!("  ```{SOFT}  let a;{SOFT}      b;{SOFT}  ```");
        let l = classify(&code);
        assert_eq!((l.kind, l.spaces), (Kind::Code, 2));
        assert_eq!(plain(&code, &l), format!("let a;{SOFT}    b;"));
        assert_eq!(
            reindent(&code, 0),
            format!("```{SOFT}let a;{SOFT}    b;{SOFT}```")
        );
        assert_eq!(reindent("- a", 4), "    - a");
        assert_eq!(child_offset(&classify("1. a")), 3);
    }

    #[test]
    fn front_matter_is_split_and_kept() {
        let raw = "---\ncreated: 2026-11-07\ntags: [x]\n---\n\n# Hi\n";
        let (fm, body) = split_front_matter(raw);
        assert_eq!(fm, "---\ncreated: 2026-11-07\ntags: [x]\n---\n\n");
        assert_eq!(format!("{fm}{}", round(body)), raw);
        let tight = "---\nicon: star\n---\n# Hi\n";
        let (fm, body) = split_front_matter(tight);
        assert_eq!(format!("{fm}{}", round(body)), tight);
        assert_eq!(split_front_matter("--- no\n"), ("", "--- no\n"));
    }

    #[test]
    fn front_matter_keys_change_alone() {
        let fm = "---\ncreated: 2026-11-07\ntags: [x]\n---\n\n";
        let set = set_front(fm, "icon", Some("star"));
        assert_eq!(
            set,
            "---\ncreated: 2026-11-07\ntags: [x]\nicon: star\n---\n\n"
        );
        assert_eq!(
            set_front(&set, "icon", Some("pin"))
                .matches("icon: pin")
                .count(),
            1
        );
        assert_eq!(set_front(&set, "icon", None), fm);
        assert_eq!(
            set_front("", "icon", Some("star")),
            "---\nicon: star\n---\n\n"
        );
        assert_eq!(set_front("", "icon", None), "");
    }

    #[test]
    fn lines_classify_with_hidden_markers() {
        let l = classify("## Hi");
        assert_eq!(
            (l.kind, l.start, l.hidden.clone()),
            (Kind::H2, 3, vec![0..3])
        );
        let l = classify("    - [x] done");
        assert_eq!((l.kind, l.spaces, l.start), (Kind::Todo(true), 4, 10));
        let code = format!("```rs{SOFT}let a;{SOFT}```");
        let l = classify(&code);
        assert_eq!((l.kind, l.start, l.end), (Kind::Code, 6, 12));
        let callout = format!("> [!NOTE]{SOFT}> Hello{SOFT}> again");
        let l = classify(&callout);
        assert_eq!(l.kind, Kind::Callout);
        assert_eq!(plain(&callout, &l), format!("Hello{SOFT}again"));
        assert_eq!(wrap(Kind::Callout, 0, &plain(&callout, &l)), callout);
        assert_eq!(classify("**bold** text").kind, Kind::Para);
        assert_eq!(classify("---").kind, Kind::Divider);
        assert_eq!(classify("#tag").kind, Kind::Para);
        assert_eq!(readable("- a **b** [c](d)"), "a b c");
    }

    #[test]
    fn inline_marks() {
        let s: Vec<char> = "a **b** *c* `d*` [e](f) snake_case_name".chars().collect();
        let marks = inline(&s, 0, s.len());
        let styled: Vec<_> = marks
            .iter()
            .filter(|(_, m)| *m != Mark::Hide)
            .map(|(r, m)| (s[r.clone()].iter().collect::<String>(), m.clone()))
            .collect();
        assert_eq!(
            styled,
            [
                ("d*".into(), Mark::Code),
                ("e".into(), Mark::Link(21..22)),
                ("b".into(), Mark::Bold),
                ("c".into(), Mark::Italic),
            ]
        );
    }
}
