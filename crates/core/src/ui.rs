//! Direct cell-grid port of the original dashboard composition and list.
//! The native host changes the presentation backend, not the product's visual language.
use crate::{
    details,
    model::{self, Agent, Pace, State},
    overview,
    terminal::{self, Canvas, Palette, Rect},
};
use std::collections::HashMap;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hit {
    pub rect: Rect,
    pub id: String,
    pub earlier: bool,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Surface {
    pub list: Rect,
    pub detail: Rect,
    pub needs_you: Rect,
    pub hits: Vec<Hit>,
    pub detail_max: usize,
    pub help_max: usize,
    pub list_scroll: usize,
    pub system_reduced: bool,
    pub selected_agent: Option<usize>,
}
/// Snapshot indexes and time-sensitive classifications shared by the presenters.
/// State owns one cached version; visible duration labels are evaluated at draw time.
#[derive(Clone, Debug)]
pub(crate) struct Prepared {
    pub rows: std::sync::Arc<model::Rows>,
    pub paces: std::sync::Arc<HashMap<String, Pace>>,
    pub by_id: std::sync::Arc<HashMap<String, usize>>,
    pub row_by_id: std::sync::Arc<HashMap<String, usize>>,
    linked: std::sync::Arc<Vec<bool>>,
    descendants_waiting: std::sync::Arc<Vec<bool>>,
    prefix: std::sync::Arc<RowCounts>,
    shown: usize,
    live: usize,
}
#[derive(Clone, Debug)]
struct PreparedShape {
    rows: std::sync::Arc<model::Rows>,
    linked_id: Option<String>,
    row_by_id: std::sync::Arc<HashMap<String, usize>>,
    linked: std::sync::Arc<Vec<bool>>,
    descendants_waiting: std::sync::Arc<Vec<bool>>,
}
/// Fenwick counters support hidden-row range totals without rewriting a suffix.
#[derive(Clone, Debug)]
struct RowCounts {
    rows: std::sync::Arc<model::Rows>,
    positions: Vec<Vec<usize>>,
    contributions: Vec<[usize; 5]>,
    tree: Vec<[isize; 5]>,
    reference: Option<Vec<[usize; 5]>>,
    shown: usize,
    live: usize,
}
impl RowCounts {
    fn contribution(a: &Agent, pc: Option<&Pace>) -> [usize; 5] {
        [
            usize::from(model::needs_you(a)),
            usize::from(pc.is_some_and(|p| p.quiet && p.stalled)),
            usize::from(pc.is_some_and(|p| !p.quiet && p.stalled)),
            usize::from(matches!(a.status.as_str(), "failed" | "error")),
            usize::from(model::wants_attention(a) && !model::needs_you(a)),
        ]
    }
    fn add(&mut self, row: usize, delta: [isize; 5]) {
        let mut at = row + 1;
        while at < self.tree.len() {
            for k in 0..5 {
                self.tree[at][k] += delta[k];
            }
            at += at & at.wrapping_neg();
        }
    }
    fn sum(&self, end: usize) -> [usize; 5] {
        if let Some(prefix) = &self.reference {
            return prefix[end];
        }
        let mut at = end;
        let mut total = [0isize; 5];
        while at > 0 {
            for k in 0..5 {
                total[k] += self.tree[at][k];
            }
            at &= at - 1;
        }
        total.map(|x| x as usize)
    }
    fn new(rows: std::sync::Arc<model::Rows>, s: &State, paces: &HashMap<String, Pace>) -> Self {
        let mut counts = Self {
            positions: vec![Vec::new(); s.snap.agents.len()],
            contributions: s
                .snap
                .agents
                .iter()
                .map(|a| Self::contribution(a, paces.get(&a.id)))
                .collect(),
            tree: vec![[0; 5]; rows.len() + 1],
            reference: None,
            rows,
            shown: 0,
            live: 0,
        };
        for r in 0..counts.rows.len() {
            let row = &counts.rows[r];
            let members: Vec<_> = (!row.is_earlier())
                .then_some(row.agent)
                .into_iter()
                .chain(row.earlier.iter().copied())
                .collect();
            if !row.is_earlier() {
                counts.shown += 1;
                counts.live += usize::from(model::live(&s.snap.agents[row.agent]));
            }
            for i in members {
                counts.positions[i].push(r);
            }
        }
        // Linear Fenwick construction, including repeated membership exactly once per occurrence.
        for (i, positions) in counts.positions.iter().enumerate() {
            for &r in positions {
                for k in 0..5 {
                    counts.tree[r + 1][k] += counts.contributions[i][k] as isize;
                }
            }
        }
        for at in 1..counts.tree.len() {
            let parent = at + (at & at.wrapping_neg());
            if parent < counts.tree.len() {
                for k in 0..5 {
                    counts.tree[parent][k] += counts.tree[at][k];
                }
            }
        }
        counts
    }
    fn reference(
        rows: std::sync::Arc<model::Rows>,
        s: &State,
        paces: &HashMap<String, Pace>,
    ) -> Self {
        let mut prefix = vec![[0usize; 5]];
        let mut shown = 0;
        let mut live = 0;
        for r in rows.iter() {
            let mut totals = *prefix.last().unwrap();
            for i in (!r.is_earlier())
                .then_some(r.agent)
                .into_iter()
                .chain(r.earlier.iter().copied())
            {
                let a = &s.snap.agents[i];
                let pc = paces.get(&a.id);
                for (total, value) in totals.iter_mut().zip(Self::contribution(a, pc)) {
                    *total += value;
                }
            }
            prefix.push(totals);
            if !r.is_earlier() {
                shown += 1;
                live += usize::from(model::live(&s.snap.agents[r.agent]));
            }
        }
        Self {
            rows,
            positions: Vec::new(),
            contributions: Vec::new(),
            tree: Vec::new(),
            reference: Some(prefix),
            shown,
            live,
        }
    }
    fn update(
        &mut self,
        dirty: &std::collections::HashSet<usize>,
        s: &State,
        paces: &HashMap<String, Pace>,
    ) {
        for &i in dirty {
            let next = Self::contribution(&s.snap.agents[i], paces.get(&s.snap.agents[i].id));
            let delta =
                std::array::from_fn(|k| next[k] as isize - self.contributions[i][k] as isize);
            if delta != [0; 5] {
                for j in 0..self.positions[i].len() {
                    self.add(self.positions[i][j], delta);
                }
                self.contributions[i] = next;
            }
        }
    }
}
impl Prepared {
    pub(crate) fn pace(&self, index: usize, s: &State) -> Option<Pace> {
        let a = &s.snap.agents[index];
        let base = self.paces.get(&a.id)?;
        let last = s
            .render_cache
            .borrow()
            .pace_lasts
            .as_ref()
            .map(|xs| xs[index]);
        model::pace_with_last(a, s.now, base.delegating, last)
    }
    fn new(s: &State, paces: std::sync::Arc<HashMap<String, Pace>>) -> Self {
        let start = std::time::Instant::now();
        let agents = &s.snap.agents;
        let by_id = s.id_index();
        let rows = s.shared_rows();
        let cached_shape = s
            .cache_enabled
            .then(|| s.render_cache.borrow().shape.clone())
            .flatten()
            .filter(|shape| {
                std::sync::Arc::ptr_eq(&shape.rows, &rows) && shape.linked_id == s.linked
            });
        let shape = cached_shape.unwrap_or_else(|| {
            let parents: Vec<_> = agents
                .iter()
                .map(|a| a.parent.as_deref().and_then(|id| by_id.get(id).copied()))
                .collect();
            let mut linked = vec![false; agents.len()];
            if let Some(id) = s.linked.as_deref() {
                for (i, _) in agents.iter().enumerate() {
                    let mut at = Some(i);
                    // Bounded walks also handle malformed cycles without per-row sets.
                    for _ in 0..=agents.len() {
                        let Some(index) = at else { break };
                        if agents[index].id == id {
                            linked[i] = true;
                            break;
                        }
                        at = parents[index];
                    }
                }
            }
            let mut descendants_waiting = vec![false; agents.len()];
            for (i, a) in agents
                .iter()
                .enumerate()
                .filter(|(_, a)| model::wants_attention(a))
            {
                let _ = a;
                let mut at = parents[i];
                for _ in 0..=agents.len() {
                    let Some(index) = at else { break };
                    if descendants_waiting[index] {
                        break;
                    }
                    descendants_waiting[index] = true;
                    at = parents[index];
                }
            }
            let mut row_by_id = HashMap::new();
            for (i, r) in rows.iter().enumerate() {
                row_by_id.entry(r.id.clone()).or_insert(i);
            }
            let shape = PreparedShape {
                rows: rows.clone(),
                linked_id: s.linked.clone(),
                row_by_id: std::sync::Arc::new(row_by_id),
                linked: std::sync::Arc::new(linked),
                descendants_waiting: std::sync::Arc::new(descendants_waiting),
            };
            if s.cache_enabled {
                s.render_cache.borrow_mut().shape = Some(shape.clone());
            }
            shape
        });
        let shape_time = start.elapsed();

        let prefix = if s.cache_enabled {
            let mut cache = s.render_cache.borrow_mut();
            let mut counts = cache
                .counts
                .take()
                .filter(|c| std::sync::Arc::ptr_eq(&c.rows, &rows));
            if let Some(counts) = &mut counts {
                std::sync::Arc::make_mut(counts).update(&cache.count_dirty, s, &paces);
            }
            let counts = counts
                .unwrap_or_else(|| std::sync::Arc::new(RowCounts::new(rows.clone(), s, &paces)));
            cache.count_dirty.clear();
            cache.counts = Some(counts.clone());
            counts
        } else {
            std::sync::Arc::new(RowCounts::reference(rows.clone(), s, &paces))
        };
        let shown = prefix.shown;
        let live = prefix.live;
        if std::env::var_os("T3_FRAME_PROFILE").is_some() {
            eprintln!(
                "prepared_us shape={} prefixes={}",
                shape_time.as_micros(),
                (start.elapsed() - shape_time).as_micros()
            );
        }
        Self {
            rows,
            row_by_id: shape.row_by_id,
            prefix,
            shown,
            live,
            paces,
            by_id,
            linked: shape.linked,
            descendants_waiting: shape.descendants_waiting,
        }
    }
}

/// One bounded entry per derived layer, owned by the snapshot's State. Replacement
/// drops all entries; layers can later be invalidated by changed agent id.
#[derive(Clone, Debug, Default)]
pub(crate) struct RenderCache {
    agents: HashMap<usize, AgentCache>,
    pub(crate) paces: Option<model::PaceStore>,
    pub(crate) tree_index: Option<model::TreeIndex>,
    pub(crate) pace_revision: usize,
    pub(crate) id_index: Option<std::sync::Arc<HashMap<String, usize>>>,
    pub(crate) rows_revision: usize,
    pub(crate) pace_lasts: Option<std::sync::Arc<Vec<f64>>>,
    lane_edges: Option<Vec<f64>>,
    pub(crate) tree_edges: Option<Vec<f64>>,
    pub(crate) events: Option<std::sync::Arc<overview::EventIndex>>,
    pub(crate) rows: Option<(String, std::sync::Arc<model::Rows>)>,
    top: Option<(String, Vec<terminal::Cell>, overview::Overview)>,
    prepared: Option<(String, std::sync::Arc<Prepared>)>,
    shape: Option<PreparedShape>,
    counts: Option<std::sync::Arc<RowCounts>>,
    pub(crate) count_dirty: std::collections::HashSet<usize>,
    lanes: Option<(
        (usize, u64, i32, usize, usize),
        std::sync::Arc<Vec<model::Lane>>,
        usize,
    )>,
}

/// Pointer keys always retain an owning Arc; allocator reuse cannot hit stale data.
#[derive(Clone, Debug)]
struct AgentCache {
    agent: std::sync::Arc<Agent>,
    last: f64,
    edges: Vec<f64>,
    lane: Option<((usize, u64, u64, usize), model::Lane)>,
}
impl RenderCache {
    fn summary(&mut self, a: &std::sync::Arc<Agent>) -> &mut AgentCache {
        self.agents
            .entry(std::sync::Arc::as_ptr(a) as usize)
            .or_insert_with(|| AgentCache {
                agent: a.clone(),
                last: model::pace_last(a),
                edges: {
                    let mut ts: Vec<_> = a.events.iter().map(|e| e.t - 5000.).collect();
                    ts.sort_by(f64::total_cmp);
                    ts
                },
                lane: None,
            })
    }
    pub(crate) fn clear_prepared_paces(&mut self) {
        self.prepared = None;
        self.pace_revision = self.pace_revision.wrapping_add(1);
    }
    pub(crate) fn last(&mut self, a: &std::sync::Arc<Agent>) -> f64 {
        self.summary(a).last
    }
    pub(crate) fn accept(
        &mut self,
        old: &model::Snapshot,
        new: &model::Snapshot,
        changed: &[usize],
        same_order: bool,
        structural: bool,
    ) -> (Vec<std::sync::Arc<Agent>>, Vec<std::sync::Arc<Agent>>) {
        self.prepared = None;
        self.count_dirty.extend(changed.iter().copied());
        if !same_order
            || changed
                .iter()
                .any(|&i| old.agents[i].parent != new.agents[i].parent)
        {
            self.paces = None;
        } else if let Some(paces) = &mut self.paces {
            paces.accept(old, new, changed);
        }
        self.top = None; // Snapshot metadata can change while help freezes now.
                         // Count Arc occurrences: repeated entries may share an allocation.
        let difference = |left: &model::Snapshot, right: &model::Snapshot| {
            let mut kept = HashMap::<usize, usize>::new();
            for a in &right.agents {
                *kept.entry(std::sync::Arc::as_ptr(a) as usize).or_default() += 1;
            }
            left.agents
                .iter()
                .filter(|a| {
                    let n = kept.entry(std::sync::Arc::as_ptr(a) as usize).or_default();
                    if *n > 0 {
                        *n -= 1;
                        false
                    } else {
                        true
                    }
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let (removed, added) = if same_order {
            (
                changed.iter().map(|&i| old.agents[i].clone()).collect(),
                changed.iter().map(|&i| new.agents[i].clone()).collect(),
            )
        } else {
            (difference(old, new), difference(new, old))
        };
        if let Some(events) = &mut self.events {
            std::sync::Arc::make_mut(events).update(&removed, &added);
        }
        for a in &removed {
            self.agents.remove(&(std::sync::Arc::as_ptr(a) as usize));
        }
        if !same_order {
            self.counts = None;
            self.count_dirty.clear();
            self.id_index = None;
            self.tree_index = None;
        } else if let Some(index) = &mut self.tree_index {
            index.update(old, new, changed);
        }
        if structural {
            self.shape = None;
            self.rows = None;
            self.tree_edges = None;
        }
        if structural || !changed.is_empty() {
            self.pace_lasts = None;
            self.prepared = None;
            self.top = None;
            self.lanes = None;
            self.lane_edges = None;
        }
        (removed, added)
    }
}

/// Aggregate borrowed agents directly into bins. Earlier rows can contain large
/// histories: collecting/cloning every Event first only adds allocations.
fn agent_lane<'a>(
    agents: impl IntoIterator<Item = &'a Agent>,
    cols: usize,
    window: f64,
    now: f64,
) -> model::Lane {
    if cols == 0 {
        return model::Lane::default();
    }
    let size = ((window / cols as f64 / 1000.).round() * 1000.).max(1000.);
    let last = (now / size).floor();
    let col = |t: f64| cols as isize - 1 - (last - (t / size).floor()) as isize;
    let mut lane = model::Lane {
        counts: vec![0; cols],
        fails: vec![false; cols],
        you: vec![false; cols],
        busy: vec![false; cols],
    };
    for a in agents {
        for e in &a.events {
            let i = col(e.t).min(cols as isize - 1);
            if i < 0 || e.t > now + 5000. {
                continue;
            }
            let i = i as usize;
            if e.cat == "you" {
                lane.you[i] = true
            } else {
                lane.counts[i] += 1
            }
            lane.fails[i] |= e.failed;
        }
        for (start, end) in &a.spans {
            let i0 = col(*start).max(0);
            let i1 = col(end.unwrap_or(now)).min(cols as isize - 1);
            if i0 <= i1 {
                lane.busy[i0 as usize..=i1 as usize].fill(true)
            }
        }
    }
    lane
}
fn fit(s: &str, w: i32) -> String {
    terminal::fit(s, w.max(0) as usize)
}
fn width(s: &str) -> i32 {
    terminal::width(s) as i32
}
fn scope_name(s: &str) -> &str {
    match s {
        "active" => "active only",
        "t3" => "T3 sidebar",
        _ => "all threads",
    }
}
fn reach(color: u32, target: u32, bg: u32) -> u32 {
    // `theme.ts`'s `reach` toward the page ink at body-text contrast, on the selected row.
    crate::integration::reach(color, target, 4.5, &[bg])
}
fn ratio(a: &Agent) -> Option<f64> {
    model::ratio(a.context.used_tokens, a.context.max_tokens)
}
fn mark(a: &Agent, pc: Option<&Pace>, s: &State, p: &Palette) -> (char, u32) {
    if model::needs_you(a) {
        return (
            if s.calm || s.acked.contains(&a.id) || s.now % 1000. < 550. {
                '◆'
            } else {
                '◇'
            },
            p.hot,
        );
    }
    if pc.is_some_and(|pc| pc.quiet) {
        return ('◌', if pc.unwrap().stalled { p.warn } else { p.text2 });
    }
    match a.status.as_str() {
        "running" | "active" => (
            if s.calm {
                '◉'
            } else {
                terminal::spinner(s.now)
            },
            if pc.is_some_and(|pc| pc.stalled) {
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
fn bar(c: &mut Canvas, x: i32, y: i32, w: i32, frac: f64, p: &Palette, track: u32) {
    terminal::hbar_with(
        c,
        x,
        y,
        w,
        frac,
        &terminal::BarOpts {
            track,
            color: &|i, n| {
                let t = (i as f64 + 0.5) / n as f64;
                if t >= 0.9 {
                    p.hot
                } else if t >= 0.75 {
                    p.warn
                } else {
                    terminal::ramp(p, p.floor + (p.floor.max(0.84) - p.floor) * (t / 0.75))
                }
            },
            marker: None,
            line: false,
        },
    );
}
fn lane(
    c: &mut Canvas,
    x: i32,
    y: i32,
    d: &model::Lane,
    max: usize,
    p: &Palette,
    bg: u32,
    tint: Option<u32>,
) {
    let w = d.counts.len();
    let tick = ((w as f64 / 6.).round() as usize).max(1);
    for i in 0..w {
        let back = if d.busy[i] {
            tint.map(|t| terminal::mix(bg, t, 0.16)).unwrap_or(p.busy)
        } else {
            bg
        };
        let k = (d.counts[i] as f64 / max as f64).min(1.);
        let (ch, color, bold) = if d.fails[i] {
            ('×', p.hot, true)
        } else if d.you[i] {
            ('┃', p.accent, false)
        } else if d.counts[i] > 0 {
            (
                terminal::level(d.counts[i] as f64, max as f64),
                tint.map(|t| terminal::mix(t, p.bright, 0.15 + 0.35 * k))
                    .unwrap_or_else(|| terminal::ramp(p, 0.35 + 0.65 * k)),
                false,
            )
        } else {
            (
                if (w - 1 - i) % tick == 0 { '·' } else { ' ' },
                p.faint,
                false,
            )
        };
        c.set(x + i as i32, y, ch, color, Some(back), bold);
    }
}
fn earlier_span(indices: &[usize], s: &State) -> String {
    let span = indices
        .iter()
        .map(|i| model::last_seen(&s.snap.agents[*i]))
        .filter(|t| *t > 0.)
        .fold(None::<(f64, f64)>, |span, t| {
            Some(match span {
                None => (t, t),
                Some((lo, hi)) => (lo.min(t), hi.max(t)),
            })
        });
    let Some((min, max)) = span else {
        return String::new();
    };
    let coarse = |t: f64| {
        terminal::duration((s.now - t).max(0.) / 1000.)
            .split(' ')
            .next()
            .unwrap_or("")
            .to_owned()
    };
    let a = coarse(max);
    let b = coarse(min);
    if a == b {
        format!("{a} ago")
    } else {
        format!("{a}–{b} ago")
    }
}
fn earlier_tint(indices: &[usize], s: &State, p: &Palette) -> Option<u32> {
    // Every agent folded into the row, on every frame: most share their neighbour's model.
    let mut memo = terminal::FamilyMemo::default();
    let first = indices.first().map(|i| {
        memo.family(
            &s.snap.agents[*i].model,
            s.snap.agents[*i].provider_id.as_deref(),
        )
    })?;
    if first == terminal::ProviderFamily::Other
        || indices.iter().any(|i| {
            memo.family(
                &s.snap.agents[*i].model,
                s.snap.agents[*i].provider_id.as_deref(),
            ) != first
        })
    {
        None
    } else {
        Some(p.provider(&s.snap.agents[indices[0]]))
    }
}
fn doing(a: &Agent, pc: Option<&Pace>, s: &State, w: i32) -> Option<(String, bool)> {
    if model::needs_you(a) && a.open_asks.total() > 0 && !model::shows_blocking_now(a) {
        return None;
    }
    if model::shows_blocking_now(a) && a.now.as_ref().is_none_or(|i| i.kind != "user_input_request") {
        return Some((fit("◆ user input request", w), false));
    }
    if let Some(pc) = pc.filter(|pc| pc.quiet) {
        let last = a
            .items
            .iter()
            .find(|i| model::shows_recent_activity(a, i));
        let lead = format!(
            "◌ {} {}",
            if pc.stalled { "stalled" } else { "quiet" },
            terminal::duration(pc.quiet_ms / 1000.)
        );
        let label = last
            .map(|i| {
                format!(
                    " · last {} {}",
                    glyph(model::category(&i.kind, i.tool.as_deref())),
                    i.display(&s.home)
                )
            })
            .unwrap_or_default();
        return Some((fit(&(lead + &label), w), pc.stalled));
    }
    let i = a.now.as_ref()?;
    let pc = pc.cloned().unwrap_or_default();
    let lead = format!("{} ", glyph(model::category(&i.kind, i.tool.as_deref())));
    let tail = if pc.tool_ms >= 30_000. {
        format!(
            " · {}{}",
            if pc.stalled { "△ " } else { "" },
            terminal::duration(pc.tool_ms / 1000.)
        )
    } else {
        String::new()
    };
    let label = i.display(&s.home);
    let text = if width(&lead) + width(&label) + width(&tail) <= w {
        lead + &label + &tail
    } else {
        let room = w - width(&lead) - width(&tail);
        if room >= 4 {
            lead + &fit(&label, room) + &tail
        } else {
            fit(&(lead + &tail), w)
        }
    };
    let _ = s;
    Some((text, pc.stalled))
}

struct Chip {
    text: String,
    estimated: bool,
    bold: bool,
}

struct Chips {
    parts: Vec<Chip>,
    separator: &'static str,
}

impl Chips {
    fn new(a: &Agent, wide: bool, collapse: bool) -> Self {
        let asks = &a.open_asks;
        let mut parts = Vec::new();
        let mut formal = |text, bold| {
            parts.push(Chip {
                text,
                estimated: false,
                bold,
            })
        };
        if collapse {
            if asks.total() > 0 {
                let mark = if model::needs_you(a) { "◆" } else { "◇" };
                formal(format!("{mark}{}", asks.total()), true);
            }
        } else {
            if asks.blocking > 0 && (asks.total() > 1 || a.estimated_asks > 0) {
                formal(
                    if wide {
                        format!("◆ {} question{}", asks.blocking, plural(asks.blocking))
                    } else {
                        format!("◆{}", asks.blocking)
                    },
                    true,
                );
            }
            let link_mark = if model::running(a) { "◇" } else { "◆" };
            for (n, kind, action, mark) in [
                (asks.cards.secret, "secret", "enter", "◆"),
                (asks.cards.link, "link", "open", link_mark),
            ] {
                if n > 0 {
                    formal(
                        match (wide, n) {
                            (true, 1) => format!("{mark} {kind} to {action}"),
                            (true, _) => format!("{mark} {n} {kind}s to {action}"),
                            (false, 1) => format!("{mark}{kind}"),
                            (false, _) => format!("{mark}{kind}×{n}"),
                        },
                        false,
                    );
                }
            }
            if asks.nonblocking > 0 {
                formal(
                    if wide {
                        format!(
                            "◇ {} question{} open",
                            asks.nonblocking,
                            plural(asks.nonblocking)
                        )
                    } else {
                        format!("◇{}", asks.nonblocking)
                    },
                    false,
                );
            }
        }
        if a.estimated_asks > 0 {
            let n = a.estimated_asks;
            parts.push(Chip {
                text: if wide {
                    format!("~{n} ask{} in reply · estimate", plural(n))
                } else {
                    format!("~{n}")
                },
                estimated: true,
                bold: false,
            });
        }
        Self {
            parts,
            separator: if wide { " · " } else { " " },
        }
    }

    fn width(&self) -> i32 {
        self.parts.iter().map(|part| width(&part.text)).sum::<i32>()
            + self.parts.len().saturating_sub(1) as i32 * width(self.separator)
    }

    fn fit(a: &Agent, wide_room: Option<i32>, narrow_room: i32) -> Self {
        if let Some(room) = wide_room {
            let chips = Self::new(a, true, false);
            if chips.width() <= room {
                return chips;
            }
        }
        let chips = Self::new(a, false, false);
        if chips.width() > narrow_room {
            Self::new(a, false, true)
        } else {
            chips
        }
    }

    fn draw(
        &self,
        c: &mut Canvas,
        mut x: i32,
        y: i32,
        bg: u32,
        ink: impl Fn(u32) -> u32,
        p: &Palette,
    ) {
        for (i, part) in self.parts.iter().enumerate() {
            if i > 0 {
                x += c.text_bg(x, y, self.separator, ink(p.dim), bg, false);
            }
            x += c.text_bg(
                x,
                y,
                &part.text,
                ink(if part.estimated { p.text2 } else { p.hot }),
                bg,
                part.bold,
            );
        }
    }
}

pub(crate) fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn glyph(cat: &str) -> char {
    model::glyph(cat)
}
fn name(
    c: &mut Canvas,
    x: i32,
    y: i32,
    text: &str,
    color: u32,
    bg: u32,
    bold: bool,
    a: &Agent,
    s: &State,
    p: &Palette,
    moving: bool,
) -> i32 {
    if s.calm || !moving {
        return c.text_bg(x, y, text, color, bg, bold);
    }
    let t = s
        .shimmer
        .get(&a.id)
        .map(|at| (s.now - at) / 450.)
        .unwrap_or(1.);
    if !(0.0..1.0).contains(&t) {
        return c.text_bg(x, y, text, color, bg, bold);
    }
    // The original shimmers over `[...clean(s)]`, then draws each code point with `c.text`.
    let chars: Vec<char> = terminal::clean(text).chars().collect();
    let pos = -4. + (1. - (1. - t).powi(3)) * (chars.len() as f64 + 8.);
    let mut col = 0;
    for (i, ch) in chars.into_iter().enumerate() {
        let k = (1. - (i as f64 - pos).abs() / 4.).max(0.);
        let mut buf = [0u8; 4];
        col += c.text_with(
            x + col,
            y,
            ch.encode_utf8(&mut buf),
            terminal::mix(color, p.bright, k),
            Some(bg),
            bold,
            usize::MAX,
        );
    }
    col
}
fn unread(c: &mut Canvas, r: Rect, s: &State, p: &Palette) -> i32 {
    terminal::section_with(
        c,
        r.x + 1,
        r.y,
        r.w - 2,
        "AGENTS",
        p,
        if s.error.is_some() {
            "can't read T3"
        } else {
            "reading T3…"
        },
        if s.error.is_some() { p.hot } else { p.dim },
    );
    let path = if s.snap.path.is_empty() {
        "T3's database"
    } else {
        &s.snap.path
    };
    let kind = s.error_kind.as_deref().unwrap_or("other");
    // `t?.detail` for a known layout, `t?.detail || st.error` for the rest: an empty detail
    // falls back to the error message, as `||` does.
    let detail = s.error_detail.as_deref().unwrap_or("");
    let fallback = || {
        if detail.is_empty() {
            s.error.as_deref().unwrap_or("")
        } else {
            detail
        }
    };
    let head = if s.error.is_none() {
        format!("Reading {path}…")
    } else if kind == "missing" {
        format!("No T3 database at {path}.")
    } else if kind == "schema" {
        format!("T3's database at {path} has a layout this version doesn't know: {detail}.")
    } else {
        format!("Can't read the T3 database at {path}: {}.", fallback())
    };
    let mut tries = Vec::new();
    if s.error.is_some() {
        if kind == "schema" {
            tries.push("T3 Code may have changed its database: look for a newer T3 Agents.");
        } else {
            tries.push(if kind == "unreadable" {
                "Check it is T3 Code's statev2.sqlite and that your user can read it."
            } else {
                "Is T3 Code installed, and has it been opened at least once?"
            });
            tries.push(
                "T3 data somewhere else? Start with --db PATH, or set T3CODE_HOME to its folder.",
            );
        }
        tries.push("Just looking? t3-agents --demo shows made-up agents.");
    }
    let mut y = r.y + 2;
    let w = (r.w - 4).max(1) as usize;
    for line in terminal::wrap(&head, w) {
        if y < r.bottom() {
            c.text_bold(r.x + 2, y, &line, p.fg);
            y += 1;
        }
    }
    y += 1;
    for tip in tries {
        for (i, line) in terminal::wrap(tip, w.saturating_sub(2)).iter().enumerate() {
            if y >= r.bottom() {
                break;
            }
            if i == 0 {
                c.text(r.x + 2, y, "·", p.dim);
            }
            c.text(r.x + 4, y, line, p.text2);
            y += 1;
        }
    }
    y
}
fn draw_list(
    c: &mut Canvas,
    s: &State,
    p: &Palette,
    frame: &mut Surface,
    prepared: &Prepared,
) -> i32 {
    let Rect {
        x: xx,
        y: yy,
        w: ww,
        h: hh,
    } = frame.list;
    let rows = &prepared.rows;
    let visible_agents = prepared.shown;
    let live = prepared.live;
    // No snapshot to show: the whole pane is one message, before any header is drawn.
    if s.snap.at == 0. {
        return unread(c, frame.list, s, p);
    }
    terminal::section(
        c,
        xx + 1,
        yy,
        ww - 2,
        "AGENTS",
        p,
        &format!("{visible_agents} shown · {live} live"),
    );
    let by = yy + 1;
    let bh = hh - 1;
    let card = rows.len() * 2 <= (bh - 1).max(0) as usize;
    let per = if card { 2 } else { 1 };
    let visible = if rows.len() * per <= (bh - 1).max(0) as usize {
        rows.len()
    } else {
        ((bh - 2) / (per as i32)).max(1) as usize
    };
    let index = s
        .selected
        .as_ref()
        .and_then(|id| prepared.row_by_id.get(id))
        .copied()
        .unwrap_or(0);
    let mut first = s.surface.borrow().list_scroll;
    if index < first {
        first = index;
    }
    if index >= first + visible {
        first = index.saturating_sub(visible) + 1;
    }
    first = first.min(rows.len().saturating_sub(visible));
    frame.list_scroll = first;
    let bar_w = if card {
        ((ww as f64 * 0.26).floor() as i32).clamp(10, 30)
    } else {
        ((ww as f64 * 0.13).floor() as i32).clamp(8, 18)
    };
    let name_w = if card {
        0
    } else {
        ((ww as f64 * 0.32).floor() as i32).clamp(14, 40)
    };
    let lane_x = if card {
        xx + 6 + bar_w + 6
    } else {
        xx + 4 + name_w + 2 + bar_w + 6
    };
    let lane_w = (xx + ww - lane_x - if card { 1 } else { 9 }).max(0);
    let window = model::WINDOWS[s.window_index % 4] as f64 * 60_000.;
    let size = ((window / lane_w.max(1) as f64 / 1000.).round() * 1000.).max(1000.);
    let phase = if s.cache_enabled {
        let mut cache = s.render_cache.borrow_mut();
        cache
            .events
            .get_or_insert_with(|| std::sync::Arc::new(overview::EventIndex::new(s)))
            .all
            .before(s.now + 5000., true)
    } else {
        0
    };
    let lane_key = (
        s.render_cache.borrow().rows_revision,
        (s.now / size).floor().to_bits(),
        lane_w,
        s.window_index,
        phase,
    );
    let cached = if s.cache_enabled {
        s.render_cache
            .borrow()
            .lanes
            .as_ref()
            .filter(|(key, _, _)| key == &lane_key)
            .map(|(_, l, m)| (l.clone(), *m))
    } else {
        None
    };
    let (lanes, max) = cached.unwrap_or_else(|| {
        let lanes: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(_n, r)| {
                if lane_w < 8 || r.is_earlier() {
                    return model::Lane::default();
                }
                if r.is_earlier() {
                    agent_lane(
                        r.earlier.iter().map(|i| &*s.snap.agents[*i]),
                        lane_w as usize,
                        window,
                        s.now,
                    )
                } else {
                    if s.cache_enabled {
                        let mut cache = s.render_cache.borrow_mut();
                        let entry = cache.summary(&s.snap.agents[r.agent]);
                        let key = (
                            lane_w as usize,
                            window.to_bits(),
                            (s.now / size).floor().to_bits(),
                            entry.edges.partition_point(|t| *t <= s.now),
                        );
                        if entry.lane.as_ref().is_none_or(|(k, _)| *k != key) {
                            entry.lane = Some((
                                key,
                                agent_lane(
                                    std::iter::once(&*entry.agent),
                                    lane_w as usize,
                                    window,
                                    s.now,
                                ),
                            ));
                        }
                        entry.lane.as_ref().unwrap().1.clone()
                    } else {
                        agent_lane(
                            std::iter::once(&*s.snap.agents[r.agent]),
                            lane_w as usize,
                            window,
                            s.now,
                        )
                    }
                }
            })
            .collect();
        let max = rows
            .iter()
            .zip(&lanes)
            .filter(|(r, _)| !r.is_earlier())
            .flat_map(|(_, l)| &l.counts)
            .copied()
            .max()
            .unwrap_or(0)
            .max(3);
        let lanes = std::sync::Arc::new(lanes);
        if s.cache_enabled {
            s.render_cache.borrow_mut().lanes = Some((lane_key, lanes.clone(), max));
        }
        (lanes, max)
    });
    let earlier_lanes: HashMap<_, _> = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .filter(|(_, r)| r.is_earlier())
        .map(|(n, r)| {
            (
                n,
                agent_lane(
                    r.earlier.iter().map(|i| &*s.snap.agents[*i]),
                    lane_w.max(0) as usize,
                    window,
                    s.now,
                ),
            )
        })
        .collect();
    let row_lane = |n: usize| earlier_lanes.get(&n).unwrap_or(&lanes[n]);
    if lane_w >= 12 {
        let label = |m: f64| {
            if m >= 60. {
                format!("-{}h", terminal::fixed_number(m / 60., 1))
            } else {
                format!("-{}m", terminal::js_number(terminal::js_round(m)))
            }
        };
        let mins = model::WINDOWS[s.window_index % 4] as f64;
        let marks = prepared
            .row_by_id
            .get(s.selected.as_deref().unwrap_or(""))
            .map(|i| &rows[*i])
            .filter(|r| !r.is_earlier())
            .map(|r| run_marks(&s.snap.agents[r.agent], lane_w, window, s.now))
            .unwrap_or_default();
        let mut used = vec![false; lane_w as usize];
        for m in &marks {
            for i in m.at.min(m.from)..=m.at.max(m.from + width(&m.text) - 1) {
                used[i as usize] = true;
            }
        }
        let mut put = |from: i32, text: &str| {
            if ((from - 2).max(0)..=(from + width(text) + 1).min(lane_w - 1))
                .any(|i| used[i as usize])
            {
                return;
            }
            c.text(lane_x + from, by, text, p.dim);
        };
        put(0, &label(mins));
        put(lane_w - 3, "now");
        if lane_w >= 30 {
            put(lane_w / 2 - 2, &label(mins / 2.));
        }
        for m in marks {
            c.set(lane_x + m.at, by, '╷', p.bright, None, false);
            c.text(lane_x + m.from, by, &m.text, p.fg);
        }
    }
    c.text(xx + 2, by, if card { "CONTEXT" } else { "AGENT" }, p.dim);
    if !card {
        c.text(xx + 4 + name_w + 2, by, "CONTEXT", p.dim);
    }
    if rows.is_empty() {
        c.text(
            xx + 2,
            by + 2,
            &fit(
                match s.scope.as_str() {
                    "active" => "No active agents. Press a for T3's sidebar.",
                    "t3" => "Nothing in T3's sidebar: every thread is settled or snoozed. Press a for all.",
                    _ => "No unarchived T3 threads found.",
                },
                ww - 4,
            ),
            p.dim,
        );
        return by + 4;
    }
    // `p.provider(a)` is the family color of `family(model, provider_id)`; a row asks for it up to
    // three times, and neighbouring rows often share a model.
    let mut families = terminal::FamilyMemo::default();
    for (n, r) in rows.iter().enumerate().skip(first).take(visible) {
        let a = &s.snap.agents[r.agent];
        let y = by + 1 + (n - first) as i32 * per as i32;
        let selected = s.selected.as_deref() == Some(&r.id);
        let linked = prepared.linked[r.agent];
        let bg = if selected {
            p.sel
        } else if linked {
            terminal::mix(p.bg, p.sel, 0.5)
        } else {
            p.bg
        };
        if selected || linked {
            c.fill(xx, y, ww, per as i32, bg);
        }
        let ink = |color| {
            if !selected && !linked {
                color
            } else if color == p.dim {
                p.text2
            } else if color == p.text2 {
                p.fg
            } else if color == p.hot || color == p.warn {
                reach(color, if p.dark { 0xffffff } else { 0 }, p.sel)
            } else {
                color
            }
        };
        frame.hits.push(Hit {
            rect: Rect {
                x: xx,
                y,
                w: ww,
                h: per as i32,
            },
            id: r.id.clone(),
            earlier: r.is_earlier(),
        });
        let current_pace = prepared.pace(r.agent, s);
        let pc = current_pace.as_ref();
        let (g, gcolor) = mark(a, pc, s, p);
        let attention = !r.is_earlier() && model::needs_you(a);
        let edge = if selected || attention {
            '▌'
        } else if linked {
            '▎'
        } else {
            ' '
        };
        let ec = if attention {
            p.hot
        } else if selected {
            p.bright
        } else {
            p.dim
        };
        for line in 0..per {
            c.set(xx, y + line as i32, edge, ec, Some(bg), false);
        }
        let mut x = xx + 2;
        for guide in &r.guides {
            c.set(
                x,
                y,
                if *guide { '│' } else { ' ' },
                p.rule,
                Some(bg),
                false,
            );
            if per == 2 {
                c.set(
                    x,
                    y + 1,
                    if *guide { '│' } else { ' ' },
                    p.rule,
                    Some(bg),
                    false,
                );
            }
            x += 2;
        }
        if r.depth > 0 {
            c.text_bg(x, y, if r.last { "└─" } else { "├─" }, p.rule, bg, false);
            if per == 2 && !r.last {
                c.set(x, y + 1, '│', p.rule, Some(bg), false);
            }
            x += 2;
        }
        if r.is_earlier() {
            let right = xx + ww - 1;
            let hint = if selected && per == 2 {
                "⏎ show all"
            } else {
                ""
            };
            if !hint.is_empty() {
                c.right(right, y, hint, ink(p.dim));
            }
            let end = if per == 2 || lane_w < 8 {
                right - if hint.is_empty() { 0 } else { width(hint) + 2 }
            } else {
                lane_x - 2
            };
            let span = earlier_span(&r.earlier, s);
            let failed = r
                .earlier
                .iter()
                .filter(|i| matches!(s.snap.agents[**i].status.as_str(), "failed" | "error"))
                .count();
            let mut parts = vec![(format!("{} earlier ▸", r.earlier.len()), ink(p.text2))];
            if !span.is_empty() {
                parts.push((span, ink(p.dim)));
            }
            if failed > 0 {
                parts.push((format!("{failed} ✗"), ink(p.hot)));
            }
            c.set(x, y, '┄', ink(p.dim), Some(bg), false);
            let mut col = 2;
            for (i, (part, color)) in parts.iter().enumerate() {
                if i > 0 {
                    col += c.text(x + col, y, &fit(" · ", end - x - col), ink(p.dim));
                }
                col += c.text(x + col, y, &fit(part, end - x - col), *color);
            }
            if lane_w >= 8 {
                lane(
                    c,
                    lane_x,
                    y + per as i32 - 1,
                    row_lane(n),
                    max,
                    p,
                    bg,
                    earlier_tint(&r.earlier, s, p),
                );
            }
            continue;
        }
        c.set(x, y, g, gcolor, Some(bg), true);
        let fold = if r.has_children && r.stage == "folded" {
            " ▸"
        } else {
            ""
        };
        let folded = if !fold.is_empty() && prepared.descendants_waiting[r.agent] {
            2
        } else {
            0
        };
        let live = model::live(a) && !attention;
        let moving = matches!(a.status.as_str(), "running" | "active")
            && !attention
            && !pc.is_some_and(|pc| pc.quiet);
        let namecolor = ink(if attention || live { p.fg } else { p.text2 });
        let bold = live || attention || selected;
        let age = run_length(a, s.now, live || attention);
        let age_color = ink(if live || attention { p.fg } else { p.dim });
        let rati = ratio(a);
        let frac =
            s.tween
                .get(&a.id)
                .copied()
                .unwrap_or(if s.calm { rati.unwrap_or(0.) } else { 0. });
        let pct = rati
            .map(|r| format!("{:>4}", format!("{}%", terminal::js_number(terminal::js_round(r * 100.)))))
            .unwrap_or("  --".into());
        let pctcolor = ink(match rati {
            None => p.dim,
            Some(r) if r >= 0.9 => p.hot,
            Some(r) if r >= 0.75 => p.warn,
            _ => p.text2,
        });
        if card {
            let right = xx + ww - 1;
            let meta = format!("● {}  {}", terminal::short_model(&a.model), age);
            let meta_w = width(&meta);
            let namemax = (right - meta_w - 2 - (x + 2)).max(6);
            let chip_room = right - meta_w - 2 - (x + 2) - folded - width(fold);
            let wide_room = (a.open_asks.total() > 0 || a.estimated_asks > 0)
                .then(|| chip_room - width(&a.title) - 2);
            let chips = Chips::fit(a, wide_room, chip_room - 8);
            let chip_w = chips.width();
            let name_room = if chip_w > 0 {
                chip_room - chip_w - 2
            } else {
                namemax - folded
            };
            let title = fit(&a.title, name_room) + fold;
            let used = name(c, x + 2, y, &title, namecolor, bg, bold, a, s, p, moving) + folded;
            if folded > 0 {
                c.set(x + 1 + used, y, '◆', p.hot, Some(bg), true);
            }
            let mut ax = x + 2 + used + 2;
            if chip_w > 0 && ax + chip_w <= right - meta_w - 2 {
                chips.draw(c, ax, y, bg, ink, p);
                ax += chip_w + 2;
            }
            let aw = right - meta_w - 2 - ax;
            if aw > 8 && (live || attention) {
                if let Some((doing, warn)) = doing(a, pc, s, aw) {
                    c.text(
                        ax,
                        y,
                        &doing,
                        ink(if warn {
                            p.warn
                        } else if pc.is_some_and(|pc| pc.quiet) {
                            p.text2
                        } else {
                            p.dim
                        }),
                    );
                }
            }
            c.right(right, y, &meta, ink(p.dim));
            c.right(right, y, &age, age_color);
            c.set(right - meta_w, y, '●', p.family_color(families.family(&a.model, a.provider_id.as_deref())), Some(bg), false);
            let shift = (r.depth as i32 - 2).max(0) * 2;
            let bx = xx + 6 + shift;
            let bw = (bar_w - shift).max(4);
            if r.has_children && r.stage != "folded" && x < bx {
                c.set(x, y + 1, '│', p.rule, Some(bg), false);
            }
            if rati.is_none() {
                c.text(
                    bx,
                    y + 1,
                    &fit(
                        if a.remote.is_some() {
                            "remote · stats on other machine"
                        } else {
                            "context not reported"
                        },
                        bw + 5,
                    ),
                    ink(p.dim),
                );
            } else {
                bar(
                    c,
                    bx,
                    y + 1,
                    bw,
                    frac,
                    p,
                    if selected {
                        terminal::mix(p.track, p.sel, 0.4)
                    } else {
                        p.track
                    },
                );
                c.text_bg(bx + bw + 1, y + 1, &pct, pctcolor, bg, true);
            }
        } else {
            let end = xx + 4 + name_w;
            let room = end - (x + 2) - folded;
            let chips = Chips::fit(a, None, room - width(fold) - 5);
            let chip_w = chips.width();
            let show_chips = chip_w > 0 && chip_w < room - width(fold);
            let title_room = if show_chips {
                room - width(fold) - chip_w - 1
            } else {
                room.max(4)
            };
            let title = fit(&a.title, title_room) + fold;
            let used = name(c, x + 2, y, &title, namecolor, bg, bold, a, s, p, moving);
            if folded > 0 {
                c.set(x + 3 + used, y, '◆', p.hot, Some(bg), true);
            }
            if show_chips {
                chips.draw(c, end - chip_w, y, bg, ink, p);
            }
            let bx = xx + 4 + name_w + 2;
            if rati.is_none() {
                c.text(bx, y, "--", ink(p.dim));
            } else {
                bar(
                    c,
                    bx,
                    y,
                    bar_w,
                    frac,
                    p,
                    if selected {
                        terminal::mix(p.track, p.sel, 0.4)
                    } else {
                        p.track
                    },
                );
                c.text_bg(bx + bar_w + 1, y, &pct, pctcolor, bg, true);
            }
            c.right(xx + ww - 1, y, &age, age_color);
            c.set(
                xx + ww - 3 - width(&age),
                y,
                '●',
                p.family_color(families.family(&a.model, a.provider_id.as_deref())),
                Some(bg),
                false,
            );
        }
        if lane_w >= 8 {
            let tint = if families.family(&a.model, a.provider_id.as_deref())
                == terminal::ProviderFamily::Other
            {
                None
            } else {
                Some(p.family_color(families.family(&a.model, a.provider_id.as_deref())))
            };
            lane(c, lane_x, y + per as i32 - 1, row_lane(n), max, p, bg, tint);
        }
    }
    let shown = visible.min(rows.len() - first);
    let below = rows.len() - first - shown;
    let y = by + 1 + shown as i32 * per as i32;
    if first > 0 || below > 0 {
        let total = prepared.prefix.sum(rows.len());
        let end = prepared.prefix.sum(first + shown);
        let start = prepared.prefix.sum(first);
        let hidden = |k| total[k] - (end[k] - start[k]);
        let waiting = hidden(0);
        let stalled = hidden(1);
        let slow = hidden(2);
        let failed = hidden(3);
        let open = hidden(4);
        let attention = waiting > 0 || open > 0;
        let lead = if first > 0 && below > 0 {
            format!("▲ {first} · ▼ {below} more")
        } else if below > 0 {
            format!("▼ {below} more")
        } else {
            format!("▲ {first} more")
        };
        let mut parts = vec![(lead, if attention { p.hot } else { p.text2 })];
        if waiting > 0 {
            parts.push((format!("◆{waiting}"), p.hot));
        }
        if open > 0 {
            parts.push((format!("◇{open}"), p.hot));
        }
        if stalled > 0 {
            parts.push((format!("{stalled} stalled"), p.warn));
        }
        if slow > 0 {
            parts.push((format!("△{slow} slow"), p.warn));
        }
        if failed > 0 {
            parts.push((format!("{failed} ✗"), p.hot));
        }
        let mut x = xx + 2;
        for (i, (text, color)) in parts.iter().enumerate() {
            if i > 0 {
                x += c.text(x, y, &fit(" · ", xx + ww - 1 - x), p.dim);
            }
            x += c.text_with(
                x,
                y,
                &fit(text, xx + ww - 1 - x),
                *color,
                None,
                i == 0 && attention,
                usize::MAX,
            );
        }
        return y + 1;
    }
    y
}
/// Decimal size, one decimal under 10. `short` is the warning line's tight form (`1.2G`).
fn format_bytes(bytes: u64, short: bool) -> String {
    let (unit, long, brief): (u64, &str, &str) = if bytes >= 1_000_000_000 {
        (1_000_000_000, " GB", "G")
    } else if bytes >= 1_000_000 {
        (1_000_000, " MB", "M")
    } else {
        (1_000, " KB", "K")
    };
    let tenths = (u128::from(bytes) * 10 + u128::from(unit) / 2) / u128::from(unit);
    let suffix = if short { brief } else { long };
    if tenths == 0 {
        format!("0{suffix}")
    } else if tenths >= 100 {
        format!("{}{suffix}", tenths / 10)
    } else {
        format!("{}.{}{suffix}", tenths / 10, tenths % 10)
    }
}

#[derive(Clone, Copy)]
enum WarnInk {
    Bold,
    Plain,
    Dim,
}

/// Warning line. Memory-backed volumes first.
/// The most detailed plan that leaves room for the `s storage` hint is drawn.
/// `s` itself belongs to the Storage view, which is not bound here.
fn warning_plan(s: &State, columns: i32) -> (Vec<(String, WarnInk)>, bool) {
    let mut lows: Vec<&crate::headroom::WarningVolume> =
        s.headroom.iter().filter(|volume| volume.low()).collect();
    lows.sort_by_key(|volume| volume.volume.is_tmpfs != Some(true));
    let says = |volume: &crate::headroom::WarningVolume, level: u8| -> Vec<(String, WarnInk)> {
        let memory = volume.volume.is_tmpfs == Some(true);
        let short = level >= 3;
        let free = format_bytes(volume.volume.available_bytes, short);
        let total = format_bytes(volume.volume.total_bytes, short);
        let memory_tail = if !memory {
            ""
        } else if level >= 4 {
            ", in RAM"
        } else {
            ", in memory"
        };
        let name = if volume.worktrees {
            if level == 0 && !volume.worktree_mount.is_empty() {
                format!("worktrees on {}", volume.worktree_mount)
            } else {
                "worktrees".to_string()
            }
        } else if volume.temp_mount.is_empty() {
            "temp".to_string()
        } else {
            volume.temp_mount.clone()
        };
        let detail = if level <= 1 {
            format!(" {free} free of {total}{memory_tail}")
        } else {
            format!(" {free} free{memory_tail}")
        };
        vec![(name, WarnInk::Bold), (detail, WarnInk::Plain)]
    };
    let lead_long = vec![("△ Low space · ".to_string(), WarnInk::Bold)];
    let lead = vec![("△ ".to_string(), WarnInk::Bold)];
    let plans: [(Vec<(String, WarnInk)>, [u8; 2]); 9] = [
        (lead_long.clone(), [0, 0]),
        (lead.clone(), [0, 0]),
        (lead_long.clone(), [1, 1]),
        (lead.clone(), [1, 1]),
        (lead.clone(), [1, 2]),
        (lead.clone(), [2, 2]),
        (lead.clone(), [2, 3]),
        (lead.clone(), [3, 3]),
        (lead, [4, 4]),
    ];
    let mut built = Vec::new();
    for (head, level) in plans {
        let mut parts = head;
        for (i, volume) in lows.iter().enumerate() {
            if i > 0 {
                parts.push((" · ".to_string(), WarnInk::Dim));
            }
            parts.extend(says(volume, level[i.min(1)]));
        }
        built.push(parts);
    }
    // `s` + ` storage` is 9 columns, as compose.py measures it.
    const HINT: i32 = 9;
    let room = columns - 2;
    let mut chosen = built.len() - 1;
    let mut with_hint = false;
    for (i, parts) in built.iter().enumerate() {
        let n: i32 = parts.iter().map(|(text, _)| width(text)).sum();
        if n + 2 + HINT <= room {
            chosen = i;
            with_hint = true;
            break;
        }
    }
    (built.swap_remove(chosen), with_hint)
}

fn draw_warning(c: &mut Canvas, s: &State, p: &Palette) {
    let w = c.w as i32;
    let y = c.h as i32 - 2;
    c.fill(0, y, w, 1, p.bg);
    let (parts, with_hint) = warning_plan(s, w);
    let limit = if with_hint { w - 1 - 9 } else { w - 1 };
    let mut x = 1;
    for (text, ink) in parts {
        if x >= limit {
            break;
        }
        let max = (limit - x) as usize;
        let (color, bold) = match ink {
            WarnInk::Bold => (p.warn, true),
            WarnInk::Plain => (p.warn, false),
            WarnInk::Dim => (p.dim, false),
        };
        x += c.text_with(x, y, &text, color, None, bold, max);
    }
    if with_hint {
        let mut x = w - 1 - 9;
        x += c.text_bold(x, y, "s", p.accent);
        c.text(x, y, " storage", p.dim);
    }
}

fn footer(c: &mut Canvas, s: &State, p: &Palette) {
    let w = c.w as i32;
    let y = c.h as i32 - 1;
    c.fill(0, y, w, 1, p.panel);
    if let Some(error) = &s.error {
        c.text(
            1,
            y,
            &fit(
                &format!(
                    "SOURCE ERROR · {}retrying every {}s · {error}",
                    if s.snap.at > 0. {
                        "last snapshot kept · "
                    } else {
                        ""
                    },
                    s.interval
                ),
                w - 2,
            ),
            p.hot,
        );
        return;
    }
    if let Some(notice) = s.notice.as_ref().filter(|_| s.now - s.notice_at < 4000.) {
        c.text(
            1,
            y,
            &fit(notice, w - 2),
            if s.notice_error { p.hot } else { p.text2 },
        );
        return;
    }
    let mut keys = Vec::new();
    if s.themes {
        keys.extend([("?", "help"), ("w", "trace")]);
        if s.storage_key_in_help {
            keys.push(("s", "storage"));
        }
    }
    if s.snap.agents.iter().any(|a| model::wants_attention(a)) {
        keys.push(("n", "needs you"));
    }
    keys.extend([
        ("↑↓", "select"),
        ("⏎", "open in T3"),
        ("␣", "fold"),
        ("a", scope_name(&s.scope)),
    ]);
    if s.t3_link {
        keys.push(("f", if s.follow { "follow T3" } else { "follow off" }));
    }
    keys.extend([("t", "lane"), ("x", "fx")]);
    if !s.themes {
        keys.push(("?", "help"));
    }
    keys.push(("q", "quit"));
    let mut x = 1;
    for (key, label) in keys {
        if x + width(key) + width(label) + 3 > w - 24 {
            break;
        }
        x += c.text_bold(x, y, key, p.accent) + 1;
        x += c.text(x, y, label, p.dim) + 2;
    }
    let status = if s.demo {
        "demo data · made up".into()
    } else {
        format!(
            "read-only · {} ago",
            terminal::duration(((s.now - s.snap.at) / 1000.).max(0.))
        )
    };
    c.right(w - 1, y, &status, p.dim);
}
pub fn draw_grid_reference(s: &State, w: usize, h: usize, p: &Palette) -> (Canvas, Surface) {
    let mut reference = s.clone();
    reference.cache_enabled = false;
    draw_grid(&reference, w, h, p)
}
pub fn draw_grid(s: &State, w: usize, h: usize, p: &Palette) -> (Canvas, Surface) {
    let phase_start = std::time::Instant::now();
    let profiling = std::env::var_os("T3_FRAME_PROFILE").is_some();
    let mut c = Canvas::cleared(w, h, p.bg, p.fg);
    let mut frame = Surface::default();
    let ww = w as i32;
    let hh = h as i32;
    if w < 48 || h < 14 {
        c.text_bold(1, 0, "T3 Agents", p.bright);
        let notice = match (s.themes, w) {
            (true, ..17) => "Enlarge terminal",
            (true, 17..42) => "Minimum: 48 × 14",
            _ => "Enlarge the terminal to at least 48 × 14.",
        };
        c.text(1, 2, notice, p.dim);
        c.text(1, 3, "q quit", p.accent);
        return (c, frame);
    }
    let _rows = s.shared_rows();
    if s.cache_enabled && s.render_cache.borrow().pace_lasts.is_none() {
        let mut cache = s.render_cache.borrow_mut();
        let lasts = s.snap.agents.iter().map(|a| cache.last(a)).collect();
        cache.pace_lasts = Some(std::sync::Arc::new(lasts));
    }
    let pace_start = std::time::Instant::now();
    let paces = s.shared_paces();
    if profiling {
        eprintln!("pace_refresh_us {}", pace_start.elapsed().as_micros());
    }
    let prepared_key = format!(
        "{:?}",
        (
            s.generation,
            s.render_cache.borrow().rows_revision,
            s.render_cache.borrow().pace_revision,
            &s.linked
        )
    );
    let cached = if s.cache_enabled {
        s.render_cache
            .borrow()
            .prepared
            .as_ref()
            .filter(|(key, _)| key == &prepared_key)
            .map(|(_, p)| p.clone())
    } else {
        None
    };
    let prepared = cached.unwrap_or_else(|| {
        let p = std::sync::Arc::new(Prepared::new(s, paces));
        if s.cache_enabled {
            let mut cache = s.render_cache.borrow_mut();
            cache.prepared = Some((prepared_key, p.clone()));
        }
        p
    });
    let prepared_time = phase_start.elapsed();
    frame.selected_agent = s
        .selected
        .as_ref()
        .and_then(|id| prepared.row_by_id.get(id))
        .map(|i| prepared.rows[*i].agent);
    let top_key = format!(
        "{:?}",
        (
            (
                s.generation,
                s.now.to_bits(),
                w,
                h,
                p,
                s.calm,
                s.paused,
                &s.error
            ),
            (
                s.interval,
                s.demo,
                &s.linked,
                s.follow,
                &s.scope,
                s.window_index,
                &s.usage,
                &s.acked
            )
        )
    );
    let cached_top = if s.cache_enabled {
        s.render_cache
            .borrow()
            .top
            .as_ref()
            .filter(|(k, _, _)| k == &top_key)
            .map(|(_, cells, top)| (cells.clone(), *top))
    } else {
        None
    };
    let top = if let Some((cells, top)) = cached_top {
        c.cells[..cells.len()].copy_from_slice(&cells);
        top
    } else {
        let top = overview::draw_header_kpis_limits(&mut c, s, p, &prepared);
        if s.cache_enabled {
            s.render_cache.borrow_mut().top = Some((
                top_key,
                c.cells[..(top.main_top.max(0) as usize * w).min(c.cells.len())].to_vec(),
                top,
            ));
        }
        top
    };
    let overview_time = phase_start.elapsed();
    frame.needs_you = top.needs_you;
    // One row above the footer, only when a watched volume is low. Healthy
    // headroom leaves `main_h` alone, so every other cell stays where it was.
    let show_warn = s.headroom.iter().any(|volume| volume.low()) && hh - 2 > top.main_top;
    let main_bottom = hh - 1 - i32::from(show_warn);
    let main_h = main_bottom - top.main_top;
    if w >= 140 {
        let lw = (ww as f64 * 0.56).floor() as i32;
        frame.list = Rect {
            x: 0,
            y: top.main_top,
            w: lw,
            h: main_h,
        };
        frame.detail = Rect {
            x: lw + 2,
            y: top.main_top,
            w: ww - lw - 3,
            h: main_h,
        };
        for y in top.main_top..main_bottom {
            c.set(lw, y, '│', p.rule, None, false);
        }
    } else {
        let rows = &prepared.rows;
        let room = (main_h - (main_h as f64 * 0.3).floor().max(7.) as i32).max(5);
        let cards = rows.len() as i32 * 2 + 2;
        let lh = if s.snap.at == 0. {
            main_h
        } else {
            if cards <= room {
                cards
            } else {
                room.min(rows.len() as i32 + 2)
            }
            .max(5)
        };
        frame.list = Rect {
            x: 0,
            y: top.main_top,
            w: ww,
            h: lh,
        };
        frame.detail = Rect {
            x: 1,
            y: top.main_top + lh,
            w: ww - 2,
            h: main_h - lh,
        };
    }
    let end = draw_list(&mut c, s, p, &mut frame, &prepared);
    let list_time = phase_start.elapsed();
    if w >= 140 && s.snap.at > 0. && top.main_top + main_h - end >= 8 {
        overview::draw_extras(
            &mut c,
            Rect {
                x: 1,
                y: end + 1,
                w: frame.list.w - 2,
                h: top.main_top + main_h - end - 1,
            },
            s,
            p,
        );
    }
    if frame.detail.h > 3 {
        let height = details::draw_details(&mut c, frame.detail, s, p, &prepared, s.detail_scroll);
        frame.detail_max = height.saturating_sub(frame.detail.h.max(0) as usize);
        let effective_scroll = s.detail_scroll.min(frame.detail_max);
        if effective_scroll != s.detail_scroll {
            c.fill(
                frame.detail.x,
                frame.detail.y,
                frame.detail.w,
                frame.detail.h,
                p.bg,
            );
            details::draw_details(&mut c, frame.detail, s, p, &prepared, effective_scroll);
        }
        let below = frame.detail_max.saturating_sub(effective_scroll);
        if below > 0 {
            c.text_bg(
                frame.detail.right() - width(&format!(" ▼ {below} more")),
                frame.detail.bottom() - 1,
                &format!(" ▼ {below} more"),
                p.text2,
                p.bg,
                false,
            );
        }
        if effective_scroll > 0 {
            c.text_bg(
                frame.detail.right() - width(&format!(" ▲ {} more", effective_scroll)),
                frame.detail.y,
                &format!(" ▲ {} more", effective_scroll),
                p.text2,
                p.bg,
                false,
            );
        }
    }
    if show_warn {
        draw_warning(&mut c, s, p);
    }
    footer(&mut c, s, p);
    if s.help {
        c.tint(0, 0, ww, hh, |fg, bg| {
            (terminal::mix(fg, p.bg, 0.7), terminal::mix(bg, p.bg, 0.5))
        });
        frame.help_max = details::draw_help(&mut c, s, p);
    }
    if !s.calm {
        let boot = ((s.now - s.boot_at) / 650.).max(0.);
        if boot < 1. {
            let edge = ((1. - (1. - boot).powi(3)) * hh as f64).floor() as i32;
            c.tint(0, edge + 1, ww, hh, |_, _| (p.bg, p.bg));
            for x in 0..ww {
                c.set(
                    x,
                    edge.min(hh - 1),
                    '▔',
                    terminal::mix(p.accent, p.bright, 0.5),
                    Some(c.bg_at(x, edge)),
                    false,
                );
            }
        }
    }
    if s.fx && !s.calm {
        let cx = ww as f64 / 2.;
        let cy = hh as f64 / 2.;
        let max = (cx * cx + cy * cy).max(1.);
        let bar_h = hh as f64 * 0.12;
        let total = bar_h * (1. + 0.4 * 2.);
        let pos = (s.fx_elapsed / 1000. * 0.35) % (hh as f64 + bar_h * 2.);
        let center = pos - total / 2. + bar_h / 2.;
        let scale = |color: u32, f: f64| -> u32 {
            let ch = |shift: u32| {
                (((((color >> shift) & 255u32) as f64 / 255. * f).clamp(0., 1.) * 255.)
                    .round()
                    .clamp(0., 255.) as u32)
                    << shift
            };
            ch(16) | ch(8) | ch(0)
        };
        for y in 0..hh {
            let dist = (y as f64 - center).abs();
            let bf = if dist <= total / 2. {
                (dist / (total / 2.) * std::f64::consts::FRAC_PI_2).cos()
            } else {
                0.
            };
            // OpenTUI interns non-ASCII cells (including wide continuations) as
            // graphemes and applies one vignette factor across each adjacent run.
            let mut sample_x: Vec<usize> = (0..w).collect();
            let mut run = 0;
            while run < w {
                let grapheme = |i: usize| {
                    let ch = c.cells[y as usize * w + i].ch;
                    ch as u32 >= 128 || ch == terminal::CONT
                };
                if !grapheme(run) {
                    run += 1;
                    continue;
                }
                let start = run;
                while run < w && grapheme(run) {
                    run += 1;
                }
                sample_x[start..run].fill(start + (run - start) / 2);
            }
            for x in 0..ww {
                let cell = &mut c.cells[y as usize * w + x as usize];
                let attenuation =
                    (((sample_x[x as usize] as f64 - cx).powi(2) + (y as f64 - cy).powi(2)) / max)
                        .min(1.)
                        * 0.45;
                // OpenTUI's scanline filter targets BG only; its native color matrix
                // and Float32 mask round channels between stages.
                let matrix_scale = |color: u32, f: f32| -> u32 {
                    let channel = |shift| {
                        ((((((color >> shift) & 255u32) as f32 / 255.) * f) * 255.)
                            .round()
                            .clamp(0., 255.) as u32)
                            << shift
                    };
                    channel(16) | channel(8) | channel(0)
                };
                if y % 2 == 0 {
                    cell.bg = matrix_scale(cell.bg, 0.78f32);
                }
                for color in [&mut cell.fg, &mut cell.bg] {
                    *color = matrix_scale(*color, 1. - attenuation as f32);
                    if bf > 0.001 {
                        *color = scale(*color, 1. + 0.18 * bf);
                    }
                }
            }
        }
    }
    if profiling {
        eprintln!(
            "frame_us prepare={} overview={} list={} rest={}",
            prepared_time.as_micros(),
            (overview_time - prepared_time).as_micros(),
            (list_time - overview_time).as_micros(),
            (phase_start.elapsed() - list_time).as_micros()
        );
    }
    (c, frame)
}

#[cfg(test)]
mod prepared_tests {
    use super::*;

    #[test]
    fn product_footer_names_help_and_the_two_views_at_phone_width() {
        let mut state = State::new(model::Snapshot::default());
        let palette = Palette::fallback();
        for width in [55, 60, 80, 120] {
            let mut canvas = Canvas::new(width, 24);
            state.themes = false;
            state.storage_key_in_help = false;
            footer(&mut canvas, &state, &palette);
            let original = canvas.row_text(23);
            assert!(!original.contains("w trace") && !original.contains("s storage"));
            state.themes = true;
            state.storage_key_in_help = true;
            state.demo = true;
            footer(&mut canvas, &state, &palette);
            let row = canvas.row_text(23);
            for label in ["? help", "w trace", "s storage", "demo data · made up"] {
                assert!(row.contains(label), "{width}: {row}");
            }
        }
    }

    #[test]
    fn short_dashboard_notice_preserves_the_reference_outside_product_mode() {
        let mut state = State::new(model::Snapshot::default());
        let palette = Palette::fallback();
        // oracle/src/view.ts writes the full sentence and lets the canvas clip it.
        let (original, _) = draw_grid(&state, 30, 14, &palette);
        assert!(original.row_text(2).trim_start().starts_with("Enlarge the terminal to"));
        state.themes = true;
        let (product, _) = draw_grid(&state, 30, 14, &palette);
        assert!(product.row_text(2).contains("48 × 14"));
    }

    #[test]
    fn borrowed_aggregate_lane_matches_original_concatenation_at_boundaries() {
        let events = [
            model::Event {
                t: -1.,
                cat: "shell".into(),
                failed: false,
            },
            model::Event {
                t: 10_000.,
                cat: "you".into(),
                failed: false,
            },
            model::Event {
                t: 11_000.,
                cat: "shell".into(),
                failed: true,
            },
            model::Event {
                t: 17_000.,
                cat: "edit".into(),
                failed: false,
            },
            model::Event {
                t: 17_001.,
                cat: "web".into(),
                failed: true,
            },
        ];
        let agents = [
            Agent {
                events: events[..2].to_vec(),
                spans: vec![(0., Some(9_000.)), (11_000., None)],
                ..Agent::default()
            },
            Agent {
                events: events[2..].to_vec(),
                spans: vec![(14_000., Some(20_000.)), (20_000., Some(1_000.))],
                ..Agent::default()
            },
        ];
        let joined: Vec<_> = agents
            .iter()
            .flat_map(|a| a.events.iter().cloned())
            .collect();
        let spans: Vec<_> = agents
            .iter()
            .flat_map(|a| a.spans.iter().copied())
            .collect();
        for cols in [0, 1, 4, 17, 91] {
            for window in [1_000., 4_000., 900_000.] {
                let expected = model::lane(&joined, &spans, cols, window, 12_000.);
                let actual = agent_lane(&agents, cols, window, 12_000.);
                assert_eq!(actual.counts, expected.counts);
                assert_eq!(actual.fails, expected.fails);
                assert_eq!(actual.you, expected.you);
                assert_eq!(actual.busy, expected.busy);
            }
        }
    }
}

pub fn run_length(a: &Agent, now: f64, counting: bool) -> String {
    match (
        model::ms(a.started.as_deref()),
        if counting {
            Some(now)
        } else {
            model::ms(a.ended.as_deref())
        },
    ) {
        (Some(start), Some(end)) => terminal::span(Some((end - start) / 1000.)),
        _ => String::new(),
    }
}
#[derive(Debug, PartialEq)]
pub struct Mark {
    pub at: i32,
    pub text: String,
    pub from: i32,
}
pub fn place_marks(ticks: Vec<(i32, String, i32)>, w: i32) -> Vec<Mark> {
    let mut used = vec![false; w.max(0) as usize];
    let mut marks = Vec::new();
    for (at, text, side) in ticks {
        if at < 0 || at >= w || used[at as usize] {
            continue;
        }
        let len = width(&text);
        for side in [side, -side] {
            let from = if side < 0 { at - len } else { at + 1 };
            let to = from + len - 1;
            if from < 0 || to >= w {
                continue;
            }
            if ((from - 1).max(0)..=(to + 1).min(w - 1)).any(|i| used[i as usize]) {
                continue;
            }
            for i in from.min(at)..=to.max(at) {
                used[i as usize] = true;
            }
            marks.push(Mark {
                at,
                text: text.clone(),
                from,
            });
            break;
        }
    }
    marks
}
pub fn run_marks(a: &Agent, w: i32, window: f64, now: f64) -> Vec<Mark> {
    let Some(start) = model::ms(a.started.as_deref()).filter(|_| w > 0) else {
        return vec![];
    };
    let size = ((window / w as f64 / 1000.).round() * 1000.).max(1000.);
    let col = |t: f64| w - 1 - ((now / size).floor() - (t / size).floor()) as i32;
    let s = (col(start), terminal::clock(start, now), -1);
    let end = if model::live(a) || model::needs_you(a) {
        None
    } else {
        model::ms(a.ended.as_deref())
    };
    let Some(end) = end else {
        return place_marks(vec![s], w);
    };
    let e = (col(end).min(w - 1), terminal::clock(end, start), 1);
    if s.0 == e.0 {
        let text = if s.1 == e.1 {
            s.1
        } else {
            format!("{}–{}", s.1, e.1)
        };
        return place_marks(vec![(s.0, text, -1)], w);
    }
    if s.1 == e.1 {
        place_marks(vec![s], w)
    } else {
        place_marks(vec![s, e], w)
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    #[test]
    fn run_lengths_hold_finished_runs_and_count_live_runs() {
        let now = model::ms(Some("2026-11-12T14:30:00Z")).unwrap();
        let a = Agent {
            started: Some("2026-11-12T14:10:00Z".into()),
            ended: Some("2026-11-12T14:19:32Z".into()),
            ..Agent::default()
        };
        assert_eq!(run_length(&a, now, false), "9m32s");
        assert_eq!(run_length(&a, now + 3600000., false), "9m32s");
        assert_eq!(run_length(&a, now, true), "20m");
        let mut a = a;
        a.ended = None;
        assert_eq!(run_length(&a, now, false), "");
        a.started = None;
        assert_eq!(run_length(&a, now, true), "");
    }
    #[test]
    fn marks_flip_at_edges_and_drop_when_crowded() {
        let marks = place_marks(vec![(2, "14:02".into(), -1), (37, "14:12".into(), 1)], 40);
        assert_eq!(
            marks.iter().map(|m| (m.at, m.from)).collect::<Vec<_>>(),
            [(2, 3), (37, 32)]
        );
        assert!(
            place_marks(vec![(-3, "14:02".into(), -1), (41, "14:12".into(), 1)], 40).is_empty()
        );
        assert_eq!(
            place_marks(vec![(2, "14:02".into(), -1), (6, "14:12".into(), 1)], 10).len(),
            1
        );
        let now = model::ms(Some("2026-11-12T14:30:00Z")).unwrap();
        let mut a = Agent {
            status: "completed".into(),
            started: Some("2026-11-12T14:10:00Z".into()),
            ended: Some("2026-11-12T14:20:00Z".into()),
            ..Agent::default()
        };
        assert_eq!(
            run_marks(&a, 60, 1800000., now)
                .iter()
                .map(|m| m.at)
                .collect::<Vec<_>>(),
            [19, 39]
        );
        a.status = "running".into();
        assert_eq!(run_marks(&a, 60, 1800000., now).len(), 1);
        a.started = Some("2026-11-12T13:45:00Z".into());
        assert!(run_marks(&a, 60, 1800000., now).is_empty());
        a.status = "completed".into();
        assert_eq!(
            run_marks(&a, 60, 1800000., now)
                .iter()
                .map(|m| m.at)
                .collect::<Vec<_>>(),
            [39]
        );
    }
}
