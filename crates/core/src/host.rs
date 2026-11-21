//! Presenter-independent host loop.
//!
//! Data refresh, usage and theme refresh, focus resolution, and the adaptive wake
//! (30 fps while something tweens, 10 fps while something moves, 2 fps idle, and no
//! wake at all for a fixed snapshot in calm mode). A presenter supplies the clock;
//! the Rune window's `Tx` and the terminal front door both implement [`Clock`].
use crate::{clock, demo, integration, live, model, model::State, overview, source, terminal, theme};
use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

/// What the host loop needs from a presenter. `instant` snaps the presenter's own
/// motion; `wake_after` schedules the next poll; `clock` is seconds since the
/// presenter started, the same value passed to [`poll_host`] as `time`.
pub trait Clock {
    fn instant(&mut self);
    fn wake_after(&mut self, after: Duration);
    fn clock(&self) -> f64;
}

pub struct Host {
    pub db: Option<PathBuf>,
    pub usage: Option<PathBuf>,
    /// The original's theme: one `colors.toml`, re-read every second.
    pub theme: Option<PathBuf>,
    /// The product's themes ([`theme`]). When set, `theme` is unused.
    pub themes: Option<theme::Themes>,
    pub fixed: bool,
    pub epoch: f64,
    pub last_poll: f64,
    pub last_config: f64,
    pub last_usage: f64,
    pub last_time: f64,
    pub focus: Option<integration::FocusWatcher>,
    pub titles: Option<mpsc::Receiver<Option<String>>>,
    // Keep the title itself: the SQLite projection may learn about its thread
    // after the window event arrived, or update the disambiguating metadata.
    pub t3_title: Option<String>,
    /// T3's database read on a background thread, once a presenter starts it.
    pub live: Option<live::Handle>,
    /// Measure worktree and temp devices on each dashboard refresh.
    /// Off for `--demo`, a user-supplied `--db`, and tests. Those set
    /// [`model::State::headroom`] directly, or [`Self::headroom_inject`].
    pub watch_headroom: bool,
    /// T3's managed worktrees folder in real mode.
    pub managed_worktrees: Option<PathBuf>,
    /// The effective T3 temp folder in real mode, captured at launch.
    pub temporary: Option<PathBuf>,
    /// When set, a due refresh copies this instead of probing the filesystem.
    pub headroom_inject: Option<Vec<crate::headroom::WarningVolume>>,
    last_headroom: f64,
    initial_reader: Option<live::Reader>,
}

impl Host {
    pub fn new(
        db: Option<PathBuf>,
        usage: Option<PathBuf>,
        theme: Option<PathBuf>,
        fixed: bool,
        epoch: f64,
    ) -> Self {
        Self {
            db,
            usage,
            theme,
            themes: None,
            fixed,
            epoch,
            last_poll: -1e9,
            last_config: -1e9,
            last_usage: -1e9,
            last_time: -1.,
            focus: None,
            titles: None,
            t3_title: None,
            live: None,
            watch_headroom: false,
            managed_worktrees: None,
            temporary: None,
            headroom_inject: None,
            last_headroom: -1e9,
            initial_reader: None,
        }
    }
    /// Attach an already-initialized live reader so `start_live` avoids a redundant initial read.
    pub fn with_live_reader(mut self, reader: live::Reader) -> Self {
        self.initial_reader = Some(reader);
        self
    }
    /// Take the pre-warmed live reader if one was attached during launch.
    pub fn take_live_reader(&mut self) -> Option<live::Reader> {
        self.initial_reader.take()
    }
    /// Omarchy's usage files and current theme, under the user's state directory.
    pub fn usage_dir() -> Option<PathBuf> {
        crate::platform::home_dir()
            .map(|h| PathBuf::from(h).join(".local/state/omarchy/agents/usage"))
    }
    pub fn theme_file() -> Option<PathBuf> {
        crate::platform::home_dir()
            .map(|h| PathBuf::from(h).join(".local/state/omarchy/current/theme/colors.toml"))
    }
    /// The product's live mode: T3's database, the user's usage files and theme.
    pub fn live(db: PathBuf, epoch: f64) -> Self {
        Self::new(Some(db), Self::usage_dir(), Self::theme_file(), false, epoch)
    }
    /// The product's demo: made-up agents and usage, reading nothing but the theme.
    pub fn demo(epoch: f64) -> Self {
        Self::new(None, None, Self::theme_file(), false, epoch)
    }
    /// The experiment flags' rules, as the Rune window has always applied them.
    pub fn from_args(s: &State) -> Self {
        let db = crate::cli::option("--db").map(PathBuf::from);
        let usage = crate::cli::option("--usage-dir")
            .map(PathBuf::from)
            .or_else(|| db.as_ref().and(Self::usage_dir()));
        let theme = crate::cli::option("--palette")
            .map(PathBuf::from)
            .or_else(|| db.as_ref().and(Self::theme_file()));
        let fixed = crate::cli::option("--snapshot").is_some()
            || std::env::args().any(|a| a == "--fixed-time");
        let epoch = if !fixed && db.is_none() {
            clock::wall_now()
        } else {
            s.snap.at
        };
        Self::new(db, usage, theme, fixed, epoch)
    }
}

/// From now on, read T3's database on a background thread. The interface thread only
/// takes finished snapshots, so a slow read never delays a frame, and a poll in which
/// nothing changed costs it nothing. `wake` is called from that thread when a snapshot
/// is ready; it must only notify. `T3_AGENTS_SYNC_READ` keeps the reads on the interface
/// thread, for comparison.
pub fn start_live(h: &mut Host, interval: f64, wake: impl Fn() + Send + 'static) {
    if std::env::var_os("T3_AGENTS_SYNC_READ").is_some() {
        h.initial_reader = None;
        return;
    }
    if let Some(path) = &h.db {
        let interval = Duration::from_secs_f64(interval.max(0.5));
        h.live = Some(match h.initial_reader.take() {
            Some(reader) => live::spawn_with_reader(path, reader, interval, wake),
            None => live::spawn(path, interval, wake),
        });
    }
}

/// Reread free space on the dashboard interval, including when the database snapshot
/// did not change. Drawing and Storage reuse [`model::State::headroom`].
fn refresh_headroom(s: &mut State, h: &mut Host) -> bool {
    if !h.watch_headroom || s.paused {
        return false;
    }
    let interval = s.interval.max(0.5);
    if h.last_headroom >= 0. && h.last_time - h.last_headroom < interval {
        return false;
    }
    h.last_headroom = h.last_time;
    let next = if let Some(injected) = &h.headroom_inject {
        injected.clone()
    } else if let (Some(root), Some(temporary)) = (&h.managed_worktrees, &h.temporary) {
        crate::headroom::sample(root, local_worktrees(&s.snap), temporary)
    } else {
        return false;
    };
    if next == s.headroom {
        return false;
    }
    s.headroom = next;
    true
}

fn local_worktrees(snapshot: &model::Snapshot) -> impl Iterator<Item = &Path> {
    snapshot.agents.iter().filter_map(|agent| {
        if agent.remote.is_some() {
            return None;
        }
        agent.storage.as_ref()?.worktree.as_deref().map(Path::new)
    })
}

/// The same agents, pointer for pointer. The state never edits the agents it is handed.
fn same_agents(a: &[std::sync::Arc<model::Agent>], b: &[std::sync::Arc<model::Agent>]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| std::sync::Arc::ptr_eq(a, b))
}

/// Takes what the background reader finished, if anything. Returns whether the state changed.
fn accept(s: &mut State, result: Result<live::Update, source::SourceError>) -> bool {
    match result {
        // Nothing changed since the reader's last poll. That is only nothing new for the
        // state if it already shows that snapshot: an update that arrived while paused may
        // have been replaced in the mailbox by a later skipped poll carrying the same data.
        Ok(update)
            if update.served == live::Served::Skipped
                && s.error.is_none()
                && same_agents(&s.snap.agents, &update.snapshot.agents) =>
        {
            false
        }
        Ok(update) => {
            s.apply(update.snapshot, &update.changed);
            true
        }
        Err(e) => {
            s.error = Some(e.to_string());
            s.error_kind = Some(e.kind.into());
            s.error_detail = Some(e.detail);
            true
        }
    }
}

pub fn data_refresh(s: &mut State, h: &Host) {
    if let Some(path) = &h.db {
        match source::snapshot(path, clock::wall_now()) {
            Ok(snapshot) => s.replace(snapshot),
            Err(e) => {
                s.error = Some(e.to_string());
                s.error_kind = Some(e.kind.into());
                s.error_detail = Some(e.detail);
            }
        }
    } else if !h.fixed {
        match demo::snapshot(s.now, h.epoch) {
            Ok(snapshot) => s.replace(snapshot),
            Err(e) => {
                s.error = Some(e.to_string());
                s.error_kind = Some(e.kind.into());
                s.error_detail = Some(e.detail);
            }
        }
    }
}

/// Event time for a key or a click. A fixed calm snapshot keeps the frozen display
/// clock; everything else follows the host epoch plus the presenter's clock.
pub fn input_now(s: &State, h: &Host) -> f64 {
    if h.db.is_some() {
        clock::wall_now()
    } else if h.fixed && s.calm {
        s.now
    } else {
        h.epoch + h.last_time.max(0.) * 1000.
    }
}

/// Enter or `o` on a row: open its thread in T3 Code, or say why not. A remote row opens
/// on its own machine; a native agent opens its owning thread here.
pub fn open_selected(s: &mut State, h: &Host) {
    let Some(agent) = s
        .snap
        .agents
        .iter()
        .find(|a| Some(&a.id) == s.selected.as_ref())
        .cloned()
    else {
        return;
    };
    let title = s.thread_to_open().unwrap().title.clone();
    open_agent(s, h, &agent, &title);
}

/// Open an inventory row without changing or acknowledging the dashboard selection.
pub fn open_local_thread(s: &mut State, h: &Host, thread_id: &str) {
    if let Some(agent) = s
        .snap
        .agents
        .iter()
        .find(|a| a.thread_id == thread_id && a.storage.is_some() && a.remote.is_none())
        .cloned()
    {
        open_agent(s, h, &agent, &agent.title);
    }
}

fn open_agent(s: &mut State, h: &Host, agent: &model::Agent, title: &str) {
    let at = input_now(s, h);
    if s.demo {
        s.note(format!("Demo: would open {title} in T3 Code"), false, at);
        return;
    }
    let local = h.db.as_deref().and_then(integration::environment_id);
    match integration::agent_thread_link(agent, local.as_deref()) {
        Some(link) => match integration::open_link(&link) {
            Ok(_) => s.note(format!("Opening in T3 · {title}"), false, at),
            Err(e) => s.note(format!("Cannot open in T3: {e}"), true, at),
        },
        None => s.note(
            "Cannot open in T3: no environment id beside the T3 database",
            true,
            at,
        ),
    }
}

/// Apply one key, and if it turns motion back on, resume the display clock in this
/// same input and ask for an immediate poll. `resume_at` is evaluated only when
/// motion actually resumes, after the key has been applied.
pub fn after_key(
    s: &mut State,
    clock: &mut impl Clock,
    key: &str,
    now: f64,
    resume_at: impl FnOnce(&State) -> f64,
) {
    let (was_calm, was_help, was_fx) = (s.calm, s.help, s.fx);
    s.action_at(key, now);
    let reduced = s.surface.borrow().system_reduced;
    let resumed = !s.calm && !reduced && ((!s.help && (was_calm || was_help)) || (!was_fx && s.fx));
    if resumed {
        // A fixed calm snapshot freezes display time, not the presenter's input
        // clock. Using the old display time here would restart the boot wipe.
        let at = resume_at(s);
        if key == "m" && was_calm {
            // This action created a fresh notice while the fixed display clock
            // was frozen. It must start at the resumed event time as well.
            s.notice_at = at;
        }
        s.advance_frame(at, reduced);
        clock.wake_after(Duration::ZERO);
    }
}

/// `c` and `C`: the next or previous theme, saved, with a notice saying which. Nothing without
/// the product's themes.
pub fn cycle_theme(s: &mut State, h: &mut Host, back: bool, at: f64) -> bool {
    let Some(themes) = h.themes.as_mut() else {
        return false;
    };
    let notice = themes.cycle(back);
    s.palette = themes.palette();
    s.note(&notice, false, at);
    true
}

/// The terminal's background color, as it answered `OSC 11`. A dark/light pair follows it.
/// Returns whether the palette changed.
pub fn terminal_background(s: &mut State, h: &mut Host, rgb: u32) -> bool {
    let Some(themes) = h.themes.as_mut() else {
        return false;
    };
    themes.set_terminal_dark(!theme::light_background(rgb));
    let palette = themes.palette();
    let changed = palette != s.palette;
    s.palette = palette;
    changed
}

/// Match the retained window title against the current snapshot on every poll.
/// Following reveals its ancestors, preserves an existing family selection, and
/// viewing the linked thread acknowledges requests anywhere in its family.
pub fn apply_focus(s: &mut State, title: Option<&str>) -> bool {
    use std::collections::{HashMap, HashSet};
    let linked = title.and_then(|t| integration::thread_for_title(t, &s.snap.agents));
    let mut changed = linked != s.linked;
    // With no linked thread the family is empty and nothing below reads the maps: the follow
    // branch needs a linked id, and no agent is in an empty family. Skip building them; this
    // runs on every wake, usually with no T3 window focused.
    let Some(linked_id) = linked.as_deref() else {
        if changed {
            s.linked = None;
        }
        return changed;
    };
    let parents: HashMap<_, _> = s
        .snap
        .agents
        .iter()
        .map(|a| (a.id.clone(), a.parent.clone()))
        .collect();
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for a in &s.snap.agents {
        if let Some(parent) = &a.parent {
            children.entry(parent).or_default().push(&a.id);
        }
    }
    let mut family = HashSet::new();
    let mut stack: Vec<&str> = vec![linked_id];
    while let Some(id) = stack.pop() {
        if !family.insert(id) {
            continue;
        }
        stack.extend(children.get(id).into_iter().flatten().copied());
    }
    if changed {
        s.linked = linked.clone();
        if s.follow {
            if let Some(id) = &linked {
                let mut at = Some(id.clone());
                let mut visited = HashSet::new();
                while let Some(id) = at {
                    if !visited.insert(id.clone()) {
                        break;
                    }
                    s.collapsed.remove(&id);
                    at = parents.get(&id).cloned().flatten();
                }
                if s.selected.as_deref().is_none_or(|id| !family.contains(id)) {
                    s.selected = Some(id.clone());
                    s.detail_scroll = 0;
                }
            }
        }
    }
    for a in &s.snap.agents {
        if family.contains(a.id.as_str()) && model::needs_you(a) {
            changed |= s.acked.insert(a.id.clone());
        }
    }
    changed
}

pub fn load_replay(
    path: &std::path::Path,
) -> Result<Vec<crate::model::Snapshot>, Box<dyn std::error::Error>> {
    let snaps: Vec<crate::model::Snapshot> = serde_json::from_slice(&std::fs::read(path)?)?;
    if snaps.is_empty() {
        return Err("empty replay".into());
    }
    Ok(snaps)
}

pub fn poll_host(s: &mut State, time: f64, clock: &mut impl Clock, h: &mut Host) -> bool {
    h.last_time = time;
    let previous = s.now;
    let mut changed = false;
    let at = if h.db.is_some() {
        crate::clock::wall_now()
    } else if !h.fixed || !s.calm {
        h.epoch + time * 1000.
    } else {
        s.now
    };
    if !s.help {
        s.now = at;
    }
    if let Some(reader) = &h.live {
        // While paused the newest snapshot stays in the mailbox and is shown on resume.
        if !s.paused {
            if let Some(result) = reader.take() {
                changed |= accept(s, result);
            }
        }
    } else if (h.db.is_some() || !h.fixed)
        && time - h.last_poll
            >= if h.db.is_some() {
                s.interval
            } else {
                demo::REFRESH_MS / 1000.
            }
    {
        h.last_poll = time;
        if !s.paused {
            data_refresh(s, h);
            changed = true;
        }
    }
    // Free space moves without a database write. This still runs when the live reader
    // had nothing new to apply.
    changed |= refresh_headroom(s, h);
    if time - h.last_usage >= 5. {
        h.last_usage = time;
        if let Some(path) = &h.usage {
            s.usage = integration::usage(path, s.now);
            changed = true;
        } else if s.demo && !h.fixed {
            s.usage = overview::demo_usage_at(s.now, h.epoch);
        }
    }
    if time - h.last_config >= 1. {
        h.last_config = time;
        // A theme file edited while the dashboard runs shows within a second, as an Omarchy theme
        // switch always has.
        if let Some(themes) = &h.themes {
            let palette = themes.palette();
            if palette != s.palette {
                s.palette = palette;
                changed = true;
            }
        } else if let Some(path) = &h.theme {
            let palette =
                integration::palette(path).unwrap_or_else(|_| terminal::Palette::fallback());
            if palette != s.palette {
                s.palette = palette;
                changed = true;
            }
        }
    }
    if let Some(title) = h
        .titles
        .as_ref()
        .and_then(|receiver| receiver.try_iter().last())
    {
        h.t3_title = title;
    }
    changed |= apply_focus(s, h.t3_title.as_deref());
    let reduced = s.surface.borrow().system_reduced;
    let calm = s.calm || reduced;
    let (smooth, beat) = s.advance_frame(at, reduced);
    let effect_active = s.fx && !calm;
    let moving = !calm && !s.help && (s.fx || beat);
    let wait = if smooth && !calm {
        1. / 30.
    } else if moving {
        0.1
    } else {
        0.5
    };
    // Fixed snapshots in calm mode can stay entirely asleep after initial data/animation setup.
    if !h.fixed || moving || smooth || effect_active || h.db.is_some() {
        clock.wake_after(Duration::from_secs_f64(wait));
    }
    changed |= s.now != previous || smooth || moving || effect_active;
    if !calm {
        clock.instant();
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Snapshot;

    struct Fake {
        t: f64,
        wakes: Vec<Duration>,
        instants: u32,
    }
    impl Clock for Fake {
        fn instant(&mut self) {
            self.instants += 1;
        }
        fn wake_after(&mut self, after: Duration) {
            self.wakes.push(after);
        }
        fn clock(&self) -> f64 {
            self.t
        }
    }

    fn fixed(s: &State) -> Host {
        Host {
            db: None,
            usage: None,
            theme: None,
            themes: None,
            fixed: true,
            epoch: s.snap.at,
            last_poll: -1e9,
            last_config: -1e9,
            last_usage: -1e9,
            last_time: -1.,
            focus: None,
            titles: None,
            t3_title: None,
            live: None,
            watch_headroom: false,
            managed_worktrees: None,
            temporary: None,
            headroom_inject: None,
            last_headroom: -1e9,
            initial_reader: None,
        }
    }

    fn demo() -> State {
        let snap: Snapshot =
            serde_json::from_str(include_str!("../../../checks/fixtures/demo.json")).unwrap();
        State::new(snap)
    }

    #[test]
    fn headroom_uses_explicit_local_checkouts_only() {
        use std::sync::Arc;

        let displayed_cwd = "/synthetic/project";
        let mut local = model::Agent {
            cwd: displayed_cwd.into(),
            storage: Some(model::ThreadStorage {
                project_id: "project".into(),
                project_root: Some(displayed_cwd.into()),
                worktree: None,
            }),
            ..model::Agent::default()
        };
        let without_checkout = local.clone();
        local.storage.as_mut().unwrap().worktree = Some("/synthetic/checkout".into());
        let mut remote = local.clone();
        remote.remote = Some(serde_json::json!({"environmentId": "remote"}));
        remote.storage.as_mut().unwrap().worktree = Some("/synthetic/remote".into());
        let snapshot = model::Snapshot {
            agents: vec![Arc::new(without_checkout), Arc::new(local), Arc::new(remote)],
            ..model::Snapshot::default()
        };
        assert_eq!(
            local_worktrees(&snapshot).collect::<Vec<_>>(),
            [Path::new("/synthetic/checkout")]
        );
    }

    #[test]
    fn fixed_calm_snapshot_does_not_schedule_a_wake() {
        let mut s = demo();
        assert!(s.calm);
        let mut h = fixed(&s);
        let mut clock = Fake {
            t: 4.,
            wakes: vec![],
            instants: 0,
        };
        poll_host(&mut s, 4., &mut clock, &mut h);
        assert!(clock.wakes.is_empty());
        assert_eq!(clock.instants, 0);
        assert_eq!(s.now, s.snap.at);
        poll_host(&mut s, 14., &mut clock, &mut h);
        assert!(clock.wakes.is_empty(), "a calm fixed snapshot stays asleep");
        assert_eq!(s.now, s.snap.at);
    }

    #[test]
    fn leaving_calm_and_enabling_crt_ask_for_a_wake() {
        let mut s = demo();
        let mut h = fixed(&s);
        let mut clock = Fake {
            t: 0.,
            wakes: vec![],
            instants: 0,
        };
        // Calm frames snap tweens to their targets, the same way the window settles.
        poll_host(&mut s, 0., &mut clock, &mut h);
        poll_host(&mut s, 0., &mut clock, &mut h);
        for a in &mut s.snap.agents {
            let a = std::sync::Arc::make_mut(a);
            a.status = "completed".into();
            a.requests.clear();
        }
        s.calm = false;
        clock.wakes.clear();
        clock.t = 4.;
        poll_host(&mut s, 4., &mut clock, &mut h);
        assert!(
            clock.wakes.is_empty(),
            "a settled snapshot with effects off does not poll"
        );
        clock.t = 4.;
        let now = s.now;
        let resumed = h.epoch + 4. * 1000.;
        after_key(&mut s, &mut clock, "x", now, |_| resumed);
        assert!(s.fx);
        assert!(
            clock.wakes.iter().any(|d| *d == Duration::ZERO),
            "turning CRT on re-arms the host"
        );
    }

    #[test]
    fn closing_help_resumes_motion_in_the_input_frame() {
        let mut s = demo();
        let mut h = fixed(&s);
        let mut clock = Fake {
            t: 4.,
            wakes: vec![],
            instants: 0,
        };
        poll_host(&mut s, 4., &mut clock, &mut h);
        let epoch = h.epoch;
        let now = s.now;
        after_key(&mut s, &mut clock, "m", now, |_| epoch + 4. * 1000.);
        assert!(!s.calm);
        let now = s.now;
        after_key(&mut s, &mut clock, "h", now, |_| epoch + 4. * 1000.);
        assert!(s.help);
        let frozen = s.now;
        clock.wakes.clear();
        clock.t = 5.;
        after_key(&mut s, &mut clock, "escape", frozen, |_| epoch + 5. * 1000.);
        assert!(!s.help);
        assert!(s.now > frozen, "display time resumes at the input's time");
        assert!(
            clock.wakes.iter().any(|d| *d == Duration::ZERO),
            "motion must not wait for another input"
        );
    }

    fn agent(id: &str, parent: Option<&str>) -> model::Agent {
        model::Agent {
            id: id.into(),
            thread_id: id.into(),
            title: id.into(),
            project: "project".into(),
            parent: parent.map(Into::into),
            status: "completed".into(),
            ..model::Agent::default()
        }
    }

    fn state(agents: Vec<model::Agent>) -> State {
        State::new(Snapshot {
            agents: agents.into_iter().map(Into::into).collect(),
            at: 1_794_484_800_000.,
            path: "synthetic:focus".into(),
        })
    }

    #[test]
    fn retained_title_resolves_when_projection_catches_up() {
        let mut s = state(vec![]);
        assert!(!apply_focus(&mut s, Some("project / new")));
        s.replace(Snapshot {
            agents: vec![agent("new", None).into()],
            ..s.snap.clone()
        });
        assert!(apply_focus(&mut s, Some("project / new")));
        assert_eq!(s.linked.as_deref(), Some("new"));
        assert_eq!(s.selected.as_deref(), Some("new"));
        // The same title is re-resolved when a formerly matching thread leaves.
        s.replace(Snapshot {
            agents: vec![],
            ..s.snap.clone()
        });
        assert!(apply_focus(&mut s, Some("project / new")));
        assert!(s.linked.is_none());
    }

    #[test]
    fn follow_unfolds_ancestors_preserves_child_selection_and_acknowledges_family() {
        let mut agents = vec![
            agent("root", None),
            agent("thread", Some("root")),
            agent("native-child", Some("thread")),
            agent("other", None),
        ];
        agents[2].thread_id = "thread".into();
        agents[2].requests.push("approval".into());
        agents[3].requests.push("approval".into());
        let mut s = state(agents);
        s.selected = Some("native-child".into());
        s.collapsed
            .extend(["root".into(), "thread".into(), "other".into()]);
        assert!(apply_focus(&mut s, Some("project / thread")));
        assert_eq!(s.selected.as_deref(), Some("native-child"));
        assert!(!s.collapsed.contains("root"));
        assert!(!s.collapsed.contains("thread"));
        assert!(s.collapsed.contains("other"));
        assert!(s.acked.contains("native-child"));
        assert!(!s.acked.contains("other"));
        assert!(s.rows().iter().any(|r| r.id == "native-child"));
    }

    #[test]
    fn follow_off_preserves_folds_but_still_acknowledges_new_linked_requests() {
        let mut s = state(vec![
            agent("thread", None),
            agent("child", Some("thread")),
            agent("other", None),
        ]);
        s.follow = false;
        s.selected = Some("other".into());
        s.collapsed.insert("thread".into());
        apply_focus(&mut s, Some("project / thread"));
        assert_eq!(s.selected.as_deref(), Some("other"));
        assert!(s.collapsed.contains("thread"));
        s.snap.agent_mut(1).requests.push("approval".into());
        assert!(apply_focus(&mut s, Some("project / thread")));
        assert!(s.acked.contains("child"));
        assert!(!apply_focus(&mut s, Some("project / thread")));
    }

    #[test]
    fn losing_the_linked_window_only_clears_the_link() {
        let mut s = state(vec![agent("thread", None), agent("child", Some("thread")), agent("other", None)]);
        s.snap.agent_mut(1).requests.push("approval".into());
        s.snap.agent_mut(2).requests.push("approval".into());
        s.collapsed.insert("other".into());
        assert!(!apply_focus(&mut s, None));
        assert!(s.acked.is_empty());
        assert!(apply_focus(&mut s, Some("project / thread")));
        assert!(s.linked.is_some());
        let (selected, collapsed, acked) = (s.selected.clone(), s.collapsed.clone(), s.acked.clone());
        // No title, or one that names no thread: the link goes, and nothing else changes.
        assert!(apply_focus(&mut s, None));
        assert_eq!(s.linked, None);
        assert!(!apply_focus(&mut s, Some("some other window")));
        assert!(!apply_focus(&mut s, None));
        assert_eq!((s.selected.clone(), s.collapsed.clone(), s.acked.clone()), (selected, collapsed, acked));
        assert!(!s.acked.contains("other"));
    }
}
