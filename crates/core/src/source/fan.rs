//! The full read on several read-only connections that are proven to hold one snapshot.
//!
//! From a cold file cache the single-connection read spends most of its time waiting for
//! the `turn_items` scan: one page read at a time, in row-id order. Here that scan is cut
//! into row-id ranges, and the ranges, the fetch of the winning rows and the other tables'
//! queries are taken from one list by the calling thread and by `threads - 1` workers, each
//! on its own connection. Every statement is built by the function the single read uses;
//! the ranges only add a row-id bound to the scan's.
//!
//! **One snapshot.** Each connection is in a read transaction before any query runs, and
//! the read goes ahead only when `PRAGMA data_version`, read on the calling connection
//! before any worker pins and again as that connection's own first read after every worker
//! has pinned, is unchanged. SQLite documents that two values from one connection differ
//! if any other connection committed in between
//! (<https://sqlite.org/pragma.html#pragma_data_version>). The value comes from
//! `sqlite3PagerDataVersion`, which `pager_reset` advances when a read transaction opens
//! on a changed database (`sqlite3PagerSharedLock`: the change counter in rollback mode,
//! `walTryBeginRead`'s `*pChanged` in WAL mode; bundled `sqlite3.c`). A snapshot can only
//! move forward, so two equal values on the calling connection mean every pin in between
//! saw the same commit.
//!
//! Otherwise the pins are released and taken again, three times in all, and then this
//! returns `None` and the caller reads on one connection. That is also what happens on a
//! database whose `-shm` is read-only with no writer attached (a kept copy): SQLite
//! reports a change on every read transaction there (`walBeginShmUnreliable` sets
//! `*pChanged`), so the two values never agree.
//!
//! **One file.** The proof is about one database file and one WAL index. A restore that
//! renames another file over the path, or copies one over it and deletes the `-shm` (T3's
//! update rollback), is not a commit and the proof does not see it: a connection opened
//! before it and one opened after would read different files, each unchanged. So the
//! device and inode of the database, its `-wal` and its `-shm` are read before the first
//! connection opens and again after the last pin, and must be equal; every connection
//! must still have the file that is at the path (`SQLITE_FCNTL_HAS_MOVED`) after it
//! pinned; and the database file's size and its modification and change times, read
//! before each attempt's pins and after them, must be equal (a checkpoint in between pins
//! again). A read whose own connection creates the `-wal` and `-shm` (a database nobody
//! has open) is therefore left to the single connection; the next read finds them.
//!
//! **Errors.** This returns a snapshot or `None`. Any failure (opening a worker, pinning,
//! a query, a row the reader rejects) returns `None`, and the caller runs the
//! single-connection read, whose result and error are then the reader's by construction.
//! A worker's panic stops every connection and is raised again on the calling thread.
use super::{
    Json, WINDOW, apply_thread_asks,
    asks_hint::read_estimated_asks_inner,
    assemble::assemble_typed,
    check_tables, empty,
    fast_reads::{
        FetchedTurnItem, HistoryRow, NodeMap, ProjectRow, RequestRow, SpanRow, read_nodes_typed,
        read_projects_typed, read_requests_typed, read_spans_typed,
    },
    iso, marks, open_reader, payloads, read_ask_cards, read_latest_runs, read_turns_and_history,
    request_fields, run_scope, str_at, strings, timing,
    turn_items::{
        self, FETCH_CHUNK, Fetched, Range, Ranked, ScanKeys, Scanned, build, fetch_chunk,
        leaf_scan_safe, rank, scan, scan_ranges, tid_index,
    },
};
use crate::model::{self, Snapshot};
use rusqlite::{Connection, InterruptHandle, types::Value as SqlValue};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

/// `1` is the single-connection read. Anything that is not 1 to 16 means the default.
pub(super) const THREADS: &str = "T3_AGENTS_FULL_READ_THREADS";
/// Connections, the calling thread's included, when the switch is unset.
const DEFAULT_THREADS: usize = 3;
const PIN_ATTEMPTS: usize = 3;
/// Row-id ranges of the scan per connection: several, so that a range with more to read
/// than the others does not leave connections idle.
const SCAN_PARTS: usize = 4;

pub(super) fn threads() -> usize {
    let available = thread::available_parallelism().map_or(1, |n| n.get());
    thread_count(std::env::var(THREADS).ok().as_deref(), available)
}
fn thread_count(value: Option<&str>, available: usize) -> usize {
    value
        .and_then(|value| value.parse().ok())
        .filter(|n| (1..=16).contains(n))
        .unwrap_or_else(|| available.clamp(1, DEFAULT_THREADS))
}

/// Which files are at the database's path: the database, its log and its WAL index.
#[derive(Debug, PartialEq)]
struct Files {
    main: (u64, u64),
    wal: Option<(u64, u64)>,
    shm: Option<(u64, u64)>,
}
/// The database file's size and its modification and change times.
#[derive(Debug, PartialEq)]
struct Written {
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
#[cfg(unix)]
fn files(path: &Path) -> Option<(Files, Written)> {
    use std::os::unix::fs::MetadataExt;
    let beside = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        std::fs::metadata(name).ok()
    };
    let main = std::fs::metadata(path).ok().filter(|meta| meta.is_file())?;
    Some((
        Files {
            main: (main.dev(), main.ino()),
            wal: beside("-wal").map(|meta| (meta.dev(), meta.ino())),
            shm: beside("-shm").map(|meta| (meta.dev(), meta.ino())),
        },
        Written {
            size: main.len(),
            modified: (main.mtime(), main.mtime_nsec()),
            changed: (main.ctime(), main.ctime_nsec()),
        },
    ))
}
#[cfg(not(unix))]
fn files(_path: &Path) -> Option<(Files, Written)> {
    None
}

/// What one call did, for tests and the debug line.
#[derive(Default)]
pub(super) struct Trace<'a> {
    pub attempts: usize,
    /// `PRAGMA data_version` was equal around the pins.
    pub proved: bool,
    /// A query failed or a row was rejected, so the caller reads on one connection.
    pub failed: bool,
    /// The files at the path were not the same throughout, or a connection's file moved.
    pub moved: bool,
    /// The version changed on every read before any worker started: no proof to wait for.
    pub restless: bool,
    /// Row-id ranges the scan ran as, and statements the fetch ran as.
    pub parts: usize,
    pub chunks: usize,
    /// Milliseconds from the start of the call to each stage's end, when timing is on.
    pub stages: Vec<(&'static str, f64)>,
    /// Called on the calling thread after the workers pinned and before it pins.
    pub between_pins: Option<&'a dyn Fn(usize)>,
    /// Called on the calling thread after it connected and before any worker does.
    pub after_open: Option<&'a dyn Fn()>,
    /// Called on the calling thread after the workers were told to work and before its
    /// own first query.
    pub before_first: Option<&'a dyn Fn()>,
    /// Tests only: called at each `Stage` of every worker, on that worker's thread.
    #[cfg(test)]
    pub worker: Option<Arc<dyn Fn(Stage) + Send + Sync>>,
    /// Tests only: fetch statements of this many row ids.
    #[cfg(test)]
    pub chunk: Option<usize>,
}
impl Trace<'_> {
    /// For the debug line: what decided how the read ran.
    pub(super) fn describe(&self, threads: usize, fanned: bool) -> String {
        let stages: Vec<String> = self
            .stages
            .iter()
            .map(|(name, ms)| format!("{name}={ms:.1}"))
            .collect();
        let how = if fanned {
            "fan proof=data_version"
        } else if threads < 2 {
            "serial why=one-thread"
        } else if self.failed {
            "serial why=failed"
        } else if self.moved {
            "serial why=files-changed"
        } else if self.restless {
            "serial why=version-never-still"
        } else if self.attempts == PIN_ATTEMPTS && !self.proved {
            "serial why=no-proof"
        } else {
            "serial why=not-started"
        };
        format!(
            "{how} threads={threads} attempts={} parts={} chunks={} stages[{}]",
            self.attempts,
            self.parts,
            self.chunks,
            stages.join(" ")
        )
    }
}
/// Tests only: where a worker is when `Trace::worker` is called, with its position (1 on).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    /// Connected, before it reads the schema.
    Connected(usize),
    /// Told to pin, before its `BEGIN` and first read.
    Pinning(usize),
    /// Its pin's result sent, before the next command.
    Pinned(usize),
    /// Told to work, before it takes a task.
    Working(usize),
    /// A task taken (on any thread, the calling one too), before its query runs.
    Query(Task),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Task {
    Runs,
    Subs,
    /// One row-id range of the `turn_items` scan, by position.
    Scan(usize),
    Estimates,
    /// One statement of the winning rows' fetch, by position.
    Fetch(usize),
    Turns,
    Providers,
    Nodes,
    Spans,
    Requests,
    Cards,
    Projects,
}
impl Task {
    /// Which of several ready tasks goes first. The runs and subagents decide the scan's
    /// keys, the scan decides the fetch and the estimates, and those are the longest chain;
    /// the rest are short and fill the gaps.
    fn order(self) -> (u8, usize) {
        match self {
            Task::Runs => (0, 0),
            Task::Subs => (1, 0),
            Task::Scan(at) => (2, at),
            Task::Estimates => (3, 0),
            Task::Fetch(at) => (4, at),
            Task::Turns => (5, 0),
            Task::Providers => (6, 0),
            Task::Nodes => (7, 0),
            Task::Spans => (8, 0),
            Task::Requests => (9, 0),
            Task::Cards => (10, 0),
            Task::Projects => (11, 0),
        }
    }
}

/// What the threads query decides: every later query binds these.
struct Threads {
    ids: Vec<String>,
    where_ids: String,
    params: Vec<SqlValue>,
    since: String,
    since_params: Vec<SqlValue>,
    /// The scan's row-id ranges; none when this schema's items are read by the reference
    /// queries instead.
    ranges: Vec<Range>,
}
/// What the latest runs and the subagents decide: whose items are read.
struct Runs {
    latest: HashMap<String, Json>,
    subs: Vec<Json>,
    run_ids: Vec<String>,
    runless: Vec<String>,
}
/// What the scan decides: the rows to fetch, in statements of `chunk` row ids.
struct Winners {
    ranked: Ranked,
    chunk: usize,
}
/// What a task reads from, as far as it has been decided.
#[derive(Clone, Default)]
struct Scope {
    threads: Option<Arc<Threads>>,
    runs: Option<Arc<Runs>>,
    winners: Option<Arc<Winners>>,
}

enum Out {
    Runs(HashMap<String, Json>, Vec<String>),
    Subs(Vec<Json>),
    Scan(Scanned),
    Estimates(HashMap<String, u64>),
    Fetch(Vec<(i64, FetchedTurnItem)>),
    Turns(HashMap<String, Json>, Vec<HistoryRow>),
    Providers(HashMap<String, Json>),
    Nodes(NodeMap),
    Spans(Vec<SpanRow>),
    Requests(Vec<RequestRow>),
    Cards(HashMap<String, model::AskCards>),
    Projects(HashMap<String, ProjectRow>),
}

/// `None`: the query failed, or what it reads from was not there.
fn run(task: Task, scope: &Scope, db: &Connection, path: &Path) -> Option<Out> {
    let threads = || scope.threads.as_deref();
    let runs = || scope.runs.as_deref();
    Some(match task {
        Task::Projects => Out::Projects(read_projects_typed(db).ok()?.0),
        Task::Cards => Out::Cards(read_ask_cards(db, None).ok()?),
        Task::Runs => {
            let t = threads()?;
            let (latest, order, _) = read_latest_runs(db, path, &t.where_ids, &t.params).ok()?;
            Out::Runs(latest, order)
        }
        Task::Subs => {
            let t = threads()?;
            Out::Subs(payloads(db, "subagents", &t.where_ids, &t.params).ok()?)
        }
        Task::Providers => {
            let t = threads()?;
            Out::Providers(
                payloads(db, "provider_threads", &t.where_ids, &t.params)
                    .ok()?
                    .into_iter()
                    .map(|p| (str_at(&p, "id").into(), p))
                    .collect(),
            )
        }
        Task::Turns => {
            let t = threads()?;
            let (turns, history, _) =
                read_turns_and_history(db, path, &t.where_ids, &t.params).ok()?;
            Out::Turns(turns, history)
        }
        Task::Requests => {
            let t = threads()?;
            let fields = request_fields(db).ok()?;
            Out::Requests(read_requests_typed(db, &t.where_ids, &fields, &t.params).ok()?)
        }
        Task::Spans => {
            let t = threads()?;
            Out::Spans(read_spans_typed(db, &t.where_ids, &t.since_params).ok()?)
        }
        Task::Nodes => {
            let (t, r) = (threads()?, runs()?);
            let _read_nodes = timing::phase("read_nodes");
            let active: HashSet<&str> = t.ids.iter().map(String::as_str).collect();
            Out::Nodes(
                read_nodes_typed(db, &t.where_ids, &t.params, &active, &r.subs, path)
                    .ok()?
                    .0,
            )
        }
        Task::Scan(at) => {
            let (t, r) = (threads()?, runs()?);
            let _read_items = timing::phase("read_items");
            let keys = ScanKeys::new(&t.ids, &r.run_ids, &r.runless, &t.since);
            Out::Scan(scan(db, &keys, *t.ranges.get(at)?).ok()?)
        }
        Task::Fetch(at) => {
            let w = scope.winners.as_deref()?;
            let _read_items = timing::phase("read_items");
            let chunk = w.ranked.rowids.chunks(w.chunk).nth(at)?;
            let mut rows = Vec::with_capacity(chunk.len());
            fetch_chunk(db, path, chunk, |rid, item| rows.push((rid, item))).ok()?;
            Out::Fetch(rows)
        }
        Task::Estimates => {
            let w = scope.winners.as_deref()?;
            Out::Estimates(read_estimated_asks_inner(db, None, w.ranked.asst.as_ref()).ok()?)
        }
    })
}

#[derive(Default)]
struct Results {
    runs: Option<(HashMap<String, Json>, Vec<String>)>,
    subs: Option<Vec<Json>>,
    scans: Vec<Scanned>,
    estimates: Option<HashMap<String, u64>>,
    fetched: Vec<Vec<(i64, FetchedTurnItem)>>,
    turns: Option<(HashMap<String, Json>, Vec<HistoryRow>)>,
    providers: Option<HashMap<String, Json>>,
    nodes: Option<NodeMap>,
    spans: Option<Vec<SpanRow>>,
    requests: Option<Vec<RequestRow>>,
    cards: Option<HashMap<String, model::AskCards>>,
    projects: Option<HashMap<String, ProjectRow>>,
}
struct Board {
    /// Tasks whose inputs are decided and that no thread has taken.
    pending: Vec<Task>,
    /// Tasks taken and not finished. A finishing task may add tasks.
    running: usize,
    /// The calling thread has added the tasks that wait for the threads query.
    open: bool,
    stop: bool,
    scope: Scope,
    results: Results,
}
struct Fan {
    board: Mutex<Board>,
    wake: Condvar,
    failed: AtomicBool,
    /// Every connection's handle, the calling thread's first.
    handles: Mutex<Vec<InterruptHandle>>,
    /// The first worker's panic, raised again on the calling thread.
    panic: Mutex<Option<Box<dyn std::any::Any + Send>>>,
    threads: usize,
    started: Instant,
    stages: Mutex<Vec<(&'static str, f64)>>,
    #[cfg(test)]
    hook: Option<Arc<dyn Fn(Stage) + Send + Sync>>,
    #[cfg(test)]
    chunk: Option<usize>,
}
/// A panic on another thread may have poisoned the lock; the board is still consistent
/// enough to stop on.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
impl Fan {
    fn stage(&self, name: &'static str) {
        if timing::on() {
            lock(&self.stages).push((name, self.started.elapsed().as_secs_f64() * 1000.));
        }
    }
    fn next(&self) -> Option<(Task, Scope)> {
        let mut board = lock(&self.board);
        loop {
            if board.stop {
                return None;
            }
            let first = (0..board.pending.len()).min_by_key(|at| board.pending[*at].order());
            if let Some(at) = first {
                board.running += 1;
                return Some((board.pending.swap_remove(at), board.scope.clone()));
            }
            if board.open && board.running == 0 {
                return None;
            }
            board = self
                .wake
                .wait(board)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
    /// No further task starts, and running queries are interrupted from `first` on
    /// (0: every connection, 1: the workers only).
    fn stop(&self, first: usize) {
        lock(&self.board).stop = true;
        self.wake.notify_all();
        for handle in lock(&self.handles).iter().skip(first) {
            handle.interrupt();
        }
    }
    /// The threads query's result: the tasks that bind it can start.
    fn open(&self, threads: Threads) {
        let mut board = lock(&self.board);
        board.scope.threads = Some(Arc::new(threads));
        board.pending.extend([
            Task::Runs,
            Task::Subs,
            Task::Turns,
            Task::Providers,
            Task::Spans,
            Task::Requests,
        ]);
        board.open = true;
        drop(board);
        self.wake.notify_all();
    }
    fn finish(&self, task: Task, out: Option<Out>) {
        let Some(out) = out else {
            // A query interrupted by `stop` did not fail on its own account.
            if !lock(&self.board).stop {
                self.failed.store(true, Ordering::SeqCst);
                self.stop(0);
            }
            return;
        };
        let mut board = lock(&self.board);
        let mut scanned = None;
        match out {
            Out::Runs(latest, order) => board.results.runs = Some((latest, order)),
            Out::Subs(subs) => board.results.subs = Some(subs),
            Out::Scan(found) => {
                board.results.scans.push(found);
                let parts = board.scope.threads.as_ref().map_or(0, |t| t.ranges.len());
                if board.results.scans.len() == parts {
                    scanned = Some(std::mem::take(&mut board.results.scans));
                }
            }
            Out::Estimates(found) => board.results.estimates = Some(found),
            Out::Fetch(rows) => board.results.fetched.push(rows),
            Out::Turns(turns, history) => board.results.turns = Some((turns, history)),
            Out::Providers(found) => board.results.providers = Some(found),
            Out::Nodes(found) => board.results.nodes = Some(found),
            Out::Spans(found) => board.results.spans = Some(found),
            Out::Requests(found) => board.results.requests = Some(found),
            Out::Cards(found) => board.results.cards = Some(found),
            Out::Projects(found) => board.results.projects = Some(found),
        }
        // Both the latest runs and the subagents are in: whose items are read is decided.
        if matches!(task, Task::Runs | Task::Subs)
            && board.results.runs.is_some()
            && board.results.subs.is_some()
        {
            if let (Some(threads), Some((latest, order)), Some(subs)) = (
                board.scope.threads.clone(),
                board.results.runs.take(),
                board.results.subs.take(),
            ) {
                let (run_ids, runless) = run_scope(&threads.ids, &latest, &order, &subs);
                board.scope.runs = Some(Arc::new(Runs {
                    latest,
                    subs,
                    run_ids,
                    runless,
                }));
                board.pending.push(Task::Nodes);
                if threads.ranges.is_empty() {
                    board.scope.winners = Some(Arc::new(Winners {
                        ranked: Ranked::default(),
                        chunk: FETCH_CHUNK,
                    }));
                    board.pending.push(Task::Estimates);
                } else {
                    board
                        .pending
                        .extend((0..threads.ranges.len()).map(Task::Scan));
                }
                self.stage("runs");
            }
        }
        // The last range is in. This thread ranks; it still counts as running, so no
        // thread leaves before the fetch and the estimates are on the list.
        if let Some(parts) = scanned {
            let threads = board.scope.threads.clone();
            drop(board);
            let ranked = {
                let _read_items = timing::phase("read_items");
                let mut parts = parts.into_iter();
                let mut whole = parts.next();
                if let Some(whole) = &mut whole {
                    for part in parts {
                        whole.absorb(part);
                    }
                }
                match (whole, threads) {
                    (Some(whole), Some(threads)) => rank(whole, tid_index(&threads.ids)),
                    _ => Ranked::default(),
                }
            };
            let rows = ranked.rowids.len();
            // Two statements for each connection, so that one with large rows does not
            // leave the others idle.
            let chunk = rows.div_ceil(self.threads * 2).clamp(64, FETCH_CHUNK);
            #[cfg(test)]
            let chunk = self.chunk.unwrap_or(chunk);
            board = lock(&self.board);
            board.scope.winners = Some(Arc::new(Winners { ranked, chunk }));
            board.pending.push(Task::Estimates);
            board
                .pending
                .extend((0..rows.div_ceil(chunk)).map(Task::Fetch));
            self.stage("scanned");
        }
        board.running -= 1;
        drop(board);
        self.wake.notify_all();
    }
    fn work(&self, db: &Connection, path: &Path) {
        while let Some((task, scope)) = self.next() {
            #[cfg(test)]
            if let Some(hook) = &self.hook {
                hook(Stage::Query(task));
            }
            let out = run(task, &scope, db, path);
            drop(scope);
            self.finish(task, out);
        }
    }
    /// A worker panicked: keep its panic, and stop every connection as a failed query does.
    fn abandon(&self, panic: Box<dyn std::any::Any + Send>) {
        lock(&self.panic).get_or_insert(panic);
        self.failed.store(true, Ordering::SeqCst);
        self.stop(0);
    }
}

/// Stops the workers when the calling thread leaves before every task is done, whether it
/// returns or unwinds: they may be waiting for tasks only it can add.
struct StopOnLeave<'a>(&'a Fan);
impl Drop for StopOnLeave<'_> {
    fn drop(&mut self) {
        self.0.stop(1);
    }
}

/// A worker's page cache, in KiB. A worker walks each page of its ranges once and reads
/// each fetched row's overflow chain once, so a larger cache holds pages nothing asks for
/// again. Measured on a 2.1 GB database: SQLite's default (2 MiB) takes the same time and
/// about 1.4 MiB more resident memory for each connection. The calling connection keeps
/// the default: it also runs the reference queries when the scan leaves a list undecided.
const WORKER_CACHE_KB: u32 = 512;

/// `at` is the path's canonical form: SQLite must have resolved the same one.
fn connect(path: &Path, at: &Path, worker: bool) -> Option<Connection> {
    let db = open_reader(path).ok()?;
    if db.path().is_none_or(|name| Path::new(name) != at) {
        return None;
    }
    db.execute_batch("PRAGMA query_only=ON").ok()?;
    if worker {
        db.execute_batch(&format!("PRAGMA cache_size=-{WORKER_CACHE_KB}"))
            .ok()?;
    }
    Some(db)
}
fn data_version(db: &Connection) -> Option<i64> {
    db.query_row("PRAGMA data_version", [], |row| row.get(0))
        .ok()
}
/// A worker's pin: a read transaction whose first statement has read the database
/// (`BEGIN` alone reads nothing), on the file that is still at the path.
fn pin(db: &Connection, at: &Path) -> bool {
    db.execute_batch("BEGIN").is_ok()
        && db
            .query_row(crate::fanout::PIN_SNAPSHOT, [], |_| Ok(()))
            .is_ok()
        && crate::cache::handle_matches(db, at)
}

enum Command {
    /// Leave any read transaction and pin again.
    Pin,
    Work,
}

/// `None`: nothing was decided, and the caller reads on one connection.
pub(super) fn read(
    path: &Path,
    now: f64,
    threads: usize,
    trace: &mut Trace<'_>,
) -> Option<Snapshot> {
    let pinning = timing::phase("fan_pin");
    // SQLite resolves the path the same way and names the log and the index after it.
    let at = std::fs::canonicalize(path).ok()?;
    let at = at.as_path();
    let (first_files, mut written) = files(at)?;
    let db = connect(path, at, false)?;
    // On a WAL index that this process cannot write and no writer holds (a kept copy),
    // SQLite reports a change on every read transaction, and the proof could never hold.
    // Two changes in a row, microseconds apart, are taken as that before anything is spent
    // on workers. A live database that commits twice in that time is read on one
    // connection this once.
    let version = data_version(&db)?;
    let again = data_version(&db)?;
    if version != again && data_version(&db)? != again {
        trace.restless = true;
        return None;
    }
    if let Some(after_open) = trace.after_open {
        after_open();
    }
    let fan = Fan {
        board: Mutex::new(Board {
            // These two bind nothing, so the workers start on them at once.
            pending: vec![Task::Cards, Task::Projects],
            running: 0,
            open: false,
            stop: false,
            scope: Scope::default(),
            results: Results::default(),
        }),
        wake: Condvar::new(),
        failed: AtomicBool::new(false),
        handles: Mutex::new(vec![db.get_interrupt_handle()]),
        panic: Mutex::new(None),
        threads,
        started: Instant::now(),
        stages: Mutex::new(Vec::new()),
        #[cfg(test)]
        hook: trace.worker.clone(),
        #[cfg(test)]
        chunk: trace.chunk,
    };
    // The active threads' rows; `Some(None)` when there is none.
    let mut first: Option<Option<Vec<Json>>> = None;
    let outcome = thread::scope(|pool| {
        let (pinned, pins) = mpsc::channel::<bool>();
        let mut commands = Vec::with_capacity(threads - 1);
        for _ in 1..threads {
            let (command, inbox) = mpsc::channel::<Command>();
            let (pinned, fan) = (pinned.clone(), &fan);
            #[cfg(test)]
            let (hook, position, timed) = (fan.hook.clone(), commands.len() + 1, timing::on());
            thread::Builder::new()
                .spawn_scoped(pool, move || {
                    // A panic must not leave another thread waiting for this one: the
                    // calling thread may wait for its pin, and the other workers for tasks
                    // that this one would have added. The panic stops the whole read, and
                    // the calling thread raises it again when every worker is done.
                    let worker = || {
                        #[cfg(test)]
                        timing::inherit(timed);
                        #[cfg(test)]
                        let stage = |at: fn(usize) -> Stage| {
                            if let Some(hook) = &hook {
                                hook(at(position));
                            }
                        };
                        let Some(db) = connect(path, at, true) else {
                            let _ = pinned.send(false);
                            return;
                        };
                        #[cfg(test)]
                        stage(Stage::Connected);
                        lock(&fan.handles).push(db.get_interrupt_handle());
                        // Read the schema before the pins are timed against commits: the
                        // pin itself is then one short read, and the window between pins
                        // is small.
                        let ready = db
                            .query_row(crate::fanout::PIN_SNAPSHOT, [], |_| Ok(()))
                            .is_ok();
                        let _ = pinned.send(ready);
                        loop {
                            match inbox.recv() {
                                Ok(Command::Pin) => {
                                    let released =
                                        db.is_autocommit() || db.execute_batch("ROLLBACK").is_ok();
                                    #[cfg(test)]
                                    stage(Stage::Pinning);
                                    let _ = pinned.send(released && pin(&db, at));
                                    #[cfg(test)]
                                    stage(Stage::Pinned);
                                }
                                Ok(Command::Work) => break,
                                // The caller gave up: nothing was read.
                                Err(_) => return,
                            }
                        }
                        #[cfg(test)]
                        stage(Stage::Working);
                        fan.work(&db, path);
                        let _ = db.execute_batch("ROLLBACK");
                    };
                    if let Err(panic) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(worker))
                    {
                        fan.abandon(panic);
                        let _ = pinned.send(false);
                    }
                })
                .ok()?;
            commands.push(command);
        }
        drop(pinned);
        // Every worker is connected and has read the schema.
        for _ in &commands {
            if !pins.recv().ok()? {
                return None;
            }
        }
        for attempt in 0..PIN_ATTEMPTS {
            trace.attempts = attempt + 1;
            if attempt > 0 {
                db.execute_batch("ROLLBACK").ok()?;
                // A checkpoint may have written the file during the last attempt: read
                // its state again. Which files they are must not have changed at all.
                let (files_now, written_now) = files(at)?;
                if files_now != first_files {
                    trace.moved = true;
                    return None;
                }
                written = written_now;
            }
            let before = data_version(&db)?;
            for command in &commands {
                command.send(Command::Pin).ok()?;
            }
            for _ in &commands {
                if !pins.recv().ok()? {
                    return None;
                }
            }
            if let Some(between) = trace.between_pins {
                between(attempt);
            }
            db.execute_batch("BEGIN").ok()?;
            // The calling connection's first read: it pins, and reports any commit since
            // `before`.
            let after = data_version(&db)?;
            // Every connection has pinned: the same files as before the first one opened?
            let (last_files, last_written) = files(at)?;
            if last_files != first_files || !crate::cache::handle_matches(&db, at) {
                trace.moved = true;
                return None;
            }
            // A database file written since this attempt began (a checkpoint, or something
            // that is not SQLite) pins again.
            trace.proved = last_written == written && before == after;
            if trace.proved {
                break;
            }
        }
        if !trace.proved {
            return None;
        }
        drop(pinning);
        fan.stage("pinned");
        // From here the workers wait for tasks that this thread adds. If it leaves early,
        // nothing else would wake them and `thread::scope` would wait forever.
        let _leaving = StopOnLeave(&fan);
        for command in &commands {
            command.send(Command::Work).ok()?;
        }
        if let Some(before_first) = trace.before_first {
            before_first();
        }
        check_tables(&db, path).ok()?;
        let rows = payloads(
            &db,
            "threads",
            "WHERE deleted_at IS NULL AND archived_at IS NULL",
            &[],
        )
        .ok()?;
        let ids: Vec<String> = rows.iter().map(|r| str_at(r, "id").into()).collect();
        if ids.is_empty() {
            first = Some(None);
            return Some(());
        }
        first = Some(Some(rows));
        let ranges = if leaf_scan_safe(&db).unwrap_or(false) {
            scan_ranges(&db, threads * SCAN_PARTS)
        } else {
            Vec::new()
        };
        let params = strings(ids.clone());
        let since = iso(now - WINDOW);
        let mut since_params = params.clone();
        since_params.push(SqlValue::Text(since.clone()));
        fan.stage("threads");
        fan.open(Threads {
            where_ids: format!("WHERE thread_id IN ({})", marks(ids.len())),
            ids,
            params,
            since,
            since_params,
            ranges,
        });
        fan.work(&db, path);
        fan.stage("read");
        Some(())
    });
    // A worker panicked. The panic hook has run on its thread, as it would have on this
    // one in the single read; the read neither falls back nor returns.
    if let Some(panic) = lock(&fan.panic).take() {
        std::panic::resume_unwind(panic);
    }
    let failed = fan.failed.load(Ordering::SeqCst);
    // The workers' connections, candidates and fetched rows were freed on threads that are
    // gone. glibc keeps each thread's freed heap to itself, so without this the assembly's
    // allocations come on top of it (measured on a 2.1 GB database: 1.4 MiB of peak
    // resident memory).
    crate::clock::release_memory();
    let Fan { board, stages, .. } = fan;
    let gathered = match (failed, outcome, first) {
        (false, Some(()), Some(None)) => Some(empty(path, now)),
        (false, Some(()), Some(Some(threads))) => {
            let board = board.into_inner().unwrap_or_else(|p| p.into_inner());
            let found = gather(threads, board, &db, path, now, trace);
            trace.failed = found.is_none();
            found
        }
        _ => {
            trace.failed = failed;
            None
        }
    };
    trace.stages = stages.into_inner().unwrap_or_else(|p| p.into_inner());
    // End the read transaction, as the single read does.
    let rollback = db.is_autocommit() || db.execute_batch("ROLLBACK").is_ok();
    gathered.filter(|_| rollback)
}

/// Every task is done: the single read's assembly on the same values. The lists the scan
/// left undecided are read here, on the calling connection, which holds the same snapshot.
fn gather(
    threads: Vec<Json>,
    board: Board,
    db: &Connection,
    path: &Path,
    now: f64,
    trace: &mut Trace<'_>,
) -> Option<Snapshot> {
    let Board { scope, results, .. } = board;
    let (scoped, winners) = (scope.threads?, scope.winners?);
    // The workers are gone, so this is the only holder.
    let runs = Arc::try_unwrap(scope.runs?).ok()?;
    trace.parts = scoped.ranges.len();
    trace.chunks = results.fetched.len();
    let mut fetched: Fetched =
        HashMap::with_capacity_and_hasher(winners.ranked.rowids.len(), Default::default());
    for rows in results.fetched {
        fetched.extend(rows);
    }
    let (items, events, plans, _) = {
        let _read_items = timing::phase("read_items");
        let built = build(&winners.ranked, &fetched, path).ok()?;
        turn_items::finish(
            db,
            path,
            &scoped.where_ids,
            &scoped.params,
            &runs.run_ids,
            &runs.runless,
            &scoped.since,
            built,
        )
        .ok()?
    };
    // The fetched rows, the ranking and the bound ids are used up: free them before the
    // assembly allocates the snapshot, as the single read has by then (measured on the
    // same database: 4.8 MiB of peak resident memory).
    drop((fetched, winners, scoped));
    let (turns, history) = results.turns?;
    let mut snapshot = assemble_typed(
        threads,
        runs.latest,
        runs.subs,
        results.providers?,
        turns,
        &results.nodes?,
        &results.projects?,
        &results.requests?,
        &items,
        &events,
        &history,
        &results.spans?,
        &plans,
        path,
        now,
    )
    .ok()?;
    apply_thread_asks(&mut snapshot, &results.cards?, &results.estimates?, None);
    Some(snapshot)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{path, schema};
    use super::super::{PREFIX, snapshot, snapshot_serial};
    use super::*;
    use serde_json::json;
    use std::{cell::Cell, path::PathBuf, time::Duration};

    const NOW: &str = "2026-11-12T12:30:00Z";
    fn now() -> f64 {
        model::ms(Some(NOW)).unwrap()
    }
    /// A folder with a seeded database, and the connection that writes it.
    struct Lab {
        dir: PathBuf,
        file: PathBuf,
        writer: Connection,
    }
    impl Drop for Lab {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    fn lab(name: &str, wal: bool) -> Lab {
        let dir = path(name).with_extension("dir");
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("synthetic.sqlite");
        let writer = Connection::open(&file).unwrap();
        if wal {
            writer
                .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0")
                .unwrap();
        }
        schema(&writer);
        writer
            .execute_batch(&format!(
                "CREATE TABLE {PREFIX}messages(message_id TEXT,thread_id TEXT,node_id TEXT,role TEXT,streaming INTEGER,created_at TEXT,updated_at TEXT,payload_json TEXT);
                 BEGIN"
            ))
            .unwrap();
        writer
            .execute(
                "INSERT INTO projection_projects VALUES('p','Project','/synthetic')",
                [],
            )
            .unwrap();
        for t in 0..6 {
            thread(&writer, &format!("t{t}"), t);
        }
        let sub = json!({"id":"s0","threadId":"t0","runId":"r-t0","providerThreadId":"pt-s0","status":"running","title":"Sub","updatedAt":"2026-11-12T12:01:00.000Z"});
        writer
            .execute(
                &format!("INSERT INTO {PREFIX}subagents VALUES('t0',?)"),
                [sub.to_string()],
            )
            .unwrap();
        writer
            .execute(
                &format!("INSERT INTO {PREFIX}nodes VALUES('t0','s0','n-t0','r-t0')"),
                [],
            )
            .unwrap();
        writer.execute_batch("COMMIT").unwrap();
        Lab { dir, file, writer }
    }
    fn thread(db: &Connection, id: &str, n: usize) {
        let (run, provider) = (format!("r-{id}"), format!("pt-{id}"));
        db.execute(
            &format!("INSERT INTO {PREFIX}threads VALUES(?,?,NULL,NULL)"),
            [
                id,
                &json!({"id":id,"projectId":"p","title":id,"activeProviderThreadId":provider})
                    .to_string(),
            ],
        )
        .unwrap();
        db.execute(
            &format!("INSERT INTO {PREFIX}runs VALUES(?,1,?,'2026-11-12T12:00:00.000Z',NULL)"),
            [id, &json!({"id":run,"threadId":id,"status":"running","providerThreadId":provider,"startedAt":"2026-11-12T12:00:00.000Z"}).to_string()],
        )
        .unwrap();
        db.execute(
            &format!("INSERT INTO {PREFIX}provider_threads VALUES(?,?)"),
            [
                id,
                &json!({"id":provider,"contextUsage":{"usedTokens":n,"maxTokens":100}}).to_string(),
            ],
        )
        .unwrap();
        db.execute(
            &format!("INSERT INTO {PREFIX}nodes VALUES(?,?,NULL,?)"),
            [id, &format!("n-{id}"), &run],
        )
        .unwrap();
        for i in 0..4 {
            item(db, id, 10 * n + i);
            db.execute(
                &format!("INSERT INTO {PREFIX}provider_turns VALUES(?,?,?,?,?,NULL)"),
                rusqlite::params![id, provider, i as i64,
                    json!({"providerThreadId":provider,"model":"m","tokenUsage":{"usedTokens":i + n,"maxTokens":100}}).to_string(),
                    format!("2026-11-12T12:0{i}:00.000Z")],
            )
            .unwrap();
        }
        db.execute(
            &format!("INSERT INTO {PREFIX}turn_items VALUES(?,?,NULL,?,'todo_list','completed','2026-11-12T12:05:00.000Z',900,?)"),
            [id, &run, &provider, &json!({"steps":[{"text":"one","status":"pending"}]}).to_string()],
        )
        .unwrap();
        // An assistant message with an ask, and its item: the estimate reads both.
        db.execute(
            &format!("INSERT INTO {PREFIX}turn_items VALUES(?1,?2,?3,?4,'assistant_message','completed','2026-11-12T12:06:00.000Z',901,?5)"),
            rusqlite::params![id, run, format!("a-{id}"), provider, json!({"messageId":format!("m-{id}"),"phase":"final_answer"}).to_string()],
        )
        .unwrap();
        db.execute(
            &format!("INSERT INTO {PREFIX}messages VALUES(?1,?2,?3,'assistant',0,'2026-11-12T12:06:00.000Z','2026-11-12T12:06:00.000Z',?4)"),
            rusqlite::params![format!("m-{id}"), id, format!("a-{id}"), json!({"text":"Done. Shall I go on?"}).to_string()],
        )
        .unwrap();
    }
    fn item(db: &Connection, thread: &str, ordinal: usize) {
        db.execute(
            &format!("INSERT INTO {PREFIX}turn_items VALUES(?1,?2,?3,?4,'command_execution',?5,?6,?7,?8)"),
            rusqlite::params![thread, format!("r-{thread}"), format!("n-{thread}"), format!("pt-{thread}"),
                if ordinal % 3 == 0 { "failed" } else { "completed" },
                format!("2026-11-12T12:{:02}:{:02}.000Z", ordinal / 60 % 30, ordinal % 60), ordinal as i64,
                json!({"input":format!("step {ordinal}"),"title":"Step","startedAt":"2026-11-12T12:00:00.000Z"}).to_string()],
        )
        .unwrap();
    }
    /// One commit that changes what five of the fanned-out queries return.
    fn commit(db: &Connection, step: usize) {
        db.execute_batch("BEGIN").unwrap();
        item(db, "t0", 500 + step);
        item(db, "t3", 600 + step);
        db.execute(
            &format!(
                "INSERT INTO {PREFIX}runtime_requests VALUES('t1',NULL,'approval','pending',NULL)"
            ),
            [],
        )
        .unwrap();
        db.execute(
            &format!("INSERT INTO {PREFIX}provider_turns VALUES('t2','pt-t2',?,?,'2026-11-12T12:20:00.000Z',NULL)"),
            rusqlite::params![50 + step as i64,
                json!({"providerThreadId":"pt-t2","model":"later","tokenUsage":{"usedTokens":90,"maxTokens":100}}).to_string()],
        )
        .unwrap();
        thread(db, &format!("late{step}"), 7 + step);
        db.execute_batch("COMMIT").unwrap();
    }
    fn same(a: &Snapshot, b: &Snapshot) {
        assert_eq!(
            serde_json::to_value(a).unwrap(),
            serde_json::to_value(b).unwrap()
        );
        assert_eq!(a.agents.len(), b.agents.len());
        for (a, b) in a.agents.iter().zip(&b.agents) {
            assert_eq!(a.open_asks, b.open_asks, "{}", a.id);
            assert_eq!(a.estimated_asks, b.estimated_asks, "{}", a.id);
            assert_eq!(a.storage, b.storage, "{}", a.id);
        }
    }
    fn single(file: &Path) -> Snapshot {
        snapshot_serial(file, now()).unwrap()
    }

    #[test]
    fn switches() {
        for (available, default) in [(0, 1), (1, 1), (2, 2), (64, DEFAULT_THREADS)] {
            for value in [
                None,
                Some(""),
                Some("0"),
                Some("17"),
                Some("x"),
                Some(" 2"),
                Some("-1"),
            ] {
                assert_eq!(
                    thread_count(value, available),
                    default,
                    "{value:?}, {available} processors"
                );
            }
            for threads in 1..=16 {
                assert_eq!(thread_count(Some(&threads.to_string()), available), threads);
            }
        }
    }

    /// Threads `th0..th23` with a run each and `bare0..bare3` with none, on top of `lab`.
    fn item_lab(name: &str) -> Lab {
        let lab = lab(name, true);
        lab.writer.execute_batch("BEGIN").unwrap();
        for n in 0..24 {
            thread(&lab.writer, &format!("th{n}"), n);
        }
        for n in 0..4 {
            let id = format!("bare{n}");
            lab.writer
                .execute(
                    &format!("INSERT INTO {PREFIX}threads VALUES(?,?,NULL,NULL)"),
                    [&id, &json!({"id":id,"projectId":"p"}).to_string()],
                )
                .unwrap();
        }
        lab.writer.execute_batch("COMMIT").unwrap();
        lab
    }
    /// `owner` below 24 is a thread with a run; from 24 a thread with none.
    fn raw_item(
        db: &Connection,
        owner: usize,
        kind: &str,
        node: Option<&str>,
        stamp: &str,
        ordinal: i64,
        payload: &str,
    ) {
        let (thread, run) = match owner {
            n if n < 24 => (format!("th{n}"), Some(format!("r-th{n}"))),
            n => (format!("bare{}", n - 24), None),
        };
        db.execute(
            &format!(
                "INSERT INTO {PREFIX}turn_items VALUES(?1,?2,?3,NULL,?4,'completed',?5,?6,?7)"
            ),
            rusqlite::params![thread, run, node, kind, stamp, ordinal, payload],
        )
        .unwrap();
    }
    /// `rows` items over `stamps` distinct milliseconds and `ordinals` distinct ordinals, each
    /// with its own label, so any difference in order or at a limit shows. `recent` of every
    /// 16 rows fall inside the event window; some are assistant messages that share a node.
    fn scatter(lab: &Lab, seed: u64, rows: usize, stamps: u64, ordinals: u64, recent: u64) {
        let mut state = seed;
        let mut random = move |below: u64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) % below
        };
        lab.writer.execute_batch("BEGIN").unwrap();
        for n in 0..rows {
            // `stamps` 0: every row its own millisecond.
            let ms = if stamps == 0 {
                n as u64
            } else {
                random(stamps)
            };
            let hour = if random(16) < recent { 11 } else { 8 };
            let stamp = format!(
                "2026-11-12T{hour:02}:{:02}:{:02}.{:03}Z",
                ms / 60_000 % 60,
                ms / 1000 % 60,
                ms % 1000
            );
            let owner = random(28) as usize;
            let (kind, node) = match random(10) {
                0 => ("assistant_message", Some(format!("shared-{}", random(40)))),
                1 => ("todo_list", None),
                2 => ("user_message", None),
                3 => ("checkpoint", None),
                _ => ("command_execution", None),
            };
            let payload = json!({"title":format!("row {n}"),"input":"x","messageId":format!("sm-{}", random(60)),"phase":"commentary","steps":[{"text":format!("step {n}"),"status":"pending"}]});
            raw_item(
                &lab.writer,
                owner,
                kind,
                node.as_deref(),
                &stamp,
                random(ordinals) as i64,
                &payload.to_string(),
            );
        }
        // Messages on the shared nodes: the estimate looks their items up by node.
        for m in 0..60 {
            lab.writer
                .execute(
                    &format!(
                        "INSERT INTO {PREFIX}messages VALUES(?1,?2,?3,'assistant',0,?4,?4,?5)"
                    ),
                    rusqlite::params![
                        format!("sm-{m}"),
                        format!("th{}", m % 24),
                        format!("shared-{}", m % 40),
                        format!("2026-11-12T12:07:{:02}.000Z", m),
                        json!({"text":"Which one? And when?"}).to_string()
                    ],
                )
                .unwrap();
        }
        lab.writer.execute_batch("COMMIT").unwrap();
    }

    #[test]
    fn ranges_cover_the_table_once() {
        let lab = item_lab("ranges");
        let db = open_reader(&lab.file).unwrap();
        assert!(leaf_scan_safe(&db).unwrap());
        // Too few rows to be worth cutting.
        assert_eq!(scan_ranges(&db, 12), [Range::All]);
        scatter(&lab, 3, 5000, 0, 50, 4);
        for parts in [2, 3, 12, 64] {
            let ranges = scan_ranges(&db, parts);
            assert_eq!(ranges.len(), parts, "SQLite walks a row-id range by row id");
            assert!(
                matches!(ranges[0], Range::Below(_)) && matches!(ranges[parts - 1], Range::From(_))
            );
            let mut edge = match ranges[0] {
                Range::Below(end) => end,
                other => panic!("{other:?}"),
            };
            for range in &ranges[1..] {
                match *range {
                    Range::Between(start, end) => {
                        assert!(start == edge && end > start);
                        edge = end;
                    }
                    Range::From(start) => assert_eq!(start, edge),
                    other => panic!("{other:?}"),
                }
            }
            // The parts added together rank as the whole scan does.
            let ids: Vec<String> = (0..24)
                .map(|n| format!("th{n}"))
                .chain((0..4).map(|n| format!("bare{n}")))
                .collect();
            let runs: Vec<String> = (0..24).map(|n| format!("r-th{n}")).collect();
            let bare: Vec<String> = (0..4).map(|n| format!("bare{n}")).collect();
            let keys = ScanKeys::new(&ids, &runs, &bare, "2026-11-12T09:30:00.000Z");
            let whole = rank(scan(&db, &keys, Range::All).unwrap(), tid_index(&ids));
            let mut parts = ranges
                .iter()
                .rev()
                .map(|range| scan(&db, &keys, *range).unwrap());
            let mut added = parts.next().unwrap();
            for part in parts {
                added.absorb(part);
            }
            let added = rank(added, tid_index(&ids));
            assert!(whole.rowids.len() > 3000 && whole.asst.is_some());
            assert_eq!(added.rowids, whole.rowids);
            assert_eq!(added.asst.is_some(), whole.asst.is_some());
        }
        // No rows at all, and a table without row ids to cut by.
        lab.writer
            .execute(&format!("DELETE FROM {PREFIX}turn_items"), [])
            .unwrap();
        assert_eq!(scan_ranges(&db, 12), [Range::All]);
    }

    #[test]
    fn ranges_and_chunks_read_what_one_reads() {
        // (rows, distinct milliseconds, distinct ordinals, recent sixteenths): no ties; sparse
        // ties; dense ties with equal full keys (the item list goes to the reference query);
        // more than 8,000 events in a few milliseconds (the event list does too).
        let cases = [
            (6000, 0, 50, 4),
            (7000, 3_000_000, 50, 2),
            (7000, 2500, 3, 4),
            (12000, 40, 2, 15),
        ];
        for (case, (rows, stamps, ordinals, recent)) in cases.into_iter().enumerate() {
            let lab = item_lab(&format!("split{case}"));
            scatter(&lab, 17 + case as u64, rows, stamps, ordinals, recent);
            if case == 1 {
                // A run id the scan cannot order, in one range only: the item list is set aside.
                lab.writer
                    .execute(&format!("UPDATE {PREFIX}turn_items SET run_id=x'00' WHERE rowid=4000 AND type='command_execution'"), [])
                    .unwrap();
            }
            let expected = single(&lab.file);
            assert!(
                expected.agents.iter().any(|a| a.estimated_asks > 0),
                "case {case}: the estimate has rows"
            );
            assert!(
                expected.agents.iter().map(|a| a.items.len()).sum::<usize>() > 1000,
                "case {case}"
            );
            for threads in [2, 3, 5] {
                for chunk in [None, Some(64), Some(7)] {
                    let mut trace = Trace {
                        chunk,
                        ..Trace::default()
                    };
                    let found = read(&lab.file, now(), threads, &mut trace)
                        .expect("several connections read it");
                    same(&found, &expected);
                    assert_eq!(
                        (trace.attempts, trace.proved, trace.failed),
                        (1, true, false)
                    );
                    assert_eq!(trace.parts, threads * SCAN_PARTS, "case {case}");
                    // Where the scan decides the lists, the fetch runs as many statements.
                    if chunk == Some(7) && case == 0 {
                        assert!(trace.chunks > 100, "{} statements", trace.chunks);
                    }
                }
            }
            same(&snapshot(&lab.file, now()).unwrap(), &expected);
        }
    }

    #[test]
    fn several_connections_read_what_one_reads() {
        for wal in [true, false] {
            let lab = lab("equal", wal);
            let before = std::fs::read(&lab.file).unwrap();
            let expected = single(&lab.file);
            assert!(
                expected.agents.len() >= 7,
                "the fixture reaches every query"
            );
            assert!(
                expected
                    .agents
                    .iter()
                    .filter(|a| a.estimated_asks > 0)
                    .count()
                    >= 6
            );
            for threads in 2..=7 {
                let mut trace = Trace::default();
                let found = read(&lab.file, now(), threads, &mut trace).unwrap();
                same(&found, &expected);
                assert_eq!(
                    (trace.attempts, trace.proved, trace.failed, trace.moved),
                    (1, true, false, false)
                );
                assert_eq!(trace.parts, 1, "a small table is one range");
            }
            same(&snapshot(&lab.file, now()).unwrap(), &expected);
            assert_eq!(std::fs::read(&lab.file).unwrap(), before);
        }
    }

    #[test]
    fn a_commit_between_pins_is_never_read_across() {
        let lab = lab("retry", true);
        let commits = Cell::new(0);
        // The workers hold the state before this commit; the calling connection would hold the one after.
        let once = |attempt: usize| {
            if attempt == 0 {
                commit(&lab.writer, 0);
                commits.set(commits.get() + 1);
            }
        };
        let mut trace = Trace {
            between_pins: Some(&once),
            ..Trace::default()
        };
        let found = read(&lab.file, now(), 4, &mut trace).unwrap();
        assert_eq!(commits.get(), 1);
        same(&found, &single(&lab.file));
        assert!(found.agents.iter().any(|a| a.id == "late0"));
        assert_eq!((trace.attempts, trace.proved), (2, true));

        // A commit between the pins of every attempt: nothing is read here at all.
        let always = |attempt: usize| commit(&lab.writer, 1 + attempt);
        let mut trace = Trace {
            between_pins: Some(&always),
            ..Trace::default()
        };
        assert!(read(&lab.file, now(), 4, &mut trace).is_none());
        assert_eq!(
            (trace.attempts, trace.proved, trace.failed),
            (PIN_ATTEMPTS, false, false)
        );
        same(&snapshot(&lab.file, now()).unwrap(), &single(&lab.file));
    }

    #[test]
    fn commits_during_reads_never_split_one() {
        // Another connection commits, as fast as it can, one transaction that writes the same
        // number into an item in the scan's first range and into one in its last, and adds an
        // item for th0 and a request for th1. In any one snapshot the two numbers are equal
        // and the two counts have grown by the same number; the rows are read by different
        // range scans and queries, here mostly on different connections.
        // Few enough items that none leaves the list of 3,000 while the writer adds more.
        let lab = item_lab("stress");
        scatter(&lab, 9, 2000, 0, 50, 4);
        let marker = |owner: usize, rowid: Option<i64>| {
            lab.writer
                .execute(
                    &format!("INSERT INTO {PREFIX}turn_items(rowid,thread_id,run_id,type,status,updated_at,ordinal,payload_json) VALUES(?1,?2,?3,'command_execution','completed','2026-11-12T12:29:00.000Z',7000,?4)"),
                    rusqlite::params![rowid, format!("th{owner}"), format!("r-th{owner}"), json!({"title":"0"}).to_string()],
                )
                .unwrap();
            lab.writer.last_insert_rowid()
        };
        lab.writer
            .execute(&format!("DELETE FROM {PREFIX}turn_items WHERE rowid=2"), [])
            .unwrap();
        // One near the first row id and one at the last: different ranges of the scan.
        let marks = [marker(5, Some(2)), marker(6, None)];
        assert!(marks[1] > 2000);
        let read_marks = |snapshot: &Snapshot| {
            let of = |id: &str| snapshot.agents.iter().find(|a| a.id == id).unwrap().clone();
            let mark = |id: &str| {
                of(id)
                    .items
                    .iter()
                    .find(|i| i.updated_at == "2026-11-12T12:29:00.000Z")
                    .unwrap()
                    .label
                    .clone()
            };
            (
                mark("th5"),
                mark("th6"),
                of("th0").items.len(),
                of("th1").requests.len(),
            )
        };
        let (_, _, items, requests) = read_marks(&single(&lab.file));
        let stop = AtomicBool::new(false);
        let (mut proved, mut retried, mut gave_up, mut grown) = (0, 0, 0, 0);
        thread::scope(|scope| {
            scope.spawn(|| {
                let writer = Connection::open(&lab.file).unwrap();
                writer.busy_timeout(Duration::from_secs(5)).unwrap();
                let mut step = 0;
                while !stop.load(Ordering::Relaxed) && step < 4000 {
                    step += 1;
                    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
                    for rowid in marks {
                        writer
                            .execute(&format!("UPDATE {PREFIX}turn_items SET payload_json=?1 WHERE rowid=?2"), rusqlite::params![json!({"title":step.to_string()}).to_string(), rowid])
                            .unwrap();
                    }
                    raw_item(&writer, 0, "command_execution", None, "2026-11-12T12:28:00.000Z", 8000 + step, "{}");
                    writer
                        .execute(&format!("INSERT INTO {PREFIX}runtime_requests VALUES('th1',NULL,'approval','pending',NULL)"), [])
                        .unwrap();
                    writer.execute_batch("COMMIT").unwrap();
                    thread::sleep(Duration::from_micros(300));
                }
            });
            for _ in 0..200 {
                let mut trace = Trace::default();
                match read(&lab.file, now(), 4, &mut trace) {
                    Some(found) => {
                        let (first, last, i, r) = read_marks(&found);
                        assert_eq!(first, last, "one snapshot across the scan's ranges");
                        assert_eq!(i - items, r - requests, "one snapshot across queries");
                        assert_eq!(first, (i - items).to_string());
                        assert!(trace.parts > 1);
                        grown = grown.max(i - items);
                        proved += 1;
                        retried += usize::from(trace.attempts > 1);
                    }
                    None => {
                        assert_eq!(
                            (trace.attempts, trace.proved, trace.failed),
                            (PIN_ATTEMPTS, false, false)
                        );
                        gave_up += 1;
                    }
                }
            }
            stop.store(true, Ordering::Relaxed);
        });
        eprintln!(
            "commits during reads: {proved} proved ({retried} after a retry), {gave_up} left to the single read, {grown} commits seen"
        );
        assert!(
            grown > 20 && proved > 100,
            "{grown} commits seen, {proved} proved"
        );
    }

    #[test]
    fn a_panic_on_the_calling_thread_does_not_leave_the_workers_waiting() {
        // The calling thread panics after the workers started and before it added the tasks
        // that wait for it: the workers finish the two they have and then wait. The panic must
        // reach the caller (as the single read's would) instead of `thread::scope` waiting for
        // them forever.
        let lab = lab("panic", true);
        let file = lab.file.clone();
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let boom = || panic!("injected");
            let mut trace = Trace {
                before_first: Some(&boom),
                ..Trace::default()
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                read(&file, now(), 4, &mut trace).is_some()
            }));
            let _ = done.send(outcome.is_err());
        });
        let panicked = finished
            .recv_timeout(Duration::from_secs(20))
            .expect("the read returned within 20 s");
        assert!(panicked, "the panic reached the caller");
    }

    /// What a read with `hook` on its workers did, on a thread of its own: `Some(message)`
    /// if it panicked, `None` if it returned, and an error if it did not return in 20 s.
    /// `hook` is also told whether it runs on the calling thread. With `pause`, the calling
    /// thread waits 300 ms after the workers pinned, so a worker that panics there is gone.
    fn panic_outcome(
        name: &str,
        pause: bool,
        hook: impl Fn(Stage, bool) + Send + Sync + 'static,
    ) -> Result<Option<String>, std::sync::mpsc::RecvTimeoutError> {
        let lab = item_lab(name);
        scatter(&lab, 4, 3000, 0, 50, 4);
        let file = lab.file.clone();
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let caller = thread::current().id();
            let worker: Arc<dyn Fn(Stage) + Send + Sync> =
                Arc::new(move |stage| hook(stage, thread::current().id() == caller));
            let wait = |_: usize| thread::sleep(Duration::from_millis(300));
            let mut trace = Trace {
                worker: Some(worker),
                ..Trace::default()
            };
            if pause {
                trace.between_pins = Some(&wait);
            }
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                read(&file, now(), 4, &mut trace).is_some()
            }));
            let _ = done.send(outcome.err().map(|panic| {
                panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_default()
            }));
        });
        let outcome = finished.recv_timeout(Duration::from_secs(20));
        drop(lab);
        outcome
    }
    /// A hook that panics when `when` holds, at most `times` times in all.
    fn boom(
        times: usize,
        when: impl Fn(Stage, bool) -> bool + Send + Sync + 'static,
    ) -> impl Fn(Stage, bool) + Send + Sync + 'static {
        let taken = std::sync::atomic::AtomicUsize::new(0);
        move |stage, caller| {
            if when(stage, caller) && taken.fetch_add(1, Ordering::SeqCst) < times {
                panic!("injected");
            }
        }
    }
    fn assert_panics(outcome: Result<Option<String>, std::sync::mpsc::RecvTimeoutError>) {
        let message = outcome.expect("the read returned within 20 s");
        assert_eq!(
            message.as_deref(),
            Some("injected"),
            "the worker's panic reached the caller"
        );
    }

    #[test]
    fn a_worker_panic_before_its_pin_leaves_no_thread_waiting() {
        // The calling thread waits for every worker's first message.
        assert_panics(panic_outcome(
            "wp-connect",
            false,
            boom(1, |s, _| s == Stage::Connected(3)),
        ));
    }
    #[test]
    fn a_worker_panic_during_its_pin_leaves_no_thread_waiting() {
        assert_panics(panic_outcome(
            "wp-pin",
            false,
            boom(1, |s, _| s == Stage::Pinning(3)),
        ));
    }
    #[test]
    fn a_worker_panic_after_its_pin_leaves_no_thread_waiting() {
        // The last worker is gone when the calling thread tells the workers to work.
        assert_panics(panic_outcome(
            "wp-pinned",
            true,
            boom(1, |s, _| s == Stage::Pinned(3)),
        ));
    }
    #[test]
    fn a_worker_panic_before_it_takes_work_leaves_no_thread_waiting() {
        assert_panics(panic_outcome(
            "wp-working",
            false,
            boom(1, |s, _| s == Stage::Working(3)),
        ));
    }
    #[test]
    fn a_worker_panic_in_any_stage_of_the_read_leaves_no_thread_waiting() {
        // The first queries, a range of the scan, a statement of the fetch, the estimates:
        // at each, other threads are running a query or waiting for the tasks this one adds.
        let stages: [(&str, fn(Task) -> bool); 6] = [
            ("wp-cards", |t| matches!(t, Task::Cards | Task::Projects)),
            ("wp-runs", |t| {
                matches!(t, Task::Runs | Task::Subs | Task::Turns)
            }),
            ("wp-scan", |t| matches!(t, Task::Scan(_))),
            ("wp-last-scan", |t| matches!(t, Task::Scan(n) if n >= 12)),
            ("wp-fetch", |t| matches!(t, Task::Fetch(_))),
            ("wp-estimates", |t| {
                matches!(t, Task::Estimates | Task::Fetch(_))
            }),
        ];
        for (name, at) in stages {
            let hook = boom(1, move |s, caller| {
                matches!(s, Stage::Query(task) if at(task)) && !caller
            });
            assert_panics(panic_outcome(name, false, hook));
        }
    }
    #[test]
    fn two_workers_panicking_at_once_leave_no_thread_waiting() {
        let two = |s| matches!(s, Stage::Connected(2 | 3));
        assert_panics(panic_outcome(
            "wp-two-connect",
            false,
            boom(2, move |s, _| two(s)),
        ));
        assert_panics(panic_outcome(
            "wp-two-query",
            false,
            boom(2, |s, caller| {
                matches!(s, Stage::Query(Task::Scan(_))) && !caller
            }),
        ));
        let pinning = |s| matches!(s, Stage::Pinning(1 | 3));
        assert_panics(panic_outcome(
            "wp-two-pin",
            false,
            boom(2, move |s, _| pinning(s)),
        ));
    }

    #[test]
    fn begin_pins_nothing_and_the_version_read_pins() {
        let lab = lab("pin", true);
        let threads = |db: &Connection| -> i64 {
            db.query_row(
                &format!("SELECT count(*) FROM {PREFIX}threads"),
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        // BEGIN alone: a commit after it is still seen.
        let db = connect(&lab.file, &lab.file, false).unwrap();
        let before = threads(&db);
        db.execute_batch("BEGIN").unwrap();
        commit(&lab.writer, 0);
        assert_eq!(threads(&db), before + 1);
        db.execute_batch("ROLLBACK").unwrap();
        // The calling connection's pin: BEGIN, then the version read. A commit after it is not
        // seen, and the version read reports the commit before it.
        let version = data_version(&db).unwrap();
        assert_eq!(data_version(&db), Some(version), "no commit, no change");
        commit(&lab.writer, 1);
        db.execute_batch("BEGIN").unwrap();
        assert_ne!(data_version(&db).unwrap(), version);
        commit(&lab.writer, 2);
        assert_eq!(threads(&db), before + 2);
        // A worker's pin likewise.
        let worker = connect(&lab.file, &lab.file, true).unwrap();
        assert!(pin(&worker, &lab.file));
        commit(&lab.writer, 3);
        assert_eq!((threads(&worker), threads(&db)), (before + 3, before + 2));
    }

    #[test]
    fn a_replaced_file_is_left_to_the_single_read() {
        // Another file renamed over the path after the calling connection opened: the workers
        // would open the new file, and the old one's version never changes.
        let renamed = lab("renamed", false);
        let replace = || {
            let copy = renamed.dir.join("copy.sqlite");
            std::fs::copy(&renamed.file, &copy).unwrap();
            commit(&Connection::open(&copy).unwrap(), 0);
            std::fs::rename(&copy, &renamed.file).unwrap();
        };
        let mut trace = Trace {
            after_open: Some(&replace),
            ..Trace::default()
        };
        assert!(read(&renamed.file, now(), 4, &mut trace).is_none());
        assert!(
            trace.moved || trace.attempts == 1,
            "the calling connection's file moved, or a worker's pin saw it"
        );
        assert!(!trace.proved);
        let found = snapshot(&renamed.file, now()).unwrap();
        assert!(found.agents.iter().any(|a| a.id == "late0"));
        same(&found, &single(&renamed.file));

        // T3's update rollback: an older copy written over the files in place, the WAL
        // index deleted. Same inode, so only the file's times and the index's identity tell.
        let restored = lab("restored", true);
        let kept = restored.dir.join("kept");
        std::fs::create_dir(&kept).unwrap();
        restored
            .writer
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        std::fs::copy(&restored.file, kept.join("db")).unwrap();
        commit(&restored.writer, 0);
        assert!(read(&restored.file, now(), 4, &mut Trace::default()).is_some());
        let restore = || {
            std::fs::copy(kept.join("db"), &restored.file).unwrap();
            std::fs::write(format!("{}-wal", restored.file.display()), b"").unwrap();
            std::fs::remove_file(format!("{}-shm", restored.file.display())).unwrap();
        };
        let mut trace = Trace {
            after_open: Some(&restore),
            ..Trace::default()
        };
        assert!(read(&restored.file, now(), 4, &mut trace).is_none());
        assert!(!trace.failed && !trace.proved);
    }

    #[test]
    fn a_waiting_writer_in_journal_mode_is_never_this_reads_error() {
        // The workers hold their read locks; a writer then waits to commit, and in rollback
        // mode that keeps new readers out, the calling connection among them.
        let lab = lab("pending", false);
        let writer = std::cell::RefCell::new(None);
        let file = lab.file.clone();
        let wait = |_: usize| {
            let file = file.clone();
            *writer.borrow_mut() = Some(thread::spawn(move || {
                let db = Connection::open(&file).unwrap();
                db.busy_timeout(Duration::from_secs(20)).unwrap();
                db.execute(&format!("INSERT INTO {PREFIX}runtime_requests VALUES('t1',NULL,'approval','pending',NULL)"), [])
                    .unwrap();
            }));
            thread::sleep(Duration::from_millis(150));
        };
        let before = single(&lab.file);
        let mut trace = Trace {
            between_pins: Some(&wait),
            ..Trace::default()
        };
        let found = read(&lab.file, now(), 4, &mut trace);
        // Either it could not pin and left the read to the single connection, or it pinned first.
        match found {
            None => assert_eq!(
                (trace.attempts, trace.proved, trace.failed),
                (1, false, false)
            ),
            Some(found) => same(&found, &before),
        }
        writer.borrow_mut().take().unwrap().join().unwrap();
        same(&snapshot(&lab.file, now()).unwrap(), &single(&lab.file));
    }

    #[test]
    fn a_rollback_journal_database_cannot_commit_between_pins() {
        let lab = lab("journal", false);
        let refused = Cell::new(0);
        let write = |_: usize| {
            lab.writer.busy_timeout(Duration::ZERO).unwrap();
            let error = lab
                .writer
                .execute(&format!("INSERT INTO {PREFIX}runtime_requests VALUES('t1',NULL,'approval','pending',NULL)"), [])
                .unwrap_err();
            assert_eq!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy)
            );
            refused.set(refused.get() + 1);
        };
        let mut trace = Trace {
            between_pins: Some(&write),
            ..Trace::default()
        };
        let found = read(&lab.file, now(), 3, &mut trace).unwrap();
        assert_eq!((refused.get(), trace.attempts, trace.proved), (1, 1, true));
        same(&found, &single(&lab.file));
    }

    #[test]
    #[cfg(unix)]
    fn a_read_only_index_is_left_to_the_single_read() {
        // A kept copy: the database, an empty log and the WAL index, all read-only. SQLite
        // reports a change on every read transaction there, so the version never holds still
        // and nothing else is taken as proof.
        use std::os::unix::fs::PermissionsExt;
        let lab = lab("frozen", true);
        commit(&lab.writer, 0);
        lab.writer
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let dir = lab.dir.join("frozen");
        std::fs::create_dir(&dir).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let to = dir.join(format!("synthetic.sqlite{suffix}"));
            std::fs::copy(format!("{}{suffix}", lab.file.display()), &to).unwrap();
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o444)).unwrap();
        }
        let file = dir.join("synthetic.sqlite");
        // Root ignores the mode, and then the index is not read-only.
        if std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join("synthetic.sqlite-shm"))
            .is_ok()
        {
            return;
        }
        let expected = single(&file);
        let probe = connect(&file, &file, false).unwrap();
        assert_ne!(data_version(&probe), data_version(&probe));
        for threads in [2, 4] {
            let mut trace = Trace::default();
            assert!(read(&file, now(), threads, &mut trace).is_none());
            // It is told apart before any worker starts.
            assert_eq!(
                (
                    trace.restless,
                    trace.attempts,
                    trace.proved,
                    trace.moved,
                    trace.failed
                ),
                (true, 0, false, false, false)
            );
        }
        same(&snapshot(&file, now()).unwrap(), &expected);
    }

    #[test]
    fn a_failing_query_reports_the_single_reads_error() {
        // Three later queries fail; the single read stops at the first of them in its order.
        let errors = lab("errors", true);
        errors
            .writer
            .execute_batch(&format!(
                "ALTER TABLE {PREFIX}provider_turns RENAME COLUMN started_at TO began_at;
                 ALTER TABLE {PREFIX}runtime_requests RENAME COLUMN status TO state;
                 ALTER TABLE {PREFIX}runs RENAME COLUMN requested_at TO asked_at;"
            ))
            .unwrap();
        let expected = snapshot_serial(&errors.file, now()).unwrap_err();
        assert!(
            expected.detail.contains("started_at"),
            "{}",
            expected.detail
        );
        for threads in [2, 4, 7] {
            let mut trace = Trace::default();
            assert!(read(&errors.file, now(), threads, &mut trace).is_none());
            assert!(trace.failed && trace.proved);
        }
        let found = snapshot(&errors.file, now()).unwrap_err();
        assert_eq!(
            (found.kind, found.to_string()),
            (expected.kind, expected.to_string())
        );

        // A row the reader rejects after every query succeeded: the same.
        let rejected = lab("rejected", true);
        rejected.writer
            .execute(&format!("UPDATE {PREFIX}turn_items SET status=NULL WHERE type='command_execution' AND thread_id='t2'"), [])
            .unwrap();
        let expected = snapshot_serial(&rejected.file, now()).unwrap_err();
        let mut trace = Trace::default();
        assert!(read(&rejected.file, now(), 3, &mut trace).is_none());
        assert!(trace.failed && trace.proved);
        let found = snapshot(&rejected.file, now()).unwrap_err();
        assert_eq!(
            (found.kind, found.to_string()),
            (expected.kind, expected.to_string())
        );
    }

    #[test]
    fn the_first_queries_decide_alone() {
        // No table: the single read reports it.
        let unknown = path("fan-unknown");
        Connection::open(&unknown)
            .unwrap()
            .execute_batch("CREATE TABLE untouched(value TEXT)")
            .unwrap();
        let expected = snapshot_serial(&unknown, now()).unwrap_err();
        assert!(
            expected.detail.starts_with("missing "),
            "{}",
            expected.detail
        );
        for _ in 0..20 {
            assert!(read(&unknown, now(), 4, &mut Trace::default()).is_none());
            assert_eq!(
                snapshot(&unknown, now()).unwrap_err().to_string(),
                expected.to_string()
            );
        }
        std::fs::remove_file(unknown).unwrap();
        // No active thread: an empty snapshot, and nothing else decides.
        let lab = lab("fan-empty", true);
        lab.writer
            .execute(&format!("UPDATE {PREFIX}threads SET deleted_at='x'"), [])
            .unwrap();
        lab.writer
            .execute_batch(&format!(
                "ALTER TABLE {PREFIX}provider_turns RENAME COLUMN started_at TO began_at"
            ))
            .unwrap();
        let mut trace = Trace::default();
        // A worker's own first query may fail first; then the single read returns the same.
        if let Some(found) = read(&lab.file, now(), 4, &mut trace) {
            assert!(found.agents.is_empty() && !trace.failed);
        }
        assert!(snapshot(&lab.file, now()).unwrap().agents.is_empty());
        // A missing file is the caller's to report.
        assert!(
            read(
                &lab.dir.join("absent.sqlite"),
                now(),
                4,
                &mut Trace::default()
            )
            .is_none()
        );
        assert_eq!(
            snapshot(&lab.dir.join("absent.sqlite"), now())
                .unwrap_err()
                .kind,
            "missing"
        );
    }

    #[test]
    fn cross_range_malformed_guards_match_single_scan() {
        let ids: Vec<String> = (0..24)
            .map(|n| format!("th{n}"))
            .chain((0..4).map(|n| format!("bare{n}")))
            .collect();
        let runs: Vec<String> = (0..24).map(|n| format!("r-th{n}")).collect();
        let bare: Vec<String> = (0..4).map(|n| format!("bare{n}")).collect();
        let check = |name: &str,
                     mutate: &dyn Fn(&Connection),
                     expect_decided: (bool, bool, bool, bool)| {
            let lab = item_lab(name);
            scatter(&lab, 41, 4000, 0, 50, 4);
            mutate(&lab.writer);
            let db = open_reader(&lab.file).unwrap();
            let keys = ScanKeys::new(&ids, &runs, &bare, "2026-11-12T09:30:00.000Z");
            let ranges = scan_ranges(&db, 12);
            assert_eq!(ranges.len(), 12);
            let whole = rank(scan(&db, &keys, Range::All).unwrap(), tid_index(&ids));
            let mut parts = ranges
                .iter()
                .rev()
                .map(|range| scan(&db, &keys, *range).unwrap());
            let mut merged = parts.next().unwrap();
            for part in parts {
                merged.absorb(part);
            }
            let merged = rank(merged, tid_index(&ids));
            assert_eq!(whole.decided(), expect_decided, "{name}: single decided");
            assert_eq!(
                merged.decided(),
                whole.decided(),
                "{name}: split decided matches single"
            );
            assert_eq!(merged.rowids, whole.rowids, "{name}: rowids match");
            let expected = single(&lab.file);
            let mut trace = Trace::default();
            let found = read(&lab.file, now(), 3, &mut trace).expect("fan-out succeeded");
            assert_eq!(
                (trace.attempts, trace.proved, trace.failed),
                (1, true, false)
            );
            same(&found, &expected);
        };

        // 1. Non-integer ordinal in an early range + BLOB run_id in a late range:
        //    items falls back, while events/plans/asst remain decided identically in both paths.
        check(
            "guard-ord-and-blob-run",
            &|db| {
                db.execute(
                    &format!("INSERT OR REPLACE INTO {PREFIX}turn_items(rowid,thread_id,run_id,type,status,updated_at,ordinal,payload_json) VALUES(500,'th0','r-th0','command_execution','completed','2026-11-12T12:10:00.000Z','not-an-int','{{}}')"),
                    [],
                )
                .unwrap();
                db.execute(
                    &format!("INSERT OR REPLACE INTO {PREFIX}turn_items(rowid,thread_id,run_id,type,status,updated_at,ordinal,payload_json) VALUES(5000,'th1',x'00','command_execution','completed','2026-11-12T12:11:00.000Z',1,'{{}}')"),
                    [],
                )
                .unwrap();
            },
            (false, true, true, true),
        );

        // 2. Non-integer ordinal on assistant_message (inactive run) in an early range +
        //    BLOB thread_id (older than event window) on assistant_message in a late range:
        //    only asst falls back, identically in single and split scans.
        check(
            "guard-asst-ord-and-blob-tid",
            &|db| {
                db.execute(
                    &format!("INSERT OR REPLACE INTO {PREFIX}turn_items(rowid,thread_id,run_id,node_id,type,status,updated_at,ordinal,payload_json) VALUES(500,'th0','r-old','n-a','assistant_message','completed','2026-11-12T08:10:00.000Z','not-an-int','{{}}')"),
                    [],
                )
                .unwrap();
                db.execute(
                    &format!("INSERT OR REPLACE INTO {PREFIX}turn_items(rowid,thread_id,run_id,node_id,type,status,updated_at,ordinal,payload_json) VALUES(5000,x'00','r-old','n-b','assistant_message','completed','2026-11-12T08:11:00.000Z',1,'{{}}')"),
                    [],
                )
                .unwrap();
            },
            (true, true, true, false),
        );

        // 3. BLOB updated_at in a late range sets the whole scan aside in both paths.
        check(
            "guard-blob-updated-at",
            &|db| {
                db.execute(
                    &format!("INSERT OR REPLACE INTO {PREFIX}turn_items(rowid,thread_id,run_id,type,status,updated_at,ordinal,payload_json) VALUES(5000,'old','r-old','command_execution','completed',x'00',1,'{{}}')"),
                    [],
                )
                .unwrap();
            },
            (false, false, false, false),
        );
    }

    #[test]
    fn leaf_scan_handles_trailing_columns_like_migration_095() {
        // 1. Checked-in T3 schema 096 (`inline_work_output` after `payload_json`) stays leaf-scan safe.
        let mem = Connection::open_in_memory().unwrap();
        mem.execute_batch(include_str!(
            "../../../../checks/fixtures/t3-schema-main-096.sql"
        ))
        .unwrap();
        assert!(
            leaf_scan_safe(&mem).unwrap(),
            "schema 096 with inline_work_output after payload_json must use the leaf scan"
        );

        // 2. A table where a scanned column follows `payload_json` is rejected.
        let bad = Connection::open_in_memory().unwrap();
        bad.execute_batch(&format!(
            "CREATE TABLE {PREFIX}turn_items(thread_id TEXT,run_id TEXT,node_id TEXT,provider_thread_id TEXT,type TEXT,status TEXT,updated_at TEXT,payload_json TEXT,ordinal INTEGER);"
        ))
        .unwrap();
        assert!(
            !leaf_scan_safe(&bad).unwrap(),
            "scanned column `ordinal` after `payload_json` must disable the leaf scan"
        );

        // 3. A table where only a fetched-only column follows `payload_json` stays safe.
        let ok_fetched = Connection::open_in_memory().unwrap();
        ok_fetched
            .execute_batch(&format!(
                "CREATE TABLE {PREFIX}turn_items(thread_id TEXT,run_id TEXT,node_id TEXT,type TEXT,updated_at TEXT,ordinal INTEGER,payload_json TEXT,provider_thread_id TEXT,status TEXT);"
            ))
            .unwrap();
        assert!(
            leaf_scan_safe(&ok_fetched).unwrap(),
            "non-scanned columns after `payload_json` keep the leaf scan safe"
        );

        // 4. End-to-end fan-out on a WAL DB with `inline_work_output` added after `payload_json`
        //    and large overflowing payloads: runs all 12 ranges and matches serial read.
        let lab = item_lab("m095-fan");
        scatter(&lab, 73, 4500, 0, 50, 4);
        lab.writer
            .execute_batch(&format!(
                "ALTER TABLE {PREFIX}turn_items ADD COLUMN inline_work_output INTEGER NOT NULL DEFAULT 0;
                 UPDATE {PREFIX}turn_items SET inline_work_output = 1 WHERE rowid % 7 = 0;"
            ))
            .unwrap();
        let db = open_reader(&lab.file).unwrap();
        assert!(leaf_scan_safe(&db).unwrap());
        let expected = single(&lab.file);
        let mut trace = Trace::default();
        let found = read(&lab.file, now(), 3, &mut trace).expect("fan-out succeeded on 095 layout");
        assert_eq!(
            (trace.attempts, trace.proved, trace.failed, trace.parts),
            (1, true, false, 3 * SCAN_PARTS)
        );
        same(&found, &expected);
    }
}
