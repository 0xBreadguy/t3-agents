//! Direct cell-grid port of the dashboard's header, summary tiles, subscription limits,
//! pulse and model totals. Geometry follows oracle/src/view.ts at the experiment baseline.
use crate::model::{self, Agent, State, WINDOWS};
use crate::terminal::{self, Canvas, Palette, Rect};
use crate::ui::Prepared;
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Face {
    Plain,
    Shadow,
}
fn big_width(value: &str, face: Face) -> i32 {
    terminal::big_width(value, face == Face::Shadow)
}
fn draw_big(c: &mut Canvas, x: i32, y: i32, s: &str, color: u32, face: Face, shade: u32) {
    terminal::draw_big(c, x, y, s, color, shade, face == Face::Shadow);
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Limit {
    pub label: String,
    pub title: Option<String>,
    pub percent: f64,
    pub resets_at: Option<f64>,
    pub window_ms: Option<f64>,
    pub elapsed: Option<f64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelUsage {
    pub model: String,
    pub tokens: f64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub tier: String,
    pub limits: Vec<Limit>,
    pub today: f64,
    pub models: Vec<ModelUsage>,
    pub stale: bool,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Overview {
    pub main_top: i32,
    pub needs_you: Rect,
}

fn mix(a: u32, b: u32, t: f64) -> u32 {
    terminal::mix(a, b, t)
}

fn ramp(p: &Palette, t: f64) -> u32 {
    terminal::ramp(p, t)
}
fn zone(p: &Palette, t: f64) -> u32 {
    if t >= 0.9 {
        p.hot
    } else if t >= 0.75 {
        p.warn
    } else {
        ramp(p, p.floor + (p.floor.max(0.84) - p.floor) * (t / 0.75))
    }
}
fn width(s: &str) -> i32 {
    terminal::width(s) as i32
}
fn fit(s: &str, w: i32) -> String {
    terminal::fit(s, w.max(0) as usize)
}
fn tokens(n: f64) -> String {
    terminal::tokens(n)
}
fn duration(s: f64) -> String {
    terminal::duration(s)
}
fn short_duration(s: f64) -> String {
    terminal::short_duration(s)
}
fn ratio(a: &Agent) -> Option<f64> {
    model::ratio(a.context.used_tokens, a.context.max_tokens)
}
fn tool(cat: &str) -> bool {
    !matches!(cat, "you" | "think" | "reply")
}
/// The provider identity index, from the same `family()` the screens and status line use.
fn family(model: &str, hint: Option<&str>) -> usize {
    match terminal::family(model, hint) {
        terminal::ProviderFamily::Claude => 0,
        terminal::ProviderFamily::OpenAi => 1,
        terminal::ProviderFamily::Grok => 2,
        terminal::ProviderFamily::Other => 3,
    }
}
fn family_color(p: &Palette, fam: usize) -> u32 {
    p.providers[fam.min(3)]
}
fn family_of(a: &Agent) -> usize {
    family(
        &a.model,
        a.provider_id
            .as_deref()
            .or_else(|| a.run.get("provider").and_then(|v| v.as_str())),
    )
}
fn level(n: usize, max: usize) -> char {
    terminal::level(n as f64, max as f64)
}
fn section(
    c: &mut Canvas,
    x: i32,
    y: i32,
    w: i32,
    label: &str,
    p: &Palette,
    right: &str,
    right_color: u32,
) {
    terminal::section_with(c, x, y, w, label, p, right, right_color);
}
fn hbar(
    c: &mut Canvas,
    x: i32,
    y: i32,
    w: i32,
    frac: f64,
    track: u32,
    line: bool,
    marker: Option<f64>,
    p: &Palette,
    color: impl Fn(i32, i32) -> u32,
) {
    terminal::hbar_with(
        c,
        x,
        y,
        w,
        frac,
        &terminal::BarOpts {
            track,
            color: &|i, n| color(i as i32, n as i32),
            marker: marker.map(|at| (at, p.bright)),
            line,
        },
    );
}
fn clock(now: f64) -> String {
    crate::clock::clock(now)
}

fn scope_name(scope: &str) -> &str {
    match scope {
        "active" => "active only",
        "t3" => "T3 sidebar",
        _ => "all threads",
    }
}
fn status(a: &Agent, pc: Option<&model::Pace>, s: &State, p: &Palette) -> (char, u32) {
    if model::needs_you(a) {
        return (
            if s.calm || ((s.now as i64).rem_euclid(1000) < 550) {
                '◆'
            } else {
                '◇'
            },
            p.hot,
        );
    }
    if let Some(pc) = pc {
        if pc.quiet {
            return ('◌', if pc.stalled { p.warn } else { p.text2 });
        }
    }
    match a.status.as_str() {
        "running" | "active" => (
            if s.calm {
                '◉'
            } else {
                terminal::spinner(s.now)
            },
            if pc.is_some_and(|p| p.stalled) {
                p.warn
            } else {
                p.bright
            },
        ),
        "starting" | "preparing" | "queued" | "pending" => ('◔', p.text2),
        "completed" => ('✓', p.dim),
        "interrupted" => ('⊘', p.warn),
        "failed" | "error" => ('✗', p.hot),
        _ => ('·', p.dim),
    }
}
pub fn face_for(w: i32, h: i32) -> Face {
    let (_, tw) = tile_grid(w, 6);
    if h >= 44 && tw - 3 >= big_width("100%", Face::Shadow) {
        Face::Shadow
    } else {
        Face::Plain
    }
}
fn tile_grid(w: i32, n: usize) -> (usize, i32) {
    let count = ((w + 1) / 19).clamp(2, n as i32) as usize;
    (count, (w - (count as i32 - 1)) / count as i32)
}
fn big_rows(face: Face) -> i32 {
    match face {
        Face::Plain => 3,
        Face::Shadow => 6,
    }
}
fn big_base(face: Face) -> i32 {
    terminal::big_base(face == Face::Shadow)
}
fn whole(s: &str) -> String {
    terminal::whole(s)
}

/// What the header draws after `calm`, in the slot Storage's tag uses.
pub(crate) enum HeaderBadge<'a> {
    /// Storage's tag. The gap and the label stay on the Live header's columns.
    Storage,
    /// Insights tabs. The active tab is Storage's tag; an inactive one is dim on the
    /// panel, with the same padding, so the words do not move when the tab changes.
    Tabs(&'a [(&'a str, bool)]),
}

pub(crate) fn draw_header(
    c: &mut Canvas,
    s: &State,
    p: &Palette,
    linked: Option<&Agent>,
    storage: bool,
) {
    draw_header_badge(c, s, p, linked, if storage { Some(HeaderBadge::Storage) } else { None });
}

pub(crate) fn draw_header_badge(
    c: &mut Canvas,
    s: &State,
    p: &Palette,
    linked: Option<&Agent>,
    badge: Option<HeaderBadge<'_>>,
) {
    let w = c.w as i32;
    c.fill(0, 0, w, 1, p.panel);
    let mut x = 1 + c.text_bold(1, 0, "T3 Agents", p.bright) + 2;
    if s.paused {
        x += c.text_bold(x, 0, "‖ PAUSED", p.warn);
    } else if s.error.is_some() {
        x += c.text_bold(
            x,
            0,
            if s.calm || s.now % 1000. < 550. {
                "↩ RETRYING"
            } else {
                "  RETRYING"
            },
            p.hot,
        );
    } else {
        let age = if s.calm {
            1.
        } else {
            ((s.now - s.snap.at) / (s.interval * 1000.)).clamp(0., 1.)
        };
        c.set(x, 0, '●', mix(p.bright, p.accent, age), None, false);
        x += 1;
        x += c.text(x + 1, 0, &format!("LIVE {}s", s.interval), p.text2) + 1;
    }
    if s.calm {
        x += 2 + c.text(x + 2, 0, "calm", p.text2);
    }
    // Storage keeps `x + 2` inside the tag draw, so a Live header with Storage does not move.
    let marked = match badge {
        Some(HeaderBadge::Storage) => {
            x += 2 + c.text_bg(x + 2, 0, " STORAGE ", p.bg, p.text2, true);
            true
        }
        Some(HeaderBadge::Tabs(tabs)) if !tabs.is_empty() => {
            // Leave one blank cell before the clock at the minimum Insights width.
            let tab_width: i32 = tabs.iter().map(|(label, _)| width(label) + 2).sum();
            x += (w - 10 - x - tab_width).clamp(0, 2);
            for &(label, active) in tabs {
                let text = format!(" {label} ");
                if active {
                    x += c.text_bg(x, 0, &text, p.bg, p.text2, true);
                } else {
                    x += c.text(x, 0, &text, p.dim);
                }
            }
            true
        }
        _ => false,
    };
    if s.demo && (!marked || x + 8 < w - 9) {
        x += 1 + c.text_bg(x + 1, 0, " DEMO ", p.bg, p.text2, true);
    }
    let room = w - x - 34;
    if let Some(linked) = linked.filter(|_| room >= 12) {
        x += 1 + c.text(x + 1, 0, "▎", p.fg);
        x += c.text(x, 0, "T3 ", p.dim);
        c.text(
            x,
            0,
            &fit(
                &format!(
                    "{}{}",
                    linked.title,
                    if s.follow { "" } else { " · follow off" }
                ),
                room - 4,
            ),
            p.text2,
        );
    }
    let win = WINDOWS[s.window_index % WINDOWS.len()];
    let window = if win >= 60 {
        format!("lane {}h", win / 60)
    } else {
        format!("lane {win}m")
    };
    c.right_with(w - 1, 0, &clock(s.now), p.fg, None, true);
    let scope = if s.scope == "all" {
        window.clone()
    } else {
        format!("{} · {window}", scope_name(&s.scope))
    };
    if w - 11 - width(&scope) > x + 1 {
        c.right(w - 11, 0, &scope, p.dim);
    } else if s.scope != "all" && w - 11 - width(scope_name(&s.scope)) > x + 1 {
        c.right(w - 11, 0, scope_name(&s.scope), p.dim);
    } else if w - 11 - width(&window) > x + 1 {
        c.right(w - 11, 0, &window, p.dim);
    }
}

pub(crate) fn draw_header_kpis_limits(
    c: &mut Canvas,
    s: &State,
    p: &Palette,
    prepared: &Prepared,
) -> Overview {
    let h = c.h as i32;
    let w = c.w as i32;
    let linked = s
        .linked
        .as_ref()
        .and_then(|id| prepared.by_id.get(id.as_str()).map(|i| s.snap.agents[*i].as_ref()));
    draw_header(c, s, p, linked, false);
    let mut result = draw_kpis(c, 1, s, p, face_for(w, h), h >= 30, prepared);
    let limited: Vec<&Provider> = s.usage.iter().filter(|u| !u.limits.is_empty()).collect();
    if !limited.is_empty() {
        result.main_top += if h < 36 {
            limits_line(c, result.main_top, w, &limited, p)
        } else {
            limits(c, result.main_top, w, &limited, p, s.now)
        };
    }
    result
}
struct Tile {
    label: &'static str,
    value: String,
    color: u32,
    quiet: bool,
    alert: bool,
    note: String,
    kind: usize,
}
/// Sorted event indexes support exact inclusive window queries and bin edges
/// without rescanning the snapshot history on each clock tick.
#[derive(Clone, Debug, Default)]
pub(crate) struct EventIndex {
    tools: CountIndex,
    failed: CountIndex,
    pub(crate) all: CountIndex,
}
/// Counts coalesce equal timestamps. Rebuilding prefix sums visits distinct times,
/// rather than every agent's event history. Inclusive endpoints match the oracle.
#[derive(Clone, Debug, Default)]
pub(crate) struct CountIndex {
    counts: std::collections::BTreeMap<u64, usize>,
    prefix: Vec<(f64, usize)>,
    total: usize,
}
impl CountIndex {
    fn key(t: f64) -> u64 {
        let b = t.to_bits();
        if b >> 63 != 0 { !b } else { b ^ (1 << 63) }
    }
    fn change(&mut self, t: f64, add: bool) {
        let key = Self::key(t);
        if add {
            *self.counts.entry(key).or_default() += 1;
        } else if let Some(n) = self.counts.get_mut(&key) {
            *n -= 1;
            if *n == 0 {
                self.counts.remove(&key);
            }
        }
    }
    fn refresh(&mut self) {
        self.total = 0;
        self.prefix = self
            .counts
            .iter()
            .map(|(&k, &n)| {
                self.total += n;
                let b = if k >> 63 != 0 { k ^ (1 << 63) } else { !k };
                (f64::from_bits(b), self.total)
            })
            .collect();
    }
    pub(crate) fn before(&self, t: f64, inclusive: bool) -> usize {
        let i = self
            .prefix
            .partition_point(|(v, _)| if inclusive { *v <= t } else { *v < t });
        i.checked_sub(1).map(|i| self.prefix[i].1).unwrap_or(0)
    }
    fn last(&self) -> Option<f64> {
        self.prefix.last().map(|x| x.0)
    }
}
impl EventIndex {
    pub(crate) fn new(s: &State) -> Self {
        let mut index = Self::default();
        index.update(&[], &s.snap.agents);
        index
    }
    pub(crate) fn update(
        &mut self,
        removed: &[std::sync::Arc<Agent>],
        added: &[std::sync::Arc<Agent>],
    ) {
        for (agents, add) in [(removed, false), (added, true)] {
            for e in agents.iter().flat_map(|a| &a.events) {
                self.all.change(e.t, add);
                if tool(&e.cat) {
                    self.tools.change(e.t, add);
                }
                if e.failed {
                    self.failed.change(e.t, add);
                }
            }
        }
        self.all.refresh();
        self.tools.refresh();
        self.failed.refresh();
    }
}
fn draw_kpis(
    c: &mut Canvas,
    y: i32,
    s: &State,
    p: &Palette,
    face: Face,
    big: bool,
    prepared: &Prepared,
) -> Overview {
    let w = c.w as i32;
    let agents = &s.snap.agents;
    let now = s.now;
    let window = WINDOWS[s.window_index % 4] as f64 * 60_000.;
    let paces = &prepared.paces;
    let running: Vec<&Agent> = agents
        .iter()
        .map(|a| &**a)
        .filter(|a| model::running(a))
        .collect();
    let waiting: Vec<&Agent> = agents
        .iter()
        .map(|a| &**a)
        .filter(|a| model::wants_attention(a))
        .collect();
    let mut stopped = 0;
    let mut open_while_working = 0;
    let mut open_idle = 0;
    for a in &waiting {
        let count = model::pending_count(a);
        if model::needs_you(a) {
            stopped += count;
        } else if model::running(a) {
            open_while_working += count;
        } else {
            open_idle += count;
        }
    }
    let pending = stopped + open_while_working + open_idle;
    let (count, tw) = tile_grid(w, 6);
    let spark_w = (if count == 3 { w - 2 * (tw + 1) } else { tw } - 3).max(1);
    let size = ((window / spark_w as f64 / 1000.).round() * 1000.).max(1000.);
    let last = (now / size).floor();
    let index = if s.cache_enabled {
        let mut cache = s.render_cache.borrow_mut();
        cache
            .events
            .get_or_insert_with(|| std::sync::Arc::new(EventIndex::new(s)))
            .clone()
    } else {
        std::sync::Arc::new(EventIndex::new(s))
    };
    let recent = index.tools.total - index.tools.before(now - 300_000., false);
    let fails = index.failed.total - index.failed.before(now - window, false);
    let last_fail = index.failed.last().filter(|t| *t >= now - window);
    let tool_counts: Vec<_> = (0..spark_w)
        .map(|i| {
            let lower = (last - (spark_w - 1 - i) as f64) * size;
            let end = if i == spark_w - 1 {
                index.tools.total
            } else {
                index.tools.before(lower + size, false)
            };
            end - index.tools.before(lower, false)
        })
        .collect();
    let rate = recent as f64 / 5.;
    let peak = agents
        .iter()
        .filter(|a| running.is_empty() || model::running(a))
        .filter(|a| ratio(a).is_some())
        .reduce(|a, b| {
            if ratio(b).unwrap() > ratio(a).unwrap() {
                b
            } else {
                a
            }
        });
    let peak_ratio = peak.and_then(|a| ratio(a));
    let today = s.usage.iter().map(|u| u.today).sum::<f64>();
    let blind = s.snap.at == 0. || s.error.is_some();
    let mut tiles = vec![
        Tile {
            label: "ACTIVE",
            value: running.len().to_string(),
            color: if running.is_empty() { p.dim } else { p.bright },
            quiet: running.is_empty(),
            alert: false,
            note: if running.is_empty() {
                "nothing running".into()
            } else {
                String::new()
            },
            kind: 0,
        },
        Tile {
            label: "NEEDS YOU",
            value: pending.to_string(),
            color: if waiting.is_empty() { p.dim } else { p.hot },
            quiet: waiting.is_empty(),
            alert: !waiting.is_empty(),
            note: if waiting.is_empty() {
                "nothing pending".into()
            } else {
                String::new()
            },
            kind: 1,
        },
        Tile {
            label: "TOOLS / MIN",
            value: if rate >= 10. {
                terminal::js_number(terminal::js_round(rate))
            } else if rate > 0. {
                terminal::to_fixed(rate, 1)
            } else {
                "0".into()
            },
            color: p.fg,
            quiet: rate == 0.,
            alert: false,
            note: String::new(),
            kind: 2,
        },
        Tile {
            label: "PEAK CONTEXT",
            value: peak_ratio
                .map(|p| format!("{}%", terminal::js_number(terminal::js_round(p * 100.))))
                .unwrap_or_else(|| "—".into()),
            color: peak_ratio.map(|r| zone(p, r.max(0.5))).unwrap_or(p.dim),
            quiet: peak_ratio.is_none(),
            alert: false,
            note: peak
                .filter(|_| peak_ratio.is_some())
                .map(|a| a.title.clone())
                .unwrap_or_else(|| {
                    if agents.is_empty() {
                        "no agents".into()
                    } else {
                        "not reported".into()
                    }
                }),
            kind: 3,
        },
        Tile {
            label: "TOKENS TODAY",
            value: if s.usage.is_empty() {
                "—".into()
            } else {
                tokens(today)
            },
            color: if today > 0. { p.fg } else { p.dim },
            quiet: today == 0. || s.usage.is_empty(),
            alert: false,
            note: if s.usage.is_empty() {
                "no Omarchy usage data".into()
            } else if today == 0. {
                "none yet today".into()
            } else {
                String::new()
            },
            kind: 4,
        },
        Tile {
            label: "FAILED CALLS",
            value: fails.to_string(),
            color: if let Some(last) = last_fail {
                if now - last < 600_000. { p.hot } else { p.warn }
            } else {
                p.dim
            },
            quiet: fails == 0,
            alert: false,
            note: last_fail
                .map(|last| format!("last {} ago", duration((now - last) / 1000.)))
                .unwrap_or_else(|| format!("none in {}m", WINDOWS[s.window_index % 4])),
            kind: 5,
        },
    ];
    if blind {
        for t in &mut tiles {
            if t.kind != 4 {
                t.value = "—".into();
                t.color = p.dim;
                t.quiet = true;
                t.alert = false;
                t.note = "can't read T3".into();
            }
        }
    }
    let rows = if big { big_rows(face) } else { 1 };
    let gap = if big { 1 } else { 0 };
    let height = rows + gap + 2;
    let base = if big { 1 + big_base(face) } else { 1 };
    let mut needs = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };
    for (n, t) in tiles.iter().take(count).enumerate() {
        let x = n as i32 * (tw + 1);
        let ww = if n == count - 1 { w - x } else { tw };
        c.fill(x, y, ww, height, p.panel);
        if t.alert {
            let steady = s.calm || waiting.iter().all(|a| !model::needs_you(a) || s.acked.contains(&a.id));
            let edge = if steady || (now as i64).rem_euclid(1000) < 550 {
                p.hot
            } else {
                mix(p.hot, p.panel, 0.6)
            };
            for yy in y..y + height {
                c.set(x, yy, '▌', edge, Some(p.panel), false);
            }
        }
        if t.kind == 1 {
            needs = Rect {
                x,
                y,
                w: ww,
                h: height,
            };
        }
        c.text_bold(
            x + 2,
            y,
            &fit(t.label, ww - 3),
            if t.alert { p.hot } else { p.dim },
        );
        let shown = if big && !t.quiet {
            let a = t.value.clone();
            let b = whole(&t.value);
            if big_width(&a, face) <= ww - 3 {
                Some(a)
            } else if big_width(&b, face) <= ww - 3 {
                Some(b)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(value) = shown {
            draw_big(
                c,
                x + 2,
                y + 1,
                &value,
                t.color,
                face,
                mix(t.color, p.panel, 0.6),
            );
        } else if t.quiet {
            c.text(x + 2, y + base, &fit(&t.value, ww - 3), p.dim);
        } else {
            c.text_bold(x + 2, y + base, &fit(&t.value, ww - 3), t.color);
        }
        let sx = x + 2;
        let sy = y + rows + gap + 1;
        let sw = ww - 3;
        if blind && t.kind != 4 || !t.note.is_empty() {
            c.text(
                sx,
                sy,
                &fit(&t.note, sw),
                if blind { p.text2 } else { p.dim },
            );
            continue;
        }
        match t.kind {
            0 => {
                let stalled = running
                    .iter()
                    .filter(|a| paces.get(&a.id).is_some_and(|p| p.quiet && p.stalled))
                    .count();
                let slow = running
                    .iter()
                    .filter(|a| paces.get(&a.id).is_some_and(|p| !p.quiet && p.stalled))
                    .count();
                let quiet = running
                    .iter()
                    .filter(|a| paces.get(&a.id).is_some_and(|p| p.quiet && !p.stalled))
                    .count();
                let parts: Vec<(String, u32)> = vec![
                    (stalled, format!("{stalled} stalled"), p.warn),
                    (slow, format!("△ {slow} slow"), p.warn),
                    (quiet, format!("{quiet} quiet"), p.text2),
                ]
                .into_iter()
                .filter(|(n, _, _)| *n > 0)
                .map(|(_, t, c)| (t, c))
                .collect();
                let mut shown = usize::from(!parts.is_empty());
                let words = |n: usize| parts.iter().take(n).map(|(t, _)| width(t) + 1).sum::<i32>();
                while shown < parts.len() && running.len() as i32 * 2 + words(shown + 1) <= sw {
                    shown += 1;
                }
                let reserved = words(shown);
                let mut col = 0;
                for a in &running {
                    if col + 2 > sw - reserved {
                        break;
                    }
                    let (glyph, color) = status(a, paces.get(&a.id), s, p);
                    c.set(sx + col, sy, glyph, color, None, false);
                    col += 2;
                }
                for (word, color) in parts.iter().take(shown) {
                    col += c.text(sx + col, sy, word, *color) + 1;
                }
            }
            1 => {
                let open = open_while_working + open_idle;
                if open > 0 {
                    let active = if open_idle == 0 { "working" } else { "open" };
                    let async_text = format!("◇{open} {active}");
                    let mut x = sx;
                    if stopped > 0 {
                        let count = format!("◆{stopped}");
                        x += c.text_bold(x, sy, &fit(&count, sw), p.hot);
                        if width(&count) + width(" stopped · ") + width(&async_text) <= sw {
                            x += c.text(x, sy, " stopped", p.hot);
                        }
                        x += c.text(x, sy, &fit(" · ", sx + sw - x), p.dim);
                    }
                    c.text(x, sy, &fit(&async_text, sx + sw - x), p.hot);
                    continue;
                }
                let first = waiting
                    .iter()
                    .min_by(|a, b| {
                        s.acked
                            .contains(&a.id)
                            .cmp(&s.acked.contains(&b.id))
                            .then_with(|| {
                                model::waiting_since(a)
                                    .unwrap_or(now)
                                    .total_cmp(&model::waiting_since(b).unwrap_or(now))
                            })
                    })
                    .copied();
                if let Some(a) = first {
                    let request = a.items.iter().find(|i| {
                        model::category(&i.kind, i.tool.as_deref()) == "ask"
                            && matches!(
                                i.status.as_str(),
                                "running"
                                    | "starting"
                                    | "preparing"
                                    | "queued"
                                    | "waiting"
                                    | "pending"
                                    | "active"
                            )
                    });
                    let title = request
                        .map(|i| i.display(&s.home))
                        .unwrap_or_else(|| a.title.clone());
                    let mut tail = model::waiting_since(a)
                        .map(|at| format!(" {}", short_duration((now - at).max(0.) / 1000.)))
                        .unwrap_or_default();
                    let more = pending - model::pending_count(a);
                    if more > 0 {
                        tail += &format!(" +{more} more");
                    }
                    let room = sw - 2 - width(&tail);
                    let what = if room >= 4 {
                        fit(&title, room)
                    } else {
                        String::new()
                    };
                    // The original passes `{ max }` here: the tile clips the text rather than
                    // shortening it with an ellipsis, which matters on a narrow tile.
                    let used = c.text_with(
                        sx,
                        sy,
                        &format!("◆ {what}"),
                        p.hot,
                        None,
                        false,
                        sw.max(0) as usize,
                    );
                    c.text_with(
                        sx + used,
                        sy,
                        if what.is_empty() { tail.trim() } else { &tail },
                        p.text2,
                        None,
                        false,
                        (sw - used).max(0) as usize,
                    );
                }
            }
            2 => {
                debug_assert_eq!(sw.max(1), spark_w);
                let counts = &tool_counts;
                let max = counts.iter().copied().max().unwrap_or(0).max(1);
                for (i, n) in counts.iter().enumerate() {
                    c.set(
                        sx + i as i32,
                        sy,
                        if *n > 0 { level(*n, max) } else { '▁' },
                        if *n > 0 {
                            ramp(p, 0.3 + 0.7 * (*n as f64 / max as f64))
                        } else {
                            p.track
                        },
                        None,
                        false,
                    );
                }
            }
            4 => {
                let mut col = 0;
                for u in s.usage.iter().filter(|u| u.today > 0.) {
                    let t = format!("{} {}", u.id, tokens(u.today));
                    if col + width(&t) + 2 > sw {
                        break;
                    }
                    c.set(
                        sx + col,
                        sy,
                        '●',
                        family_color(p, family(&u.id, None)),
                        None,
                        false,
                    );
                    col += 2 + c.text(sx + col + 2, sy, &t, p.dim) + 2;
                }
            }
            _ => {}
        }
    }
    Overview {
        main_top: y + height,
        needs_you: needs,
    }
}

fn clean_limit_label(l: &Limit) -> String {
    let label = terminal::limit_label(&l.label);
    if let Some(t) = &l.title {
        format!("{t} {label}")
    } else {
        label
    }
}
fn limits(c: &mut Canvas, y: i32, w: i32, providers: &[&Provider], p: &Palette, now: f64) -> i32 {
    let rows: Vec<_> = providers
        .iter()
        .flat_map(|u| {
            u.limits
                .iter()
                .enumerate()
                .map(move |(i, l)| (*u, l, i == 0))
        })
        .take(6)
        .collect();
    let cols = if w >= 180 && rows.len() > 1 { 2 } else { 1 };
    let per_col = (rows.len() + cols - 1) / cols;
    let col_w = (w - 2 - (cols as i32 - 1) * 3) / cols as i32;
    section(
        c,
        1,
        y,
        w - 2,
        "LIMITS",
        p,
        "│ marks time elapsed in each window",
        p.dim,
    );
    for (n, (u, l, first)) in rows.iter().enumerate() {
        let col = n / per_col;
        limit_row(
            c,
            1 + col as i32 * (col_w + 3),
            y + 1 + (n % per_col) as i32,
            col_w,
            u,
            l,
            *first || n % per_col == 0,
            p,
            now,
        );
    }
    per_col as i32 + 1
}
fn limits_line(c: &mut Canvas, y: i32, w: i32, providers: &[&Provider], p: &Palette) -> i32 {
    let mut x = 1 + c.text_bold(1, y, "LIMITS", p.text2) + 2;
    let mut worst: Vec<_> = providers
        .iter()
        .filter_map(|u| {
            u.limits
                .iter()
                .max_by(|a, b| a.percent.total_cmp(&b.percent))
                .map(|l| (*u, l))
        })
        .collect();
    worst.sort_by(|a, b| b.1.percent.total_cmp(&a.1.percent));
    for (i, (u, l)) in worst.iter().enumerate() {
        let name = format!(
            "{} {}",
            u.name.split(' ').next().unwrap_or(""),
            clean_limit_label(l)
        );
        let hot = l.percent >= 0.9;
        let ahead = l.elapsed.is_some_and(|e| l.percent > e + 0.08) && l.percent >= 0.15;
        let pct = format!("{}%", terminal::js_number(terminal::js_round(l.percent * 100.)));
        let ww = 2 + width(&name) + 1 + 8 + 1 + width(&pct) + if ahead { 2 } else { 0 };
        let more = worst.len() - i - 1;
        if x + ww
            + if more > 0 {
                format!("+{more}").len() as i32 + 3
            } else {
                0
            }
            > w - 1
        {
            let more = format!("+{}", worst.len() - i);
            if x + width(&more) <= w - 1 {
                c.text(x, y, &more, p.dim);
            }
            break;
        }
        c.set(x, y, '●', family_color(p, family(&u.id, None)), None, false);
        x += 2 + c.text(x + 2, y, &name, p.text2) + 1;
        hbar(
            c,
            x,
            y,
            8,
            l.percent,
            p.rule,
            true,
            l.elapsed,
            p,
            |k, cells| {
                if hot {
                    p.hot
                } else {
                    ramp(p, terminal::along(0.3, 0.7, k as f64, cells as f64))
                }
            },
        );
        x += 9 + c.text_bold(x + 9, y, &pct, if hot { p.hot } else { p.fg });
        if ahead {
            x += 1 + c.text(x + 1, y, "▲", p.hot);
        }
        x += 3;
    }
    1
}
fn limit_row(
    c: &mut Canvas,
    x0: i32,
    y: i32,
    w: i32,
    u: &Provider,
    l: &Limit,
    named: bool,
    p: &Palette,
    now: f64,
) {
    let narrow = w < 96;
    if named {
        c.set(x0, y, '●', family_color(p, family(&u.id, None)), None, false);
    }
    c.text_bold(
        x0 + 2,
        y,
        &if named {
            fit(&u.name, 11)
        } else {
            String::new()
        },
        p.text2,
    );
    let mut x = x0 + 14;
    if !narrow {
        c.text(
            x,
            y,
            &if named {
                fit(&u.tier, 9)
            } else {
                String::new()
            },
            p.dim,
        );
        x += 10;
    }
    let lw = if narrow { 11 } else { 14 };
    c.text(x, y, &fit(&clean_limit_label(l), lw - 1), p.dim);
    x += lw;
    let hot = l.percent >= 0.9;
    let ahead = l.elapsed.is_some_and(|e| l.percent > e + 0.08) && l.percent >= 0.15;
    let reset = l
        .resets_at
        .map(|r| format!("↩ {}", short_duration((r - now).max(0.) / 1000.)))
        .unwrap_or_default();
    let pace = if narrow {
        ""
    } else if hot {
        "near limit"
    } else if ahead {
        "▲ ahead of pace"
    } else if u.stale {
        "stale"
    } else {
        "on pace"
    };
    let bw = (x0 + w
        - x
        - 5
        - if !reset.is_empty() { 9 } else { 0 }
        - if !pace.is_empty() { 16 } else { 0 })
    .clamp(6, 72);
    hbar(
        c,
        x,
        y,
        bw,
        l.percent,
        p.rule,
        true,
        l.elapsed,
        p,
        |i, cells| {
            if hot {
                p.hot
            } else if ahead {
                mix(ramp(p, terminal::along(0.4, 0.6, i as f64, cells as f64)), p.warn, 0.7)
            } else {
                ramp(p, terminal::along(0.3, 0.7, i as f64, cells as f64))
            }
        },
    );
    x += bw + 1;
    x += c.text_bold(
        x,
        y,
        &format!("{:>4}", format!("{}%", terminal::js_number(terminal::js_round(l.percent * 100.)))),
        if hot { p.hot } else { p.fg },
    ) + 1;
    if !reset.is_empty() {
        c.text(x, y, &reset, p.dim);
    }
    if !pace.is_empty() {
        c.text(x + 9, y, pace, if hot || ahead { p.hot } else { p.dim });
    }
}

pub fn draw_extras(c: &mut Canvas, r: Rect, s: &State, p: &Palette) {
    let mut models: Vec<_> = s
        .usage
        .iter()
        .flat_map(|u| u.models.iter().map(move |m| (m, family(&m.model, Some(&u.id)))))
        .collect();
    models.sort_by(|a, b| b.0.tokens.total_cmp(&a.0.tokens));
    let model_rows = models.len().min(6);
    let models_h = if model_rows > 0 {
        model_rows as i32 + 2
    } else {
        0
    };
    let pulse_room = r.h - models_h;
    let mut y = r.y;
    if pulse_room >= 6 {
        y += pulse(c, r.x, y, r.w, (pulse_room - 4).min(8), s, p);
    }
    if model_rows > 0 && r.y + r.h - y >= 3 {
        section(
            c,
            r.x,
            y,
            r.w,
            "MODELS TODAY",
            p,
            "Omarchy usage records",
            p.dim,
        );
        y += 1;
        let max = models[0].0.tokens;
        let label_w = (models
            .iter()
            .take(model_rows)
            .map(|(m, _)| width(&m.model))
            .max()
            .unwrap_or(0)
            + 1)
        .min(22);
        for (m, fam) in models
            .into_iter()
            .take(model_rows.min((r.y + r.h - y).max(0) as usize))
        {
            let value = tokens(m.tokens);
            c.set(r.x, y, '●', family_color(p, fam), None, false);
            c.text(r.x + 2, y, &fit(&m.model, label_w - 1), p.text2);
            let bw = (r.w - label_w - 4 - width(&value)).max(4);
            hbar(
                c,
                r.x + 2 + label_w,
                y,
                bw,
                m.tokens / max,
                p.bg,
                false,
                None,
                p,
                |i, cells| {
                    mix(
                        family_color(p, fam),
                        p.bg,
                        terminal::along(0.5, -0.3, i as f64, cells as f64),
                    )
                },
            );
            c.right(r.x + r.w, y, &value, p.fg);
            y += 1;
        }
    }
}
fn pulse(c: &mut Canvas, x: i32, y: i32, w: i32, h: i32, s: &State, p: &Palette) -> i32 {
    if w <= 0 || h <= 0 {
        return 0;
    }
    let slots = w as usize * 2;
    let window = WINDOWS[s.window_index % 4] as f64 * 60_000.;
    let size = ((window / slots as f64 / 1000.).round() * 1000.).max(1000.);
    let last = (s.now / size).floor();
    let mut counts = vec![0usize; slots];
    let mut by_family = vec![[0usize; 4]; w as usize];
    let mut failed = vec![false; w as usize];
    for a in &s.snap.agents {
        // The family is a pure function of the agent's model names and is only needed for an
        // event in the window: work it out at the first one, not for every agent on every frame.
        let mut fam = None;
        for e in &a.events {
            if e.t > s.now + 5000. || !tool(&e.cat) {
                continue;
            }
            let i = (slots as i64 - 1 - (last - (e.t / size).floor()) as i64).min(slots as i64 - 1);
            if i < 0 {
                continue;
            }
            let i = i as usize;
            counts[i] += 1;
            by_family[i / 2][*fam.get_or_insert_with(|| family_of(a))] += 1;
            failed[i / 2] |= e.failed;
        }
    }
    let max = counts.iter().copied().max().unwrap_or(0).max(1);
    let any = counts.iter().any(|n| *n > 0);
    let per_min = max as f64 / (size / 60_000.);
    let rate = if per_min >= 10. {
        terminal::js_number(terminal::js_round(per_min))
    } else {
        terminal::to_fixed(per_min, 1)
    };
    section(
        c,
        x,
        y,
        w,
        "PULSE",
        p,
        &if any {
            format!("tool calls, all agents · peak {rate}/min")
        } else {
            "no tool calls in this window".into()
        },
        p.dim,
    );
    let mut bits = vec![0u8; (w * h) as usize];
    let map = [[0x01u8, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
    let pixel_h = h * 4;
    for (i, n) in counts.iter().enumerate() {
        if *n == 0 {
            continue;
        }
        let frac = (*n as f64 / max as f64).max(0.08);
        let top = pixel_h - (frac * pixel_h as f64).round() as i32;
        for py in top..pixel_h {
            bits[(py / 4 * w + i as i32 / 2) as usize] |= map[(py % 4) as usize][i % 2];
        }
    }
    for row in 0..h {
        for col in 0..w {
            let b = bits[(row * w + col) as usize];
            if b == 0 {
                continue;
            }
            let f = by_family[col as usize];
            let mut fam = 3;
            for next in 0..4 {
                if f[next] > f[fam] {
                    fam = next;
                }
            }
            let lift = 0.1 + 0.3 * (1. - row as f64 / h as f64);
            let color = if fam == 3 {
                ramp(p, 0.4 + 0.6 * (1. - row as f64 / h as f64))
            } else {
                mix(family_color(p, fam), p.bright, lift)
            };
            c.set(
                x + col,
                y + 1 + row,
                char::from_u32(0x2800 + b as u32).unwrap(),
                color,
                None,
                false,
            );
        }
    }
    for col in 0..w {
        let i = col as usize * 2;
        if failed[col as usize] {
            let peak = counts[i].max(counts[i + 1]);
            let top = (pixel_h
                - ((peak as f64 / max as f64).max(0.08) * pixel_h as f64).round() as i32)
                / 4;
            c.set(x + col, y + 1 + (top - 1).max(0), '×', p.hot, None, true);
        }
        if counts[i] == 0 && counts[i + 1] == 0 {
            c.set(x + col, y + h, '▁', p.track, None, false);
        }
    }
    let mins = WINDOWS[s.window_index % 4];
    c.text(
        x,
        y + h + 1,
        &if mins >= 60 {
            format!("-{}h", terminal::fixed_number(mins as f64 / 60., 1))
        } else {
            format!("-{}m", terminal::js_number(terminal::js_round(mins as f64)))
        },
        p.dim,
    );
    c.right(x + w, y + h + 1, "now", p.dim);
    let mut lx = x + 8;
    for fam in 0..4 {
        if !s.snap.agents.iter().any(|a| family_of(a) == fam) {
            continue;
        }
        let name = ["claude", "gpt · codex", "grok", "other"][fam];
        if lx + width(name) + 4 > x + w - 5 {
            break;
        }
        c.set(lx, y + h + 1, '●', family_color(p, fam), None, false);
        lx += 2 + c.text(lx + 2, y + h + 1, name, p.dim) + 2;
    }
    h + 3
}

/// Exact baseline DemoUsage seed, with reset timestamps derived from the supplied clock.
/// Invented usage only; never reads Omarchy's records or account state.
pub fn demo_usage(now: f64) -> Vec<Provider> {
    let limit = |label: &str, title: Option<&str>, percent: f64, hours: f64, window: f64| Limit {
        label: label.into(),
        title: title.map(Into::into),
        percent,
        resets_at: Some(now + hours * 3_600_000.),
        window_ms: Some(window),
        elapsed: Some((1. - hours * 3_600_000. / window).clamp(0., 1.)),
    };
    let model = |model: &str, tokens: f64| ModelUsage {
        model: model.into(),
        tokens,
    };
    vec![
        Provider {
            id: "claude".into(),
            name: "Claude Code".into(),
            tier: "Max".into(),
            limits: vec![
                limit("Session (5-hour)", None, 0.68, 2.6, 18_000_000.),
                limit("Weekly (7-day)", None, 0.41, 82., 604_800_000.),
                limit("Weekly (7-day)", Some("Opus"), 0.92, 82., 604_800_000.),
            ],
            today: 18_400_000.,
            models: vec![
                model("claude-opus-5-5", 11_200_000.),
                model("claude-sonnet-5-5", 5_900_000.),
                model("claude-haiku-5-5", 1_300_000.),
            ],
            stale: false,
        },
        Provider {
            id: "codex".into(),
            name: "Codex".into(),
            tier: "Pro".into(),
            limits: vec![
                limit("Session (5-hour)", None, 0.23, 3.85, 18_000_000.),
                limit("Weekly (7-day)", None, 0.47, 68., 604_800_000.),
            ],
            today: 6_100_000.,
            models: vec![model("gpt-6.1-sol", 6_100_000.)],
            stale: false,
        },
        Provider {
            id: "grok".into(),
            name: "Grok".into(),
            tier: "SuperGrok".into(),
            limits: vec![],
            today: 3_100_000.,
            models: vec![
                model("grok-4.7", 2_400_000.),
                model("grok-4.7-fast", 700_000.),
            ],
            stale: false,
        },
    ]
}

/// Moving-clock demo usage, matching `demoUsage(now, epoch)` rather than restarting
/// allowance windows at every refresh.
pub fn demo_usage_at(now: f64, epoch: f64) -> Vec<Provider> {
    let mut providers = demo_usage(epoch);
    let minutes = ((now - epoch) / 60_000.).max(0.);
    for provider in &mut providers {
        for limit in &mut provider.limits {
            if let (Some(reset), Some(window)) = (limit.resets_at, limit.window_ms) {
                if reset <= now {
                    limit.percent = 0.;
                    limit.elapsed = Some(0.);
                } else {
                    limit.elapsed = Some((1. - (reset - now) / window).clamp(0., 1.));
                }
            }
        }
        for model in &mut provider.models {
            let per_min = match model.model.as_str() {
                "claude-opus-5-5" => 42_000.,
                "claude-sonnet-5-5" => 31_000.,
                "claude-haiku-5-5" => 4_000.,
                "gpt-6.1-sol" => 18_000.,
                "grok-4.7" => 22_000.,
                "grok-4.7-fast" => 9_000.,
                _ => 0.,
            };
            model.tokens = (model.tokens + minutes * per_min).round();
        }
        provider.today = provider.models.iter().map(|m| m.tokens).sum();
    }
    providers.sort_by(|a, b| b.today.total_cmp(&a.today));
    providers
}

/// Convert an Omarchy usage-record value into only the counters the dashboard draws.
/// Parsing is separate from I/O: callers choose whether to read any local record.
pub fn parse_provider(record: &serde_json::Value, now: f64) -> Option<Provider> {
    use serde_json::Value;
    if !record.is_object() || record.get("ready") == Some(&Value::Bool(false)) {
        return None;
    }
    fn number(v: Option<&Value>) -> Option<f64> {
        match v? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => {
                if s.trim().is_empty() {
                    Some(0.)
                } else {
                    s.trim().parse().ok()
                }
            }
            Value::Bool(b) => Some(if *b { 1. } else { 0. }),
            Value::Null => Some(0.),
            _ => None,
        }
        .filter(|n| n.is_finite())
    }
    fn string(v: Option<&Value>, fallback: &str) -> String {
        match v {
            None | Some(Value::Null) => fallback.into(),
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
        }
    }
    let mut limits = Vec::new();
    if let Some(entries) = record.get("limits").and_then(Value::as_array) {
        for entry in entries {
            let Some(percent) = number(entry.get("percent")).filter(|p| *p >= 0.) else {
                continue;
            };
            let label = string(entry.get("label"), "Limit");
            let reset = entry
                .get("resetsAt")
                .and_then(Value::as_str)
                .and_then(|s| model::ms(Some(s)));
            let window = window_length(&label);
            let expired = reset.is_some_and(|r| r <= now);
            let elapsed = match (reset, window) {
                (Some(reset), Some(window)) => Some(if expired {
                    0.
                } else {
                    (1. - (reset - now) / window).clamp(0., 1.)
                }),
                _ => None,
            };
            let title = entry
                .get("title")
                .filter(|v| match v {
                    Value::Null => false,
                    Value::Bool(b) => *b,
                    Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.),
                    Value::String(s) => !s.is_empty(),
                    _ => true,
                })
                .map(|v| string(Some(v), ""));
            limits.push(Limit {
                label,
                title,
                percent: if expired { 0. } else { percent.min(1.) },
                resets_at: reset,
                window_ms: window,
                elapsed,
            });
        }
    }
    let today = number(record.get("todayTotalTokens")).unwrap_or(0.);
    if limits.is_empty() && today == 0. {
        return None;
    }
    let mut models: Vec<ModelUsage> = record
        .get("todayTokensByModel")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|m| m.iter())
        .filter_map(|(name, value)| {
            number(Some(value))
                .filter(|n| *n > 0.)
                .map(|tokens| ModelUsage {
                    model: name.clone(),
                    tokens,
                })
        })
        .collect();
    models.sort_by(|a, b| b.tokens.total_cmp(&a.tokens));
    let id = string(record.get("id"), "agent");
    Some(Provider {
        name: string(
            record
                .get("name")
                .filter(|v| !v.is_null())
                .or_else(|| record.get("id")),
            "Agent",
        ),
        id,
        tier: string(record.get("tierLabel"), ""),
        limits,
        today,
        models,
        stale: record.get("limitsStale").is_some_and(|v| match v {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.),
            Value::String(s) => !s.is_empty(),
            _ => true,
        }),
    })
}
/// Same hour/day/week/month label windows as oracle/src/usage.ts.
pub fn window_length(label: &str) -> Option<f64> {
    let text = label.to_lowercase();
    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        if !bytes[start].is_ascii_digit() {
            continue;
        }
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let n = text.get(start..end)?.parse::<f64>().ok()?;
        let after = text[end..].trim_start_matches(crate::terminal::is_js_whitespace);
        let after = after.strip_prefix('-').unwrap_or(after);
        let after = after.trim_start_matches(crate::terminal::is_js_whitespace);
        let unit = if after.starts_with("hour") {
            Some(3_600_000.)
        } else if after.starts_with("day") {
            Some(86_400_000.)
        } else if after.starts_with("week") {
            Some(604_800_000.)
        } else if after.starts_with("month") {
            Some(2_592_000_000.)
        } else {
            None
        };
        if let Some(unit) = unit {
            return Some(n * unit);
        }
    }
    if text.contains("weekly") {
        Some(604_800_000.)
    } else if text.contains("monthly") {
        Some(2_592_000_000.)
    } else {
        None
    }
}
