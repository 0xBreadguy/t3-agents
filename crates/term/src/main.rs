//! `t3-agents-term`: the cell grid on any terminal.
//!
//! It speaks the product command line of the original (`--demo`, `--db`, `--once`,
//! `--status`, `--interval`, `--calm`), and the experiment flags when one is present.
//! Whatever is not answered without a screen is presented here. The loop blocks in
//! `platform::wait` until the terminal has input, a signal arrives, or the host's next wake;
//! a calm fixed snapshot does not wake at all. Every operating-system call is in `platform`.
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use t3_agents_core::{
    cli::{self, interaction_state},
    clock, demo, host,
    host::{Clock, Host},
    integration, live,
    model::{Snapshot, State},
    product::{self, Command, Database},
    recap, source, terminal, theme, trace, trace_live, trace_view,
};
use t3_agents_term::platform::{self, Wake, Waker};
use t3_agents_term::{
    BACKGROUND_QUERY, Encoder, Event, GRAPHEME_ON, Parser, Probe, Report, compose, probe_wanted,
    startup_probe_for,
    storage::StorageSource,
};

mod insights_screen;
use insights_screen::{Effect as InsightsEffect, InsightsScreen, InsightsSource};
mod storage_screen;
use storage_screen::StorageScreen;
#[cfg(feature = "frame-stream")]
mod stream_check;

struct TermClock {
    now: f64,
    wake: Option<Duration>,
}

impl Clock for TermClock {
    fn instant(&mut self) {}
    fn wake_after(&mut self, after: Duration) {
        self.wake = Some(match self.wake {
            Some(prev) => prev.min(after),
            None => after,
        });
    }
    fn clock(&self) -> f64 {
        self.now
    }
}

/// How long the loop may sleep: until the nearer of the host's next wake and the key parser's
/// escape deadline, or for as long as it takes when neither is set.
fn timeout(wake: Option<Instant>, esc: Option<Instant>) -> Option<Duration> {
    let deadline = match (wake, esc) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    deadline.map(|when| when.saturating_duration_since(Instant::now()))
}

fn setup(s: &mut State, h: &mut Host, wake: Waker) {
    if let Some(themes) = &h.themes {
        s.palette = themes.palette();
        s.themes = true;
    } else if let Some(path) = &h.theme {
        s.palette = integration::palette(path).unwrap_or_else(|_| terminal::Palette::fallback());
    }
    if let Some(path) = &h.usage {
        s.usage = integration::usage(path, s.now);
    }
    if h.db.is_none() && !h.fixed {
        let epoch = h.epoch;
        match demo::snapshot(epoch, epoch) {
            Ok(snapshot) => {
                s.seeded = false;
                s.first_seen.clear();
                s.shimmer.clear();
                s.replace(snapshot);
                s.boot_at = epoch;
            }
            Err(e) => {
                s.error = Some(e.to_string());
                s.error_kind = Some(e.kind.into());
                s.error_detail = Some(e.detail);
            }
        }
    }
    if h.db.is_some() {
        if let Some(dir) = integration::hypr_dir() {
            s.t3_link = true;
            let (sender, receiver) = mpsc::channel();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                while let Ok(title) = receiver.recv() {
                    if tx.send(title).is_err() {
                        break;
                    }
                    wake.wake();
                }
            });
            if let Ok(watch) = integration::FocusWatcher::start(dir, sender) {
                h.focus = Some(watch);
                h.titles = Some(rx);
            }
        }
    }
    host::start_live(h, s.interval, move || wake.wake());
}

/// Trace the selected row's own demo history or its live thread. Live delegates with no
/// thread of their own use their parent's thread. Remote rows have nothing here to read.
fn trace_target(s: &State) -> Result<trace::Target, String> {
    let agent = s.snap.agents.iter().find(|a| Some(&a.id) == s.selected.as_ref()).ok_or("Select an agent to trace it")?;
    if agent.remote.is_some() || agent.reference_only == Some(true) {
        return Err(format!("{} runs on another machine: no trace here", agent.title));
    }
    let id = if s.demo { &agent.id } else { &agent.thread_id };
    Ok(trace::Target::Thread(id.clone()))
}

/// Start reading the trace's task, unless a read is running or the demo has its task. On T3's
/// database, a watch rereads it as often as the dashboard reads; the demo's task is made up
/// once, and its clocks run on the screen's own time.
fn trace_read(t: &mut trace_live::Screen, s: &State, host: &Host, wake: &Wake) {
    if t.watch.is_some() || t.data.is_some() {
        return;
    }
    match &host.db {
        Some(path) => {
            let waker = wake.waker();
            let every = Duration::from_secs_f64(s.interval.max(0.5));
            t.watch = Some(trace_live::watch(path, t.target().clone(), every, move || waker.wake()));
        }
        None => {
            t.data = Some(demo::trace(s.snap.at, host.epoch, t.target()));
        }
    }
}

/// Enter on a trace: open its thread in T3 as Enter does on the dashboard, by selecting its row.
fn trace_open_thread(s: &mut State, host: &Host, thread: &str, at: f64) {
    match s.snap.agents.iter().find(|a| a.id == thread || a.thread_id == thread).map(|a| a.id.clone()) {
        Some(id) => {
            s.selected = Some(id);
            host::open_selected(s, host);
        }
        None => s.note("This thread is not in the dashboard's list", true, at),
    }
}

/// A key while Trace is open. Returns whether to quit; `None` when the dashboard handles it.
fn trace_key(s: &mut State, host: &Host, tracing: &mut Option<trace_live::Screen>, wake: &Wake, name: &str) -> Option<bool> {
    let t = tracing.as_mut()?;
    if s.help {
        return None;
    }
    match name {
        "q" => return Some(true),
        // Help, calm, pause, refresh and the theme keys are the dashboard's still.
        "?" | "h" | "m" | "p" | "r" | "c" | "C" => return None,
        _ => {}
    }
    let at = host::input_now(s, host);
    match t.key(name) {
        trace_live::Outcome::Close => *tracing = None,
        trace_live::Outcome::OpenThread(thread) => trace_open_thread(s, host, &thread, at),
        trace_live::Outcome::Say(text) => s.note(text, false, at),
        trace_live::Outcome::Read => trace_read(t, s, host, wake),
        trace_live::Outcome::Stay => {}
    }
    Some(false)
}

fn on_key(s: &mut State, host: &mut Host, clock: &mut TermClock, name: &str, replay: bool) -> bool {
    if name == "q" || (name == "escape" && !s.help) {
        return true;
    }
    if matches!(name, "c" | "C") {
        // The product's theme keys. Without its themes `c` is not a key, as in the original.
        if host.themes.is_some() {
            clock.instant();
            let now = host::input_now(s, host);
            host::cycle_theme(s, host, name == "C", now);
            clock.wake_after(Duration::ZERO);
        }
        return false;
    }
    clock.instant();
    // Only opening a thread needs the pre-action row's earlier/folded state.
    let opening = matches!(name, "enter" | "o");
    let before = opening
        && s.rows()
            .iter()
            .find(|r| Some(&r.id) == s.selected.as_ref())
            .is_some_and(|r| r.is_earlier());
    let now = host::input_now(s, host);
    let secs = clock.clock();
    let db = host.db.is_some();
    let epoch = host.epoch;
    host::after_key(s, clock, name, now, |state| {
        if replay {
            state.snap.at
        } else if db {
            clock::wall_now()
        } else {
            epoch + secs * 1000.
        }
    });
    if opening && !before {
        host::open_selected(s, host);
    }
    if name == "r" && !s.paused {
        host::data_refresh(s, host);
    }
    interaction_state(s);
    false
}

fn on_press(s: &mut State, host: &mut Host, clock: &mut TermClock, x: i32, y: i32, replay: bool) {
    let frame = s.surface.borrow().clone();
    if s.help {
        on_key(s, host, clock, "escape", replay);
        return;
    }
    if frame.needs_you.contains(x, y) {
        on_key(s, host, clock, "n", replay);
        return;
    }
    let hit = frame
        .hits
        .iter()
        .rev()
        .find(|hit| hit.rect.contains(x, y))
        .map(|hit| (hit.id.clone(), hit.earlier));
    if let Some((id, earlier)) = hit {
        clock.instant();
        let at = host::input_now(s, host);
        s.pick_at(&id, at);
        if earlier {
            s.action_at("right", at);
        }
        interaction_state(s);
    }
}

fn on_wheel(s: &mut State, host: &Host, x: i32, y: i32, down: bool) {
    let frame = s.surface.borrow().clone();
    if s.help {
        s.help_scroll = if down {
            (s.help_scroll + 3).min(frame.help_max)
        } else {
            s.help_scroll.saturating_sub(3)
        };
    } else if frame.detail.contains(x, y) {
        s.detail_scroll = if down {
            (s.detail_scroll + 3).min(frame.detail_max)
        } else {
            s.detail_scroll.saturating_sub(3)
        };
    } else {
        s.action_at(if down { "down" } else { "up" }, host::input_now(s, host));
    }
}

static APPLIED: AtomicUsize = AtomicUsize::new(0);
static SKIPPED: AtomicUsize = AtomicUsize::new(0);
static PREVIOUS: AtomicUsize = AtomicUsize::new(0);

fn replay_step(s: &mut State, replay: &[Snapshot], time: f64, clock: &mut TermClock) -> bool {
    if ((time * 10.) as usize) < replay.len() - 1 || s.snap.at != replay.last().unwrap().at {
        clock.wake_after(Duration::from_millis(100));
    }
    if s.paused {
        return false;
    }
    let index = ((time * 10.) as usize).min(replay.len() - 1);
    let snap = &replay[index];
    if snap.at == s.snap.at {
        let reduced = s.surface.borrow().system_reduced;
        s.advance_frame(s.snap.at, reduced);
        return false;
    }
    clock.instant();
    s.replace(snap.clone());
    let reduced = s.surface.borrow().system_reduced;
    s.advance_frame(s.snap.at, reduced);
    if let Some(path) = std::env::var_os("T3_EXPERIMENT_METRICS") {
        let previous = PREVIOUS.swap(index, Ordering::Relaxed);
        let applied = APPLIED.fetch_add(1, Ordering::Relaxed) + 1;
        let gap = index.saturating_sub(previous + 1);
        let skipped = SKIPPED.fetch_add(gap, Ordering::Relaxed) + gap;
        let body = format!(
            "{{\"index\":{index},\"at\":{},\"agents\":{},\"updates_applied\":{applied},\"updates_skipped\":{skipped},\"errors\":0}}",
            s.snap.at,
            s.snap.agents.len()
        );
        let _ = std::fs::write(path, body);
    }
    true
}

/// Trace, drawn over the whole terminal with its own contextual help.
fn draw_trace(s: &State, t: &mut trace_live::Screen, cols: usize, rows: usize) -> terminal::Canvas {
    if cols < 60 || rows < 16 {
        return trace_view::small(&s.palette, cols, rows);
    }
    let reduced = s.surface.borrow().system_reduced;
    let view = trace_view::View {
        selected: t.selected.clone(),
        scroll: t.scroll,
        calm: s.calm || reduced,
        mono: false,
        spin: (s.now / 100.) as usize,
        demo: s.demo,
        now: s.now,
    };
    if let Some(canvas) = t.frame(&s.palette, cols, rows, &view, &t3_agents_core::platform::local_time) {
        return canvas;
    }
    match &t.data {
        Some(Ok(None)) => trace_view::notice(&s.palette, cols, rows, "Nothing to trace", "T3 has no run on record for this agent yet.", &["w goes back to the dashboard".into()]),
        Some(Err(e)) => trace_view::notice(&s.palette, cols, rows, "Can't read T3", &e.to_string(), &["w goes back to the dashboard".into()]),
        _ => trace_view::notice(&s.palette, cols, rows, "Trace", "Reading…", &[]),
    }
}

fn paint(
    s: &mut State,
    storage: &mut StorageScreen,
    cols: usize,
    rows: usize,
    enc: &mut Encoder,
    full: &mut bool,
    tracing: &mut Option<trace_live::Screen>,
) -> io::Result<()> {
    paint_with(s, storage, cols, rows, enc, full, tracing, &mut |_, bytes, _| {
        if !bytes.is_empty() {
            platform::write_stdout(bytes)?;
        }
        Ok(())
    })
}

/// What the loop does to draw a frame, with the finished canvas, its bytes and whether the frame
/// was a full repaint handed to `sink`. [`paint`] writes the bytes; the stream check hashes them.
type Sink<'a> = dyn FnMut(&terminal::Canvas, &[u8], bool) -> io::Result<()> + 'a;

#[allow(clippy::too_many_arguments)]
fn paint_with(
    s: &mut State,
    storage: &mut StorageScreen,
    cols: usize,
    rows: usize,
    enc: &mut Encoder,
    full: &mut bool,
    tracing: &mut Option<trace_live::Screen>,
    sink: &mut Sink<'_>,
) -> io::Result<()> {
    let canvas = match tracing.as_mut() {
        Some(t) => {
            let mut canvas = draw_trace(s, t, cols.max(1), rows.max(1));
            if s.help { trace_view::help(&mut canvas, &s.palette); }
            canvas
        },
        _ => storage.render(s, (cols, rows)).unwrap_or_else(|| compose(s, cols, rows)),
    };
    let was_full = *full;
    let bytes = enc.encode(&canvas, *full);
    *full = false;
    sink(&canvas, &bytes, was_full)
}

type Launch = (State, Host, StorageSource, Option<InsightsSource>);

fn present(wake: Arc<Wake>, launch: Launch) -> Result<(), Box<dyn std::error::Error>> {
    let _restore = platform::enter_raw()?;
    let (mut state, mut host, source, insights_source) = launch;
    let mut insights = InsightsScreen::default();
    let mut storage = StorageScreen::new(source, Arc::clone(&wake));
    // Ask the terminal what it can do before anything slow, and never wait for the answer: it
    // arrives while the first frame is being composed. See `caps.rs`. A theme that follows the
    // terminal's light or dark mode also asks for its background and for color scheme reports.
    let probing = probe_wanted(std::env::var("TERM").ok().as_deref());
    let theme_mode = probing && host.themes.as_ref().is_some_and(theme::Themes::follows_terminal);
    if theme_mode {
        platform::restore_theme_mode_on_exit();
    }
    if probing {
        platform::write_stdout(&startup_probe_for(theme_mode))?;
    }
    if std::env::var_os("T3_TERM_PANIC").is_some() {
        panic!("t3-agents-term: test panic");
    }
    let replay = cli::option("--replay")
        .map(|path| host::load_replay(std::path::Path::new(&path)))
        .transpose()?;
    setup(&mut state, &mut host, wake.waker());
    let mut clock = TermClock {
        now: 0.,
        wake: None,
    };
    let mut parser = Parser::new();
    // The width method and cursor rules the original takes from this terminal's environment.
    let mut encoder = Encoder::for_terminal(t3_agents_core::cluster::TermEnv::from_process());
    encoder.process_color_environment();
    let mut palette_queried = false;
    // When the background was last asked for after a color scheme report (OpenTUI asks once per
    // 250 ms at most: `RendererThemeMode.QUERY_TIMEOUT_MS` in `src/renderer.ts`).
    let mut background_asked: Option<Instant> = None;
    let mut full = true;
    // The wait for answers counts from here, where the first can be read, not from the write: setup
    // is not the terminal's delay.
    let mut probe = probing.then(|| Probe::started(Instant::now()));
    if let Some(probe) = &probe {
        parser.expect_replies(probe.until());
    }
    let mut size = platform::window_size()?;
    let started = Instant::now();
    // T3_FIXED_NOW pins wall_now(). The demo's display clock is epoch plus the
    // presenter's elapsed time, so hold that elapsed time at zero or the demo
    // keeps moving while the original's Date.now() stays put. A real clock is
    // untouched: this is only set when the pin parses.
    let pinned = std::env::var("T3_FIXED_NOW")
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .is_some_and(|ms| ms.is_finite());
    // The first pass runs immediately. After that, only a scheduled wake or input.
    let mut next_wake = Some(Instant::now());
    // Nothing is drawn until the terminal has answered the start-up questions, or the wait for
    // them is over. The original's first frame also comes after the answers. Drawing sooner means
    // drawing twice, and where a terminal measures text differently from the layout the first
    // drawing can leave cells behind that the second never touches.
    let mut painted = false;
    // Trace, while it is open over the dashboard (the product's `w`).
    let mut tracing: Option<trace_live::Screen> = None;
    loop {
        let ready = platform::wait(&wake, timeout(next_wake, parser.deadline()))?;
        if ready.wake {
            wake.drain();
        }
        let mut events = Vec::new();
        let mut eof = false;
        if ready.input {
            let (bytes, ended) = platform::read_input()?;
            eof = ended;
            events.extend(parser.push(&bytes, Instant::now()));
        } else if parser.deadline().is_some_and(|d| Instant::now() >= d) {
            events.extend(parser.timed_out(Instant::now()));
        }
        // The terminal's background, and word that its color scheme changed: OpenTUI answers a
        // `CSI ? 997 ; n n` by asking for the colors again and takes the mode from the background.
        let mut background_changed = false;
        for event in &events {
            match event {
                Event::Report(Report::Background(rgb)) => {
                    background_changed |= host::terminal_background(&mut state, &mut host, *rgb);
                }
                Event::Report(Report::ColorScheme { .. }) if theme_mode => {
                    if background_asked.is_none_or(|at| at.elapsed() >= Duration::from_millis(250)) {
                        platform::write_stdout(BACKGROUND_QUERY)?;
                        background_asked = Some(Instant::now());
                    }
                }
                _ => {}
            }
        }
        // The terminal's answers to the probe change how the next frame is written.
        for event in &events {
            let (Event::Report(report), Some(probe)) = (event, probe.as_mut()) else {
                continue;
            };
            let change = probe.answer(report.clone(), Instant::now());
            if change.explicit_width {
                encoder.set_explicit_width(true);
                full = true;
            }
            if change.mode_2027.is_some_and(|value| encoder.terminal_mode_2027(value)) {
                full = true;
            }
            if change.xtversion.as_deref().is_some_and(|text| encoder.terminal_xtversion(text)) {
                full = true;
            }
            if change.truecolor && encoder.terminal_truecolor() {
                full = true;
            }
            if let Some((index, rgb)) = change.palette {
                if !palette_queried {
                    let queries: String = (0..16).map(|i| format!("\x1b]4;{i};?\x07")).collect();
                    platform::write_stdout(queries.as_bytes())?;
                    palette_queried = true;
                } else if encoder.terminal_palette(index, rgb) {
                    full = true;
                }
            }
            if change.grapheme_on {
                // Marked first: whatever happens to the write, leaving puts the mode back.
                platform::restore_grapheme_mode_on_exit();
                platform::write_stdout(GRAPHEME_ON)?;
            }
        }
        events.retain(|event| !matches!(event, Event::Report(_)));
        clock.now = if pinned {
            0.
        } else {
            started.elapsed().as_secs_f64()
        };
        clock.wake = None;
        let signals = platform::take_signals();
        if signals.resized {
            // A terminal also signals when only the window's pixel size changed, or nothing did.
            // The original ignores a resize to the size it already has, and so does this: a
            // repaint there is wasted work, and beside text the terminal measures differently it
            // is visible.
            let now = platform::window_size()?;
            if now != size {
                size = now;
                full = true;
            }
        }
        let host_changed = if let Some(replay) = &replay {
            replay_step(&mut state, replay, clock.now, &mut clock)
        } else {
            host::poll_host(&mut state, clock.now, &mut clock, &mut host)
        };
        storage.poll(&state);
        if let Some(t) = tracing.as_mut() {
            t.poll();
            trace_read(t, &state, &host, &wake);
            // Open steps' clocks tick every second; the spinner turns while the task runs.
            let reduced = state.surface.borrow().system_reduced;
            let running = t.trace().is_some_and(|task| task.ended.is_none());
            clock.wake_after(Duration::from_millis(if running && !state.calm && !reduced { 100 } else { 1000 }));
        }
        let hold = !painted && probe.as_ref().is_some_and(|probe| probe.waiting(Instant::now()));
        let mut insights_dirty = false;
        if insights.is_open() {
            let source = insights_source.unwrap();
            let completed = ready.wake && insights.poll(source);
            insights_dirty = completed || full || background_changed || host_changed;
        } else if !hold {
            paint(&mut state, &mut storage, size.0, size.1, &mut encoder, &mut full, &mut tracing)?;
            painted = true;
        }
        let mut quit = eof || signals.quit;
        for event in events {
            if insights.is_open() {
                if quit {
                    break;
                }
                let source = insights_source.unwrap();
                let effect = match event {
                    Event::Quit => InsightsEffect::Quit,
                    Event::Key(name) => insights.key(name, source),
                    Event::Wheel { down, .. } => insights.key(if down { "down" } else { "up" }, source),
                    _ => InsightsEffect::Stay,
                };
                match effect {
                    InsightsEffect::Quit => {
                        quit = true;
                        break;
                    }
                    InsightsEffect::Back => {
                        insights.close();
                        paint(&mut state, &mut storage, size.0, size.1, &mut encoder, &mut full, &mut tracing)?;
                    }
                    InsightsEffect::Paint => {
                        insights.present_key(&state, source, host.db.as_deref(), size, &mut encoder, &mut full, &wake)?;
                        insights_dirty = false;
                    }
                    InsightsEffect::Stay => {}
                }
                continue;
            }
            let stop = match event {
                Event::Quit => true,
                Event::Key("i") if state.storage_key_in_help && tracing.is_none()
                    && !storage.is_open() && !quit && let Some(source) = insights_source => {
                    if state.help {
                        on_key(&mut state, &mut host, &mut clock, "escape", replay.is_some());
                    }
                    insights.open();
                    insights.present_key(&state, source, host.db.as_deref(), size, &mut encoder, &mut full, &wake)?;
                    insights_dirty = false;
                    painted = true;
                    continue;
                }
                Event::Key("i" | "tab" | "shift-tab") => continue,
                Event::Key("s") if state.storage_key_in_help && tracing.is_none() && !storage.is_open() => {
                    if state.help {
                        on_key(&mut state, &mut host, &mut clock, "escape", replay.is_some());
                    }
                    storage.open(&state);
                    false
                }
                Event::Key(name) if storage.is_open() => {
                    if matches!(name, "c" | "C") {
                        on_key(&mut state, &mut host, &mut clock, name, replay.is_some())
                    } else {
                        storage.key(name, &mut state, &host, size)
                    }
                }
                Event::Key(name) => match trace_key(&mut state, &host, &mut tracing, &wake, name) {
                    Some(quit) => quit,
                    // `w` opens Trace on the selected agent: the product's key, like `c`. Without
                    // the product's options it is not a key, as in the original.
                    None if name == "w" => {
                        if host.themes.is_some() && !state.help && replay.is_none() {
                            match trace_target(&state) {
                                Ok(target) => {
                                    let mut t = trace_live::Screen::new(target);
                                    trace_read(&mut t, &state, &host, &wake);
                                    tracing = Some(t);
                                }
                                Err(message) => {
                                    let at = host::input_now(&state, &host);
                                    state.note(message, false, at);
                                }
                            }
                        }
                        false
                    }
                    None => on_key(&mut state, &mut host, &mut clock, name, replay.is_some()),
                },
                Event::Press { x, y } => {
                    match tracing.as_mut() {
                        Some(t) if !state.help => t.press(y),
                        _ if storage.is_open() => storage.press(y, size),
                        _ => on_press(&mut state, &mut host, &mut clock, x, y, replay.is_some()),
                    }
                    false
                }
                Event::Wheel { x, y, down } => {
                    match tracing.as_mut() {
                        Some(t) if !state.help => {
                            t.key(if down { "down" } else { "up" });
                        }
                        _ if storage.is_open() => { storage.key(if down { "down" } else { "up" }, &mut state, &host, size); }
                        _ => on_wheel(&mut state, &host, x, y, down),
                    }
                    false
                }
                // Answers were taken above.
                Event::Report(_) => false,
            };
            paint(&mut state, &mut storage, size.0, size.1, &mut encoder, &mut full, &mut tracing)?;
            painted = true;
            if stop {
                quit = true;
                break;
            }
        }
        if quit {
            break;
        }
        // Fold a host wake into the key's paint when both arrived together.
        if insights_dirty && insights.is_open() {
            insights.paint(&state, insights_source.unwrap(), size, &mut encoder, &mut full)?;
        }
        next_wake = clock.wake.map(|after| Instant::now() + after);
        if !painted {
            // A terminal that never answers must not leave the screen undrawn: wake when the wait ends.
            let until = probe.as_ref().map(Probe::until);
            next_wake = match (next_wake, until) {
                (Some(wake), Some(until)) => Some(wake.min(until)),
                (wake, until) => wake.or(until),
            };
        }
    }
    Ok(())
}

fn bench(spec: &str) -> Result<(), Box<dyn std::error::Error>> {
    let n: usize = spec.parse()?;
    if n == 0 {
        return Err("iterations must be positive".into());
    }
    let grid = cli::option("--grid").unwrap_or_else(|| "233x55".into());
    let (w, h) = grid.split_once('x').ok_or("grid WxH")?;
    let (w, h): (usize, usize) = (w.parse()?, h.parse()?);
    let mut state = cli::init();
    let before = compose(&mut state, w, h);
    state.action("down");
    let after = compose(&mut state, w, h);
    let mut screen = t3_agents_term::Screen::new(w, h);
    let mut enc = Encoder::new();
    let full_bytes = enc.encode(&before, true);
    screen.apply(&full_bytes);
    t3_agents_term::cells_match(&screen, &before)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let diff_bytes = enc.encode(&after, false);
    if diff_bytes.is_empty() {
        t3_agents_term::cells_match(&screen, &after)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    } else {
        screen.apply(&diff_bytes);
        t3_agents_term::cells_match(&screen, &after)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    }
    for _ in 0..20 {
        let mut warm = Encoder::new();
        std::hint::black_box(warm.encode(std::hint::black_box(&before), true));
    }
    let mut full_samples = Vec::with_capacity(n);
    let mut full_len = 0;
    for _ in 0..n {
        let mut enc = Encoder::new();
        let started = Instant::now();
        let bytes = enc.encode(std::hint::black_box(&before), true);
        full_samples.push(started.elapsed());
        full_len = bytes.len();
    }
    let mut diff_samples = Vec::with_capacity(n);
    let mut diff_len = 0;
    for _ in 0..n {
        let mut enc = Encoder::new();
        let _ = enc.encode(&before, true);
        let started = Instant::now();
        let bytes = enc.encode(std::hint::black_box(&after), false);
        diff_samples.push(started.elapsed());
        diff_len = bytes.len();
    }
    let (full_min, full_med, full_max) = spread(&full_samples);
    let (diff_min, diff_med, diff_max) = spread(&diff_samples);
    let fixture = cli::option("--snapshot").unwrap_or_else(|| "demo".into());
    println!(
        "{{\"fixture\":{},\"grid\":\"{w}x{h}\",\"operations\":{n},\"errors\":0,\"full_bytes\":{full_len},\"full_median_us\":{full_med:.3},\"full_min_us\":{full_min:.3},\"full_max_us\":{full_max:.3},\"diff_key\":\"down\",\"diff_bytes\":{diff_len},\"diff_median_us\":{diff_med:.3},\"diff_min_us\":{diff_min:.3},\"diff_max_us\":{diff_max:.3}}}",
        json_string(&fixture)
    );
    Ok(())
}

fn spread(samples: &[Duration]) -> (f64, f64, f64) {
    let mut us: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1e6).collect();
    us.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let min = us[0];
    let max = *us.last().unwrap();
    let mid = us.len() / 2;
    let median = if us.len() % 2 == 1 {
        us[mid]
    } else {
        (us[mid - 1] + us[mid]) * 0.5
    };
    (min, median, max)
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The product's interactive start: T3's database or the demo, as `main.ts` builds its app.
fn product_launch(
    database: Database,
    interval: f64,
    calm: bool,
    themes: Option<theme::Themes>,
) -> Launch {
    let storage = StorageSource::for_database(&database);
    let now = clock::wall_now();
    let insights_source = InsightsSource::from(&database);
    let (mut state, mut host) = match database {
        Database::Demo => {
            let state = match demo::snapshot(now, now) {
                Ok(snapshot) => State::new(snapshot),
                Err(e) => failed(Snapshot::default(), &e),
            };
            (state, Host::demo(now))
        }
        Database::Path { path, .. } => {
            let mut initial_reader = None;
            let result = if std::env::var_os("T3_AGENTS_SYNC_READ").is_some() {
                source::snapshot(&path, now)
            } else {
                match live::Reader::open(&path).and_then(|mut reader| {
                    let update = reader.poll(now)?;
                    if update.served == live::Served::Full {
                        clock::release_memory();
                    }
                    initial_reader = Some(reader);
                    Ok(update.snapshot)
                }) {
                    Ok(snapshot) => Ok(snapshot),
                    // Any failure reads as the reference does, with its error.
                    Err(_) => source::snapshot(&path, now),
                }
            };
            let mut state = match result {
                Ok(snapshot) => State::new(snapshot),
                Err(e) => failed(
                    Snapshot {
                        path: path.to_string_lossy().into(),
                        ..Snapshot::default()
                    },
                    &e,
                ),
            };
            state.demo = false;
            state.usage.clear();
            let mut host = Host::live(path, now);
            if let Some(reader) = initial_reader {
                host = host.with_live_reader(reader);
            }
            (state, host)
        }
    };
    state.calm = calm;
    state.storage_key_in_help = !theme::disabled();
    state.interval = interval;
    // Use the same launch roots for the dashboard warning and the Storage view.
    if let StorageSource::Disk(roots) = &storage {
        host.watch_headroom = true;
        host.managed_worktrees = Some(roots.worktrees.clone());
        host.temporary = Some(roots.temp.clone());
    }
    if themes.is_some() {
        host.theme = None;
        host.themes = themes;
    }
    (state, host, storage, Some(insights_source))
}

/// A state that shows why T3's database could not be read.
fn failed(snapshot: Snapshot, e: &source::SourceError) -> State {
    let mut state = State::new(snapshot);
    state.error = Some(e.to_string());
    state.error_kind = Some(e.kind.into());
    state.error_detail = Some(e.detail.clone());
    state
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Experiment flags (fixtures, replays, benchmarks) keep their own command line. Anything
    // else is the product's: --once, --status and --help are answered without a screen.
    let launch = if cli::experiment_requested() {
        if cli::headless()? {
            return Ok(());
        }
        if let Some(n) = cli::option("--bench-term") {
            return bench(&n);
        }
        #[cfg(feature = "frame-stream")]
        if std::env::args().any(|a| a == "--frame-stream") {
            return stream_check::run();
        }
        if !io::stdout().is_terminal() {
            return Err("stdout is not a terminal".into());
        }
        let state = cli::init();
        let host = Host::from_args(&state);
        (state, host, StorageSource::Unavailable, None)
    } else {
        let cache_stats = request_recap_cache_stats();
        let headless = product::headless();
        if cache_stats {
            report_cache_stats();
        }
        if headless? {
            return Ok(());
        }
        let args: Vec<String> = std::env::args().skip(1).collect();
        let Ok(Command::Interactive {
            database,
            interval,
            calm,
            theme: flag,
        }) = product::parse(&args)
        else {
            unreachable!("product::headless answers every other command");
        };
        product::ensure_terminal();
        // The theme is settled before the screen is taken, so a bad --theme is said plainly.
        let themes = if theme::disabled() {
            None
        } else {
            match theme::Themes::from_environment(flag.as_deref()) {
                Ok((themes, skipped)) => {
                    for line in skipped {
                        eprintln!("t3-agents: skipped {line}");
                    }
                    Some(themes)
                }
                Err(e) => {
                    eprintln!("t3-agents: {e}");
                    std::process::exit(2);
                }
            }
        };
        product_launch(database, interval, calm, themes)
    };
    let wake = Arc::new(Wake::new()?);
    platform::install_signals(wake.waker())?;
    present(wake, launch)
}

/// Arm the optional report only for the Recap command.
fn request_recap_cache_stats() -> bool {
    if !std::env::var_os("T3_AGENTS_CACHE_STATS").is_some_and(|value| value == "1") {
        return false;
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !matches!(product::parse(&args), Ok(Command::Recap { .. })) {
        return false;
    }
    recap::request_cache_stats();
    #[cfg(unix)]
    platform::report_on_exit(report_cache_stats);
    true
}

/// Write the diagnostic once, including when the product exits after a read error.
extern "C" fn report_cache_stats() {
    if let Some(line) = recap::take_cache_stats() {
        let _ = writeln!(io::stderr().lock(), "{line}");
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("t3-agents-term: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The demo's state, as the product launches it, but with a fixed clock and no theme files.
    /// `Host::demo` never opens a window or a link: `enter` and `o` only set a notice.
    fn demo_state() -> (State, Host, TermClock) {
        let now = 1_795_163_100_000.0;
        let mut state = State::new(demo::snapshot(now, now).expect("the demo snapshot builds"));
        state.demo = true;
        state.calm = false;
        (state, Host::demo(now), TermClock { now: 0., wake: None })
    }

    fn select(s: &mut State, id: &str) {
        assert!(s.rows().iter().any(|r| r.id == id), "{id} is not a row of the demo");
        let at = s.now;
        s.pick_at(id, at);
    }

    fn first_earlier_row(s: &State) -> String {
        s.rows()
            .iter()
            .find(|r| r.is_earlier())
            .expect("the demo folds one family's older children")
            .id
            .clone()
    }

    #[test]
    fn enter_and_o_unfold_a_folded_earlier_row_without_opening_anything() {
        for key in ["enter", "o"] {
            let (mut s, mut host, mut clock) = demo_state();
            let earlier = first_earlier_row(&s);
            let before = s.rows().len();
            select(&mut s, &earlier);
            assert_eq!(s.notice, None, "no notice before the key");
            assert!(!on_key(&mut s, &mut host, &mut clock, key, false), "{key} does not quit");
            assert!(
                !s.notice.as_deref().is_some_and(|n| n.starts_with("Demo: would open")),
                "{key} on an earlier row opened a thread: {:?}",
                s.notice
            );
            assert!(s.rows().len() > before, "{key} unfolded the earlier row");
            assert!(s.rows().iter().all(|r| r.id != earlier), "the earlier row is gone once unfolded");
            let picked = s.selected.clone().expect("a row stays selected");
            assert!(
                s.rows().iter().any(|r| r.id == picked && !r.is_earlier()),
                "{key} selects the first revealed row, got {picked}"
            );
        }
    }

    #[test]
    fn enter_and_o_open_an_ordinary_agent_row() {
        for key in ["enter", "o"] {
            let (mut s, mut host, mut clock) = demo_state();
            let row = s.rows().into_iter().find(|r| !r.is_earlier()).expect("the demo has ordinary rows");
            select(&mut s, &row.id);
            assert_eq!(s.notice, None, "no notice before the key");
            assert!(!on_key(&mut s, &mut host, &mut clock, key, false), "{key} does not quit");
            let notice = s.notice.as_deref().unwrap_or_default();
            assert!(notice.starts_with("Demo: would open"), "{key} on {} said {notice:?}", row.id);
        }
    }
}
