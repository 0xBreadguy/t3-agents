//! Diff a cell grid into capability-selected terminal color bytes, and the inverse used by tests.
//!
//! The grid is first laid out the way the original's renderer lays it out
//! ([`t3_agents_core::cluster::lay_out`]): each run of identically styled cells is regrouped into
//! grapheme clusters, each cluster gets the width OpenTUI gives it and one cell, and the run is
//! packed to the left. That buffer, not the canvas, is what is diffed and written: a cluster as one
//! piece of text, its later cells never.
//!
//! Autowrap is off for the session (`DECAWM`), so a character written in the last
//! column stays there. The encoder and the virtual screen share that rule.
//!
//! In a terminal that speaks the text-sizing protocol (kitty's OSC 66, which foot implements for
//! the `w` key), [`Encoder::set_explicit_width`] writes every non-ASCII cluster as
//! `ESC ] 66 ; w=<columns> ; <text> ESC \`, as the original's renderer does. The terminal
//! then draws that text from the font and takes the width from the sequence, not from its own
//! tables. ASCII is always written bare. Without it, the original trusts the terminal to advance
//! by the cluster's width, except in tmux, screen and Alacritty, where it moves the cursor itself
//! after every cluster; the encoder does the same, and moves it where the original starts a new
//! run after a cluster the terminal may measure differently.
use std::io::Write;
use t3_agents_core::cluster::{
    self, DEFAULT_BG, Glyph, Laid, LaidCell, TermEnv, WidthMethod, explicit_cursor_positioning,
    select_width_method,
};
use t3_agents_core::terminal::{Canvas, Cell};

/// Past this many distinct multi-character cluster texts, the next frame is laid out afresh, so
/// the text table a reused buffer carries from frame to frame cannot grow without bound.
const MAX_KEPT_TEXTS: usize = 4096;

/// Alternate screen, hidden cursor, no autowrap, button-event mouse, SGR encoding, and bracketed
/// paste (2004, which the original also switches on), so pasted text arrives between markers the
/// key parser drops instead of as key presses.
pub const ENTER_TERM: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[?1000h\x1b[?1006h\x1b[?2004h";
/// The reverse, plus an SGR reset, in an order that leaves the normal screen usable.
pub const LEAVE_TERM: &[u8] = b"\x1b[?1000l\x1b[?1006l\x1b[?2004l\x1b[?25h\x1b[?7h\x1b[0m\x1b[?1049l";

pub struct Encoder {
    colors: crate::color::Colors,
    prev: Option<Laid>,
    cursor: (usize, usize),
    cursor_known: bool,
    fg: u32,
    bg: u32,
    bold: bool,
    style_known: bool,
    explicit_width: bool,
    method: WidthMethod,
    env: TermEnv,
    cursor_after_cluster: bool,
    drift: bool,
    /// The terminal's last answer about mode 2027, which the width method follows.
    mode_2027: Option<u8>,
    /// The canvas `prev` was laid out from, with the width method used, when `prev` came from
    /// [`Encoder::encode`]. A row equal to the same row here lays out to the same cells, so it
    /// is neither laid out nor compared again.
    prev_canvas: Option<(Vec<Cell>, WidthMethod)>,
    /// Scratch kept between frames: one row of the previous buffer, the spans to write, and the
    /// run text for the layout.
    old_row: Vec<LaidCell>,
    spans: Vec<usize>,
    text: String,
}

impl Encoder {
    /// An encoder for a terminal nothing is known about: [`WidthMethod::Unicode`], and the
    /// terminal trusted to advance by each cluster's width.
    pub fn new() -> Self {
        Self::for_terminal(TermEnv::default())
    }

    /// An encoder for the terminal this environment describes, as the original reads it before any
    /// answer: which width method, and whether to move the cursor after every cluster.
    pub fn for_terminal(env: TermEnv) -> Self {
        Self {
            colors: crate::color::Colors::default(),
            prev: None,
            cursor: (0, 0),
            cursor_known: false,
            fg: 0,
            bg: 0,
            bold: false,
            style_known: false,
            explicit_width: false,
            method: select_width_method(&env, None),
            cursor_after_cluster: explicit_cursor_positioning(&env),
            env,
            drift: false,
            mode_2027: None,
            prev_canvas: None,
            old_row: Vec::new(),
            spans: Vec::new(),
            text: String::new(),
        }
    }

    /// Write non-ASCII cells with an explicit width from now on. Cells already on the terminal
    /// were drawn the other way, so the caller repaints with `full` after changing this.
    pub fn set_explicit_width(&mut self, on: bool) {
        self.explicit_width = on;
    }

    /// The terminal's answer about mode 2027 (`CSI ? 2027 ; <value> $ y`), which can change the
    /// width method as it does in the original. True when it did: the caller repaints with `full`.
    pub fn terminal_mode_2027(&mut self, value: u8) -> bool {
        self.mode_2027 = Some(value);
        self.reconsider()
    }

    /// The terminal's answer to XTVERSION (the text between `DCS > |` and ST), which names the
    /// terminal as the original reads it: for the width method, and for whether to move the cursor
    /// after every cluster. True when either changed: the caller repaints with `full`.
    pub fn terminal_xtversion(&mut self, text: &str) -> bool {
        self.env.set_xtversion(text);
        let color = self.colors.version(text);
        self.reconsider() || color
    }

    /// Read the real terminal color environment; standalone encoders default to RGB.
    pub fn process_color_environment(&mut self) {
        self.colors = crate::color::Colors::from_process();
    }
    /// The terminal answered that it has full color. True when that is news: the caller repaints.
    pub fn terminal_truecolor(&mut self) -> bool {
        self.colors.truecolor()
    }
    /// The terminal reported one of its sixteen base colors. True when it changes which palette
    /// entry is nearest to some color: the caller repaints.
    pub fn terminal_palette(&mut self, index: u8, rgb: u32) -> bool {
        self.colors.palette(index, rgb)
    }

    /// Choose again from everything the terminal has said, as the original does on every answer.
    fn reconsider(&mut self) -> bool {
        let method = select_width_method(&self.env, self.mode_2027);
        let cursor = explicit_cursor_positioning(&self.env);
        let changed = method != self.method || cursor != self.cursor_after_cluster;
        self.method = method;
        self.cursor_after_cluster = cursor;
        changed
    }

    /// Set the width method directly (tests and checks).
    pub fn set_width_method(&mut self, method: WidthMethod) {
        self.method = method;
    }

    pub fn width_method(&self) -> WidthMethod {
        self.method
    }

    /// Move the cursor after every cluster written without an explicit width (tests and checks).
    pub fn set_cursor_after_cluster(&mut self, on: bool) {
        self.cursor_after_cluster = on;
    }

    /// Bytes for this frame. An unchanged diff is empty: no synchronized-output
    /// wrapper and no cursor move. `full` repaints every cell (first frame, resize).
    pub fn encode(&mut self, canvas: &Canvas, full: bool) -> Vec<u8> {
        let reusable = !full
            && self.prev.as_ref().is_some_and(|p| {
                p.w == canvas.w && p.h == canvas.h && p.text_count() <= MAX_KEPT_TEXTS
            })
            && self.prev_canvas.as_ref().is_some_and(|(cells, method)| {
                *method == self.method && cells.len() == canvas.cells.len()
            });
        if reusable {
            return self.encode_rows(canvas);
        }
        let laid = cluster::lay_out(canvas, self.method);
        let bytes = self.encode_laid_inner(laid, full);
        let mut cells = self.prev_canvas.take().map(|(cells, _)| cells).unwrap_or_default();
        cells.clear();
        cells.extend_from_slice(&canvas.cells);
        self.prev_canvas = Some((cells, self.method));
        bytes
    }

    /// [`Encoder::encode`] when the previous frame was laid out from a canvas of this size with
    /// this width method, and this frame is not a full repaint. The result is byte for byte what
    /// laying out the whole canvas and diffing it with [`Encoder::encode_laid`] gives:
    ///
    /// - A row whose canvas cells equal the previous canvas's lays out to the same cells (a row's
    ///   layout depends on nothing else), every one of them [`Laid::same`] as before, so it adds
    ///   no span and leaves the buffer as it was. Only the other rows are laid out again, in
    ///   place, and compared.
    /// - Within one buffer two cells hold the same text exactly when they hold the same cluster
    ///   id (`intern` gives one id per text), so comparing a re-laid row with its old cells by
    ///   value is [`Laid::same`].
    /// - Spans are collected in index order, as the full diff collects them, and written with the
    ///   same `place`, so the encoder's cursor and style state moves through the same steps.
    /// - When nothing differs the frame is empty and the buffer that stays equals the old one
    ///   cell for cell, which is all a later diff reads.
    fn encode_rows(&mut self, canvas: &Canvas) -> Vec<u8> {
        let mut laid = self.prev.take().expect("checked by encode");
        let (mut prev_cells, method) = self.prev_canvas.take().expect("checked by encode");
        let w = laid.w;
        let mut spans = std::mem::take(&mut self.spans);
        spans.clear();
        let mut changed = false;
        for y in 0..laid.h {
            let row = y * w..(y + 1) * w;
            if canvas.cells[row.clone()] == prev_cells[row.clone()] {
                continue;
            }
            prev_cells[row.clone()].copy_from_slice(&canvas.cells[row.clone()]);
            self.old_row.clear();
            self.old_row.extend_from_slice(&laid.cells[row.clone()]);
            laid.relay_row(canvas, y, method, &mut self.text);
            for (k, (new, old)) in laid.cells[row.clone()].iter().zip(&self.old_row).enumerate() {
                if new == old {
                    continue;
                }
                changed = true;
                if !matches!(new.glyph, Glyph::Cont { .. }) {
                    spans.push(row.start + k);
                }
            }
        }
        self.prev_canvas = Some((prev_cells, method));
        if !changed {
            self.prev = Some(laid);
            self.spans = spans;
            return Vec::new();
        }
        let mut out = Vec::with_capacity(laid.w * laid.h);
        out.extend_from_slice(b"\x1b[?2026h");
        for &i in &spans {
            self.place(&mut out, i % w, i / w, &laid, i, false);
        }
        out.extend_from_slice(b"\x1b[?2026l");
        self.prev = Some(laid);
        self.spans = spans;
        out
    }

    /// [`Encoder::encode`] for a buffer already laid out.
    pub fn encode_laid(&mut self, laid: Laid, full: bool) -> Vec<u8> {
        // The canvas behind this buffer is not known: the next frame lays out every row.
        self.prev_canvas = None;
        self.encode_laid_inner(laid, full)
    }

    fn encode_laid_inner(&mut self, laid: Laid, mut full: bool) -> Vec<u8> {
        let resized = self
            .prev
            .as_ref()
            .is_some_and(|p| p.w != laid.w || p.h != laid.h);
        if resized {
            full = true;
        }
        if !full && self.prev.as_ref().is_some_and(|p| p.same_frame(&laid)) {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(laid.w * laid.h);
        out.extend_from_slice(b"\x1b[?2026h");
        if full {
            // Drop unknown style. A full frame writes every cell over what is there and does
            // not clear the screen, like the original's: where a terminal measures a cluster
            // differently from the layout, some cell is not written, and it keeps what was there.
            out.extend_from_slice(b"\x1b[0m");
            if resized {
                // After a resize the screen is cleared first, which the original does not do.
                // This program starts fast enough to draw before a new window has its final size;
                // the original, slower to start, draws once at the final size, on an empty
                // screen. Clearing here gives the same picture at start-up.
                out.extend_from_slice(b"\x1b[2J");
            }
            self.cursor_known = false;
            self.style_known = false;
            self.bold = false;
            self.drift = false;
        }
        // Cells are decided against the previous buffer before `place` mutates the encoder. A
        // cluster's later cells are never written: the cluster covers them.
        let mut spans: Vec<usize> = Vec::new();
        {
            let prev = if full { None } else { self.prev.as_ref() };
            for (i, cell) in laid.cells.iter().enumerate() {
                if matches!(cell.glyph, Glyph::Cont { .. }) {
                    continue;
                }
                if prev.is_none_or(|p| !p.same(i, &laid, i)) {
                    spans.push(i);
                }
            }
        }
        let w = laid.w;
        for i in spans {
            self.place(&mut out, i % w, i / w, &laid, i, full);
        }
        out.extend_from_slice(b"\x1b[?2026l");
        self.prev = Some(laid);
        out
    }

    fn place(&mut self, out: &mut Vec<u8>, x: usize, y: usize, laid: &Laid, i: usize, full: bool) {
        let cell = laid.cells[i];
        let cols = laid.w;
        let restyle = !self.style_known || self.fg != cell.fg || self.bg != cell.bg || self.bold != cell.bold;
        // The original starts a new run, with a cursor move, at every change of style. After
        // printable ASCII the cursor is where that move would put it in any terminal, so the move
        // is left out. After anything else it is written, as the original writes it.
        if !self.cursor_known || self.cursor != (x, y) || (self.drift && restyle) {
            let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
            self.cursor = (x, y);
            self.cursor_known = true;
            self.drift = false;
        }
        if restyle {
            emit_sgr(out, self.style_known, self.fg, self.bg, self.bold, cell, &mut self.colors);
            self.fg = cell.fg;
            self.bg = cell.bg;
            self.bold = cell.bold;
            self.style_known = true;
        }
        let width = match cell.glyph {
            Glyph::Plain(c) => {
                let mut tmp = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                if !c.is_ascii() {
                    self.drift = true;
                }
                1
            }
            Glyph::Start { id, width } => {
                let text = laid.text(id);
                let width = width as usize;
                if self.explicit_width {
                    // OpenTUI writes at most 7 in the sequence.
                    let _ = write!(out, "\x1b]66;w={};", width.min(7));
                    out.extend_from_slice(text.as_bytes());
                    out.extend_from_slice(b"\x1b\\");
                    if width > 7 {
                        self.drift = true;
                    }
                } else {
                    out.extend_from_slice(text.as_bytes());
                    if self.cursor_after_cluster || (!full && width > 1) {
                        // In an update the original's run ends after every wide cluster (its
                        // later cells already match), and the next cell starts with a cursor
                        // move. A full frame is one unbroken pass.
                        self.cursor_known = false;
                    } else {
                        self.drift = true;
                    }
                }
                width
            }
            Glyph::Cont { .. } => 0,
        };
        self.cursor.0 = advance(x, width.max(1), cols);
    }
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn advance(x: usize, width: usize, cols: usize) -> usize {
    let next = x.saturating_add(width);
    if cols == 0 {
        0
    } else if next < cols {
        next
    } else {
        cols - 1
    }
}

fn emit_sgr(out: &mut Vec<u8>, known: bool, fg: u32, bg: u32, bold: bool, cell: LaidCell, colors: &mut crate::color::Colors) {
    out.extend_from_slice(b"\x1b[");
    let mut sep = false;
    let mut push_sep = |out: &mut Vec<u8>| {
        if sep {
            out.push(b';');
        }
        sep = true;
    };
    if !known {
        if cell.bold {
            out.extend_from_slice(b"1;");
        }
        push_color(out, 38, cell.fg, colors);
        out.push(b';');
        push_color(out, 48, cell.bg, colors);
        out.push(b'm');
        return;
    }
    if bold != cell.bold {
        push_sep(out);
        out.extend_from_slice(if cell.bold { b"1" } else { b"22" });
    }
    if fg != cell.fg {
        push_sep(out);
        push_color(out, 38, cell.fg, colors);
    }
    if bg != cell.bg {
        push_sep(out);
        push_color(out, 48, cell.bg, colors);
    }
    out.push(b'm');
}

fn push_color(out: &mut Vec<u8>, kind: u8, rgb: u32, colors: &mut crate::color::Colors) {
    if rgb == DEFAULT_BG {
        // The terminal's own color: 39 or 49.
        let _ = write!(out, "{}", kind + 1);
        return;
    }
    if colors.indexed() {
        let _ = write!(out, "{kind};5;{}", colors.nearest(rgb));
        return;
    }
    let _ = write!(
        out,
        "{kind};2;{};{};{}",
        (rgb >> 16) & 255,
        (rgb >> 8) & 255,
        rgb & 255
    );
}

/// One cell of the virtual screen: the text drawn there (empty for a cell a wider glyph to its
/// left covers) and its style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenCell {
    pub text: String,
    pub fg: u32,
    pub bg: u32,
    pub bold: bool,
}

/// Virtual terminal covering the sequences this presenter emits. It clusters bare text the way
/// the original's renderer measures it ([`WidthMethod::Unicode`] unless told otherwise), so it
/// stands for a terminal that agrees with the original about widths.
pub struct Screen {
    pub w: usize,
    pub h: usize,
    pub cells: Vec<ScreenCell>,
    x: usize,
    y: usize,
    fg: u32,
    bg: u32,
    bold: bool,
    esc: Vec<u8>,
    utf: Vec<u8>,
    /// Width in columns of text-sizing text seen so far, per cell index: 0 for bare text.
    pub explicit: Vec<u8>,
    /// How bare text is measured.
    pub method: WidthMethod,
    /// The bare glyph just written, which the next bare code point may join: its cell index.
    last: Option<usize>,
}

impl Screen {
    pub fn new(w: usize, h: usize) -> Self {
        let blank = Canvas::new(w, h);
        let cell = ScreenCell {
            text: " ".into(),
            fg: 0xff_ffff,
            bg: 0,
            bold: false,
        };
        Self {
            w: blank.w,
            h: blank.h,
            cells: vec![cell; blank.w * blank.h],
            x: 0,
            y: 0,
            fg: 0xff_ffff,
            bg: 0,
            bold: false,
            esc: Vec::new(),
            utf: Vec::new(),
            explicit: vec![0; w.max(1) * h.max(1)],
            method: WidthMethod::Unicode,
            last: None,
        }
    }

    pub fn resize(&mut self, w: usize, h: usize) {
        let method = self.method;
        *self = Self::new(w, h);
        self.method = method;
    }

    pub fn apply(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.feed(b);
        }
    }

    fn feed(&mut self, b: u8) {
        if !self.esc.is_empty() || b == 0x1b {
            self.last = None;
            self.esc.push(b);
            if self.esc.len() == 1 {
                return;
            }
            if self.esc[1] == b'O' {
                if self.esc.len() >= 3 {
                    self.esc.clear();
                }
                return;
            }
            if self.esc[1] == b']' {
                self.osc(b);
                return;
            }
            if self.esc[1] != b'[' {
                self.esc.clear();
                return;
            }
            if self.esc.len() < 3 {
                return;
            }
            let last = *self.esc.last().unwrap();
            if (0x40..=0x7e).contains(&last) {
                let seq = std::mem::take(&mut self.esc);
                self.csi(&seq);
            } else if !(0x20..=0x3f).contains(&last) || self.esc.len() > 64 {
                self.esc.clear();
            }
            return;
        }
        self.push_utf(b);
    }

    /// An operating-system command ends at BEL or at `ESC \\`. Only text sizing is understood:
    /// `66 ; w=<columns> ; <text>`, drawn as one glyph. Everything else is dropped.
    fn osc(&mut self, last: u8) {
        let st = self.esc.ends_with(b"\x1b\\");
        if last != 0x07 && !st {
            if self.esc.len() > 1024 {
                self.esc.clear();
            }
            return;
        }
        let end = self.esc.len() - if st { 2 } else { 1 };
        let body = self.esc[2..end].to_vec();
        self.esc.clear();
        let Some(rest) = body.strip_prefix(b"66;") else {
            return;
        };
        let Some(at) = rest.iter().position(|&b| b == b';') else {
            return;
        };
        let width = std::str::from_utf8(&rest[..at])
            .ok()
            .and_then(|p| p.split(':').find_map(|kv| kv.strip_prefix("w=")))
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        if let Ok(text) = std::str::from_utf8(&rest[at + 1..]) {
            let w = if width == 0 { cluster::text_width(text, self.method) as usize } else { width };
            self.put_glyph(text, w, if width == 0 { 0 } else { width as u8 });
        }
        self.last = None;
    }

    fn csi(&mut self, seq: &[u8]) {
        let final_b = *seq.last().unwrap();
        let body = &seq[2..seq.len() - 1];
        match final_b {
            b'H' | b'f' => {
                let (row, col) = cup_params(body);
                self.y = row.saturating_sub(1).min(self.h.saturating_sub(1));
                self.x = col.saturating_sub(1).min(self.w.saturating_sub(1));
            }
            b'J' => {
                if first_num(body).unwrap_or(0) >= 2 {
                    let fill = ScreenCell {
                        text: " ".into(),
                        fg: self.fg,
                        bg: self.bg,
                        bold: self.bold,
                    };
                    self.cells.fill(fill);
                    self.explicit.fill(0);
                }
            }
            b'm' => self.sgr(body),
            _ => {}
        }
    }

    fn sgr(&mut self, body: &[u8]) {
        let nums = numbers(body);
        if nums.is_empty() {
            self.bold = false;
            return;
        }
        let mut i = 0;
        while i < nums.len() {
            match nums[i] {
                0 => {
                    self.bold = false;
                    i += 1;
                }
                1 => {
                    self.bold = true;
                    i += 1;
                }
                22 => {
                    self.bold = false;
                    i += 1;
                }
                39 => {
                    self.fg = DEFAULT_BG;
                    i += 1;
                }
                49 => {
                    self.bg = DEFAULT_BG;
                    i += 1;
                }
                38 if i + 4 < nums.len() && nums[i + 1] == 2 => {
                    self.fg = rgb(nums[i + 2], nums[i + 3], nums[i + 4]);
                    i += 5;
                }
                48 if i + 4 < nums.len() && nums[i + 1] == 2 => {
                    self.bg = rgb(nums[i + 2], nums[i + 3], nums[i + 4]);
                    i += 5;
                }
                _ => i += 1,
            }
        }
    }

    fn push_utf(&mut self, b: u8) {
        self.utf.push(b);
        let need = match self.utf[0] {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => {
                self.utf.clear();
                return;
            }
        };
        if self.utf.len() < need {
            return;
        }
        let bytes = std::mem::take(&mut self.utf);
        if let Ok(text) = std::str::from_utf8(&bytes) {
            if let Some(ch) = text.chars().next() {
                self.put_bare(ch);
            }
        }
    }

    /// Bare text: a code point that does not start a new cluster joins the glyph just written.
    fn put_bare(&mut self, ch: char) {
        if let Some(at) = self.last {
            let mut joined = self.cells[at].text.clone();
            joined.push(ch);
            if cluster::clusters(&joined, self.method).len() == 1 {
                let width = cluster::text_width(&joined, self.method) as usize;
                self.x = at % self.w;
                self.y = at / self.w;
                self.put_glyph(&joined, width, 0);
                self.last = Some(at);
                return;
            }
        }
        let text = ch.to_string();
        let width = cluster::text_width(&text, self.method) as usize;
        if width == 0 {
            return;
        }
        let at = self.y * self.w + self.x;
        let fits = self.x < self.w && self.y < self.h;
        self.put_glyph(&text, width, 0);
        self.last = fits.then_some(at);
    }

    /// Put one glyph `width` columns wide at the cursor. `explicit` is the width the sender gave
    /// it, or 0 for bare text, whose width the terminal measures itself.
    fn put_glyph(&mut self, text: &str, width: usize, explicit: u8) {
        if self.w == 0 || self.h == 0 || self.y >= self.h || self.x >= self.w {
            return;
        }
        let width = width.max(1);
        let i = self.y * self.w + self.x;
        // Writing over half of a wide glyph blanks the other half.
        if self.cells[i].text.is_empty() && self.x > 0 {
            let mut k = i;
            while k > self.y * self.w && self.cells[k].text.is_empty() {
                k -= 1;
            }
            self.cells[k].text = " ".into();
        }
        self.explicit[i] = explicit;
        self.cells[i] = ScreenCell {
            text: text.to_string(),
            fg: self.fg,
            bg: self.bg,
            bold: self.bold,
        };
        let row_end = (self.y + 1) * self.w;
        for k in 1..width {
            if i + k >= row_end {
                break;
            }
            self.explicit[i + k] = 0;
            self.cells[i + k] = ScreenCell {
                text: String::new(),
                fg: self.fg,
                bg: self.bg,
                bold: self.bold,
            };
        }
        // And whatever the old glyph under its right edge covered.
        let mut k = i + width;
        while k < row_end && self.cells[k].text.is_empty() {
            self.cells[k].text = " ".into();
            k += 1;
        }
        self.x = advance(self.x, width, self.w);
    }
}

/// Whether the screen shows the laid-out buffer: the same text in the same cells, in the same
/// style. A cluster's later cells show nothing of their own.
pub fn screen_matches(screen: &Screen, laid: &Laid) -> Result<(), String> {
    if screen.w != laid.w || screen.h != laid.h {
        return Err(format!("size {}x{} != {}x{}", screen.w, screen.h, laid.w, laid.h));
    }
    for (i, got) in screen.cells.iter().enumerate() {
        let want = laid.cells[i];
        let text = laid.cell_text(i).map(|t| t.into_owned()).unwrap_or_default();
        if got.text != text || got.fg != want.fg || got.bg != want.bg || got.bold != want.bold {
            let x = i % laid.w;
            let y = i / laid.w;
            return Err(format!("cell {x},{y}: got {got:?} want {text:?} {want:?}"));
        }
    }
    Ok(())
}

fn rgb(r: i32, g: i32, b: i32) -> u32 {
    ((r as u32 & 255) << 16) | ((g as u32 & 255) << 8) | (b as u32 & 255)
}

fn numbers(body: &[u8]) -> Vec<i32> {
    let mut out = Vec::new();
    let mut cur: Option<i32> = None;
    let mut any = false;
    for &b in body {
        if b.is_ascii_digit() {
            cur = Some(cur.unwrap_or(0).saturating_mul(10) + i32::from(b - b'0'));
            any = true;
        } else if b == b';' {
            out.push(cur.unwrap_or(0));
            cur = None;
            any = true;
        }
    }
    if cur.is_some() || any {
        out.push(cur.unwrap_or(0));
    }
    out
}

fn first_num(body: &[u8]) -> Option<i32> {
    let nums = numbers(body);
    nums.first().copied()
}

fn cup_params(body: &[u8]) -> (usize, usize) {
    let mut row = None;
    let mut col = None;
    let mut cur: Option<usize> = None;
    let mut field = 0;
    for &b in body {
        if b.is_ascii_digit() {
            cur = Some(cur.unwrap_or(0) * 10 + (b - b'0') as usize);
        } else if b == b';' {
            if field == 0 {
                row = cur;
            }
            field += 1;
            cur = None;
        }
    }
    if field == 0 {
        row = cur;
    } else {
        col = cur;
    }
    (row.unwrap_or(1).max(1), col.unwrap_or(1).max(1))
}

#[cfg(test)]
mod tests {
    #[test]
    fn palette_change_repaints_an_unchanged_canvas_in_one_pass() {
        let mut enc = Encoder::new();
        enc.colors = crate::color::Colors::from_env(|k| (k == "TERM").then(|| "xterm-256color".into()));
        let canvas = Canvas::new(4,2);
        let first = enc.encode(&canvas, true);
        assert!(enc.encode(&canvas, false).is_empty());
        assert!(enc.terminal_palette(0, 0x123456));
        let repaint = enc.encode(&canvas, true);
        assert_eq!(String::from_utf8(first).unwrap().replace("48;5;0m", "48;5;16m"), String::from_utf8(repaint).unwrap());
        assert!(enc.encode(&canvas, false).is_empty());
        assert!(enc.terminal_truecolor());
        assert!(!enc.encode(&canvas,true).is_empty());
    }
    use super::*;
    use crate::{cells_match, compose};
    use t3_agents_core::terminal::CONT;
    use t3_agents_core::model::{Snapshot, State};

    fn round_trip(canvas: &Canvas, full: bool, enc: &mut Encoder, screen: &mut Screen) -> Vec<u8> {
        let bytes = enc.encode(canvas, full);
        if bytes.is_empty() {
            return bytes;
        }
        screen.apply(&bytes);
        cells_match(screen, canvas).unwrap();
        bytes
    }

    #[test]
    fn hand_built_frame_diff_and_identical() {
        let mut canvas = Canvas::new(6, 2);
        canvas.set(0, 0, 'A', 0xaa_0000, Some(0x001122), true);
        canvas.set(1, 0, '你', 0x00_ff00, Some(0x112233), false);
        canvas.set(2, 0, CONT, 0x00_ff00, Some(0x112233), false);
        canvas.set(3, 0, 'Z', 0xff_ffff, Some(0), false);
        canvas.set(0, 1, 'b', 0x0000_ff, Some(0x222222), true);
        let mut enc = Encoder::new();
        let mut screen = Screen::new(6, 2);
        screen.apply(ENTER_TERM);
        let first = round_trip(&canvas, true, &mut enc, &mut screen);
        assert!(!first.is_empty());
        assert!(first.windows(8).any(|w| w == b"\x1b[?2026h"));
        let again = enc.encode(&canvas, false);
        assert!(again.is_empty(), "an unchanged frame writes nothing");
        canvas.set(3, 0, 'Y', 0xff_ee00, Some(0x010203), true);
        let diff = round_trip(&canvas, false, &mut enc, &mut screen);
        assert!(diff.windows(8).any(|w| w == b"\x1b[?2026h"));
        assert!(diff.len() < first.len());
        screen.apply(LEAVE_TERM);
        cells_match(&screen, &canvas).unwrap();
    }

    #[test]
    fn resize_repaints_the_new_grid() {
        let mut enc = Encoder::new();
        let mut screen = Screen::new(4, 2);
        let first = Canvas::new(4, 2);
        round_trip(&first, true, &mut enc, &mut screen);
        let mut next = Canvas::new(3, 3);
        next.set(2, 2, 'Q', 0x10_2030, Some(0x40_5060), false);
        screen.resize(3, 3);
        round_trip(&next, true, &mut enc, &mut screen);
        assert!(enc.encode(&next, false).is_empty());
    }

    fn count(hay: &[u8], needle: &[u8]) -> usize {
        hay.windows(needle.len()).filter(|w| *w == needle).count()
    }

    #[test]
    fn explicit_width_wraps_exactly_the_non_ascii_cells() {
        let mut canvas = Canvas::new(8, 2);
        canvas.set(0, 0, 'A', 0xaa_0000, Some(0x001122), true);
        canvas.set(1, 0, '\u{2588}', 0x00_ff00, Some(0x112233), false); // a block, width 1
        canvas.set(2, 0, '\u{b7}', 0x00_ff00, Some(0x112233), false); // Latin-1: still wrapped
        canvas.set(3, 0, '\u{4f60}', 0x00_ff00, Some(0x112233), false); // wide
        canvas.set(4, 0, CONT, 0x00_ff00, Some(0x112233), false);
        canvas.set(5, 0, 'z', 0xff_ffff, Some(0), false);
        let mut enc = Encoder::new();
        enc.set_explicit_width(true);
        let mut screen = Screen::new(8, 2);
        let bytes = enc.encode(&canvas, true);
        screen.apply(&bytes);
        cells_match(&screen, &canvas).unwrap();
        assert_eq!(count(&bytes, b"\x1b]66;"), 3, "three non-ASCII cells");
        assert_eq!(count(&bytes, "\x1b]66;w=1;\u{2588}\x1b\\".as_bytes()), 1);
        assert_eq!(count(&bytes, "\x1b]66;w=1;\u{b7}\x1b\\".as_bytes()), 1);
        assert_eq!(count(&bytes, "\x1b]66;w=2;\u{4f60}\x1b\\".as_bytes()), 1);
        // ASCII is bare: the three wrapped cells are the only OSC 66 in the frame, and `A` and `z`
        // are written as themselves.
        assert!(bytes.contains(&b'A') && bytes.contains(&b'z'));
        // The screen saw the width each wrapped cell was given, and none on the bare ones.
        let at = |x: usize| screen.explicit[x];
        assert_eq!((at(0), at(1), at(2), at(3), at(5)), (0, 1, 1, 2, 0));
        // An unchanged frame still writes nothing, and a cursor that tracks the right width
        // means the next change costs no cursor move to get past the wide cell.
        assert!(enc.encode(&canvas, false).is_empty());
        canvas.set(5, 0, 'y', 0xff_ffff, Some(0), false);
        let diff = enc.encode(&canvas, false);
        screen.apply(&diff);
        cells_match(&screen, &canvas).unwrap();
        assert_eq!(count(&diff, b"\x1b]66;"), 0);
    }

    /// The original starts every run of one style with a cursor move. This encoder leaves that
    /// move out only after printable ASCII, which every terminal advances by exactly one cell.
    /// After anything else the terminal may have put the cursor somewhere the dashboard did not
    /// expect: a lone regional indicator is one cell to OpenTUI and two to Ghostty and WezTerm,
    /// and a block or an accented letter is two in a terminal set to draw ambiguous widths wide.
    /// So the move is written, as the original writes it.
    #[test]
    fn after_anything_but_ascii_the_next_style_gets_a_cursor_move() {
        let frame = |lead: char| {
            let mut canvas = Canvas::new(6, 1);
            canvas.set(0, 0, lead, 0xff_ffff, Some(0), false);
            canvas.set(1, 0, 'x', 0x00_ff00, Some(0), false);
            canvas
        };
        let diff_after = |lead: char| {
            let mut enc = Encoder::new();
            enc.encode(&Canvas::new(6, 1), true);
            String::from_utf8(enc.encode(&frame(lead), false)).unwrap()
        };
        for lead in ['\u{1f1ec}', '\u{2588}', '\u{e9}', '\u{2500}'] {
            let bytes = diff_after(lead);
            assert!(bytes.contains(&format!("{lead}\x1b[1;2H\x1b[38;2;0;255;0mx")), "{bytes:?}");
        }
        assert!(!diff_after('a').contains("\x1b[1;2H"), "{:?}", diff_after('a'));
        // Within one style nothing is added: the original writes such a run without moves too.
        let mut canvas = Canvas::new(6, 1);
        canvas.set(0, 0, '\u{2588}', 0xff_ffff, Some(0), false);
        canvas.set(1, 0, '\u{2588}', 0xff_ffff, Some(0), false);
        let mut enc = Encoder::new();
        enc.encode(&Canvas::new(6, 1), true);
        let same = String::from_utf8(enc.encode(&canvas, false)).unwrap();
        assert!(same.contains("\u{2588}\u{2588}"), "{same:?}");
    }

    #[test]
    fn turning_explicit_width_on_repaints_in_the_new_way_and_off_is_what_it_always_was() {
        let mut canvas = Canvas::new(6, 1);
        canvas.set(0, 0, '\u{2588}', 0xff_ffff, Some(0), false);
        canvas.set(1, 0, 'a', 0xff_ffff, Some(0), false);
        let mut enc = Encoder::new();
        let bare = enc.encode(&canvas, true);
        assert_eq!(count(&bare, b"\x1b]66;"), 0, "bare until told otherwise");
        assert!(enc.encode(&canvas, false).is_empty());
        enc.set_explicit_width(true);
        // The caller repaints in full when it changes the mode; the diff alone would not.
        assert!(enc.encode(&canvas, false).is_empty());
        let wrapped = enc.encode(&canvas, true);
        assert_eq!(count(&wrapped, b"\x1b]66;"), 1);
        let mut screen = Screen::new(6, 1);
        screen.apply(&wrapped);
        cells_match(&screen, &canvas).unwrap();
        // Back off: the same bytes the encoder wrote at first.
        enc.set_explicit_width(false);
        assert_eq!(enc.encode(&canvas, true), bare);
    }

    fn load(rel: &str) -> State {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        let snap: Snapshot = serde_json::from_str(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
        )
        .unwrap();
        State::new(snap)
    }

    const KEYS: &[&str] = &[
        "up", "down", "left", "right", "home", "end", "enter", "escape", "pagedown", "pageup",
        "space", "j", "k", "J", "K", "g", "G", "?", "h", "o", "a", "n", "t", "p", "r", "m", "x",
        "f",
    ];
    const SIZES: [(usize, usize); 4] = [(233, 55), (80, 24), (40, 12), (120, 40)];

    fn fuzz(rel: &str, seed: u64) {
        let mut state = load(rel);
        let mut rng = seed;
        let mut next = || {
            rng ^= rng << 7;
            rng ^= rng >> 9;
            rng ^= rng << 8;
            rng
        };
        let mut size = SIZES[0];
        let mut enc = Encoder::new();
        let mut screen = Screen::new(size.0, size.1);
        let mut full = true;
        for step in 0..48 {
            if step > 0 {
                let key = KEYS[(next() as usize) % KEYS.len()];
                state.action(key);
                if step % 5 == 0 {
                    state.advance_frame(state.now + 90., false);
                }
                if step % 11 == 0 {
                    let picked = SIZES[(next() as usize) % SIZES.len()];
                    if picked != size {
                        size = picked;
                        screen.resize(size.0, size.1);
                        full = true;
                    }
                }
            }
            // Step far enough in that `m` has left calm mode, then `x` turns CRT on
            // so the post-process in `draw_grid` is part of the round trip.
            if step == 8 {
                state.action("m");
            }
            if step == 9 {
                state.action("x");
            }
            let canvas = compose(&mut state, size.0, size.1);
            let bytes = enc.encode(&canvas, full);
            if bytes.is_empty() {
                cells_match(&screen, &canvas)
                    .unwrap_or_else(|e| panic!("{rel} step {step}: empty diff but {e}"));
            } else {
                screen.apply(&bytes);
                cells_match(&screen, &canvas).unwrap_or_else(|e| panic!("{rel} step {step}: {e}"));
            }
            let quiet = enc.encode(&canvas, false);
            assert!(quiet.is_empty(), "{rel} step {step} repeated a frame");
            full = false;
        }
    }

    #[test]
    fn fuzz_demo_and_edge_frames() {
        fuzz("../../checks/fixtures/demo.json", 0x51_7a_c0_11);
        fuzz("../../checks/generated/edge.json", 0xa1_7e_d9_03);
    }
}

#[cfg(test)]
mod row_reuse_tests {
    use super::*;
    use t3_agents_core::terminal::CONT;

    /// Cells written raw, so the canvas can hold anything a frame can: wide halves, orphaned
    /// continuations, combining marks, emoji sequences split across cells, and style changes in
    /// the middle of a cluster.
    const GLYPHS: &[char] = &[
        'a', 'b', ' ', '│', '▔', '●', '⠋', '漢', '字', CONT, '\u{301}', '\u{200d}', '👩', '🏽', 'é', '\u{fe0f}', '❤',
        '\u{7}',
    ];
    const COLORS: &[u32] = &[0x10_2030, 0xff_ffff, cluster::DEFAULT_BG, 0x00_0001];

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as usize
        }
    }

    fn scramble(c: &mut Canvas, rng: &mut Rng, cells: usize) {
        for _ in 0..cells {
            let i = rng.next() % c.cells.len();
            let cell = &mut c.cells[i];
            match rng.next() % 4 {
                0 => cell.fg = COLORS[rng.next() % COLORS.len()],
                1 => cell.bg = COLORS[rng.next() % COLORS.len()],
                2 => cell.bold = !cell.bold,
                _ => cell.ch = GLYPHS[rng.next() % GLYPHS.len()],
            }
        }
    }

    /// The reference: every frame laid out whole and diffed whole, as before rows were reused.
    fn reference(enc: &mut Encoder, canvas: &Canvas, full: bool) -> Vec<u8> {
        enc.encode_laid(cluster::lay_out(canvas, enc.method), full)
    }

    #[test]
    fn reused_rows_write_the_same_bytes_as_a_whole_layout() {
        let configs: [(WidthMethod, bool, bool); 6] = [
            (WidthMethod::Unicode, false, false),
            (WidthMethod::Unicode, false, true),
            (WidthMethod::Wcwidth, false, false),
            (WidthMethod::UnicodeWide, false, true),
            (WidthMethod::Unicode, true, false),
            (WidthMethod::Wcwidth, true, true),
        ];
        for (n, (method, explicit, after)) in configs.into_iter().enumerate() {
            let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ n as u64);
            let make = || {
                let mut e = Encoder::new();
                e.set_width_method(method);
                e.set_explicit_width(explicit);
                e.set_cursor_after_cluster(after);
                e
            };
            let (mut fast, mut slow) = (make(), make());
            let mut size = (23, 7);
            let mut canvas = Canvas::new(size.0, size.1);
            scramble(&mut canvas, &mut rng, 200);
            let mut full = true;
            for step in 0..400 {
                if step % 97 == 50 {
                    size = if size == (23, 7) { (17, 5) } else { (23, 7) };
                    canvas = Canvas::new(size.0, size.1);
                    scramble(&mut canvas, &mut rng, 120);
                }
                if step % 61 == 30 {
                    full = true;
                }
                // Mostly a few cells, sometimes none (an identical frame), sometimes many.
                let touched = match rng.next() % 6 {
                    0 => 0,
                    1 => 40,
                    _ => 1 + rng.next() % 4,
                };
                scramble(&mut canvas, &mut rng, touched);
                let a = fast.encode(&canvas, full);
                let b = reference(&mut slow, &canvas, full);
                assert_eq!(a, b, "config {n} step {step}");
                full = false;
            }
        }
    }

    /// Every setter and answer the dashboard can give the encoder
    /// between frames, in any order and without a repaint, with rows copied over each other and
    /// shifted (so equal rows meet different neighbours), frames that go through `encode_laid`
    /// in between (which drops the kept canvas), and canvases down to one cell.
    #[test]
    fn reused_rows_survive_any_order_of_changes() {
        const METHODS: [WidthMethod; 3] = [WidthMethod::Unicode, WidthMethod::Wcwidth, WidthMethod::UnicodeWide];
        for seed in 0..6u64 {
            let mut rng = Rng(0xd1b5_4a32_d192_ed03 ^ (seed << 32) ^ seed);
            let (mut fast, mut slow) = (Encoder::new(), Encoder::new());
            let mut canvas = Canvas::new(1 + rng.next() % 30, 1 + rng.next() % 9);
            let cells = canvas.cells.len();
            scramble(&mut canvas, &mut rng, cells);
            let mut full = true;
            for step in 0..6000 {
                let mut laid_frame = false;
                match rng.next() % 48 {
                    0 => {
                        canvas = Canvas::new(1 + rng.next() % 30, 1 + rng.next() % 9);
                        let cells = canvas.cells.len();
                        scramble(&mut canvas, &mut rng, cells / 2 + 1);
                    }
                    1 | 2 => {
                        let method = METHODS[rng.next() % 3];
                        fast.set_width_method(method);
                        slow.set_width_method(method);
                    }
                    3 => {
                        let on = rng.next() % 2 == 0;
                        fast.set_explicit_width(on);
                        slow.set_explicit_width(on);
                    }
                    4 => {
                        let on = rng.next() % 2 == 0;
                        fast.set_cursor_after_cluster(on);
                        slow.set_cursor_after_cluster(on);
                    }
                    5 => full = true,
                    6 => {
                        let value = (rng.next() % 5) as u8;
                        assert_eq!(fast.terminal_mode_2027(value), slow.terminal_mode_2027(value));
                    }
                    7 => assert_eq!(fast.terminal_truecolor(), slow.terminal_truecolor()),
                    8 => {
                        let (index, rgb) = ((rng.next() % 17) as u8, (rng.next() & 0xff_ffff) as u32);
                        assert_eq!(fast.terminal_palette(index, rgb), slow.terminal_palette(index, rgb));
                    }
                    9 => {
                        let text = ["kitty(0.42.1)", "tmux 3.5a", "ghostty 1.2.0", "nothing 1"][rng.next() % 4];
                        assert_eq!(fast.terminal_xtversion(text), slow.terminal_xtversion(text));
                    }
                    10 | 11 => laid_frame = true,
                    12..=14 if canvas.h > 1 => {
                        // One row copied over another: the same cells under a different row.
                        let (from, to) = (rng.next() % canvas.h, rng.next() % canvas.h);
                        let w = canvas.w;
                        canvas.cells.copy_within(from * w..(from + 1) * w, to * w);
                    }
                    15..=17 if canvas.w > 1 => {
                        // A row shifted by one cell, splitting whatever clusters it held.
                        let (y, w) = (rng.next() % canvas.h, canvas.w);
                        canvas.cells[y * w..(y + 1) * w].rotate_right(1);
                    }
                    _ => {}
                }
                let touched = match rng.next() % 6 {
                    0 => 0,
                    1 => 25,
                    _ => 1 + rng.next() % 3,
                };
                scramble(&mut canvas, &mut rng, touched);
                let a = if laid_frame {
                    fast.encode_laid(cluster::lay_out(&canvas, fast.method), full)
                } else {
                    fast.encode(&canvas, full)
                };
                let b = reference(&mut slow, &canvas, full);
                assert_eq!(a, b, "seed {seed} step {step}");
                full = false;
            }
        }
    }

    /// The kept text table is filled past `MAX_KEPT_TEXTS` with
    /// distinct multi-character clusters, so the frame after that is laid out afresh, and the
    /// bytes stay those of a whole layout before, at and after the cap.
    #[test]
    fn the_kept_text_cap_is_reached_and_changes_no_byte() {
        let marks: Vec<char> = ('\u{300}'..='\u{36f}').collect();
        let bases: Vec<char> = ('a'..='z').chain('A'..='Z').collect();
        let mut rng = Rng(0x5851_f42d_4c95_7f2d);
        let (mut fast, mut slow) = (Encoder::new(), Encoder::new());
        let mut canvas = Canvas::new(40, 6);
        let (mut most, mut resets, mut last) = (0, 0, 0);
        let mut full = true;
        for step in 0..1200 {
            for _ in 0..12 {
                // A base letter and a combining mark in neighbouring cells: one two-character
                // cluster, one more entry in the table unless it was seen before.
                let i = rng.next() % (canvas.cells.len() - 1);
                canvas.cells[i].ch = bases[rng.next() % bases.len()];
                canvas.cells[i + 1].ch = marks[rng.next() % marks.len()];
                canvas.cells[i + 1].fg = canvas.cells[i].fg;
                canvas.cells[i + 1].bg = canvas.cells[i].bg;
                canvas.cells[i + 1].bold = canvas.cells[i].bold;
            }
            assert_eq!(fast.encode(&canvas, full), reference(&mut slow, &canvas, full), "step {step}");
            full = false;
            let kept = fast.prev.as_ref().unwrap().text_count();
            most = most.max(kept);
            resets += usize::from(kept < last);
            last = kept;
        }
        assert!(most > MAX_KEPT_TEXTS, "the table reached {most}");
        assert!(resets >= 1, "the table was never dropped");
    }

    #[test]
    fn a_method_change_without_full_lays_out_every_row() {
        let mut rng = Rng(7);
        let mut canvas = Canvas::new(12, 4);
        scramble(&mut canvas, &mut rng, 60);
        let (mut fast, mut slow) = (Encoder::new(), Encoder::new());
        assert_eq!(fast.encode(&canvas, true), reference(&mut slow, &canvas, true));
        for method in [WidthMethod::Wcwidth, WidthMethod::UnicodeWide, WidthMethod::Unicode] {
            fast.set_width_method(method);
            slow.set_width_method(method);
            scramble(&mut canvas, &mut rng, 2);
            assert_eq!(fast.encode(&canvas, false), reference(&mut slow, &canvas, false));
        }
    }
}
