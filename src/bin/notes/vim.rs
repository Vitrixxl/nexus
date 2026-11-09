//! Vim keys for the page editor, off by default: a small engine over a
//! [`Buf`], tested on plain lines. The editor's lines are blocks whose
//! Markdown markers are hidden; motions step over what is hidden, as the
//! arrows do, and the rows of a block (its line breaks) are lines to j, k,
//! 0, ^ and $. Linewise commands (dd, yy, cc, p, V, o) take whole blocks.
use crate::doc::{self, Kind, Mark, SOFT};
use std::ops::Range;

pub const ESC: char = '\x1b';
pub const CTRL_R: char = '\x12';

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}
pub fn pos(line: usize, col: usize) -> Pos {
    Pos { line, col }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Insert,
    Visual,
    VisualLine,
}

/// A line as the engine sees it, in char offsets.
pub struct VLine {
    pub chars: Vec<char>,
    /// Where the cursor may rest: text the reader sees, not a marker.
    pub shown: Vec<bool>,
    /// Where the block's text starts and ends.
    pub start: usize,
    pub end: usize,
    /// Inline markers that go in pairs: deleting one alone would break the other.
    pub pairs: Vec<(Range<usize>, Range<usize>)>,
}

impl VLine {
    pub fn plain(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        Self {
            shown: chars.iter().map(|c| *c != SOFT).collect(),
            start: 0,
            end: chars.len(),
            chars,
            pairs: Vec::new(),
        }
    }
    /// A block of the editor, its markers hidden as the editor hides them.
    pub fn block(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let line = doc::classify(text);
        let mut hidden = line.hidden.clone();
        let mut pairs = Vec::new();
        if !matches!(line.kind, Kind::Code | Kind::Divider) {
            // Each style comes as three: its opening marker, its text, its closing one.
            let marks = doc::inline(&chars, line.start, line.end);
            for (i, (r, m)) in marks.iter().enumerate() {
                if *m == Mark::Hide {
                    hidden.push(r.clone());
                } else {
                    pairs.push((marks[i - 1].0.clone(), marks[i + 1].0.clone()));
                }
            }
        }
        let shown = (0..chars.len())
            .map(|i| {
                (line.start..line.end).contains(&i)
                    && chars[i] != SOFT
                    && !hidden.iter().any(|h| h.contains(&i))
            })
            .collect();
        Self {
            chars,
            shown,
            start: line.start,
            end: line.end,
            pairs,
        }
    }
    fn rows(&self) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut s = self.start;
        for i in self.start..self.end {
            if self.chars[i] == SOFT {
                out.push(s..i);
                s = i + 1;
            }
        }
        out.push(s..self.end.max(s));
        out
    }
    pub fn row(&self, col: usize) -> Range<usize> {
        let rows = self.rows();
        let last = rows.last().cloned().unwrap_or_default();
        rows.into_iter().find(|r| col <= r.end).unwrap_or(last)
    }
    fn stops(&self, r: &Range<usize>) -> Vec<usize> {
        r.clone().filter(|i| self.shown[*i]).collect()
    }
    pub fn home(&self, r: &Range<usize>) -> usize {
        self.stops(r).first().copied().unwrap_or(r.end)
    }
    fn first_nonblank(&self, r: &Range<usize>) -> usize {
        let s = self.stops(r);
        s.iter()
            .copied()
            .find(|i| !self.chars[*i].is_whitespace())
            .or(s.last().copied())
            .unwrap_or(r.end)
    }
}

/// What the engine reads and edits: the editor's buffer, or plain lines.
pub trait Buf {
    fn lines(&self) -> usize;
    fn line(&self, l: usize) -> VLine;
    /// Lines before this one are not blocks (the title): linewise commands skip them.
    fn first(&self) -> usize {
        0
    }
    fn cursor(&self) -> Pos;
    fn set_cursor(&mut self, p: Pos);
    /// Shows the selection from `a` to `b`, both included; whole lines with `lines`.
    fn select(&mut self, a: Pos, b: Pos, lines: bool);
    /// Replaces the text from `from` to `to` (exclusive); `\n` in `text` starts lines.
    fn replace(&mut self, from: Pos, to: Pos, text: &str);
    /// Opens an empty block (or row of a block) below or above the cursor, and goes there.
    fn open(&mut self, below: bool);
    /// Nests lines `lo..=hi` one level deeper, or lifts them.
    fn nest(&mut self, lo: usize, hi: usize, deeper: bool);
    fn undo(&mut self);
    fn redo(&mut self);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cls {
    Blank,
    Word,
    Punct,
    /// A row with nothing in it: a stop for w, b and e.
    Empty,
    /// The end of a row.
    Break,
}
fn cls(c: char, big: bool) -> Cls {
    if c.is_whitespace() {
        Cls::Blank
    } else if big || c.is_alphanumeric() || c == '_' {
        Cls::Word
    } else {
        Cls::Punct
    }
}
fn items(b: &impl Buf, l: usize, big: bool) -> Vec<(Pos, Cls)> {
    let v = b.line(l);
    let mut out = Vec::new();
    for r in v.rows() {
        let stops = v.stops(&r);
        if stops.is_empty() {
            out.push((pos(l, r.end), Cls::Empty));
            continue;
        }
        out.extend(stops.iter().map(|&i| (pos(l, i), cls(v.chars[i], big))));
        out.push((pos(l, r.end), Cls::Break));
    }
    out
}
/// The first item after `p` that `f` takes.
fn scan_fwd(
    b: &impl Buf,
    p: Pos,
    big: bool,
    mut f: impl FnMut(Pos, Cls) -> bool,
) -> Option<(Pos, Cls)> {
    (p.line..b.lines()).find_map(|l| {
        items(b, l, big)
            .into_iter()
            .find(|(q, c)| *q > p && f(*q, *c))
    })
}
fn scan_back(
    b: &impl Buf,
    p: Pos,
    big: bool,
    mut f: impl FnMut(Pos, Cls) -> bool,
) -> Option<(Pos, Cls)> {
    (0..=p.line.min(b.lines().saturating_sub(1)))
        .rev()
        .find_map(|l| {
            items(b, l, big)
                .into_iter()
                .rev()
                .find(|(q, c)| *q < p && f(*q, *c))
        })
}
fn class_at(b: &impl Buf, p: Pos, big: bool) -> Cls {
    items(b, p.line, big)
        .into_iter()
        .find(|(q, _)| *q == p)
        .map_or(Cls::Empty, |(_, c)| c)
}
fn word_fwd(b: &impl Buf, p: Pos, big: bool) -> Option<Pos> {
    let c0 = class_at(b, p, big);
    let mut left = !matches!(c0, Cls::Word | Cls::Punct);
    scan_fwd(b, p, big, |_, c| match c {
        Cls::Blank | Cls::Break => {
            left = true;
            false
        }
        Cls::Empty => true,
        c => left || c != c0,
    })
    .map(|x| x.0)
}
fn word_end(b: &impl Buf, p: Pos, big: bool) -> Option<Pos> {
    let (q, c) = scan_fwd(b, p, big, |_, c| matches!(c, Cls::Word | Cls::Punct))?;
    let mut last = q;
    scan_fwd(b, q, big, |r, d| {
        if d == c {
            last = r;
            false
        } else {
            true
        }
    });
    Some(last)
}
fn word_back(b: &impl Buf, p: Pos, big: bool) -> Option<Pos> {
    let (q, c) = scan_back(b, p, big, |_, c| !matches!(c, Cls::Blank | Cls::Break))?;
    if c == Cls::Empty {
        return Some(q);
    }
    let mut first = q;
    scan_back(b, q, big, |r, d| {
        if d == c {
            first = r;
            false
        } else {
            true
        }
    });
    Some(first)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum How {
    Excl,
    Incl,
    Lines,
}

#[derive(Default)]
struct Reg {
    text: String,
    lines: bool,
}

pub struct Vim {
    pub mode: Mode,
    keys: Vec<char>,
    reg: Reg,
    /// The visual selection's fixed end, and its moving one.
    anchor: Pos,
    head: Pos,
    /// The column j and k keep, in stops from the row's start.
    want: Option<usize>,
    find: Option<(char, char)>,
    search: Option<(String, bool)>,
    /// The last change, for `.`: its keys and the text it typed.
    last: Vec<char>,
    last_text: String,
    insert_at: Option<Pos>,
    /// Set by `/` and `?`: the editor asks for a pattern, forward or not.
    pub prompt: Option<bool>,
}

enum Parse {
    More,
    Bad,
    Done {
        n: Option<usize>,
        op: Option<char>,
        m: Vec<char>,
    },
}

const MOTIONS: &str = "hjklwbeWBE0^$G;,nN";
const ACTIONS: &str = "xXpPuiaIAoOJ~DCSsvV./?\x12\x1b";

fn parse(keys: &[char], visual: bool) -> Parse {
    let num = |i: &mut usize| {
        let s = *i;
        while *i < keys.len() && keys[*i].is_ascii_digit() && !(*i == s && keys[*i] == '0') {
            *i += 1;
        }
        (*i > s).then(|| {
            keys[s..*i]
                .iter()
                .collect::<String>()
                .parse::<usize>()
                .unwrap_or(1)
                .min(9999)
        })
    };
    let mut i = 0;
    let n1 = num(&mut i);
    let Some(&first) = keys.get(i) else {
        return Parse::More;
    };
    let (op, n2) = if !visual && "dcy<>".contains(first) {
        i += 1;
        (Some(first), num(&mut i))
    } else {
        (None, None)
    };
    let rest = &keys[i..];
    let Some(&k) = rest.first() else {
        return Parse::More;
    };
    let need = match k {
        'g' | 'f' | 't' | 'F' | 'T' => 2,
        'r' if op.is_none() && !visual => 2,
        'i' | 'a' if op.is_some() || visual => 2,
        c if Some(c) == op => 1,
        _ if matches!(op, Some('<' | '>')) => return Parse::Bad,
        c if MOTIONS.contains(c) => 1,
        c if visual && "dcyxoJ~<>".contains(c) => 1,
        c if op.is_none() && ACTIONS.contains(c) => 1,
        _ => return Parse::Bad,
    };
    if rest.len() < need {
        return Parse::More;
    }
    let bad = rest.len() > need
        || (k == 'g' && rest[1] != 'g')
        || (need == 2 && matches!(k, 'i' | 'a') && !matches!(rest[1], 'w' | 'W'));
    if bad {
        return Parse::Bad;
    }
    let n = match (n1, n2) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(1) * b.unwrap_or(1)),
    };
    Parse::Done {
        n,
        op,
        m: rest.to_vec(),
    }
}

impl Default for Vim {
    fn default() -> Self {
        Self::new()
    }
}

impl Vim {
    pub fn new() -> Self {
        Self {
            mode: Mode::Normal,
            keys: Vec::new(),
            reg: Reg::default(),
            anchor: Pos::default(),
            head: Pos::default(),
            want: None,
            find: None,
            search: None,
            last: Vec::new(),
            last_text: String::new(),
            insert_at: None,
            prompt: None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self.mode {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "VISUAL LINE",
        }
    }
    /// What has been typed of a command not complete yet.
    pub fn pending(&self) -> String {
        self.keys.iter().filter(|c| !c.is_control()).collect()
    }
    /// Leaves visual mode and any half-typed command, for a page (re)loaded;
    /// `insert` for one about to be typed in.
    pub fn reset(&mut self, insert: bool) {
        self.mode = if insert || self.mode == Mode::Insert {
            Mode::Insert
        } else {
            Mode::Normal
        };
        self.keys.clear();
        self.insert_at = None;
        self.prompt = None;
    }

    /// A key; false when the editor should handle it (typing in insert mode).
    pub fn key(&mut self, b: &mut impl Buf, k: char) -> bool {
        if self.mode == Mode::Insert {
            if k == ESC {
                self.leave_insert(b);
                return true;
            }
            return false;
        }
        self.keys.push(k);
        let keys = self.keys.clone();
        match parse(&keys, self.mode != Mode::Normal) {
            Parse::More => {}
            Parse::Bad => self.keys.clear(),
            Parse::Done { n, op, m } => {
                self.keys.clear();
                self.exec(b, n, op, &m, keys);
            }
        }
        true
    }

    /// Looks for `pattern` from the cursor, and remembers it for n and N.
    pub fn search(&mut self, b: &mut impl Buf, pattern: &str, forward: bool) {
        if !pattern.is_empty() {
            self.search = Some((pattern.to_string(), forward));
        }
        self.exec(b, None, None, &['n'], Vec::new());
    }

    fn leave_insert(&mut self, b: &mut impl Buf) {
        self.mode = Mode::Normal;
        let c = b.cursor();
        let v = b.line(c.line);
        if let Some(s) = self.insert_at.take()
            && s.line == c.line
            && s.col <= c.col
        {
            self.last_text = v.chars[s.col..c.col.min(v.chars.len())].iter().collect();
        }
        let r = v.row(c.col);
        let stops = v.stops(&r);
        let to = stops
            .iter()
            .rev()
            .find(|i| **i < c.col)
            .or(stops.first())
            .copied()
            .unwrap_or(r.end);
        b.set_cursor(pos(c.line, to));
    }

    fn insert(&mut self, b: &mut impl Buf, at: Pos) {
        b.set_cursor(at);
        self.mode = Mode::Insert;
        self.insert_at = Some(at);
    }

    /// The cursor where normal mode may have it: on a stop of its row.
    fn settle(b: &impl Buf, p: Pos, fwd: bool) -> Pos {
        let l = p.line.min(b.lines().saturating_sub(1));
        let v = b.line(l);
        let r = v.row(p.col);
        let s = v.stops(&r);
        if s.contains(&p.col) {
            return pos(l, p.col);
        }
        let pick = if fwd {
            s.iter().find(|i| **i >= p.col).or(s.last())
        } else {
            s.iter().rev().find(|i| **i <= p.col).or(s.first())
        };
        pos(l, pick.copied().unwrap_or(r.end))
    }

    /// Steps `d` rows, across lines; returns the line and the row.
    fn rows(b: &impl Buf, p: Pos, d: isize) -> (usize, Range<usize>) {
        let mut l = p.line;
        let mut rows = b.line(l).rows();
        let mut i = rows.iter().position(|r| p.col <= r.end).unwrap_or(0);
        for _ in 0..d.unsigned_abs() {
            if d > 0 {
                if i + 1 < rows.len() {
                    i += 1;
                } else if l + 1 < b.lines() {
                    l += 1;
                    rows = b.line(l).rows();
                    i = 0;
                } else {
                    break;
                }
            } else if i > 0 {
                i -= 1;
            } else if l > 0 {
                l -= 1;
                rows = b.line(l).rows();
                i = rows.len() - 1;
            } else {
                break;
            }
        }
        (l, rows[i].clone())
    }

    fn end_of(b: &impl Buf) -> Pos {
        let l = b.lines().saturating_sub(1);
        let v = b.line(l);
        pos(l, v.rows().last().map_or(v.end, |r| r.end))
    }

    /// Where motion `m` goes from `p`, and how an operator takes it.
    fn motion(
        &mut self,
        b: &impl Buf,
        p: Pos,
        m: &[char],
        n: usize,
        counted: bool,
        op: bool,
    ) -> Option<(Pos, How)> {
        let v = b.line(p.line);
        let row = v.row(p.col);
        let stops = v.stops(&row);
        let idx = stops
            .iter()
            .position(|s| *s >= p.col)
            .unwrap_or(stops.len());
        let at = |i: usize| pos(p.line, stops.get(i).copied().unwrap_or(row.end));
        if !matches!(m[0], 'j' | 'k') {
            self.want = None;
        }
        Some(match m {
            ['h'] => (at(idx.saturating_sub(n)), How::Excl),
            ['l'] if op && idx + n >= stops.len() => (
                pos(p.line, stops.last().map_or(p.col, |s| s + 1)),
                How::Excl,
            ),
            ['l'] => (at((idx + n).min(stops.len().saturating_sub(1))), How::Excl),
            ['0'] => (pos(p.line, v.home(&row)), How::Excl),
            ['^'] => (pos(p.line, v.first_nonblank(&row)), How::Excl),
            ['$'] => {
                let (l, r) = Self::rows(b, p, n as isize - 1);
                let v = b.line(l);
                let s = v.stops(&r);
                (pos(l, s.last().copied().unwrap_or(r.end)), How::Incl)
            }
            ['j' | 'k'] => {
                let want = *self.want.get_or_insert(idx);
                let d = if m[0] == 'j' {
                    n as isize
                } else {
                    -(n as isize)
                };
                let (l, r) = Self::rows(b, p, d);
                let v = b.line(l);
                let s = v.stops(&r);
                let col = s
                    .get(want.min(s.len().saturating_sub(1)))
                    .copied()
                    .unwrap_or(r.end);
                (pos(l, col), How::Lines)
            }
            ['g', 'g'] | ['G'] => {
                let last = b.lines().saturating_sub(1);
                let l = match (counted, m[0]) {
                    (true, _) => (n - 1).min(last),
                    (false, 'g') => 0,
                    _ => last,
                };
                let v = b.line(l);
                let r = v.rows()[0].clone();
                (pos(l, v.first_nonblank(&r)), How::Lines)
            }
            ['w' | 'W' | 'b' | 'B' | 'e' | 'E'] => {
                let big = m[0].is_uppercase();
                let mut t = p;
                for _ in 0..n {
                    let next = match m[0].to_ascii_lowercase() {
                        'w' => word_fwd(b, t, big),
                        'b' => word_back(b, t, big),
                        _ => word_end(b, t, big),
                    };
                    match next {
                        Some(q) => t = q,
                        // Past the last word: the end of the text.
                        None if op && m[0].eq_ignore_ascii_case(&'w') => {
                            t = Self::end_of(b);
                            break;
                        }
                        None => break,
                    }
                }
                let how = if m[0].eq_ignore_ascii_case(&'e') {
                    How::Incl
                } else {
                    How::Excl
                };
                let w = m[0].eq_ignore_ascii_case(&'w');
                if op && w && (t.line != p.line || b.line(t.line).row(t.col) != row) {
                    // The last word of a row: up to its end, not across the break.
                    if let Some((q, _)) =
                        scan_back(b, t, big, |_, c| matches!(c, Cls::Word | Cls::Punct))
                        && q >= p
                    {
                        t = pos(q.line, q.col + 1);
                    }
                }
                (t, how)
            }
            [f @ ('f' | 't' | 'F' | 'T'), c] => {
                self.find = Some((*f, *c));
                Self::find_in(&v, &stops, p, *f, *c, n, false)?
            }
            [';' | ','] => {
                let (f, c) = self.find?;
                let f = if m[0] == ',' {
                    match f {
                        'f' => 'F',
                        'F' => 'f',
                        't' => 'T',
                        _ => 't',
                    }
                } else {
                    f
                };
                Self::find_in(&v, &stops, p, f, c, n, true)?
            }
            ['n' | 'N'] => {
                let (pat, fwd) = self.search.clone()?;
                let fwd = fwd == (m[0] == 'n');
                let mut t = p;
                for _ in 0..n {
                    t = find(b, t, &pat, fwd)?;
                }
                (t, How::Excl)
            }
            _ => return None,
        })
    }

    /// f, t, F or T in the row; `again` (for ; and ,) steps past the
    /// letter that t or T stands next to.
    #[allow(clippy::too_many_arguments)]
    fn find_in(
        v: &VLine,
        stops: &[usize],
        p: Pos,
        f: char,
        c: char,
        n: usize,
        again: bool,
    ) -> Option<(Pos, How)> {
        let i = stops.iter().position(|s| *s == p.col)?;
        let hits = stops.iter().enumerate().filter(|(_, s)| v.chars[**s] == c);
        let k = if matches!(f, 'f' | 't') {
            let from = i + usize::from(again && f == 't');
            let k = hits.filter(|(k, _)| *k > from).nth(n - 1)?.0;
            if f == 't' { k - 1 } else { k }
        } else {
            let from = i.saturating_sub(usize::from(again && f == 'T'));
            let k = hits.rev().filter(|(k, _)| *k < from).nth(n - 1)?.0;
            if f == 'T' { k + 1 } else { k }
        };
        let how = if matches!(f, 'f' | 't') {
            How::Incl
        } else {
            How::Excl
        };
        Some((pos(p.line, stops[k]), how))
    }

    /// The text from `a` to `z` (exclusive) as it goes, and the markers that
    /// stay because their other half does.
    fn take(b: &impl Buf, a: Pos, z: Pos) -> (String, String) {
        let (mut gone, mut kept) = (String::new(), String::new());
        for l in a.line..=z.line.min(b.lines().saturating_sub(1)) {
            let v = b.line(l);
            let s = if l == a.line { a.col } else { 0 };
            let e = if l == z.line { z.col } else { v.chars.len() };
            let e = e.min(v.chars.len());
            if l > a.line {
                gone.push('\n');
            }
            for i in s..e.max(s) {
                let lone = !v.shown[i]
                    && v.pairs.iter().any(|(o, c)| {
                        (o.contains(&i) || c.contains(&i)) && !(o.start >= s && c.end <= e)
                    });
                if lone {
                    kept.push(v.chars[i]);
                } else {
                    gone.push(v.chars[i]);
                }
            }
        }
        (gone, kept)
    }

    fn operate(&mut self, b: &mut impl Buf, op: char, p: Pos, t: Pos, how: How) {
        if how == How::Lines || matches!(op, '<' | '>') {
            let (lo, hi) = (p.line.min(t.line), p.line.max(t.line));
            return self.lines(b, op, lo, hi);
        }
        let (a, z) = (p.min(t), p.max(t));
        // An inclusive end takes its letter; an empty row has none.
        let z = if how == How::Incl && b.line(z.line).shown.get(z.col) == Some(&true) {
            pos(z.line, z.col + 1)
        } else {
            z
        };
        // A style whose whole text goes takes its markers along; a change
        // keeps them, to type in the style.
        let (mut a, mut z) = (a, z);
        for l in [a.line, z.line].into_iter().filter(|_| op != 'c') {
            let v = b.line(l);
            let s = if l == a.line { a.col } else { 0 };
            let e = if l == z.line { z.col } else { v.chars.len() };
            for (o, c) in &v.pairs {
                if o.end >= s && c.start <= e && o.end < c.start {
                    if l == a.line {
                        a.col = a.col.min(o.start);
                    }
                    if l == z.line {
                        z.col = z.col.max(c.end);
                    }
                }
            }
        }
        if a == z {
            if op == 'c' {
                self.insert(b, a);
            }
            return;
        }
        let (gone, kept) = Self::take(b, a, z);
        self.reg = Reg {
            text: gone,
            lines: false,
        };
        match op {
            'y' => b.set_cursor(a),
            'd' => {
                b.replace(a, z, &kept);
                b.set_cursor(Self::settle(b, a, true));
            }
            _ => {
                b.replace(a, z, &kept);
                self.insert(b, a);
            }
        }
    }

    fn lines(&mut self, b: &mut impl Buf, op: char, lo: usize, hi: usize) {
        let lo = lo.max(b.first());
        let n = b.lines();
        let hi = hi.min(n.saturating_sub(1));
        if hi < lo {
            return;
        }
        if matches!(op, '<' | '>') {
            b.nest(lo, hi, op == '>');
            let v = b.line(lo);
            b.set_cursor(pos(lo, v.first_nonblank(&v.rows()[0])));
            return;
        }
        let text: Vec<String> = (lo..=hi)
            .map(|l| b.line(l).chars.iter().collect())
            .collect();
        self.reg = Reg {
            text: text.join("\n"),
            lines: true,
        };
        let len = |l: usize| b.line(l).chars.len();
        match op {
            'y' => {}
            'd' => {
                if hi + 1 < n {
                    b.replace(pos(lo, 0), pos(hi + 1, 0), "");
                } else if lo > b.first() {
                    b.replace(pos(lo - 1, len(lo - 1)), pos(hi, len(hi)), "");
                } else {
                    b.replace(pos(lo, 0), pos(hi, len(hi)), "");
                }
            }
            _ => {
                let v = b.line(lo);
                let end = if hi == lo { v.end } else { len(hi) };
                b.replace(pos(lo, v.start), pos(hi, end), "");
                let at = pos(lo, b.line(lo).start);
                return self.insert(b, at);
            }
        }
        let l = lo.min(b.lines().saturating_sub(1));
        let v = b.line(l);
        b.set_cursor(pos(l, v.first_nonblank(&v.rows()[0])));
    }

    fn put(&mut self, b: &mut impl Buf, p: Pos, before: bool, n: usize) {
        if self.reg.text.is_empty() && !self.reg.lines {
            return;
        }
        if self.reg.lines {
            let text = vec![self.reg.text.as_str(); n].join("\n");
            let l = if before {
                p.line.max(b.first())
            } else {
                p.line
            };
            let at = if before {
                b.replace(pos(l, 0), pos(l, 0), &format!("{text}\n"));
                l
            } else {
                let len = b.line(l).chars.len();
                b.replace(pos(l, len), pos(l, len), &format!("\n{text}"));
                l + 1
            };
            let v = b.line(at);
            return b.set_cursor(pos(at, v.first_nonblank(&v.rows()[0])));
        }
        let text = self.reg.text.repeat(n);
        let v = b.line(p.line);
        let has = v.shown.get(p.col).copied().unwrap_or(false);
        let at = pos(p.line, if before || !has { p.col } else { p.col + 1 });
        b.replace(at, at, &text);
        let end = if text.contains('\n') {
            at
        } else {
            pos(at.line, at.col + text.chars().count() - 1)
        };
        b.set_cursor(Self::settle(b, end, false));
    }

    fn exec(
        &mut self,
        b: &mut impl Buf,
        n: Option<usize>,
        op: Option<char>,
        m: &[char],
        keys: Vec<char>,
    ) {
        let count = n.unwrap_or(1);
        let visual = self.mode != Mode::Normal;
        let p = if visual {
            self.head
        } else {
            Self::settle(b, b.cursor(), false)
        };
        if let Some(op) = op {
            let target = if m == [op] {
                Some((pos(p.line + count - 1, p.col), How::Lines))
            } else if matches!(m[0], 'i' | 'a') {
                if let Some((a, z)) = self.object(b, p, m[0] == 'i', m[1] == 'W') {
                    self.operate(b, op, a, z, How::Incl);
                }
                if op != 'y' {
                    self.last = keys;
                }
                return;
            } else if op == 'c'
                && matches!(m[0], 'w' | 'W')
                && matches!(class_at(b, p, m[0] == 'W'), Cls::Word | Cls::Punct)
            {
                // cw changes to the end of the word, as ce does; on the last
                // letter of a word, just that letter.
                let big = m[0] == 'W';
                let c0 = class_at(b, p, big);
                let at_end = scan_fwd(b, p, big, |_, _| true).is_none_or(|(_, c)| c != c0);
                let k = count - usize::from(at_end);
                let e = if big { 'E' } else { 'e' };
                let t = if k == 0 {
                    p
                } else {
                    self.motion(b, p, &[e], k, true, true).map_or(p, |x| x.0)
                };
                Some((t, How::Incl))
            } else {
                self.motion(b, p, m, count, n.is_some(), true)
            };
            if let Some((t, how)) = target {
                self.operate(b, op, p, t, how);
            }
            if op != 'y' {
                self.last = keys;
            }
            return;
        }
        if visual && matches!(m[0], 'i' | 'a') {
            if let Some((a, z)) = self.object(b, p, m[0] == 'i', m[1] == 'W') {
                (self.anchor, self.head) = (a.min(self.anchor), z);
                b.select(self.anchor, self.head, self.mode == Mode::VisualLine);
            }
            return;
        }
        if visual && self.visual(b, m[0]) {
            return;
        }
        if let Some((t, how)) = self.motion(b, p, m, count, n.is_some(), false) {
            let t = if how == How::Lines {
                t
            } else {
                Self::settle(b, t, false)
            };
            if visual {
                self.head = t;
                b.select(self.anchor, t, self.mode == Mode::VisualLine);
            } else {
                b.set_cursor(t);
            }
            return;
        }
        let v = b.line(p.line);
        let row = v.row(p.col);
        let stops = v.stops(&row);
        let idx = stops
            .iter()
            .position(|s| *s >= p.col)
            .unwrap_or(stops.len());
        let change = "xXpPJr~DCSsoOiaIA<>".contains(m[0]);
        match m[0] {
            'x' | 'X' if !stops.is_empty() => {
                let (a, z) = if m[0] == 'x' {
                    (
                        p,
                        pos(p.line, stops[(idx + count - 1).min(stops.len() - 1)] + 1),
                    )
                } else if idx > 0 {
                    (pos(p.line, stops[idx.saturating_sub(count)]), p)
                } else {
                    return;
                };
                self.operate(b, 'd', a, z, How::Excl);
            }
            'D' | 'C' => {
                let t = self
                    .motion(b, p, &['$'], count, false, true)
                    .map_or(p, |x| x.0);
                self.operate(b, if m[0] == 'D' { 'd' } else { 'c' }, p, t, How::Incl);
            }
            's' => {
                let z = stops
                    .get(idx + count - 1)
                    .or(stops.last())
                    .copied()
                    .unwrap_or(p.col);
                self.operate(b, 'c', p, pos(p.line, z), How::Incl);
            }
            'S' => self.lines(b, 'c', p.line, p.line + count - 1),
            'p' | 'P' => self.put(b, p, m[0] == 'P', count),
            'J' => {
                for _ in 0..count.max(2) - 1 {
                    let l = p.line;
                    if l + 1 >= b.lines() || l < b.first() {
                        break;
                    }
                    let v = b.line(l);
                    let next = b.line(l + 1);
                    let r = next.rows()[0].clone();
                    let from = next.first_nonblank(&r);
                    let empty = next.stops(&r).is_empty();
                    let end = v.chars.len();
                    let blank = v.chars.last().is_none_or(|c| c.is_whitespace());
                    let glue = if empty || blank { "" } else { " " };
                    b.replace(
                        pos(l, end),
                        pos(l + 1, if empty { r.end } else { from }),
                        glue,
                    );
                    b.set_cursor(Self::settle(b, pos(l, end), true));
                }
            }
            // One letter at a time: hidden markers between them stay.
            'r' | '~' if idx + count <= stops.len() => {
                for i in stops[idx..idx + count].iter().rev() {
                    let c = v.chars[*i];
                    let t: String = match m {
                        ['r', r] => r.to_string(),
                        _ if c.is_uppercase() => c.to_lowercase().collect(),
                        _ => c.to_uppercase().collect(),
                    };
                    b.replace(pos(p.line, *i), pos(p.line, i + 1), &t);
                }
                let last = idx + count - 1;
                let to = if m[0] == '~' {
                    (last + 1).min(stops.len() - 1)
                } else {
                    last
                };
                b.set_cursor(pos(p.line, stops[to]));
            }
            'u' | CTRL_R => {
                for _ in 0..count {
                    if m[0] == 'u' {
                        b.undo();
                    } else {
                        b.redo();
                    }
                }
                let c = b.cursor();
                b.set_cursor(Self::settle(b, c, true));
            }
            'i' => self.insert(b, p),
            'a' => {
                let col = if stops.is_empty() { p.col } else { p.col + 1 };
                self.insert(b, pos(p.line, col));
            }
            'I' => self.insert(b, pos(p.line, v.first_nonblank(&row))),
            'A' => self.insert(b, pos(p.line, row.end)),
            'o' | 'O' => {
                b.open(m[0] == 'o');
                let at = b.cursor();
                self.insert(b, at);
            }
            '<' | '>' => self.lines(b, m[0], p.line, p.line + count - 1),
            'v' | 'V' => {
                self.mode = if m[0] == 'v' {
                    Mode::Visual
                } else {
                    Mode::VisualLine
                };
                self.anchor = p;
                self.head = p;
                b.select(p, p, self.mode == Mode::VisualLine);
            }
            '.' => {
                let (keys, text) = (self.last.clone(), self.last_text.clone());
                for k in keys {
                    self.key(b, k);
                }
                if self.mode == Mode::Insert {
                    let c = b.cursor();
                    b.replace(c, c, &text);
                    b.set_cursor(pos(c.line, c.col + text.chars().count()));
                    self.key(b, ESC);
                }
                return;
            }
            '/' | '?' => self.prompt = Some(m[0] == '/'),
            _ => {}
        }
        if change {
            self.last = keys;
        }
    }

    /// Keys of visual mode that are not motions; false for a motion.
    fn visual(&mut self, b: &mut impl Buf, k: char) -> bool {
        let lines = self.mode == Mode::VisualLine;
        let (a, z) = (self.anchor.min(self.head), self.anchor.max(self.head));
        match k {
            ESC => {
                self.mode = Mode::Normal;
                b.set_cursor(self.head);
            }
            'v' | 'V' => {
                let want = if k == 'v' {
                    Mode::Visual
                } else {
                    Mode::VisualLine
                };
                if self.mode == want {
                    self.mode = Mode::Normal;
                    b.set_cursor(self.head);
                } else {
                    self.mode = want;
                    b.select(self.anchor, self.head, want == Mode::VisualLine);
                }
            }
            'o' => {
                std::mem::swap(&mut self.anchor, &mut self.head);
                b.select(self.anchor, self.head, lines);
            }
            'd' | 'x' | 'y' | 'c' | '<' | '>' | 'J' | '~' => {
                self.mode = Mode::Normal;
                let op = match k {
                    'x' => 'd',
                    c => c,
                };
                match op {
                    'J' => {
                        b.set_cursor(a);
                        let n = (z.line - a.line + 1).max(2);
                        self.exec(b, Some(n), None, &['J'], vec!['J']);
                    }
                    '~' => {
                        b.set_cursor(a);
                        let (gone, kept) = Self::take(b, a, pos(z.line, z.col + 1));
                        if a.line == z.line && kept.is_empty() {
                            let t: String = gone
                                .chars()
                                .map(|c| {
                                    if c.is_uppercase() {
                                        c.to_lowercase().next().unwrap_or(c)
                                    } else {
                                        c.to_uppercase().next().unwrap_or(c)
                                    }
                                })
                                .collect();
                            b.replace(a, pos(z.line, z.col + 1), &t);
                        }
                        b.set_cursor(a);
                    }
                    _ if lines || matches!(op, '<' | '>') => self.lines(b, op, a.line, z.line),
                    _ => self.operate(b, op, a, z, How::Incl),
                }
            }
            k if MOTIONS.contains(k) || "gfFtT".contains(k) => return false,
            // Anything else does nothing here.
            _ => {}
        }
        true
    }

    /// A word (`iw`) or a word and the blanks after it (`aw`), in its row.
    fn object(&self, b: &impl Buf, p: Pos, inner: bool, big: bool) -> Option<(Pos, Pos)> {
        let v = b.line(p.line);
        let row = v.row(p.col);
        let s = v.stops(&row);
        let i = s.iter().position(|x| *x == p.col)?;
        let c = |k: usize| cls(v.chars[s[k]], big);
        let run = |from: usize| {
            let (mut lo, mut hi) = (from, from);
            while lo > 0 && c(lo - 1) == c(from) {
                lo -= 1;
            }
            while hi + 1 < s.len() && c(hi + 1) == c(from) {
                hi += 1;
            }
            (lo, hi)
        };
        let (mut lo, mut hi) = run(i);
        if !inner {
            if c(i) == Cls::Blank {
                if hi + 1 < s.len() {
                    hi = run(hi + 1).1;
                }
            } else if hi + 1 < s.len() && c(hi + 1) == Cls::Blank {
                hi = run(hi + 1).1;
            } else if lo > 0 && c(lo - 1) == Cls::Blank {
                lo = run(lo - 1).0;
            }
        }
        Some((pos(p.line, s[lo]), pos(p.line, s[hi])))
    }
}

/// The next match of `pattern` after `p` (or before it), around the end;
/// lowercase patterns ignore case.
fn find(b: &impl Buf, p: Pos, pattern: &str, fwd: bool) -> Option<Pos> {
    let fold = !pattern.chars().any(char::is_uppercase);
    let low = |c: char| {
        if fold {
            c.to_lowercase().next().unwrap_or(c)
        } else {
            c
        }
    };
    let pat: Vec<char> = pattern.chars().map(low).collect();
    let n = b.lines();
    if pat.is_empty() || n == 0 {
        return None;
    }
    for k in 0..=n {
        let l = if fwd {
            (p.line + k) % n
        } else {
            (p.line + n * 2 - k) % n
        };
        let v = b.line(l);
        let vis: Vec<usize> = (0..v.chars.len()).filter(|i| v.shown[*i]).collect();
        let text: Vec<char> = vis.iter().map(|i| low(v.chars[*i])).collect();
        let hits: Vec<usize> = (0..text.len())
            .filter(|j| text[*j..].starts_with(&pat))
            .map(|j| vis[j])
            .collect();
        let pick = match (k, fwd) {
            (0, true) => hits.into_iter().find(|c| *c > p.col),
            (0, false) => hits.into_iter().rev().find(|c| *c < p.col),
            (k, true) if k == n => hits.into_iter().find(|c| *c <= p.col),
            (k, false) if k == n => hits.into_iter().rev().find(|c| *c >= p.col),
            (_, true) => hits.into_iter().next(),
            (_, false) => hits.into_iter().next_back(),
        };
        if let Some(c) = pick {
            return Some(pos(l, c));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain lines with the editor's hidden markers, and a snapshot undo.
    #[derive(Default)]
    struct Plain {
        lines: Vec<String>,
        cur: Pos,
        sel: Option<(Pos, Pos, bool)>,
        past: Vec<(Vec<String>, Pos)>,
        future: Vec<(Vec<String>, Pos)>,
    }
    impl Plain {
        fn text(&self) -> String {
            self.lines.join("\n")
        }
        fn save(&mut self) {
            self.past.push((self.lines.clone(), self.cur));
            self.future.clear();
        }
    }
    impl Buf for Plain {
        fn lines(&self) -> usize {
            self.lines.len()
        }
        fn line(&self, l: usize) -> VLine {
            VLine::block(&self.lines[l])
        }
        fn cursor(&self) -> Pos {
            self.cur
        }
        fn set_cursor(&mut self, p: Pos) {
            self.cur = p;
            self.sel = None;
        }
        fn select(&mut self, a: Pos, b: Pos, lines: bool) {
            self.sel = Some((a, b, lines));
        }
        fn replace(&mut self, from: Pos, to: Pos, text: &str) {
            self.save();
            let off = |p: Pos| -> usize {
                self.lines[..p.line]
                    .iter()
                    .map(|l| l.chars().count() + 1)
                    .sum::<usize>()
                    + p.col
            };
            let all: Vec<char> = self.text().chars().collect();
            let (a, z) = (off(from), off(to));
            let mut s: String = all[..a].iter().collect();
            s.push_str(text);
            s.extend(&all[z..]);
            self.lines = s.split('\n').map(String::from).collect();
        }
        fn open(&mut self, below: bool) {
            let l = self.cur.line;
            let at = pos(
                l,
                if below {
                    self.lines[l].chars().count()
                } else {
                    0
                },
            );
            self.replace(at, at, "\n");
            self.cur = pos(l + usize::from(below), 0);
        }
        fn nest(&mut self, lo: usize, hi: usize, deeper: bool) {
            self.save();
            for l in &mut self.lines[lo..=hi] {
                *l = if deeper {
                    format!("  {l}")
                } else {
                    l.strip_prefix("  ").unwrap_or(l).to_string()
                };
            }
        }
        fn undo(&mut self) {
            if let Some((lines, cur)) = self.past.pop() {
                self.future
                    .push((std::mem::replace(&mut self.lines, lines), self.cur));
                self.cur = cur;
            }
        }
        fn redo(&mut self) {
            if let Some((lines, cur)) = self.future.pop() {
                self.past
                    .push((std::mem::replace(&mut self.lines, lines), self.cur));
                self.cur = cur;
            }
        }
    }

    /// Types `keys` at `cur` in `text`; what insert mode does not take is typed.
    fn run(text: &str, cur: (usize, usize), keys: &str) -> (String, Pos, Vim, Plain) {
        let mut b = Plain {
            lines: text.split('\n').map(String::from).collect(),
            cur: pos(cur.0, cur.1),
            ..Plain::default()
        };
        let mut v = Vim::new();
        for k in keys.chars() {
            if !v.key(&mut b, k) {
                let c = b.cur;
                b.replace(c, c, &k.to_string());
                b.cur = pos(c.line, c.col + 1);
            }
        }
        (b.text(), b.cur, v, b)
    }
    fn at(text: &str, cur: (usize, usize), keys: &str) -> (usize, usize) {
        let p = run(text, cur, keys).1;
        (p.line, p.col)
    }
    fn edit(text: &str, cur: (usize, usize), keys: &str) -> String {
        run(text, cur, keys).0
    }

    #[test]
    fn motions() {
        let t = "one two.three  four\n\n  five six";
        assert_eq!(at(t, (0, 0), "l"), (0, 1));
        assert_eq!(at(t, (0, 0), "3l"), (0, 3));
        assert_eq!(at(t, (0, 0), "$"), (0, 18));
        assert_eq!(at(t, (0, 5), "0"), (0, 0));
        assert_eq!(at(t, (2, 9), "^"), (2, 2));
        assert_eq!(at(t, (0, 0), "w"), (0, 4));
        assert_eq!(at(t, (0, 0), "2w"), (0, 7));
        assert_eq!(at(t, (0, 0), "3w"), (0, 8));
        assert_eq!(at(t, (0, 15), "w"), (1, 0));
        assert_eq!(at(t, (0, 15), "ww"), (2, 2));
        assert_eq!(at(t, (0, 0), "W"), (0, 4));
        assert_eq!(at(t, (0, 0), "2W"), (0, 15));
        assert_eq!(at(t, (0, 0), "e"), (0, 2));
        assert_eq!(at(t, (0, 2), "e"), (0, 6));
        assert_eq!(at(t, (2, 2), "b"), (1, 0));
        assert_eq!(at(t, (0, 8), "b"), (0, 7));
        assert_eq!(at(t, (0, 8), "B"), (0, 4));
        assert_eq!(at(t, (0, 5), "j"), (1, 0));
        assert_eq!(at(t, (0, 5), "jj"), (2, 7));
        assert_eq!(at(t, (0, 5), "2jk"), (1, 0));
        assert_eq!(at(t, (2, 4), "gg"), (0, 0));
        assert_eq!(at(t, (0, 4), "G"), (2, 2));
        assert_eq!(at(t, (0, 4), "2G"), (1, 0));
        assert_eq!(at(t, (0, 0), "fe"), (0, 2));
        assert_eq!(at(t, (0, 0), "2fe"), (0, 11));
        assert_eq!(at(t, (0, 0), "te;"), (0, 10));
        assert_eq!(at(t, (0, 11), "Fo,"), (0, 16));
    }

    #[test]
    fn motions_step_over_hidden_markers() {
        // Bold markers, a list marker and a link address are hidden.
        let t = "a **bold** c\n- item [link](http://x.org) end";
        assert_eq!(at(t, (0, 0), "w"), (0, 4));
        assert_eq!(at(t, (0, 4), "e"), (0, 7));
        assert_eq!(at(t, (0, 4), "w"), (0, 11));
        assert_eq!(at(t, (0, 11), "b"), (0, 4));
        assert_eq!(at(t, (0, 0), "$"), (0, 11));
        assert_eq!(at(t, (0, 0), "j"), (1, 2));
        assert_eq!(at(t, (1, 2), "0"), (1, 2));
        assert_eq!(at(t, (1, 2), "ww"), (1, 28));
        assert_eq!(at(t, (1, 2), "$"), (1, 30));
        assert_eq!(at(t, (1, 2), "6l"), (1, 9));
    }

    #[test]
    fn rows_of_a_block_are_lines() {
        let t = format!("```{SOFT}fn a(){SOFT}  b{SOFT}```\nafter");
        // The fences are hidden: the code's rows are lines.
        assert_eq!(at(&t, (0, 4), "j"), (0, 11));
        assert_eq!(at(&t, (0, 4), "j$"), (0, 13));
        assert_eq!(at(&t, (0, 4), "jj"), (1, 0));
        assert_eq!(at(&t, (1, 0), "k^"), (0, 13));
        assert_eq!(at(&t, (0, 12), "0"), (0, 11));
    }

    #[test]
    fn operators() {
        assert_eq!(edit("one two three", (0, 4), "dw"), "one three");
        assert_eq!(edit("one two three", (0, 0), "d2w"), "three");
        assert_eq!(edit("one two three", (0, 0), "2dw"), "three");
        assert_eq!(edit("one two\nthree", (0, 4), "dw"), "one \nthree");
        assert_eq!(edit("one two three", (0, 4), "cwX\x1b"), "one X three");
        assert_eq!(edit("one two three", (0, 6), "cwX\x1b"), "one twX three");
        assert_eq!(edit("one two three", (0, 4), "d$"), "one ");
        assert_eq!(edit("one two three", (0, 4), "D"), "one ");
        assert_eq!(edit("one two three", (0, 5), "diw"), "one  three");
        assert_eq!(edit("one two three", (0, 5), "daw"), "one three");
        assert_eq!(edit("one two three", (0, 5), "ciwX\x1b"), "one X three");
        assert_eq!(edit("one two three", (0, 5), "db"), "one wo three");
        assert_eq!(edit("one two three", (0, 4), "dfe"), "one e");
        assert_eq!(edit("one two three", (0, 4), "dte"), "one ee");
        assert_eq!(edit("one two three", (0, 0), "x"), "ne two three");
        assert_eq!(edit("one two three", (0, 0), "3x"), " two three");
        assert_eq!(edit("one two three", (0, 2), "X"), "oe two three");
        assert_eq!(edit("one", (0, 0), "rX"), "Xne");
        assert_eq!(edit("one", (0, 0), "2~"), "ONe");
        assert_eq!(edit("one two", (0, 4), "C!\x1b"), "one !");
        assert_eq!(edit("one two", (0, 1), "sX\x1b"), "oXe two");
        assert_eq!(edit("a\nb\nc", (0, 0), "J"), "a b\nc");
        assert_eq!(edit("a\n- b", (0, 0), "J"), "a b");
    }

    #[test]
    fn deleting_across_a_marker_keeps_its_other_half() {
        assert_eq!(edit("**bold** next", (0, 4), "dw"), "**bo**next");
        assert_eq!(edit("a **bold** c", (0, 0), "dw"), "**bold** c");
        assert_eq!(edit("a **bold** c", (0, 4), "diw"), "a  c");
        assert_eq!(edit("a **b** c", (0, 4), "x"), "a  c");
        assert_eq!(edit("a **bold** c", (0, 4), "ciwX\x1b"), "a **X** c");
        assert_eq!(edit("a **bold** c", (0, 4), "daw"), "a c");
        assert_eq!(edit("a **bold** c", (0, 4), "x"), "a **old** c");
    }

    #[test]
    fn lines_yank_and_put() {
        let t = "- a\n- b\n- c";
        assert_eq!(edit(t, (1, 2), "dd"), "- a\n- c");
        assert_eq!(edit(t, (0, 2), "2dd"), "- c");
        assert_eq!(edit(t, (2, 2), "dd"), "- a\n- b");
        assert_eq!(edit(t, (0, 2), "dj"), "- c");
        assert_eq!(edit(t, (2, 2), "dgg"), "");
        assert_eq!(edit(t, (0, 2), "ddp"), "- b\n- a\n- c");
        assert_eq!(edit(t, (0, 2), "yyP"), "- a\n- a\n- b\n- c");
        assert_eq!(edit(t, (0, 2), "yy2p"), "- a\n- a\n- a\n- b\n- c");
        assert_eq!(edit(t, (1, 2), "ccx\x1b"), "- a\n- x\n- c");
        assert_eq!(edit("one two", (0, 0), "ywP"), "one one two");
        assert_eq!(edit("one two", (0, 0), "ywwp"), "one tone wo");
        assert_eq!(edit("one two", (0, 0), "xp"), "noe two");
        assert_eq!(edit(t, (0, 2), ">>"), "  - a\n- b\n- c");
        assert_eq!(edit(t, (0, 2), ">>j>>u"), "  - a\n- b\n- c");
    }

    #[test]
    fn insert_and_back() {
        let (text, p, v, _) = run("one", (0, 1), "iX");
        assert_eq!(
            (text.as_str(), p, v.mode),
            ("oXne", pos(0, 2), Mode::Insert)
        );
        assert_eq!(at("one", (0, 1), "iX\x1b"), (0, 1));
        assert_eq!(edit("one", (0, 1), "aX\x1b"), "onXe");
        assert_eq!(edit("  one", (0, 4), "IX\x1b"), "  Xone");
        assert_eq!(edit("- one", (0, 3), "AX\x1b"), "- oneX");
        assert_eq!(edit("a **b**", (0, 0), "AX\x1b"), "a **b**X");
        assert_eq!(edit("a\nb", (0, 0), "oX\x1b"), "a\nX\nb");
        assert_eq!(edit("a\nb", (1, 0), "OX\x1b"), "a\nX\nb");
        // Normal mode never types.
        assert_eq!(edit("one", (0, 0), "zqZ\t"), "one");
    }

    #[test]
    fn undo_redo_and_repeat() {
        assert_eq!(edit("one two", (0, 0), "xxu"), "ne two");
        assert_eq!(edit("one two", (0, 0), "xxuu"), "one two");
        assert_eq!(edit("one two", (0, 0), "xxuu\x12"), "ne two");
        assert_eq!(edit("a b c d", (0, 0), "dw.."), "d");
        assert_eq!(edit("one two three", (0, 0), "cwX\x1bw."), "X X three");
        assert_eq!(edit("- a\n- b\n- c", (0, 0), "dd."), "- c");
        assert_eq!(edit("ab\ncd", (0, 0), "AX\x1bj."), "abX\ncdX");
    }

    #[test]
    fn visual() {
        assert_eq!(edit("one two three", (0, 4), "vey$p"), "one two threetwo");
        assert_eq!(edit("one two three", (0, 4), "vld"), "one o three");
        assert_eq!(edit("one two three", (0, 4), "vecX\x1b"), "one X three");
        assert_eq!(edit("one two three", (0, 4), "viwd"), "one  three");
        assert_eq!(edit("a\nb\nc", (0, 0), "Vjd"), "c");
        assert_eq!(edit("a\nb\nc", (1, 0), "VyP"), "a\nb\nb\nc");
        assert_eq!(edit("a\nb\nc", (0, 0), "Vjx"), "c");
        assert_eq!(edit("one", (0, 0), "vl\x1bx"), "oe");
        let (_, _, v, b) = run("one two", (0, 0), "vw");
        assert_eq!(
            (v.mode, b.sel),
            (Mode::Visual, Some((pos(0, 0), pos(0, 4), false)))
        );
    }

    #[test]
    fn search() {
        let mut b = Plain {
            lines: vec!["Alpha beta".into(), "gamma **Beta**".into(), "beta".into()],
            ..Plain::default()
        };
        let mut v = Vim::new();
        v.key(&mut b, '/');
        assert_eq!(v.prompt.take(), Some(true));
        v.search(&mut b, "beta", true);
        assert_eq!(b.cur, pos(0, 6));
        v.key(&mut b, 'n');
        assert_eq!(b.cur, pos(1, 8));
        v.key(&mut b, 'n');
        assert_eq!(b.cur, pos(2, 0));
        v.key(&mut b, 'n');
        assert_eq!(b.cur, pos(0, 6));
        v.key(&mut b, 'N');
        assert_eq!(b.cur, pos(2, 0));
        v.search(&mut b, "Beta", true);
        assert_eq!(b.cur, pos(1, 8));
        v.key(&mut b, 'd');
        v.key(&mut b, 'n');
        assert_eq!(b.lines[1], "gamma **Beta**");
    }

    #[test]
    fn counts_and_bad_keys() {
        assert_eq!(at("a b c d e", (0, 0), "3w"), (0, 6));
        assert_eq!(at("a b c d e", (0, 0), "10l"), (0, 8));
        assert_eq!(at("a b c d e", (0, 0), "dq0"), (0, 0));
        assert_eq!(edit("a b c d e", (0, 0), "dqx"), " b c d e");
        assert_eq!(edit("a b c d e", (0, 0), "2d2w"), "e");
    }
}
