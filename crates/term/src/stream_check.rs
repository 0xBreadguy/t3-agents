//! `--frame-stream`: the dashboard's frame loop without a terminal, for proving that a change to
//! how frames are made leaves every written byte as it was.
//!
//! It launches the product as `t3-agents --db PATH` (or `--demo`) does, then drives the same
//! functions the loop in `present` drives (`host::poll_host`, the key handlers, `paint_with`
//! with the real encoder) through a fixed script of clock steps, sizes and keys. The clock is
//! pinned through `T3_FIXED_NOW`, which `clock::wall_now` reads, and advanced by the script. On a
//! database the background reader is replaced by the same `live::Reader`, polled in this thread
//! every two seconds of scripted time and posted to the host's mailbox. Trace is read in this
//! thread for the same reason. Nothing that depends on the real clock or on thread timing is
//! left, so two builds given the same script and data must print the same hashes.
//!
//! It prints one line of JSON: frame and byte counts, and SHA-256 digests over the encoder
//! output (for several terminal configurations), over every composed canvas, and over the wake
//! the host asked for after each pass. No screen text leaves the process.
//!
//! Options: `--db PATH --now-ms MS` or `--demo --now-ms MS`; `--calm` starts in calm mode;
//! `--encoders primary` encodes only for the primary terminal (for timing); `--break-cell` flips
//! one cell of one canvas before it is encoded (to show the check can fail).
use super::*;
use t3_agents_core::cluster::{TermEnv, WidthMethod};
use t3_agents_core::live;
use t3_agents_core::product::Origin;

mod sched;

struct Segment {
    name: &'static str,
    step_ms: f64,
    frames: usize,
    size: (usize, usize),
    /// Keys pressed after the pass of a frame, by frame index within the segment.
    keys: &'static [(usize, &'static str)],
}

/// The script. Keys that open things in T3 (`enter`, `o`), reread the database on this thread
/// (`r`), open Insights (`i`) or pause (`p`) are left out; `s` is pressed only where Storage has no
/// disk walk behind it.
fn script(storage_key: bool) -> Vec<Segment> {
    let mut v = vec![
        Segment {
            name: "100ms_190x60",
            step_ms: 100.,
            frames: 640,
            size: (190, 60),
            keys: &[
                (40, "down"), (41, "down"), (42, "down"), (60, "up"), (80, "left"), (95, "right"),
                (110, "space"), (130, "space"), (150, "a"), (170, "a"), (190, "a"), (210, "t"),
                (230, "t"), (250, "J"), (251, "J"), (270, "K"), (290, "?"), (300, "down"),
                (310, "escape"), (330, "m"), (360, "m"), (380, "G"), (390, "g"), (400, "n"),
                (420, "w"), (450, "down"), (470, "w"), (500, "x"), (530, "x"), (560, "f"),
                (580, "f"),
            ],
        },
        Segment {
            name: "33ms_120x40",
            step_ms: 1000. / 30.,
            frames: 300,
            size: (120, 40),
            keys: &[(20, "down"), (50, "m"), (80, "m"), (120, "?"), (150, "escape"), (200, "t")],
        },
        Segment {
            name: "500ms_80x24",
            step_ms: 500.,
            frames: 200,
            size: (80, 24),
            keys: &[(10, "down"), (30, "J"), (50, "a"), (70, "w"), (90, "w"), (120, "m"), (150, "m")],
        },
        Segment {
            name: "100ms_190x60_b",
            step_ms: 100.,
            frames: 200,
            size: (190, 60),
            keys: &[(10, "up"), (20, "home"), (40, "end"), (60, "c"), (80, "C")],
        },
    ];
    if storage_key {
        v.push(Segment {
            name: "storage_190x60",
            step_ms: 100.,
            frames: 60,
            size: (190, 60),
            keys: &[(5, "s"), (15, "down"), (30, "escape")],
        });
    }
    v
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    if let Some(name) = value("--schedule") {
        return sched::run(&args, &name);
    }
    let now0: f64 = value("--now-ms").ok_or("--frame-stream needs --now-ms")?.parse()?;
    let calm = has("--calm");
    let all_encoders = value("--encoders").as_deref() != Some("primary");
    let break_cell = has("--break-cell");
    set_now(now0);
    let database = match value("--db") {
        Some(path) => Database::Path { path: path.into(), origin: Origin::Db },
        None if has("--demo") => Database::Demo,
        None => return Err("--frame-stream needs --db PATH or --demo".into()),
    };
    let storage_key = matches!(database, Database::Path { .. });
    let themes = if theme::disabled() { None } else { Some(theme::Themes::from_environment(None)?.0) };
    let (mut state, mut host, source, _insights) = product_launch(database, 2., calm, themes);
    let wake = Arc::new(Wake::new()?);
    let mut storage = StorageScreen::new(source, Arc::clone(&wake));
    // `setup` without the threads: the palette, and a mailbox in place of the reader thread.
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
    // The terminal the timed runs use: tmux, which asks for a cursor move after every cluster.
    let tmux = TermEnv::from_pairs([("TERM", "tmux-256color"), ("TMUX", "/tmp/tmux-0/default,1,0")]);
    let mut primary = Encoder::for_terminal(tmux);
    let mut extras: Vec<(&str, Encoder)> = Vec::new();
    if all_encoders {
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
            extras.push((name, e));
        }
    }
    let mut stream = Sha256::new();
    let mut canvases = Sha256::new();
    let mut wakes = Sha256::new();
    let mut extra_hash: Vec<(Sha256, u64)> = extras.iter().map(|_| (Sha256::new(), 0)).collect();
    let mut frames = 0u64;
    let mut written = 0u64;
    let mut bytes_total = 0u64;
    let mut clock = TermClock { now: 0., wake: None };
    let mut tracing: Option<trace_live::Screen> = None;
    let mut full = true;
    let mut time = 0.;
    let mut last_read = f64::NEG_INFINITY;
    let mut segments = Vec::new();
    let mut size = (0, 0);
    let mut broke = false;
    let mut pass = 0u64;
    for seg in script(storage_key) {
        let mut samples: Vec<f64> = Vec::with_capacity(seg.frames);
        for i in 0..seg.frames {
            pass += 1;
            if pass > 1 {
                time += seg.step_ms / 1000.;
            }
            set_now(now0 + (time * 1000.0f64).round());
            // The reader thread's poll, on the scripted clock, every two seconds.
            if let (Some(r), Some(handle)) = (reader.as_mut(), host.live.as_ref()) {
                if time - last_read >= 2. {
                    last_read = time;
                    handle.offer(r.poll(clock::wall_now()));
                }
            }
            let started = Instant::now();
            let mut sink_time = Duration::ZERO;
            let mut sink = |canvas: &terminal::Canvas, bytes: &[u8], was_full: bool| -> io::Result<()> {
                let t = Instant::now();
                frames += 1;
                bytes_total += bytes.len() as u64;
                written += u64::from(!bytes.is_empty());
                stream.update(bytes);
                hash_canvas(&mut canvases, canvas);
                for ((_, e), (h, n)) in extras.iter_mut().zip(extra_hash.iter_mut()) {
                    let b = e.encode(canvas, was_full);
                    *n += b.len() as u64;
                    h.update(&(b.len() as u64).to_le_bytes());
                    h.update(&b);
                }
                sink_time += t.elapsed();
                Ok(())
            };
            clock.now = time;
            clock.wake = None;
            if size != seg.size {
                size = seg.size;
                full = true;
            }
            host::poll_host(&mut state, clock.now, &mut clock, &mut host);
            storage.poll(&state);
            if let Some(t) = tracing.as_mut() {
                t.poll();
                trace_read_now(t, &state, &host);
                let reduced = state.surface.borrow().system_reduced;
                let running = t.trace().is_some_and(|task| task.ended.is_none());
                clock.wake_after(Duration::from_millis(if running && !state.calm && !reduced { 100 } else { 1000 }));
            }
            if break_cell && !broke && pass == 700 {
                // One wrong cell in one frame: the digests must change.
                broke = true;
                let mut canvas = compose(&mut state, size.0, size.1);
                canvas.cells[0].fg ^= 1;
                let was_full = full;
                let bytes = primary.encode(&canvas, full);
                full = false;
                sink(&canvas, &bytes, was_full)?;
            } else {
                paint_with(&mut state, &mut storage, size.0, size.1, &mut primary, &mut full, &mut tracing, &mut sink)?;
            }
            for &(_, key) in seg.keys.iter().filter(|(at, _)| *at == i) {
                dispatch_key(&mut state, &mut host, &mut clock, &mut storage, &mut tracing, key, size);
                paint_with(&mut state, &mut storage, size.0, size.1, &mut primary, &mut full, &mut tracing, &mut sink)?;
            }
            let elapsed = started.elapsed().saturating_sub(sink_time);
            samples.push(elapsed.as_secs_f64() * 1e6);
            let wake_ms = clock.wake.map_or(-1., |d| d.as_secs_f64() * 1000.);
            wakes.update(&wake_ms.to_le_bytes());
        }
        samples.sort_by(f64::total_cmp);
        let q = |p: f64| samples[((samples.len() - 1) as f64 * p).round() as usize];
        segments.push(format!(
            "{{\"name\":\"{}\",\"passes\":{},\"median_us\":{:.1},\"p10_us\":{:.1},\"p90_us\":{:.1}}}",
            seg.name,
            samples.len(),
            q(0.5),
            q(0.1),
            q(0.9)
        ));
    }
    let extra_json: Vec<String> = extras
        .iter()
        .zip(extra_hash)
        .map(|((name, _), (h, n))| format!("{{\"name\":\"{name}\",\"bytes\":{n},\"sha256\":\"{}\"}}", h.hex()))
        .collect();
    println!(
        "{{\"frames\":{frames},\"written\":{written},\"bytes\":{bytes_total},\"stream_sha256\":\"{}\",\"canvas_sha256\":\"{}\",\"wake_sha256\":\"{}\",\"encoders\":[{}],\"segments\":[{}]}}",
        stream.hex(),
        canvases.hex(),
        wakes.hex(),
        extra_json.join(","),
        segments.join(",")
    );
    Ok(())
}

/// The key handling of `present`'s loop, for the keys the script presses.
fn dispatch_key(
    state: &mut State,
    host: &mut Host,
    clock: &mut TermClock,
    storage: &mut StorageScreen,
    tracing: &mut Option<trace_live::Screen>,
    name: &'static str,
    size: (usize, usize),
) {
    if name == "s" && state.storage_key_in_help && tracing.is_none() && !storage.is_open() {
        if state.help {
            on_key(state, host, clock, "escape", false);
        }
        storage.open(state);
        return;
    }
    if storage.is_open() {
        if matches!(name, "c" | "C") {
            on_key(state, host, clock, name, false);
        } else {
            storage.key(name, state, host, size);
        }
        return;
    }
    if let Some(t) = tracing.as_mut() {
        if !state.help && !matches!(name, "?" | "h" | "m" | "p" | "r" | "c" | "C" | "q") {
            let at = host::input_now(state, host);
            match t.key(name) {
                trace_live::Outcome::Close => *tracing = None,
                trace_live::Outcome::OpenThread(_) => {}
                trace_live::Outcome::Say(text) => state.note(text, false, at),
                trace_live::Outcome::Read => trace_read_now(t, state, host),
                trace_live::Outcome::Stay => {}
            }
            return;
        }
    }
    if name == "w" {
        if host.themes.is_some() && !state.help {
            match trace_target(state) {
                Ok(target) => {
                    let mut t = trace_live::Screen::new(target);
                    trace_read_now(&mut t, state, host);
                    *tracing = Some(t);
                }
                Err(message) => {
                    let at = host::input_now(state, host);
                    state.note(message, false, at);
                }
            }
        }
        return;
    }
    on_key(state, host, clock, name, false);
}

/// `trace_read`, in this thread: one read now instead of a watch thread.
fn trace_read_now(t: &mut trace_live::Screen, s: &State, host: &Host) {
    if t.watch.is_some() || t.data.is_some() {
        return;
    }
    t.data = Some(match &host.db {
        Some(path) => trace::load(path, t.target(), clock::wall_now()),
        None => demo::trace(s.snap.at, host.epoch, t.target()),
    });
}

fn set_now(ms: f64) {
    // SAFETY: the stream check runs on one thread; nothing reads the environment concurrently.
    unsafe { std::env::set_var("T3_FIXED_NOW", format!("{ms}")) };
}

fn hash_canvas(h: &mut Sha256, c: &terminal::Canvas) {
    h.update(&(c.w as u64).to_le_bytes());
    h.update(&(c.h as u64).to_le_bytes());
    let mut buf = Vec::with_capacity(c.cells.len() * 13);
    for cell in &c.cells {
        buf.extend_from_slice(&(cell.ch as u32).to_le_bytes());
        buf.extend_from_slice(&cell.fg.to_le_bytes());
        buf.extend_from_slice(&cell.bg.to_le_bytes());
        buf.push(u8::from(cell.bold));
    }
    h.update(&buf);
}

/// SHA-256 (FIPS 180-4), so the check needs no new dependency.
struct Sha256 {
    state: [u32; 8],
    buf: Vec<u8>,
    len: u64,
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
            ],
            buf: Vec::with_capacity(64),
            len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.len += data.len() as u64;
        if !self.buf.is_empty() {
            let take = (64 - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == 64 {
                let block: [u8; 64] = self.buf[..].try_into().unwrap();
                self.block(&block);
                self.buf.clear();
            }
        }
        while data.len() >= 64 {
            self.block(data[..64].try_into().unwrap());
            data = &data[64..];
        }
        self.buf.extend_from_slice(data);
    }

    fn block(&mut self, b: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b2, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b2) ^ (a & c) ^ (b2 & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b2;
            b2 = a;
            a = t1.wrapping_add(t2);
        }
        for (s, v) in self.state.iter_mut().zip([a, b2, c, d, e, f, g, h]) {
            *s = s.wrapping_add(v);
        }
    }

    fn hex(mut self) -> String {
        let bits = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buf.len() != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        self.state.iter().map(|v| format!("{v:08x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Sha256;

    #[test]
    fn sha256_known_answers() {
        let hex = |data: &[u8]| {
            let mut h = Sha256::new();
            h.update(data);
            h.hex()
        };
        assert_eq!(hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let long = vec![b'a'; 1000];
        let mut split = Sha256::new();
        split.update(&long[..3]);
        split.update(&long[3..130]);
        split.update(&long[130..]);
        assert_eq!(split.hex(), "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3");
    }
}
