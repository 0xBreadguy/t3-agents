//! Terminal key routing and borrowed report presentation for one Insights visit.

use std::{io, path::Path, sync::Arc};

use t3_agents_core::{
    clock,
    insights_view::{self as view, Body, InsightsView, Motion, Tab},
    model::State,
    platform::local_time,
    product::{self, Database, Origin},
    recap_view::Source,
    terminal::Canvas,
};
use t3_agents_term::{
    Encoder,
    insights_data::{InsightsData, ReadError, Report, Selection, Snapshot, Value},
    platform::{self, Wake},
};

#[derive(Default)]
pub(super) struct InsightsScreen {
    data: InsightsData,
    database: Option<Database>,
    view: Option<InsightsView>,
    help: bool,
    staged: bool,
    revision: u64,
    kept_revision: u64,
    notice: Option<(String, Vec<String>)>,
}

/// Product provenance is cheap to retain on Live. Its path stays in Host.
#[derive(Clone, Copy)]
pub(super) enum InsightsSource {
    Demo,
    Records(Origin),
}

impl From<&Database> for InsightsSource {
    fn from(database: &Database) -> Self {
        match database {
            Database::Demo => Self::Demo,
            Database::Path { origin, .. } => Self::Records(*origin),
        }
    }
}

pub(super) enum Effect {
    Stay,
    Paint,
    Back,
    Quit,
}

impl InsightsScreen {
    pub(super) fn is_open(&self) -> bool {
        self.view.is_some()
    }

    pub(super) fn open(&mut self) {
        self.data.open(clock::wall_now());
        self.view = Some(InsightsView::new());
        self.staged = true;
    }

    pub(super) fn close(&mut self) {
        self.data.close();
        self.database = None;
        self.view = None;
        self.help = false;
        self.staged = false;
        self.notice = None;
    }

    pub(super) fn key(&mut self, name: &str, source: InsightsSource) -> Effect {
        let Some(selected) = self.data.selection() else {
            return Effect::Stay;
        };
        match name {
            "q" => return Effect::Quit,
            "i" => return Effect::Back,
            "escape" if !self.help => return Effect::Back,
            "?" | "escape" => {
                self.help = !self.help;
                return Effect::Paint;
            }
            _ if self.help => return Effect::Stay,
            _ => {}
        }
        if let Some(next) = next_selection(selected, name) {
            if next == selected || !self.data.select(next, clock::wall_now()) {
                return Effect::Stay;
            }
            if next.period != selected.period {
                self.view.as_mut().unwrap().scroll(Motion::Start);
            }
            self.staged = self.data.loading_since().is_some();
            self.completed(source);
        } else if name == "r" {
            if self
                .data
                .outcome()
                .is_some_and(|snapshot| snapshot.outcome.is_ok())
            {
                self.kept_revision = self.revision;
            }
            self.data.refresh(clock::wall_now());
            self.staged = true;
            self.notice = None;
        } else if let Some(motion) = motion(name) {
            self.view.as_mut().unwrap().scroll(motion);
        } else {
            return Effect::Stay;
        }
        Effect::Paint
    }

    pub(super) fn poll(&mut self, source: InsightsSource) -> bool {
        if self.data.loading_since().is_none() || !self.data.poll() {
            return false;
        }
        self.completed(source);
        true
    }

    /// Called after writing the newly staged frame to the terminal. A start
    /// failure has no worker to wake the presenter, so ask for a paint now.
    fn dispatch(&mut self, source: InsightsSource, path: Option<&Path>, wake: &Arc<Wake>) -> bool {
        if !std::mem::take(&mut self.staged) {
            return false;
        }
        let database = self.database.get_or_insert_with(|| match source {
            InsightsSource::Demo => Database::Demo,
            InsightsSource::Records(origin) => Database::Path {
                path: path.expect("the product database is in Host").to_path_buf(),
                origin,
            },
        });
        let wake = Arc::clone(wake);
        if self.data.dispatch(database, move || wake.waker().wake())
            && self.data.loading_since().is_none()
        {
            self.completed(source);
            return true;
        }
        false
    }

    fn completed(&mut self, source: InsightsSource) {
        self.revision += 1;
        self.notice = self.data.outcome().and_then(|snapshot| {
            snapshot
                .outcome
                .as_ref()
                .err()
                .map(|error| notice(error, source))
        });
    }

    fn frame(
        &mut self,
        state: &State,
        source: InsightsSource,
        (cols, rows): (usize, usize),
    ) -> &Canvas {
        let selection = self.data.selection().unwrap();
        let started = self.data.loading_since();
        let outcome = self.data.outcome();
        let visible = self.data.visible();
        let report = visible.and_then(borrow_report);
        let body = if started.is_some() {
            Body::Loading
        } else if let Some((message, tips)) = &self.notice {
            Body::Unreadable { message, tips }
        } else {
            Body::Report(report.expect("a finished read has a report or an error"))
        };
        let previous = if matches!(body, Body::Report(_)) {
            None
        } else {
            visible.zip(report).map(|(snapshot, report)| view::Kept {
                report,
                read_at: snapshot.finished_at,
                revision: self.kept_revision,
            })
        };

        self.view.as_mut().unwrap().paint(&view::Input {
            state,
            palette: &state.palette,
            local: &local_time,
            tab: match selection.report {
                Report::Recap => Tab::Recap,
                Report::Tools => Tab::Tools,
            },
            requested: selection.period,
            source: match source {
                InsightsSource::Demo => Source::Demo,
                InsightsSource::Records(_) => Source::Records,
            },
            body,
            previous,
            started,
            read_at: outcome.map(|snapshot| snapshot.finished_at),
            help: self.help,
            explicit_db: matches!(source, InsightsSource::Records(Origin::Db)),
            revision: self.revision,
            cols,
            rows,
        })
    }

    pub(super) fn present_key(
        &mut self,
        state: &State,
        source: InsightsSource,
        path: Option<&Path>,
        size: (usize, usize),
        encoder: &mut Encoder,
        full: &mut bool,
        wake: &Arc<Wake>,
    ) -> io::Result<()> {
        self.present_key_with(state, source, path, size, encoder, full, wake, &mut write_frame)
    }

    /// [`InsightsScreen::present_key`] with each frame handed to `sink` (the stream check hashes
    /// it; the dashboard writes it).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn present_key_with(
        &mut self,
        state: &State,
        source: InsightsSource,
        path: Option<&Path>,
        size: (usize, usize),
        encoder: &mut Encoder,
        full: &mut bool,
        wake: &Arc<Wake>,
        sink: &mut FrameSink<'_>,
    ) -> io::Result<()> {
        // write_stdout writes directly to the tty before dispatch, even on demo.
        self.paint_with(state, source, size, encoder, full, sink)?;
        if self.dispatch(source, path, wake) {
            self.paint_with(state, source, size, encoder, full, sink)?;
        }
        Ok(())
    }

    pub(super) fn paint(
        &mut self,
        state: &State,
        source: InsightsSource,
        size: (usize, usize),
        encoder: &mut Encoder,
        full: &mut bool,
    ) -> io::Result<()> {
        self.paint_with(state, source, size, encoder, full, &mut write_frame)
    }

    /// [`InsightsScreen::paint`] with the canvas, its bytes and whether it was a full repaint
    /// handed to `sink`.
    pub(super) fn paint_with(
        &mut self,
        state: &State,
        source: InsightsSource,
        size: (usize, usize),
        encoder: &mut Encoder,
        full: &mut bool,
        sink: &mut FrameSink<'_>,
    ) -> io::Result<()> {
        let was_full = *full;
        let canvas = self.frame(state, source, size);
        let bytes = encoder.encode(canvas, was_full);
        *full = false;
        sink(canvas, &bytes, was_full)
    }

    /// Whether a read is running (the stream check waits for it instead of for a wake).
    #[cfg(feature = "frame-stream")]
    pub(super) fn loading(&self) -> bool {
        self.data.loading_since().is_some()
    }
}

pub(super) type FrameSink<'a> = dyn FnMut(&Canvas, &[u8], bool) -> io::Result<()> + 'a;

fn write_frame(_: &Canvas, bytes: &[u8], _: bool) -> io::Result<()> {
    if !bytes.is_empty() {
        platform::write_stdout(bytes)?;
    }
    Ok(())
}

fn borrow_report(snapshot: &Snapshot) -> Option<view::Report<'_>> {
    // The typed value contains the returned period and exact dates, including fallback.
    Some(match &snapshot.outcome.as_ref().ok()?.value {
        Value::Recap(value) => view::Report::Recap(value),
        Value::Tools(value) => view::Report::Tools(value),
    })
}

fn next_selection(mut selection: Selection, key: &str) -> Option<Selection> {
    // The requested period is the cursor, even when the report fell forward.
    match key {
        "tab" | "shift-tab" => {
            selection.report = match selection.report {
                Report::Recap => Report::Tools,
                Report::Tools => Report::Recap,
            };
        }
        "left" => selection.period = view::step_period(selection.period, -1),
        "right" => selection.period = view::step_period(selection.period, 1),
        _ => return None,
    }
    Some(selection)
}

fn motion(key: &str) -> Option<Motion> {
    Some(match key {
        "up" | "k" => Motion::Rows(-1),
        "down" | "j" => Motion::Rows(1),
        "pageup" => Motion::Page(-1),
        "pagedown" => Motion::Page(1),
        "home" => Motion::Start,
        "end" => Motion::End,
        _ => return None,
    })
}

fn notice(error: &ReadError, source: InsightsSource) -> (String, Vec<String>) {
    let (ReadError::Source(error), InsightsSource::Records(origin)) = (error, source) else {
        return (error.to_string(), Vec::new());
    };
    let advice = product::advice(error, origin);
    let mut lines = advice.lines();
    let first = lines
        .next()
        .unwrap_or_default()
        .trim_start_matches("t3-agents: ");
    let mut message: String = first
        .chars()
        .take(1)
        .flat_map(char::to_uppercase)
        .chain(first.chars().skip(1))
        .collect();
    if !message.ends_with('.') {
        message.push('.');
    }
    let home = t3_agents_core::platform::home_dir();
    let home = home
        .as_ref()
        .and_then(|path| path.to_str())
        .filter(|path| path.len() > 1);
    let shorten = |line: &str| match home {
        Some(home) => line.replace(home, "~"),
        None => line.to_owned(),
    };
    let tips = lines
        .map(str::trim)
        .filter(|line| !line.starts_with("To try the dashboard without T3 data:"))
        .map(shorten)
        .collect();
    (shorten(&message), tips)
}
