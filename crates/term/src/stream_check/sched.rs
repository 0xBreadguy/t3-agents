//! More schedules for `--frame-stream`. `--schedule extended` is a fixed script that adds what the
//! first script leaves out, and `--schedule random --seed N --frames N` is a seeded mix of all of it.
//!
//! Added to the first script's keys: the CRT effect in motion, theme keys, pause and resume,
//! mouse presses (a row, the needs-you tile, anywhere) and the wheel (list, details, anywhere),
//! resizes (below 48 by 14, to 300 by 80, several in consecutive passes), the terminal's answers
//! after the first frame (explicit width, mode 2027, XTVERSION, truecolor, a palette entry, the
//! background color), the focused T3 window's title arriving, changing and going away
//! (`--titles`, random schedule only), 33 ms steps while keys arrive, Storage on a database and Insights on the
//! demo (its report is made up in memory; on a database it would be a real read, so it is left
//! out there). `enter` and `o` are never pressed: they hand a link to the desktop.
//!
//! `--wide-overlay`: the data sets put no wide character on the screen, so with this flag each
//! canvas is also encoded, by five more encoders, with wide characters, emoji and combining
//! marks written over some of its rows (placed from the frame number alone, kept for six frames,
//! then moved), and with one cell's weight flipped in some other rows (no data set changes a row
//! in weight alone either). Those digests are printed as `overlay`.
//!
//! A pass follows `present`'s loop: answers and a resize first, then the host's poll, a paint
//! (or Insights' dirty flag), then each input with its own paint. Everything is decided from the
//! seed and from the state the program itself computed, so two builds that draw the same frames
//! take the same path. The output is counts and digests; `--pass-digests PATH` also writes one
//! line per pass (pass number, what the schedule did, bytes, and 64-bit digests of that pass's
//! bytes and canvases) for finding the first pass in which two builds differ.
use super::*;

#[derive(Clone, Debug)]
enum Pre {
    Resize(usize, usize),
    Explicit,
    Mode2027(u8),
    Xtversion(&'static str),
    Truecolor,
    Palette(u8, u32),
    Background(u32),
    /// The focused T3 window's title becomes that of the n-th thread (modulo how many there
    /// are), built in memory from the snapshot; `None` is no T3 window.
    Title(Option<usize>),
    /// A window title that names no thread.
    TitleUnknown,
}

#[derive(Clone, Debug)]
enum Input {
    Key(&'static str),
    /// A press at per-mille coordinates of the terminal.
    PressAt(u32, u32),
    /// A press on the n-th row hit rectangle of the last frame (modulo how many there are).
    PressRow(usize),
    PressNeedsYou,
    WheelAt(u32, u32, bool),
    WheelDetail(bool),
    WheelList(bool),
}

#[derive(Clone, Debug, Default)]
struct Pass {
    step_ms: f64,
    pre: Vec<Pre>,
    inputs: Vec<Input>,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 >> 11
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
    fn pick<T: Copy>(&mut self, of: &[T]) -> T {
        of[self.below(of.len())]
    }
}

const SIZES: &[(usize, usize)] = &[
    (190, 60), (190, 60), (120, 40), (80, 24), (47, 13), (48, 14), (49, 15), (300, 80), (140, 45), (139, 45),
    (100, 30), (60, 16), (59, 15), (20, 6), (250, 70), (190, 59), (191, 60),
];
const VERSIONS: &[&str] =
    &["kitty(0.42.1)", "ghostty 1.2.0", "tmux 3.5a", "foot(1.22.0)", "WezTerm 20240203-110809", "XTerm(390)", "nothing 1.0"];
/// Dashboard keys, weighted by repetition. No `enter`, no `o`, no `q`.
const KEYS: &[&str] = &[
    "down", "down", "down", "down", "up", "up", "up", "left", "right", "right", "space", "space", "a", "t", "J", "K",
    "j", "k", "?", "?", "escape", "escape", "m", "G", "g", "n", "n", "w", "x", "x", "f", "home", "end", "c", "C", "p",
    "pageup", "pagedown", "h", "tab", "shift-tab", "1", "z",
];
const INSIGHTS_KEYS: &[&str] = &[
    "tab", "shift-tab", "left", "right", "up", "down", "down", "pageup", "pagedown", "home", "end", "?", "escape",
    "r", "k", "j", "x",
];

fn random(seed: u64, frames: usize, db: bool, refresh_key: bool, titles: bool) -> Vec<Pass> {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut v = Vec::with_capacity(frames);
    let mut step = 100.;
    let mut left = 0;
    let mut resize_again = false;
    for i in 0..frames {
        if left == 0 {
            step = match rng.below(20) {
                0..=9 => 1000. / 30.,
                10..=16 => 100.,
                17..=18 => 500.,
                _ => 16.,
            };
            left = 20 + rng.below(180);
        }
        left -= 1;
        let mut p = Pass { step_ms: step, ..Pass::default() };
        if i == 0 {
            p.pre.push(Pre::Resize(190, 60));
        } else if resize_again || rng.chance(45) {
            p.pre.push({
                let (w, h) = rng.pick(SIZES);
                Pre::Resize(w, h)
            });
            resize_again = rng.chance(3);
        }
        if i > 3 && rng.chance(250) {
            p.pre.push(match rng.below(8) {
                0 => Pre::Explicit,
                1 | 2 => Pre::Mode2027(rng.below(5) as u8),
                3 => Pre::Xtversion(rng.pick(VERSIONS)),
                4 => Pre::Truecolor,
                5 | 6 => Pre::Palette(rng.below(17) as u8, (rng.next() & 0xff_ffff) as u32),
                _ => Pre::Background(if rng.chance(2) { 0xf8_f4_e8 } else { 0x10_12_18 }),
            });
        }
        // Only with `--titles`, so the schedules without it stay what they were.
        if titles && rng.chance(40) {
            p.pre.push(match rng.below(6) {
                0 => Pre::Title(None),
                1 => Pre::TitleUnknown,
                _ => Pre::Title(Some(rng.below(4096))),
            });
        }
        let mut more = rng.chance(6);
        while more {
            p.inputs.push(match rng.below(40) {
                0..=1 => Input::PressRow(rng.below(64)),
                2 => Input::PressNeedsYou,
                3 => Input::PressAt(rng.below(1000) as u32, rng.below(1000) as u32),
                4 => Input::WheelAt(rng.below(1000) as u32, rng.below(1000) as u32, rng.chance(2)),
                5..=6 => Input::WheelDetail(!rng.chance(3)),
                7..=8 => Input::WheelList(!rng.chance(3)),
                9 if !db => Input::Key("i"),
                9 => Input::Key("s"),
                10 if refresh_key => Input::Key("r"),
                11..=13 if !db => Input::Key(rng.pick(INSIGHTS_KEYS)),
                _ => Input::Key(rng.pick(KEYS)),
            });
            more = rng.chance(4);
        }
        v.push(p);
    }
    v
}

/// The fixed script: one segment per thing the first script leaves out.
fn extended(db: bool) -> Vec<Pass> {
    let mut v: Vec<Pass> = Vec::new();
    let mut seg = |step_ms: f64, frames: usize, pre: &[(usize, Pre)], inputs: &[(usize, Input)]| {
        for i in 0..frames {
            let mut p = Pass { step_ms, ..Pass::default() };
            p.pre.extend(pre.iter().filter(|(at, _)| *at == i).map(|(_, e)| e.clone()));
            p.inputs.extend(inputs.iter().filter(|(at, _)| *at == i).map(|(_, e)| e.clone()));
            v.push(p);
        }
    };
    use Input::*;
    // The effect on and off in motion, themes, pause, the keys, the mouse, a light and a dark
    // background.
    seg(
        100.,
        260,
        &[(0, Pre::Resize(190, 60)), (200, Pre::Background(0xf8_f4_e8)), (215, Pre::Background(0x10_12_18))],
        &[
            (10, Key("x")), (15, Key("down")), (16, Key("down")), (20, Key("c")), (30, Key("C")), (35, Key("c")),
            (40, Key("p")), (45, Key("down")), (50, Key("up")), (60, Key("p")), (70, Key("n")), (75, Key("n")),
            (80, Key("g")), (90, Key("G")), (100, Key("f")), (110, Key("down")), (120, Key("f")), (130, PressRow(0)),
            (134, PressRow(3)), (138, PressRow(17)), (142, PressNeedsYou), (146, PressAt(500, 500)),
            (150, WheelList(true)), (151, WheelList(true)), (152, WheelList(true)), (156, WheelList(false)),
            (160, WheelDetail(true)), (161, WheelDetail(true)), (166, WheelDetail(false)), (170, WheelAt(990, 10, true)),
            (180, Key("x")), (190, Key("?")), (192, WheelAt(500, 500, true)), (194, PressAt(100, 100)),
            (225, Key("C")), (240, Key("m")), (245, Key("x")), (250, Key("m")), (255, Key("x")),
        ],
    );
    // Resizes: below 48 by 14 and back, 300 by 80, several in consecutive passes.
    seg(
        100.,
        120,
        &[
            (5, Pre::Resize(47, 13)), (15, Pre::Resize(190, 60)), (25, Pre::Resize(300, 80)), (40, Pre::Resize(120, 40)),
            (41, Pre::Resize(121, 40)), (42, Pre::Resize(120, 41)), (43, Pre::Resize(80, 24)), (44, Pre::Resize(20, 6)),
            (45, Pre::Resize(48, 14)), (46, Pre::Resize(49, 15)), (60, Pre::Resize(190, 60)), (61, Pre::Resize(190, 60)),
            (80, Pre::Resize(140, 45)), (81, Pre::Resize(139, 45)), (100, Pre::Resize(190, 60)),
        ],
        &[(8, Key("down")), (27, Key("down")), (30, Key("?")), (35, Key("escape")), (42, Key("up")), (44, Key("space")), (90, Key("a"))],
    );
    // The terminal's answers after the first frame, at 33 ms steps with keys arriving.
    seg(
        1000. / 30.,
        330,
        &[
            (20, Pre::Palette(1, 0xcc_24_1d)), (21, Pre::Palette(1, 0xcc_24_1d)), (22, Pre::Palette(4, 0x45_85_88)),
            (50, Pre::Mode2027(2)), (80, Pre::Mode2027(0)), (110, Pre::Explicit), (140, Pre::Xtversion("kitty(0.42.1)")),
            (170, Pre::Xtversion("tmux 3.5a")), (200, Pre::Truecolor), (230, Pre::Palette(2, 0x98_97_1a)),
            (260, Pre::Mode2027(1)), (290, Pre::Xtversion("ghostty 1.2.0")),
        ],
        &[
            (5, Key("down")), (25, Key("down")), (26, Key("down")), (55, Key("up")), (85, Key("x")), (115, Key("a")),
            (116, Key("a")), (145, Key("t")), (175, Key("J")), (205, Key("K")), (235, Key("x")), (265, Key("n")),
            (295, Key("space")), (300, Key("space")), (310, Key("a")),
        ],
    );
    if db {
        seg(
            100.,
            70,
            &[(40, Pre::Resize(120, 40)), (55, Pre::Resize(190, 60))],
            &[(5, Key("s")), (10, Key("down")), (15, PressAt(300, 400)), (20, WheelAt(300, 400, true)), (25, Key("c")), (45, Key("up")), (60, Key("escape"))],
        );
    } else {
        // Insights on the demo: open, both reports, periods, scrolling, help, a resize, back.
        seg(
            100.,
            160,
            &[(90, Pre::Resize(120, 40)), (105, Pre::Resize(47, 13)), (110, Pre::Resize(190, 60)), (120, Pre::Background(0xf8_f4_e8))],
            &[
                (5, Key("i")), (15, Key("down")), (16, Key("down")), (20, Key("pagedown")), (25, Key("home")),
                (30, Key("tab")), (40, Key("down")), (45, Key("end")), (50, Key("right")), (55, Key("left")),
                (60, Key("left")), (65, Key("shift-tab")), (70, Key("?")), (75, Key("down")), (80, Key("escape")),
                (85, WheelAt(500, 500, true)), (95, Key("up")), (115, Key("r")), (130, Key("i")), (135, Key("down")),
                (140, Key("i")), (145, Key("escape")), (150, Key("down")),
            ],
        );
    }
    v
}

struct Fnv(u64);
impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

struct Tally {
    stream: Sha256,
    canvases: Sha256,
    extras: Vec<(&'static str, Encoder, Sha256, u64)>,
    frames: u64,
    written: u64,
    bytes: u64,
    full_frames: u64,
    pass_bytes: Fnv,
    pass_canvas: Fnv,
    pass_len: u64,
    /// Encoders fed each canvas with wide characters written over it (`--wide-overlay`).
    overlay: Vec<(&'static str, Encoder, Sha256, u64)>,
}

/// Wide characters, emoji and combining marks over some rows of a canvas, from the frame number
/// alone: the same in any build. They stay for six frames while the dashboard changes under
/// them, then move, so a row's clusters are met unchanged, changed and split.
fn widen(c: &mut terminal::Canvas, frame: u64) {
    const WIDE: &[char] = &['漢', '字', '👩', '🏽', '한', '❤', '🎉'];
    let epoch = frame / 6;
    for y in 0..c.h {
        let mut rng = Rng((epoch.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ ((y as u64 + 1) << 24)) | 1);
        rng.next();
        rng.next();
        if rng.below(3) != 0 {
            // No wide character in this row for now. Some of these rows get one cell's weight
            // flipped instead, on a four-frame rhythm of its own, so that a row also changes in
            // weight alone (no data set does that either).
            let mut weight = Rng(((frame / 4).wrapping_mul(0xd1b5_4a32_d192_ed03) ^ ((y as u64 + 1) << 20)) | 1);
            weight.next();
            weight.next();
            if weight.below(5) == 0 {
                let x = weight.below(c.w);
                c.cells[y * c.w + x].bold ^= true;
            }
            continue;
        }
        let mut x = rng.below(c.w);
        for _ in 0..1 + rng.below(6) {
            if x + 1 >= c.w {
                break;
            }
            let i = y * c.w + x;
            let style = c.cells[i];
            c.cells[i].ch = rng.pick(WIDE);
            c.cells[i + 1] = terminal::Cell { ch: terminal::CONT, ..style };
            x += 2;
            match rng.below(8) {
                0 if x < c.w => {
                    c.cells[y * c.w + x].ch = '\u{301}';
                    x += 1;
                }
                1 => x += 1,
                _ => {}
            }
        }
        if rng.below(3) == 0 {
            // A wide character in the last column, with no room for its second half.
            c.cells[y * c.w + c.w - 1].ch = '字';
        }
    }
}

impl Tally {
    fn take(&mut self, canvas: &terminal::Canvas, bytes: &[u8], was_full: bool) -> io::Result<()> {
        self.frames += 1;
        self.bytes += bytes.len() as u64;
        self.written += u64::from(!bytes.is_empty());
        self.full_frames += u64::from(was_full);
        self.stream.update(bytes);
        self.pass_bytes.update(bytes);
        self.pass_len += bytes.len() as u64;
        hash_canvas(&mut self.canvases, canvas);
        self.pass_canvas.update(&(canvas.w as u64).to_le_bytes());
        for cell in &canvas.cells {
            self.pass_canvas.update(&(cell.ch as u32).to_le_bytes());
            self.pass_canvas.update(&cell.fg.to_le_bytes());
            self.pass_canvas.update(&cell.bg.to_le_bytes());
            self.pass_canvas.update(&[u8::from(cell.bold)]);
        }
        for (_, e, h, n) in self.extras.iter_mut() {
            let b = e.encode(canvas, was_full);
            *n += b.len() as u64;
            h.update(&(b.len() as u64).to_le_bytes());
            h.update(&b);
        }
        if !self.overlay.is_empty() {
            let mut wide = canvas.clone();
            widen(&mut wide, self.frames);
            for (_, e, h, n) in self.overlay.iter_mut() {
                let b = e.encode(&wide, was_full);
                *n += b.len() as u64;
                h.update(&(b.len() as u64).to_le_bytes());
                h.update(&b);
            }
        }
        Ok(())
    }
}

pub(super) fn run(args: &[String], name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let now0: f64 = value("--now-ms").ok_or("--frame-stream needs --now-ms")?.parse()?;
    set_now(now0);
    let database = match value("--db") {
        Some(path) => Database::Path { path: path.into(), origin: Origin::Db },
        None if has("--demo") => Database::Demo,
        None => return Err("--frame-stream needs --db PATH or --demo".into()),
    };
    let db = matches!(database, Database::Path { .. });
    let schedule = match name {
        "extended" => extended(db),
        "random" => random(
            value("--seed").ok_or("--schedule random needs --seed")?.parse()?,
            value("--frames").map_or(Ok(20_000), |n| n.parse())?,
            db,
            has("--refresh-key"),
            has("--titles"),
        ),
        other => return Err(format!("no schedule named {other}").into()),
    };
    let stop_after: usize = value("--stop-after").map_or(Ok(usize::MAX), |n| n.parse())?;
    let mut digests = match value("--pass-digests") {
        Some(path) => Some(io::BufWriter::new(std::fs::File::create(path)?)),
        None => None,
    };
    let themes = if theme::disabled() { None } else { Some(theme::Themes::from_environment(None)?.0) };
    let (mut state, mut host, source, insights_source) = product_launch(database, 2., has("--calm"), themes);
    let insights_source = insights_source.filter(|_| !db);
    let wake = Arc::new(Wake::new()?);
    let mut storage = StorageScreen::new(source, Arc::clone(&wake));
    let mut insights = InsightsScreen::default();
    if let Some(themes) = &host.themes {
        state.palette = themes.palette();
        state.themes = true;
    }
    // The reader the launch read through, cache and all, as `start_live` hands it to the worker.
    let handed = host.take_live_reader();
    let mut reader = match &host.db {
        Some(path) => {
            host.live = Some(live::Handle::detached());
            Some(match handed {
                Some(reader) => reader,
                None => live::Reader::open(path)?,
            })
        }
        None => None,
    };
    let tmux = TermEnv::from_pairs([("TERM", "tmux-256color"), ("TMUX", "/tmp/tmux-0/default,1,0")]);
    let mut primary = Encoder::for_terminal(tmux);
    // `--color-env`: the color depth comes from this process's environment, as in the dashboard
    // (TERM=xterm-256color without COLORTERM starts with the 256-color palette).
    let color_env = has("--color-env");
    if color_env {
        primary.process_color_environment();
    }
    let mut tally = Tally {
        stream: Sha256::new(),
        canvases: Sha256::new(),
        extras: Vec::new(),
        frames: 0,
        written: 0,
        bytes: 0,
        full_frames: 0,
        pass_bytes: Fnv::new(),
        pass_canvas: Fnv::new(),
        pass_len: 0,
        overlay: Vec::new(),
    };
    if has("--wide-overlay") {
        for (name, method, explicit, after) in [
            ("wide_tmux", WidthMethod::Unicode, false, true),
            ("wide_unicode", WidthMethod::Unicode, false, false),
            ("wide_wcwidth", WidthMethod::Wcwidth, false, false),
            ("wide_unicode_wide", WidthMethod::UnicodeWide, false, false),
            ("wide_explicit", WidthMethod::Unicode, true, false),
        ] {
            let mut e = Encoder::new();
            e.set_width_method(method);
            e.set_explicit_width(explicit);
            e.set_cursor_after_cluster(after);
            tally.overlay.push((name, e, Sha256::new(), 0));
        }
    }
    if value("--encoders").as_deref() != Some("primary") {
        for (name, method, explicit) in [
            ("unicode", WidthMethod::Unicode, false),
            ("wcwidth", WidthMethod::Wcwidth, false),
            ("unicode_wide", WidthMethod::UnicodeWide, false),
            ("unicode_explicit", WidthMethod::Unicode, true),
            ("wcwidth_explicit", WidthMethod::Wcwidth, true),
        ] {
            let mut e = Encoder::new();
            e.set_width_method(method);
            e.set_explicit_width(explicit);
            if color_env {
                e.process_color_environment();
            }
            tally.extras.push((name, e, Sha256::new(), 0));
        }
    }
    let mut wakes = Sha256::new();
    let mut clock = TermClock { now: 0., wake: None };
    let mut tracing: Option<trace_live::Screen> = None;
    let mut full = true;
    let mut time = 0.;
    let mut last_read = f64::NEG_INFINITY;
    let mut size = (190, 60);
    let mut passes = 0u64;
    let mut insights_frames = 0u64;
    for (index, p) in schedule.iter().enumerate().take(stop_after) {
        passes += 1;
        if index > 0 {
            time += p.step_ms / 1000.;
        }
        set_now(now0 + (time * 1000.0f64).round());
        if let (Some(r), Some(handle)) = (reader.as_mut(), host.live.as_ref()) {
            if time - last_read >= 2. {
                last_read = time;
                handle.offer(r.poll(clock::wall_now()));
            }
        }
        tally.pass_bytes = Fnv::new();
        tally.pass_canvas = Fnv::new();
        tally.pass_len = 0;
        clock.now = time;
        clock.wake = None;
        // The terminal's answers and a resize, as `present` takes them before the pass's paint.
        let mut background_changed = false;
        for pre in &p.pre {
            match *pre {
                Pre::Resize(w, h) => {
                    if (w, h) != size {
                        size = (w, h);
                        full = true;
                    }
                }
                Pre::Explicit => {
                    primary.set_explicit_width(true);
                    full = true;
                }
                Pre::Mode2027(v) => full |= primary.terminal_mode_2027(v),
                Pre::Xtversion(text) => full |= primary.terminal_xtversion(text),
                Pre::Truecolor => full |= primary.terminal_truecolor(),
                Pre::Palette(i, rgb) => full |= primary.terminal_palette(i, rgb),
                Pre::Background(rgb) => background_changed |= host::terminal_background(&mut state, &mut host, rgb),
                Pre::Title(pick) => {
                    host.t3_title = pick.and_then(|n| {
                        let threads: Vec<_> = state.snap.agents.iter().filter(|a| a.id == a.thread_id).collect();
                        (!threads.is_empty()).then(|| {
                            let a = threads[n % threads.len()];
                            format!("{} / {}", a.project, a.title)
                        })
                    });
                }
                Pre::TitleUnknown => host.t3_title = Some("no window by this name".into()),
            }
        }
        let host_changed = host::poll_host(&mut state, clock.now, &mut clock, &mut host);
        storage.poll(&state);
        if let Some(t) = tracing.as_mut() {
            t.poll();
            trace_read_now(t, &state, &host);
            let reduced = state.surface.borrow().system_reduced;
            let running = t.trace().is_some_and(|task| task.ended.is_none());
            clock.wake_after(Duration::from_millis(if running && !state.calm && !reduced { 100 } else { 1000 }));
        }
        let mut insights_dirty = false;
        if insights.is_open() {
            insights_dirty = full || background_changed || host_changed;
        } else {
            paint_with(&mut state, &mut storage, size.0, size.1, &mut primary, &mut full, &mut tracing, &mut |c, b, f| tally.take(c, b, f))?;
        }
        for input in &p.inputs {
            if let Input::Key(name) = input {
                assert!(!matches!(*name, "enter" | "o" | "return" | "q"), "the schedule never opens a link");
            }
            if insights.is_open() {
                let source = insights_source.expect("open only with a source");
                let effect = match *input {
                    Input::Key(name) => insights.key(name, source),
                    Input::WheelAt(_, _, down) | Input::WheelDetail(down) | Input::WheelList(down) => {
                        insights.key(if down { "down" } else { "up" }, source)
                    }
                    _ => InsightsEffect::Stay,
                };
                match effect {
                    InsightsEffect::Back => {
                        insights.close();
                        paint_with(&mut state, &mut storage, size.0, size.1, &mut primary, &mut full, &mut tracing, &mut |c, b, f| tally.take(c, b, f))?;
                    }
                    InsightsEffect::Paint => {
                        insights.present_key_with(&state, source, host.db.as_deref(), size, &mut primary, &mut full, &wake, &mut |c, b, f| tally.take(c, b, f))?;
                        insights_frames += 1;
                        settle(&mut insights, &state, source, size, &mut primary, &mut full, &mut tally)?;
                        insights_dirty = false;
                    }
                    InsightsEffect::Quit | InsightsEffect::Stay => {}
                }
                continue;
            }
            match *input {
                Input::Key("i") => {
                    let Some(source) = insights_source else { continue };
                    if !state.storage_key_in_help || tracing.is_some() || storage.is_open() {
                        continue;
                    }
                    if state.help {
                        on_key(&mut state, &mut host, &mut clock, "escape", false);
                    }
                    insights.open();
                    insights.present_key_with(&state, source, host.db.as_deref(), size, &mut primary, &mut full, &wake, &mut |c, b, f| tally.take(c, b, f))?;
                    insights_frames += 1;
                    settle(&mut insights, &state, source, size, &mut primary, &mut full, &mut tally)?;
                    insights_dirty = false;
                    continue;
                }
                Input::Key("tab" | "shift-tab") => continue,
                Input::Key(name) => dispatch_key(&mut state, &mut host, &mut clock, &mut storage, &mut tracing, name, size),
                Input::PressAt(..) | Input::PressRow(_) | Input::PressNeedsYou => {
                    let (x, y) = point(input, &state, size);
                    match tracing.as_mut() {
                        Some(t) if !state.help => t.press(y),
                        _ if storage.is_open() => storage.press(y, size),
                        _ => on_press(&mut state, &mut host, &mut clock, x, y, false),
                    }
                }
                Input::WheelAt(_, _, down) | Input::WheelDetail(down) | Input::WheelList(down) => {
                    let (x, y) = point(input, &state, size);
                    match tracing.as_mut() {
                        Some(t) if !state.help => {
                            t.key(if down { "down" } else { "up" });
                        }
                        _ if storage.is_open() => {
                            storage.key(if down { "down" } else { "up" }, &mut state, &host, size);
                        }
                        _ => on_wheel(&mut state, &host, x, y, down),
                    }
                }
            }
            paint_with(&mut state, &mut storage, size.0, size.1, &mut primary, &mut full, &mut tracing, &mut |c, b, f| tally.take(c, b, f))?;
        }
        if insights_dirty && insights.is_open() {
            let source = insights_source.expect("open only with a source");
            insights.paint_with(&state, source, size, &mut primary, &mut full, &mut |c, b, f| tally.take(c, b, f))?;
            insights_frames += 1;
        }
        let wake_ms = clock.wake.map_or(-1., |d| d.as_secs_f64() * 1000.);
        wakes.update(&wake_ms.to_le_bytes());
        if let Some(out) = digests.as_mut() {
            writeln!(out, "{index} step={:.1} pre={:?} in={:?} bytes={} stream={:016x} canvas={:016x} wake={wake_ms}", p.step_ms, p.pre, p.inputs, tally.pass_len, tally.pass_bytes.0, tally.pass_canvas.0)?;
        }
    }
    if let Some(out) = digests.as_mut() {
        out.flush()?;
    }
    let json = |list: Vec<(&'static str, Encoder, Sha256, u64)>| -> String {
        let items: Vec<String> = list
            .into_iter()
            .map(|(name, _, h, n)| format!("{{\"name\":\"{name}\",\"bytes\":{n},\"sha256\":\"{}\"}}", h.hex()))
            .collect();
        items.join(",")
    };
    let extra_json = json(std::mem::take(&mut tally.extras));
    let overlay_json = json(std::mem::take(&mut tally.overlay));
    println!(
        "{{\"schedule\":\"{name}\",\"passes\":{passes},\"frames\":{},\"written\":{},\"full_frames\":{},\"insights_frames\":{insights_frames},\"bytes\":{},\"stream_sha256\":\"{}\",\"canvas_sha256\":\"{}\",\"wake_sha256\":\"{}\",\"encoders\":[{}],\"overlay\":[{}]}}",
        tally.frames,
        tally.written,
        tally.full_frames,
        tally.bytes,
        tally.stream.hex(),
        tally.canvases.hex(),
        wakes.hex(),
        extra_json,
        overlay_json
    );
    Ok(())
}

/// Where an input lands, from the frame the program last drew.
fn point(input: &Input, state: &State, size: (usize, usize)) -> (i32, i32) {
    let frame = state.surface.borrow();
    let middle = |r: &terminal::Rect| (r.x + r.w / 2, r.y + r.h / 2);
    match *input {
        Input::PressAt(x, y) | Input::WheelAt(x, y, _) => {
            ((x as usize * size.0 / 1000) as i32, (y as usize * size.1 / 1000) as i32)
        }
        Input::PressRow(n) => match frame.hits.len() {
            0 => (0, 0),
            len => {
                let r = &frame.hits[n % len].rect;
                (r.x + r.w / 2, r.y)
            }
        },
        Input::PressNeedsYou => middle(&frame.needs_you),
        Input::WheelDetail(_) => middle(&frame.detail),
        Input::WheelList(_) => middle(&frame.list),
        Input::Key(_) => (0, 0),
    }
}

/// Wait for Insights' read (made up in memory on the demo) and paint what it brought, as the
/// loop does when the reader's wake arrives. The scripted clock does not move meanwhile.
fn settle(
    insights: &mut InsightsScreen,
    state: &State,
    source: InsightsSource,
    size: (usize, usize),
    primary: &mut Encoder,
    full: &mut bool,
    tally: &mut Tally,
) -> io::Result<()> {
    let started = Instant::now();
    while insights.loading() {
        if insights.poll(source) {
            insights.paint_with(state, source, size, primary, full, &mut |c, b, f| tally.take(c, b, f))?;
            continue;
        }
        if started.elapsed() > Duration::from_secs(120) {
            return Err(io::Error::other("Insights did not finish reading"));
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    Ok(())
}
