//! What the original's renderer makes of the composed grid: terminal cells.
//!
//! The canvas gives every code point a cell of its own. The original does not draw it that way.
//! Its `flush()` (oracle/src/app.ts) hands OpenTUI 0.8.3 one string per run of identically styled
//! cells (a wide character's continuation left out, an orphaned one as a space), and OpenTUI's
//! `drawText` regroups each string into grapheme clusters, gives each cluster a width, and packs
//! the run to the left from the run's first column into a buffer the renderer then writes out.
//! [`lay_out`] does the same to a [`Canvas`]: the result is the original's buffer, cell for cell.
//!
//! The rules come from OpenTUI's source and were checked against its own drawing. In short
//! (OpenTUI `packages/native/src`: `buffer.zig` `drawTextFitted`, `TextRow` and `set`; `utf8.zig`
//! `findRenderClusterInfo`, `GraphemeWidthState`, `eawToWidth`; uucode `grapheme.zig`
//! `computeGraphemeBreak`):
//!
//! - A run of printable ASCII is one cell per byte.
//! - Anything else is split at grapheme boundaries (UAX #29 with uucode's tailoring: an emoji
//!   modifier joins only an emoji modifier base). A cluster's width is the sum of its code points'
//!   widths under [`WidthMethod::Wcwidth`], and under [`WidthMethod::Unicode`] the width of its
//!   first code point with width, widened to 2 by VS16, an emoji modifier, a regional-indicator
//!   pair or a spacing mark. A code point's own width is 0 for marks and default-ignorables, 2 for
//!   East Asian Wide and Fullwidth, 1 otherwise.
//! - Zero-width clusters take no cell. A cluster that does not fit before the right edge is
//!   dropped, and so is everything after it. One that a cell cannot hold (a control, more than 128
//!   bytes, wider than 16) is written as spaces.
//! - Columns the run no longer needs keep what the buffer had: at the start of a frame, a space in
//!   white on the terminal's default background.
//! - Writing over part of a wide cluster blanks the rest of it, as OpenTUI's `set` does.
use crate::terminal::{CONT, Canvas, Rgb};
use std::borrow::Cow;
use std::collections::HashMap;

#[path = "cluster_tables.rs"]
mod tables;

/// How OpenTUI measures a cluster. It picks one from the terminal (see [`select_width_method`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WidthMethod {
    /// The widths of terminals that cluster graphemes (mode 2027, kitty, foot, tmux 3.6 and later).
    #[default]
    Unicode,
    /// The sum of the code points' widths, as terminals that draw code points one by one.
    Wcwidth,
    /// OpenTUI's `unicode_wide`, chosen for Ghostty (1.3 and later). Like [`WidthMethod::Unicode`],
    /// except that an emoji modifier does not widen a base, and any code point that is not
    /// zero-width inside a cluster widens it to at least 2.
    UnicodeWide,
}

/// A background that is the terminal's own: written as `SGR 49`. OpenTUI clears its buffer to a
/// transparent background at the start of a frame, and writes those cells that way.
pub const DEFAULT_BG: Rgb = 0x0100_0000;
/// The foreground of a cell OpenTUI cleared: opaque white.
pub const CLEARED_FG: Rgb = 0xff_ffff;
/// OpenTUI never puts more bytes than this in one cell.
pub const GLYPH_BYTES_MAX: usize = 128;
/// Nor makes one cell wider than this.
pub const GLYPH_WIDTH_MAX: u32 = 16;

/// Grapheme break classes, in uucode's order (the tables store the index).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Gb {
    Other,
    Control,
    Prepend,
    Cr,
    Lf,
    RegionalIndicator,
    SpacingMark,
    L,
    V,
    T,
    Lv,
    Lvt,
    Zwj,
    Zwnj,
    ExtendedPictographic,
    EmojiModifierBase,
    EmojiModifier,
    IcbExtend,
    IcbLinker,
    IcbConsonant,
}

const GB: [Gb; 20] = [
    Gb::Other,
    Gb::Control,
    Gb::Prepend,
    Gb::Cr,
    Gb::Lf,
    Gb::RegionalIndicator,
    Gb::SpacingMark,
    Gb::L,
    Gb::V,
    Gb::T,
    Gb::Lv,
    Gb::Lvt,
    Gb::Zwj,
    Gb::Zwnj,
    Gb::ExtendedPictographic,
    Gb::EmojiModifierBase,
    Gb::EmojiModifier,
    Gb::IcbExtend,
    Gb::IcbLinker,
    Gb::IcbConsonant,
];

const WIDE: u8 = 1;
const MN: u8 = 2;
const MARK: u8 = 4;
const IGNORABLE: u8 = 8;
/// uucode's `wcwidth_zero_in_grapheme`, which [`WidthMethod::UnicodeWide`] reads.
const ZERO_IN_GRAPHEME: u8 = 16;

fn props(cp: u32) -> (Gb, u8) {
    if cp < 0x80 {
        let gb = match cp {
            0x0a => Gb::Lf,
            0x0d => Gb::Cr,
            0x00..=0x1f | 0x7f => Gb::Control,
            _ => Gb::Other,
        };
        return (gb, 0);
    }
    let rows = tables::PROPS;
    let i = rows.partition_point(|&(_, last, _, _)| last < cp);
    match rows.get(i) {
        Some(&(first, _, gb, flags)) if first <= cp => (GB[gb as usize], flags),
        _ => (Gb::Other, 0),
    }
}

fn gb(cp: u32) -> Gb {
    props(cp).0
}

/// OpenTUI's `eawToWidth`: -1 for a control, 0 for a mark or a default-ignorable (not the soft
/// hyphen, nor the Hangul fillers U+115F and U+1160), 2 for East Asian Wide or Fullwidth, else 1.
pub fn code_point_width(cp: u32) -> i32 {
    if cp == 0 {
        return 0;
    }
    if cp < 32 || (0x7f..0xa0).contains(&cp) {
        return -1;
    }
    let (_, flags) = props(cp);
    if flags & MARK != 0 {
        return 0;
    }
    if cp != 0xad && cp != 0x115f && cp != 0x1160 && flags & IGNORABLE != 0 {
        return 0;
    }
    if flags & WIDE != 0 { 2 } else { 1 }
}

/// OpenTUI's `charWidth` for drawn text: a tab is 2, a control 0.
fn char_width(cp: u32) -> u32 {
    if cp < 0x80 {
        return match cp {
            0x09 => 2,
            0x20..=0x7e => 1,
            _ => 0,
        };
    }
    code_point_width(cp).max(0) as u32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum BreakState {
    #[default]
    Default,
    RegionalIndicator,
    ExtendedPictographic,
    IcbConsonant,
    IcbLinker,
}

fn is_extend(g: Gb) -> bool {
    matches!(g, Gb::Zwnj | Gb::IcbExtend | Gb::IcbLinker)
}

fn is_pictographic(g: Gb) -> bool {
    matches!(g, Gb::ExtendedPictographic | Gb::EmojiModifierBase)
}

fn is_icb_extend(g: Gb) -> bool {
    matches!(g, Gb::IcbExtend | Gb::Zwj)
}

/// uucode's `computeGraphemeBreak`, rule for rule.
fn compute_break(gb1: Gb, gb2: Gb, state: &mut BreakState) -> bool {
    use Gb::*;
    match *state {
        BreakState::RegionalIndicator => {
            if gb1 != RegionalIndicator || gb2 != RegionalIndicator {
                *state = BreakState::Default;
            }
        }
        BreakState::ExtendedPictographic => {
            let keep = |g: Gb| {
                matches!(
                    g,
                    IcbExtend | IcbLinker | Zwnj | Zwj | ExtendedPictographic | EmojiModifierBase | EmojiModifier
                )
            };
            if !keep(gb1) {
                *state = BreakState::Default;
            }
            if !keep(gb2) {
                *state = BreakState::Default;
            }
        }
        BreakState::IcbConsonant | BreakState::IcbLinker => {
            let keep = |g: Gb| matches!(g, IcbConsonant | IcbLinker | IcbExtend | Zwj);
            if !keep(gb1) {
                *state = BreakState::Default;
            }
            if !keep(gb2) {
                *state = BreakState::Default;
            }
        }
        BreakState::Default => {}
    }
    // GB3, GB4, GB5
    if gb1 == Cr && gb2 == Lf {
        return false;
    }
    if matches!(gb1, Control | Cr | Lf) || matches!(gb2, Control | Cr | Lf) {
        return true;
    }
    // GB6, GB7, GB8: Hangul
    if gb1 == L && matches!(gb2, L | V | Lv | Lvt) {
        return false;
    }
    if matches!(gb1, Lv | V) && matches!(gb2, V | T) {
        return false;
    }
    if matches!(gb1, Lvt | T) && gb2 == T {
        return false;
    }
    // GB9a, GB9b
    if gb2 == SpacingMark || gb1 == Prepend {
        return false;
    }
    // GB9c: Indic conjuncts
    if gb1 == IcbConsonant {
        if is_icb_extend(gb2) {
            *state = BreakState::IcbConsonant;
            return false;
        } else if gb2 == IcbLinker {
            *state = BreakState::IcbLinker;
            return false;
        }
    } else if *state == BreakState::IcbConsonant {
        if gb2 == IcbLinker {
            *state = BreakState::IcbLinker;
            return false;
        } else if is_icb_extend(gb2) {
            return false;
        } else {
            *state = BreakState::Default;
        }
    } else if *state == BreakState::IcbLinker {
        if gb2 == IcbLinker || is_icb_extend(gb2) {
            return false;
        } else if gb2 == IcbConsonant {
            *state = BreakState::Default;
            return false;
        } else {
            *state = BreakState::Default;
        }
    }
    // GB11: emoji ZWJ sequences, and uucode's emoji modifier sequence
    if is_pictographic(gb1) {
        if is_extend(gb2) || gb2 == Zwj {
            *state = BreakState::ExtendedPictographic;
            return false;
        }
        if gb1 == EmojiModifierBase && gb2 == EmojiModifier {
            *state = BreakState::ExtendedPictographic;
            return false;
        }
    } else if *state == BreakState::ExtendedPictographic {
        if (is_extend(gb1) || gb1 == EmojiModifier) && (is_extend(gb2) || gb2 == Zwj) {
            return false;
        } else if gb1 == Zwj && is_pictographic(gb2) {
            *state = BreakState::Default;
            return false;
        } else {
            *state = BreakState::Default;
        }
    }
    // GB12, GB13: regional indicator pairs
    if gb1 == RegionalIndicator && gb2 == RegionalIndicator {
        if *state == BreakState::Default {
            *state = BreakState::RegionalIndicator;
            return false;
        }
        *state = BreakState::Default;
        return true;
    }
    // GB9
    if is_extend(gb2) || gb2 == Zwj {
        return false;
    }
    true
}

/// OpenTUI's `isGraphemeBreak`: U+FFFD always stands alone.
fn is_break(prev: Option<u32>, cur: u32, state: &mut BreakState) -> bool {
    let Some(prev) = prev else { return true };
    if prev == 0xfffd || cur == 0xfffd {
        *state = BreakState::Default;
        return true;
    }
    compute_break(gb(prev), gb(cur), state)
}

/// OpenTUI's `GraphemeWidthState`.
struct WidthState {
    width: u32,
    has_width: bool,
    regional_pair: bool,
    modifier_base: bool,
    virama: bool,
}

impl WidthState {
    fn new(cp: u32) -> Self {
        let width = char_width(cp);
        Self {
            width,
            has_width: width > 0,
            regional_pair: (0x1f1e6..=0x1f1ff).contains(&cp),
            modifier_base: gb(cp) == Gb::EmojiModifierBase,
            virama: false,
        }
    }

    fn add(&mut self, cp: u32, method: WidthMethod) {
        if method == WidthMethod::Wcwidth {
            let w = code_point_width(cp);
            if w > 0 {
                self.width += w as u32;
                self.has_width = true;
            }
            return;
        }
        let cp_width = char_width(cp);
        if cp == 0xfe0f {
            if self.has_width && self.width == 1 {
                self.width = 2;
            }
            return;
        }
        let (gb, flags) = props(cp);
        if flags & MN != 0 {
            self.virama = true;
            return;
        }
        let devanagari_base = (0x0915..=0x0939).contains(&cp) || (0x0958..=0x095f).contains(&cp);
        let wide = method == WidthMethod::UnicodeWide;
        if self.regional_pair && (0x1f1e6..=0x1f1ff).contains(&cp) {
            self.width += cp_width;
            self.has_width = true;
        } else if !self.has_width && cp_width > 0 {
            self.width = cp_width;
            self.has_width = true;
        } else if self.modifier_base && !wide && gb == Gb::EmojiModifier {
            self.width = 2;
        } else if wide && self.has_width && flags & ZERO_IN_GRAPHEME == 0 {
            self.width = self.width.max(2);
        } else if self.has_width && gb == Gb::SpacingMark && cp_width > 0 {
            self.width = self.width.max(2);
        } else if self.has_width && self.virama && devanagari_base && cp_width > 0 {
            if cp != 0x0930 {
                self.width += cp_width;
            }
            self.virama = false;
        }
    }
}

/// The grapheme clusters of `text` as byte ranges, with the width OpenTUI gives each.
pub fn clusters(text: &str, method: WidthMethod) -> Vec<(usize, usize, u32)> {
    let mut out = Vec::new();
    let mut prev: Option<u32> = None;
    let mut state = BreakState::Default;
    let mut start = 0;
    let mut width: Option<WidthState> = None;
    for (at, c) in text.char_indices() {
        let cp = c as u32;
        if is_break(prev, cp, &mut state) {
            if let Some(w) = width.take() {
                out.push((start, at, w.width));
            }
            start = at;
            width = Some(WidthState::new(cp));
        } else if let Some(w) = width.as_mut() {
            w.add(cp, method);
        }
        prev = Some(cp);
    }
    if let Some(w) = width {
        out.push((start, text.len(), w.width));
    }
    out
}

/// The width OpenTUI gives `text`: the sum of its clusters'.
pub fn text_width(text: &str, method: WidthMethod) -> u32 {
    clusters(text, method).iter().map(|c| c.2).sum()
}

/// OpenTUI's `isPrintableGlyph`: what one cell can hold.
fn printable(bytes: &[u8], width: u32) -> bool {
    if bytes.len() > GLYPH_BYTES_MAX || width > GLYPH_WIDTH_MAX {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        if b < 0x20 || b == 0x7f {
            return false;
        }
        if b == 0xc2 && i + 1 < bytes.len() && bytes[i + 1] < 0xa0 {
            return false;
        }
    }
    true
}

/// What a cell of the laid-out buffer holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    /// One byte of printable ASCII, or the space of a cleared or blanked cell.
    Plain(char),
    /// The first cell of a cluster (any non-ASCII text), `width` cells wide.
    Start { id: u32, width: u8 },
    /// A later cell of the cluster that starts `left` cells to the left.
    Cont { id: u32, left: u8 },
}

/// Ids of single-character clusters are the character; longer text is numbered from here.
const MULTI: u32 = 0x20_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaidCell {
    pub glyph: Glyph,
    pub fg: Rgb,
    /// [`DEFAULT_BG`] for the terminal's own background.
    pub bg: Rgb,
    pub bold: bool,
}

impl LaidCell {
    const CLEARED: LaidCell = LaidCell {
        glyph: Glyph::Plain(' '),
        fg: CLEARED_FG,
        bg: DEFAULT_BG,
        bold: false,
    };
}

/// The original's buffer for one frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Laid {
    pub w: usize,
    pub h: usize,
    pub cells: Vec<LaidCell>,
    texts: Vec<String>,
    ids: HashMap<String, u32>,
}

impl Laid {
    /// A buffer OpenTUI has just cleared.
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            cells: vec![LaidCell::CLEARED; w * h],
            texts: Vec::new(),
            ids: HashMap::new(),
        }
    }

    /// The text of a cluster id.
    pub fn text(&self, id: u32) -> Cow<'_, str> {
        if id >= MULTI {
            Cow::Borrowed(self.texts[(id - MULTI) as usize].as_str())
        } else {
            Cow::Owned(char::from_u32(id).unwrap_or(' ').to_string())
        }
    }

    /// The text a cell starts, if it starts one: `None` for a continuation.
    pub fn cell_text(&self, i: usize) -> Option<Cow<'_, str>> {
        match self.cells[i].glyph {
            Glyph::Plain(c) => Some(Cow::Owned(c.to_string())),
            Glyph::Start { id, .. } => Some(self.text(id)),
            Glyph::Cont { .. } => None,
        }
    }

    /// Whether cell `i` here and cell `j` of `other` hold the same thing in the same style.
    pub fn same(&self, i: usize, other: &Laid, j: usize) -> bool {
        let (a, b) = (self.cells[i], other.cells[j]);
        if a.fg != b.fg || a.bg != b.bg || a.bold != b.bold {
            return false;
        }
        match (a.glyph, b.glyph) {
            (Glyph::Plain(x), Glyph::Plain(y)) => x == y,
            (Glyph::Start { id: x, width: wx }, Glyph::Start { id: y, width: wy }) => {
                wx == wy && (x == y && x < MULTI || self.text(x) == other.text(y))
            }
            (Glyph::Cont { id: x, left: lx }, Glyph::Cont { id: y, left: ly }) => {
                lx == ly && (x == y && x < MULTI || self.text(x) == other.text(y))
            }
            _ => false,
        }
    }

    /// The same frame, cell for cell.
    pub fn same_frame(&self, other: &Laid) -> bool {
        self.w == other.w && self.h == other.h && (0..self.cells.len()).all(|i| self.same(i, other, i))
    }

    fn intern(&mut self, text: &str) -> u32 {
        let mut chars = text.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            return c as u32;
        }
        if let Some(&id) = self.ids.get(text) {
            return id;
        }
        let id = MULTI + self.texts.len() as u32;
        self.texts.push(text.to_owned());
        self.ids.insert(text.to_owned(), id);
        id
    }

    fn id_of(glyph: Glyph) -> Option<u32> {
        match glyph {
            Glyph::Plain(_) => None,
            Glyph::Start { id, .. } | Glyph::Cont { id, .. } => Some(id),
        }
    }

    /// OpenTUI's `OptimizedBuffer.set` with span cleanup, for a cell drawn by `drawText`.
    fn set(&mut self, x: usize, y: usize, cell: LaidCell) {
        if x >= self.w || y >= self.h {
            return;
        }
        let row = y * self.w;
        let index = row + x;
        let prev = self.cells[index];
        let new_is_start = matches!(cell.glyph, Glyph::Start { .. });
        if Self::id_of(prev.glyph).is_none() && !new_is_start {
            self.cells[index] = cell;
            return;
        }
        if let Some(prev_id) = Self::id_of(prev.glyph) {
            if prev.glyph != cell.glyph {
                // Writing over part of a cluster with something else blanks all of it.
                let left = match prev.glyph {
                    Glyph::Cont { left, .. } => left as usize,
                    _ => 0,
                };
                let span_start = index - left.min(index - row);
                let span_end = (span_start + GLYPH_WIDTH_MAX as usize).min(row + self.w);
                for i in span_start..span_end {
                    let c = self.cells[i].glyph;
                    let (id, l) = match c {
                        Glyph::Start { id, .. } => (id, 0),
                        Glyph::Cont { id, left } => (id, left as usize),
                        Glyph::Plain(_) => continue,
                    };
                    if id != prev_id || l != i - span_start {
                        continue;
                    }
                    self.cells[i].glyph = Glyph::Plain(' ');
                    self.cells[i].bold = false;
                }
            }
        }
        let Glyph::Start { id, width } = cell.glyph else {
            self.cells[index] = cell;
            return;
        };
        let width = width as usize;
        if x + width > self.w {
            for c in &mut self.cells[index..row + self.w] {
                *c = LaidCell { glyph: Glyph::Plain(' '), ..cell };
            }
            return;
        }
        self.cells[index] = cell;
        let row_end = row + self.w - 1;
        for k in 1..width {
            let cont_index = index + k;
            if let Glyph::Start { id: old_id, width: old_width } = self.cells[cont_index].glyph {
                // A cluster whose start this one covers loses the rest of its cells too.
                let tail_end = cont_index + (old_width as usize - 1).min(row_end - cont_index);
                for tail in index + width..=tail_end {
                    if let Glyph::Cont { id: t, left } = self.cells[tail].glyph {
                        if t == old_id && left as usize == tail - cont_index {
                            self.cells[tail].glyph = Glyph::Plain(' ');
                            self.cells[tail].bold = false;
                        }
                    }
                }
            }
        }
        for k in 1..width {
            self.cells[index + k] = LaidCell {
                glyph: Glyph::Cont { id, left: k as u8 },
                ..cell
            };
        }
    }

    /// OpenTUI's `drawText(text, x, y, fg, bg, attributes)` on this buffer: no scissor, opaque.
    pub fn draw_text(&mut self, text: &str, x: usize, y: usize, fg: Rgb, bg: Rgb, bold: bool, method: WidthMethod) {
        if x >= self.w || y >= self.h || text.is_empty() {
            return;
        }
        let style = |glyph| LaidCell { glyph, fg, bg, bold };
        if text.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
            for (col, b) in (x..self.w).zip(text.bytes()) {
                self.set(col, y, style(Glyph::Plain(b as char)));
            }
            return;
        }
        // Lay out first (TextRow), then write (the prepared runs), as drawTextFitted does.
        let end = self.w;
        let mut runs: Vec<(usize, Glyph, usize)> = Vec::new();
        let mut advance = 0usize;
        for (a, b, width) in clusters(text, method) {
            if x + advance >= end {
                break;
            }
            let width = width as usize;
            if width == 0 {
                continue;
            }
            let bytes = &text.as_bytes()[a..b];
            let gx = x + advance;
            advance += width;
            if !printable(bytes, width as u32) {
                let count = (gx + width).min(end) - gx;
                if count > 0 {
                    runs.push((gx, Glyph::Plain(' '), count));
                }
                continue;
            }
            if gx + width > end {
                continue;
            }
            let glyph = if bytes.len() == 1 {
                Glyph::Plain(bytes[0] as char)
            } else {
                Glyph::Start {
                    id: self.intern(&text[a..b]),
                    width: width as u8,
                }
            };
            runs.push((gx, glyph, 1));
        }
        for (gx, glyph, count) in runs {
            for col in gx..gx + count {
                self.set(col, y, style(glyph));
            }
        }
    }
}

/// The original's `flush()`: the canvas as runs of identically styled cells, each drawn with
/// `drawText` from its first column into a buffer cleared for the frame.
pub fn lay_out(canvas: &Canvas, method: WidthMethod) -> Laid {
    let mut laid = Laid::new(canvas.w, canvas.h);
    let mut text = String::new();
    for y in 0..canvas.h {
        laid.draw_row(canvas, y, method, &mut text);
    }
    laid
}

impl Laid {
    /// Lay out row `y` of `canvas` again, as [`lay_out`] lays out that row into a cleared
    /// buffer. A row's layout reads only that row of the canvas and writes only that row of the
    /// buffer (`set` keeps its span cleanup inside the row, `draw_text` writes one row), so a
    /// buffer laid out for one canvas and then given this for every row in which a second canvas
    /// differs is cell for cell what [`lay_out`] gives for the second canvas. Text interned for
    /// earlier rows stays in the table: [`Laid::same`] compares cluster text, not ids.
    pub fn relay_row(&mut self, canvas: &Canvas, y: usize, method: WidthMethod, text: &mut String) {
        let row = y * self.w;
        self.cells[row..row + self.w].fill(LaidCell::CLEARED);
        self.draw_row(canvas, y, method, text);
    }

    /// How many multi-character cluster texts the buffer holds.
    pub fn text_count(&self) -> usize {
        self.texts.len()
    }

    /// [`lay_out`]'s loop for one row, into cells already cleared.
    fn draw_row(&mut self, canvas: &Canvas, y: usize, method: WidthMethod, text: &mut String) {
        let row = &canvas.cells[y * canvas.w..(y + 1) * canvas.w];
        let mut x = 0;
        while x < canvas.w {
            let first = row[x];
            text.clear();
            let mut end = x;
            while end < canvas.w {
                let c = row[end];
                if c.fg != first.fg || c.bg != first.bg || c.bold != first.bold {
                    break;
                }
                if c.ch != CONT {
                    text.push(c.ch);
                } else if text.is_empty() {
                    text.push(' '); // orphaned half of a wide character
                }
                end += 1;
            }
            self.draw_text(text, x, y, first.fg, first.bg, first.bold, method);
            x = end;
        }
    }
}

/// What XTVERSION (`CSI > 0 q`) answered: the terminal's name and version, as OpenTUI's
/// `parseXtversion` splits `kitty(0.40.1)` or `ghostty 1.4.0`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Xtversion {
    pub name: String,
    pub version: String,
    /// The answer as sent, which OpenTUI also searches for "alacritty".
    raw: String,
}

impl Xtversion {
    /// The name and version in an XTVERSION answer (the text between `DCS > |` and ST). `None` for
    /// an empty answer, which OpenTUI ignores.
    pub fn parse(text: &str) -> Option<Self> {
        if text.is_empty() {
            return None;
        }
        // OpenTUI keeps 64 bytes of name and 32 of version, cut anywhere.
        let cut = |s: &str, max: usize| String::from_utf8_lossy(&s.as_bytes()[..s.len().min(max)]).into_owned();
        let (name, version) = if let Some(open) = text.find('(') {
            let version = text[open..].find(')').map_or("", |close| &text[open + 1..open + close]);
            (&text[..open], version)
        } else if let Some(space) = text.find(' ') {
            (&text[..space], &text[space + 1..])
        } else {
            (text, "")
        };
        Some(Self { name: cut(name, 64), version: cut(version, 32), raw: text.to_owned() })
    }
}

/// What the terminal environment and its answers say, for [`select_width_method`] and
/// [`explicit_cursor_positioning`]. The environment is read once from the process; the answers
/// are added as they arrive.
#[derive(Clone, Debug, Default)]
pub struct TermEnv {
    vars: HashMap<String, String>,
    xtversion: Option<Xtversion>,
}

impl TermEnv {
    const NAMES: [&'static str; 22] = [
        "OPENTUI_FORCE_NOZWJ",
        "OPENTUI_FORCE_UNICODE",
        "OPENTUI_FORCE_WCWIDTH",
        "SSH_CONNECTION",
        "SSH_CLIENT",
        "SSH_TTY",
        "MOSH_CONNECTION",
        "TMUX",
        "STY",
        "ZELLIJ",
        "ZELLIJ_SESSION_NAME",
        "ZELLIJ_PANE_ID",
        "TERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "TERMUX_VERSION",
        "VHS_RECORD",
        "KONSOLE_VERSION",
        "VTE_VERSION",
        "XTERM_VERSION",
        "ALACRITTY_SOCKET",
        "ALACRITTY_LOG",
    ];

    pub fn from_process() -> Self {
        Self::from_pairs(Self::NAMES.iter().filter_map(|&k| std::env::var(k).ok().map(|v| (k, v))))
    }

    pub fn from_pairs<K: Into<String>, V: Into<String>>(pairs: impl IntoIterator<Item = (K, V)>) -> Self {
        Self {
            vars: pairs.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
            xtversion: None,
        }
    }

    /// Take the terminal's XTVERSION answer (the text between `DCS > |` and ST). True when it
    /// changed what the original would conclude, so the caller can repaint.
    pub fn set_xtversion(&mut self, text: &str) -> bool {
        let before = (select_width_method(self, None), explicit_cursor_positioning(self));
        if let Some(version) = Xtversion::parse(text) {
            self.xtversion = Some(version);
        }
        before != (select_width_method(self, None), explicit_cursor_positioning(self))
    }

    pub fn xtversion(&self) -> Option<&Xtversion> {
        self.xtversion.as_ref()
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.vars.get(k).map(String::as_str)
    }

    /// OpenTUI's `isRemoteSessionEnv`: an SSH or Mosh session.
    fn remote(&self) -> bool {
        ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "MOSH_CONNECTION"].iter().any(|k| self.get(k).is_some())
    }

    /// The environment that may describe the terminal (`identityEnv`): not a remote session's,
    /// which describes the machine the app runs on and not the one the terminal runs on.
    fn identity_env(&self) -> Option<&Self> {
        (!self.remote()).then_some(self)
    }

    /// OpenTUI's multiplexer detection (`checkEnvironmentOverrides`), and whether that detection
    /// also turned on explicit cursor positioning. An XTVERSION answer names the multiplexer
    /// itself and replaces the environment's say.
    fn multiplexer(&self) -> (Multiplexer, bool) {
        let alacritty_answer = self.xtversion.as_ref().is_some_and(|x| x.raw.contains("alacritty"));
        let (mut found, mut ecp) = match &self.xtversion {
            Some(x) if x.name == "tmux" => (Multiplexer::Tmux, false),
            Some(x) if x.name.eq_ignore_ascii_case("Zellij") => (Multiplexer::Zellij, false),
            _ => (Multiplexer::None, false),
        };
        if self.xtversion.is_none() && self.identity_env().is_some() {
            (found, ecp) = self.env_multiplexer();
        }
        // Once the terminal has answered, being in tmux turns it on as well.
        if found == Multiplexer::Tmux || alacritty_answer {
            ecp = true;
        }
        (found, ecp)
    }

    fn env_multiplexer(&self) -> (Multiplexer, bool) {
        let term = self.get("TERM").unwrap_or("");
        let (mut found, mut ecp) = if self.get("TMUX").is_some() {
            (Multiplexer::Tmux, true)
        } else if self.get("ZELLIJ").is_some() || self.get("ZELLIJ_SESSION_NAME").is_some() || self.get("ZELLIJ_PANE_ID").is_some() {
            (Multiplexer::Zellij, false)
        } else if self.get("STY").is_some() {
            (Multiplexer::Screen, true)
        } else if term.starts_with("tmux") {
            (Multiplexer::Tmux, true)
        } else if term.starts_with("screen") {
            (Multiplexer::Screen, true)
        } else {
            (Multiplexer::None, false)
        };
        let zellij = found == Multiplexer::Zellij;
        let plain_term = !matches!(found, Multiplexer::Zellij) && self.get("TMUX").is_none() && self.get("STY").is_none();
        if plain_term && term.contains("alacritty") {
            ecp = true;
        }
        if !zellij && self.get("TERM_PROGRAM") == Some("tmux") {
            found = Multiplexer::Tmux;
            ecp = true;
        }
        if self.get("TERM_PROGRAM") == Some("Alacritty") || self.get("ALACRITTY_SOCKET").is_some() || self.get("ALACRITTY_LOG").is_some() {
            ecp = true;
        }
        (found, ecp)
    }

    /// OpenTUI's `getTerminalName` and `getTerminalVersion`: the XTVERSION answer, or else the
    /// environment's `TERM_PROGRAM` (not under Zellij, and not in a remote session).
    fn terminal(&self) -> (&str, &str) {
        if let Some(x) = &self.xtversion {
            return (&x.name, &x.version);
        }
        if self.identity_env().is_some() && self.env_multiplexer().0 != Multiplexer::Zellij {
            return (self.get("TERM_PROGRAM").unwrap_or(""), self.get("TERM_PROGRAM_VERSION").unwrap_or(""));
        }
        ("", "")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Multiplexer {
    None,
    Tmux,
    Zellij,
    Screen,
}

/// OpenTUI's `selectWidthMethod`, from the environment, the terminal's XTVERSION answer if it has
/// given one, and its answer to `CSI ? 2027 $ p` (`None` before or without one).
pub fn select_width_method(env: &TermEnv, mode_2027: Option<u8>) -> WidthMethod {
    if env.get("OPENTUI_FORCE_NOZWJ").is_some() || env.get("OPENTUI_FORCE_UNICODE").is_some() {
        return WidthMethod::Unicode;
    }
    if env.get("OPENTUI_FORCE_WCWIDTH").is_some() {
        return WidthMethod::Wcwidth;
    }
    let inner = env.identity_env().is_some_and(|e| e.get("STY").is_some() || e.get("ZELLIJ").is_some());
    let from_xtversion = env.xtversion.is_some();
    match env.multiplexer().0 {
        Multiplexer::None => {}
        Multiplexer::Tmux => {
            if from_xtversion || mode_2027.is_none() {
                let (name, version) = env.terminal();
                let graphemes = name == "tmux" && tmux_graphemes(version);
                return if graphemes && (from_xtversion || !inner) { WidthMethod::Unicode } else { WidthMethod::Wcwidth };
            }
        }
        Multiplexer::Zellij | Multiplexer::Screen => return WidthMethod::Wcwidth,
    }
    let identity = match &env.xtversion {
        Some(x) => xtversion_width_identity(&x.name, &x.version),
        None => env.identity_env().map_or(Identity::Unknown, env_width_identity),
    };
    let Some(mode) = mode_2027 else {
        return match identity {
            Identity::Wcwidth => WidthMethod::Wcwidth,
            Identity::Ghostty => WidthMethod::UnicodeWide,
            _ => WidthMethod::Unicode,
        };
    };
    if (mode == 0 || mode == 4) && identity != Identity::Graphemes {
        return WidthMethod::Wcwidth;
    }
    if identity == Identity::Ghostty { WidthMethod::UnicodeWide } else { WidthMethod::Unicode }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Identity {
    Unknown,
    Wcwidth,
    Graphemes,
    Ghostty,
}

/// OpenTUI's `envWidthIdentity`.
fn env_width_identity(env: &TermEnv) -> Identity {
    if env.get("TERMUX_VERSION").is_some() || env.get("VHS_RECORD").is_some() {
        return Identity::Wcwidth;
    }
    let term = env.get("TERM").unwrap_or("");
    if term.starts_with("xterm-kitty") {
        return Identity::Graphemes;
    }
    if term.starts_with("alacritty") || term.starts_with("rxvt") {
        return Identity::Wcwidth;
    }
    if !term.is_empty() && !term.starts_with("xterm") {
        return Identity::Unknown;
    }
    let program = env.get("TERM_PROGRAM").unwrap_or("");
    if program.eq_ignore_ascii_case("ghostty") && !env.remote() && ghostty_wide_grapheme_widths(env.get("TERM_PROGRAM_VERSION").unwrap_or("")) {
        return Identity::Ghostty;
    }
    if program == "vscode" || program == "Apple_Terminal" {
        return Identity::Wcwidth;
    }
    if !program.is_empty() || env.get("KONSOLE_VERSION").is_some() {
        return Identity::Unknown;
    }
    if env.get("VTE_VERSION").is_some()
        || env.get("XTERM_VERSION").is_some()
        || env.get("ALACRITTY_SOCKET").is_some()
        || env.get("ALACRITTY_LOG").is_some()
    {
        return Identity::Wcwidth;
    }
    Identity::Unknown
}

/// OpenTUI's `xtversionWidthIdentity`: what the terminal's own name says about its widths.
fn xtversion_width_identity(name: &str, version: &str) -> Identity {
    if name.eq_ignore_ascii_case("libghostty") || (name.eq_ignore_ascii_case("ghostty") && ghostty_wide_grapheme_widths(version)) {
        return Identity::Ghostty;
    }
    if name.eq_ignore_ascii_case("kitty") || name.eq_ignore_ascii_case("iTerm2") {
        return Identity::Graphemes;
    }
    if name.eq_ignore_ascii_case("XTerm") || name.eq_ignore_ascii_case("VTE") || name.eq_ignore_ascii_case("xterm.js") {
        return Identity::Wcwidth;
    }
    Identity::Unknown
}

/// OpenTUI's `semanticVersionAtLeast`: `major.minor.patch`, with an optional `-<commits>-g<hash>`
/// development suffix that counts as at least the release it follows.
fn semantic_version_at_least(version: &str, required_major: u32, required_minor: u32) -> bool {
    let (core, suffix) = match version.split_once('-') {
        Some((core, suffix)) => (core, Some(suffix)),
        None => (version, None),
    };
    let mut parts = core.split('.');
    let (Some(major), Some(minor), Some(patch), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(major), Ok(minor), Ok(patch)) = (major.parse::<u32>(), minor.parse::<u32>(), patch.parse::<u32>()) else {
        return false;
    };
    if major > required_major || (major == required_major && minor > required_minor) {
        return true;
    }
    if major != required_major || minor != required_minor {
        return false;
    }
    if patch > 0 {
        return true;
    }
    let Some(suffix) = suffix else { return true };
    let mut parts = suffix.split('-');
    let (Some(commits), Some(revision), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    if commits.is_empty() || revision.len() < 2 || !revision.starts_with('g') || commits.parse::<u32>().is_err() {
        return false;
    }
    revision[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// OpenTUI's `ghosttyWideGraphemeWidths`: Ghostty 1.3 and later, or a tip build from 2026-02-24 on.
fn ghostty_wide_grapheme_widths(version: &str) -> bool {
    if semantic_version_at_least(version, 1, 3) {
        return true;
    }
    let prefix = "0.0.0-";
    let bytes = version.as_bytes();
    if !version.starts_with(prefix) || bytes.len() <= prefix.len() + 8 || bytes[prefix.len() + 8] != b'.' {
        return false;
    }
    let date = &version[prefix.len()..prefix.len() + 8];
    date.bytes().all(|b| b.is_ascii_digit()) && date.parse::<u32>().is_ok_and(|d| d >= 20260224)
}

/// tmux 3.6 and later draw emoji sequences as one cluster (OpenTUI's `tmuxGraphemes`).
fn tmux_graphemes(version: &str) -> bool {
    let release = version.strip_prefix("next-").unwrap_or(version);
    let Some((major, minor)) = release.split_once('.') else { return false };
    let Ok(major) = major.parse::<u32>() else { return false };
    let digits: String = minor.chars().take_while(|c| c.is_ascii_digit()).collect();
    let Ok(minor) = digits.parse::<u32>() else { return false };
    major > 3 || (major == 3 && minor >= 6)
}

/// Whether the original moves the cursor itself after every cluster it writes without an explicit
/// width: in tmux, screen and Alacritty, whose widths it does not trust.
pub fn explicit_cursor_positioning(env: &TermEnv) -> bool {
    env.multiplexer().1
}

/// `t3-agents-core --layout-rows`: for checks/scripts/cluster-fuzz.ts. Each line of standard input
/// is a canvas as JSON (`m` "unicode", "unicode-wide" or "wcwidth", `w`, `h`, and per cell `ch` (0 for a
/// continuation), `fg`, `bg`, `at`); each line of output is [`lay_out`]'s buffer, one
/// `[kind, text, n, fg, bg, bold]` per cell: kind 0 plain, 1 a cluster `n` wide, 2 the cell `n`
/// to the right of a cluster's start, whose text is empty. `bg` is [`DEFAULT_BG`] for the
/// terminal's own.
pub fn layout_rows() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{BufRead, Write};
    #[derive(serde::Deserialize)]
    struct Row {
        m: String,
        w: usize,
        h: usize,
        ch: Vec<u32>,
        fg: Vec<u32>,
        bg: Vec<u32>,
        at: Vec<u8>,
    }
    let stdin = std::io::stdin();
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let row: Row = serde_json::from_str(&line)?;
        let method = match row.m.as_str() {
            "wcwidth" => WidthMethod::Wcwidth,
            "unicode-wide" => WidthMethod::UnicodeWide,
            _ => WidthMethod::Unicode,
        };
        let mut canvas = Canvas::new(row.w, row.h);
        for (i, cell) in canvas.cells.iter_mut().enumerate() {
            cell.ch = char::from_u32(row.ch[i]).unwrap_or('\u{fffd}');
            cell.fg = row.fg[i];
            cell.bg = row.bg[i];
            cell.bold = row.at[i] & 1 != 0;
        }
        let laid = lay_out(&canvas, method);
        let cells: Vec<serde_json::Value> = laid
            .cells
            .iter()
            .map(|c| {
                let (kind, text, n) = match c.glyph {
                    Glyph::Plain(ch) => (0, ch.to_string(), 1),
                    Glyph::Start { id, width } => (1, laid.text(id).into_owned(), width),
                    Glyph::Cont { left, .. } => (2, String::new(), left),
                };
                serde_json::json!([kind, text, n, c.fg, c.bg, c.bold])
            })
            .collect();
        serde_json::to_writer(&mut out, &cells)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

/// `t3-agents-core --layout-strings`: the exhaustive mode of checks/scripts/cluster-fuzz.ts. Each
/// input line is `<u|w|x> <columns> <code points in hex, comma separated>`: the code points as one
/// run from column 0 of a row that wide, in one style. Each output line is the row as a compact
/// signature: per cell `P<code point>` for a plain cell, `S<width>.<UTF-16 length>` for a
/// cluster's start, `C<left>` for the rest of it, separated by spaces.
pub fn layout_strings() -> Result<(), Box<dyn std::error::Error>> {
    use std::fmt::Write as _;
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let mut line_out = String::new();
    for line in stdin.lock().lines() {
        let line = line?;
        let mut parts = line.split(' ');
        let method = match parts.next() {
            Some("w") => WidthMethod::Wcwidth,
            Some("x") => WidthMethod::UnicodeWide,
            _ => WidthMethod::Unicode,
        };
        let w: usize = parts.next().ok_or("columns")?.parse()?;
        let text: String = parts
            .next()
            .unwrap_or("")
            .split(',')
            .filter(|p| !p.is_empty())
            .map(|p| u32::from_str_radix(p, 16).ok().and_then(char::from_u32).unwrap_or('\u{fffd}'))
            .collect();
        let mut laid = Laid::new(w, 1);
        laid.draw_text(&text, 0, 0, 0, 0, false, method);
        line_out.clear();
        for c in &laid.cells {
            if !line_out.is_empty() {
                line_out.push(' ');
            }
            match c.glyph {
                Glyph::Plain(ch) => write!(line_out, "P{:x}", ch as u32)?,
                Glyph::Start { id, width } => write!(line_out, "S{width}.{}", laid.text(id).encode_utf16().count())?,
                Glyph::Cont { left, .. } => write!(line_out, "C{left}")?,
            }
        }
        out.write_all(line_out.as_bytes())?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widths(s: &str, m: WidthMethod) -> Vec<(String, u32)> {
        clusters(s, m).into_iter().map(|(a, b, w)| (s[a..b].to_string(), w)).collect()
    }

    #[test]
    fn the_clusters_seen_in_the_original() {
        use WidthMethod::*;
        // Measured with OpenTUI 0.8.3's drawText (checks/scripts/cluster-fuzz.ts has the harness).
        assert_eq!(widths("a\u{1f44d}\u{1f3fd}b", Unicode), [("a".into(), 1), ("\u{1f44d}\u{1f3fd}".into(), 2), ("b".into(), 1)]);
        assert_eq!(widths("\u{1f44d}\u{1f3fd}", Wcwidth), [("\u{1f44d}\u{1f3fd}".into(), 4)]);
        assert_eq!(widths("1\u{20e3}", Unicode), [("1\u{20e3}".into(), 1)]);
        assert_eq!(widths("\u{1f1ef}\u{1f1f5}\u{1f1e7}", Unicode), [("\u{1f1ef}\u{1f1f5}".into(), 2), ("\u{1f1e7}".into(), 1)]);
        assert_eq!(widths("\u{1f3f3}", Unicode), [("\u{1f3f3}".into(), 1)]);
        assert_eq!(widths("\u{2764}\u{fe0f}", Unicode), [("\u{2764}\u{fe0f}".into(), 2)]);
        // A modifier after something that is not a base stands alone.
        assert_eq!(widths("x\u{1f3fd}", Unicode), [("x".into(), 1), ("\u{1f3fd}".into(), 2)]);
        assert_eq!(widths("\u{fffd}\u{301}", Unicode), [("\u{fffd}".into(), 1), ("\u{301}".into(), 0)]);
        // A lone Hangul vowel takes a cell here; tmux draws it with no width, the one cluster the
        // real-tmux check leaves open (terminal-parity.ts, clusterCase).
        assert_eq!(widths(" \u{1161}", Unicode), [(" ".into(), 1), ("\u{1161}".into(), 1)]);
    }

    #[test]
    fn a_shrunk_run_leaves_cleared_cells_and_a_cut_cluster_is_dropped() {
        let mut laid = Laid::new(6, 1);
        laid.draw_text("\u{1f44d}\u{1f3fd}z", 0, 0, 1, 2, true, WidthMethod::Unicode);
        assert!(matches!(laid.cells[0].glyph, Glyph::Start { width: 2, .. }));
        assert!(matches!(laid.cells[1].glyph, Glyph::Cont { left: 1, .. }));
        assert_eq!(laid.cells[2].glyph, Glyph::Plain('z'));
        assert_eq!(laid.cells[3], LaidCell::CLEARED);
        // At the edge: a two-cell cluster with one column left is not drawn, nor what follows.
        let mut laid = Laid::new(3, 1);
        laid.draw_text("ab\u{4f60}c", 0, 0, 1, 2, false, WidthMethod::Unicode);
        assert_eq!(laid.cells[2], LaidCell::CLEARED);
    }

    #[test]
    fn writing_over_half_of_a_wide_cluster_blanks_the_other_half() {
        let mut laid = Laid::new(4, 1);
        laid.draw_text("\u{4f60}\u{4f60}", 0, 0, 1, 2, false, WidthMethod::Unicode);
        laid.draw_text("x", 1, 0, 3, 4, false, WidthMethod::Unicode);
        assert_eq!(laid.cells[0].glyph, Glyph::Plain(' '));
        assert_eq!((laid.cells[0].fg, laid.cells[0].bg), (1, 2), "a blanked cell keeps its colors");
        assert_eq!(laid.cells[1].glyph, Glyph::Plain('x'));
        assert!(matches!(laid.cells[2].glyph, Glyph::Start { width: 2, .. }));
    }

    /// What OpenTUI would choose after these replies, each one a chunk read from the terminal.
    fn chosen(vars: &[(&str, &str)], replies: &[&str]) -> WidthMethod {
        let mut env = TermEnv::from_pairs(vars.iter().copied());
        let mut mode = None;
        for reply in replies {
            if let Some(at) = reply.find("?2027;") {
                mode = reply.as_bytes().get(at + 6).map(|b| b - b'0');
            }
            if let Some(at) = reply.find("\x1bP>|") {
                let rest = &reply[at + 4..];
                env.set_xtversion(&rest[..rest.find("\x1b\\").unwrap_or(rest.len())]);
            }
        }
        select_width_method(&env, mode)
    }

    /// The rows of OpenTUI's own test of `selectWidthMethod` (`terminal_test.zig`, "width method
    /// follows the environment, then the DECRPM 2027 reply and XTVERSION"), apart from its
    /// `no_zwj` rows. Replies are measured in the terminals named.
    #[test]
    fn width_method_follows_opentui_s_own_table() {
        use WidthMethod::{Unicode as U, UnicodeWide as X, Wcwidth as W};
        const SSH: (&str, &str) = ("SSH_CONNECTION", "192.0.2.1 54231 192.0.2.2 22");
        const TMUX: (&str, &str) = ("TMUX", "/tmp/tmux-1000/default,12345,0");
        const GHOSTTY: [(&str, &str); 2] = [("TERM_PROGRAM", "ghostty"), ("TERM_PROGRAM_VERSION", "1.3.1")];
        let tmux38 = [TMUX, ("TERM_PROGRAM", "tmux"), ("TERM_PROGRAM_VERSION", "3.8")];
        let rows: Vec<(Vec<(&str, &str)>, Vec<&str>, WidthMethod)> = vec![
            (vec![], vec![], U),
            (vec![("TERM_PROGRAM", "WezTerm")], vec![], U),
            (vec![("TERM_PROGRAM", "WezTerm"), ("VTE_VERSION", "7800")], vec![], U),
            (GHOSTTY.to_vec(), vec![], X),
            ([&GHOSTTY[..], &[("TERM", "xterm-ghostty")]].concat(), vec![], X),
            (vec![("TERM_PROGRAM", "ghostty"), ("TERM_PROGRAM_VERSION", "1.2.3")], vec![], U),
            (vec![("TERM_PROGRAM", "ghostty"), ("TERM_PROGRAM_VERSION", "0.0.0-20260223.r14707.gc61f184")], vec![], U),
            (vec![("TERM_PROGRAM", "ghostty"), ("TERM_PROGRAM_VERSION", "0.0.0-20260224.r14762.gc51f0d7")], vec![], X),
            ([&GHOSTTY[..], &[SSH]].concat(), vec![], U),
            ([&GHOSTTY[..], &[("TERM", "xterm-kitty")]].concat(), vec![], U),
            ([&GHOSTTY[..], &[("TERM", "foot")]].concat(), vec![], U),
            ([&GHOSTTY[..], &[("TERM", "alacritty")]].concat(), vec![], W),
            (vec![("TERM", "rxvt-unicode-256color")], vec![], W),
            (vec![("TERM_PROGRAM", "vscode")], vec![], W),
            (vec![("TERM_PROGRAM", "Apple_Terminal")], vec![], W),
            (vec![("TERM", "xterm-256color"), ("VTE_VERSION", "7800")], vec![], W),
            (vec![("XTERM_VERSION", "XTerm(411)")], vec![], W),
            (vec![("ALACRITTY_LOG", "/tmp/Alacritty-1.log")], vec![], W),
            (vec![("ALACRITTY_SOCKET", "/run/user/1000/Alacritty-wayland-1-1.sock")], vec![], W),
            (vec![("TERMUX_VERSION", "0.118")], vec![], W),
            (vec![("VHS_RECORD", "1")], vec![], W),
            (vec![("ZELLIJ", "0")], vec![], W),
            (vec![("TERM", "screen")], vec![], W),
            (vec![("TERM", "tmux-256color")], vec![], W),
            ([&[TMUX][..], &GHOSTTY[..]].concat(), vec![], W),
            (vec![TMUX, ("TERM_PROGRAM", "iTerm.app"), ("TERM_PROGRAM_VERSION", "3.7.6")], vec![], W),
            (vec![TMUX, ("TERM_PROGRAM", "tmux"), ("TERM_PROGRAM_VERSION", "3.6a")], vec![], U),
            (vec![TMUX, ("TERM_PROGRAM", "tmux"), ("TERM_PROGRAM_VERSION", "3.5a")], vec![], W),
            (vec![TMUX, ("TERM_PROGRAM", "tmux"), ("TERM_PROGRAM_VERSION", "3.5a")], vec!["\x1bP>|tmux 3.7c\x1b\\"], U),
            (vec![TMUX, ("TERM_PROGRAM", "tmux"), ("TERM_PROGRAM_VERSION", "3.7c")], vec!["\x1bP>|tmux 3.5a\x1b\\"], W),
            (vec![], vec!["\x1b[?2027;0$y\x1bP>|tmux 3.7c\x1b\\"], U),
            (vec![], vec!["\x1b]0;tmux\x07\x1bP>|kitty(0.40.1)\x1b\\"], U),
            // Alacritty, urxvt and conhost answer 2027;0$y and no XTVERSION.
            (vec![("TERM", "alacritty")], vec!["\x1b[?2027;0$y"], W),
            (vec![], vec!["\x1b[?2027;0$y"], W),
            (vec![("TERM", "xterm-256color")], vec!["\x1b[?2027;0$y\x1bP>|XTerm(411)\x1b\\"], W),
            (vec![], vec!["\x1bP>|XTerm(411)\x1b\\"], W),
            (vec![("VTE_VERSION", "7800")], vec!["\x1bP>|VTE(7800)\x1b\\"], W),
            (vec![("TERM_PROGRAM", "vscode")], vec!["\x1bP>|xterm.js(5.5.0)\x1b\\"], W),
            (vec![("TERM_PROGRAM", "vscode")], vec!["\x1b[?2027;2$y\x1bP>|xterm.js(7.0.0)\x1b\\"], U),
            (vec![], vec!["\x1bP>|Zellij(4501)\x1b\\"], W),
            (vec![("TERM", "foot")], vec!["\x1b[?2027;4$y\x1bP>|foot(1.28.0)\x1b\\"], W),
            (vec![("TERM", "foot")], vec!["\x1b[?2027;1$y\x1bP>|foot(1.28.0)\x1b\\"], U),
            ([&GHOSTTY[..], &[("TERM", "xterm-kitty")]].concat(), vec!["\x1b[?2027;0$y"], U),
            (GHOSTTY.to_vec(), vec!["\x1b[?2027;0$y"], W),
            (GHOSTTY.to_vec(), vec!["\x1b[?2027;0$y", "\x1bP>|kitty(0.48.2)\x1b\\"], U),
            (GHOSTTY.to_vec(), vec!["\x1b[?2027;2$y\x1bP>|WezTerm 20240203-110809-5046fc22\x1b\\"], U),
            (GHOSTTY.to_vec(), vec!["\x1b[?2027;2$y\x1bP>|ghostty 1.3.1\x1b\\"], X),
            ([&GHOSTTY[..], &[("OPENTUI_FORCE_WCWIDTH", "1")]].concat(), vec!["\x1b[?2027;2$y\x1bP>|ghostty 1.3.1\x1b\\"], W),
            (vec![SSH], vec!["\x1b[?2027;1$y\x1bP>|ghostty 1.3.1\x1b\\"], X),
            (vec![], vec!["\x1b[?2027;1$y\x1bP>|ghostty 1.1.3\x1b\\"], U),
            (vec![], vec!["\x1b[?2027;2$y\x1bP>|libghostty\x1b\\"], X),
            (vec![("TERM_PROGRAM", "iTerm.app")], vec!["\x1b[?2027;4$y\x1bP>|iTerm2 3.7.6\x1b\\"], U),
            (vec![("TERM_PROGRAM", "WezTerm")], vec!["\x1b[?2027;3$y\x1bP>|WezTerm 20260716-195552-76b606ec\x1b\\"], U),
            (vec![("WT_SESSION", "fixture")], vec!["\x1b[?2027;3$y"], U),
            (vec![("OPENTUI_FORCE_WCWIDTH", "1")], vec!["\x1bP>|kitty(0.48.2)\x1b\\"], W),
            (vec![("OPENTUI_FORCE_UNICODE", "1")], vec!["\x1b[?2027;0$y"], U),
            (vec![("OPENTUI_FORCE_UNICODE", "1"), ("OPENTUI_FORCE_WCWIDTH", "1")], vec![], U),
            ([&tmux38[..], &[SSH, ("OPENTUI_FORCE_WCWIDTH", "1")]].concat(), vec!["\x1bP>|tmux 3.8\x1b\\", "\x1b[?2027;0$y"], W),
            ([&GHOSTTY[..], &[SSH, ("OPENTUI_FORCE_WCWIDTH", "1")]].concat(), vec!["\x1b[?2027;1$y\x1bP>|ghostty 1.3.1\x1b\\"], W),
            (vec![SSH, ("TERM", "alacritty"), ("OPENTUI_FORCE_UNICODE", "1")], vec!["\x1b[?2027;0$y"], U),
            // Inside tmux 3.8: tmux itself, screen, Zellij, Alacritty and urxvt, and tmux inside
            // screen or Zellij.
            (tmux38.to_vec(), vec!["\x1bP>|tmux 3.8\x1b\\", "\x1b[?2027;0$y"], U),
            ([&tmux38[..], &[("STY", "1.pts-0.box"), ("TERM", "screen")]].concat(), vec!["\x1b[?1;2c"], W),
            ([&tmux38[..], &[("ZELLIJ", "0")]].concat(), vec![], W),
            ([&tmux38[..], &[("ZELLIJ", "0")]].concat(), vec!["\x1bP>|Zellij(4501)\x1b\\"], W),
            ([&tmux38[..], &[("TERM", "alacritty")]].concat(), vec!["\x1b[?2027;0$y\x1b[?6c"], W),
            ([&tmux38[..], &[("TERM", "rxvt-unicode-256color")]].concat(), vec!["\x1b[?2027;0$y\x1b[?1;2c"], W),
            ([&tmux38[..], &[("STY", "1.pts-0.box")]].concat(), vec!["\x1bP>|tmux 3.8\x1b\\"], U),
            ([&tmux38[..], &[("ZELLIJ", "0")]].concat(), vec!["\x1bP>|tmux 3.8\x1b\\"], U),
            (vec![("TERM", "xterm-256color"), ("XTERM_VERSION", "XTerm(411)"), ("KONSOLE_VERSION", "250400")], vec![], U),
        ];
        for (n, (vars, replies, want)) in rows.iter().enumerate() {
            assert_eq!(chosen(vars, replies), *want, "row {n}: {vars:?} {replies:?}");
        }
    }

    #[test]
    fn xtversion_answers_are_split_as_opentui_splits_them() {
        let split = |s: &str| Xtversion::parse(s).map(|x| (x.name, x.version));
        assert_eq!(split("kitty(0.49.2)"), Some(("kitty".into(), "0.49.2".into())));
        assert_eq!(split("ghostty 1.4.0"), Some(("ghostty".into(), "1.4.0".into())));
        assert_eq!(split("libghostty"), Some(("libghostty".into(), String::new())));
        assert_eq!(split("WezTerm 20260716-195552-76b606ec"), Some(("WezTerm".into(), "20260716-195552-76b606ec".into())));
        assert_eq!(split("foo(1.0"), Some(("foo".into(), String::new())));
        assert_eq!(split(""), None);
    }

    #[test]
    fn ghostty_versions_that_widen_clusters() {
        for (v, want) in [
            ("1.3.0", true), ("1.3.1", true), ("1.4.0", true), ("2.0.0", true), ("1.2.9", false), ("1.2", false), ("1.3", false), ("1.3.0.1", false),
            ("1.3.0-5-gabc123", true), ("1.3.0-5-gxyz", false), ("1.3.0-g1", false), ("1.2.0-9-gabc", false),
            ("0.0.0-20260224.r14762.gc51f0d7", true), ("0.0.0-20260223.r14707.gc61f184", false), ("0.0.0-2026022x.r1", false), ("", false),
        ] {
            assert_eq!(ghostty_wide_grapheme_widths(v), want, "{v}");
        }
    }

    /// The clusters whose width differs between OpenTUI's `unicode` and `unicode-wide`, measured
    /// with the original's own buffer (`cluster-fuzz.ts` is the proof that all others agree).
    #[test]
    fn unicode_wide_changes_three_clusters_in_the_thread_titles() {
        use WidthMethod::*;
        let w = |s: &str, m| text_width(s, m);
        // A pointing hand with a skin tone: two cells under unicode, one under unicode-wide.
        assert_eq!((w("\u{261d}\u{1f3fc}", Unicode), w("\u{261d}\u{1f3fc}", UnicodeWide)), (2, 1));
        // A Devanagari consonant with a vowel sign, and a Bengali conjunct: one cell, then two.
        assert_eq!((w("\u{0928}\u{093e}", Unicode), w("\u{0928}\u{093e}", UnicodeWide)), (1, 2));
        assert_eq!((w("\u{0995}\u{09cd}\u{09b7}", Unicode), w("\u{0995}\u{09cd}\u{09b7}", UnicodeWide)), (1, 2));
        // And what both agree on.
        for s in ["\u{1f44d}\u{1f3fd}", "\u{1f1ef}\u{1f1f5}", "\u{2764}\u{fe0f}", "e\u{301}", "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}"] {
            assert_eq!(w(s, Unicode), w(s, UnicodeWide), "{s:?}");
        }
    }

    #[test]
    fn width_method_follows_the_original_s_rules() {
        let env = |pairs: &[(&str, &str)]| TermEnv::from_pairs(pairs.iter().copied());
        let foot = env(&[("TERM", "foot")]);
        assert_eq!(select_width_method(&foot, None), WidthMethod::Unicode);
        assert_eq!(select_width_method(&foot, Some(2)), WidthMethod::Unicode);
        assert_eq!(select_width_method(&foot, Some(1)), WidthMethod::Unicode);
        let plain = env(&[("TERM", "xterm-256color")]);
        assert_eq!(select_width_method(&plain, Some(0)), WidthMethod::Wcwidth);
        assert_eq!(select_width_method(&plain, None), WidthMethod::Unicode);
        let kitty = env(&[("TERM", "xterm-kitty")]);
        assert_eq!(select_width_method(&kitty, Some(0)), WidthMethod::Unicode);
        let mut tmux = env(&[("TMUX", "/tmp/x,1,0"), ("TERM", "tmux-256color"), ("TERM_PROGRAM", "tmux"), ("TERM_PROGRAM_VERSION", "3.8")]);
        // Before tmux has answered XTVERSION, a 2027 answer of 0 reads as a terminal that draws code points.
        assert_eq!(select_width_method(&tmux, Some(0)), WidthMethod::Wcwidth);
        tmux.set_xtversion("tmux 3.8");
        assert_eq!(select_width_method(&tmux, Some(0)), WidthMethod::Unicode);
        assert!(explicit_cursor_positioning(&tmux));
        let old_tmux = env(&[("TMUX", "/tmp/x,1,0"), ("TERM_PROGRAM", "tmux"), ("TERM_PROGRAM_VERSION", "3.5a")]);
        assert_eq!(select_width_method(&old_tmux, None), WidthMethod::Wcwidth);
        assert!(!explicit_cursor_positioning(&foot));
        assert!(tmux_graphemes("next-3.8") && tmux_graphemes("3.6a") && !tmux_graphemes("openbsd-7.8"));
    }
}
