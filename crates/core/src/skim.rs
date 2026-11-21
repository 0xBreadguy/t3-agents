//! One pass over a JSON object's text: the values of a few top-level keys, found without
//! building anything for what is skipped. By default the Tools reader passes each call's
//! payload to the SQL function `t3_call`; `t3_times` gives Recap the two times of any turn
//! item the same way. `T3_AGENTS_SKIM` means one thing for every reader (`choice`): unset or
//! `full` is the skim, `off` is `json_extract`.
//!
//! What it reads is what SQLite's `json_extract` (3.51, the bundled one) reads from the same
//! text, decided by testing each case against it (this file's tests), not by the JSON
//! standard alone:
//! - the first of duplicate keys wins, and a key written with escapes matches its
//!   unescaped name;
//! - a string is unescaped to the bytes SQLite gives, a lone surrogate included (three
//!   bytes that are not UTF-8, so nothing that reads the value accepts it);
//! - raw control characters inside a string are accepted, a raw NUL is not, and the text
//!   ends at a NUL after the top-level value;
//! - nesting deeper than 1000 is an error.
//!
//! Two things SQLite accepts are a fixed error here instead of a guess: JSON5 (comments,
//! single quotes, bare keys, trailing commas, hexadecimal and other number forms, its extra
//! escapes and whitespace), which `JSON.stringify` never writes; and a payload stored as a
//! BLOB, which SQLite reads as JSONB or as text. No error carries text from the payload.
//!
//! One more difference, accepted for Recap: Recap's read with `json_extract` fails as a whole,
//! with a type error, when a time is a number and not a string; `t3_times` gives that time as
//! absent (NaN), as it does a time that is null or a string `recap::ms` cannot read.

use crate::{program, recap::ms};
use rusqlite::{Connection, functions::{Context, FunctionFlags}, types::ValueRef};
use std::{cell::RefCell, fmt};

/// `Full` checks the whole object to its closing brace. `Early` stops as soon as the keys
/// that matter to the row's type have been seen, or `startedAt` has put the row out, and so
/// does not notice a malformed tail.
///
/// `Head` reads a call's payload through SQLite's incremental blob interface, as far as T3's
/// key order says it must, instead of loading it whole (see "Head reads" below); commands,
/// whose exit code follows their output, are read whole and in full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode { Full, Early, Head }

/// The variable every reader reads its choice from.
pub const VARIABLE: &str = "T3_AGENTS_SKIM";

/// What a value of `VARIABLE` chooses, the same for every reader: `off` is SQLite's own JSON
/// parser (None), `early` is early, `head` is head reads, and anything else, unset included,
/// is the full skim.
pub fn choice(value: Option<&str>) -> Option<Mode> {
    match value {
        Some("off") => None,
        Some("early") => Some(Mode::Early),
        Some("head") => Some(Mode::Head),
        _ => Some(Mode::Full),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    NotStrict,
    NotText,
    Bound,
    /// A head read could not open or read the row's value.
    Unread,
    /// Head reads read the file's bytes, which are the payload's text only in UTF-8.
    Encoding,
    /// A head ended before the pass could decide: read further. Never leaves this file.
    Short,
}

// A reader shows these to a person ("can't read T3", then the sentence), so each is plain and
// fixed, names no function, holds nothing of the payload, and is true whichever function raised
// it and whatever kind of turn item it was reading.
impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Fault::NotStrict => "a recorded item is not strict JSON",
            Fault::NotText => "a recorded item is not text",
            Fault::Bound => "a period bound is not a number",
            Fault::Unread => "a recorded item could not be read",
            Fault::Encoding => "the database does not store its text as UTF-8",
            Fault::Short => "a recorded item was read only in part",
        })
    }
}

impl Fault {
    /// Every fault a function can raise (`Short` never leaves this file).
    pub const RAISED: [Fault; 5] = [Fault::NotStrict, Fault::NotText, Fault::Bound, Fault::Unread, Fault::Encoding];
}

impl std::error::Error for Fault {}

pub const MAX_KEYS: usize = 8;
// SQLite's JSON_MAX_DEPTH: an object holding 999 nested arrays parses, 1000 does not.
const MAX_DEPTH: usize = 1000;

/// A found value, as a slice of the text it was found in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found<'a> {
    /// Between the quotes, still escaped when `escaped`.
    Str { raw: &'a [u8], escaped: bool },
    /// `integer` when it has no fraction and no exponent: SQLite's `json_type` says
    /// "integer" for it, whatever its size.
    Number { raw: &'a [u8], integer: bool },
    True,
    False,
    Null,
    /// The whole value, brackets included.
    Object(&'a [u8]),
    Array(&'a [u8]),
}

pub type Fields<'a> = [Option<Found<'a>>; MAX_KEYS];

/// Where the pass is when it asks whether to stop.
#[derive(Clone, Copy, Debug)]
pub enum Step<'k> {
    /// A top-level key has been read, and its value not yet.
    Key { raw: &'k [u8], escaped: bool },
    /// A wanted key's value has just been found.
    Value,
}

/// Find the first value of each of `keys` (at most `MAX_KEYS`) at the top level of `text`.
/// `None` when the text is a JSON value other than an object. `stop` is asked after each
/// top-level key and after each wanted value; when it says so, the pass ends there,
/// unchecked beyond.
pub fn skim<'a>(text: &'a [u8], keys: &[&[u8]], stop: &mut dyn FnMut(Step<'_>, &Fields<'a>) -> bool) -> Result<Option<Fields<'a>>, Fault> {
    skim_in(text, false, keys, stop)
}

// `cut` says `text` is the head of a longer value. Then reaching its end, anywhere, and
// every fault, mean only that the pass needs more of the value (`Fault::Short`): a string,
// an escape or a character cut short is the normal case, and a real fault is found again,
// and reported, when the whole value is read. Nothing is accepted at the cut: a number
// that reaches it might go on, and the text after the top-level value is not yet seen.
fn skim_in<'a>(text: &'a [u8], cut: bool, keys: &[&[u8]], stop: &mut dyn FnMut(Step<'_>, &Fields<'a>) -> bool) -> Result<Option<Fields<'a>>, Fault> {
    let found = skim_on(text, cut, keys, stop);
    if cut && found.is_err() { Err(Fault::Short) } else { found }
}

fn skim_on<'a>(text: &'a [u8], cut: bool, keys: &[&[u8]], stop: &mut dyn FnMut(Step<'_>, &Fields<'a>) -> bool) -> Result<Option<Fields<'a>>, Fault> {
    let keys = &keys[..keys.len().min(MAX_KEYS)];
    let mut c = Cursor { b: text, i: 0, cut };
    c.ws();
    if c.peek() != b'{' {
        c.value(0)?;
        return c.end().map(|()| None);
    }
    c.i += 1;
    c.ws();
    let mut fields: Fields<'a> = [None; MAX_KEYS];
    if c.peek() == b'}' {
        c.i += 1;
        return c.end().map(|()| Some(fields));
    }
    loop {
        if c.peek() != b'"' { return Err(Fault::NotStrict); }
        let start = c.i + 1;
        let escaped = c.string()?;
        let raw = &text[start..c.i - 1];
        let slot = wanted(raw, escaped, keys).filter(|&k| fields[k].is_none());
        c.ws();
        c.expect(b':')?;
        c.ws();
        if stop(Step::Key { raw, escaped }, &fields) { return Ok(Some(fields)); }
        match slot {
            Some(k) => {
                fields[k] = Some(c.found(1)?);
                if stop(Step::Value, &fields) { return Ok(Some(fields)); }
            }
            None => c.value(1)?,
        }
        c.ws();
        match c.peek() {
            b',' => { c.i += 1; c.ws(); }
            b'}' => { c.i += 1; return c.end().map(|()| Some(fields)); }
            _ => return Err(Fault::NotStrict),
        }
    }
}

fn wanted(raw: &[u8], escaped: bool, keys: &[&[u8]]) -> Option<usize> {
    if !escaped { return keys.iter().position(|key| *key == raw); }
    // SQLite compares a key unescaped. Unescaping never lengthens text, and an escape takes
    // at most six bytes for one byte of a name, so a long escaped key cannot be a wanted one.
    let mut buf = [0u8; 96];
    if raw.len() > buf.len() { return None; }
    let mut n = 0;
    unescape(raw, &mut |part| { buf[n..n + part.len()].copy_from_slice(part); n += part.len(); });
    keys.iter().position(|key| *key == &buf[..n])
}

struct Cursor<'a> { b: &'a [u8], i: usize, cut: bool }

impl<'a> Cursor<'a> {
    // The end of the text reads as NUL: SQLite stops at a NUL, and neither is valid inside
    // a document, so both end it the same way.
    fn peek(&self) -> u8 { self.b.get(self.i).copied().unwrap_or(0) }

    fn ws(&mut self) {
        while matches!(self.peek(), b' ' | b'\t' | b'\n' | b'\r') { self.i += 1; }
    }

    fn expect(&mut self, byte: u8) -> Result<(), Fault> {
        if self.peek() != byte { return Err(Fault::NotStrict); }
        self.i += 1;
        Ok(())
    }

    fn end(&mut self) -> Result<(), Fault> {
        self.ws();
        if self.peek() == 0 && !self.cut { Ok(()) } else { Err(Fault::NotStrict) }
    }

    /// At an opening quote: past the closing one, and whether there was an escape. A vectorized
    /// search (`memchr`, chosen at run time) to the next quote, backslash or NUL; nothing is copied.
    fn string(&mut self) -> Result<bool, Fault> {
        let b = self.b;
        let mut i = self.i + 1;
        let mut escaped = false;
        loop {
            i += memchr::memchr3(b'"', b'\\', 0, &b[i..]).ok_or(Fault::NotStrict)?;
            match b[i] {
                0 => return Err(Fault::NotStrict),
                b'"' => { self.i = i + 1; return Ok(escaped); }
                b'\\' => {
                    escaped = true;
                    match b.get(i + 1) {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => i += 2,
                        Some(b'u') if b.get(i + 2..i + 6).is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit)) => i += 6,
                        _ => return Err(Fault::NotStrict),
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    /// Strict JSON's number: `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`. Whether it is an
    /// integer. What follows is the caller's to check, so `01` fails there.
    fn number(&mut self) -> Result<bool, Fault> {
        let digits = |c: &mut Self| -> Result<(), Fault> {
            if !c.peek().is_ascii_digit() { return Err(Fault::NotStrict); }
            while c.peek().is_ascii_digit() { c.i += 1; }
            Ok(())
        };
        if self.peek() == b'-' { self.i += 1; }
        match self.peek() {
            b'0' => self.i += 1,
            b'1'..=b'9' => digits(self)?,
            _ => return Err(Fault::NotStrict),
        }
        let mut integer = true;
        if self.peek() == b'.' {
            integer = false;
            self.i += 1;
            digits(self)?;
        }
        if matches!(self.peek(), b'e' | b'E') {
            integer = false;
            self.i += 1;
            if matches!(self.peek(), b'+' | b'-') { self.i += 1; }
            digits(self)?;
        }
        if self.cut && self.i >= self.b.len() { return Err(Fault::NotStrict); }
        Ok(integer)
    }

    fn literal(&mut self, word: &[u8]) -> Result<(), Fault> {
        if !self.b.get(self.i..).is_some_and(|rest| rest.starts_with(word)) { return Err(Fault::NotStrict); }
        self.i += word.len();
        Ok(())
    }

    // At a member's key: past the key, the colon and the whitespace after it.
    fn key(&mut self) -> Result<(), Fault> {
        if self.peek() != b'"' { return Err(Fault::NotStrict); }
        self.string()?;
        self.ws();
        self.expect(b':')?;
        self.ws();
        Ok(())
    }

    /// Check one value and step past it; `depth` is how deep the value sits. Containers are
    /// followed with a stack of bits, one per open container, set for an object.
    fn value(&mut self, depth: usize) -> Result<(), Fault> {
        let mut open = [0u64; MAX_DEPTH / 64 + 1];
        let mut n = 0;
        'value: loop {
            match self.peek() {
                b'"' => { self.string()?; }
                b @ (b'{' | b'[') => {
                    if depth + n + 1 > MAX_DEPTH { return Err(Fault::NotStrict); }
                    let object = b == b'{';
                    if object { open[n / 64] |= 1 << (n % 64); } else { open[n / 64] &= !(1 << (n % 64)); }
                    n += 1;
                    self.i += 1;
                    self.ws();
                    let close = if object { b'}' } else { b']' };
                    if self.peek() == close {
                        self.i += 1;
                        n -= 1;
                    } else {
                        if object { self.key()?; }
                        continue 'value;
                    }
                }
                b't' => self.literal(b"true")?,
                b'f' => self.literal(b"false")?,
                b'n' => self.literal(b"null")?,
                b'-' | b'0'..=b'9' => { self.number()?; }
                _ => return Err(Fault::NotStrict),
            }
            loop {
                if n == 0 { return Ok(()); }
                self.ws();
                let object = open[(n - 1) / 64] >> ((n - 1) % 64) & 1 == 1;
                match self.peek() {
                    b',' => {
                        self.i += 1;
                        self.ws();
                        if object { self.key()?; }
                        continue 'value;
                    }
                    b'}' if object => { self.i += 1; n -= 1; }
                    b']' if !object => { self.i += 1; n -= 1; }
                    _ => return Err(Fault::NotStrict),
                }
            }
        }
    }

    fn found(&mut self, depth: usize) -> Result<Found<'a>, Fault> {
        let start = self.i;
        Ok(match self.peek() {
            b'"' => {
                let escaped = self.string()?;
                Found::Str { raw: &self.b[start + 1..self.i - 1], escaped }
            }
            b'-' | b'0'..=b'9' => {
                let integer = self.number()?;
                Found::Number { raw: &self.b[start..self.i], integer }
            }
            b't' => { self.literal(b"true")?; Found::True }
            b'f' => { self.literal(b"false")?; Found::False }
            b'n' => { self.literal(b"null")?; Found::Null }
            b'{' => { self.value(depth)?; Found::Object(&self.b[start..self.i]) }
            b'[' => { self.value(depth)?; Found::Array(&self.b[start..self.i]) }
            _ => return Err(Fault::NotStrict),
        })
    }
}

/// The bytes `json_extract` gives for a string, from its raw text (which `Cursor::string`
/// has checked). A high surrogate joins a low one that follows it; any other surrogate is
/// written on its own as three bytes, as SQLite writes it.
fn unescape(raw: &[u8], push: &mut dyn FnMut(&[u8])) {
    let mut i = 0;
    while i < raw.len() {
        let next = raw[i..].iter().position(|&b| b == b'\\').map_or(raw.len(), |p| i + p);
        push(&raw[i..next]);
        let Some(&escape) = raw.get(next + 1) else { return };
        i = next + 2;
        if escape != b'u' {
            push(&[match escape { b'b' => 8, b'f' => 12, b'n' => b'\n', b'r' => b'\r', b't' => b'\t', other => other }]);
            continue;
        }
        let Some(mut code) = hex4(raw, i) else { return };
        i += 4;
        if code & 0xfc00 == 0xd800 && raw.get(i..i + 2) == Some(b"\\u") {
            if let Some(low) = hex4(raw, i + 2).filter(|low| low & 0xfc00 == 0xdc00) {
                code = ((code & 0x3ff) << 10) + (low & 0x3ff) + 0x10000;
                i += 6;
            }
        }
        push(utf8(code, &mut [0; 4]));
    }
}

fn hex4(raw: &[u8], at: usize) -> Option<u32> {
    raw.get(at..at + 4)?.iter().try_fold(0, |value, &digit| Some(value * 16 + (digit as char).to_digit(16)?))
}

// UTF-8's arithmetic for any code below 0x110000, surrogates included.
fn utf8(code: u32, out: &mut [u8; 4]) -> &[u8] {
    let n = match code { 0..=0x7f => 1, 0x80..=0x7ff => 2, 0x800..=0xffff => 3, _ => 4 };
    if n == 1 {
        out[0] = code as u8;
    } else {
        for k in (1..n).rev() { out[k] = 0x80 | (code >> (6 * (n - 1 - k))) as u8 & 0x3f; }
        out[0] = [0, 0, 0xc0, 0xe0, 0xf0][n] | (code >> (6 * (n - 1))) as u8;
    }
    &out[..n]
}

// A found string's bytes as `json_extract` gives them, to a reader that may decline them.
fn with_text<R>(value: Found<'_>, read: impl FnOnce(&[u8]) -> Option<R>) -> Option<R> {
    let Found::Str { raw, escaped } = value else { return None };
    if !escaped { return read(raw); }
    let mut text = Vec::with_capacity(raw.len());
    unescape(raw, &mut |part| text.extend_from_slice(part));
    read(&text)
}

// `tools::stamp`: text that is UTF-8 and that `recap::ms` reads.
fn stamp(bytes: &[u8]) -> Option<f64> { std::str::from_utf8(bytes).ok().and_then(ms) }

// A call belongs to a period by its instant, not by its spelling: `startedAt` read by `stamp`,
// at or after `from` and before `to`, compared as epoch milliseconds. A value that is not a
// string, or that `ms` cannot read, is not a call.
fn started(value: Found<'_>, from: f64, to: f64) -> Option<f64> {
    with_text(value, stamp).filter(|&start| from <= start && start < to)
}

// The query's test for a tool's name: text, 1 to 80 characters, no NUL, and nothing outside
// `A-Za-z0-9_.:/-` (so ASCII, and its characters are its bytes).
fn tool_name(bytes: &[u8]) -> Option<String> {
    let safe = (1..=80).contains(&bytes.len()) && bytes.iter().all(|&b| b.is_ascii_alphanumeric() || b"_.:/-".contains(&b));
    safe.then(|| bytes.iter().map(|&b| b as char).collect())
}

// `json_extract` gives an object or an array as its text without the whitespace between
// tokens; strings and numbers are kept as written (tested against SQLite).
fn minify(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let (mut inside, mut escape) = (false, false);
    for &b in raw {
        if inside {
            out.push(b);
            if escape { escape = false; } else if b == b'\\' { escape = true; } else if b == b'"' { inside = false; }
        } else if !matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
            out.push(b);
            inside = b == b'"';
        }
    }
    out
}

// The order T3 writes a turn item's top-level keys: the base fields, then `type`,
// then the variant's own. An absent optional key is left out, never null.
const BASE: [&[u8]; 20] = [b"deferredWork", b"toolNonExecutionKind", b"toolSurface", b"toolIcon", b"toolSource", b"id",
    b"threadId", b"runId", b"nodeId", b"providerThreadId", b"providerTurnId", b"nativeItemRef", b"parentItemId", b"ordinal",
    b"status", b"title", b"startedAt", b"completedAt", b"updatedAt", b"type"];
const VARIANTS: [(&[u8], &[&[u8]]); 5] = [
    (b"command_execution", &[b"input", b"output", b"outputOmitted", b"outputIndicatesFailure", b"exitCode", b"monitor", b"completionWake"]),
    (b"dynamic_tool", &[b"toolName", b"viewedImagePath", b"input", b"output", b"outputOmitted"]),
    (b"file_change", &[b"fileName", b"additions", b"deletions", b"diffStr", b"oldStr", b"newStr", b"changes"]),
    (b"file_search", &[b"pattern", b"results"]),
    (b"web_search", &[b"patterns", b"results"]),
];

/// A key's place in the order T3 writes a turn item of this type in, if T3 writes it.
pub(crate) fn rank(kind: &[u8], key: &[u8]) -> Option<usize> {
    let (_, own) = VARIANTS.iter().find(|(name, _)| *name == kind)?;
    BASE.iter().chain(own.iter()).position(|k| *k == key)
}

// `rank` for a row of a known tool type, or, without one (`t3_times`, any turn item), the place
// among the base fields every variant opens with; a variant's own key then has no place.
fn place(kind: Option<&[u8]>, key: &[u8]) -> Option<usize> {
    match kind { Some(kind) => rank(kind, key), None => BASE.iter().position(|k| *k == key) }
}

const STARTED: usize = 0;
const COMPLETED: usize = 1;
const TOOL: usize = 2;
const EXIT: usize = 3;
const INPUT: usize = 4;
const KEYS: [&[u8]; 5] = [b"startedAt", b"completedAt", b"toolName", b"exitCode", b"input"];
/// Joins the five fields of `call`'s value.
pub const SEPARATOR: char = '\x1f';

// When an early pass, or a head read, may stop. Order-free: every relevant key has been seen,
// or `startedAt` has put the row out of the period (both hold whatever the order, since the
// first of duplicates wins). From T3's order: once `startedAt` has been read, and while every
// key read so far is one T3 writes, unescaped, each later in its order than the one before, a
// relevant key that the order places before the latest key is taken to be absent. The first
// key that breaks this (unknown, escaped, repeated or out of place) turns that rule off for
// the row, and so does any key read before `startedAt` that is out of T3's order. Requiring
// `startedAt` first is the evidence: without it a payload that opens with `type`, as no T3
// payload does, would rule out both times at its first key. What the rule cannot see is a
// payload that keeps T3's order up to where it stops and breaks it after.
struct Rule<'r> {
    kind: Option<&'r [u8]>,
    relevant: &'r [usize],
    bounds: Option<(f64, f64)>,
    // Decided once, when `startedAt` is first seen: the start, or None for out.
    start: Option<Option<f64>>,
    // The place of the latest key while the payload keeps T3's order; None once it has not.
    ordered: Option<Option<usize>>,
    anchored: bool,
    places: [usize; 5],
}

impl<'r> Rule<'r> {
    fn new(kind: Option<&'r [u8]>, relevant: &'r [usize], bounds: Option<(f64, f64)>, by_order: bool) -> Rule<'r> {
        let known = kind.is_none_or(|kind| rank(kind, b"type").is_some());
        Rule { kind, relevant, bounds, start: None, ordered: (by_order && known).then_some(None), anchored: false,
            places: KEYS.map(|key| place(kind, key).unwrap_or(usize::MAX)) }
    }

    fn stop(&mut self, step: Step<'_>, fields: &Fields<'_>) -> bool {
        match step {
            Step::Key { raw, escaped } => if let Some(latest) = self.ordered {
                self.ordered = match place(self.kind, raw) {
                    Some(place) if !escaped && latest.is_none_or(|latest| place > latest) => Some(Some(place)),
                    _ => None,
                };
                self.anchored |= self.ordered.is_some() && !escaped && raw == KEYS[STARTED];
            },
            Step::Value => if let (None, Some((from, to)), Some(value)) = (self.start, self.bounds, fields[STARTED]) {
                self.start = Some(started(value, from, to));
            },
        }
        let gone = |k: usize| self.anchored && matches!(self.ordered, Some(Some(latest)) if self.places[k] < latest);
        self.start == Some(None) || self.relevant.iter().all(|&k| fields[k].is_some() || gone(k))
    }
}

/// What the Tools reader takes from one call's payload, or `None` when the call is not
/// counted (no `startedAt` that `recap::ms` reads at an instant in `[from, to)`, epoch
/// milliseconds). Five fields joined by
/// `SEPARATOR`: the start and the end (when it reads) as epoch milliseconds, the tool's name
/// when it is shaped like a name (`dynamic_tool` only), the exit code when it is a JSON
/// integer that fits (`command_execution` only), and the listed programs of the command
/// line, space-separated (`command_execution` only). Nothing else of the payload leaves.
///
/// `Early` stops by `Rule`; `Full` (and `Head`, given a whole payload) checks it to the end.
pub fn call(payload: &[u8], kind: &[u8], from: f64, to: f64, mode: Mode) -> Result<Option<String>, Fault> {
    call_with(payload, kind, from, to, mode == Mode::Early, true, false)
}

// `cut`: `payload` is the head of a longer value (a head read), so the pass stops by `Rule` or
// says it is short.
fn call_with(payload: &[u8], kind: &[u8], from: f64, to: f64, early: bool, by_order: bool, cut: bool) -> Result<Option<String>, Fault> {
    let tool = kind == b"dynamic_tool";
    let command = kind == b"command_execution";
    let relevant: &[usize] = if command { &[STARTED, COMPLETED, EXIT, INPUT] } else if tool { &[STARTED, COMPLETED, TOOL] } else { &[STARTED, COMPLETED] };
    let fields = if early || cut {
        let mut rule = Rule::new(Some(kind), relevant, Some((from, to)), by_order);
        skim_in(payload, cut, &KEYS, &mut |step, fields| rule.stop(step, fields))?
    } else {
        skim_in(payload, false, &KEYS, &mut |_, _| false)?
    };
    let Some(fields) = fields else { return Ok(None) };
    let Some(start) = fields[STARTED].and_then(|value| started(value, from, to)) else { return Ok(None) };
    let end = fields[COMPLETED].and_then(|value| with_text(value, stamp));
    let name = if tool { fields[TOOL].and_then(|value| with_text(value, tool_name)) } else { None };
    let exit = match fields[EXIT] {
        Some(Found::Number { raw, integer: true }) if command => std::str::from_utf8(raw).ok().and_then(|n| n.parse::<i64>().ok()),
        _ => None,
    };
    let programs = match fields[INPUT] {
        _ if !command => None,
        Some(value @ Found::Str { .. }) => with_text(value, |bytes| std::str::from_utf8(bytes).ok().map(program::programs)),
        Some(Found::Object(raw) | Found::Array(raw)) => std::str::from_utf8(&minify(raw)).ok().map(program::programs),
        _ => None,
    };
    let s = SEPARATOR;
    Ok(Some(format!("{start}{s}{}{s}{}{s}{}{s}{}", end.map_or(String::new(), |end| end.to_string()),
        name.unwrap_or_default(), exit.map_or(String::new(), |exit| exit.to_string()), programs.unwrap_or_default().join(" "))))
}

/// Register `t3_call(payload, type, from, to)` on a connection, in one mode (`from` and `to`
/// are epoch milliseconds). `Head` registers
/// `t3_call` in full, for commands, and `t3_call_head` beside it (`register_heads`).
pub fn register(db: &Connection, mode: Mode) -> rusqlite::Result<()> {
    register_with(db, mode, true)?;
    if mode == Mode::Head { register_heads(db)?; }
    Ok(())
}

fn register_with(db: &Connection, mode: Mode, by_order: bool) -> rusqlite::Result<()> {
    db.create_scalar_function("t3_call", 4, FunctionFlags::SQLITE_UTF8
        | FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS, move |ctx| {
        let payload = match ctx.get_raw(0) {
            ValueRef::Text(bytes) => bytes,
            // `json_extract` finds no key in NULL or in a number's text.
            ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => return Ok(None),
            ValueRef::Blob(_) => return Err(fail(Fault::NotText)),
        };
        let (kind, from, to) = call_arguments(ctx, 1)?;
        call_with(payload, kind, from, to, mode == Mode::Early, by_order, false).map_err(fail)
    })
}

fn fail(fault: Fault) -> rusqlite::Error { rusqlite::Error::UserFunctionError(Box::new(fault)) }

// The type and the two bounds (epoch milliseconds), from argument `at` on.
fn call_arguments<'c>(ctx: &'c Context<'_>, at: usize) -> rusqlite::Result<(&'c [u8], f64, f64)> {
    // A type that is not text is neither of the two that read more than the times.
    let kind: &[u8] = match ctx.get_raw(at) { ValueRef::Text(bytes) => bytes, _ => &[] };
    let (Some(from), Some(to)) = (number(ctx.get_raw(at + 1)), number(ctx.get_raw(at + 2))) else { return Err(fail(Fault::Bound)) };
    Ok((kind, from, to))
}

// A bound, given as epoch milliseconds, integer or real.
fn number(value: ValueRef<'_>) -> Option<f64> {
    match value { ValueRef::Integer(n) => Some(n as f64), ValueRef::Real(f) => Some(f), _ => None }
}

/// `startedAt` and `completedAt` of a turn item's payload, as epoch milliseconds the way
/// `recap::ms` reads the string, NaN where a time is absent, null, not a string, or a string
/// `ms` cannot read. A full pass: the payload is checked to its end, and errors as `call` does.
pub fn times(payload: &[u8]) -> Result<[f64; 2], Fault> { times_with(payload, false, false) }

// `early`: stop by `Rule`, which for the times alone uses only the base fields' order. `cut`:
// `payload` is the head of a longer value.
fn times_with(payload: &[u8], early: bool, cut: bool) -> Result<[f64; 2], Fault> {
    let fields = if early || cut {
        let mut rule = Rule::new(None, &[STARTED, COMPLETED], None, true);
        skim_in(payload, cut, &KEYS[..2], &mut |step, fields| rule.stop(step, fields))?
    } else {
        skim_in(payload, false, &KEYS[..2], &mut |_, _| false)?
    };
    let read = |value: Option<Found<'_>>| value.and_then(|value| with_text(value, stamp)).unwrap_or(f64::NAN);
    Ok(fields.map_or([f64::NAN; 2], |fields| [read(fields[STARTED]), read(fields[COMPLETED])]))
}

/// The value `t3_times` gives: the two times as 64-bit floats, little-endian, start first.
pub fn times_blob(times: [f64; 2]) -> [u8; 16] {
    let mut out = [0; 16];
    out[..8].copy_from_slice(&times[0].to_le_bytes());
    out[8..].copy_from_slice(&times[1].to_le_bytes());
    out
}

/// Register `t3_times(payload)`, deterministic: always a 16-byte blob (`times_blob`), two NaN
/// for NULL or a number, where `json_extract` finds no key; an error for a BLOB or a payload
/// that is not strict JSON, with `call`'s fixed messages.
pub fn register_times(db: &Connection) -> rusqlite::Result<()> {
    db.create_scalar_function("t3_times", 1, FunctionFlags::SQLITE_UTF8
        | FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS, |ctx| {
        let times = match ctx.get_raw(0) {
            ValueRef::Text(bytes) => times(bytes).map_err(fail)?,
            ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => [f64::NAN; 2],
            ValueRef::Blob(_) => return Err(fail(Fault::NotText)),
        };
        Ok(times_blob(times).to_vec())
    })
}

// ── Head reads ──────────────────────────────────────────────────────────────
//
// `t3_call_head(rowid, typeof(payload_json), type, from, to)` and
// `t3_times_head(rowid, typeof(payload_json))` give what `t3_call` and `t3_times` give for the
// row's payload, without its being loaded whole. A function's argument is always loaded whole;
// these are given the row instead, and read `orchestration_v2_projection_turn_items.payload_json`
// through SQLite's incremental blob interface on the connection running the query: inside its
// read transaction, so they see the query's own snapshot, and inside the function, so no byte
// of the payload leaves it but what `t3_call` or `t3_times` would give. `typeof` of a column is
// answered without loading the value (sqlite3.c, OPFLAG_TYPEOFARG), and tells text, which is
// read, from a BLOB, refused as `t3_call` refuses it, and from NULL or a number, which hold no key.
//
// Why this is allowed: sqlite3.h, on sqlite3_create_function: "An application-defined function is
// permitted to call other SQLite interfaces. However, such calls must not close the database
// connection nor finalize or reset the prepared statement in which the function is running." A
// blob handle opened and closed within the call does neither. rusqlite's `get_connection` is
// unsafe only because the non-owning connection it returns could be sent to another thread; it
// is used within the call and dropped there. The functions read the database, so they are
// registered neither deterministic nor innocuous, and direct-only.
//
// How far they read: a first step of `FIRST_STEP` bytes, then four times as far each time,
// to the whole value. On a head the pass stops by `Rule`; if it cannot decide there it asks
// for more. On the whole value it is the full pass, so a payload short enough for one step, or
// one whose keys `Rule` never settles, gives exactly what the full pass gives.
//
// What a head read gives up, as early does, and only on a payload longer than one step: it
// does not check what follows the point where `Rule` stopped (a malformed tail, or JSON5 there,
// gives a row where the full pass gives an error), and it trusts T3's key order as `Rule` says
// (a payload that keeps the order up to the stop and breaks it after can lose a key the full
// pass would find: an end, a tool's name). Every other payload gives the full pass's value.

const TABLE: &std::ffi::CStr = c"orchestration_v2_projection_turn_items";
const COLUMN: &std::ffi::CStr = c"payload_json";
/// Initial read size in bytes. Incomplete fields cause the next read to grow.
pub const FIRST_STEP: usize = 2048;

// Read row `rowid`'s value step by step until `read` decides on a head, or on the whole value.
fn head<R>(ctx: &Context<'_>, rowid: i64, buf: &mut Vec<u8>, read: &mut dyn FnMut(&[u8], bool) -> Result<R, Fault>) -> Result<R, Fault> {
    // SAFETY: the connection is the one running this call's statement, on this thread; it is
    // neither closed nor sent anywhere, and dropped before the call returns.
    let db = unsafe { ctx.get_connection() }.map_err(|_| Fault::Unread)?;
    let blob = db.blob_open(rusqlite::MAIN_DB, TABLE, COLUMN, rowid, true).map_err(|_| Fault::Unread)?;
    let len = blob.len();
    buf.clear();
    let mut step = FIRST_STEP;
    loop {
        let (have, want) = (buf.len(), step.min(len));
        buf.resize(want, 0);
        blob.read_at_exact(&mut buf[have..want], have).map_err(|_| Fault::Unread)?;
        let cut = want < len;
        match read(&buf[..want], cut) {
            Err(Fault::Short) if cut => step = step.saturating_mul(4),
            Err(Fault::Short) => return Err(Fault::NotStrict),
            other => return other,
        }
    }
}

// The row and what `typeof(payload_json)` said of its value: Some(rowid) to read, or None for
// a value that holds no key.
fn head_row(ctx: &Context<'_>) -> rusqlite::Result<Option<i64>> {
    let ValueRef::Integer(rowid) = ctx.get_raw(0) else { return Err(fail(Fault::Unread)) };
    match ctx.get_raw(1) {
        ValueRef::Text(b"text") => Ok(Some(rowid)),
        ValueRef::Text(b"blob") => Err(fail(Fault::NotText)),
        _ => Ok(None),
    }
}

/// Register `t3_call_head` and `t3_times_head` (see "Head reads"). An error on a database whose
/// text is not UTF-8, since the blob interface gives the file's bytes.
pub fn register_heads(db: &Connection) -> rusqlite::Result<()> {
    let encoding: String = db.query_row("PRAGMA encoding", [], |r| r.get(0))?;
    if encoding != "UTF-8" { return Err(fail(Fault::Encoding)); }
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DIRECTONLY;
    // One buffer a function, kept between calls; a fresh one if it were ever in use.
    let buf = RefCell::new(Vec::new());
    db.create_scalar_function("t3_call_head", 5, flags, move |ctx| {
        let Some(rowid) = head_row(ctx)? else { return Ok(None) };
        let (kind, from, to) = call_arguments(ctx, 2)?;
        let mut spare = Vec::new();
        let mut buf = buf.try_borrow_mut();
        let buf = buf.as_deref_mut().unwrap_or(&mut spare);
        head(ctx, rowid, buf, &mut |payload, cut| call_with(payload, kind, from, to, false, true, cut)).map_err(fail)
    })?;
    let buf = RefCell::new(Vec::new());
    db.create_scalar_function("t3_times_head", 2, flags, move |ctx| {
        let mut spare = Vec::new();
        let mut buf = buf.try_borrow_mut();
        let buf = buf.as_deref_mut().unwrap_or(&mut spare);
        let times = match head_row(ctx)? {
            Some(rowid) => head(ctx, rowid, buf, &mut |payload, cut| times_with(payload, false, cut)).map_err(fail)?,
            None => [f64::NAN; 2],
        };
        Ok(times_blob(times).to_vec())
    })
}

/// Seeded payloads for the differential tests here and in `tools.rs`: what T3 writes, and
/// around it everything `json_extract` might read differently.
#[cfg(test)]
pub(crate) mod cases {
    use crate::recap::{iso, ms};

    const BS: &str = "\\";
    // Stands for one raw byte that is not UTF-8 (0xFF) until `case` swaps it in.
    const BAD: char = '\u{e000}';
    pub(crate) const SENTINEL: &str = "never read";
    pub(crate) const KINDS: [&str; 6] = ["command_execution", "dynamic_tool", "file_change", "file_search", "web_search", "user_message"];

    pub(crate) struct Rng(pub(crate) u64);

    impl Rng {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        pub(crate) fn below(&mut self, n: usize) -> usize { (self.next() % n as u64) as usize }
        pub(crate) fn chance(&mut self, p: f64) -> bool { ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p }
        pub(crate) fn pick<T: Clone>(&mut self, items: &[T]) -> T { items[self.below(items.len())].clone() }
    }

    #[derive(Clone, Debug)]
    pub(crate) enum Stored { Text(Vec<u8>), Blob(Vec<u8>), Integer(i64), Real(f64), Null }

    pub(crate) struct Case {
        pub(crate) kind: &'static str,
        pub(crate) payload: Stored,
        /// Holds JSON5 that SQLite accepts: the skim must refuse it, never misread it.
        pub(crate) json5: bool,
        /// Not JSON even to SQLite: today's read fails.
        pub(crate) malformed: bool,
        pub(crate) notes: Vec<&'static str>,
        /// The top-level keys as written, in order: None for one with an escape or not quoted.
        pub(crate) keys: Vec<Option<String>>,
    }

    impl Case {
        pub(crate) fn new(kind: &'static str, text: &str, json5: bool, malformed: bool, note: &'static str) -> Case {
            Case { kind, payload: Stored::Text(text.as_bytes().to_vec()), json5, malformed, notes: vec![note], keys: vec![None] }
        }
        /// The payload's text, for building on.
        pub(crate) fn describe_text(&self) -> String {
            match &self.payload { Stored::Text(b) => String::from_utf8_lossy(b).into_owned(), _ => String::new() }
        }

        /// For a failure message: the notes, and the payload's head, escaped.
        pub(crate) fn describe(&self) -> String {
            let head = match &self.payload {
                Stored::Text(b) | Stored::Blob(b) => String::from_utf8_lossy(&b[..b.len().min(400)]).escape_debug().to_string(),
                other => format!("{other:?}"),
            };
            format!("{} {:?} json5={} malformed={} keys={:?} {head}", self.kind, self.notes, self.json5, self.malformed, self.keys)
        }

        /// Whether the keys follow the order T3 writes them in, as `call`'s early stop assumes.
        pub(crate) fn in_t3_order(&self) -> bool {
            let mut latest = None;
            self.keys.iter().all(|key| {
                let place = key.as_deref().and_then(|key| super::rank(self.kind.as_bytes(), key.as_bytes()));
                let ok = place.is_some_and(|place| latest.is_none_or(|latest| place > latest));
                latest = place;
                ok
            })
        }
    }

    pub(crate) fn u(code: u32) -> String { format!("{BS}u{code:04x}") }
    fn upper(code: u32) -> String { format!("{BS}u{code:04X}") }
    pub(crate) fn quote(raw: &str) -> String { format!("\"{raw}\"") }

    /// A JSON string's raw text for `text`, escapes chosen at random where they are optional.
    pub(crate) fn encode(rng: &mut Rng, text: &str) -> String {
        let mut out = String::new();
        let esc = |rng: &mut Rng, out: &mut String, code: u32| out.push_str(&if rng.chance(0.5) { u(code) } else { upper(code) });
        for ch in text.chars() {
            match ch {
                '"' => out.push_str(&format!("{BS}\"")),
                '\\' => out.push_str(&format!("{BS}{BS}")),
                '\0' => esc(rng, &mut out, 0),
                '\n' if rng.chance(0.8) => out.push_str(&format!("{BS}n")),
                '\t' if rng.chance(0.8) => out.push_str(&format!("{BS}t")),
                // SQLite takes a raw control character in a string; JSON.stringify never writes one.
                c if (c as u32) < 0x20 => if rng.chance(0.3) { out.push(c) } else { esc(rng, &mut out, c as u32) },
                '/' if rng.chance(0.3) => out.push_str(&format!("{BS}/")),
                c if c == BAD => out.push(c),
                c if c.is_ascii() => if rng.chance(0.03) { esc(rng, &mut out, c as u32) } else { out.push(c) },
                c => if rng.chance(0.4) {
                    out.push(c)
                } else {
                    for unit in c.encode_utf16(&mut [0; 2]) { esc(rng, &mut out, u32::from(*unit)); }
                },
            }
        }
        out
    }

    fn mid(rng: &mut Rng, from: &str, to: &str) -> String {
        let (lo, hi) = (ms(from).unwrap(), ms(to).unwrap());
        iso(lo + rng.below((hi - lo) as usize) as f64)
    }

    fn timestamp(rng: &mut Rng, from: &str, to: &str) -> String {
        let (lo, hi) = (ms(from).unwrap(), ms(to).unwrap());
        let mid = mid(rng, from, to);
        let plain = |s: &str| format!("{}Z", &s[..19]);
        match rng.below(24) {
            0 => rng.pick(&["1794816000000".to_owned(), "null".into(), "true".into(), "false".into(), "-0".into(), "1e2".into(),
                format!("{{\"x\":\"{mid}\"}}"), format!("[\"{mid}\"]"), format!("[ \"{mid}\" ]"), "{}".into(), "[]".into()]),
            1 => quote(&mid.chars().map(|c| u(c as u32)).collect::<String>()),
            // ms reads past what follows the seconds; the bounds still compare all of it.
            2 => quote(&format!("{mid}{}", u(0))),
            3 => quote(&format!("{mid}{}junk", u(0x1f))),
            4 => quote(&format!("{mid}{}", u(0xd800))),
            5 => quote(&format!("{}{mid}", u(0xdc00))),
            6 => quote(&format!("{mid}{BS}/x")),
            7 => quote(&format!("{mid}{BAD}")),
            8 => quote(&format!("{}{}{}", &mid[..5], u(0xd83d) + &u(0xde00), &mid[5..])),
            9 => quote(&format!("{}{}", u(0x32), &from[1..])),
            // At a bound, spellings that sort as text on one side of it and read as a time on the other.
            10..=12 => {
                let bound = rng.pick(&[from.to_owned(), to.to_owned(), iso(lo - 1000.), iso(hi - 1000.)]);
                let seconds = &bound[..19];
                let text = rng.pick(&[bound.replace('Z', "+01:00"), format!("{seconds},5Z"), format!("{seconds}.0001Z"), format!("{seconds}.9995Z"),
                    format!("{bound} "), format!("{seconds}.000"), format!("{seconds}.Z"), format!("{seconds}Z"), format!("{seconds}.0Z")]);
                quote(&encode(rng, &text))
            }
            _ => {
                let text = rng.pick(&[from.to_owned(), to.to_owned(), iso(lo - 1.), iso(hi - 1.), mid.clone(), mid.clone(), mid.clone(),
                    plain(&mid), plain(from), plain(to), plain(&iso(lo - 1000.)), plain(&iso(hi - 1000.)), mid.replace('Z', "+01:00"),
                    mid.replace('Z', "123Z"), format!("{}.Z", &mid[..19]), format!("{},5Z", &mid[..19]), format!("{}.5Z", &mid[..19]),
                    "yesterday".into(), String::new(), mid[..10].to_owned(), mid.replace('T', " "), "2026-13-45T99:99:99Z".into(),
                    "2026-11-1xT10:00:00Z".into(), format!("{mid}é"), format!("{mid} {SENTINEL}"), format!("+{}", &mid[1..]),
                    format!("{mid}\t"), mid.to_lowercase()]);
                quote(&encode(rng, &text))
            }
        }
    }

    const NAMES: [&str; 9] = ["Read", "Grep", "mcp__t3-code__delegate_task", "t3-code.delegate_task", "codex_apps.github.fetch",
        "x/y:z-w_v.u", "a", "View_Image", SENTINEL];
    const EXCLUDED: [&str; 37] = [" ", "\"", "\\", "'", ";", "$", "@", "#", "!", "*", "[", "]", "?", "=", "+", ",", "\x1f", "\x7f",
        "é", "😀", "\0", "~", "`", "|", "&", "(", ")", "{", "}", "<", ">", "%", "^", "\n", "\t", "\u{a0}", "\u{2028}"];

    fn tool_name(rng: &mut Rng) -> String {
        match rng.below(12) {
            0 => rng.pick(&["123", "null", "true", "{\"name\":\"Read\"}", "[\"Read\"]", "-0", "{}", "[]"]).to_owned(),
            1 => quote(&"a".repeat(79 + rng.below(3))),
            2 => quote(&u(0x41).repeat(79 + rng.below(3))),
            3 | 4 => {
                let (name, bad) = (rng.pick(&NAMES), rng.pick(&EXCLUDED));
                let at = rng.below(name.len() + 1);
                let text = format!("{}{bad}{}", &name[..at], &name[at..]);
                quote(&encode(rng, &text))
            }
            5 => quote(&format!("Re{}d", u(0x61))),
            6 => quote(&format!("mcp{BS}/x")),
            7 => quote(""),
            8 => quote(&format!("Read{}", u(0xd800))),
            9 => quote(&format!("Read{BAD}")),
            _ => { let name = rng.pick(&NAMES); quote(&encode(rng, name)) }
        }
    }

    const EXITS: [&str; 31] = ["-1", "0", "1", "2", "3", "124", "125", "126", "127", "128", "129", "255", "256", "-0", "1.0", "1e2", "1E2",
        "0.5", "-0.0", "9223372036854775807", "9223372036854775808", "-9223372036854775808", "-9223372036854775809",
        "99999999999999999999", "\"1\"", "null", "true", "false", "{}", "[]", "[1]"];

    const LINES: [&str; 18] = ["cargo test -p x", "cd app && git diff | tail -20", "bash -lc \"cargo test\"", "mkdir -p out && cargo build --release",
        "./run-checks.sh", "rg -n foo | head", "python3 - <<'PY'\nprint(1)\nPY", "echo 'x | rg y'", "sudo -E env A=1 timeout 60 make test",
        "cargo test -p x # never read", "./script 'never read'", "never-read-program arg", "", "x=$(git rev-parse HEAD); echo $x",
        "git status\t&& npm test", "cat \"a\\\"b\" | jq .", "printf '%s\\n' a", "cd /tmp/é && ls"];

    fn input(rng: &mut Rng, big: bool) -> String {
        match rng.below(16) {
            0 => rng.pick(&["5", "null", "true", "-1.5", "0"]).to_owned(),
            1 | 2 => rng.pick(&["[ \"cargo\" , \"test\" ]".to_owned(), "{\"cmd\" : \"cargo test\"}".into(), "[\"bash\",\"-lc\",\"cargo test\"]".into(),
                "[\"a b; cargo x\"]".into(), "[1, \"x\x01y\", { \"k\" : \"$(cargo test)\" }]".into(), format!("{{\"a\":\"{}\"}}", u(0x41)),
                "[]".into(), "{}".into(), "[\"cargo\",\n\t\"test\"]".into(), format!("[\"{}\"]", u(0xd800)), "[ ]".into(), "{ }".into(),
                format!("[\"x {BS}\" ; cargo test ; {BS}\"\"]"), format!("[1.0E+2, -0, \"{BS}/\", true, null, \"{BAD}\"]"),
                "[\"git\", \"status\" ]\t".into(), format!("{{\"never\":\"{SENTINEL}\",\"cmd\":[\"rg\", \"x\"]}}")]),
            3 => quote(&format!("cargo test{}", u(0xd800))),
            4 => quote(&format!("git status {}", u(0xd83d) + &u(0xde00))),
            5 if big => {
                let line = format!("python3 - <<'PY'\n{}\nPY", "x".repeat((1 << 20) + rng.below(1000)));
                quote(&encode(rng, &line))
            }
            6 => quote(&format!("cargo test {BAD}")),
            7 => quote("cargo test\x01"),
            _ => { let line = rng.pick(&LINES); quote(&encode(rng, line)) }
        }
    }

    fn big_string(rng: &mut Rng, big: bool, from: &str, to: &str) -> String {
        let size = match rng.below(200) {
            0 if big => (1 << 20) + rng.below(4096),
            1..=6 => 10_000 + rng.below(90_000),
            _ => rng.below(2000),
        };
        let mut s = String::from(SENTINEL);
        while s.len() < size {
            match rng.below(16) {
                0 => s.push_str(&format!("{BS}\"startedAt{BS}\":{BS}\"{}{BS}\",", mid(rng, from, to))),
                1 => s.push_str(&format!("{BS}{BS}")),
                2 => s.push_str(&format!("{BS}\"")),
                3 => s.push_str(&u(rng.below(0x10000) as u32)),
                4 => s.push_str(&format!("{BS}n{BS}t{BS}r{BS}b{BS}f{BS}/")),
                5 => s.push('é'),
                6 => s.push('\x01'),
                7 => s.push(BAD),
                8 => s.push_str("\"toolName\":"),
                _ => s.push_str(&"x".repeat(rng.below(size / 4 + 64) + 1)),
            }
        }
        match rng.below(4) {
            0 => s.push_str(&format!("{BS}{BS}")),
            1 => s.push_str(&format!("{BS}\"")),
            _ => {}
        }
        // A raw quote pushed above would end the string early: escape any that is not.
        quote(&escape_bare_quotes(&s))
    }

    fn escape_bare_quotes(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut escape = false;
        for c in s.chars() {
            if escape { escape = false; } else if c == '\\' { escape = true; } else if c == '"' { out.push('\\'); }
            out.push(c);
        }
        out
    }

    fn filler(rng: &mut Rng, big: bool, from: &str, to: &str, depth: usize) -> String {
        match rng.below(if depth == 0 { 7 } else { 11 }) {
            0..=4 => big_string(rng, big, from, to),
            5 => rng.pick(&["-0", "1e2", "1.0", "1E+2", "123456789012345678901234567890", "1e-999", "0.5e-3", "-12.5E-7", "0", "42"]).to_owned(),
            6 => rng.pick(&["true", "false", "null"]).to_owned(),
            7 => format!("{{ \"startedAt\" : \"{}\", \"toolName\":\"Read\", \"exitCode\":1, \"input\":\"cargo test\", \"k\": {} }}",
                mid(rng, from, to), filler(rng, false, from, to, depth - 1)),
            8 => format!("[{}, {{\"startedAt\":\"{}\"}} ,{}]", filler(rng, false, from, to, depth - 1), mid(rng, from, to), filler(rng, false, from, to, depth - 1)),
            9 => rng.pick(&["{}", "[]", "[[[[]]]]", "{\"a\":{\"b\":{\"c\":[]}}}", "[ { } , [ ] ]"]).to_owned(),
            _ => format!("{{\"input\":{}}}", input(rng, false)),
        }
    }

    fn ws(rng: &mut Rng) -> String {
        if rng.chance(0.7) { return String::new(); }
        (0..1 + rng.below(3)).map(|_| rng.pick(&[' ', '\t', '\n', '\r'])).collect()
    }

    fn key(rng: &mut Rng, name: &str) -> String {
        if rng.chance(0.08) && !name.is_empty() {
            let at = rng.below(name.len());
            quote(&format!("{}{}{}", &name[..at], u(u32::from(name.as_bytes()[at])), &name[at + 1..]))
        } else {
            quote(name)
        }
    }

    fn deep(levels: usize) -> String { format!("{}{}", "[".repeat(levels), "]".repeat(levels)) }

    // A turn item as T3 writes it: its keys in the schema's order, absent ones left out, the
    // bulk after the small fields. The wanted values still come from the edge-case pools.
    fn t3_members(rng: &mut Rng, kind: &'static str, from: &str, to: &str, big: bool) -> Vec<(String, String)> {
        let mut members = vec![];
        let base: [(&str, f64); 20] = [("deferredWork", 0.05), ("toolNonExecutionKind", 0.05), ("toolSurface", 0.3), ("toolIcon", 0.3),
            ("toolSource", 0.3), ("id", 1.), ("threadId", 1.), ("runId", 0.9), ("nodeId", 0.9), ("providerThreadId", 0.5),
            ("providerTurnId", 0.5), ("nativeItemRef", 0.3), ("parentItemId", 0.1), ("ordinal", 1.), ("status", 1.), ("title", 0.7),
            ("startedAt", 0.97), ("completedAt", 0.8), ("updatedAt", 1.), ("type", 1.)];
        let own: &[(&str, f64)] = match kind {
            "command_execution" => &[("input", 0.95), ("output", 0.9), ("outputOmitted", 0.2), ("outputIndicatesFailure", 0.2),
                ("exitCode", 0.6), ("monitor", 0.1), ("completionWake", 0.1)],
            "dynamic_tool" => &[("toolName", 0.95), ("viewedImagePath", 0.05), ("input", 0.9), ("output", 0.9), ("outputOmitted", 0.1)],
            "file_change" => &[("fileName", 0.9), ("additions", 0.8), ("deletions", 0.8), ("diffStr", 0.7), ("oldStr", 0.4), ("newStr", 0.4), ("changes", 0.5)],
            "file_search" => &[("pattern", 0.9), ("results", 0.9)],
            "web_search" => &[("patterns", 0.9), ("results", 0.9)],
            _ => &[],
        };
        for &(name, p) in base.iter().chain(own) {
            if !rng.chance(p) { continue; }
            let value = match name {
                "startedAt" | "completedAt" => timestamp(rng, from, to),
                "updatedAt" => quote(&mid(rng, from, to)),
                "type" => quote(kind),
                "toolName" => tool_name(rng),
                "exitCode" => rng.pick(&EXITS).to_owned(),
                "input" if kind == "command_execution" => input(rng, big),
                "ordinal" | "additions" | "deletions" => rng.below(500).to_string(),
                "deferredWork" | "outputOmitted" | "outputIndicatesFailure" => rng.pick(&["true", "false"]).to_owned(),
                "output" | "diffStr" | "oldStr" | "newStr" | "results" | "changes" | "input" => filler(rng, big, from, to, 2),
                "nativeItemRef" | "monitor" | "completionWake" => "{\"kind\":\"x\",\"id\":\"y\"}".into(),
                "title" | "pattern" | "patterns" | "fileName" | "viewedImagePath" => quote(&encode(rng, SENTINEL)),
                _ => quote(&format!("{name}-{}", rng.below(1000))),
            };
            members.push((quote(name), value));
        }
        members
    }

    // A key's name when it is written plainly in quotes.
    fn plain(raw: &str) -> Option<String> {
        let inner = raw.strip_prefix('"')?;
        let inner = &inner[..inner.find('"')?];
        (!inner.contains('\\')).then(|| inner.to_owned())
    }

    pub(crate) fn case(rng: &mut Rng, from: &str, to: &str, big: bool) -> Case {
        let kind = rng.pick(&KINDS);
        let mut notes = vec![];
        let t3 = rng.chance(0.4);
        let mut members: Vec<(String, String)> = if t3 { notes.push("T3's order"); t3_members(rng, kind, from, to, big) } else { vec![] };
        let wanted: &[(&str, f64)] = if t3 { &[] } else { &[("startedAt", 0.93), ("completedAt", 0.8), ("toolName", 0.6), ("exitCode", 0.6), ("input", 0.7)] };
        for &(name, p) in wanted {
            let n = if !rng.chance(p) { 0 } else if rng.chance(0.12) { 2 } else { 1 };
            for _ in 0..n {
                let value = match name {
                    "startedAt" | "completedAt" => timestamp(rng, from, to),
                    "toolName" => tool_name(rng),
                    "exitCode" => rng.pick(&EXITS).to_owned(),
                    _ => input(rng, big),
                };
                members.push((key(rng, name), value));
            }
        }
        for _ in 0..if t3 { 0 } else { rng.below(6) } {
            let name = rng.pick(&["output", "text", "title", "detail", "item", "result", "prompt", "message", "startedAtX", "started",
                "StartedAt", "toolname", "exit_code", "startedAt ", "Input", "", "never read"]);
            let value = filler(rng, big, from, to, 3);
            members.push((key(rng, name), value));
        }
        if !t3 { for i in (1..members.len()).rev() { members.swap(i, rng.below(i + 1)); } }

        // At most one fault of each kind, placed at a boundary between members.
        let (mut json5, mut malformed) = (false, false);
        let boundary = rng.below(members.len() + 1);
        let five = if rng.chance(0.07) { Some(rng.below(8)) } else { None };
        let fault = if rng.chance(0.07) { Some(rng.below(11)) } else { None };
        let oddity = if rng.chance(0.06) { Some(rng.below(3)) } else { None };
        match five {
            Some(0) if !members.is_empty() => { json5 = true; notes.push("json5: trailing comma") }
            Some(1) => { json5 = true; notes.push("json5: member") ; members.insert(boundary, (rng.pick(&["\"m\"", "m5", "'m'"]).into(),
                rng.pick(&["'single'".to_owned(), "0x1F".into(), "+1".into(), ".5".into(), "5.".into(), "Infinity".into(), "-Infinity".into(),
                    "NaN".into(), "[1,]".into(), "{\"a\":1,}".into(), format!("\"{BS}x41\""), format!("\"{BS}'\""), format!("\"{BS}v\""),
                    format!("\"{BS}0\""), format!("\"a{BS}\nb\""), "{a:1}".into()]))) }
            Some(2) => { json5 = true; notes.push("json5: bare key") ; members.insert(boundary, ("startedAt".into(), quote(&mid(rng, from, to)))) }
            _ => {}
        }
        match fault {
            Some(0) => { malformed = true; notes.push("malformed: raw NUL in a string"); members.insert(boundary, ("\"m\"".into(), "\"ab\0cd\"".into())) }
            Some(1) => { malformed = true; notes.push("malformed: bad escape"); members.insert(boundary, ("\"m\"".into(),
                rng.pick(&[format!("\"a{BS}qb\""), format!("\"{BS}u12G4\""), format!("\"{BS}u12\""), format!("\"x{BS}\"")]))) }
            Some(2) => { malformed = true; notes.push("malformed: no colon"); members.insert(boundary, ("\"m\" 1".into(), String::new())) }
            Some(3) => { malformed = true; notes.push("malformed: number"); members.insert(boundary, ("\"m\"".into(), rng.pick(&["01", "-01", "00", "-", "1e", "1e+", "--1", "1.2.3", "-a"]).into())) }
            Some(4) => { malformed = true; notes.push("malformed: literal"); members.insert(boundary, ("\"m\"".into(), rng.pick(&["tru", "nul", "True", "nulll", "truex", "fals"]).into())) }
            Some(5) => { malformed = true; notes.push("malformed: too deep"); members.insert(boundary, ("\"m\"".into(), deep(1000))) }
            Some(6) => { malformed = true; notes.push("malformed: unbalanced"); members.insert(boundary, ("\"m\"".into(), rng.pick(&["[1,2", "{\"a\":1", "[}", "{]", "[1 2]", "{\"c\" \"d\"}", "{\"c\":}", "[,]", "{,}"]).into())) }
            _ => {}
        }
        match oddity {
            Some(0) => { notes.push("depth 1000"); members.insert(boundary, ("\"m\"".into(), deep(999))) }
            _ => {}
        }

        let mut text = ws(rng);
        if five == Some(3) { json5 = true; notes.push("json5: leading BOM"); text.insert(0, '\u{feff}'); }
        text.push('{');
        for (i, (k, v)) in members.iter().enumerate() {
            if i > 0 {
                if fault == Some(7) && i == boundary.max(1) { malformed = true; notes.push("malformed: no comma"); text.push(' '); }
                else if fault == Some(8) && i == boundary.max(1) { malformed = true; notes.push("malformed: two commas"); text.push_str(",,"); }
                else { text += &ws(rng); text.push(','); }
            }
            if i == boundary {
                match five {
                    Some(4) => { json5 = true; notes.push("json5: comment"); text.push_str(&rng.pick(&["/* c */", "// c\n"])); }
                    Some(5) => { json5 = true; notes.push("json5: whitespace"); text.push(rng.pick(&['\x0b', '\x0c', '\u{a0}', '\u{2028}', '\u{feff}'])); }
                    _ => {}
                }
            }
            text += &ws(rng);
            text += k;
            if !v.is_empty() { text += &ws(rng); text.push(':'); text += &ws(rng); text += v; }
        }
        if five == Some(0) && !members.is_empty() { text.push(','); }
        text += &ws(rng);
        let close = text.len();
        text.push('}');
        text += &ws(rng);
        match (fault, oddity, five) {
            (Some(9), _, _) => { malformed = true; notes.push("malformed: after the end"); text.push_str(&rng.pick(&["x", "}", "]", "{}", "\"\""])); }
            (Some(10), _, _) => {
                malformed = true;
                notes.push("malformed: cut short");
                let mut at = 1 + rng.below(close);
                while !text.is_char_boundary(at) { at -= 1; }
                text.truncate(at);
            }
            (_, Some(1), _) => { notes.push("NUL after the end"); text.push_str("\0junk {"); }
            (_, _, Some(6)) => { json5 = true; notes.push("json5: comment after the end"); text.push_str("/**/"); }
            _ => {}
        }
        if oddity == Some(2) && !json5 && !malformed {
            notes.push("not an object");
            let inside = mid(rng, from, to);
            text = rng.pick(&[format!("[1,{{\"startedAt\":\"{inside}\"}}]"), "\"str\"".into(), "5".into(), "null".into(), "[]".into(), deep(1000), deep(1001)]);
            malformed = text == deep(1001);
        }
        let mut bytes = Vec::with_capacity(text.len());
        let bad = BAD.to_string();
        let mut rest = text.as_str();
        while let Some(at) = rest.find(BAD) {
            bytes.extend_from_slice(rest[..at].as_bytes());
            bytes.push(0xff);
            rest = &rest[at + bad.len()..];
        }
        bytes.extend_from_slice(rest.as_bytes());
        let payload = match rng.below(100) {
            0..=2 => { notes.push("stored as a BLOB"); Stored::Blob(bytes) }
            3 | 4 => Stored::Integer(rng.next() as i64 % 1000),
            5 => Stored::Real(1.5),
            6 | 7 => Stored::Null,
            _ => Stored::Text(bytes),
        };
        let keys = members.iter().map(|(key, _)| plain(key)).collect();
        Case { kind, payload, json5, malformed, notes, keys }
    }
}

#[cfg(test)]
mod tests {
    use super::{cases::*, *};
    use rusqlite::{params, types::Value};

    // An error is exactly one of the fixed sentences: no function's name, nothing of the payload.
    fn plain(error: &str) -> bool {
        Fault::RAISED.iter().any(|fault| error == fault.to_string()) && !error.contains("never") && !error.contains("t3_")
    }

    const FROM: &str = "2026-11-09T08:00:00.000Z";
    const TO: &str = "2026-11-16T08:00:00.000Z";
    const BS: &str = "\\";

    /// What `tools::read_calls` keeps of a row.
    #[derive(Clone, Debug, PartialEq)]
    struct Got { start: f64, end: Option<f64>, name: Option<String>, programs: Vec<&'static str>, exit: Option<i64> }

    fn bind(stored: &Stored) -> (Value, &'static str) {
        match stored {
            Stored::Text(b) => (Value::Blob(b.clone()), "CAST(?1 AS TEXT)"),
            Stored::Blob(b) => (Value::Blob(b.clone()), "?1"),
            Stored::Integer(n) => (Value::Integer(*n), "?1"),
            Stored::Real(f) => (Value::Real(*f), "?1"),
            Stored::Null => (Value::Null, "?1"),
        }
    }

    fn listed(words: &str) -> Vec<&'static str> { words.split_whitespace().filter_map(program::listed).collect() }

    // The `off` path's expressions, copied from `tools::queries`, over one bound payload; then
    // what `tools::read_calls` takes from them, the period by the start's instant. `tools.rs`
    // holds the same comparison through the reader.
    fn oracle(db: &Connection, case: &Case, from: &str, to: &str) -> Result<Option<Got>, String> {
        let (value, p) = bind(&case.payload);
        let sql = format!("SELECT json_extract(p,'$.startedAt'), json_extract(p,'$.completedAt'),
                CASE WHEN k='dynamic_tool' THEN CASE
                    WHEN typeof(json_extract(p,'$.toolName'))='text'
                     AND length(json_extract(p,'$.toolName')) BETWEEN 1 AND 80
                     AND instr(json_extract(p,'$.toolName'),char(0))=0
                     AND json_extract(p,'$.toolName') NOT GLOB '*[^A-Za-z0-9_.:/-]*'
                    THEN json_extract(p,'$.toolName') END END,
                CASE WHEN k='command_execution' THEN t3_programs(json_extract(p,'$.input')) END,
                CASE WHEN k='command_execution' AND json_type(p,'$.exitCode')='integer' THEN json_extract(p,'$.exitCode') END
             FROM (SELECT {p} AS p, ?2 AS k)");
        let mut q = db.prepare_cached(&sql).unwrap();
        let mut rows = q.query(params![value, case.kind]).map_err(|e| e.to_string())?;
        let Some(r) = rows.next().map_err(|e| e.to_string())? else { return Ok(None) };
        let text = |v: ValueRef<'_>| match v { ValueRef::Text(b) => stamp(b), _ => None };
        let (lo, hi) = (ms(from).unwrap(), ms(to).unwrap());
        let Some(start) = text(r.get_ref(0).unwrap()).filter(|&start| lo <= start && start < hi) else { return Ok(None) };
        let name: Option<String> = r.get(2).unwrap();
        let words: Option<String> = r.get(3).unwrap();
        let exit = match r.get_ref(4).unwrap() { ValueRef::Integer(n) => Some(n), _ => None };
        Ok(Some(Got { start, end: text(r.get_ref(1).unwrap()), name, programs: listed(words.as_deref().unwrap_or("")), exit }))
    }

    // The value `t3_call` gives, read back as the reader's skim handler reads it.
    fn parse(text: &str) -> Got {
        let f: Vec<&str> = text.split(SEPARATOR).collect();
        assert_eq!(f.len(), 5, "{text:?}");
        let some = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        Got { start: f[0].parse().unwrap(), end: some(f[1]).map(|s| s.parse().unwrap()), name: some(f[2]),
            exit: some(f[3]).map(|s| s.parse().unwrap()), programs: listed(f[4]) }
    }

    fn skimmed(db: &Connection, case: &Case, from: &str, to: &str) -> Result<Option<Got>, String> {
        let (value, p) = bind(&case.payload);
        let got: Option<String> = db.query_row(&format!("SELECT t3_call({p}, ?2, ?3, ?4)"), params![value, case.kind, ms(from).unwrap(), ms(to).unwrap()], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if let Some(text) = &got { assert!(!text.contains("never"), "{text:?}"); }
        Ok(got.as_deref().map(parse))
    }

    struct Three { oracle: Connection, full: Connection, early: Connection, unordered: Connection, times: Connection }

    impl Three {
        fn new() -> Three {
            let oracle = Connection::open_in_memory().unwrap();
            program::register(&oracle).unwrap();
            let open = || Connection::open_in_memory().unwrap();
            let (full, early, unordered) = (open(), open(), open());
            register(&full, Mode::Full).unwrap();
            register(&early, Mode::Early).unwrap();
            register_with(&unordered, Mode::Early, false).unwrap();
            let times = open();
            register_times(&times).unwrap();
            Three { oracle, full, early, unordered, times }
        }

        /// `t3_times` against `json_extract` of the two keys and `recap::ms`: the same bits, or
        /// both refused; a payload the skim refuses (JSON5, a BLOB) it must refuse.
        fn check_times(&self, case: &Case) -> Result<(), String> {
            let (value, p) = bind(&case.payload);
            let oracle = self.oracle.query_row(&format!("SELECT json_extract(p,'$.startedAt'), json_extract(p,'$.completedAt') FROM (SELECT {p} AS p)"),
                params![value.clone()], |r| {
                    let read = |v: ValueRef<'_>| match v { ValueRef::Text(b) => stamp(b).unwrap_or(f64::NAN), _ => f64::NAN };
                    Ok([read(r.get_ref(0)?), read(r.get_ref(1)?)].map(f64::to_bits))
                }).map_err(|e| e.to_string());
            let got = self.times.query_row(&format!("SELECT t3_times({p})"), params![value], |r| r.get::<_, Vec<u8>>(0)).map_err(|e| e.to_string());
            let got = got.map(|blob| {
                assert_eq!(blob.len(), 16);
                [0, 8].map(|at| u64::from_le_bytes(blob[at..at + 8].try_into().unwrap()))
            });
            if let Err(error) = &got {
                if ![Fault::NotStrict, Fault::NotText].iter().any(|f| *error == f.to_string()) { return Err(format!("t3_times: an error of its own: {error}")); }
            }
            let refused = case.json5 && matches!(case.payload, Stored::Text(_)) || matches!(case.payload, Stored::Blob(_));
            if refused {
                if got.is_ok() { return Err(format!("t3_times read what it must refuse: {got:?}")); }
            } else if oracle != got && !(oracle.is_err() && got.is_err()) {
                return Err(format!("t3_times {got:?}, json_extract {oracle:?}"));
            }
            Ok(())
        }

        /// The skim against `json_extract`: the same row or none in full mode, unless the
        /// payload is one the skim refuses (JSON5, a BLOB), which it must refuse; in early
        /// mode the same, except that a fault after where it stopped goes unseen, and, with
        /// T3's order assumed, except for a payload whose keys do not follow it. Whether that
        /// last exception was taken.
        fn check(&self, case: &Case, from: &str, to: &str) -> Result<bool, String> {
            let oracle = oracle(&self.oracle, case, from, to);
            let full = skimmed(&self.full, case, from, to);
            let early = skimmed(&self.early, case, from, to);
            let unordered = skimmed(&self.unordered, case, from, to);
            for error in [&full, &early, &unordered].into_iter().filter_map(|r| r.as_ref().err()) {
                if ![Fault::NotStrict, Fault::NotText].iter().any(|f| *error == f.to_string()) || error.contains("never") {
                    return Err(format!("an error of its own: {error}"));
                }
            }
            // The faults the generator planted matter only in text; a BLOB is refused whatever it holds.
            let text = matches!(case.payload, Stored::Text(_));
            let refused = case.json5 && text || matches!(case.payload, Stored::Blob(_));
            let malformed = case.malformed && text;
            if !refused && !malformed && oracle.is_err() { return Err(format!("the generator meant a valid payload: {oracle:?}")); }
            if malformed && oracle.is_ok() { return Err(format!("the generator meant a malformed payload: {oracle:?}")); }
            if refused {
                if full.is_ok() { return Err(format!("full read what it must refuse: {full:?}, json_extract {oracle:?}")); }
            } else if oracle.is_ok() != full.is_ok() || oracle.is_ok() && oracle != full {
                return Err(format!("full {full:?}, json_extract {oracle:?}"));
            }
            match (&oracle, &unordered) {
                (Ok(o), Ok(e)) if o != e => return Err(format!("early, without the order, {e:?}, json_extract {o:?}")),
                (Ok(_), Err(e)) if !refused => return Err(format!("early, without the order, refused a valid payload: {e}")),
                _ => {}
            }
            match (&oracle, &early) {
                (Ok(o), Ok(e)) if o != e && case.in_t3_order() => Err(format!("early {e:?}, json_extract {o:?}, keys in T3's order")),
                (Ok(o), Ok(e)) if o != e => Ok(true),
                (Ok(_), Err(e)) if !refused => Err(format!("early refused a valid payload: {e}")),
                _ => Ok(false),
            }
        }
    }

    /// The head functions against the full ones, over a row of a real table: a head read gives
    /// the full pass's value or early's on the whole payload, never a third; and the full
    /// pass's, exactly, on a payload in T3's order that the skim does not refuse. Whether it gave
    /// something other than the full pass's.
    struct Heads { db: Connection }

    impl Heads {
        fn new() -> Heads {
            let db = Connection::open_in_memory().unwrap();
            // No type on the column, so a number stays a number.
            db.execute_batch("CREATE TABLE orchestration_v2_projection_turn_items(type TEXT, payload_json)").unwrap();
            register(&db, Mode::Head).unwrap();
            register_times(&db).unwrap();
            Heads { db }
        }

        fn check(&self, case: &Case, from: &str, to: &str) -> Result<bool, String> {
            let (value, p) = bind(&case.payload);
            self.db.execute("DELETE FROM orchestration_v2_projection_turn_items", []).unwrap();
            self.db.execute(&format!("INSERT INTO orchestration_v2_projection_turn_items VALUES(?2, {p})"), params![value, case.kind]).unwrap();
            let (lo, hi) = (ms(from).unwrap(), ms(to).unwrap());
            let call = |sql: &str| self.db.query_row(&format!("SELECT {sql} FROM orchestration_v2_projection_turn_items"), params![lo, hi],
                |r| r.get::<_, Option<String>>(0)).map_err(|e| e.to_string()).map(|got| got.as_deref().map(parse));
            let times = |sql: &str| self.db.query_row(&format!("SELECT {sql} FROM orchestration_v2_projection_turn_items"), [],
                |r| r.get::<_, Vec<u8>>(0)).map_err(|e| e.to_string()).map(|b| [0, 8].map(|at| u64::from_le_bytes(b[at..at + 8].try_into().unwrap())));
            let full = call("t3_call(payload_json, type, ?1, ?2)");
            let head = call("t3_call_head(rowid, typeof(payload_json), type, ?1, ?2)");
            let times_full = times("t3_times(payload_json)");
            let times_head = times("t3_times_head(rowid, typeof(payload_json))");
            // Early on the whole payload, and what it gives where it is not text.
            let (early, times_early) = match &case.payload {
                Stored::Text(bytes) => (call_with(bytes, case.kind.as_bytes(), lo, hi, true, true, false).map(|got| got.as_deref().map(parse)).map_err(|f| f.to_string()),
                    times_with(bytes, true, false).map(|t| t.map(f64::to_bits)).map_err(|f| f.to_string())),
                _ => (full.clone(), times_full.clone()),
            };
            for error in [&head, &full].into_iter().filter_map(|r| r.as_ref().err()).chain([&times_head, &times_full].into_iter().filter_map(|r| r.as_ref().err())) {
                if !plain(error) { return Err(format!("an error of its own: {error}")); }
            }
            let same = |a: &Result<Option<Got>, String>, b: &Result<Option<Got>, String>| a == b || a.is_err() && b.is_err();
            let same_times = |a: &Result<[u64; 2], String>, b: &Result<[u64; 2], String>| a == b || a.is_err() && b.is_err();
            if !same(&head, &full) && !same(&head, &early) { return Err(format!("head {head:?}, full {full:?}, early {early:?}")); }
            if !same_times(&times_head, &times_full) && !same_times(&times_head, &times_early) {
                return Err(format!("times head {times_head:?}, full {times_full:?}, early {times_early:?}"));
            }
            let refused = case.json5 && matches!(case.payload, Stored::Text(_)) || matches!(case.payload, Stored::Blob(_));
            if case.in_t3_order() && !refused && full.is_ok() && (head != full || times_head != times_full) {
                return Err(format!("keys in T3's order: head {head:?} {times_head:?}, full {full:?} {times_full:?}"));
            }
            Ok(!same(&head, &full) || !same_times(&times_head, &times_full))
        }
    }

    fn run(seed: u64, count: usize, big: bool) {
        let three = Three::new();
        let heads = Heads::new();
        let mut rng = Rng(seed);
        let mut seen = [0usize; 4];
        let mut stats = std::collections::BTreeMap::<String, usize>::new();
        for i in 0..count {
            let case = case(&mut rng, FROM, TO, big);
            let misfire = three.check(&case, FROM, TO).unwrap_or_else(|why| panic!("seed {seed} case {i}: {why}\n{}", case.describe()));
            three.check_times(&case).unwrap_or_else(|why| panic!("seed {seed} case {i}: {why}\n{}", case.describe()));
            let head_differs = heads.check(&case, FROM, TO).unwrap_or_else(|why| panic!("seed {seed} case {i}: {why}\n{}", case.describe()));
            seen[0] += usize::from(case.json5);
            seen[1] += usize::from(case.malformed);
            seen[2] += usize::from(oracle(&three.oracle, &case, FROM, TO).is_ok_and(|r| r.is_some()));
            seen[3] += 1;
            if std::env::var_os("SKIM_STATS").is_some() {
                let r = oracle(&three.oracle, &case, FROM, TO);
                *stats.entry(format!("oracle {}", match &r { Err(_) => "error", Ok(None) => "no row", Ok(Some(_)) => "row" })).or_insert(0) += 1;
                if let Ok(Some(g)) = &r {
                    for (label, on) in [("end", g.end.is_some()), ("name", g.name.is_some()), ("exit", g.exit.is_some()), ("programs", !g.programs.is_empty())] {
                        if on { *stats.entry(format!("row has {label}")).or_insert(0) += 1; }
                    }
                }
                if misfire { *stats.entry("early misled by an order T3 does not write".into()).or_insert(0) += 1; }
                if head_differs { *stats.entry("head gave early's value, not the full pass's".into()).or_insert(0) += 1; }
                if case.in_t3_order() { *stats.entry("keys in T3's order".into()).or_insert(0) += 1; }
                for note in &case.notes { *stats.entry((*note).to_owned()).or_insert(0) += 1; }
                *stats.entry(format!("stored {}", match case.payload { Stored::Text(_) => "text", Stored::Blob(_) => "blob", _ => "other" })).or_insert(0) += 1;
                if let Stored::Text(b) = &case.payload { *stats.entry("bytes".into()).or_insert(0) += b.len(); }
            }
        }
        if std::env::var_os("SKIM_STATS").is_some() { eprintln!("{stats:#?}"); }
        // The draw reaches each outcome often.
        assert!(seen[0] * 40 > count && seen[1] * 40 > count && seen[2] * 4 > count, "{seen:?}");
    }

    #[test]
    fn the_skim_reads_what_json_extract_reads() { run(20261118, 20_000, false); }

    #[test]
    #[ignore = "a long run: cargo test --release -p t3-agents-core -- --ignored skim"]
    fn the_skim_reads_what_json_extract_reads_long() {
        for seed in 1..=20 { run(seed, 5000, true); }
    }

    // The cases the probe of SQLite found, one by one.
    #[test]
    fn what_sqlite_does_case_by_case() {
        let three = Three::new();
        let mid = "2026-11-12T10:00:00.000Z";
        let out = "2026-11-17T10:00:00.000Z";
        let s = |text: &str| format!("{{\"startedAt\":\"{mid}\",{text}}}");
        let valid = [
            format!("{{\"startedAt\":\"{mid}\",\"startedAt\":\"{out}\"}}"),
            format!("{{\"startedAt\":\"{out}\",\"startedAt\":\"{mid}\"}}"),
            format!("{{\"start{}dAt\":\"{mid}\",\"startedAt\":\"{out}\"}}", u(0x65)),
            format!("{{\"{}tartedAt\":\"{mid}\"}}", u(0x53)),
            s(&format!("\"m\":{}", "[".repeat(999) + &"]".repeat(999))),
            s(&format!("\"m\":{}1{}", "{\"c\":".repeat(999), "}".repeat(999))),
            "[".repeat(1000) + &"]".repeat(1000),
            s("\"m\":1}\0x"),
            format!("{{\"startedAt\":\"{mid}\"}} \0 garbage"),
            s("\"m\":\"x\x01\x1f\x7f\ty\""),
            s(&format!("\"completedAt\":\"{mid}{}\"", u(0))),
            s(&format!("\"completedAt\":\"{}\"", mid.replace('Z', &u(0xd800)))),
            format!("{{\"startedAt\":\"{mid}\",\"toolName\":\"{}\"}}", u(0x41).repeat(80)),
            format!("{{\"startedAt\":\"{mid}\",\"toolName\":\"Re{}ad\"}}", u(0)),
            format!("{{\"startedAt\":\"{mid}\",\"input\":[ 1 , \"a b\" , {{\"x\" : \"{}\"}}, 1.0E+2, -0, \"{BS}/\" ,true, null] }}", u(0x41)),
            format!("{{\"startedAt\":\"{mid}\",\"input\":\"cargo test {}\"}}", u(0xd800)),
            " \t\r\n{}\n\t ".into(), "[1]".into(), "\"s\"".into(), "5".into(),
        ];
        let json5 = ["{a:1}", "{'a':1}", "{\"a\":1,}", "{\"a\":1 /*c*/}", "{\"a\":1 //c\n}", "{\"a\":0x1F}", "{\"a\":+1}", "{\"a\":.5}",
            "{\"a\":5.}", "{\"a\":Infinity}", "{\"a\":-Infinity}", "{\"a\":NaN}", "{\x0b\"a\":1}", "{\x0c\"a\":1}", "{\u{a0}\"a\":1}",
            "{\u{feff}\"a\":1}", "\u{feff}{\"a\":1}", "{\u{2028}\"a\":1}", "{\"a\":1}\x0b", "{\"a\":1}/**/", "{\"a\":[1,]}"]
            .map(|text| text.replace("\"a\"", "\"startedAt\"").replace("{a:", "{startedAt:").replace("'a'", "'startedAt'"));
        let json5_escapes = ["x41", "'", "v", "0", "\n"].map(|e| s(&format!("\"m\":\"{BS}{e}\"")));
        let malformed = [
            s(&format!("\"m\":{}", "[".repeat(1000) + &"]".repeat(1000))),
            "[".repeat(1001) + &"]".repeat(1001),
            "\0{\"a\":1}".into(), s("\"m\":\"x\0y\""), s(&format!("\"m\":\"{BS}q\"")), s(&format!("\"m\":\"{BS}u12\"")),
            s("\"m\":01"), s("\"m\":-01"), s("\"m\":-"), s("\"m\":1e"), s("\"m\":00"), s("\"m\":1} x"), s("\"m\":tru"),
            s("\"m\":True"), s("\"m\" 1"), "{,\"a\":1}".into(), String::new(), "   ".into(), s("\"m\":\"x"), s("\"m\":1,,\"n\":2"),
        ];
        let numbers = EXITS_FOR_TEST.map(|n| format!("{{\"startedAt\":\"{mid}\",\"exitCode\":{n}}}"));
        let groups: [(&[String], bool, bool); 5] = [(&valid, false, false), (&json5, true, false), (&json5_escapes, true, false),
            (&malformed, false, true), (&numbers, false, false)];
        for (texts, json5, malformed) in groups {
            for text in texts {
                for kind in ["command_execution", "dynamic_tool", "file_change"] {
                    let case = Case::new(kind, text, json5, malformed, "fixed");
                    if let Err(why) = three.check(&case, FROM, TO).and_then(|_| three.check_times(&case)) { panic!("{why}\n{}", case.describe()); }
                }
            }
        }
        // At the bounds, by instant: every spelling `ms` reads as the first instant is in, every one
        // it reads as the end is out (it reads no offset, rounds the fraction, stops at what is not one).
        for (start, counted) in [(FROM, true), (TO, false), ("2026-11-09T07:59:59.999Z", false), ("2026-11-16T07:59:59.999Z", true),
            ("2026-11-09T08:00:00Z", true), ("2026-11-16T08:00:00Z", false), ("2026-11-16T08:00:00.000+01:00", false),
            ("2026-11-16T08:00:00.0001Z", false), ("2026-11-16T07:59:59.9995Z", false), ("2026-11-09T08:00:00,5Z", true),
            ("2026-11-09T08:00:00.000", true), ("2026-11-09T09:00:00.000+01:00", true), ("2026-11-09T07:59:59.9996Z", true)] {
            let case = Case::new("file_change", &format!("{{\"startedAt\":\"{start}\"}}"), false, false, "bound");
            three.check(&case, FROM, TO).unwrap();
            assert_eq!(skimmed(&three.full, &case, FROM, TO).unwrap().is_some(), counted, "{start}");
        }
    }

    // What early assumes from T3's order, where that stops it, and where the assumption fails.
    #[test]
    fn the_order_early_assumes() {
        let mid = "2026-11-12T10:00:00.000Z";
        let get = |mode: Mode, kind: &str, text: &str| call(text.as_bytes(), kind.as_bytes(), ms(FROM).unwrap(), ms(TO).unwrap(), mode).map(|got| got.as_deref().map(parse));
        let bulk = format!("\"{SENTINEL}{}\"", "x".repeat(1000));
        // It stops before the bulk: a fault there goes unseen, so it was never read.
        for (kind, text) in [
            ("dynamic_tool", format!("{{\"id\":\"i\",\"status\":\"completed\",\"startedAt\":\"{mid}\",\"completedAt\":\"{mid}\",\"updatedAt\":\"{mid}\",\"type\":\"dynamic_tool\",\"toolName\":\"Read\",\"input\":{{}},\"output\":{bulk},BROKEN")),
            // An open call has no completedAt: `updatedAt` says it is not coming.
            ("file_change", format!("{{\"id\":\"i\",\"startedAt\":\"{mid}\",\"updatedAt\":\"{mid}\",\"type\":\"file_change\",\"diffStr\":{bulk},BROKEN")),
            // A dynamic tool without a name: `input` says it is not coming.
            ("dynamic_tool", format!("{{\"startedAt\":\"{mid}\",\"completedAt\":\"{mid}\",\"type\":\"dynamic_tool\",\"input\":BROKEN")),
        ] {
            assert!(get(Mode::Full, kind, &text).is_err());
            assert!(get(Mode::Early, kind, &text).unwrap().is_some(), "{text}");
        }
        // A command's exit code is after its output: early reads to the end unless the order rules it out.
        let command = format!("{{\"startedAt\":\"{mid}\",\"completedAt\":\"{mid}\",\"type\":\"command_execution\",\"input\":\"cargo test\",\"output\":{bulk},BROKEN");
        assert!(get(Mode::Early, "command_execution", &command).is_err());
        let monitored = format!("{{\"startedAt\":\"{mid}\",\"type\":\"command_execution\",\"input\":\"cargo test\",\"output\":{bulk},\"monitor\":BROKEN");
        assert_eq!(get(Mode::Early, "command_execution", &monitored).unwrap().unwrap().programs, ["cargo"]);
        // Out of the period, it stops at startedAt.
        assert_eq!(get(Mode::Early, "command_execution", "{\"startedAt\":\"2026-12-01T00:00:00.000Z\",BROKEN").unwrap(), None);
        // A key T3 does not write, one out of its place, one escaped, or one repeated, turns the
        // rule off: the late completedAt is found.
        for (before, after) in [("", "\"text\":1,"), ("", "\"id\":\"i\","), ("", "\"update\\u0064At\":1,"), ("\"status\":1,\"status\":1,", "")] {
            let text = format!("{{{before}\"startedAt\":\"{mid}\",{after}\"updatedAt\":\"{mid}\",\"completedAt\":\"{mid}\"}}");
            let early = get(Mode::Early, "file_change", &text).unwrap().unwrap();
            assert_eq!(early, get(Mode::Full, "file_change", &text).unwrap().unwrap(), "{text}");
            assert!(early.end.is_some(), "{text}");
        }
        // A payload that opens with `type` (as the independent check writes them) never turns the
        // rule on: `startedAt` was not read first.
        for kind in ["file_change", "dynamic_tool", "command_execution"] {
            let text = format!("{{\"type\":\"completed\",\"status\":\"x\",\"startedAt\":\"{mid}\",\"completedAt\":\"{mid}\",\"toolName\":\"Read\",\"input\":\"cargo test\",\"exitCode\":0}}");
            assert_eq!(get(Mode::Early, kind, &text).unwrap(), get(Mode::Full, kind, &text).unwrap(), "{kind}");
            let text = format!("{{\"updatedAt\":\"{mid}\",\"startedAt\":\"{mid}\",\"completedAt\":\"{mid}\"}}");
            assert!(get(Mode::Early, kind, &text).unwrap().unwrap().end.is_some(), "{kind}");
        }
        // The limit: a payload that keeps T3's order until early stops and breaks it after.
        let late = format!("{{\"startedAt\":\"{mid}\",\"updatedAt\":\"{mid}\",\"completedAt\":\"{mid}\"}}");
        assert!(get(Mode::Full, "file_change", &late).unwrap().unwrap().end.is_some());
        assert_eq!(get(Mode::Early, "file_change", &late).unwrap().unwrap().end, None);
    }

    // What the function costs apart from loading the rows, on a lab built under checks/generated:
    // T3_AGENTS_SKIM_BENCH_DB=checks/generated/tools-lab/week.sqlite T3CODE_HOME=<empty folder> \
    //   cargo test --release --locked -p t3-agents-core --lib -- --ignored skim_cost --nocapture
    #[test]
    #[ignore = "a measurement on a lab database; see the comment"]
    fn skim_cost() {
        use std::time::Instant;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../checks/generated").canonicalize().unwrap();
        let path = std::path::PathBuf::from(std::env::var("T3_AGENTS_SKIM_BENCH_DB").expect("T3_AGENTS_SKIM_BENCH_DB")).canonicalize().unwrap();
        assert!(path.starts_with(&root), "only a lab under {root:?}");
        let trials: usize = std::env::var("T3_AGENTS_SKIM_TRIALS").ok().and_then(|n| n.parse().ok()).unwrap_or(7);
        let open = || {
            let db = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            db.execute_batch("PRAGMA query_only=ON").unwrap();
            program::register(&db).unwrap();
            db
        };
        const TYPES: &str = "type IN ('command_execution','dynamic_tool','file_change','file_search','web_search')";
        let (from, to) = (ms(FROM).unwrap(), ms(TO).unwrap());
        let rows: Vec<(String, Vec<u8>)> = open().prepare(&format!("SELECT type, CAST(payload_json AS BLOB) FROM orchestration_v2_projection_turn_items WHERE {TYPES}"))
            .unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
        let bytes: usize = rows.iter().map(|(_, p)| p.len()).sum();
        eprintln!("{} tool rows, {bytes} payload bytes, sqlite {}", rows.len(), rusqlite::version());

        let mut runs: Vec<(&str, Box<dyn FnMut() -> usize>)> = vec![];
        // (a) The function alone, on payloads in memory.
        for (label, mode) in [("a. skim full, in memory", Mode::Full), ("a. skim early, in memory", Mode::Early)] {
            let rows = &rows;
            runs.push((label, Box::new(move || rows.iter().filter(|(kind, p)| call(p, kind.as_bytes(), from, to, mode).unwrap().is_some()).count())));
        }
        // (b) The same values bound one at a time: binding alone, today's expressions, t3_call.
        let memory = |mode: Option<Mode>| {
            let db = Connection::open_in_memory().unwrap();
            program::register(&db).unwrap();
            if let Some(mode) = mode { register(&db, mode).unwrap(); }
            db
        };
        let today = "SELECT json_extract(p,'$.startedAt'), json_extract(p,'$.completedAt'),
                CASE WHEN k='dynamic_tool' THEN CASE
                    WHEN typeof(json_extract(p,'$.toolName'))='text'
                     AND length(json_extract(p,'$.toolName')) BETWEEN 1 AND 80
                     AND instr(json_extract(p,'$.toolName'),char(0))=0
                     AND json_extract(p,'$.toolName') NOT GLOB '*[^A-Za-z0-9_.:/-]*'
                    THEN json_extract(p,'$.toolName') END END,
                CASE WHEN k='command_execution' THEN t3_programs(json_extract(p,'$.input')) END,
                CASE WHEN k='command_execution' AND json_type(p,'$.exitCode')='integer' THEN json_extract(p,'$.exitCode') END
             FROM (SELECT ?1 AS p, ?2 AS k) WHERE ?3 < ?4";
        for (label, mode, sql) in [("b. bound, binding only", None, "SELECT typeof(?1), ?2, ?3, ?4"), ("b. bound, json_extract today", None, today),
            ("b. bound, t3_call full", Some(Mode::Full), "SELECT t3_call(?1, ?2, ?3, ?4)"), ("b. bound, t3_call early", Some(Mode::Early), "SELECT t3_call(?1, ?2, ?3, ?4)")] {
            let db = memory(mode);
            let rows = &rows;
            runs.push((label, Box::new(move || {
                let mut q = db.prepare_cached(sql).unwrap();
                rows.iter().map(|(kind, p)| q.query(params![rusqlite::types::ToSqlOutput::Borrowed(ValueRef::Text(p)), kind, from, to]).unwrap().next().unwrap().map_or(0, |r| usize::from(!matches!(r.get_ref(0).unwrap(), ValueRef::Null)))).sum()
            })));
        }
        // (c) In the file: finding the rows, loading the payloads, today's call query, the skim's.
        let items = "orchestration_v2_projection_turn_items";
        let today_file = format!("SELECT type, status, json_extract(payload_json,'$.startedAt'), json_extract(payload_json,'$.completedAt'),
                CASE WHEN type='dynamic_tool' THEN CASE
                    WHEN typeof(json_extract(payload_json,'$.toolName'))='text'
                     AND length(json_extract(payload_json,'$.toolName')) BETWEEN 1 AND 80
                     AND instr(json_extract(payload_json,'$.toolName'),char(0))=0
                     AND json_extract(payload_json,'$.toolName') NOT GLOB '*[^A-Za-z0-9_.:/-]*'
                    THEN json_extract(payload_json,'$.toolName') END END,
                CASE WHEN type='command_execution' THEN t3_programs(json_extract(payload_json,'$.input')) END,
                CASE WHEN type='command_execution' AND json_type(payload_json,'$.exitCode')='integer' THEN json_extract(payload_json,'$.exitCode') END
             FROM {items} WHERE {TYPES} AND ?1 < ?2");
        for (label, mode, sql) in [
            ("c. file, type and status", None, format!("SELECT type, status FROM {items} WHERE {TYPES} AND ?1 < ?2")),
            ("c. file, and length(payload)", None, format!("SELECT type, status, length(payload_json) FROM {items} WHERE {TYPES} AND ?1 < ?2")),
            // Loads each payload into a function that reads none of its bytes: the cost of loading alone.
            ("c. file, loaded, no byte read", None, format!("SELECT type, status, t3_loaded(payload_json) FROM {items} WHERE {TYPES} AND ?1 < ?2")),
            ("c. file, json_extract today", None, today_file),
            ("c. file, t3_call full", Some(Mode::Full), format!("SELECT type, status, t3_call(payload_json, type, ?1, ?2) FROM {items} WHERE {TYPES}")),
            ("c. file, t3_call early", Some(Mode::Early), format!("SELECT type, status, t3_call(payload_json, type, ?1, ?2) FROM {items} WHERE {TYPES}")),
        ] {
            let db = open();
            if let Some(mode) = mode { register(&db, mode).unwrap(); }
            db.create_scalar_function("t3_loaded", 1, FunctionFlags::SQLITE_UTF8, |ctx| Ok(match ctx.get_raw(0) {
                ValueRef::Text(bytes) => bytes.len() as i64, _ => 0 })).unwrap();
            runs.push((label, Box::new(move || {
                let mut q = db.prepare_cached(&sql).unwrap();
                let mut rows = q.query(params![from, to]).unwrap();
                let mut n = 0;
                while let Some(r) = rows.next().unwrap() { n += usize::from(!matches!(r.get_ref(r.as_ref().column_count() - 1).unwrap(), ValueRef::Null)); }
                n
            })));
        }
        for (label, run) in runs.iter_mut() { eprintln!("warmup {label}: {} rows", run()); }
        let mut times: Vec<Vec<f64>> = vec![vec![]; runs.len()];
        for trial in 0..trials {
            let order: Vec<usize> = if trial % 2 == 0 { (0..runs.len()).collect() } else { (0..runs.len()).rev().collect() };
            for i in order {
                let started = Instant::now();
                std::hint::black_box((runs[i].1)());
                times[i].push(started.elapsed().as_secs_f64());
            }
        }
        for ((label, _), t) in runs.iter().zip(&times) {
            let mut sorted = t.clone();
            sorted.sort_by(f64::total_cmp);
            let median = if sorted.len() % 2 == 1 { sorted[sorted.len() / 2] } else { (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2. };
            eprintln!("{label:<34} median {:.4} s  range {:.4}..{:.4}  trials {:?}", median, sorted[0], sorted[sorted.len() - 1],
                t.iter().map(|x| (x * 10000.).round() / 10000.).collect::<Vec<_>>());
        }
    }

    #[test]
    fn the_variable_means_one_thing() {
        assert_eq!(choice(Some("off")), None);
        assert_eq!(choice(Some("early")), Some(Mode::Early));
        for value in [None, Some(""), Some("full"), Some("FULL"), Some("1"), Some("on"), Some("early "), Some("Off")] {
            assert_eq!(choice(value), Some(Mode::Full), "{value:?}");
        }
    }

    // t3_times: its layout, NaN where a time is not a readable string, and the difference from
    // Recap's present read (a number there fails Recap's read; here it is absent).
    #[test]
    fn the_two_times() {
        let db = Connection::open_in_memory().unwrap();
        register_times(&db).unwrap();
        let get = |sql: &str| -> [f64; 2] {
            let blob: Vec<u8> = db.query_row(&format!("SELECT t3_times({sql})"), [], |r| r.get(0)).unwrap();
            assert_eq!(blob.len(), 16, "{sql}");
            [0, 8].map(|at| f64::from_le_bytes(blob[at..at + 8].try_into().unwrap()))
        };
        let both = get("'{\"completedAt\":\"2026-11-12T10:00:01.500Z\",\"startedAt\":\"2026-11-12T10:00:00.000Z\"}'");
        assert_eq!(both, [ms("2026-11-12T10:00:00.000Z").unwrap(), ms("2026-11-12T10:00:01.500Z").unwrap()]);
        for sql in ["NULL", "42", "1.5", "'[]'", "'{}'", "'{\"startedAt\":1794816000000,\"completedAt\":null}'",
            "'{\"startedAt\":\"yesterday\",\"completedAt\":[\"2026-11-12T10:00:00.000Z\"]}'", "'{\"x\":{\"startedAt\":\"2026-11-12T10:00:00.000Z\"}}'"] {
            assert!(get(sql).iter().all(|t| t.is_nan()), "{sql}");
        }
        assert_eq!(times_blob([f64::NAN; 2]), [0, 0, 0, 0, 0, 0, 0xf8, 0x7f, 0, 0, 0, 0, 0, 0, 0xf8, 0x7f]);
        for sql in ["x'7b7d'", "''", "'{\"startedAt\":\"2026-11-12T10:00:00.000Z\",''never read'':1}'", "'{\"a\":\"never read'"] {
            let error = db.query_row(&format!("SELECT t3_times({sql})"), [], |r| r.get::<_, Vec<u8>>(0)).unwrap_err().to_string();
            assert!(plain(&error), "{error}");
        }
    }

    // An object from members in order, and its keys as `Case::in_t3_order` reads them.
    fn object(kind: &'static str, members: &[(&str, String)], note: &'static str) -> Case {
        let text = format!("{{{}}}", members.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect::<Vec<_>>().join(","));
        let mut case = Case::new(kind, &text, false, false, note);
        case.keys = members.iter().map(|(k, _)| Some((*k).to_owned())).collect();
        case
    }

    // T3's order for a row, with `title` (a base field before both times) of any text.
    fn t3_row(kind: &'static str, title: &str, completed: bool, bulk: &str) -> Vec<(&'static str, String)> {
        let q = |s: &str| format!("\"{s}\"");
        let mut members = vec![("id", q("i")), ("threadId", q("t")), ("status", q("completed")), ("title", q(title)),
            ("startedAt", q("2026-11-12T10:00:00.000Z"))];
        if completed { members.push(("completedAt", q("2026-11-12T10:00:02.500Z"))); }
        members.extend([("updatedAt", q("2026-11-12T10:00:03.000Z")), ("type", q(kind))]);
        match kind {
            "dynamic_tool" => members.extend([("toolName", q("mcp__t3-code__delegate_task")), ("input", "{}".into()), ("output", q(bulk))]),
            "command_execution" => members.extend([("input", q("cargo test -p x")), ("output", q(bulk)), ("exitCode", "124".into())]),
            "file_change" => members.extend([("fileName", q("never read")), ("diffStr", q(bulk))]),
            _ => members.extend([("results", q(bulk))]),
        }
        members
    }

    // The cases built for head reads: keys past every step, absent, repeated late; a large input
    // before toolName; a character, an escape, a key, a value or a number across each step's end;
    // a value of one step or less; rows that hold no text; a malformed tail past the stop.
    #[test]
    fn head_reads_at_every_step() {
        let heads = Heads::new();
        let kinds = ["dynamic_tool", "file_change", "command_execution", "web_search"];
        let (lo, hi) = (ms(FROM).unwrap(), ms(TO).unwrap());
        let mut cases: Vec<(Case, &str)> = vec![];
        for kind in kinds {
            for size in [0, 1500, 2000, 2040, 2100, 8000, 8300, 33_000, 140_000, 700_000] {
                for completed in [true, false] {
                    cases.push((object(kind, &t3_row(kind, &"x".repeat(size), completed, &"o".repeat(50_000)), "keys past a step"), "full"));
                }
            }
            // No startedAt; no toolName.
            let mut members = t3_row(kind, "t", true, &"o".repeat(9000));
            members.retain(|(k, _)| *k != "startedAt");
            cases.push((object(kind, &members, "no startedAt"), "full"));
            let mut members = t3_row(kind, "t", true, &"o".repeat(9000));
            members.retain(|(k, _)| *k != "toolName");
            cases.push((object(kind, &members, "no toolName"), "full"));
            // Repeated late, after the bulk: the first wins in both.
            let mut members = t3_row(kind, "t", true, &"o".repeat(9000));
            members.extend([("startedAt", "\"2027-01-01T00:00:00.000Z\"".into()), ("toolName", "\"Late\"".into()), ("completedAt", "\"2027-01-01T00:00:00.000Z\"".into())]);
            cases.push((object(kind, &members, "repeated late"), "full"));
            // A malformed tail past the stop: the head does not see it (command rows are read whole).
            let text = format!("{}BROKEN", object(kind, &t3_row(kind, "t", true, &"o".repeat(9000)), "").describe_text());
            cases.push((Case::new(kind, &text, false, true, "malformed tail"), if kind == "command_execution" { "error" } else { "early" }));
            // A value of one step or less, and just over.
            for size in [10, 1700, 1750] {
                cases.push((object(kind, &t3_row(kind, "t", true, &"o".repeat(size)), "short"), "full"));
            }
        }
        // A large input before toolName: out of T3's order, so the head takes toolName for absent,
        // as early does, where the full pass finds it.
        let q = |s: &str| format!("\"{s}\"");
        let members = vec![("id", q("i")), ("startedAt", q("2026-11-12T10:00:00.000Z")), ("completedAt", q("2026-11-12T10:00:02.500Z")),
            ("updatedAt", q("2026-11-12T10:00:03.000Z")), ("type", q("dynamic_tool")), ("input", q(&"i".repeat(100_000))), ("toolName", q("Read"))];
        let mut late_name = object("dynamic_tool", &members, "100 KB input before toolName");
        late_name.keys = vec![None];
        cases.push((late_name, "early, not full"));
        // Across each step's end: a character, an escape, the startedAt key, its value, and (for a
        // command) the exit code's digits.
        let specials = ["é".to_owned(), "😀".into(), u(0xe9), u(0xd83d) + &u(0xde00), format!("{BS}\""), format!("{BS}{BS}"), format!("{BS}n")];
        for step in [FIRST_STEP, FIRST_STEP * 4, FIRST_STEP * 16, FIRST_STEP * 64] {
            for kind in ["dynamic_tool", "file_change"] {
                let head = format!("{{\"id\":\"i\",\"threadId\":\"t\",\"status\":\"completed\",\"title\":\"");
                for special in &specials {
                    for k in 1..special.len() {
                        let title = format!("{}{special}{}", "x".repeat(step - k - head.len()), "y".repeat(20));
                        cases.push((object(kind, &t3_row(kind, &title, true, &"o".repeat(9000)), "a character or an escape across a step"), "full"));
                    }
                }
                // The key, then the start's value, across the step.
                let before = format!("{head}{{T}}\",");
                for k in 1..30 {
                    let title = "x".repeat(step - k - before.len() + 3);
                    cases.push((object(kind, &t3_row(kind, &title, true, &"o".repeat(9000)), "startedAt across a step"), "full"));
                }
            }
            let output_at = format!("{{\"startedAt\":\"2026-11-12T10:00:00.000Z\",\"completedAt\":\"2026-11-12T10:00:02.500Z\",\"type\":\"command_execution\",\"input\":\"cargo\",\"output\":\"");
            for k in 1..5 {
                let pad = step - k - output_at.len() - "\",\"exitCode\":".len();
                let text = format!("{output_at}{}\",\"exitCode\":1234}}", "o".repeat(pad));
                let mut case = Case::new("command_execution", &text, false, false, "an exit code across a step");
                case.keys = ["startedAt", "completedAt", "type", "input", "output", "exitCode"].map(|k| Some(k.to_owned())).to_vec();
                cases.push((case, "full"));
            }
        }
        // Rows that hold no text, and one that is not an object.
        for payload in [Stored::Null, Stored::Integer(7), Stored::Real(1.5), Stored::Blob(b"{\"startedAt\":\"2026-11-12T10:00:00.000Z\"}".to_vec())] {
            cases.push((Case { kind: "file_change", payload, json5: false, malformed: false, notes: vec!["no text"], keys: vec![None] }, "full"));
        }
        cases.push((Case::new("file_change", &format!("[{}]", "1,".repeat(5000) + "1"), false, false, "an array"), "full"));

        for (case, expect) in &cases {
            heads.check(case, FROM, TO).unwrap_or_else(|why| panic!("{why}\n{}", case.describe()));
            let Stored::Text(bytes) = &case.payload else { continue };
            let full = call_with(bytes, case.kind.as_bytes(), lo, hi, false, true, false);
            let head = heads.db.query_row("SELECT t3_call_head(rowid, typeof(payload_json), type, ?1, ?2) FROM orchestration_v2_projection_turn_items",
                params![lo, hi], |r| r.get::<_, Option<String>>(0)).map_err(|e| e.to_string());
            let early = call_with(bytes, case.kind.as_bytes(), lo, hi, true, true, false);
            match *expect {
                "full" => assert_eq!(head, full.clone().map_err(|f| f.to_string()), "{}", case.describe()),
                "early" => { assert!(full.is_err(), "{}", case.describe()); assert_eq!(head.as_ref().ok(), early.as_ref().ok(), "{}", case.describe()); }
                "early, not full" => { assert_eq!(head.as_ref().ok(), early.as_ref().ok()); assert_ne!(head.as_ref().ok(), full.as_ref().ok()); }
                _ => assert!(head.is_err() && full.is_err(), "{}", case.describe()),
            }
        }
    }

    // The head-reading functions' own cost against json_extract and the whole-value functions,
    // over every item of a lab week (Recap's read) and over its tool calls (Tools'):
    // T3_AGENTS_SKIM_BENCH_DB=<absolute path under checks/generated> \
    //   cargo test --release --locked -p t3-agents-core --lib -- --ignored head_cost --nocapture
    // With T3_AGENTS_SKIM_ONE=<label> it runs that one query once (for a cold run).
    #[test]
    #[ignore = "a measurement on a lab database; see the comment"]
    fn head_cost() {
        use std::time::Instant;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../checks/generated").canonicalize().unwrap();
        let path = std::path::PathBuf::from(std::env::var("T3_AGENTS_SKIM_BENCH_DB").expect("T3_AGENTS_SKIM_BENCH_DB")).canonicalize().unwrap();
        assert!(path.starts_with(&root), "only a lab under {root:?}");
        let trials: usize = std::env::var("T3_AGENTS_SKIM_TRIALS").ok().and_then(|n| n.parse().ok()).unwrap_or(7);
        let db = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.execute_batch("PRAGMA query_only=ON").unwrap();
        program::register(&db).unwrap();
        register_times(&db).unwrap();
        register(&db, Mode::Head).unwrap();
        let items = "orchestration_v2_projection_turn_items";
        let tools = "type IN ('command_execution','dynamic_tool','file_change','file_search','web_search')";
        let queries: Vec<(&str, String)> = vec![
            ("items: columns only", format!("SELECT type, status FROM {items}")),
            ("items: json_extract of the two times", format!("SELECT json_extract(payload_json,'$.startedAt'), json_extract(payload_json,'$.completedAt') FROM {items}")),
            ("items: t3_times", format!("SELECT t3_times(payload_json) FROM {items}")),
            ("items: t3_times_head", format!("SELECT t3_times_head(rowid, typeof(payload_json)) FROM {items}")),
            ("calls: t3_call full", format!("SELECT type, status, t3_call(payload_json, type, ?1, ?2) FROM {items} WHERE {tools} AND ?1 < ?2")),
            ("calls: t3_call head", format!("SELECT type, status, CASE WHEN type='command_execution' THEN t3_call(payload_json, type, ?1, ?2)
                ELSE t3_call_head(rowid, typeof(payload_json), type, ?1, ?2) END FROM {items} WHERE {tools} AND ?1 < ?2")),
        ];
        let (lo, hi) = (ms(FROM).unwrap(), ms(TO).unwrap());
        // A digest of every row's last column, to show a head form gives what its whole form gives.
        let run = |sql: &str| -> (usize, u64) {
            let mut q = db.prepare_cached(sql).unwrap();
            let bound = q.parameter_count();
            let mut rows = if bound == 2 { q.query(params![lo, hi]).unwrap() } else { q.query([]).unwrap() };
            let (mut n, mut digest) = (0usize, 0u64);
            while let Some(r) = rows.next().unwrap() {
                n += 1;
                let last = r.as_ref().column_count() - 1;
                let bytes: Vec<u8> = match r.get_ref(last).unwrap() {
                    ValueRef::Text(b) | ValueRef::Blob(b) => b.to_vec(),
                    ValueRef::Integer(i) => i.to_le_bytes().to_vec(),
                    ValueRef::Real(f) => f.to_le_bytes().to_vec(),
                    ValueRef::Null => vec![0xff],
                };
                for b in bytes { digest = (digest ^ u64::from(b)).wrapping_mul(0x100_0000_01b3); }
                digest = digest.rotate_left(5) ^ n as u64;
            }
            (n, digest)
        };
        if let Ok(one) = std::env::var("T3_AGENTS_SKIM_ONE") {
            let (_, sql) = queries.iter().find(|(label, _)| *label == one).expect("a label");
            let started = Instant::now();
            let (n, digest) = run(sql);
            eprintln!("one {one}: {:.6} s, {n} rows, digest {digest:016x}", started.elapsed().as_secs_f64());
            return;
        }
        let digests: Vec<(usize, u64)> = queries.iter().map(|(label, sql)| { let d = run(sql); eprintln!("warmup {label}: {} rows, digest {:016x}", d.0, d.1); d }).collect();
        assert_eq!(digests[2], digests[3], "t3_times_head gave what t3_times gives");
        assert_eq!(digests[4], digests[5], "t3_call head gave what t3_call gives");
        let mut times: Vec<Vec<f64>> = vec![vec![]; queries.len()];
        for trial in 0..trials {
            let order: Vec<usize> = if trial % 2 == 0 { (0..queries.len()).collect() } else { (0..queries.len()).rev().collect() };
            for i in order {
                let started = Instant::now();
                std::hint::black_box(run(&queries[i].1));
                times[i].push(started.elapsed().as_secs_f64());
            }
        }
        for ((label, _), t) in queries.iter().zip(&times) {
            let mut sorted = t.clone();
            sorted.sort_by(f64::total_cmp);
            let median = if sorted.len() % 2 == 1 { sorted[sorted.len() / 2] } else { (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2. };
            eprintln!("{label:<40} median {median:.4} s  range {:.4}..{:.4}  trials {:?}", sorted[0], sorted[sorted.len() - 1],
                t.iter().map(|x| (x * 10000.).round() / 10000.).collect::<Vec<_>>());
        }
    }

    const EXITS_FOR_TEST: [&str; 12] = ["-0", "1e2", "1.0", "9223372036854775807", "9223372036854775808", "-9223372036854775808",
        "-9223372036854775809", "99999999999999999999", "0.5", "-0.0", "\"1\"", "[1]"];

    #[test]
    fn an_escaped_value_is_unescaped_as_sqlite_does() {
        let mut out = vec![];
        let raw = format!("a{BS}n{}{}{}{}{}{BS}/{BS}\"", u(0xd83d), u(0xde00), u(0xd800), u(0x41), u(0xdc00) + &u(0xd800));
        unescape(raw.as_bytes(), &mut |part| out.extend_from_slice(part));
        assert_eq!(out, b"a\n\xf0\x9f\x98\x80\xed\xa0\x80A\xed\xb0\x80\xed\xa0\x80/\"");
    }

    #[test]
    fn the_sql_function_is_total() {
        let db = Connection::open_in_memory().unwrap();
        register(&db, Mode::Full).unwrap();
        for payload in ["NULL", "42", "1.5", "'{}'", "'[]'"] {
            let got: Option<String> = db.query_row(&format!("SELECT t3_call({payload}, 'command_execution', {}, {})", ms(FROM).unwrap(), ms(TO).unwrap()), [], |r| r.get(0)).unwrap();
            assert_eq!(got, None, "{payload}");
        }
        for sql in ["SELECT t3_call(x'7b7d', 'x', 0, 1)", "SELECT t3_call('', 'x', 0, 1)", "SELECT t3_call(cast(x'ff' as text), 'x', 0, 1)",
            "SELECT t3_call('{}', 'x', 1, 'b')", "SELECT t3_call('{}', 'x', '0', 1)", "SELECT t3_call('{\"a\":\"never read', 'x', 0, 1.5)"] {
            let error = db.query_row(sql, [], |r| r.get::<_, Option<String>>(0)).unwrap_err().to_string();
            assert!(plain(&error), "{error}");
        }
    }
}

#[cfg(test)]
mod string_scan {
    use super::*;

    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    const fn has_zero(word: u64) -> bool { word.wrapping_sub(ONES) & !word & HIGHS != 0 }

    /// `Cursor::string` as 1.4.0 had it.
    fn old(c: &mut Cursor<'_>) -> Result<bool, Fault> {
        let b = c.b;
        let mut i = c.i + 1;
        let mut escaped = false;
        loop {
            while let Some(chunk) = b.get(i..i + 8) {
                let word = u64::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7]]);
                if has_zero(word ^ (b'"' as u64 * ONES)) || has_zero(word ^ (b'\\' as u64 * ONES)) || has_zero(word) { break; }
                i += 8;
            }
            match b.get(i) {
                None | Some(0) => return Err(Fault::NotStrict),
                Some(b'"') => { c.i = i + 1; return Ok(escaped); }
                Some(b'\\') => {
                    escaped = true;
                    match b.get(i + 1) {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => i += 2,
                        Some(b'u') if b.get(i + 2..i + 6).is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit)) => i += 6,
                        _ => return Err(Fault::NotStrict),
                    }
                }
                Some(_) => i += 1,
            }
        }
    }

    // Xorshift64* with a fixed seed so failures can be reproduced.
    struct Gen(u64);

    impl Gen {
        fn below(&mut self, limit: usize) -> usize {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            ((self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as usize) % limit
        }
    }

    fn compare(buf: &[u8], at: usize, skew: usize, cut: bool) {
        let mut padded = vec![0x55; skew];
        padded.extend_from_slice(buf);
        let b = &padded[skew..];
        let mut before = Cursor { b, i: at, cut };
        let mut after = Cursor { b, i: at, cut };
        let expected = old(&mut before);
        let actual = after.string();
        assert_eq!(
            (actual, after.i), (expected, before.i),
            "buffer {buf:?}, opening quote {at}, skew {skew}, cut {cut}",
        );
    }

    // Escapes and their tails: valid and invalid, near the end and in the middle.
    const TAILS: [&[u8]; 14] = [b"\\\"", b"\\\\", b"\\/", b"\\b", b"\\n", b"\\u00e9", b"\\uD83D", b"\\u12G4", b"\\u12", b"\\x", b"\\", b"\\\\0", b"\\u", b"\\u0000"];

    #[test]
    fn vectorized_string_scan_matches_the_eight_byte_scan() {
        // 1. Every position of a quote, backslash or NUL (alone, and with a second one after it),
        //    in texts of every length to 72 (past two 32-byte blocks), at pads 0 to 3, every skew
        //    to 33 in steps, with and without `cut`.
        for pad in 0..4usize {
            for len in 0..=72usize {
                for first in 0..=len {
                    for &x in &[b'"', b'\\', 0u8, b'a'] {
                        for second in (first..=len).step_by(7).chain([len]) {
                            for &y in &[b'"', b'\\', 0u8] {
                                let mut buf = vec![0x55u8; pad];
                                buf.push(b'"');
                                let mut text = vec![b'a'; len];
                                if first < len { text[first] = x; }
                                if second < len && second > first { text[second] = y; }
                                buf.extend_from_slice(&text);
                                let skew = (first + second + pad) % 33;
                                compare(&buf, pad, skew, first % 2 == 0);
                            }
                        }
                    }
                }
            }
        }
        // 2. An escape (every tail above) at every position of every length to 72, and a text
        //    cut off right after it.
        for len in 0..=72usize {
            for at in 0..=len {
                for tail in TAILS {
                    for cut_after in [false, true] {
                        let mut buf = vec![b'"'];
                        buf.extend(std::iter::repeat(b'a').take(at));
                        buf.extend_from_slice(tail);
                        if !cut_after { buf.extend(std::iter::repeat(b'b').take(len - at)); buf.push(b'"'); }
                        let skew = (at + len) % 33;
                        compare(&buf, 0, skew, cut_after);
                    }
                }
            }
        }
        // 3. Random buffers over an alphabet weighted toward the interesting bytes.
        let mut g = Gen(0x9e37_79b9_7f4a_7c15);
        const PLAIN: &[u8] = b"aaaz ";
        const SPECIAL: &[u8] = b"\"\\\\\0u0fFgn/\t\n\x7f\x80\xc3\xff\x01";
        for _ in 0..300_000 {
            let pad = g.below(40);
            let len = g.below(100);
            let mut buf = vec![0x55; pad];
            buf.push(b'"');
            let density = g.below(4);
            for _ in 0..len {
                let byte = if g.below(16) <= density {
                    SPECIAL[g.below(SPECIAL.len())]
                } else {
                    PLAIN[g.below(PLAIN.len())]
                };
                buf.push(byte);
            }
            if g.below(2) == 0 { buf.push(b'"'); }
            let skew = g.below(64);
            compare(&buf, pad, skew, g.below(2) == 0);
        }
    }
}
