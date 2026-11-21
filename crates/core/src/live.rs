//! Kept-open, read-only snapshot reader, with conservative full-read fallback.
//!
//! Privacy: source.rs's column/JSON allowlist, plus approved projection rowids.
//! No event payload, prompt, message body, result or reasoning is returned by a
//! query. The estimated ask function sees eligible assistant text within SQL.
//! The event feed reads only sequence/thread_id (stream id, kind and version in the
//! real log), projection metadata watermarks, and the number of rows in T3's
//! migration ledger, no column of it.
//! Verified suffixes assemble from the typed cache, with conservative fallback
//! for unproved SQL encounter orders. Forward
//! time reuses the snapshot until the first possible three-hour SQL cutoff change;
//! backwards time always rereads.
use crate::{
    model::{Changed, Snapshot},
    source::{self, SourceError},
};
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn malloc_trim(pad: usize) -> i32;
}

mod poll1;

/// An agent reduced to what the bar's status line reads (`status::status`):
/// title, status, model, provider id and the run's `provider`, context tokens,
/// start time, requests, events (time and failure) and the current item's
/// kind and tool, plus the identity and structure fields every agent keeps.
/// Items, recent failures, history, spans, plan, the latest turn, options and
/// the category mix go, and the current item keeps no label, command or file.
/// None of what goes reads anything the status line reads; what stays is
/// computed before it goes.
pub fn status_view(agent: &mut crate::model::Agent) {
    agent.items = Vec::new();
    agent.recent_failures = Vec::new();
    agent.history = Vec::new();
    agent.spans = Vec::new();
    agent.plan = None;
    agent.turn = serde_json::Value::Null;
    agent.options = Vec::new();
    agent.mix = Default::default();
    agent.run = match agent.run.get("provider") {
        Some(provider) => serde_json::json!({ "provider": provider }),
        None => serde_json::json!({}),
    };
    if let Some(now) = agent.now.as_mut() {
        now.label = String::new();
        now.command = String::new();
        now.file = String::new();
    }
}
fn view_snapshot(snapshot: &mut Snapshot) {
    for agent in &mut snapshot.agents {
        status_view(Arc::make_mut(agent));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Served {
    Skipped,
    Incremental,
    Full,
}
/// First rule that prevented a full poll from using cached assembly.
/// None on skipped/incremental polls. Later blockers may be masked by this one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    InitialBaseline,
    DatabaseReplaced,
    SchemaChanged,
    EnvironmentChanged,
    BackwardsClock,
    WindowExpired,
    FeedUnavailable,
    InvalidMetadata,
    MissingMetadata,
    InvalidWatermark,
    InvalidEventTip,
    WatermarkMismatch,
    FeedBaseline,
    CursorRegression,
    /// The number of rows in T3's migration ledger changed. A migration can rewrite
    /// thread and project rows with no event; one committed with an ordinary write
    /// between two polls would otherwise leave the rows it rewrote stale.
    LedgerChanged,
    PruningMarkerChanged,
    MissingAnchor,
    InvalidAnchor,
    /// The event the previous poll saw last now names something else at the same
    /// sequence: the log was replaced, as by an older copy of the file restored in place.
    AnchorReplaced,
    EventGap,
    /// The log held an event that names something other than a thread, such as a project.
    NonThreadEvent,
    NoNewEvents,
    CacheUnavailable,
    ThreadScanPlan,
    /// A subagent carries an empty provider id. The oracle's JSON router
    /// (history, events, plans) routes it; the typed item router does not.
    CrossThreadAttribution,
    /// A subagent id equals an active thread id, so the oracle replaces that
    /// thread's agent in place.
    SubagentThreadCollision,
    /// An active thread row's payload id is missing or differs from its
    /// thread_id column, which keys the cache.
    ThreadIdentityMismatch,
    RunOwnerMismatch,
    RunOrdinalTie,
    MultipleSubagents,
    DuplicateProviderId,
    HistoryModelType,
    ProviderIdentityMismatch,
    ProviderOrdinalTie,
    DuplicateNodeId,
    MultipleRequests,
    ItemTimestampOrdinalTie,
    EventTimestampTie,
    PlanTimestampTie,
    MultipleSpans,
    AssemblyError,
    /// A selected row's allowlisted keys disagreed with the by-rowid text read,
    /// or that read failed. The staged keys stay; this poll uses the oracle.
    TextDrift,
}
impl FallbackReason {
    /// Link shapes the cache's whole-snapshot assembly is not proved for
    /// either. These polls read in full rather than assemble from the cache.
    fn refuses_cached_assembly(self) -> bool {
        matches!(
            self,
            Self::CrossThreadAttribution | Self::SubagentThreadCollision | Self::ThreadIdentityMismatch
        )
    }
}
#[derive(Clone, Debug, Default)]
pub struct Counters {
    pub fallback_reason: Option<FallbackReason>,
    /// Materialized projection/schema rows; excludes scalar PRAGMA probes.
    pub rows_read: usize,
    pub threads_reread: usize,
    /// Rows staged for the cache; included in rows_read.
    pub cache_rows_read: usize,
    /// Successful cache staging; false on optional cache/schema/type failure.
    pub cache_ready: bool,
    /// Whether this poll rebuilt the entire row cache rather than changed owners.
    pub cache_full: bool,
    /// Feed rows inspected, separate from oracle projection-row accounting.
    pub event_rows_read: usize,
    /// Distinct changed thread ids from a verified contiguous event suffix.
    pub changed_threads: usize,
    /// A usable event suffix, not a claim that assembly was incremental.
    pub feed_complete: bool,
    pub probe: Duration,
    pub read: Duration,
    pub reconcile: Duration,
    pub self_check: Duration,
    pub total: Duration,
    pub mismatches: u64,
    /// Opt-in phase data captured before the self-check (never includes canary work).
    pub phases: Option<source::timing::Data>,
}
#[derive(Clone, Debug)]
pub struct Update {
    pub snapshot: Snapshot,
    pub changed: Changed,
    pub served: Served,
    pub counters: Counters,
    /// A canary discrepancy is delivered explicitly, never silently repaired.
    pub mismatch: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Identity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    created: Option<std::time::SystemTime>,
}
fn identity(path: &Path) -> Result<Identity, SourceError> {
    let m = std::fs::metadata(path).map_err(|e| error(path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Identity {
            device: m.dev(),
            inode: m.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(Identity {
            created: m.created().ok(),
        })
    }
}
/// The identity of the WAL and shared-memory files beside the database, `None` where one
/// does not exist. SQLite never replaces either while a connection is open on the file.
/// T3's update rollback can: it copies a snapshot over the live files, and where the
/// snapshot was taken while T3 ran it deletes `-shm` (and `-wal`, if the snapshot has
/// none). A kept-open connection would go on reading the deleted file, see no change,
/// and skip every poll from then on.
#[derive(Clone, Debug, PartialEq)]
struct Sidecars {
    wal: Option<Identity>,
    shm: Option<Identity>,
}
fn sidecars(path: &Path) -> Sidecars {
    let beside = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        identity(Path::new(&name)).ok()
    };
    Sidecars {
        wal: beside("-wal"),
        shm: beside("-shm"),
    }
}
fn error(path: &Path, detail: impl ToString) -> SourceError {
    if !path.exists() {
        SourceError {
            kind: "missing",
            path: path.into(),
            detail: String::new(),
        }
    } else if path.is_dir() {
        SourceError {
            kind: "unreadable",
            path: path.into(),
            detail: "it is a folder, not a file".into(),
        }
    } else {
        SourceError {
            kind: "unreadable",
            path: path.into(),
            detail: detail.to_string(),
        }
    }
}
fn sql_error(path: &Path, err: rusqlite::Error) -> SourceError {
    if !path.exists() {
        return SourceError {
            kind: "missing",
            path: path.into(),
            detail: String::new(),
        };
    }
    if path.is_dir() {
        return SourceError {
            kind: "unreadable",
            path: path.into(),
            detail: "it is a folder, not a file".into(),
        };
    }
    if matches!(&err, rusqlite::Error::SqliteFailure(e, _) if e.extended_code == 26) {
        return SourceError {
            kind: "unreadable",
            path: path.into(),
            detail: "not a SQLite database".into(),
        };
    }
    SourceError {
        kind: "unreadable",
        path: path.into(),
        detail: err.to_string(),
    }
}
fn open(path: &Path) -> Result<Connection, SourceError> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| sql_error(path, e))?;
    db.busy_timeout(Duration::from_millis(250))
        .map_err(|e| sql_error(path, e))?;
    crate::asks::register(&db).map_err(|e| sql_error(path, e))?;
    // The scale databases are hundreds of megabytes of unread payload padding.
    // Leave them on disk: a mapped file stays in this process's resident set
    // after the baseline scan, and the parsed cache is much smaller.
    // A changed poll re-reads a thread through its indexes and mostly touches
    // payload pages that a small page cache cannot hold. Keep that cache small
    // to leave more memory for the parsed rows.
    db.execute_batch("PRAGMA query_only=ON; PRAGMA mmap_size=0; PRAGMA cache_size=-500")
        .map_err(|e| sql_error(path, e))?;
    // The same projections are prepared on every changed poll.
    db.set_prepared_statement_cache_capacity(64);
    Ok(db)
}
/// Read once, which opens (or creates) the WAL index, then say what lies beside the file.
/// A file SQLite cannot read yet fails later, in a poll, as it always did.
fn settle(db: &Connection, path: &Path) -> Sidecars {
    let _ = scalar(db, "PRAGMA schema_version", path);
    sidecars(path)
}
fn scalar(db: &Connection, sql: &str, path: &Path) -> Result<i64, SourceError> {
    db.prepare_cached(sql)
        .and_then(|mut stmt| stmt.query_row([], |row| row.get(0)))
        .map_err(|e| sql_error(path, e))
}

const EVENT_SUFFIX: &str =
    "SELECT sequence, thread_id FROM orchestration_v2_events WHERE sequence > ? ORDER BY sequence";
const EVENT_TIP: &str =
    "SELECT sequence, thread_id FROM orchestration_v2_events ORDER BY sequence DESC LIMIT 1";
const EVENT_ANCHOR: &str =
    "SELECT sequence, thread_id FROM orchestration_v2_events WHERE sequence = ?";
// The log a real installation appends to. One sequence covers every aggregate, so each
// row also says what kind of thing it names: only version-2 thread events name a thread.
const APPLICATION_SUFFIX: &str = "SELECT sequence, stream_id, aggregate_kind, application_event_version FROM orchestration_events WHERE sequence > ? ORDER BY sequence";
const APPLICATION_TIP: &str =
    "SELECT sequence, stream_id FROM orchestration_events ORDER BY sequence DESC LIMIT 1";
const APPLICATION_ANCHOR: &str =
    "SELECT sequence, stream_id FROM orchestration_events WHERE sequence = ?";
const METADATA: &str =
    "SELECT projection_name, last_sequence FROM orchestration_v2_projection_metadata";
/// The number of rows in T3's migration ledger, and nothing else of it: no column
/// is read (SQLite's authorizer sees this table with an empty column name).
const LEDGER: &str = "SELECT COUNT(*) FROM effect_sql_migrations";

/// T3's schema has two event tables. A real installation, and the scale lab's
/// t3-schema tiers, append every event to `orchestration_events` and leave
/// `orchestration_v2_events` empty. The tests still build the v2 log. Neither is
/// assumed: the log in use is the one whose newest sequence equals the projection's
/// cursor, checked again on every poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Log {
    Application,
    V2,
}
impl Log {
    fn tip(self) -> &'static str {
        match self {
            Log::Application => APPLICATION_TIP,
            Log::V2 => EVENT_TIP,
        }
    }
    fn anchor(self) -> &'static str {
        match self {
            Log::Application => APPLICATION_ANCHOR,
            Log::V2 => EVENT_ANCHOR,
        }
    }
    fn suffix(self) -> &'static str {
        match self {
            Log::Application => APPLICATION_SUFFIX,
            Log::V2 => EVENT_SUFFIX,
        }
    }
}

#[derive(Clone, Debug)]
struct Feed {
    log: Log,
    cursor: i64,
    /// The log's newest sequence: the cursor, or past it by project events only,
    /// which T3 appends without moving the cursor (with a readable ledger only).
    tip: i64,
    /// What the event at `tip` names. The same sequence naming something else on a
    /// later poll means the log is not the one read here: T3 restores an older copy
    /// of the database in place when an update fails, and then appends again.
    named: String,
    metadata: BTreeMap<String, i64>,
    /// Rows in T3's migration ledger. `None` when the table is not there or the
    /// count could not be read; then no rule below that relies on it applies.
    ledger: Option<i64>,
}
#[derive(Debug)]
struct FeedRead {
    next: Feed,
    threads: BTreeSet<String>,
    rows: usize,
    /// Events were appended since the previous poll, all accounted for: version-2
    /// thread events up to the cursor, and project events (with a readable ledger).
    complete: bool,
    /// Nothing was appended, and the cursor, the tip and what it names, the prune
    /// marks and the ledger count are what they were, with a ledger to count. A
    /// commit like that changed no table the dashboard reads (t3-write-paths.md).
    quiet: bool,
}
/// What an event names, as far as the dashboard is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Named {
    /// A version-2 thread event: its stream is a thread whose rows can be re-read.
    Thread,
    /// A version-2 project event. T3 applies it to `projection_projects` alone,
    /// and that table is read in full on every changed poll.
    Project,
    Other,
}
fn named(log: Log, row: &rusqlite::Row) -> rusqlite::Result<Named> {
    Ok(match log {
        Log::V2 => Named::Thread,
        Log::Application => match (row.get::<_, String>(2)?.as_str(), row.get::<_, i64>(3)?) {
            ("thread", 2) => Named::Thread,
            ("project", 2) => Named::Project,
            _ => Named::Other,
        },
    })
}
/// Rows in T3's migration ledger, or `None` when there is no such table or its
/// count cannot be read. T3's migrator (`effect`'s `sql/Migrator`) adds one row per
/// migration in the same transaction as the migration's own writes.
fn ledger_count(db: &Connection) -> Option<i64> {
    db.prepare_cached(LEDGER)
        .and_then(|mut statement| statement.query_row([], |row| row.get(0)))
        .ok()
}
/// Newest sequence of one log and what it names: `Ok(None)` when that table is not
/// in this database, `Err(reason)` when its newest row is not a usable event.
fn log_tip(
    db: &Connection,
    log: Log,
) -> rusqlite::Result<Result<Option<(i64, String)>, FallbackReason>> {
    let Ok(mut tip) = db.prepare_cached(log.tip()) else {
        return Ok(Ok(None));
    };
    let mut rows = tip.query([])?;
    Ok(match rows.next()? {
        Some(row) => {
            let sequence: i64 = row.get(0)?;
            let named: String = row.get(1)?;
            if sequence <= 0 || named.is_empty() {
                Err(FallbackReason::InvalidEventTip)
            } else {
                Ok(Some((sequence, named)))
            }
        }
        None => Ok(Some((0, String::new()))),
    })
}
/// Whether the events after the cursor, up to the tip, are a gap-free run of project
/// events. T3 logs a project change without moving the projection's cursor
/// (`commitProjectCommand`), so this is a committed state, not a torn one.
fn project_tail(db: &Connection, cursor: i64, tip: i64, rows: &mut usize) -> rusqlite::Result<bool> {
    let mut statement = db.prepare_cached(APPLICATION_SUFFIX)?;
    let mut found = statement.query([cursor])?;
    let mut expected = cursor;
    while let Some(row) = found.next()? {
        *rows += 1;
        let sequence: i64 = row.get(0)?;
        let stream: String = row.get(1)?;
        if expected.checked_add(1) != Some(sequence)
            || stream.is_empty()
            || named(Log::Application, row)? != Named::Project
        {
            return Ok(false);
        }
        expected = sequence;
    }
    Ok(expected == tip)
}
/// Called only within the same pinned transaction as the projection read.
/// Missing/incompatible feed tables are optional: callers retain the oracle path.
fn read_feed(
    db: &Connection,
    previous: Option<&Feed>,
    reason: &mut Option<FallbackReason>,
) -> rusqlite::Result<Option<FeedRead>> {
    let mut metadata = BTreeMap::new();
    let mut statement = db.prepare_cached(METADATA)?;
    for row in statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (name, sequence) = row?;
        if sequence < 0 || metadata.insert(name, sequence).is_some() {
            *reason = Some(FallbackReason::InvalidMetadata);
            return Ok(None);
        }
    }
    let Some(&cursor) = metadata.get("thread-projections") else {
        *reason = Some(FallbackReason::MissingMetadata);
        return Ok(None);
    };
    // Treat every other metadata entry conservatively as a potential pruning
    // watermark. No knowledge of a particular bundle's marker names is needed.
    if metadata.values().any(|&sequence| sequence > cursor) {
        *reason = Some(FallbackReason::InvalidWatermark);
        return Ok(None);
    }
    let ledger = ledger_count(db);
    // The log in use is the one the projection has consumed to its tip. The real one is
    // tried first; an empty database matches either and has nothing to tell them apart.
    // With a ledger to count, the real log may also run past the cursor by project
    // events, which T3 appends without moving it.
    let mut tips = Vec::new();
    for log in [Log::Application, Log::V2] {
        match log_tip(db, log)? {
            Ok(None) => {}
            Ok(Some((head, named))) => tips.push((log, head, named)),
            Err(invalid) => {
                *reason = Some(invalid);
                return Ok(None);
            }
        }
    }
    let present = tips.len();
    let mut rows = 0;
    // A log whose tip is the cursor wins; only then may the real one run past it.
    let mut found = tips.iter().position(|(_, head, _)| *head == cursor);
    if found.is_none() && ledger.is_some() {
        for (index, (log, head, _)) in tips.iter().enumerate() {
            if *log == Log::Application && *head > cursor && project_tail(db, cursor, *head, &mut rows)? {
                found = Some(index);
            }
        }
    }
    let found = found.map(|index| tips.swap_remove(index));
    if present == 0 {
        // No event table at all: surface the same error an absent table always did.
        db.prepare_cached(EVENT_TIP)?;
    }
    let Some((log, tip, named_at_tip)) = found else {
        *reason = Some(FallbackReason::WatermarkMismatch);
        return Ok(None);
    };
    let mut result = FeedRead {
        next: Feed {
            log,
            cursor,
            tip,
            named: named_at_tip,
            metadata,
            ledger,
        },
        threads: BTreeSet::new(),
        rows: rows + usize::from(tip > 0),
        complete: false,
        quiet: false,
    };
    let Some(previous) = previous else {
        *reason = Some(FallbackReason::FeedBaseline);
        return Ok(Some(result));
    };
    // The two logs number their events independently. A sequence from one says nothing
    // about the other, except that an empty log's zero is the start of either.
    if previous.log != log && previous.tip > 0 {
        *reason = Some(FallbackReason::FeedBaseline);
        return Ok(Some(result));
    }
    // T3 never moves either backwards. A restored older copy of the file does.
    if cursor < previous.cursor || tip < previous.tip {
        *reason = Some(FallbackReason::CursorRegression);
        return Ok(Some(result));
    }
    // A migration adds its ledger row in the same transaction as its own writes, which
    // can rewrite thread and project rows with no event. Any change, either way, is
    // read in full: a restored older file brings the count back down.
    if ledger != previous.ledger {
        *reason = Some(FallbackReason::LedgerChanged);
        return Ok(Some(result));
    }
    // Changes to unknown marker rows are uncertain, even if their new value is
    // below the cursor. Establish a fresh baseline with a full read.
    let markers = |f: &Feed| {
        f.metadata
            .iter()
            .filter(|(name, _)| name.as_str() != "thread-projections")
            .map(|(name, value)| (name.clone(), *value))
            .collect::<BTreeMap<_, _>>()
    };
    if markers(&result.next) != markers(previous)
        || result
            .next
            .metadata
            .iter()
            .any(|(name, &sequence)| name != "thread-projections" && sequence > previous.cursor)
    {
        *reason = Some(FallbackReason::PruningMarkerChanged);
        return Ok(Some(result));
    }
    // The newest event the previous poll saw must still be there and name the same
    // thing. Every event after it is then read below.
    if previous.tip > 0 {
        let mut anchor = db.prepare_cached(log.anchor())?;
        let mut rows = anchor.query([previous.tip])?;
        let Some(row) = rows.next()? else {
            *reason = Some(FallbackReason::MissingAnchor);
            return Ok(Some(result));
        };
        let _: i64 = row.get(0)?;
        let named: String = row.get(1)?;
        result.rows += 1;
        if named.is_empty() {
            *reason = Some(FallbackReason::InvalidAnchor);
            return Ok(Some(result));
        }
        if named != previous.named {
            *reason = Some(FallbackReason::AnchorReplaced);
            return Ok(Some(result));
        }
    }
    let mut statement = db.prepare_cached(log.suffix())?;
    let mut expected = previous.cursor;
    let mut rows = statement.query([previous.cursor])?;
    while let Some(row) = rows.next()? {
        let sequence: i64 = row.get(0)?;
        let stream: String = row.get(1)?;
        let kind = named(log, row)?;
        result.rows += 1;
        if expected.checked_add(1) != Some(sequence) || stream.is_empty() {
            *reason = Some(FallbackReason::EventGap);
            return Ok(Some(result));
        }
        // Only a version-2 thread event names a thread whose rows can be re-read. A
        // project event changes `projection_projects` alone, which a changed poll reads
        // in full; that is relied on only with a ledger to count. Anything else, or
        // anything this reader does not know, changes something else.
        match kind {
            Named::Thread => {
                result.threads.insert(stream);
            }
            Named::Project if ledger.is_some() => {}
            _ => {
                *reason = Some(FallbackReason::NonThreadEvent);
                return Ok(Some(result));
            }
        }
        expected = sequence;
    }
    // A commit without an event can contain a project update or an untracked
    // projection edit. Never claim a usable suffix for such a commit. With a ledger to
    // count, the only such edits are migrations, and the count has not moved.
    result.complete = expected == tip && tip > previous.tip;
    result.quiet = expected == tip
        && tip == previous.tip
        && cursor == previous.cursor
        && ledger.is_some();
    if !result.complete {
        *reason = Some(FallbackReason::NoNewEvents);
    }
    Ok(Some(result))
}

// Row-cache checkpoint: populated alongside the oracle, not used for assembly
// until global ordering/caps and selective attribution have been proved.
#[allow(dead_code)]
mod cache {
    use super::*;
    use rusqlite::types::Value as SqlValue;
    use serde_json::{Value, json};
    use std::{
        borrow::Cow,
        cmp::Ordering,
        cmp::Reverse,
        collections::HashSet,
        hash::{Hash, Hasher},
        sync::OnceLock,
    };

    pub(super) const TABLES: [&str; 8] = [
        "threads",
        "runs",
        "subagents",
        "provider_threads",
        "provider_turns",
        "nodes",
        "runtime_requests",
        "turn_items",
    ];

    #[derive(Clone, Debug)]
    pub(super) struct Payload {
        pub(super) payload: Value,
    }
    #[derive(Clone, Debug)]
    pub(super) struct Thread {
        pub(super) data: Payload,
        pub(super) archived_at: Option<String>,
        pub(super) deleted_at: Option<String>,
    }
    #[derive(Clone, Debug)]
    pub(super) struct Run {
        pub(super) data: Payload,
        pub(super) ordinal: i64,
        pub(super) requested_at: String,
        pub(super) completed_at: Option<String>,
    }
    /// Canonical ISO-8601 stamps sort like their millisecond value and are stored
    /// as that integer. Anything else keeps the original text. A canonical stamp
    /// is never stored as `Raw`, so the two variants do not alias. `Raw` holds a
    /// thin pointer, so a stamp is 16 bytes on every turn item.
    #[derive(Clone, Debug)]
    pub(super) enum Stamp {
        Ms(i64),
        Raw(Arc<String>),
    }
    impl Stamp {
        pub(super) fn parse(text: &str) -> Self {
            if let Some(millis) = poll1::parse_canonical_utc_ms(text) {
                return Stamp::Ms(millis);
            }
            Stamp::Raw(Arc::new(text.to_owned()))
        }
        fn text(&self) -> Cow<'_, str> {
            match self {
                Stamp::Ms(ms) => Cow::Owned(source::iso(*ms as f64)),
                Stamp::Raw(text) => Cow::Borrowed(text.as_str()),
            }
        }
        fn text_owned(&self) -> String {
            self.text().into_owned()
        }
        fn is_canonical(&self) -> bool {
            matches!(self, Stamp::Ms(_))
        }
        fn millis(&self) -> Option<f64> {
            match self {
                Stamp::Ms(ms) => Some(*ms as f64),
                Stamp::Raw(text) => crate::model::ms(Some(text.as_str())),
            }
        }
    }
    impl PartialEq for Stamp {
        fn eq(&self, other: &Self) -> bool {
            self.cmp(other).is_eq()
        }
    }
    impl Eq for Stamp {}
    impl PartialOrd for Stamp {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for Stamp {
        fn cmp(&self, other: &Self) -> Ordering {
            match (self, other) {
                (Stamp::Ms(left), Stamp::Ms(right)) => left.cmp(right),
                (Stamp::Raw(left), Stamp::Raw(right)) => left.as_str().cmp(right.as_str()),
                (Stamp::Ms(left), Stamp::Raw(right)) => {
                    source::iso(*left as f64).as_str().cmp(right.as_str())
                }
                (Stamp::Raw(left), Stamp::Ms(right)) => {
                    left.as_str().cmp(source::iso(*right as f64).as_str())
                }
            }
        }
    }
    impl Hash for Stamp {
        fn hash<H: Hasher>(&self, state: &mut H) {
            match self {
                Stamp::Ms(ms) => source::iso(*ms as f64).hash(state),
                Stamp::Raw(text) => text.as_str().hash(state),
            }
        }
    }
    pub(super) fn stamp_opt(text: Option<String>) -> Option<Stamp> {
        text.map(|text| Stamp::parse(&text))
    }
    fn ms_of(stamp: &Option<Stamp>) -> Option<f64> {
        stamp.as_ref().and_then(Stamp::millis)
    }
    /// `updated_at >= since`, matching SQLite text order. `since` is usually
    /// `source::iso`, so a canonical stamp compares as an integer.
    pub(super) fn stamp_ge(stamp: &Stamp, since: &str) -> bool {
        match stamp {
            Stamp::Ms(ms) => {
                if let Some(since_ms) = poll1::parse_canonical_utc_ms(since) {
                    *ms >= since_ms
                } else {
                    stamp.text().as_ref() >= since
                }
            }
            Stamp::Raw(text) => text.as_str() >= since,
        }
    }
    fn opt_same(held: &Option<Arc<str>>, fresh: &Option<String>) -> bool {
        match (held, fresh) {
            (None, None) => true,
            (Some(held), Some(fresh)) => held.as_ref() == fresh,
            _ => false,
        }
    }
    pub(super) struct ItemPayloadRead {
        pub(super) rowid: i64,
        pub(super) thread_id: String,
        pub(super) run_id: Option<String>,
        pub(super) node_id: Option<String>,
        pub(super) provider_thread_id: Option<String>,
        pub(super) kind: String,
        pub(super) status: String,
        pub(super) updated_at: Stamp,
        pub(super) started_at: Option<Stamp>,
        pub(super) tool: Option<String>,
        pub(super) exit_code: Option<i32>,
        pub(super) ordinal: i64,
        pub(super) text: Option<Text>,
    }
    pub(super) fn blank() -> Arc<str> {
        static BLANK: OnceLock<Arc<str>> = OnceLock::new();
        Arc::clone(BLANK.get_or_init(|| Arc::from("")))
    }
    pub(super) fn empty_json() -> Arc<str> {
        static EMPTY_JSON: OnceLock<Arc<str>> = OnceLock::new();
        Arc::clone(EMPTY_JSON.get_or_init(|| Arc::from("{}")))
    }
    /// Repeated identity strings (kinds, statuses, tool names, ids). Not used
    /// for timestamps or for label, command, file or turn JSON: those are unique.
    #[derive(Clone, Default)]
    pub(super) struct Pool {
        values: HashSet<Arc<str>, std::hash::BuildHasherDefault<poll1::FnvHasher>>,
    }
    impl Pool {
        pub(super) fn intern(&mut self, text: &str) -> Arc<str> {
            if let Some(found) = self.values.get(text) {
                return Arc::clone(found);
            }
            let arc: Arc<str> = Arc::from(text);
            self.values.insert(Arc::clone(&arc));
            arc
        }
        pub(super) fn intern_opt_ref(&mut self, text: Option<&str>) -> Option<Arc<str>> {
            text.map(|text| self.intern(text))
        }
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct Body {
        pub(super) label: Arc<str>,
        pub(super) command: Arc<str>,
        pub(super) file: Arc<str>,
    }
    /// Projected turn JSON kept as the SQL text, plus the fields a history or
    /// identity check needs without parsing every turn on a content poll.
    #[derive(Clone, Debug)]
    pub(super) struct Turn {
        pub(super) json: Arc<str>,
        pub(super) provider_thread_id: Arc<str>,
        pub(super) ordinal: i64,
        pub(super) started_at: Option<Stamp>,
        pub(super) completed_at: Option<Stamp>,
        pub(super) model_bad: bool,
        pub(super) identity_bad: bool,
        pub(super) context_bad: bool,
        pub(super) lean_context: Option<Box<crate::model::Context>>,
        pub(super) model: Option<Arc<str>>,
        pub(super) used: f64,
        pub(super) max: f64,
    }
    impl Turn {
        fn parsed(&self) -> Result<Value, serde_json::Error> {
            serde_json::from_str(&self.json)
        }
        fn lean_json(&self) -> Result<Value, FallbackReason> {
            if self.context_bad {
                return Err(FallbackReason::AssemblyError);
            }
            let mut map = serde_json::Map::new();
            if let Some(name) = &self.model {
                map.insert("model".into(), Value::String(name.to_string()));
            }
            if let Some(ctx) = &self.lean_context {
                let tu = serde_json::to_value(&**ctx).map_err(|_| FallbackReason::AssemblyError)?;
                map.insert("tokenUsage".into(), tu);
            }
            Ok(Value::Object(map))
        }
        fn model_value(&self) -> Value {
            match &self.model {
                Some(name) => Value::String(name.to_string()),
                None => Value::Null,
            }
        }
    }
    /// Node ids are interned, so the node index and turn items share them.
    /// The node query also selects `run_id`, but the oracle's node map is read
    /// only through `Attributor::owner`, which follows `parent_node_id`; a
    /// node's run id reaches no snapshot, so it is not kept.
    #[derive(Clone, Debug)]
    pub(super) struct Node {
        pub(super) node_id: Arc<str>,
        pub(super) parent_node_id: Option<Arc<str>>,
    }
    /// Active and inactive node rows by node id, for `Attributor::owner` walks.
    /// A lookup only sees rows whose thread is active, as the oracle's
    /// `thread_id IN (active)` node query does.
    #[derive(Clone, Default)]
    struct NodeIndex {
        map: HashMap<Arc<str>, NodeSlot>,
        /// Ids that more than one row holds. Checked against the active set
        /// before every assembly.
        shared: HashSet<Arc<str>>,
    }
    #[derive(Clone)]
    enum NodeSlot {
        One(NodeRef),
        Many(Vec<NodeRef>),
    }
    #[derive(Clone)]
    struct NodeRef {
        row: i64,
        owner: Arc<str>,
        parent: Option<Arc<str>>,
    }
    impl NodeIndex {
        fn add(&mut self, row: i64, owner: Arc<str>, node: &Node) {
            let entry = NodeRef {
                row,
                owner,
                parent: node.parent_node_id.clone(),
            };
            match self.map.get_mut(&node.node_id) {
                None => {
                    self.map.insert(Arc::clone(&node.node_id), NodeSlot::One(entry));
                }
                Some(slot) => {
                    let mut list = match std::mem::replace(slot, NodeSlot::Many(Vec::new())) {
                        NodeSlot::One(one) => vec![one],
                        NodeSlot::Many(list) => list,
                    };
                    list.retain(|other| other.row != row);
                    list.push(entry);
                    *slot = if list.len() == 1 {
                        NodeSlot::One(list.pop().unwrap())
                    } else {
                        self.shared.insert(Arc::clone(&node.node_id));
                        NodeSlot::Many(list)
                    };
                }
            }
        }
        fn remove(&mut self, row: i64, node: &Node) {
            let Some(slot) = self.map.get_mut(&node.node_id) else { return };
            match slot {
                NodeSlot::One(one) => {
                    if one.row == row {
                        self.map.remove(&node.node_id);
                    }
                }
                NodeSlot::Many(list) => {
                    list.retain(|other| other.row != row);
                    if list.len() == 1 {
                        *slot = NodeSlot::One(list.pop().unwrap());
                        self.shared.remove(&node.node_id);
                    } else if list.is_empty() {
                        self.map.remove(&node.node_id);
                        self.shared.remove(&node.node_id);
                    }
                }
            }
        }
        /// `Some(parent)` when exactly one active row holds the id. Two active
        /// rows are refused before any walk (`duplicate_active`).
        fn parent(&self, id: &str, active: &HashSet<String>) -> Option<Option<&Arc<str>>> {
            match self.map.get(id)? {
                NodeSlot::One(one) => active.contains(one.owner.as_ref()).then_some(one.parent.as_ref()),
                NodeSlot::Many(list) => list
                    .iter()
                    .find(|one| active.contains(one.owner.as_ref()))
                    .map(|one| one.parent.as_ref()),
            }
        }
        fn duplicate_active(&self, active: &HashSet<String>) -> bool {
            self.shared.iter().any(|id| match self.map.get(id) {
                Some(NodeSlot::Many(list)) => {
                    list.iter().filter(|one| active.contains(one.owner.as_ref())).count() > 1
                }
                _ => false,
            })
        }
    }
    /// `Attributor::owner` over the node index: the first subagent id on the
    /// parent chain, through its alias.
    fn walk_owner(
        nodes: &NodeIndex,
        active: &HashSet<String>,
        sub_ids: &HashSet<String>,
        aliases: &HashMap<String, String>,
        start: Option<&str>,
    ) -> Option<String> {
        let mut node = start.filter(|id| !id.is_empty());
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(id) = node {
            if !seen.insert(id) {
                break;
            }
            if sub_ids.contains(id) {
                return Some(aliases.get(id).cloned().unwrap_or_else(|| id.to_owned()));
            }
            node = nodes
                .parent(id, active)
                .flatten()
                .map(|parent| parent.as_ref())
                .filter(|id| !id.is_empty());
        }
        None
    }
    /// Ids a parent-chain walk from `start` visits, including the id where it
    /// stops. A node change outside this set cannot change the walk.
    fn walk_ids(
        nodes: &NodeIndex,
        active: &HashSet<String>,
        sub_ids: &HashSet<String>,
        start: Option<&str>,
        out: &mut HashSet<Arc<str>>,
    ) {
        let mut node = start.filter(|id| !id.is_empty());
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(id) = node {
            if !seen.insert(id) {
                break;
            }
            out.insert(Arc::from(id));
            if sub_ids.contains(id) {
                break;
            }
            node = nodes
                .parent(id, active)
                .flatten()
                .map(|parent| parent.as_ref())
                .filter(|id| !id.is_empty());
        }
    }
    #[derive(Clone, Debug)]
    pub(super) struct Request {
        pub(super) node_id: String,
        pub(super) kind: String,
        pub(super) status: String,
        pub(super) created_at: Option<String>,
        pub(super) resolved_at: Option<String>,
        pub(super) async_input: i64,
    }
    /// Label, command, file and todo-list steps, read by rowid for a row inside
    /// a selection. They are always read, kept and dropped together.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct Text {
        pub(super) body: Body,
        pub(super) steps: Option<Arc<str>>,
    }
    /// Payload-derived keys and optional text, loaded by rowid only for rows
    /// inside an item cap, an event cap, or the plan set.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct ItemPayload {
        pub(super) started_at: Option<Stamp>,
        pub(super) tool: Option<Arc<str>>,
        pub(super) exit_code: Option<i32>,
        pub(super) text: Option<Text>,
    }
    /// Inline keys for every kept turn-item. Payload-derived keys and text are
    /// loaded only while the row is inside an item cap, an event cap, or the plan set.
    #[derive(Clone, Debug)]
    pub(super) struct Item {
        pub(super) run_id: Option<Arc<str>>,
        pub(super) node_id: Option<Arc<str>>,
        pub(super) provider_thread_id: Option<Arc<str>>,
        pub(super) kind: Arc<str>,
        pub(super) status: Arc<str>,
        pub(super) updated_at: Stamp,
        pub(super) ordinal: i64,
        pub(super) payload: Option<Box<ItemPayload>>,
    }
    impl Item {
        /// Compares every inline key (thread id is compared by row owner).
        fn inline_keys_eq(&self, other: &Self) -> bool {
            self.run_id == other.run_id
                && self.node_id == other.node_id
                && self.provider_thread_id == other.provider_thread_id
                && self.kind == other.kind
                && self.status == other.status
                && self.updated_at == other.updated_at
                && self.ordinal == other.ordinal
        }
        /// An inline-only read (`payload` is `None`) is not a change and must not
        /// wipe payload keys/text already held for a selected row.
        fn same_projected(&self, other: &Self) -> bool {
            if !self.inline_keys_eq(other) {
                return false;
            }
            match (&self.payload, &other.payload) {
                (Some(left), Some(right)) => {
                    left.started_at == right.started_at
                        && left.tool == right.tool
                        && left.exit_code == right.exit_code
                        && match (&left.text, &right.text) {
                            (Some(a), Some(b)) => a == b,
                            _ => true,
                        }
                }
                _ => true,
            }
        }
        fn matches_inline(&self, owner: &str, read: &ItemPayloadRead) -> bool {
            opt_same(&self.run_id, &read.run_id)
                && opt_same(&self.node_id, &read.node_id)
                && opt_same(&self.provider_thread_id, &read.provider_thread_id)
                && owner == read.thread_id
                && self.kind.as_ref() == read.kind
                && self.status.as_ref() == read.status
                && self.updated_at == read.updated_at
                && self.ordinal == read.ordinal
        }
        /// The item without label, command or file, for the status-only reader,
        /// which never reads them (`status_view`).
        fn bare(&self, owner: &str) -> Result<crate::model::Item, FallbackReason> {
            let payload = self.payload.as_ref().ok_or(FallbackReason::TextDrift)?;
            Ok(crate::model::Item {
                thread_id: owner.to_owned(),
                run_id: self.run_id.as_ref().map(|text| text.to_string()),
                node_id: self.node_id.as_ref().map(|text| text.to_string()),
                provider_thread_id: self.provider_thread_id.as_ref().map(|text| text.to_string()),
                kind: self.kind.to_string(),
                status: self.status.to_string(),
                updated_at: self.updated_at.text_owned(),
                started_at: payload.started_at.as_ref().map(Stamp::text_owned),
                label: String::new(),
                command: String::new(),
                file: String::new(),
                tool: payload.tool.as_ref().map(|text| text.to_string()),
                exit_code: payload.exit_code,
            })
        }
        fn materialize(&self, owner: &str) -> Result<crate::model::Item, FallbackReason> {
            let payload = self.payload.as_ref().ok_or(FallbackReason::TextDrift)?;
            let body = &payload.text.as_ref().ok_or(FallbackReason::TextDrift)?.body;
            Ok(crate::model::Item {
                thread_id: owner.to_owned(),
                run_id: self.run_id.as_ref().map(|text| text.to_string()),
                node_id: self.node_id.as_ref().map(|text| text.to_string()),
                provider_thread_id: self
                    .provider_thread_id
                    .as_ref()
                    .map(|text| text.to_string()),
                kind: self.kind.to_string(),
                status: self.status.to_string(),
                updated_at: self.updated_at.text_owned(),
                started_at: payload.started_at.as_ref().map(Stamp::text_owned),
                label: body.label.to_string(),
                command: body.command.to_string(),
                file: body.file.to_string(),
                tool: payload.tool.as_ref().map(|text| text.to_string()),
                exit_code: payload.exit_code,
            })
        }
    }
    /// One row of any table. Every variant is boxed: the tables differ a lot in
    /// size (a node is two ids, a turn item a dozen fields), and an inline enum
    /// made every node row as large as a turn item.
    #[derive(Clone, Debug)]
    pub(super) enum Data {
        Thread(Box<Thread>),
        Run(Box<Run>),
        Subagent(Box<Payload>),
        Provider(Box<Payload>),
        Turn(Box<Turn>),
        Node(Box<Node>),
        Request(Box<Request>),
        Item(Box<Item>),
    }
    /// `owner` is the row's `thread_id` column, interned.
    #[derive(Clone, Debug)]
    pub(super) struct Row {
        pub(super) id: i64,
        pub(super) owner: Arc<str>,
        pub(super) data: Data,
        pub(super) position: usize,
    }
    #[derive(Clone, Default)]
    struct Table {
        rows: BTreeMap<i64, Row>,
        owners: BTreeMap<Arc<str>, BTreeSet<i64>>,
    }
    impl Table {
        /// The table from one full read. `BTreeMap::from_iter` sorts and fills
        /// its nodes; inserting row by row leaves them about half full. Equal
        /// rowids keep the last row, as repeated `insert` would.
        fn build(rows: Vec<Row>) -> Self {
            let rows: BTreeMap<i64, Row> = rows.into_iter().map(|row| (row.id, row)).collect();
            let mut owners: HashMap<Arc<str>, Vec<i64>, std::hash::BuildHasherDefault<poll1::FnvHasher>> =
                HashMap::default();
            for row in rows.values() {
                if let Some(ids) = owners.get_mut(&row.owner) {
                    ids.push(row.id);
                } else {
                    owners.insert(Arc::clone(&row.owner), vec![row.id]);
                }
            }
            Self {
                rows,
                owners: owners
                    .into_iter()
                    .map(|(owner, ids)| (owner, ids.into_iter().collect()))
                    .collect(),
            }
        }
        fn remove_owner(&mut self, owner: &str) {
            if let Some(ids) = self.owners.remove(owner) {
                for id in ids {
                    self.rows.remove(&id);
                }
            }
        }
        fn insert(&mut self, row: Row) {
            let id = row.id;
            self.owners.entry(Arc::clone(&row.owner)).or_default().insert(id);
            if let Some(old) = self.rows.insert(id, row) {
                if old.owner != self.rows[&id].owner {
                    if let Some(ids) = self.owners.get_mut(&old.owner) {
                        ids.remove(&id);
                    }
                }
            }
        }
        fn take(&mut self, id: i64) -> Option<Row> {
            let row = self.rows.remove(&id)?;
            let empty = self.owners.get_mut(&row.owner).is_some_and(|ids| {
                ids.remove(&id);
                ids.is_empty()
            });
            if empty {
                self.owners.remove(&row.owner);
            }
            Some(row)
        }
    }
    #[derive(Clone, Debug)]
    pub(super) struct Project {
        pub(super) project_id: String,
        pub(super) title: String,
        pub(super) workspace_root: String,
    }
    fn same_projects(left: &BTreeMap<String, Project>, right: &BTreeMap<String, Project>) -> bool {
        left.len() == right.len()
            && left.iter().all(|(id, project)| {
                right.get(id).is_some_and(|other| {
                    project.title == other.title && project.workspace_root == other.workspace_root
                })
            })
    }
    fn same_data(left: &Data, right: &Data) -> bool {
        match (left, right) {
            (Data::Thread(a), Data::Thread(b)) => {
                a.data.payload == b.data.payload
                    && a.archived_at == b.archived_at
                    && a.deleted_at == b.deleted_at
            }
            (Data::Run(a), Data::Run(b)) => {
                a.data.payload == b.data.payload
                    && a.ordinal == b.ordinal
                    && a.requested_at == b.requested_at
                    && a.completed_at == b.completed_at
            }
            (Data::Subagent(a), Data::Subagent(b)) | (Data::Provider(a), Data::Provider(b)) => {
                a.payload == b.payload
            }
            (Data::Turn(a), Data::Turn(b)) => {
                a.json == b.json
                    && a.provider_thread_id == b.provider_thread_id
                    && a.ordinal == b.ordinal
                    && a.started_at == b.started_at
                    && a.completed_at == b.completed_at
                    && a.model_bad == b.model_bad
                    && a.identity_bad == b.identity_bad
                    && a.context_bad == b.context_bad
                    && a.model == b.model
                    && a.lean_context == b.lean_context
            }
            (Data::Node(a), Data::Node(b)) => {
                a.node_id == b.node_id && a.parent_node_id == b.parent_node_id
            }
            (Data::Request(a), Data::Request(b)) => {
                a.node_id == b.node_id
                    && a.kind == b.kind
                    && a.status == b.status
                    && a.created_at == b.created_at
                    && a.resolved_at == b.resolved_at
                    && a.async_input == b.async_input
            }
            (Data::Item(a), Data::Item(b)) => a.same_projected(b),
            _ => false,
        }
    }
    /// `Cache::touched` bits are `1 << table index`. Run and subagent rows move
    /// the selected run ids; `Changes` carries everything else a poll needs.
    const TOUCH_RUN: u8 = 1 << 1;
    const TOUCH_SUB: u8 = 1 << 2;
    #[derive(Clone, Default)]
    pub(super) struct Cache {
        tables: [Table; 8],
        projects: BTreeMap<String, Project>,
        idx: Select,
        /// Pre-content agents, sealed. A later poll clones one of these instead of
        /// rebuilding every shell.
        shells: Vec<Arc<crate::model::Agent>>,
        /// Tables whose rows actually changed on the last stage. Cleared once consumed.
        touched: u8,
        projects_changed: bool,
        /// Attribution maps, rebuilt with the shells on a structural poll.
        routes: Option<Arc<Routes>>,
        /// Node parents by node id, maintained row by row.
        nodes: NodeIndex,
        /// Provider rows by payload id and provider turns by provider id,
        /// every owner. Lookups keep active owners only.
        provider_rows: HashMap<Arc<str>, BTreeSet<i64>>,
        turn_rows: HashMap<Arc<str>, BTreeSet<i64>>,
        /// Turns whose payload provider id disagrees with the column.
        bad_turns: BTreeSet<i64>,
        /// What the last stage changed, consumed by `assemble_fast`.
        changes: Changes,
        pool: Pool,
        /// Rowids that currently store label, command, file or steps. Dropping
        /// text walks this set, not every turn item.
        text_held: HashSet<i64>,
        /// Test seam. The next text read treats one selected row as drifted.
        force_text_drift: bool,
        /// Status-only: agents are painted as `status_view` and no text is read.
        lean: bool,
    }
    #[derive(Debug)]
    pub(super) struct Pending {
        pub(super) full: bool,
        pub(super) owners: BTreeSet<String>,
        pub(super) tables: Vec<Vec<Row>>,
        pub(super) projects: BTreeMap<String, Project>,
        pub rows: usize,
    }
    // Narrow projection expressions plus rowid identity. Request capability
    // is added by request_query after checking schema compatibility.
    pub(super) fn queries() -> Vec<(&'static str, String)> {
        TABLES.iter().map(|&table| {
            let fields = match table {
                "threads" => format!("{},archived_at,deleted_at", payload_sql(table)),
                "runs" => format!("{},ordinal,requested_at,completed_at", payload_sql(table)),
                "subagents" | "provider_threads" => payload_sql(table),
                "provider_turns" => format!("{},provider_thread_id,ordinal,started_at,completed_at", payload_sql(table)),
                "nodes" => "node_id,parent_node_id,run_id".into(),
                "runtime_requests" => "node_id,kind,status,created_at,resolved_at".into(),
                // Keys only. Label, command, file and steps are read by rowid
                // for rows that enter a selection.
                "turn_items" => format!("{},ordinal", source::item_keys()),
                _ => unreachable!(),
            };
            (table, format!("SELECT rowid AS cache_rowid,thread_id,{fields} FROM orchestration_v2_projection_{table}"))
        }).collect()
    }
    pub(super) fn request_query(fields: &str) -> String {
        format!("SELECT rowid AS cache_rowid,thread_id,{fields} FROM orchestration_v2_projection_runtime_requests")
    }
    fn payload_sql(table: &str) -> String {
        format!("{} AS payload", source::projection(table))
    }
    /// Turn-item keys of one run, for a run that becomes selected. The same
    /// projection as the owner read, on T3's `(run_id, ordinal)` index.
    pub(super) fn run_query() -> String {
        format!("{} WHERE run_id=?", queries()[7].1)
    }
    /// By-rowid text read. Same allowlisted expressions as the full reader,
    /// plus ordinal and the todo-list steps expression the key scan omits.
    pub(super) fn item_text_query(n: usize) -> String {
        let marks = if n == 0 {
            "NULL".to_string()
        } else {
            vec!["?"; n.max(1)].join(",")
        };
        format!(
            "SELECT rowid,{fields},ordinal,CASE WHEN type='todo_list' THEN json_extract(payload_json,'$.steps') END AS steps FROM orchestration_v2_projection_turn_items WHERE rowid IN ({marks})",
            fields = source::item_fields()
        )
    }
    impl Pending {
        pub fn read(
            db: &Connection,
            path: &Path,
            owners: Option<&BTreeSet<String>>,
            pool: &mut Pool,
        ) -> Result<Self, SourceError> {
            let inline_ok = poll1::item_inline_query_ok(db);
            Self::read_scoped(db, path, owners, pool, false, inline_ok, None, None)
        }
        #[allow(clippy::too_many_arguments)]
        fn read_scoped(
            db: &Connection,
            path: &Path,
            owners: Option<&BTreeSet<String>>,
            pool: &mut Pool,
            lean: bool,
            inline_ok: bool,
            now: Option<f64>,
            idx_and_items: Option<(&Select, &BTreeMap<i64, Row>)>,
        ) -> Result<Self, SourceError> {
            poll1::read_pending_scoped(db, path, owners, pool, lean, inline_ok, now, idx_and_items)
        }
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) enum EncounterOrder {
        Rowid,
        ThreadIndex(String),
    }
    // Match only the two observed, single-loop shapes. In particular, do not
    // accept covering/automatic indexes, OR loops, or extra sorting operations.
    pub(super) fn subagent_order(db: &Connection, count: usize) -> Option<EncounterOrder> {
        let table = "orchestration_v2_projection_subagents";
        let oracle = classify(
            db,
            table,
            " (thread_id=?)",
            format!(
                "SELECT {} AS payload FROM {table} WHERE thread_id IN ({})",
                source::projection("subagents"),
                vec!["?"; count.max(1)].join(",")
            ),
            vec![SqlValue::Text(String::new()); count.max(1)],
        )?;
        let per_thread = classify(
            db,
            table,
            " (thread_id=?)",
            format!("{} WHERE thread_id=?", queries()[2].1),
            vec![SqlValue::Text(String::new())],
        )?;
        (oracle == per_thread).then_some(oracle)
    }
    use source::REQUEST_FILTER;
    pub(super) fn distinct_owners(db: &Connection, table: &str) -> rusqlite::Result<BTreeSet<String>> {
        let sql = format!("SELECT DISTINCT thread_id FROM orchestration_v2_projection_{table}");
        let mut stmt = db.prepare_cached(&sql)?;
        let mut rows = stmt.query([])?;
        let mut ids = BTreeSet::new();
        while let Some(row) = rows.next()? {
            ids.insert(row.get(0)?);
        }
        Ok(ids)
    }
    pub(super) fn owner_query(index: usize, sql: &str) -> String {
        let filter = if index == 6 {
            format!(" AND {REQUEST_FILTER}")
        } else {
            String::new()
        };
        format!("{sql} WHERE thread_id=?{filter}")
    }
    pub(super) fn classify(
        db: &Connection,
        table: &str,
        constraint: &str,
        sql: String,
        args: Vec<SqlValue>,
    ) -> Option<EncounterOrder> {
        let plans = db
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .ok()?
            .query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(3))
            .ok()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .ok()?;
        if plans == [format!("SCAN {table}")] {
            return Some(EncounterOrder::Rowid);
        }
        if plans.len() != 1 {
            return None;
        }
        let index = plans[0]
            .strip_prefix(&format!("SEARCH {table} USING INDEX "))?
            .strip_suffix(constraint)?;
        if index.is_empty() || index.contains(' ') {
            return None;
        }
        Some(EncounterOrder::ThreadIndex(index.into()))
    }
    pub(super) fn event_order(db: &Connection, count: usize) -> Option<EncounterOrder> {
        let table = "orchestration_v2_projection_turn_items";
        // This is the exact oracle projection/predicate/order/cap. Only the
        // single scan plus LIMIT's stable temporary sorter is proved here.
        let sql = format!(
            "SELECT {} FROM {table} WHERE thread_id IN ({}) AND updated_at>=? AND type NOT IN ('checkpoint','todo_list') ORDER BY updated_at DESC LIMIT 8000",
            source::item_fields(),
            vec!["?"; count.max(1)].join(",")
        );
        let params = vec![SqlValue::Text(String::new()); count.max(1) + 1];
        let plans: Vec<String> = db
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .ok()?
            .query_map(rusqlite::params_from_iter(params.iter()), |r| r.get(3))
            .ok()?
            .collect::<rusqlite::Result<_>>()
            .ok()?;
        if plans.len() != 2 || plans[1] != "USE TEMP B-TREE FOR ORDER BY" {
            return None;
        }
        let oracle = if plans[0] == format!("SCAN {table}") {
            EncounterOrder::Rowid
        } else {
            let index = plans[0]
                .strip_prefix(&format!("SEARCH {table} USING INDEX "))?
                .strip_suffix(" (thread_id=?)")?;
            if index.is_empty() || index.contains(' ') {
                return None;
            }
            EncounterOrder::ThreadIndex(index.into())
        };
        let owner = classify(
            db,
            table,
            " (thread_id=?)",
            owner_query(7, &queries()[7].1),
            vec![SqlValue::Text(String::new())],
        )?;
        (oracle == owner).then_some(oracle)
    }
    pub(super) fn request_order(db: &Connection, count: usize) -> Option<EncounterOrder> {
        let table = "orchestration_v2_projection_runtime_requests";
        // Both loops apply the identical predicate, including the status IN loop.
        // Require their entire index constraint to match as well as the index name.
        let fields = source::request_fields(db).ok()?;
        let oracle_sql = format!(
            "SELECT thread_id,{fields} FROM {table} WHERE thread_id IN ({}) AND {REQUEST_FILTER}",
            vec!["?"; count.max(1)].join(",")
        );
        for constraint in [" (thread_id=?)", " (thread_id=? AND status=?)"] {
            let oracle = classify(
                db,
                table,
                constraint,
                oracle_sql.clone(),
                vec![SqlValue::Text(String::new()); count.max(1)],
            );
            let owner = classify(
                db,
                table,
                constraint,
                owner_query(6, &request_query(&fields)),
                vec![SqlValue::Text(String::new())],
            );
            if let (Some(a), Some(b)) = (oracle, owner) {
                if a == b {
                    return Some(a);
                }
            }
        }
        None
    }
    /// Plans have no LIMIT, so SQLite 3.51.1's `USE TEMP B-TREE FOR ORDER BY`
    /// line is the external sorter, not the sequence-numbered ephemeral tree.
    /// Measured on this schema, that sorter keeps the index encounter order for
    /// equal `updated_at`: `run_id`, then `ordinal`, then `rowid`. A plain scan
    /// encounters `rowid` order and the sorter keeps that. Any other plan,
    /// including a differently named index with the same constraint text, is
    /// not this order.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) enum PlanOrder {
        Rowid,
        RunOrdinal(String),
    }
    pub(super) fn plan_order(db: &Connection, count: usize) -> Option<PlanOrder> {
        let table = "orchestration_v2_projection_turn_items";
        let marks = if count == 0 {
            "NULL".to_string()
        } else {
            vec!["?"; count].join(",")
        };
        let sql = format!(
            "SELECT thread_id,run_id,node_id,provider_thread_id,updated_at,json_extract(payload_json,'$.steps') AS steps FROM {table} WHERE type='todo_list' AND run_id IN ({marks}) ORDER BY updated_at DESC"
        );
        let params = vec![SqlValue::Text(String::new()); count];
        let plans: Vec<String> = db
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .ok()?
            .query_map(rusqlite::params_from_iter(params.iter()), |r| r.get(3))
            .ok()?
            .collect::<rusqlite::Result<_>>()
            .ok()?;
        if plans.len() != 2 || plans[1] != "USE TEMP B-TREE FOR ORDER BY" {
            return None;
        }
        if plans[0] == format!("SCAN {table}") {
            return Some(PlanOrder::Rowid);
        }
        let rest = plans[0].strip_prefix(&format!("SEARCH {table} USING INDEX "))?;
        let (index, constraint) = rest.split_once(' ')?;
        if constraint != "(run_id=?)"
            || index != "orchestration_v2_projection_turn_items_run_ordinal_idx"
        {
            return None;
        }
        Some(PlanOrder::RunOrdinal(index.into()))
    }
    /// Item query: multi-index OR, then `ORDER BY updated_at DESC, ordinal DESC LIMIT 3000`.
    /// Branch 1 is the run-ordinal index (`run_id`, `ordinal`, `rowid`). Branch 2 is
    /// the thread-run index for `run_id IS NULL` (`thread_id`, then `rowid`). The
    /// LIMIT sorter keeps that encounter order for equal sort keys, including at
    /// the cut. A plain scan encounters `rowid` order. Anything else is unproved.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) enum ItemOrder {
        Rowid,
        RunOr { run_index: String, thread_index: String },
    }
    pub(super) fn item_order(db: &Connection, runs: usize, threads: usize) -> Option<ItemOrder> {
        let table = "orchestration_v2_projection_turn_items";
        let list = |count: usize| {
            if count == 0 {
                "NULL".to_string()
            } else {
                vec!["?"; count].join(",")
            }
        };
        let sql = format!(
            "SELECT {} FROM {table} WHERE (run_id IN ({}) OR (run_id IS NULL AND thread_id IN ({}))) AND type IN ({}) ORDER BY updated_at DESC,ordinal DESC LIMIT 3000",
            source::item_fields(),
            list(runs),
            list(threads),
            source::TOOLISH
        );
        let params = vec![SqlValue::Text(String::new()); runs + threads];
        let rows: Vec<(i64, i64, String)> = db
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .ok()?
            .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(3)?))
            })
            .ok()?
            .collect::<rusqlite::Result<_>>()
            .ok()?;
        if rows.len() == 2
            && rows[0].1 == 0
            && rows[0].2 == format!("SCAN {table}")
            && rows[1].1 == 0
            && rows[1].2 == "USE TEMP B-TREE FOR ORDER BY"
        {
            return Some(ItemOrder::Rowid);
        }
        if rows.len() != 6 || rows[0].2 != "MULTI-INDEX OR" || rows[0].1 != 0 {
            return None;
        }
        if rows[1].2 != "INDEX 1" || rows[1].1 != rows[0].0 {
            return None;
        }
        if rows[3].2 != "INDEX 2" || rows[3].1 != rows[0].0 {
            return None;
        }
        if rows[2].1 != rows[1].0 || rows[4].1 != rows[3].0 || rows[5].1 != 0 {
            return None;
        }
        if rows[5].2 != "USE TEMP B-TREE FOR ORDER BY" {
            return None;
        }
        let search = |detail: &str, index: &str, constraint: &str| {
            detail
                == format!("SEARCH {table} USING INDEX {index} {constraint}")
        };
        let run_index = "orchestration_v2_projection_turn_items_run_ordinal_idx";
        let thread_index = "orchestration_v2_projection_turn_items_thread_run_idx";
        if !search(&rows[2].2, run_index, "(run_id=?)")
            || !search(&rows[4].2, thread_index, "(thread_id=? AND run_id=?)")
        {
            return None;
        }
        Some(ItemOrder::RunOr {
            run_index: run_index.into(),
            thread_index: thread_index.into(),
        })
    }
    /// Span query has no `ORDER BY`. On this schema both the oracle `IN` search
    /// and the per-thread reread use the unique `(thread_id, ordinal)` index, so
    /// each thread's in-window runs come back in `ordinal` order, threads in
    /// ascending id order. A plain scan is `rowid` order. Any other index,
    /// including the other `thread_id` indexes on this table, is not this order.
    pub(super) fn span_order(db: &Connection, count: usize) -> Option<EncounterOrder> {
        let table = "orchestration_v2_projection_runs";
        let marks = if count == 0 {
            "NULL".to_string()
        } else {
            vec!["?"; count].join(",")
        };
        let oracle = format!(
            "SELECT thread_id,requested_at,completed_at,json_extract(payload_json,'$.startedAt') AS started_at FROM {table} WHERE thread_id IN ({marks}) AND (completed_at IS NULL OR completed_at>=?)"
        );
        let explained = |sql: String, params: Vec<SqlValue>| -> Option<Vec<String>> {
            db.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .ok()?
                .query_map(rusqlite::params_from_iter(params.iter()), |row| row.get(3))
                .ok()?
                .collect::<rusqlite::Result<_>>()
                .ok()
        };
        let oracle_plan = explained(oracle, vec![SqlValue::Text(String::new()); count + 1])?;
        let owner_plan = explained(
            format!("{} WHERE thread_id=?", queries()[1].1),
            vec![SqlValue::Text(String::new())],
        )?;
        if oracle_plan != owner_plan {
            return None;
        }
        if oracle_plan == [format!("SCAN {table}")] {
            return Some(EncounterOrder::Rowid);
        }
        let expected = format!(
            "SEARCH {table} USING INDEX orchestration_v2_projection_runs_thread_ordinal_idx (thread_id=?)"
        );
        (oracle_plan.len() == 1 && oracle_plan[0] == expected).then_some(
            EncounterOrder::ThreadIndex(
                "orchestration_v2_projection_runs_thread_ordinal_idx".into(),
            ),
        )
    }
    fn run_in_window(row: &Row, since: &str) -> bool {
        match &row.data {
            Data::Run(run) => run
                .completed_at
                .as_ref()
                .is_none_or(|stamp| stamp.as_str() >= since),
            _ => false,
        }
    }
    /// Sort in-window runs into the proved encounter order. Without a proved
    /// plan, more than one run per owner is `MultipleSpans`.
    fn order_windowed_runs(
        rows: &mut Vec<&Row>,
        order: Option<&EncounterOrder>,
    ) -> Result<(), FallbackReason> {
        match order {
            Some(EncounterOrder::ThreadIndex(_)) => rows.sort_by(|left, right| {
                let ordinal = |row: &Row| match &row.data {
                    Data::Run(run) => run.ordinal,
                    _ => 0,
                };
                left.owner
                    .cmp(&right.owner)
                    .then(ordinal(left).cmp(&ordinal(right)))
                    .then(left.id.cmp(&right.id))
            }),
            Some(EncounterOrder::Rowid) => rows.sort_by_key(|row| row.id),
            None => {
                let mut owners = HashSet::new();
                if rows.iter().any(|row| !owners.insert(row.owner.as_ref())) {
                    return Err(FallbackReason::MultipleSpans);
                }
            }
        }
        Ok(())
    }
    /// `updated_at DESC`, `ordinal DESC`, then proved encounter order. Unknown
    /// and rowid modes leave `branch` and `group` empty, so only rowid remains.
    #[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
    struct ItemKey {
        updated: Reverse<Stamp>,
        ordinal: Reverse<i64>,
        branch: u8,
        group: Arc<str>,
        rowid: i64,
    }
    fn item_key(mode: Option<&ItemOrder>, row: &Row) -> ItemKey {
        let Data::Item(v) = &row.data else {
            return ItemKey {
                updated: Reverse(Stamp::Raw(Arc::new(String::new()))),
                ordinal: Reverse(0),
                branch: 0,
                group: blank(),
                rowid: row.id,
            };
        };
        let (branch, group) = match mode {
            Some(ItemOrder::RunOr { .. }) => {
                match v.run_id.as_ref().filter(|id| !id.is_empty()) {
                    // Branch 1 of the OR walks selected runs by run id. Branch 2
                    // walks runless rows by the thread_id column, which is also
                    // the item's thread id.
                    Some(run) => (0, Arc::clone(run)),
                    None => (1, Arc::clone(&row.owner)),
                }
            }
            _ => (0, blank()),
        };
        ItemKey {
            updated: Reverse(v.updated_at.clone()),
            ordinal: Reverse(v.ordinal),
            branch,
            group,
            rowid: row.id,
        }
    }
    #[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
    struct StampKey {
        updated: Reverse<Stamp>,
        owner: Arc<str>,
        position: u64,
        rowid: i64,
    }
    /// `updated_at DESC`, then the proved encounter order. Rowid mode and an
    /// unknown plan leave `run` empty and `ordinal` zero, so only rowid remains.
    #[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
    struct PlanKey {
        updated: Reverse<Stamp>,
        run: Arc<str>,
        ordinal: i64,
        rowid: i64,
    }
    fn plan_key(mode: Option<&PlanOrder>, row: &Row, updated: &Stamp) -> PlanKey {
        let (run, ordinal) = match mode {
            Some(PlanOrder::RunOrdinal(_)) => match &row.data {
                Data::Item(v) => (
                    v.run_id.clone().unwrap_or_else(blank),
                    v.ordinal,
                ),
                _ => (blank(), 0),
            },
            _ => (blank(), 0),
        };
        PlanKey {
            updated: Reverse(updated.clone()),
            run,
            ordinal,
            rowid: row.id,
        }
    }
    fn canonical_stamp(text: &str) -> bool {
        poll1::parse_canonical_utc_ms(text).is_some()
    }
    fn stamp_key(mode: Option<&EncounterOrder>, row: &Row, updated: &Stamp) -> StampKey {
        match mode {
            Some(EncounterOrder::ThreadIndex(_)) => StampKey {
                updated: Reverse(updated.clone()),
                owner: Arc::clone(&row.owner),
                position: row.position as u64,
                rowid: row.id,
            },
            _ => StampKey {
                updated: Reverse(updated.clone()),
                owner: blank(),
                position: 0,
                rowid: row.id,
            },
        }
    }
    /// Ordered caps and the per-agent input hashes. Small tables are re-read on
    /// each poll; only turn-items stay in these trees, because that is the table
    /// whose full scan dominated a poll.
    #[derive(Clone, Default)]
    pub(super) struct Select {
        ready: bool,
        inputs_ready: bool,
        pub(super) since: String,
        bad_events: u32,

        event_tie_groups: u32,
        plan_tie_groups: u32,
        /// Visible items in cap order. A key ends in the rowid, so it is the
        /// whole entry; an indexed row's key is recomputed from its cached row,
        /// which is the row it was built from (`align_items` rebuilds every key
        /// when the item mode changes).
        items: BTreeSet<ItemKey>,
        item_ids: HashSet<i64>,
        /// Newest 3,000 visible items, kept in cap order so a poll does not walk the tree.
        item_prefix: Vec<ItemKey>,
        item_mode: Option<ItemOrder>,
        item_mode_known: bool,
        events: BTreeMap<StampKey, i64>,
        event_at: HashMap<i64, StampKey>,
        /// Newest 8,000 in-window events, in cap order.
        event_prefix: Vec<i64>,
        event_ids: HashSet<i64>,
        event_group: HashMap<Stamp, u32>,
        event_mode: Option<EncounterOrder>,
        event_mode_known: bool,
        plans: BTreeMap<PlanKey, i64>,
        plan_at: HashMap<i64, PlanKey>,
        plan_mode: Option<PlanOrder>,
        plan_mode_known: bool,
        plan_group: HashMap<Stamp, u32>,
        by_run: HashMap<String, HashSet<i64>>,
        plan_by_run: HashMap<String, HashSet<i64>>,
        runless: HashMap<String, HashSet<i64>>,
        pub(super) active: HashSet<String>,
        latest_keys: HashSet<String>,
        pub(super) run_refs: HashSet<String>,
        item_members: HashSet<i64>,
        event_members: HashSet<i64>,
        plan_members: HashSet<i64>,
        /// Attributed agent of every turn-item row inside a selection: the
        /// item cap, the event cap or the plan set. Exactly those rows.
        targets: HashMap<i64, String>,
        /// Attributed agent of every open request.
        request_targets: HashMap<i64, String>,
        /// Minimum canonical completion among in-window runs, invalidated when
        /// runs, the active set, or the window moves. `span_floor_bad` forces
        /// the cutoff to `now`, matching a noncanonical stamp in `deadline_stamps`.
        span_floor_ready: bool,
        span_floor_bad: bool,
        span_floor_ms: f64,
    }
    pub(super) struct Elig {
        pub(super) active: HashSet<String>,
        pub(super) latest_keys: HashSet<String>,
        pub(super) run_refs: HashSet<String>,
    }
    pub(super) fn eligibility_rows<'a>(
        threads: impl Iterator<Item = &'a Row>,
        runs: impl Iterator<Item = &'a Row>,
        subs: impl Iterator<Item = &'a Row>,
    ) -> Elig {
        let mut active = HashSet::new();
        for row in threads {
            if let Data::Thread(t) = &row.data {
                if t.archived_at.is_none() && t.deleted_at.is_none() {
                    if let Some(id) = t.data.payload.get("id").and_then(|v| v.as_str()) {
                        active.insert(id.to_string());
                    }
                }
            }
        }
        let mut best: HashMap<String, (i64, String)> = HashMap::new();
        for row in runs {
            let Data::Run(v) = &row.data else { continue };
            if !active.contains(row.owner.as_ref()) {
                continue;
            }
            let tid = v
                .data
                .payload
                .get("threadId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let run_id = v
                .data
                .payload
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let entry = best.entry(tid.to_string()).or_insert((i64::MIN, String::new()));
            if v.ordinal > entry.0 || (v.ordinal == entry.0 && run_id > entry.1.as_str()) {
                *entry = (v.ordinal, run_id.to_string());
            }
        }
        let latest_keys = best.keys().cloned().collect();
        let mut run_refs = HashSet::new();
        for id in best.values() {
            if !id.1.is_empty() {
                run_refs.insert(id.1.clone());
            }
        }
        for row in subs {
            if !active.contains(row.owner.as_ref()) {
                continue;
            }
            if let Data::Subagent(v) = &row.data {
                if let Some(id) = v.payload.get("runId").and_then(|v| v.as_str()) {
                    if !id.is_empty() {
                        run_refs.insert(id.to_string());
                    }
                }
            }
        }
        Elig { active, latest_keys, run_refs }
    }
    fn eligibility(tables: &[Table; 8]) -> Elig {
        eligibility_rows(
            tables[0].rows.values(),
            tables[1].rows.values(),
            tables[2].rows.values(),
        )
    }
    impl Select {
        fn item_visible(&self, owner: &str, kind: &str, run_id: Option<&str>) -> bool {
            if !source::is_toolish(kind) {
                return false;
            }
            let selected = run_id.is_some_and(|id| self.run_refs.contains(id));
            let runless = run_id.is_none()
                && self.active.contains(owner)
                && !self.latest_keys.contains(owner);
            selected || runless
        }
        fn insert_item(&mut self, row: &Row) {
            if self.item_ids.contains(&row.id) {
                return;
            }
            if !matches!(row.data, Data::Item(_)) {
                return;
            }
            let key = item_key(self.item_mode.as_ref(), row);
            self.item_ids.insert(row.id);
            self.items.insert(key.clone());
            self.admit_item(key);
        }
        fn admit_item(&mut self, key: ItemKey) {
            const CAP: usize = 3000;
            if self.item_prefix.len() >= CAP && *self.item_prefix.last().unwrap() <= key {
                return;
            }
            let pos = self.item_prefix.partition_point(|other| *other < key);
            self.item_prefix.insert(pos, key);
            if self.item_prefix.len() > CAP {
                self.item_prefix.pop();
            }
        }
        fn drop_item(&mut self, key: &ItemKey) {
            let pos = self.item_prefix.partition_point(|other| other < key);
            if self.item_prefix.get(pos) != Some(key) {
                return;
            }
            self.item_prefix.remove(pos);
            if let Some(next) = self.items.iter().nth(self.item_prefix.len()) {
                self.item_prefix.push(next.clone());
            }
        }
        /// `row` is the cached row the entry was built from.
        fn remove_item(&mut self, row: &Row) {
            if !self.item_ids.remove(&row.id) {
                return;
            }
            let key = item_key(self.item_mode.as_ref(), row);
            self.items.remove(&key);
            self.drop_item(&key);
        }
        /// Two visible items with one `(updated_at, ordinal)`. Their order is
        /// the plan's encounter order, which only a proved plan supplies. Equal
        /// pairs are adjacent in key order.
        fn item_ties(&self) -> bool {
            self.items
                .iter()
                .zip(self.items.iter().skip(1))
                .any(|(a, b)| a.updated == b.updated && a.ordinal == b.ordinal)
        }
        fn insert_event(&mut self, row: &Row, updated: &Stamp) {
            if self.event_at.contains_key(&row.id) {
                return;
            }
            if !updated.is_canonical() {
                self.bad_events += 1;
            }
            let group = self.event_group.entry(updated.clone()).or_default();
            *group += 1;
            if *group == 2 {
                self.event_tie_groups += 1;
            }
            let key = stamp_key(self.event_mode.as_ref(), row, updated);
            self.event_ids.insert(row.id);
            self.event_at.insert(row.id, key.clone());
            self.events.insert(key, row.id);
            self.admit_event(row.id);
        }
        fn admit_event(&mut self, id: i64) {
            const CAP: usize = 8000;
            let key = self.event_at[&id].clone();
            if self.event_prefix.len() >= CAP {
                let tail = *self.event_prefix.last().unwrap();
                if self.event_at[&tail] <= key {
                    return;
                }
            }
            let pos = self
                .event_prefix
                .partition_point(|other| self.event_at[other] < key);
            self.event_prefix.insert(pos, id);
            if self.event_prefix.len() > CAP {
                self.event_prefix.pop();
            }
        }
        fn drop_event(&mut self, id: i64) {
            let Some(pos) = self.event_prefix.iter().position(|other| *other == id) else {
                return;
            };
            self.event_prefix.remove(pos);
            if let Some(next) = self.events.values().nth(self.event_prefix.len()).copied() {
                self.event_prefix.push(next);
            }
        }
        fn remove_event(&mut self, id: i64, updated: &Stamp) {
            let Some(key) = self.event_at.remove(&id) else { return };
            self.events.remove(&key);
            self.event_ids.remove(&id);
            self.drop_event(id);
            if !updated.is_canonical() {
                self.bad_events = self.bad_events.saturating_sub(1);
            }
            let stamp = updated.clone();
            if let Some(group) = self.event_group.get_mut(&stamp) {
                *group -= 1;
                if *group == 1 {
                    self.event_tie_groups -= 1;
                }
                if *group == 0 {
                    self.event_group.remove(&stamp);
                }
            }
        }
        fn insert_plan(&mut self, row: &Row, updated: &Stamp) {
            if self.plan_at.contains_key(&row.id) {
                return;
            }
            let group = self.plan_group.entry(updated.clone()).or_default();
            *group += 1;
            if *group == 2 {
                self.plan_tie_groups += 1;
            }
            let key = plan_key(self.plan_mode.as_ref(), row, updated);
            self.plan_at.insert(row.id, key.clone());
            self.plans.insert(key, row.id);
        }
        fn remove_plan(&mut self, id: i64, updated: &Stamp) {
            let Some(key) = self.plan_at.remove(&id) else { return };
            self.plans.remove(&key);
            let stamp = updated.clone();
            if let Some(group) = self.plan_group.get_mut(&stamp) {
                *group -= 1;
                if *group == 1 {
                    self.plan_tie_groups -= 1;
                }
                if *group == 0 {
                    self.plan_group.remove(&stamp);
                }
            }
        }
        fn unindex_item(&mut self, row: &Row) {
            let Data::Item(v) = &row.data else { return };
            self.remove_item(row);
            self.remove_event(row.id, &v.updated_at);
            self.remove_plan(row.id, &v.updated_at);
            if let Some(run) = v.run_id.as_deref().filter(|id| !id.is_empty()) {
                if let Some(set) = self.by_run.get_mut(run) {
                    set.remove(&row.id);
                }
                if let Some(set) = self.plan_by_run.get_mut(run) {
                    set.remove(&row.id);
                }
            } else if v.run_id.is_none() {
                if let Some(set) = self.runless.get_mut(row.owner.as_ref()) {
                    set.remove(&row.id);
                }
            }
        }
        fn index_item(&mut self, row: &Row) {
            self.index_item_inner(row, true);
        }
        fn index_item_bulk(&mut self, row: &Row) {
            self.index_item_inner(row, false);
        }
        fn index_item_inner(&mut self, row: &Row, maintain_prefix: bool) {
            let Data::Item(v) = &row.data else { return };
            let run = v.run_id.as_deref().filter(|id| !id.is_empty());
            if source::is_toolish(&v.kind) {
                if let Some(run) = run {
                    if let Some(set) = self.by_run.get_mut(run) {
                        set.insert(row.id);
                    } else {
                        let mut set = HashSet::new();
                        set.insert(row.id);
                        self.by_run.insert(run.to_owned(), set);
                    }
                } else if v.run_id.is_none() {
                    if let Some(set) = self.runless.get_mut(row.owner.as_ref()) {
                        set.insert(row.id);
                    } else {
                        let mut set = HashSet::new();
                        set.insert(row.id);
                        self.runless.insert(row.owner.to_string(), set);
                    }
                }
                if self.item_visible(&row.owner, &v.kind, run) {
                    if maintain_prefix {
                        self.insert_item(row);
                    } else if !self.item_ids.contains(&row.id) {
                        let key = item_key(self.item_mode.as_ref(), row);
                        self.item_ids.insert(row.id);
                        self.items.insert(key);
                    }
                }
            }
            if source::is_event_kind(&v.kind)
                && self.active.contains(row.owner.as_ref())
                && stamp_ge(&v.updated_at, &self.since)
            {
                if maintain_prefix {
                    self.insert_event(row, &v.updated_at);
                } else if !self.event_at.contains_key(&row.id) {
                    let updated = &v.updated_at;
                    if !updated.is_canonical() {
                        self.bad_events += 1;
                    }
                    let group = self.event_group.entry(updated.clone()).or_default();
                    *group += 1;
                    if *group == 2 {
                        self.event_tie_groups += 1;
                    }
                    let key = stamp_key(self.event_mode.as_ref(), row, updated);
                    self.event_ids.insert(row.id);
                    self.event_at.insert(row.id, key.clone());
                    self.events.insert(key, row.id);
                }
            }
            if v.kind.as_ref() == "todo_list" {
                if let Some(run) = run {
                    if let Some(set) = self.plan_by_run.get_mut(run) {
                        set.insert(row.id);
                    } else {
                        let mut set = HashSet::new();
                        set.insert(row.id);
                        self.plan_by_run.insert(run.to_owned(), set);
                    }
                }
                if run.is_some_and(|id| self.run_refs.contains(id)) {
                    self.insert_plan(row, &v.updated_at);
                }
            }
        }
        fn store_elig(&mut self, elig: Elig) {
            self.active = elig.active;
            self.latest_keys = elig.latest_keys;
            self.run_refs = elig.run_refs;
        }
        /// Forward clock only drops rows. A backwards clock never reaches this path.
        /// Returns the rows whose events left the window.
        fn roll_since(&mut self, now: f64, items: &BTreeMap<i64, Row>) -> Vec<i64> {
            let since = source::iso(now - source::WINDOW);
            if since == self.since {
                return Vec::new();
            }
            self.since = since;
            self.span_floor_ready = false;
            let stale: Vec<i64> = self
                .events
                .iter()
                .filter(|(key, _)| !stamp_ge(&key.updated.0, &self.since))
                .map(|(_, id)| *id)
                .collect();
            for id in &stale {
                let stamp = items.get(id).and_then(|row| match &row.data {
                    Data::Item(v) => Some(v.updated_at.clone()),
                    _ => None,
                });
                if let Some(stamp) = stamp {
                    self.remove_event(*id, &stamp);
                }
            }
            stale
        }
        /// Whether a turn-item row has to stay in the cache: whether a selection
        /// can take it without the row being read again.
        ///
        /// - The event cap: an event kind on an active thread, inside the window.
        /// - The item cap: a tool-like row of a selected run, or one without a
        ///   run on an active thread (whether or not that thread has a latest
        ///   run, so a change of latest run never needs a read).
        /// - The plan set: a todo list of a selected run.
        ///
        /// Any other row is outside all three, and can enter one only when its
        /// run becomes selected (`Cache::fetch_runs` reads that run by run id),
        /// when its thread becomes active (a change to that thread's own row,
        /// which re-reads the thread in the same poll), or when the row itself
        /// changes (a re-read too). The window only moves forward between full
        /// reads, and a row with an empty run id is never an item or a plan.
        fn keeps(&self, row: &Row) -> bool {
            let Data::Item(v) = &row.data else { return true };
            let owner = row.owner.as_ref();
            if source::is_event_kind(&v.kind) && self.active.contains(owner) && stamp_ge(&v.updated_at, &self.since) {
                return true;
            }
            let selected = || v.run_id.as_deref().is_some_and(|run| !run.is_empty() && self.run_refs.contains(run));
            if source::is_toolish(&v.kind) {
                return selected() || (v.run_id.is_none() && self.active.contains(owner));
            }
            v.kind.as_ref() == "todo_list" && selected()
        }
        fn ensure_event_mode(&mut self, mode: Option<EncounterOrder>, items: &BTreeMap<i64, Row>) {
            if self.event_mode_known && self.event_mode == mode {
                return;
            }
            let ids: Vec<i64> = self.event_ids.iter().copied().collect();
            self.events.clear();
            self.event_at.clear();
            self.event_mode = mode.clone();
            self.event_mode_known = true;
            for id in ids {
                let Some(row) = items.get(&id) else { continue };
                let Data::Item(v) = &row.data else { continue };
                let key = stamp_key(self.event_mode.as_ref(), row, &v.updated_at);
                self.event_at.insert(id, key.clone());
                self.events.insert(key, id);
            }
            self.rebuild_event_prefix();
        }
        fn rebuild_event_prefix(&mut self) {
            self.event_prefix.clear();
            self.event_prefix.extend(self.events.values().take(8000).copied());
        }
        /// An owner that left the active set keeps its rows, but the oracle's
        /// event query is `thread_id IN (active)` and a runless item is visible
        /// only on an active thread. Drop those index entries. Run-scoped items
        /// follow `apply_run_diff`.
        fn forget_inactive(&mut self, ids: &BTreeSet<i64>, items: &BTreeMap<i64, Row>) {
            for id in ids {
                let Some(row) = items.get(id) else { continue };
                let Data::Item(v) = &row.data else { continue };
                if self.event_at.contains_key(id) {
                    self.remove_event(*id, &v.updated_at);
                }
                if v.run_id.is_none() {
                    self.remove_item(row);
                }
            }
        }
        fn apply_run_diff(&mut self, gained: &[String], lost: &[String], items: &BTreeMap<i64, Row>) {
            for run in lost {
                let ids = self.by_run.get(run).cloned().unwrap_or_default();
                for id in ids {
                    if let Some(row) = items.get(&id) {
                        self.remove_item(row);
                    }
                }
                let ids = self.plan_by_run.get(run).cloned().unwrap_or_default();
                for id in ids {
                    if let Some(row) = items.get(&id) {
                        if let Data::Item(v) = &row.data {
                            self.remove_plan(id, &v.updated_at);
                        }
                    }
                }
            }
            for run in gained {
                let ids = self.by_run.get(run).cloned().unwrap_or_default();
                for id in ids {
                    if let Some(row) = items.get(&id) {
                        if let Data::Item(v) = &row.data {
                            if source::is_toolish(&v.kind) {
                                self.insert_item(row);
                            }
                        }
                    }
                }
                let ids = self.plan_by_run.get(run).cloned().unwrap_or_default();
                for id in ids {
                    if let Some(row) = items.get(&id) {
                        if let Data::Item(v) = &row.data {
                            self.insert_plan(row, &v.updated_at);
                        }
                    }
                }
            }
        }
        fn prefix(&self, cap: usize, events: bool) -> Vec<i64> {
            if events {
                self.event_prefix.iter().take(cap).copied().collect()
            } else {
                self.item_prefix.iter().take(cap).map(|key| key.rowid).collect()
            }
        }
    }
    pub(super) struct FastOut {
        pub(super) snapshot: Snapshot,
        pub(super) rebuilt: Vec<String>,
        pub(super) valid_until: f64,
    }
    pub(super) struct Undo {
        /// A full stage released the previous cache rather than keep two
        /// caches alive for the poll: a failed full poll leaves an unready
        /// cache, and the next poll reads in full.
        replaced: bool,
        owners: BTreeSet<String>,
        /// Previous versions of rows that were overwritten or deleted.
        rows: Vec<(usize, Row)>,
        /// Row ids inserted by this stage that did not exist before it.
        fresh: Vec<(usize, i64)>,
        projects: BTreeMap<String, Project>,
        /// Rows read by run id for newly selected runs, or rowid for moved runs.
        pub(super) read: usize,
    }
    /// The `source::Attributor` maps for the sealed shells, plus the reverse
    /// maps a content poll needs to find the agents a changed row reaches.
    #[derive(Default)]
    struct Routes {
        native: HashMap<String, String>,
        aliases: HashMap<String, String>,
        sub_ids: HashSet<String>,
        /// Providers that `native` sends to each agent, sorted.
        native_for: HashMap<String, Vec<String>>,
        /// Agents whose shell reads each provider id (`context`, `turn`, `model`).
        consumers: HashMap<String, Vec<String>>,
        /// Ids on every subagent's `parentNodeId` chain. A node change outside
        /// this set cannot move a subagent's parent.
        sub_chain: HashSet<Arc<str>>,
        /// Position of each agent in `shells`.
        position: HashMap<String, usize>,
    }
    /// Row changes from the last stage. A link change makes the poll structural;
    /// everything else reaches only the agents its rows route to.
    #[derive(Clone, Default)]
    struct Changes {
        /// A thread's link fields, the active set or a subagent row changed.
        links: bool,
        /// Threads whose own row changed (any field).
        threads: BTreeSet<String>,
        /// Owners of changed run rows.
        runs: BTreeSet<String>,
        /// Provider ids whose provider row or turns changed.
        providers: HashSet<Arc<str>>,
        /// Owners of changed turn rows, before and after.
        turn_owners: BTreeSet<String>,
        /// Node ids added, removed or re-parented.
        nodes: HashSet<Arc<str>>,
        /// Changed request rowids.
        requests: HashSet<i64>,
        /// Changed turn-item rowids, including a text-only change.
        items: HashSet<i64>,
    }
    /// Fields of a thread row that another agent, or the parent graph, reads.
    /// Everything else in the row only feeds that thread's own `thread_agent`.
    fn thread_links(thread: &Thread) -> [Option<&Value>; 5] {
        let payload = &thread.data.payload;
        let lineage = payload.get("lineage");
        [
            payload.get("id"),
            payload.get("projectId"),
            payload.get("delegatedFrom"),
            lineage.and_then(|v| v.get("relationshipToParent")),
            lineage.and_then(|v| v.get("parentThreadId")),
        ]
    }
    /// Record one replaced, inserted or removed row.
    fn note_change(changes: &mut Changes, pool: &mut Pool, index: usize, old: Option<&Row>, new: Option<&Row>) {
        let owners = || old.iter().chain(new.iter()).map(|row| row.owner.to_string());
        if index < 7 {
            if let (Some(old), Some(new)) = (old, new) {
                // A row that moved to another thread changes every owner-keyed rule.
                if old.owner != new.owner {
                    changes.links = true;
                }
            }
        }
        match index {
            0 => {
                changes.threads.extend(owners());
                let same = match (old.map(|row| &row.data), new.map(|row| &row.data)) {
                    (Some(Data::Thread(a)), Some(Data::Thread(b))) => same_links(a, b),
                    _ => false,
                };
                if !same {
                    changes.links = true;
                }
            }
            1 => changes.runs.extend(owners()),
            2 => changes.links = true,
            3 | 4 => {
                for row in old.iter().chain(new.iter()) {
                    match &row.data {
                        Data::Provider(v) => {
                            changes.providers.insert(pool.intern(source::str_at(&v.payload, "id")));
                        }
                        Data::Turn(v) => {
                            changes.providers.insert(Arc::clone(&v.provider_thread_id));
                            changes.turn_owners.insert(row.owner.to_string());
                        }
                        _ => {}
                    }
                }
            }
            5 => {
                for row in old.iter().chain(new.iter()) {
                    if let Data::Node(v) = &row.data {
                        changes.nodes.insert(Arc::clone(&v.node_id));
                    }
                }
            }
            6 => {
                for row in old.iter().chain(new.iter()) {
                    changes.requests.insert(row.id);
                }
            }
            _ => {
                for row in old.iter().chain(new.iter()) {
                    changes.items.insert(row.id);
                }
            }
        }
    }
    /// Keep the node and provider indexes in step with one row.
    fn note_index(
        nodes: &mut NodeIndex,
        provider_rows: &mut HashMap<Arc<str>, BTreeSet<i64>>,
        turn_rows: &mut HashMap<Arc<str>, BTreeSet<i64>>,
        bad_turns: &mut BTreeSet<i64>,
        pool: &mut Pool,
        row: &Row,
        add: bool,
    ) {
        let note = |map: &mut HashMap<Arc<str>, BTreeSet<i64>>, key: Arc<str>| {
            if add {
                map.entry(key).or_default().insert(row.id);
            } else if let Some(set) = map.get_mut(&key) {
                set.remove(&row.id);
                if set.is_empty() {
                    map.remove(&key);
                }
            }
        };
        match &row.data {
            Data::Node(node) => {
                if add {
                    nodes.add(row.id, Arc::clone(&row.owner), node);
                } else {
                    nodes.remove(row.id, node);
                }
            }
            Data::Provider(v) => note(provider_rows, pool.intern(source::str_at(&v.payload, "id"))),
            Data::Turn(v) => {
                note(turn_rows, Arc::clone(&v.provider_thread_id));
                if add && v.identity_bad {
                    bad_turns.insert(row.id);
                } else if !add {
                    bad_turns.remove(&row.id);
                }
            }
            _ => {}
        }
    }
    pub(super) fn thread_active(thread: &Thread) -> bool {
        thread.archived_at.is_none() && thread.deleted_at.is_none()
    }
    fn same_links(left: &Thread, right: &Thread) -> bool {
        let norm = |v: Option<&Value>| v.filter(|v| !v.is_null()).cloned();
        thread_active(left) == thread_active(right)
            && thread_links(left)
                .iter()
                .zip(thread_links(right).iter())
                .all(|(a, b)| norm(*a) == norm(*b))
    }
    impl Cache {
        pub(super) fn new(lean: bool) -> Self {
            Self {
                lean,
                ..Self::default()
            }
        }
        pub fn active_count(&self) -> usize {
            self.tables[0]
                .rows
                .values()
                .filter(|r| {
                    matches!(&r.data,
                Data::Thread(t) if t.archived_at.is_none() && t.deleted_at.is_none())
                })
                .count()
        }
        pub(super) fn run_ref_count(&self) -> usize {
            self.idx.run_refs.len()
        }
        pub(super) fn runless_count(&self) -> usize {
            self.idx.active.difference(&self.idx.latest_keys).count()
        }
    }
    impl Cache {
        /// Reject any ordering ambiguity rather than guess a SQLite tie winner.
        pub fn snapshot(
            &self,
            path: &Path,
            now: f64,
            report: &mut source::ReadReport,
            sub_order: Option<EncounterOrder>,
            request_order: Option<EncounterOrder>,
            event_order: Option<EncounterOrder>,
            plan_order: Option<PlanOrder>,
            item_order: Option<ItemOrder>,
            span_order: Option<EncounterOrder>,
        ) -> Result<Snapshot, FallbackReason> {
            // A failed run read left rows the selection needs out of the cache.
            if !self.idx.ready {
                return Err(FallbackReason::CacheUnavailable);
            }
            let selection = source::timing::phase("cache_selection");
            let rows = |n: usize| self.tables[n].rows.values().collect::<Vec<_>>();
            let threads: Vec<_> = rows(0)
                .into_iter()
                .filter_map(|r| match &r.data {
                    Data::Thread(t) if t.archived_at.is_none() && t.deleted_at.is_none() => {
                        Some(t.data.payload.clone())
                    }
                    _ => None,
                })
                .collect();
            let ids: HashSet<_> = threads
                .iter()
                .map(|t| t["id"].as_str().unwrap_or("").to_owned())
                .collect();
            report.threads = ids.len();
            let active = |n| {
                let mut r = rows(n);
                r.retain(|r| ids.contains(r.owner.as_ref()));
                r.sort_by(|a, b| a.owner.cmp(&b.owner).then(a.id.cmp(&b.id)));
                r
            };
            let mut latest = HashMap::new();
            let mut run_rows = active(1);
            run_rows.sort_by_key(|r| match &r.data {
                Data::Run(v) => std::cmp::Reverse(v.ordinal),
                _ => unreachable!(),
            });
            let mut ordinals = HashSet::new();
            for r in &run_rows {
                if let Data::Run(v) = &r.data {
                    if v.data.payload["threadId"].as_str() != Some(r.owner.as_ref()) {
                        return Err(FallbackReason::RunOwnerMismatch);
                    }
                    // A tie within one thread has no guaranteed latest run.
                    if !ordinals.insert((&r.owner, v.ordinal)) {
                        return Err(FallbackReason::RunOrdinalTie);
                    }
                    latest
                        .entry(v.data.payload["threadId"].as_str().unwrap_or("").to_owned())
                        .or_insert(v.data.payload.clone());
                }
            }
            let mut sub_rows = active(2);
            if sub_rows.len() > 1 {
                match sub_order {
                    Some(EncounterOrder::ThreadIndex(_)) => sub_rows
                        .sort_by(|a, b| a.owner.cmp(&b.owner).then(a.position.cmp(&b.position))),
                    Some(EncounterOrder::Rowid) => sub_rows.sort_by_key(|r| r.id),
                    None => return Err(FallbackReason::MultipleSubagents),
                }
            }
            let mut subs = Vec::new();
            for r in sub_rows {
                if let Data::Subagent(v) = &r.data {
                    // Typed events route past an empty provider alias; the
                    // oracle's JSON event router does not.
                    if v.payload.get("providerThreadId").and_then(Value::as_str) == Some("") {
                        return Err(FallbackReason::CrossThreadAttribution);
                    }
                    subs.push(v.payload.clone());
                }
            }
            let mut provider_ids = HashSet::new();
            for r in active(3) {
                if let Data::Provider(v) = &r.data {
                    if !provider_ids.insert(v.payload["id"].as_str().unwrap_or("")) {
                        return Err(FallbackReason::DuplicateProviderId);
                    }
                }
            }
            let providers = active(3)
                .into_iter()
                .filter_map(|r| match &r.data {
                    Data::Provider(v) => Some((
                        v.payload["id"].as_str().unwrap_or("").to_owned(),
                        v.payload.clone(),
                    )),
                    _ => None,
                })
                .collect();
            let mut turns = HashMap::new();
            let mut history = Vec::new();
            let mut turn_rows = active(4);
            turn_rows.sort_by(|a, b| match (&a.data, &b.data) {
                (Data::Turn(a), Data::Turn(b)) => a
                    .provider_thread_id
                    .cmp(&b.provider_thread_id)
                    .then(b.ordinal.cmp(&a.ordinal)),
                _ => unreachable!(),
            });
            let mut turn_keys = HashSet::new();
            let mut counts = HashMap::new();
            for r in turn_rows {
                if let Data::Turn(v) = &r.data {
                    // json_extract returns JSON objects/arrays as SQL text. Do
                    // not replace that history model value with a JSON object.
                    if v.model_bad {
                        return Err(FallbackReason::HistoryModelType);
                    }
                    if v.identity_bad {
                        return Err(FallbackReason::ProviderIdentityMismatch);
                    }
                    if !turn_keys.insert((v.provider_thread_id.as_ref(), v.ordinal)) {
                        return Err(FallbackReason::ProviderOrdinalTie);
                    }
                    let parsed = if self.lean {
                        v.lean_json()?
                    } else {
                        v.parsed().map_err(|_| FallbackReason::AssemblyError)?
                    };
                    turns
                        .entry(v.provider_thread_id.to_string())
                        .or_insert(parsed.clone());
                    let count = counts.entry(v.provider_thread_id.as_ref()).or_insert(0);
                    *count += 1;
                    if *count <= 160 && !self.lean {
                        history.push(json!({
                            "provider_thread_id": v.provider_thread_id.as_ref(),
                            "thread_id": r.owner,
                            "started_at": v.started_at.as_ref().map(Stamp::text_owned),
                            "completed_at": v.completed_at.as_ref().map(Stamp::text_owned),
                            "used": parsed["tokenUsage"]["usedTokens"],
                            "max": parsed["tokenUsage"]["maxTokens"],
                            "model": parsed["model"],
                        }));
                    }
                }
            }
            let mut node_ids = HashSet::new();
            for r in active(5) {
                if let Data::Node(v) = &r.data {
                    if !node_ids.insert(&v.node_id) {
                        return Err(FallbackReason::DuplicateNodeId);
                    }
                }
            }
            let nodes = active(5).into_iter().filter_map(|r| match &r.data { Data::Node(v) => Some((v.node_id.to_string(),json!({"node_id":v.node_id.as_ref(),"parent_node_id":v.parent_node_id.as_deref(),"run_id":Value::Null}))), _ => None }).collect();
            let projects = self.projects.values().map(|v| (v.project_id.clone(),json!({"project_id":v.project_id,"title":v.title,"workspace_root":v.workspace_root}))).collect();
            let mut requests = Vec::new();
            let mut request_rows = active(6);
            request_rows.retain(|r| matches!(&r.data, Data::Request(v) if v.resolved_at.is_none() && ["pending", "open", "waiting", "requested"].contains(&v.status.as_str())));
            if request_rows.len() > 1 {
                match request_order {
                    Some(EncounterOrder::ThreadIndex(_)) => request_rows
                        .sort_by(|a, b| a.owner.cmp(&b.owner).then(a.position.cmp(&b.position))),
                    Some(EncounterOrder::Rowid) => request_rows.sort_by_key(|r| r.id),
                    None => return Err(FallbackReason::MultipleRequests),
                }
            }
            for r in request_rows {
                if let Data::Request(v) = &r.data {
                    if v.resolved_at.is_none()
                        && ["pending", "open", "waiting", "requested"].contains(&v.status.as_str())
                    {
                        requests.push(json!({"thread_id":r.owner,"node_id":v.node_id,"kind":v.kind,"status":v.status,"created_at":v.created_at,"async_input":v.async_input}));
                    }
                }
            }
            let mut run_ids: HashSet<String> = latest
                .values()
                .filter_map(|v| {
                    v["id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                })
                .collect();
            run_ids.extend(subs.iter().filter_map(|v| {
                v["runId"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            }));
            let since = source::iso(now - source::WINDOW);
            let mut items = Vec::new();
            let mut events = Vec::new();
            let mut plans = Vec::new();
            // Items/plans may belong to an inactive thread if their run id is selected.
            for r in rows(7) {
                if let Data::Item(v) = &r.data {
                    let selected_run = v
                        .run_id
                        .as_ref()
                        .is_some_and(|id| run_ids.contains(id.as_ref()));
                    if (selected_run
                        || (v.run_id.is_none()
                            && ids.contains(r.owner.as_ref())
                            && !latest.contains_key(r.owner.as_ref())))
                        && source::is_toolish(v.kind.as_ref())
                    {
                        items.push((&**v, r));
                    }
                    if ids.contains(r.owner.as_ref())
                        && stamp_ge(&v.updated_at, &since)
                        && !matches!(v.kind.as_ref(), "checkpoint" | "todo_list")
                    {
                        events.push((&**v, r));
                    }
                    if selected_run && v.kind.as_ref() == "todo_list" {
                        plans.push((&**v, r));
                    }
                }
            }
            let ordering = source::timing::phase("cap_ordering");
            let runless_n = ids.iter().filter(|id| !latest.contains_key(*id)).count();
            let item_order = (run_ids.len() == self.idx.run_refs.len()
                && runless_n == self.idx.active.difference(&self.idx.latest_keys).count())
            .then_some(item_order)
            .flatten();
            match &item_order {
                Some(ItemOrder::RunOr { .. }) => items.sort_by(|(a, ra), (b, rb)| {
                    b.updated_at
                        .cmp(&a.updated_at)
                        .then(b.ordinal.cmp(&a.ordinal))
                        .then_with(|| {
                            let place = |item: &Item, row: &Row| {
                                if item
                                    .run_id
                                    .as_ref()
                                    .is_some_and(|id| run_ids.contains(id.as_ref()))
                                {
                                    (0u8, item.run_id.clone().unwrap_or_else(blank))
                                } else {
                                    (1u8, Arc::clone(&row.owner))
                                }
                            };
                            let (branch_a, group_a) = place(a, ra);
                            let (branch_b, group_b) = place(b, rb);
                            branch_a.cmp(&branch_b).then(group_a.cmp(&group_b))
                        })
                        .then(ra.id.cmp(&rb.id))
                }),
                Some(ItemOrder::Rowid) | None => items.sort_by(|(a, ra), (b, rb)| {
                    b.updated_at
                        .cmp(&a.updated_at)
                        .then(b.ordinal.cmp(&a.ordinal))
                        .then(ra.id.cmp(&rb.id))
                }),
            }
            // Stable timestamp sorting must start from the oracle scan order:
            // the LIMIT sorter uses encounter order for ties, including its cut.
            match &event_order {
                Some(EncounterOrder::ThreadIndex(_)) => {
                    events.sort_by(|(_, a), (_, b)| a.owner.cmp(&b.owner).then(a.position.cmp(&b.position)))
                }
                Some(EncounterOrder::Rowid) => events.sort_by_key(|(_, r)| r.id),
                None => {}
            }
            events.sort_by(|(a, _), (b, _)| b.updated_at.cmp(&a.updated_at));
            // The plans query has no LIMIT. Its sorter keeps the proved encounter
            // order among equal updated_at. The run set here is the one this sort
            // filters on; a classification explained for a different count is not used.
            let plan_order = (run_ids.len() == self.idx.run_refs.len()).then_some(plan_order).flatten();
            match &plan_order {
                Some(PlanOrder::RunOrdinal(_)) => plans.sort_by(|(a, ra), (b, rb)| {
                    b.updated_at.cmp(&a.updated_at)
                        .then_with(|| a.run_id.cmp(&b.run_id))
                        .then(a.ordinal.cmp(&b.ordinal))
                        .then(ra.id.cmp(&rb.id))
                }),
                Some(PlanOrder::Rowid) => plans.sort_by(|(a, ra), (b, rb)| {
                    b.updated_at.cmp(&a.updated_at).then(ra.id.cmp(&rb.id))
                }),
                None => plans.sort_by(|(a, _), (b, _)| b.updated_at.cmp(&a.updated_at)),
            }
            // Even ties away from the cut affect per-agent vector order.
            if item_order.is_none()
                && items.windows(2).any(|w| {
                    w[0].0.updated_at == w[1].0.updated_at
                        && w[0].0.ordinal == w[1].0.ordinal
                })
            {
                return Err(FallbackReason::ItemTimestampOrdinalTie);
            }
            if event_order.is_none()
                && events
                    .windows(2)
                    .any(|w| w[0].0.updated_at == w[1].0.updated_at)
            {
                return Err(FallbackReason::EventTimestampTie);
            }
            if plan_order.is_none()
                && plans
                    .windows(2)
                    .any(|w| w[0].0.updated_at == w[1].0.updated_at)
            {
                return Err(FallbackReason::PlanTimestampTie);
            }
            items.truncate(3000);
            events.truncate(8000);
            drop(ordering);
            let mut windowed: Vec<&Row> = run_rows
                .iter()
                .copied()
                .filter(|row| run_in_window(row, &since))
                .collect();
            // Explained for this active-thread count. A different `IN` length is
            // not the plan this order was proved against.
            let span_order = (ids.len() == self.active_count())
                .then_some(span_order)
                .flatten();
            order_windowed_runs(&mut windowed, span_order.as_ref())?;
            let mut spans = Vec::new();
            for r in windowed {
                if let Data::Run(v) = &r.data {
                    spans.push(json!({"thread_id":r.owner,"requested_at":v.requested_at,"completed_at":v.completed_at,"started_at":v.data.payload["startedAt"]}));
                }
            }
            let paint = |v: &Item, r: &Row| if self.lean { v.bare(&r.owner) } else { v.materialize(&r.owner) };
            let painted_items = items
                .iter()
                .map(|(v, r)| paint(v, r))
                .collect::<Result<Vec<_>, _>>()?;
            let painted_events = events
                .iter()
                .map(|(v, r)| paint(v, r))
                .collect::<Result<Vec<_>, _>>()?;
            if self.lean {
                // `status_view` drops the plan; the status-only cache holds no steps.
                plans.clear();
            }
            let items: Vec<_> = painted_items.iter().collect();
            let events: Vec<_> = painted_events.iter().collect();
            report.valid_until = source::deadline_stamps(
                events
                    .iter()
                    .map(|v| v.updated_at.as_str())
                    .chain(spans.iter().filter_map(|v| v["completed_at"].as_str())),
                now,
            );
            let mut plan_rows = Vec::with_capacity(plans.len());
            for (v, r) in plans {
                let text = v
                    .payload
                    .as_ref()
                    .and_then(|p| p.text.as_ref())
                    .ok_or(FallbackReason::TextDrift)?;
                plan_rows.push(json!({
                    "thread_id": r.owner.as_ref(),
                    "run_id": v.run_id.as_deref(),
                    "node_id": v.node_id.as_deref(),
                    "provider_thread_id": v.provider_thread_id.as_deref(),
                    "updated_at": v.updated_at.text_owned(),
                    "steps": text.steps.as_deref(),
                }));
            }
            let plans = plan_rows;
            drop(selection);
            let mut snapshot = source::assemble(
                threads, latest, subs, providers, turns, nodes, projects, requests, items, events,
                history, spans, plans, path, now,
            )
            .map_err(|_| FallbackReason::AssemblyError)?;
            if self.lean {
                view_snapshot(&mut snapshot);
            }
            Ok(snapshot)
        }
        pub fn apply(&mut self, pending: Pending) {
            if pending.full {
                let pool = std::mem::take(&mut self.pool);
                let drift = self.force_text_drift;
                *self = Self::default();
                self.pool = pool;
                self.force_text_drift = drift;
            } else {
                self.forget_owner_text(&pending.owners);
            }
            for (table, rows) in self.tables.iter_mut().zip(pending.tables) {
                for owner in &pending.owners {
                    table.remove_owner(owner);
                }
                for row in rows {
                    table.insert(row);
                }
            }
            self.projects = pending.projects;
        }
        fn forget_owner_text(&mut self, owners: &BTreeSet<String>) {
            let mut drop_ids = Vec::new();
            for owner in owners {
                if let Some(ids) = self.tables[7].owners.get(owner.as_str()) {
                    drop_ids.extend(ids.iter().copied());
                }
            }
            for id in drop_ids {
                self.text_held.remove(&id);
            }
        }
        fn reindex(&mut self, now: f64) {
            let mut idx = Select::default();
            idx.since = source::iso(now - source::WINDOW);
            idx.store_elig(eligibility(&self.tables));
            for row in self.tables[7].rows.values() {
                idx.index_item_bulk(row);
            }
            idx.item_prefix = idx.items.iter().take(3000).cloned().collect();
            idx.rebuild_event_prefix();
            idx.ready = true;
            self.idx = idx;
            self.nodes = NodeIndex::default();
            self.nodes.map.reserve(self.tables[5].rows.len());
            self.provider_rows.clear();
            self.turn_rows.clear();
            self.bad_turns.clear();
            for index in [3, 4, 5] {
                for row in self.tables[index].rows.values() {
                    note_index(
                        &mut self.nodes,
                        &mut self.provider_rows,
                        &mut self.turn_rows,
                        &mut self.bad_turns,
                        &mut self.pool,
                        row,
                        true,
                    );
                }
            }
            self.changes = Changes::default();
        }
        pub(super) fn index_ready(&self) -> bool {
            self.idx.ready
        }
        pub(super) fn mark_inputs_stale(&mut self) {
            self.idx.inputs_ready = false;
            self.shells.clear();
            self.routes = None;
            self.touched = 0;
            self.projects_changed = false;
            self.changes = Changes::default();
        }
        /// Apply a read in place. The undo is the previous rows of the owners
        /// that changed. A full refresh releases the previous cache first.
        pub(super) fn stage(&mut self, db: &Connection, pending: Pending, now: f64, inline_ok: bool) -> Undo {
            if pending.full || !self.idx.ready {
                let pool = std::mem::take(&mut self.pool);
                let drift = self.force_text_drift;
                let lean = self.lean;
                drop(std::mem::take(self));
                self.pool = pool;
                self.force_text_drift = drift;
                self.lean = lean;
                let mut tables = pending.tables;
                let items = std::mem::take(&mut tables[7]);
                for (table, rows) in self.tables.iter_mut().zip(tables) {
                    *table = Table::build(rows);
                }
                // Eligibility reads threads, runs and subagents only.
                let mut gate = Select::default();
                gate.since = source::iso(now - source::WINDOW);
                gate.store_elig(eligibility(&self.tables));
                self.tables[7] = Table::build(items.into_iter().filter(|row| gate.keeps(row)).collect());
                self.projects = pending.projects;
                self.reindex(now);
                return Undo {
                    replaced: true,
                    owners: BTreeSet::new(),
                    rows: Vec::new(),
                    fresh: Vec::new(),
                    projects: BTreeMap::new(),
                    read: 0,
                };
            }
            self.touched = 0;
            self.projects_changed = false;
            self.changes = Changes::default();
            let rolled = self.idx.roll_since(now, &self.tables[7].rows);
            // Leave identical rows where they are. Reindexing every item of a
            // changed thread was most of the stage, and those rows had not moved.
            let mut saved = Vec::new();
            let mut fresh = Vec::new();
            let mut seen = HashSet::new();
            let mut reindex = Vec::new();
            // Every table but turn items first: they decide eligibility, which
            // decides which turn-item rows the cache keeps.
            let mut tables = pending.tables;
            let item_rows = std::mem::take(&mut tables[7]);
            // A run absent from a refreshed owner may have moved, rather than
            // been deleted. Its new owner need not be named in the event suffix.
            // Recover that row by identity before recomputing latest runs.
            let returned: HashSet<_> = tables[1].iter().map(|row| row.id).collect();
            let missing: Vec<_> = pending.owners.iter()
                .filter_map(|owner| self.tables[1].owners.get(owner.as_str()))
                .flat_map(|ids| ids.iter().copied())
                .filter(|id| !returned.contains(id)).collect();
            let mut moved_read = 0;
            if !missing.is_empty() {
                let sql = format!("{} WHERE rowid=?", queries()[1].1);
                for id in missing {
                    match poll1::load_rows(db, Path::new(""), 1, &sql, &[SqlValue::Integer(id)], &mut self.pool) {
                        Ok(rows) => {
                            moved_read += rows.len();
                            tables[1].extend(rows);
                        }
                        Err(_) => self.idx.ready = false,
                    }
                }
            }
            for (index, rows) in tables.into_iter().enumerate().take(7) {
                for row in rows {
                    seen.insert((index, row.id));
                    // Encounter position is read only for subagents, requests
                    // and turn items. A baseline scans the other tables in rowid
                    // order and a re-read walks the owner's index, so their
                    // positions differ without any change in the row.
                    let ordered = matches!(index, 2 | 6);
                    let same = self.tables[index].rows.get(&row.id).is_some_and(|previous| {
                        previous.owner == row.owner
                            && (!ordered || previous.position == row.position)
                            && same_data(&previous.data, &row.data)
                    });
                    if same {
                        continue;
                    }
                    let previous = self.tables[index].take(row.id);
                    note_change(&mut self.changes, &mut self.pool, index, previous.as_ref(), Some(&row));
                    if let Some(previous) = &previous {
                        note_index(&mut self.nodes, &mut self.provider_rows, &mut self.turn_rows, &mut self.bad_turns, &mut self.pool, previous, false);
                    }
                    note_index(&mut self.nodes, &mut self.provider_rows, &mut self.turn_rows, &mut self.bad_turns, &mut self.pool, &row, true);
                    match previous {
                        Some(previous) => saved.push((index, previous)),
                        None => fresh.push((index, row.id)),
                    }
                    self.touched |= 1 << index;
                    self.tables[index].insert(row);
                }
            }
            for index in 0..7 {
                self.doom_unseen(index, &pending.owners, &seen, &mut saved);
            }
            let projects = std::mem::replace(&mut self.projects, pending.projects);
            self.projects_changed = !same_projects(&projects, &self.projects);
            // A thread row that changed only in its own display fields cannot
            // move the active set, a latest run or a selected run id.
            let eligibility_moved = self.changes.links || self.touched & (TOUCH_RUN | TOUCH_SUB) != 0;
            let (old_refs, old_active, old_latest) = if eligibility_moved {
                let old = (self.idx.run_refs.clone(), self.idx.active.clone(), self.idx.latest_keys.clone());
                self.idx.store_elig(eligibility(&self.tables));
                old
            } else {
                (HashSet::new(), HashSet::new(), HashSet::new())
            };
            let mut unkept = Vec::new();
            for row in item_rows {
                seen.insert((7, row.id));
                let same = self.tables[7].rows.get(&row.id).is_some_and(|previous| {
                    previous.owner == row.owner
                        && previous.position == row.position
                        && same_data(&previous.data, &row.data)
                });
                if same {
                    if !self.idx.keeps(&row) {
                        unkept.push(row.id);
                    }
                    continue;
                }
                let keep = self.idx.keeps(&row);
                if !keep && !self.tables[7].rows.contains_key(&row.id) {
                    // Not cached before or after, and no selection can take it.
                    continue;
                }
                // The replacement is keys only. A later text read fills it.
                self.text_held.remove(&row.id);
                let previous = self.tables[7].take(row.id);
                note_change(&mut self.changes, &mut self.pool, 7, previous.as_ref(), Some(&row));
                match previous {
                    Some(previous) => {
                        self.idx.unindex_item(&previous);
                        saved.push((7, previous));
                    }
                    None => fresh.push((7, row.id)),
                }
                self.touched |= 1 << 7;
                if keep {
                    reindex.push(row.id);
                    self.tables[7].insert(row);
                }
            }
            self.doom_unseen(7, &pending.owners, &seen, &mut saved);
            let mut read = moved_read;
            let mut evict: Vec<i64> = rolled;
            evict.extend(unkept);
            if eligibility_moved {
                let gained: Vec<String> = self.idx.run_refs.difference(&old_refs).cloned().collect();
                let lost: Vec<String> = old_refs.difference(&self.idx.run_refs).cloned().collect();
                // A newly selected run's rows may not be cached: read them.
                match self.fetch_runs(db, &gained, inline_ok) {
                    Ok(rows) => {
                        read += rows.len();
                        for row in rows {
                            // A cached row is this transaction's version. A row
                            // `keeps` turns down is not one the run's selection takes.
                            if self.tables[7].rows.contains_key(&row.id) || !self.idx.keeps(&row) {
                                continue;
                            }
                            fresh.push((7, row.id));
                            reindex.push(row.id);
                            self.tables[7].insert(row);
                        }
                    }
                    Err(_) => {
                        // Rows the selection needs are missing. An unready
                        // cache makes this poll and the next read in full.
                        self.idx.ready = false;
                    }
                }
                for run in &lost {
                    for map in [&self.idx.by_run, &self.idx.plan_by_run] {
                        if let Some(ids) = map.get(run.as_str()) {
                            evict.extend(ids.iter().copied());
                        }
                    }
                }
                self.idx.apply_run_diff(&gained, &lost, &self.tables[7].rows);
                // Unchanged runless rows change eligibility when their owner
                // gains its first run or loses its last. They stay cached even
                // while hidden by a latest run; update the candidate tree too,
                // so removing one can refill the cap from an untouched owner.
                let changed_latest: Vec<_> = old_latest.symmetric_difference(&self.idx.latest_keys).cloned().collect();
                for owner in changed_latest {
                    let ids = self.idx.runless.get(&owner).cloned().unwrap_or_default();
                    for id in ids {
                        let Some(row) = self.tables[7].rows.get(&id) else { continue };
                        let Data::Item(v) = &row.data else { continue };
                        if self.idx.item_visible(&row.owner, &v.kind, v.run_id.as_deref()) {
                            self.idx.insert_item(row);
                        } else {
                            self.idx.remove_item(row);
                        }
                    }
                }
                let left: Vec<String> = old_active.difference(&self.idx.active).cloned().collect();
                let joined: Vec<String> = self.idx.active.difference(&old_active).cloned().collect();
                let items = &self.tables[7];
                let idx = &mut self.idx;
                for owner in &left {
                    if let Some(ids) = items.owners.get(owner.as_str()) {
                        idx.forget_inactive(ids, &items.rows);
                        evict.extend(ids.iter().copied());
                    }
                }
                for owner in &joined {
                    if let Some(ids) = items.owners.get(owner.as_str()) {
                        for id in ids {
                            if let Some(row) = items.rows.get(id) {
                                idx.index_item(row);
                            }
                        }
                    }
                }
            }
            // In-window runs of active threads: runs and the active set.
            if self.changes.links || self.touched & TOUCH_RUN != 0 {
                self.idx.span_floor_ready = false;
            }
            {
                let items = &self.tables[7];
                let idx = &mut self.idx;
                for id in reindex {
                    if let Some(row) = items.rows.get(&id) {
                        idx.index_item(row);
                    }
                }
            }
            for row in self.evict(evict) {
                saved.push((7, row));
            }
            Undo {
                replaced: false,
                owners: pending.owners,
                rows: saved,
                fresh,
                projects,
                read,
            }
        }
        /// Rows of re-read owners that the read no longer returned.
        fn doom_unseen(
            &mut self,
            index: usize,
            owners: &BTreeSet<String>,
            seen: &HashSet<(usize, i64)>,
            saved: &mut Vec<(usize, Row)>,
        ) {
            let mut doomed = Vec::new();
            for owner in owners {
                let Some(ids) = self.tables[index].owners.get(owner.as_str()) else {
                    continue;
                };
                for id in ids {
                    if !seen.contains(&(index, *id)) {
                        doomed.push(*id);
                    }
                }
            }
            for id in doomed {
                if let Some(previous) = self.tables[index].take(id) {
                    note_change(&mut self.changes, &mut self.pool, index, Some(&previous), None);
                    note_index(&mut self.nodes, &mut self.provider_rows, &mut self.turn_rows, &mut self.bad_turns, &mut self.pool, &previous, false);
                    if index == 7 {
                        self.idx.unindex_item(&previous);
                        self.text_held.remove(&id);
                    }
                    self.touched |= 1 << index;
                    saved.push((index, previous));
                }
            }
        }
        /// Every turn-item row of each run, keys only, by run id: the columns an
        /// owner read takes. Their encounter position is unknown; the one kept is
        /// the row's place in this read. It is read only for the event cap, and a
        /// row the event cap can take is never missing from the cache (`keeps`).
        fn fetch_runs(&mut self, db: &Connection, runs: &[String], inline_ok: bool) -> rusqlite::Result<Vec<Row>> {
            let mut out = Vec::new();
            if runs.is_empty() {
                return Ok(out);
            }
            let sql = if inline_ok {
                poll1::run_inline_query()
            } else {
                run_query()
            };
            for run in runs {
                out.extend(poll1::item_rows_scoped(
                    db,
                    &sql,
                    &[SqlValue::Text(run.clone())],
                    inline_ok,
                    &mut self.pool,
                    None,
                )?);
            }
            Ok(out)
        }
        /// Release the rows among `ids` that `keeps` no longer holds, with their
        /// index entries. A row that was selected on the last poll is reported
        /// leaving its selection by `assemble`'s membership diff, which reads
        /// no row.
        fn evict(&mut self, mut ids: Vec<i64>) -> Vec<Row> {
            ids.sort_unstable();
            ids.dedup();
            let mut gone = Vec::new();
            for id in ids {
                let doomed = self.tables[7].rows.get(&id).is_some_and(|row| !self.idx.keeps(row));
                if !doomed {
                    continue;
                }
                if let Some(row) = self.tables[7].take(id) {
                    self.idx.unindex_item(&row);
                    self.text_held.remove(&id);
                    gone.push(row);
                }
            }
            gone
        }
        /// Load label, command, file and steps for rows inside a cap, and drop
        /// that text everywhere else. A keys-only stage leaves `body` empty, so
        /// a row that just entered a selection is read here, by rowid, with the
        /// oracle's allowlisted expressions. Refreshed owners are re-read even
        /// when their keys did not change: a payload-only text edit is invisible
        /// to `same_data`.
        pub(super) fn sync_text(
            &mut self,
            db: &Connection,
            path: &Path,
            refreshed: &BTreeSet<String>,
        ) -> Result<usize, FallbackReason> {
            if !self.idx.ready {
                return Ok(0);
            }
            let mut selected = HashSet::new();
            selected.extend(self.idx.item_prefix.iter().map(|key| key.rowid));
            selected.extend(self.idx.event_prefix.iter().copied());
            if !self.lean {
                selected.extend(self.idx.plan_at.keys().copied());
            }
            let stale: Vec<i64> = self
                .text_held
                .iter()
                .copied()
                .filter(|id| !selected.contains(id))
                .collect();
            for id in stale {
                self.text_held.remove(&id);
                if let Some(row) = self.tables[7].rows.get_mut(&id) {
                    if let Data::Item(item) = &mut row.data {
                        item.payload = None;
                    }
                }
            }
            // Hash lookups only. Newly selected rows are the ones not yet in
            // text_held. Refreshed owners are re-read from that owner's id set,
            // because a payload change (including startedAt/tool/exitCode) can
            // leave inline columns unchanged.
            let mut need = Vec::new();
            for id in &selected {
                if !self.text_held.contains(id) && self.tables[7].rows.contains_key(id) {
                    need.push(*id);
                }
            }
            for owner in refreshed {
                let Some(ids) = self.tables[7].owners.get(owner.as_str()) else {
                    continue;
                };
                for id in ids {
                    if selected.contains(id) {
                        need.push(*id);
                    }
                }
            }
            let forced = self.force_text_drift;
            self.force_text_drift = false;
            if forced && need.is_empty() {
                if let Some(id) = selected.iter().copied().next() {
                    need.push(id);
                }
            }
            if need.is_empty() {
                return Ok(0);
            }
            need.sort_unstable();
            need.dedup();
            let mut seen = HashSet::new();
            let mut rows_back = 0usize;
            let mut force_next = forced;
            for chunk in need.chunks(800) {
                let found = poll1::load_item_payload(db, path, chunk, self.lean)?;
                rows_back += found.len();
                for read in found {
                    seen.insert(read.rowid);
                    let rowid = read.rowid;
                    let drifted = match self.tables[7].rows.get_mut(&rowid) {
                        Some(row) => match &mut row.data {
                            Data::Item(item) => {
                                if force_next || !item.matches_inline(&row.owner, &read) {
                                    item.payload = None;
                                    true
                                } else {
                                    // A payload-only edit keeps the inline keys. The agent
                                    // that shows this row still has to be repainted.
                                    let fresh = ItemPayload {
                                        started_at: read.started_at,
                                        tool: read.tool.as_deref().map(|t| self.pool.intern(t)),
                                        exit_code: read.exit_code,
                                        text: read.text,
                                    };
                                    if item.payload.as_deref() != Some(&fresh) {
                                        self.changes.items.insert(rowid);
                                    }
                                    item.payload = Some(Box::new(fresh));
                                    false
                                }
                            }
                            _ => true,
                        },
                        None => true,
                    };
                    if drifted {
                        self.text_held.remove(&rowid);
                        return Err(FallbackReason::TextDrift);
                    }
                    force_next = false;
                    self.text_held.insert(rowid);
                }
            }
            if seen.len() != need.len() {
                for id in &need {
                    if seen.contains(id) {
                        continue;
                    }
                    if let Some(row) = self.tables[7].rows.get_mut(id) {
                        if let Data::Item(item) = &mut row.data {
                            item.payload = None;
                        }
                    }
                    self.text_held.remove(id);
                }
                return Err(FallbackReason::TextDrift);
            }
            Ok(rows_back)
        }
        pub(super) fn arm_text_drift(&mut self) {
            self.force_text_drift = true;
        }
        pub(super) fn pool_mut(&mut self) -> &mut Pool {
            &mut self.pool
        }
        pub(super) fn read_pending(
            &mut self,
            db: &Connection,
            path: &Path,
            owners: Option<&BTreeSet<String>>,
            now: f64,
            inline_ok: bool,
        ) -> Result<Pending, SourceError> {
            let idx_and_items = owners
                .is_some()
                .then_some((&self.idx, &self.tables[7].rows));
            Pending::read_scoped(
                db,
                path,
                owners,
                &mut self.pool,
                self.lean,
                inline_ok,
                Some(now),
                idx_and_items,
            )
        }
        pub(super) fn restore(&mut self, undo: Undo) {
            if undo.replaced {
                // Nothing to go back to. An unready cache makes the next poll
                // read in full, which a failed full poll does in any case.
                let pool = std::mem::take(&mut self.pool);
                let drift = self.force_text_drift;
                let lean = self.lean;
                *self = Self::default();
                self.pool = pool;
                self.force_text_drift = drift;
                self.lean = lean;
                return;
            }
            for (table, id) in undo.fresh {
                let _ = self.tables[table].take(id);
            }
            for (table, row) in undo.rows {
                self.tables[table].insert(row);
            }
            self.projects = undo.projects;
            // The index ops are not reversed. The next poll rebuilds them,
            // including the node and provider indexes (`reindex`).
            self.idx.ready = false;
            self.idx.inputs_ready = false;
            self.shells.clear();
            self.routes = None;
            self.touched = 0;
            self.projects_changed = false;
            self.changes = Changes::default();
        }
        fn align_events(&mut self, mode: Option<EncounterOrder>) {
            if self.idx.event_mode_known && self.idx.event_mode == mode {
                return;
            }
            let ids: Vec<i64> = self.idx.event_ids.iter().copied().collect();
            self.idx.events.clear();
            self.idx.event_at.clear();
            self.idx.event_mode = mode.clone();
            self.idx.event_mode_known = true;
            for id in ids {
                let key = {
                    let Some(row) = self.tables[7].rows.get(&id) else { continue };
                    let Data::Item(v) = &row.data else { continue };
                    stamp_key(mode.as_ref(), row, &v.updated_at)
                };
                self.idx.event_at.insert(id, key.clone());
                self.idx.events.insert(key, id);
            }
            self.idx.rebuild_event_prefix();
        }
        fn align_plans(&mut self, mode: Option<PlanOrder>) {
            if self.idx.plan_mode_known && self.idx.plan_mode == mode {
                return;
            }
            let ids: Vec<i64> = self.idx.plan_at.keys().copied().collect();
            self.idx.plans.clear();
            self.idx.plan_at.clear();
            self.idx.plan_mode = mode.clone();
            self.idx.plan_mode_known = true;
            for id in ids {
                let key = {
                    let Some(row) = self.tables[7].rows.get(&id) else {
                        continue;
                    };
                    let Data::Item(v) = &row.data else { continue };
                    plan_key(mode.as_ref(), row, &v.updated_at)
                };
                self.idx.plan_at.insert(id, key.clone());
                self.idx.plans.insert(key, id);
            }
        }
        fn align_items(&mut self, mode: Option<ItemOrder>) {
            if self.idx.item_mode_known && self.idx.item_mode == mode {
                return;
            }
            let ids: Vec<i64> = self.idx.item_ids.iter().copied().collect();
            self.idx.items.clear();
            self.idx.item_ids.clear();
            self.idx.item_prefix.clear();
            self.idx.item_mode = mode.clone();
            self.idx.item_mode_known = true;
            for id in ids {
                let key = {
                    let Some(row) = self.tables[7].rows.get(&id) else {
                        continue;
                    };
                    item_key(mode.as_ref(), row)
                };
                self.idx.item_ids.insert(id);
                self.idx.items.insert(key);
            }
            self.idx.item_prefix = self.idx.items.iter().take(3000).cloned().collect();
        }
        /// Active subagent rows in the oracle's encounter order.
        fn ordered_subs(&self, sub_order: Option<&EncounterOrder>) -> Result<Vec<&Row>, FallbackReason> {
            let mut rows: Vec<&Row> = self.tables[2]
                .rows
                .values()
                .filter(|row| self.idx.active.contains(row.owner.as_ref()))
                .collect();
            if rows.len() > 1 {
                match sub_order {
                    Some(EncounterOrder::ThreadIndex(_)) => {
                        rows.sort_by(|a, b| a.owner.cmp(&b.owner).then(a.position.cmp(&b.position)))
                    }
                    Some(EncounterOrder::Rowid) => rows.sort_by_key(|row| row.id),
                    None => return Err(FallbackReason::MultipleSubagents),
                }
            }
            Ok(rows)
        }
        /// The `source::Attributor` maps for the sealed shells, built in the
        /// oracle's agent and subagent order so every first-wins and overwrite
        /// rule resolves the same way. The node walk reads the node index.
        fn build_routes(&self, sub_order: Option<&EncounterOrder>) -> Result<Routes, FallbackReason> {
            let active = &self.idx.active;
            let mut native: HashMap<String, String> = HashMap::new();
            for agent in &self.shells {
                if agent.kind == "subagent" {
                    if let Some(pid) = agent.provider_id.as_ref().filter(|id| !id.is_empty()) {
                        native.entry(pid.clone()).or_insert_with(|| agent.id.clone());
                    }
                }
            }
            let mut sub_ids = HashSet::new();
            let mut aliases = HashMap::new();
            let mut sub_provider = Vec::new();
            let mut parent_nodes = Vec::new();
            for row in self.ordered_subs(sub_order)? {
                let Data::Subagent(v) = &row.data else { continue };
                if let Some(parent) = v.payload.get("parentNodeId").and_then(Value::as_str) {
                    parent_nodes.push(parent);
                }
                let id = source::str_at(&v.payload, "id");
                if id.is_empty() {
                    continue;
                }
                sub_ids.insert(id.to_owned());
                if let Some(child) = source::truthy_str(&v.payload, "childThreadId") {
                    aliases.insert(id.to_owned(), child);
                }
                if let Some(pid) = v.payload.get("providerThreadId").and_then(Value::as_str).filter(|id| !id.is_empty()) {
                    sub_provider.push((id.to_owned(), pid.to_owned()));
                }
            }
            for agent in &self.shells {
                for id in agent.subagent_ids.iter().flatten() {
                    aliases.insert(id.clone(), agent.id.clone());
                }
            }
            for (id, pid) in sub_provider {
                if let Some(found) = walk_owner(&self.nodes, active, &sub_ids, &aliases, Some(&id)) {
                    native.entry(pid).or_insert(found);
                }
            }
            let mut sub_chain = HashSet::new();
            for parent in parent_nodes {
                walk_ids(&self.nodes, active, &sub_ids, Some(parent), &mut sub_chain);
            }
            let mut native_for: HashMap<String, Vec<String>> = HashMap::new();
            for (pid, agent) in &native {
                native_for.entry(agent.clone()).or_default().push(pid.clone());
            }
            for list in native_for.values_mut() {
                list.sort();
            }
            let mut consumers: HashMap<String, Vec<String>> = HashMap::new();
            let mut position = HashMap::new();
            for (index, agent) in self.shells.iter().enumerate() {
                position.insert(agent.id.clone(), index);
                if let Some(pid) = agent.provider_id.as_ref().filter(|id| !id.is_empty()) {
                    consumers.entry(pid.clone()).or_default().push(agent.id.clone());
                }
            }
            Ok(Routes {
                native,
                aliases,
                sub_ids,
                native_for,
                consumers,
                sub_chain,
                position,
            })
        }
        /// Cutoff of the capped events and in-window runs. Canonical stamps are
        /// ordered, so the oldest selected event is the last prefix entry.
        fn selection_deadline(&mut self, event_ids: &[i64], now: f64) -> f64 {
            if source::iso(now - source::WINDOW).len() != 24 {
                return now;
            }
            let mut oldest = f64::INFINITY;
            let mut bad = false;
            let consider = |text: &str, oldest: &mut f64, bad: &mut bool| {
                match canonical_stamp(text).then(|| crate::model::ms(Some(text))).flatten() {
                    Some(stamp) => *oldest = oldest.min(stamp),
                    None => *bad = true,
                }
            };
            if self.idx.bad_events == 0 {
                if let Some(id) = event_ids.last() {
                    if let Some(text) = self.item_stamp(*id) {
                        consider(&text, &mut oldest, &mut bad);
                    }
                }
            } else {
                for id in event_ids {
                    if bad {
                        break;
                    }
                    if let Some(text) = self.item_stamp(*id) {
                        consider(&text, &mut oldest, &mut bad);
                    }
                }
            }
            if !bad {
                self.ensure_span_floor();
                if self.idx.span_floor_bad {
                    bad = true;
                } else {
                    oldest = oldest.min(self.idx.span_floor_ms);
                }
            }
            if bad {
                now
            } else if oldest.is_infinite() {
                f64::INFINITY
            } else {
                oldest + source::WINDOW + 1.
            }
        }
        fn item_stamp(&self, id: i64) -> Option<String> {
            match &self.tables[7].rows.get(&id)?.data {
                Data::Item(v) => Some(v.updated_at.text_owned()),
                _ => None,
            }
        }
        fn ensure_span_floor(&mut self) {
            if self.idx.span_floor_ready {
                return;
            }
            let mut oldest = f64::INFINITY;
            let mut bad = false;
            let since = self.idx.since.clone();
            for row in self.tables[1].rows.values() {
                if !self.idx.active.contains(row.owner.as_ref()) {
                    continue;
                }
                let Data::Run(v) = &row.data else { continue };
                let Some(done) = v.completed_at.as_deref() else { continue };
                if done < since.as_str() {
                    continue;
                }
                match canonical_stamp(done).then(|| crate::model::ms(Some(done))).flatten() {
                    Some(stamp) => oldest = oldest.min(stamp),
                    None => {
                        bad = true;
                        break;
                    }
                }
            }
            self.idx.span_floor_bad = bad;
            self.idx.span_floor_ms = oldest;
            self.idx.span_floor_ready = true;
        }
        /// `Attributor::item_target` (and its JSON `target` for events and plans:
        /// no subagent carries an empty provider id, so the two agree).
        fn route_row(
            &self,
            routes: &Routes,
            provider_thread_id: Option<&str>,
            node_id: Option<&str>,
            thread_id: &str,
        ) -> String {
            provider_thread_id
                .filter(|id| !id.is_empty())
                .and_then(|pid| routes.native.get(pid).cloned())
                .or_else(|| walk_owner(&self.nodes, &self.idx.active, &routes.sub_ids, &routes.aliases, node_id))
                .unwrap_or_else(|| thread_id.to_owned())
        }
        fn item_target(&self, routes: &Routes, id: i64) -> Option<String> {
            let row = self.tables[7].rows.get(&id)?;
            let Data::Item(v) = &row.data else { return None };
            Some(self.route_row(
                routes,
                v.provider_thread_id.as_deref(),
                v.node_id.as_deref(),
                row.owner.as_ref(),
            ))
        }
        fn latest_run(&self, thread_id: &str) -> Result<Value, FallbackReason> {
            let Some(ids) = self.tables[1].owners.get(thread_id) else {
                return Ok(json!({}));
            };
            let mut seen = HashSet::new();
            let mut best: Option<(i64, i64)> = None;
            for id in ids {
                let Data::Run(v) = &self.tables[1].rows[id].data else { continue };
                if v.data.payload["threadId"].as_str() != Some(thread_id) {
                    return Err(FallbackReason::RunOwnerMismatch);
                }
                if !seen.insert(v.ordinal) {
                    return Err(FallbackReason::RunOrdinalTie);
                }
                if best.map(|(ordinal, _)| v.ordinal > ordinal).unwrap_or(true) {
                    best = Some((v.ordinal, *id));
                }
            }
            Ok(best
                .map(|(_, id)| match &self.tables[1].rows[&id].data {
                    Data::Run(v) => v.data.payload.clone(),
                    _ => json!({}),
                })
                .unwrap_or_else(|| json!({})))
        }
        /// The oracle's `providers[pid]`: the one active provider row with that
        /// payload id. Two such rows would make the oracle's map order decide.
        fn provider_payload(&self, pid: &str) -> Result<Option<Value>, FallbackReason> {
            let Some(ids) = self.provider_rows.get(pid) else {
                return Ok(None);
            };
            let mut found = None;
            for id in ids {
                let Some(row) = self.tables[3].rows.get(id) else { continue };
                if !self.idx.active.contains(row.owner.as_ref()) {
                    continue;
                }
                let Data::Provider(v) = &row.data else { continue };
                if found.is_some() {
                    return Err(FallbackReason::DuplicateProviderId);
                }
                found = Some(v.payload.clone());
            }
            Ok(found)
        }
        /// Every active turn of one provider, newest first, from any thread: the
        /// oracle's `PARTITION BY provider_thread_id ORDER BY ordinal DESC`.
        /// Ordinal ties and the guarded payload shapes are refused.
        fn provider_turns(&self, pid: &str) -> Result<Vec<&Row>, FallbackReason> {
            let Some(ids) = self.turn_rows.get(pid) else {
                return Ok(Vec::new());
            };
            let mut rows = Vec::with_capacity(ids.len());
            for id in ids {
                let Some(row) = self.tables[4].rows.get(id) else { continue };
                if !self.idx.active.contains(row.owner.as_ref()) {
                    continue;
                }
                let Data::Turn(v) = &row.data else { continue };
                if v.model_bad {
                    return Err(FallbackReason::HistoryModelType);
                }
                if v.identity_bad {
                    return Err(FallbackReason::ProviderIdentityMismatch);
                }
                rows.push((v.ordinal, row));
            }
            rows.sort_by_key(|(ordinal, _)| Reverse(*ordinal));
            if rows.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                return Err(FallbackReason::ProviderOrdinalTie);
            }
            Ok(rows.into_iter().map(|(_, row)| row).collect())
        }
        /// The oracle's `turns[pid]`, the newest active turn of that provider.
        fn latest_turn(&self, pid: &str) -> Result<Option<Value>, FallbackReason> {
            match self.provider_turns(pid)?.first() {
                Some(row) => match &row.data {
                    Data::Turn(v) => Ok(Some(if self.lean {
                        v.lean_json()?
                    } else {
                        v.parsed().map_err(|_| FallbackReason::AssemblyError)?
                    })),
                    _ => Ok(None),
                },
                None => Ok(None),
            }
        }
        /// A turn whose payload provider id disagrees with its column would be
        /// keyed differently by the oracle's latest-turn map. Refused anywhere.
        fn turn_identity_bad(&self) -> bool {
            self.bad_turns.iter().any(|id| {
                self.tables[4]
                    .rows
                    .get(id)
                    .is_some_and(|row| self.idx.active.contains(row.owner.as_ref()))
            })
        }
        /// The oracle's parent graph over the cached rows, by the oracle's own
        /// `source::link_structure` and `source::seal_parents`. Returns the new
        /// shells, the previous `Arc` wherever a shell is unchanged, and the ids
        /// of shells that are new or changed.
        ///
        /// `link_structure` reads a thread agent's id, project and subagent ids
        /// and writes its parent, kind and subagent ids; it reads nothing else of
        /// a thread agent. So it runs over stubs holding exactly what
        /// `thread_agent` puts in those fields, and each thread's shell is its
        /// `thread_agent` with the stub's link fields: rebuilt by `thread_shell`
        /// when the thread's own inputs changed, otherwise the sealed shell.
        /// The other inputs are the oracle's, narrowed only where
        /// `link_structure` cannot look: providers and turns for the agents it
        /// builds from subagent rows, nodes on each subagent's `parentNodeId`
        /// chain, and the threads with a valid `delegatedFrom`, in thread order.
        fn relink(
            &self,
            path: &Path,
            sub_order: Option<&EncounterOrder>,
            fresh: &BTreeSet<String>,
            all: bool,
        ) -> Result<(Vec<Arc<crate::model::Agent>>, BTreeSet<String>), FallbackReason> {
            let old: HashMap<&str, &Arc<crate::model::Agent>> =
                self.shells.iter().map(|agent| (agent.id.as_str(), agent)).collect();
            let mut stubs = Vec::new();
            let mut built: Vec<Option<crate::model::Agent>> = Vec::new();
            let mut delegated = Vec::new();
            let mut thread_ids: HashSet<&str> = HashSet::new();
            for row in self.tables[0].rows.values() {
                let Data::Thread(thread) = &row.data else { continue };
                if !thread_active(thread) {
                    continue;
                }
                let payload = &thread.data.payload;
                // The cache keys a thread's rows by its thread_id column; the
                // oracle names the agent by the payload id.
                if row.owner.is_empty() || payload.get("id").and_then(Value::as_str) != Some(row.owner.as_ref()) {
                    return Err(FallbackReason::ThreadIdentityMismatch);
                }
                thread_ids.insert(row.owner.as_ref());
                let kept = old.get(row.owner.as_ref()).filter(|prior| {
                    !all && !fresh.contains(row.owner.as_ref()) && prior.id == prior.thread_id && prior.reference_only.is_none()
                });
                let (shell, project) = match kept {
                    Some(prior) => (None, prior.project.clone()),
                    None => {
                        let shell = self.thread_shell(&row.owner, path)?;
                        let project = shell.project.clone();
                        (Some(shell), project)
                    }
                };
                let lineage = source::obj(payload, "lineage");
                stubs.push(crate::model::Agent {
                    id: row.owner.to_string(),
                    parent: if source::str_at(&lineage, "relationshipToParent") == "subagent" {
                        source::optional(&lineage, "parentThreadId")
                    } else {
                        None
                    },
                    kind: "thread".into(),
                    project,
                    thread_id: row.owner.to_string(),
                    ..crate::model::Agent::default()
                });
                built.push(shell);
                if payload
                    .get("delegatedFrom")
                    .and_then(crate::integration::thread_ref)
                    .is_some()
                {
                    delegated.push(payload.clone());
                }
            }
            let sub_rows = self.ordered_subs(sub_order)?;
            let mut subs = Vec::with_capacity(sub_rows.len());
            let mut sub_ids: HashSet<&str> = HashSet::new();
            let mut providers = HashMap::new();
            let mut turns = HashMap::new();
            let mut looked = HashSet::new();
            for row in &sub_rows {
                let Data::Subagent(v) = &row.data else { continue };
                let payload = &v.payload;
                let id = source::str_at(payload, "id");
                // `link_structure` would replace that thread's agent in place.
                if thread_ids.contains(id) {
                    return Err(FallbackReason::SubagentThreadCollision);
                }
                // The oracle's JSON router reads an empty provider alias; the
                // typed item router does not.
                if payload.get("providerThreadId").and_then(Value::as_str) == Some("") {
                    return Err(FallbackReason::CrossThreadAttribution);
                }
                // `link_structure` reads providers and turns only for an agent it
                // builds from a subagent row: one whose child is not an active
                // thread, or the preferred row of a remote child group.
                let may_build = !source::truthy_str(payload, "childThreadId")
                    .is_some_and(|child| thread_ids.contains(child.as_str()))
                    || payload.get("remoteChild").and_then(crate::integration::thread_ref).is_some();
                if let Some(pid) = source::truthy_str(payload, "providerThreadId").filter(|_| may_build) {
                    if looked.insert(pid.clone()) {
                        if let Some(body) = self.provider_payload(&pid)? {
                            providers.insert(pid.clone(), body);
                        }
                        if let Some(turn) = self.latest_turn(&pid)? {
                            turns.insert(pid, turn);
                        }
                    }
                }
                if !id.is_empty() {
                    sub_ids.insert(id);
                }
                subs.push(payload.clone());
            }
            let active = &self.idx.active;
            let mut nodes: HashMap<String, Value> = HashMap::new();
            for row in &sub_rows {
                let Data::Subagent(v) = &row.data else { continue };
                let mut node = v.payload.get("parentNodeId").and_then(Value::as_str).filter(|id| !id.is_empty());
                let mut seen: HashSet<&str> = HashSet::new();
                while let Some(id) = node {
                    if !seen.insert(id) || sub_ids.contains(id) {
                        break;
                    }
                    let Some(parent) = self.nodes.parent(id, active) else { break };
                    let parent = parent.map(|parent| parent.as_ref());
                    nodes
                        .entry(id.to_owned())
                        .or_insert_with(|| json!({"node_id": id, "parent_node_id": parent}));
                    node = parent.filter(|id| !id.is_empty());
                }
            }
            source::link_structure(&mut stubs, &subs, &providers, &turns, &nodes, &delegated, path)
                .map_err(|_| FallbackReason::AssemblyError)?;
            source::seal_parents(&mut stubs);
            let threads = built.len();
            let mut shells = Vec::with_capacity(stubs.len());
            let mut changed = BTreeSet::new();
            for (index, mut stub) in stubs.into_iter().enumerate() {
                let prior = old.get(stub.id.as_str()).copied();
                if index >= threads {
                    if self.lean {
                        stub.turn = Value::Null;
                        stub.options.clear();
                    }
                    // A subagent agent or a remote placeholder, built whole.
                    match prior {
                        Some(prior) if **prior == stub => shells.push(Arc::clone(prior)),
                        _ => {
                            changed.insert(stub.id.clone());
                            shells.push(Arc::new(stub));
                        }
                    }
                    continue;
                }
                match built[index].take() {
                    None => {
                        let prior = prior.ok_or(FallbackReason::CacheUnavailable)?;
                        if prior.parent == stub.parent && prior.kind == stub.kind && prior.subagent_ids == stub.subagent_ids {
                            shells.push(Arc::clone(prior));
                        } else {
                            let mut agent = (**prior).clone();
                            agent.parent = stub.parent;
                            agent.kind = stub.kind;
                            agent.subagent_ids = stub.subagent_ids;
                            changed.insert(agent.id.clone());
                            shells.push(Arc::new(agent));
                        }
                    }
                    Some(mut agent) => {
                        agent.parent = stub.parent;
                        agent.kind = stub.kind;
                        agent.subagent_ids = stub.subagent_ids;
                        match prior {
                            Some(prior) if **prior == agent => shells.push(Arc::clone(prior)),
                            _ => {
                                changed.insert(agent.id.clone());
                                shells.push(Arc::new(agent));
                            }
                        }
                    }
                }
            }
            Ok((shells, changed))
        }
        fn thread_shell(&self, thread_id: &str, path: &Path) -> Result<crate::model::Agent, FallbackReason> {
            let Some(ids) = self.tables[0].owners.get(thread_id) else {
                return Err(FallbackReason::CacheUnavailable);
            };
            let Some(thread) = ids.iter().find_map(|id| match &self.tables[0].rows[id].data {
                Data::Thread(v) if v.archived_at.is_none() && v.deleted_at.is_none() => Some(v.data.payload.clone()),
                _ => None,
            }) else {
                return Err(FallbackReason::CacheUnavailable);
            };
            let run = self.latest_run(thread_id)?;
            let pid = source::truthy_str(&run, "providerThreadId")
                .or_else(|| source::truthy_str(&thread, "activeProviderThreadId"));
            let mut providers = HashMap::new();
            let mut turns = HashMap::new();
            if let Some(pid) = pid {
                if let Some(payload) = self.provider_payload(&pid)? {
                    providers.insert(pid.clone(), payload);
                }
                if let Some(turn) = self.latest_turn(&pid)? {
                    turns.insert(pid, turn);
                }
            }
            let mut projects = HashMap::new();
            if let Some(project) = self.projects.get(source::str_at(&thread, "projectId")) {
                projects.insert(
                    project.project_id.clone(),
                    json!({"project_id": project.project_id, "title": project.title, "workspace_root": project.workspace_root}),
                );
            }
            let mut latest = HashMap::new();
            latest.insert(thread_id.to_owned(), run);
            let mut shell = source::thread_agent(
                &thread,
                latest.get(thread_id).unwrap(),
                &providers,
                &turns,
                &projects,
                path,
            )
            .map_err(|_| FallbackReason::AssemblyError)?;
            if self.lean {
                shell.turn = Value::Null;
                shell.options.clear();
            }
            Ok(shell)
        }
        fn attach_run_spans(
            &self,
            agent: &mut crate::model::Agent,
            since: &str,
            order: Option<&EncounterOrder>,
        ) -> Result<(), FallbackReason> {
            if agent.id != agent.thread_id {
                return Ok(());
            }
            let Some(ids) = self.tables[1].owners.get(agent.thread_id.as_str()) else {
                return Ok(());
            };
            let mut rows: Vec<&Row> = ids
                .iter()
                .filter_map(|id| self.tables[1].rows.get(id))
                .filter(|row| run_in_window(row, since))
                .collect();
            order_windowed_runs(&mut rows, order)?;
            for row in rows {
                let Data::Run(v) = &row.data else { continue };
                source::attach_span(
                    agent,
                    v.data.payload["startedAt"].as_str(),
                    Some(v.requested_at.as_str()),
                    v.completed_at.as_deref(),
                );
            }
            Ok(())
        }
        /// History rows the oracle routes to this agent. A history row goes to
        /// `native[provider]`, else to the thread that owns it. So this agent
        /// takes every top-160 turn of each provider `native` sends to it, and its
        /// own thread's turns of providers `native` does not route, attached in
        /// the oracle's row order: provider id, then newest first.
        fn attach_history(&self, agent: &mut crate::model::Agent, routes: &Routes) -> Result<(), FallbackReason> {
            let mut providers: BTreeSet<&str> = BTreeSet::new();
            if let Some(list) = routes.native_for.get(&agent.id) {
                providers.extend(list.iter().map(String::as_str));
            }
            if self.idx.active.contains(&agent.id) {
                if let Some(ids) = self.tables[4].owners.get(agent.id.as_str()) {
                    for id in ids {
                        if let Some(Data::Turn(v)) = self.tables[4].rows.get(id).map(|row| &row.data) {
                            if !routes.native.contains_key(v.provider_thread_id.as_ref()) {
                                providers.insert(v.provider_thread_id.as_ref());
                            }
                        }
                    }
                }
            }
            for pid in providers {
                let routed = routes.native.get(pid);
                for row in self.provider_turns(pid)?.into_iter().take(160) {
                    if routed.map(String::as_str).unwrap_or(row.owner.as_ref()) != agent.id {
                        continue;
                    }
                    let Data::Turn(v) = &row.data else { continue };
                    let at = ms_of(&v.started_at).or_else(|| ms_of(&v.completed_at)).unwrap_or(0.);
                    source::attach_history(agent, v.used, v.max, at, pid, v.model.as_deref().unwrap_or(""));
                }
            }
            Ok(())
        }
        /// One agent from its sealed shell and every selected row routed to it,
        /// in the oracle's order of steps.
        #[allow(clippy::too_many_arguments)]
        fn paint(
            &self,
            shell: &crate::model::Agent,
            routes: &Routes,
            known: &HashSet<String>,
            requests: &[(String, String, Option<String>, bool)],
            items: Option<&Vec<i64>>,
            events: Option<&Vec<i64>>,
            plans: Option<&Vec<i64>>,
            since: &str,
            span_order: Option<&EncounterOrder>,
        ) -> Result<crate::model::Agent, FallbackReason> {
            let mut agent = shell.clone();
            for (target, kind, created, async_input) in requests {
                if target == &agent.id {
                    source::attach_request(&mut agent, kind, created.as_deref(), *async_input);
                }
            }
            if self.lean {
                // The parts of `source::attach_item`, `finish_agent_body` and the event loop
                // below that `status_view` keeps: the newest update, the current item, the
                // failure count and the events. Label, command and file are never read here.
                let agent_run_id = agent.run.get("id").and_then(Value::as_str);
                let mut matched: Vec<(&Item, &Row)> = Vec::new();
                for id in items.into_iter().flatten() {
                    let Some(row) = self.tables[7].rows.get(id) else { continue };
                    let Data::Item(v) = &row.data else { continue };
                    if v.run_id.as_deref() == agent_run_id {
                        matched.push((&**v, row));
                    }
                }
                matched.sort_by(|(a, _), (b, _)| b.updated_at.cmp(&a.updated_at));
                if let Some((first, _)) = matched.first() {
                    let updated = first.updated_at.text_owned();
                    if updated.as_str() > agent.updated.as_deref().unwrap_or("") {
                        agent.updated = Some(updated);
                    }
                }
                if !agent.requests.is_empty() && crate::model::live(&agent) {
                    agent.status = "waiting".into();
                }
                if agent
                    .parent
                    .as_ref()
                    .is_none_or(|parent| parent == &agent.id || !known.contains(parent))
                {
                    agent.parent = None;
                }
                let current = |v: &Item| {
                    source::live_status(&v.status)
                        && !matches!(v.kind.as_ref(), "secret_request" | "link_request")
                        && !(agent.open_asks.nonblocking > 0 && v.kind.as_ref() == "user_input_request")
                };
                let chosen = matched
                    .iter()
                    .find(|(v, _)| current(v) && !matches!(v.kind.as_ref(), "reasoning" | "assistant_message"))
                    .or_else(|| matched.iter().find(|(v, _)| current(v)))
                    .copied();
                agent.now = match chosen {
                    Some((v, row)) => Some(v.bare(&row.owner)?),
                    None => None,
                };
                for (v, _) in &matched {
                    if !matches!(v.kind.as_ref(), "reasoning" | "assistant_message")
                        && v.status.as_ref() == "failed"
                    {
                        agent.failures += 1;
                    }
                }
                for id in events.into_iter().flatten() {
                    let Some(row) = self.tables[7].rows.get(id) else { continue };
                    let Data::Item(v) = &row.data else { continue };
                    let payload = v.payload.as_ref().ok_or(FallbackReason::TextDrift)?;
                    let failed = v.status.as_ref() == "failed";
                    let time = if failed {
                        v.updated_at.millis()
                    } else {
                        payload
                            .started_at
                            .as_ref()
                            .and_then(Stamp::millis)
                            .or_else(|| v.updated_at.millis())
                    };
                    if let Some(time) = time {
                        source::attach_event(
                            &mut agent,
                            time,
                            v.kind.as_ref(),
                            payload.tool.as_deref(),
                            failed,
                            None,
                        );
                    }
                }
                agent.events.sort_by(|p, q| p.t.total_cmp(&q.t));
                status_view(&mut agent);
                return Ok(agent);
            }
            for id in items.into_iter().flatten() {
                let Some(row) = self.tables[7].rows.get(id) else { continue };
                let Data::Item(v) = &row.data else { continue };
                let item = v.materialize(&row.owner)?;
                source::attach_item(&mut agent, &item);
            }
            source::finish_agent_body(&mut agent, known);
            for id in events.into_iter().flatten() {
                let Some(row) = self.tables[7].rows.get(id) else { continue };
                let Data::Item(v) = &row.data else { continue };
                let payload = v.payload.as_ref().ok_or(FallbackReason::TextDrift)?;
                let failed = v.status.as_ref() == "failed";
                let time = if failed {
                    v.updated_at.millis()
                } else {
                    payload
                        .started_at
                        .as_ref()
                        .and_then(Stamp::millis)
                        .or_else(|| v.updated_at.millis())
                };
                let painted = if failed { Some(v.materialize(&row.owner)?) } else { None };
                if let Some(time) = time {
                    source::attach_event(
                        &mut agent,
                        time,
                        v.kind.as_ref(),
                        payload.tool.as_deref(),
                        failed,
                        painted.as_ref(),
                    );
                }
            }
            self.attach_history(&mut agent, routes)?;
            self.attach_run_spans(&mut agent, since, span_order)?;
            source::finish_agent_tail(&mut agent);
            for id in plans.into_iter().flatten() {
                let Some(Data::Item(v)) = self.tables[7].rows.get(id).map(|row| &row.data) else { continue };
                let Some(text) = v.payload.as_ref().and_then(|p| p.text.as_ref()) else {
                    return Err(FallbackReason::TextDrift);
                };
                source::consider_plan(&mut agent, v.run_id.as_deref(), text.steps.as_deref());
            }
            Ok(agent)
        }
        /// Shells and routes from nothing, after a full read: every thread shell
        /// rebuilt, every selected row routed. No agent is painted; the snapshot
        /// just read stays authoritative.
        #[allow(clippy::too_many_arguments)]
        pub(super) fn seal(
            &mut self,
            path: &Path,
            now: f64,
            sub_order: Option<EncounterOrder>,
            request_order: Option<EncounterOrder>,
            event_order: Option<EncounterOrder>,
            plan_order: Option<PlanOrder>,
            item_order: Option<ItemOrder>,
            span_order: Option<EncounterOrder>,
        ) -> Result<(), FallbackReason> {
            self.mark_inputs_stale();
            self.assemble(
                path,
                now,
                None,
                sub_order,
                request_order,
                event_order,
                plan_order,
                item_order,
                span_order,
                &BTreeSet::new(),
                &mut source::ReadReport::default(),
                false,
            )
            .map(|_| ())
        }
        /// Rebuild only agents whose shell, routed rows, cap membership or
        /// window changed. Every other agent is the previous `Arc`.
        #[allow(clippy::too_many_arguments)]
        pub(super) fn assemble_fast(
            &mut self,
            path: &Path,
            now: f64,
            previous: Option<&Snapshot>,
            sub_order: Option<EncounterOrder>,
            request_order: Option<EncounterOrder>,
            event_order: Option<EncounterOrder>,
            plan_order: Option<PlanOrder>,
            item_order: Option<ItemOrder>,
            span_order: Option<EncounterOrder>,
            changed_owners: &BTreeSet<String>,
            report: &mut source::ReadReport,
        ) -> Result<FastOut, FallbackReason> {
            self.assemble(
                path,
                now,
                previous,
                sub_order,
                request_order,
                event_order,
                plan_order,
                item_order,
                span_order,
                changed_owners,
                report,
                true,
            )
        }
        #[allow(clippy::too_many_arguments)]
        fn assemble(
            &mut self,
            path: &Path,
            now: f64,
            previous: Option<&Snapshot>,
            sub_order: Option<EncounterOrder>,
            request_order: Option<EncounterOrder>,
            event_order: Option<EncounterOrder>,
            plan_order: Option<PlanOrder>,
            item_order: Option<ItemOrder>,
            span_order: Option<EncounterOrder>,
            changed_owners: &BTreeSet<String>,
            report: &mut source::ReadReport,
            paint: bool,
        ) -> Result<FastOut, FallbackReason> {
            if !self.idx.ready {
                return Err(FallbackReason::CacheUnavailable);
            }
            let selection = source::timing::phase("cache_selection");
            self.align_events(event_order.clone());
            self.align_plans(plan_order.clone());
            self.align_items(item_order.clone());
            if item_order.is_none() && self.idx.item_ties() {
                return Err(FallbackReason::ItemTimestampOrdinalTie);
            }
            if event_order.is_none() && self.idx.event_tie_groups > 0 {
                return Err(FallbackReason::EventTimestampTie);
            }
            if plan_order.is_none() && self.idx.plan_tie_groups > 0 {
                return Err(FallbackReason::PlanTimestampTie);
            }
            // The oracle's node map keeps one row per id; two active rows with one
            // id leave the winner to SQLite's row order.
            if self.nodes.duplicate_active(&self.idx.active) {
                return Err(FallbackReason::DuplicateNodeId);
            }
            if self.turn_identity_bad() {
                return Err(FallbackReason::ProviderIdentityMismatch);
            }
            let changes = std::mem::take(&mut self.changes);
            let projects_changed = std::mem::replace(&mut self.projects_changed, false);
            self.touched = 0;
            drop(selection);
            let assembly = source::timing::phase("assembly");
            let sealed = self.idx.inputs_ready && self.routes.is_some();
            let previous = previous.filter(|_| sealed);
            let mut dirty: HashSet<String> = HashSet::new();
            let mut everything = false;
            let mut retarget_all = !changes.nodes.is_empty();
            if previous.is_none() {
                let relinking = source::timing::phase("relink");
                let (shells, _) = self.relink(path, sub_order.as_ref(), &BTreeSet::new(), true)?;
                self.shells = shells;
                self.routes = Some(Arc::new(self.build_routes(sub_order.as_ref())?));
                drop(relinking);
                self.idx.targets.clear();
                self.idx.request_targets.clear();
                self.idx.item_members.clear();
                self.idx.event_members.clear();
                self.idx.plan_members.clear();
                everything = true;
                retarget_all = true;
            } else {
                let old_routes = Arc::clone(self.routes.as_ref().unwrap());
                // A link change, or a node on a subagent's parent chain, can move
                // agents, parents, kinds, aliases or remote placeholders.
                let mut structural = changes.links
                    || projects_changed
                    || changes.nodes.iter().any(|id| old_routes.sub_chain.contains(id));
                // Shells whose own inputs changed: the thread row, its runs, or
                // the provider row or turns of the provider the shell reads.
                let mut fresh: BTreeSet<String> = changes.threads.clone();
                fresh.extend(changes.runs.iter().cloned());
                for pid in &changes.providers {
                    if let Some(list) = old_routes.consumers.get(pid.as_ref()) {
                        fresh.extend(list.iter().cloned());
                    }
                }
                if !structural {
                    let patching = source::timing::phase("shell_patch");
                    let mut patched = Vec::new();
                    for id in &fresh {
                        let Some(&index) = old_routes.position.get(id) else { continue };
                        let old = &self.shells[index];
                        if old.id != old.thread_id || old.reference_only.is_some() {
                            // A subagent's or placeholder's shell comes from
                            // `link_structure`; rebuild it there.
                            structural = true;
                            break;
                        }
                        let mut shell = self.thread_shell(id, path)?;
                        shell.parent = old.parent.clone();
                        shell.kind = old.kind.clone();
                        shell.subagent_ids = old.subagent_ids.clone();
                        shell.remote = old.remote.clone();
                        shell.reference_only = old.reference_only;
                        // A provider id feeds `native`, a route other rows use.
                        if shell.provider_id != old.provider_id {
                            structural = true;
                            break;
                        }
                        if shell != **old {
                            patched.push((index, Arc::new(shell)));
                        }
                    }
                    drop(patching);
                    if !structural {
                        for (index, shell) in patched {
                            dirty.insert(shell.id.clone());
                            self.shells[index] = shell;
                        }
                    }
                }
                if structural {
                    let relinking = source::timing::phase("relink");
                    let (shells, changed) = self.relink(path, sub_order.as_ref(), &fresh, projects_changed)?;
                    dirty.extend(changed);
                    self.shells = shells;
                    let routes = Arc::new(self.build_routes(sub_order.as_ref())?);
                    if routes.native != old_routes.native
                        || routes.aliases != old_routes.aliases
                        || routes.sub_ids != old_routes.sub_ids
                    {
                        retarget_all = true;
                    }
                    // History rows follow `native`: a provider that changed route
                    // moves its rows between the old and the new target.
                    let pids: BTreeSet<&String> = routes.native.keys().chain(old_routes.native.keys()).collect();
                    for pid in pids {
                        let before = old_routes.native.get(pid);
                        let after = routes.native.get(pid);
                        if before == after {
                            continue;
                        }
                        dirty.extend(before.iter().chain(after.iter()).map(|id| (*id).clone()));
                        if before.is_none() || after.is_none() {
                            for row in self.provider_turns(pid)? {
                                dirty.insert(row.owner.to_string());
                            }
                        }
                    }
                    self.routes = Some(routes);
                    drop(relinking);
                }
            }
            let routes = Arc::clone(self.routes.as_ref().unwrap());
            let routing = source::timing::phase("shell_routes");
            // Spans are the thread's own runs.
            for owner in &changes.runs {
                if routes.position.contains_key(owner) {
                    dirty.insert(owner.clone());
                }
            }
            let since = self.idx.since.clone();
            if let Some(previous) = previous {
                if previous.at != now {
                    let old_since = source::iso(previous.at - source::WINDOW);
                    if old_since != since {
                        for row in self.tables[1].rows.values() {
                            let Data::Run(v) = &row.data else { continue };
                            if let Some(done) = v.completed_at.as_deref() {
                                if done >= old_since.as_str() && done < since.as_str() {
                                    dirty.insert(row.owner.to_string());
                                }
                            }
                        }
                    }
                }
            }
            // History: a changed turn moves the provider's newest-160 window.
            // A row that left a thread leaves that thread's history too.
            for owner in &changes.turn_owners {
                if routes.position.contains_key(owner) {
                    dirty.insert(owner.clone());
                }
            }
            for pid in &changes.providers {
                match routes.native.get(pid.as_ref()) {
                    Some(target) => {
                        dirty.insert(target.clone());
                    }
                    None => {
                        for row in self.provider_turns(pid)? {
                            dirty.insert(row.owner.to_string());
                        }
                    }
                }
            }
            drop(routing);
            let capping = source::timing::phase("shell_caps");
            let item_ids = self.idx.prefix(3000, false);
            let event_ids = self.idx.prefix(8000, true);
            let plan_ids: Vec<i64> = if self.lean {
                Vec::new()
            } else {
                self.idx.plans.values().copied().collect()
            };
            let new_items: HashSet<i64> = item_ids.iter().copied().collect();
            let new_events: HashSet<i64> = event_ids.iter().copied().collect();
            let new_plans: HashSet<i64> = plan_ids.iter().copied().collect();
            let mut targets = std::mem::take(&mut self.idx.targets);
            // A row that left every selection takes its old agent with it.
            for id in self
                .idx
                .item_members
                .iter()
                .chain(self.idx.event_members.iter())
                .chain(self.idx.plan_members.iter())
            {
                if !new_items.contains(id) && !new_events.contains(id) && !new_plans.contains(id) {
                    if let Some(old) = targets.remove(id) {
                        dirty.insert(old);
                    }
                }
            }
            // A selected row reaches its target. Re-route it when it changed, when
            // it is new to the selection, or when a route or a node changed.
            for id in item_ids.iter().chain(event_ids.iter()).chain(plan_ids.iter()) {
                let changed = changes.items.contains(id);
                if !retarget_all && !changed && targets.contains_key(id) {
                    continue;
                }
                let Some(target) = self.item_target(&routes, *id) else { continue };
                match targets.insert(*id, target.clone()) {
                    Some(old) if old != target => {
                        dirty.insert(old);
                        dirty.insert(target);
                    }
                    Some(_) if !changed => {}
                    _ => {
                        dirty.insert(target);
                    }
                }
            }
            // Entering or leaving one cap while staying in another.
            for (now_in, before) in [
                (&new_items, &self.idx.item_members),
                (&new_events, &self.idx.event_members),
                (&new_plans, &self.idx.plan_members),
            ] {
                for id in now_in.symmetric_difference(before) {
                    if let Some(target) = targets.get(id) {
                        dirty.insert(target.clone());
                    }
                }
            }
            let mut request_rows = Vec::new();
            let mut next_request_targets = HashMap::new();
            {
                let mut rows: Vec<&Row> = self.tables[6]
                    .rows
                    .values()
                    .filter(|row| {
                        self.idx.active.contains(row.owner.as_ref())
                            && matches!(&row.data, Data::Request(v) if v.resolved_at.is_none()
                                && ["pending", "open", "waiting", "requested"].contains(&v.status.as_str()))
                    })
                    .collect();
                if rows.len() > 1 {
                    match request_order.as_ref() {
                        Some(EncounterOrder::ThreadIndex(_)) => {
                            rows.sort_by(|a, b| a.owner.cmp(&b.owner).then(a.position.cmp(&b.position)))
                        }
                        Some(EncounterOrder::Rowid) => rows.sort_by_key(|row| row.id),
                        None => return Err(FallbackReason::MultipleRequests),
                    }
                }
                for row in rows {
                    let Data::Request(v) = &row.data else { continue };
                    let target = walk_owner(
                        &self.nodes,
                        &self.idx.active,
                        &routes.sub_ids,
                        &routes.aliases,
                        Some(v.node_id.as_str()),
                    )
                    .unwrap_or_else(|| row.owner.to_string());
                    match self.idx.request_targets.get(&row.id) {
                        Some(old) if old != &target => {
                            dirty.insert(old.clone());
                            dirty.insert(target.clone());
                        }
                        Some(_) if !changes.requests.contains(&row.id) => {}
                        _ => {
                            dirty.insert(target.clone());
                        }
                    }
                    request_rows.push((
                        target.clone(),
                        v.kind.clone(),
                        v.created_at.clone(),
                        v.async_input == 1,
                    ));
                    next_request_targets.insert(row.id, target);
                }
                for (id, old) in &self.idx.request_targets {
                    if !next_request_targets.contains_key(id) {
                        dirty.insert(old.clone());
                    }
                }
            }
            drop(capping);
            let mut out = Vec::new();
            let mut rebuilt = Vec::new();
            if paint {
                let painting = source::timing::phase("shell_paint");
                let known: HashSet<String> = self.shells.iter().map(|agent| agent.id.clone()).collect();
                let previous_by_id: HashMap<&str, &Arc<crate::model::Agent>> = previous
                    .map(|snapshot| snapshot.agents.iter().map(|agent| (agent.id.as_str(), agent)).collect())
                    .unwrap_or_default();
                let wanted = |target: &String| everything || dirty.contains(target);
                let mut items_for: HashMap<&str, Vec<i64>> = HashMap::new();
                let mut events_for: HashMap<&str, Vec<i64>> = HashMap::new();
                let mut plans_for: HashMap<&str, Vec<i64>> = HashMap::new();
                for (ids, map) in [
                    (&item_ids, &mut items_for),
                    (&event_ids, &mut events_for),
                    (&plan_ids, &mut plans_for),
                ] {
                    for id in ids {
                        if let Some(target) = targets.get(id).filter(|target| wanted(target)) {
                            map.entry(target.as_str()).or_default().push(*id);
                        }
                    }
                }
                out.reserve(self.shells.len());
                for shell in &self.shells {
                    if !everything && !dirty.contains(&shell.id) {
                        if let Some(prior) = previous_by_id.get(shell.id.as_str()) {
                            out.push(Arc::clone(prior));
                            continue;
                        }
                    }
                    let agent = self.paint(
                        shell,
                        &routes,
                        &known,
                        &request_rows,
                        items_for.get(shell.id.as_str()),
                        events_for.get(shell.id.as_str()),
                        plans_for.get(shell.id.as_str()),
                        &since,
                        span_order.as_ref(),
                    )?;
                    rebuilt.push(agent.id.clone());
                    out.push(Arc::new(agent));
                }
                drop(painting);
            }
            let deadlines = source::timing::phase("shell_deadline");
            report.valid_until = self.selection_deadline(&event_ids, now);
            drop(deadlines);
            report.threads = changed_owners.len();
            drop(assembly);
            self.idx.item_members = new_items;
            self.idx.event_members = new_events;
            self.idx.plan_members = new_plans;
            self.idx.targets = targets;
            self.idx.request_targets = next_request_targets;
            self.idx.inputs_ready = true;
            Ok(FastOut {
                snapshot: Snapshot {
                    agents: out,
                    at: now,
                    path: path.to_string_lossy().into(),
                },
                rebuilt,
                valid_until: report.valid_until,
            })
        }
        pub(super) fn roll_window(&mut self, now: f64) {
            self.touched = 0;
            self.projects_changed = false;
            self.changes = Changes::default();
            let rolled = self.idx.roll_since(now, &self.tables[7].rows);
            self.evict(rolled);
        }
        pub(super) fn blocks_clock(&self, now: f64) -> bool {
            if !self.idx.ready || self.idx.bad_events > 0 || source::iso(now - source::WINDOW).len() != 24
            {
                return true;
            }
            let since = source::iso(now - source::WINDOW);
            self.tables[1].rows.values().any(|row| {
                let Data::Run(v) = &row.data else { return false };
                v.completed_at.as_ref().is_some_and(|stamp| {
                    stamp.as_str() >= since.as_str() && !canonical_stamp(stamp)
                })
            })
        }
    }

    /// What the typed cache and the published snapshot actually retain.
    /// String lengths are character bytes. `capacity` is the reserved heap of
    /// each `String`. `allocs` counts strings with a heap buffer. `slots` counts
    /// `serde_json::Value` nodes (32 bytes each on this platform) and object
    /// keys are their own strings. This does not ask the allocator, so glibc's
    /// per-chunk header is reported separately as `allocs * 16`.
    pub(super) fn account(cache: &Cache, snapshot: Option<&crate::model::Snapshot>) -> Value {
        #[derive(Default, Clone)]
        struct Bag {
            n: u64,
            bytes: u64,
            capacity: u64,
            allocs: u64,
        }
        impl Bag {
            fn string(&mut self, value: &str, capacity: usize) {
                self.n += 1;
                self.bytes += value.len() as u64;
                self.capacity += capacity as u64;
                self.allocs += u64::from(capacity > 0);
            }
            fn owned(&mut self, value: &String) {
                self.string(value, value.capacity());
            }
            fn opt(&mut self, value: &Option<String>) {
                if let Some(value) = value {
                    self.owned(value);
                }
            }
            fn arc(&mut self, value: &Arc<str>) {
                self.string(value.as_ref(), value.len());
            }
            fn opt_arc(&mut self, value: &Option<Arc<str>>) {
                if let Some(value) = value {
                    self.arc(value);
                }
            }
            fn stamp(&mut self, stamp: &Stamp) {
                if let Stamp::Raw(text) = stamp {
                    self.string(text.as_str(), text.capacity());
                }
            }
            fn opt_stamp(&mut self, stamp: &Option<Stamp>) {
                if let Some(stamp) = stamp {
                    self.stamp(stamp);
                }
            }
            fn add(&mut self, other: &Bag) {
                self.n += other.n;
                self.bytes += other.bytes;
                self.capacity += other.capacity;
                self.allocs += other.allocs;
            }
            fn json(&self) -> Value {
                json!({
                    "n": self.n,
                    "bytes": self.bytes,
                    "capacity": self.capacity,
                    "allocs": self.allocs,
                    "malloc_header_estimate": self.allocs * 16,
                })
            }
        }
        #[derive(Default)]
        struct JsonBag {
            slots: u64,
            strings: Bag,
            keys: Bag,
            numbers: u64,
            arrays: u64,
            objects: u64,
        }
        fn walk(value: &Value, bag: &mut JsonBag) {
            bag.slots += 1;
            match value {
                Value::String(text) => bag.strings.owned(text),
                Value::Number(_) => bag.numbers += 1,
                Value::Array(items) => {
                    bag.arrays += 1;
                    for item in items {
                        walk(item, bag);
                    }
                }
                Value::Object(map) => {
                    bag.objects += 1;
                    for (key, item) in map {
                        bag.keys.owned(key);
                        walk(item, bag);
                    }
                }
                Value::Bool(_) | Value::Null => {}
            }
        }
        let item_cap: HashSet<i64> = cache.idx.item_prefix.iter().map(|key| key.rowid).collect();
        let event_cap: HashSet<i64> = cache.idx.event_prefix.iter().copied().collect();
        let mut tables = Vec::new();
        for (index, name) in TABLES.iter().enumerate() {
            let table = &cache.tables[index];
            let mut fields: BTreeMap<&'static str, Bag> = BTreeMap::new();
            let mut payload = JsonBag::default();
            let mut heavy_bytes = 0u64;
            let mut heavy_cap = 0u64;
            let mut heavy_in_item = 0u64;
            let mut heavy_in_event = 0u64;
            let mut heavy_in_plan = 0u64;
            let mut heavy_outside = 0u64;
            let mut selected_heavy_rows = 0u64;
            for row in table.rows.values() {
                let mut owner = Bag::default();
                owner.arc(&row.owner);
                fields.entry("owner").or_default().add(&owner);
                match &row.data {
                    Data::Thread(value) => {
                        walk(&value.data.payload, &mut payload);
                        let mut bag = Bag::default();
                        bag.opt(&value.archived_at);
                        fields.entry("archived_at").or_default().add(&bag);
                        let mut bag = Bag::default();
                        bag.opt(&value.deleted_at);
                        fields.entry("deleted_at").or_default().add(&bag);
                    }
                    Data::Run(value) => {
                        walk(&value.data.payload, &mut payload);
                        let mut bag = Bag::default();
                        bag.owned(&value.requested_at);
                        fields.entry("requested_at").or_default().add(&bag);
                        let mut bag = Bag::default();
                        bag.opt(&value.completed_at);
                        fields.entry("completed_at").or_default().add(&bag);
                    }
                    Data::Subagent(value) | Data::Provider(value) => walk(&value.payload, &mut payload),
                    Data::Turn(value) => {
                        let mut bag = Bag::default();
                        bag.arc(&value.json);
                        fields.entry("json").or_default().add(&bag);
                        let mut bag = Bag::default();
                        bag.arc(&value.provider_thread_id);
                        fields.entry("provider_thread_id").or_default().add(&bag);
                        let mut bag = Bag::default();
                        bag.opt_stamp(&value.started_at);
                        fields.entry("started_at").or_default().add(&bag);
                        let mut bag = Bag::default();
                        bag.opt_stamp(&value.completed_at);
                        fields.entry("completed_at").or_default().add(&bag);
                        let mut bag = Bag::default();
                        bag.opt_arc(&value.model);
                        fields.entry("model").or_default().add(&bag);
                    }
                    Data::Node(value) => {
                        let mut bag = Bag::default();
                        bag.arc(&value.node_id);
                        fields.entry("node_id").or_default().add(&bag);
                        let mut bag = Bag::default();
                        bag.opt_arc(&value.parent_node_id);
                        fields.entry("parent_node_id").or_default().add(&bag);
                    }
                    Data::Request(value) => {
                        for (name, text) in [
                            ("node_id", Some(&value.node_id)),
                            ("kind", Some(&value.kind)),
                            ("status", Some(&value.status)),
                        ] {
                            let mut bag = Bag::default();
                            if let Some(text) = text {
                                bag.owned(text);
                            }
                            fields.entry(name).or_default().add(&bag);
                        }
                        let mut bag = Bag::default();
                        bag.opt(&value.created_at);
                        fields.entry("created_at").or_default().add(&bag);
                        let mut bag = Bag::default();
                        bag.opt(&value.resolved_at);
                        fields.entry("resolved_at").or_default().add(&bag);
                    }
                    Data::Item(value) => {
                        let mut note = |name, bag: Bag| {
                            fields.entry(name).or_default().add(&bag);
                        };
                        let arc_bag = |text: &Arc<str>| {
                            let mut bag = Bag::default();
                            bag.arc(text);
                            bag
                        };
                        let opt_arc_bag = |text: &Option<Arc<str>>| {
                            let mut bag = Bag::default();
                            bag.opt_arc(text);
                            bag
                        };
                        let stamp_bag = |stamp: &Stamp| {
                            let mut bag = Bag::default();
                            bag.stamp(stamp);
                            bag
                        };
                        note("run_id", opt_arc_bag(&value.run_id));
                        note("node_id", opt_arc_bag(&value.node_id));
                        note("provider_thread_id", opt_arc_bag(&value.provider_thread_id));
                        note("kind", arc_bag(&value.kind));
                        note("status", arc_bag(&value.status));
                        note("updated_at", stamp_bag(&value.updated_at));
                        let mut started = Bag::default();
                        let mut tool_bag = Bag::default();
                        if let Some(payload) = &value.payload {
                            started.opt_stamp(&payload.started_at);
                            tool_bag.opt_arc(&payload.tool);
                            if let Some(text) = &payload.text {
                                note("label", arc_bag(&text.body.label));
                                note("command", arc_bag(&text.body.command));
                                note("file", arc_bag(&text.body.file));
                                let mut bag = Bag::default();
                                bag.opt_arc(&text.steps);
                                note("steps", bag);
                            }
                        }
                        note("started_at", started);
                        note("tool", tool_bag);
                        let bytes = value
                            .payload
                            .as_ref()
                            .and_then(|p| p.text.as_ref())
                            .map(|text| {
                                text.body.label.len()
                                    + text.body.command.len()
                                    + text.body.file.len()
                                    + text.steps.as_ref().map(|steps| steps.len()).unwrap_or(0)
                            })
                            .unwrap_or(0) as u64;
                        heavy_bytes += bytes;
                        heavy_cap += bytes;
                        let in_item = item_cap.contains(&row.id);
                        let in_event = event_cap.contains(&row.id);
                        let in_plan = cache.idx.plan_at.contains_key(&row.id);
                        if in_item {
                            heavy_in_item += bytes;
                        }
                        if in_event {
                            heavy_in_event += bytes;
                        }
                        if in_plan {
                            heavy_in_plan += bytes;
                        }
                        if in_item || in_event || in_plan {
                            selected_heavy_rows += 1;
                        } else {
                            heavy_outside += bytes;
                        }
                    }
                }
            }
            let mut field_json = serde_json::Map::new();
            let mut field_bytes = 0u64;
            let mut field_capacity = 0u64;
            let mut field_allocs = 0u64;
            for (name, bag) in &fields {
                field_bytes += bag.bytes;
                field_capacity += bag.capacity;
                field_allocs += bag.allocs;
                field_json.insert((*name).to_string(), bag.json());
            }
            tables.push(json!({
                "table": name,
                "rows": table.rows.len(),
                "owners": table.owners.len(),
                "row_struct_bytes": std::mem::size_of::<Row>() * table.rows.len(),
                "fields": field_json,
                "field_bytes": field_bytes,
                "field_capacity": field_capacity,
                "field_allocs": field_allocs,
                "payload_slots": payload.slots,
                "payload_slot_bytes": payload.slots * std::mem::size_of::<Value>() as u64,
                "payload_string_bytes": payload.strings.bytes,
                "payload_string_capacity": payload.strings.capacity,
                "payload_string_allocs": payload.strings.allocs,
                "payload_key_bytes": payload.keys.bytes,
                "payload_key_capacity": payload.keys.capacity,
                "payload_key_allocs": payload.keys.allocs,
                "payload_objects": payload.objects,
                "payload_numbers": payload.numbers,
                "heavy_text_bytes": heavy_bytes,
                "heavy_text_capacity": heavy_cap,
                "heavy_in_item_cap_bytes": heavy_in_item,
                "heavy_in_event_cap_bytes": heavy_in_event,
                "heavy_in_plan_bytes": heavy_in_plan,
                "heavy_outside_selection_bytes": heavy_outside,
                "rows_in_some_selection": selected_heavy_rows,
            }));
        }
        let mut index_strings = Bag::default();
        for key in cache.idx.items.iter() {
            index_strings.stamp(&key.updated.0);
            index_strings.arc(&key.group);
        }
        for key in cache.idx.events.keys() {
            index_strings.stamp(&key.updated.0);
            index_strings.arc(&key.owner);
        }
        for key in cache.idx.plans.keys() {
            index_strings.stamp(&key.updated.0);
            index_strings.arc(&key.run);
        }
        let mut index_sets = Bag::default();
        for key in cache
            .idx
            .by_run
            .keys()
            .chain(cache.idx.plan_by_run.keys())
            .chain(cache.idx.runless.keys())
            .chain(cache.idx.active.iter())
            .chain(cache.idx.latest_keys.iter())
            .chain(cache.idx.run_refs.iter())
        {
            index_sets.owned(key);
        }
        for key in cache
            .idx
            .targets
            .values()
            .chain(cache.idx.request_targets.values())
        {
            index_sets.owned(key);
        }
        for stamp in cache.idx.event_group.keys().chain(cache.idx.plan_group.keys()) {
            index_sets.stamp(stamp);
        }
        let mut shells = Bag::default();
        let mut shell_json = JsonBag::default();
        for agent in &cache.shells {
            note_agent(agent, &mut shells, &mut shell_json);
        }
        let mut published = Bag::default();
        let mut published_json = JsonBag::default();
        let mut published_items = 0u64;
        if let Some(snapshot) = snapshot {
            for agent in &snapshot.agents {
                note_agent(agent, &mut published, &mut published_json);
                published_items += agent.items.len() as u64 + agent.recent_failures.len() as u64;
            }
        }
        fn note_agent(agent: &crate::model::Agent, strings: &mut Bag, json_bag: &mut JsonBag) {
            for text in [
                &agent.id,
                &agent.title,
                &agent.kind,
                &agent.project,
                &agent.cwd,
                &agent.model,
                &agent.status,
                &agent.thread_id,
            ] {
                strings.owned(text);
            }
            for text in [
                &agent.parent,
                &agent.provider_id,
                &agent.updated,
                &agent.started,
                &agent.ended,
                &agent.asked,
                &agent.branch,
                &agent.context.prompt_cache_expires_at,
                &agent.context.updated_at,
            ] {
                strings.opt(text);
            }
            if let Some(storage) = &agent.storage {
                strings.owned(&storage.project_id);
                strings.opt(&storage.project_root);
                strings.opt(&storage.worktree);
            }
            if let Some(ids) = &agent.subagent_ids {
                for id in ids {
                    strings.owned(id);
                }
            }
            if let Some(shelf) = &agent.shelf {
                strings.opt(&shelf.snoozed_until);
            }
            for text in &agent.requests {
                strings.owned(text);
            }
            for item in agent.items.iter().chain(agent.recent_failures.iter()).chain(agent.now.iter()) {
                for text in [
                    &item.thread_id,
                    &item.kind,
                    &item.status,
                    &item.updated_at,
                    &item.label,
                    &item.command,
                    &item.file,
                ] {
                    strings.owned(text);
                }
                for text in [
                    &item.run_id,
                    &item.node_id,
                    &item.provider_thread_id,
                    &item.started_at,
                    &item.tool,
                ] {
                    strings.opt(text);
                }
            }
            for point in &agent.history {
                strings.owned(&point.provider);
                strings.owned(&point.model);
            }
            for event in &agent.events {
                strings.owned(&event.cat);
            }
            if let Some(steps) = &agent.plan {
                for step in steps {
                    strings.owned(&step.text);
                    strings.owned(&step.status);
                }
            }
            for key in agent.mix.keys() {
                strings.owned(key);
            }
            walk(&agent.run, json_bag);
            walk(&agent.turn, json_bag);
            if let Some(remote) = &agent.remote {
                walk(remote, json_bag);
            }
            for option in &agent.options {
                walk(option, json_bag);
            }
        }
        let projects = {
            let mut bag = Bag::default();
            for project in cache.projects.values() {
                bag.owned(&project.project_id);
                bag.owned(&project.title);
                bag.owned(&project.workspace_root);
            }
            bag
        };
        json!({
            "row_struct_bytes": std::mem::size_of::<Row>(),
            "item_key_struct_bytes": std::mem::size_of::<ItemKey>(),
            "value_struct_bytes": std::mem::size_of::<Value>(),
            "pool_unique": cache.pool.values.len(),
            "pool_bytes": cache.pool.values.iter().map(|text| text.len()).sum::<usize>(),
            "tables": tables,
            "index": {
                "items": cache.idx.items.len(),
                "item_prefix": cache.idx.item_prefix.len(),
                "events": cache.idx.events.len(),
                "event_prefix": cache.idx.event_prefix.len(),
                "plans": cache.idx.plans.len(),
                "key_strings": index_strings.json(),
                "set_strings": index_sets.json(),
                "item_key_struct_bytes": std::mem::size_of::<ItemKey>() * cache.idx.items.len(),
                "event_key_struct_bytes": std::mem::size_of::<StampKey>() * cache.idx.events.len(),
                "plan_key_struct_bytes": std::mem::size_of::<PlanKey>() * cache.idx.plans.len(),
            },
            "projects": projects.json(),
            "shells": {
                "agents": cache.shells.len(),
                "strings": shells.json(),
                "json_slots": shell_json.slots,
                "json_slot_bytes": shell_json.slots * std::mem::size_of::<Value>() as u64,
                "json_string_bytes": shell_json.strings.bytes,
                "json_key_bytes": shell_json.keys.bytes,
            },
            "snapshot": {
                "agents": snapshot.map(|s| s.agents.len()).unwrap_or(0),
                "item_slots": published_items,
                "strings": published.json(),
                "json_slots": published_json.slots,
                "json_slot_bytes": published_json.slots * std::mem::size_of::<Value>() as u64,
                "json_string_bytes": published_json.strings.bytes,
                "json_key_bytes": published_json.keys.bytes,
            },
        })
    }

    /// Heap bytes glibc counts as handed out: arena chunks in use plus mmapped
    /// chunks, each including its chunk header. `None` off glibc.
    pub(super) fn heap_in_use() -> Option<u64> {
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        {
            // SAFETY: mallinfo2 only reads allocator statistics.
            let info = unsafe { libc::mallinfo2() };
            Some((info.uordblks + info.hblkhd) as u64)
        }
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        {
            None
        }
    }
    /// Free bytes glibc keeps inside its arenas (not returned to the system).
    pub(super) fn heap_free() -> Option<u64> {
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        {
            // SAFETY: as above.
            let info = unsafe { libc::mallinfo2() };
            Some(info.fordblks as u64)
        }
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        {
            None
        }
    }
    /// Exact heap attribution by taking the cache apart: each entry is the drop
    /// in `heap_in_use` when that structure is freed, so it includes container
    /// nodes, slack and chunk headers. A shared `Arc<str>` is freed by whichever
    /// holder drops last; the intern pool goes last so ids land there.
    pub(super) struct Ledger {
        last: u64,
        pub(super) entries: Vec<(String, u64)>,
    }
    impl Ledger {
        pub(super) fn new() -> Option<Self> {
            Some(Self {
                last: heap_in_use()?,
                entries: Vec::new(),
            })
        }
        pub(super) fn mark(&mut self, name: &str) {
            let now = heap_in_use().unwrap_or(self.last);
            self.entries.push((name.to_string(), self.last.saturating_sub(now)));
            self.last = now;
        }
        pub(super) fn left(&self) -> u64 {
            self.last
        }
    }
    pub(super) fn teardown(cache: Cache, ledger: &mut Ledger) {
        let Cache {
            mut tables,
            projects,
            idx,
            shells,
            routes,
            nodes,
            provider_rows,
            turn_rows,
            bad_turns,
            changes,
            pool,
            text_held,
            ..
        } = cache;
        let Select {
            items,
            item_ids,
            item_prefix,
            events,
            event_at,
            event_prefix,
            event_ids,
            event_group,
            plans,
            plan_at,
            plan_group,
            by_run,
            plan_by_run,
            runless,
            active,
            latest_keys,
            run_refs,
            item_members,
            event_members,
            plan_members,
            targets,
            request_targets,
            ..
        } = idx;
        drop((items, item_ids, item_prefix));
        ledger.mark("index: item cap (items, item_ids, item_prefix)");
        drop((events, event_at, event_prefix, event_ids, event_group));
        ledger.mark("index: event cap (events, event_at, event_prefix, event_ids, event_group)");
        drop((plans, plan_at, plan_group));
        ledger.mark("index: plans (plans, plan_at, plan_group)");
        drop((by_run, plan_by_run, runless));
        ledger.mark("index: by_run, plan_by_run, runless");
        drop((item_members, event_members, plan_members, targets, request_targets));
        ledger.mark("index: members and targets");
        drop((active, latest_keys, run_refs));
        ledger.mark("index: active, latest_keys, run_refs");
        drop(shells);
        ledger.mark("shells");
        drop(routes);
        ledger.mark("routes");
        drop(nodes);
        ledger.mark("node index");
        drop((provider_rows, turn_rows, bad_turns));
        ledger.mark("provider and turn indexes");
        drop((changes, text_held, projects));
        ledger.mark("changes, text_held, projects");
        for row in tables[7].rows.values_mut() {
            if let Data::Item(item) = &mut row.data {
                item.payload = None;
            }
        }
        ledger.mark("turn_items: selected text (label, command, file, steps)");
        for (index, table) in tables.iter_mut().enumerate() {
            drop(std::mem::take(&mut table.owners));
            ledger.mark(&format!("{}: owners map", TABLES[index]));
            if index == 4 {
                for row in table.rows.values_mut() {
                    if let Data::Turn(turn) = &mut row.data {
                        turn.json = empty_json();
                    }
                }
                ledger.mark("provider_turns: projected JSON text");
            }
            drop(std::mem::take(&mut table.rows));
            ledger.mark(&format!("{}: rows", TABLES[index]));
        }
        drop(pool);
        ledger.mark("intern pool");
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn cache_has_no_many_id_bind_limit() {
            let mut db = crate::live::privacy_support::Db::new();
            let tx = db.db.transaction().unwrap();
            for i in 0..33_010 {
                tx.execute("INSERT INTO orchestration_v2_projection_threads(thread_id,project_id,title,default_provider,runtime_mode,interaction_mode,created_at,updated_at,payload_json) VALUES(?,'p','Synthetic','codex','full-access','default','','',?)", rusqlite::params![format!("t{i}"), format!(r#"{{"id":"t{i}","projectId":"p"}}"#)]).unwrap();
            }
            tx.commit().unwrap();
            let mut cache = Cache::default();
            let pending = Pending::read(&db.db, &db.path, None, &mut cache.pool).unwrap();
            cache.apply(pending);
            assert_eq!(cache.tables[0].rows.len(), 33_010);
            let owners = cache.tables[0].owners.keys().map(|owner| owner.to_string()).collect();
            let pending = Pending::read(&db.db, &db.path, Some(&owners), &mut cache.pool).unwrap();
            assert_eq!(pending.rows, 33_011); // all threads plus the global project
            cache.apply(pending);
            assert_eq!(cache.tables[0].rows.len(), 33_010);
        }

        #[test]
        fn item_upserts_keep_identity_and_replace_projected_content() {
            let mut db = crate::live::privacy_support::Db::new();
            db.write("a", 0);
            db.db.execute("INSERT INTO orchestration_v2_projection_turn_items(turn_item_id,thread_id,ordinal,type,status,updated_at,payload_json) VALUES('i','a',1,'command_execution','running','','{\"input\":\"synthetic\"}')", []).unwrap();
            let mut cache = Cache::default();
            let pending = Pending::read(&db.db, &db.path, None, &mut cache.pool).unwrap();
            cache.apply(pending);
            let id = *cache.tables[7].rows.keys().next().unwrap();
            db.db.execute("INSERT INTO orchestration_v2_projection_turn_items(turn_item_id,thread_id,ordinal,type,status,updated_at,payload_json) VALUES('i','a',1,'command_execution','completed','','{\"input\":\"synthetic\"}') ON CONFLICT(turn_item_id) DO UPDATE SET status=excluded.status", []).unwrap();
            let pending = Pending::read(&db.db, &db.path, Some(&BTreeSet::from(["a".into()])), &mut cache.pool).unwrap();
            cache.apply(pending);
            assert_eq!(cache.tables[7].rows.len(), 1);
            let Data::Item(item) = &cache.tables[7].rows[&id].data else {
                panic!("item row")
            };
            assert_eq!(item.status.as_ref(), "completed");
            assert!(item.payload.is_none());
            cache.reindex(1_794_484_800_000.);
            cache
                .sync_text(&db.db, &db.path, &BTreeSet::new())
                .expect("selected text");
            let Data::Item(item) = &cache.tables[7].rows[&id].data else {
                panic!("item row")
            };
            assert_eq!(
                item.payload
                    .as_ref()
                    .and_then(|p| p.text.as_ref())
                    .map(|text| text.body.command.as_ref()),
                Some("synthetic")
            );
        }

        #[test]
        fn moved_row_evicts_its_previous_owner_without_an_old_owner_event() {
            let mut db = crate::live::privacy_support::Db::new();
            db.write("a", 0);
            db.write("b", 1);
            db.db.execute("INSERT INTO orchestration_v2_projection_nodes(node_id,thread_id,root_node_id,kind,status,payload_json) VALUES('n','a','n','tool_call','running','{}')", []).unwrap();
            let mut cache = Cache::default();
            let pending = Pending::read(&db.db, &db.path, None, &mut cache.pool).unwrap();
            cache.apply(pending);
            let id = *cache.tables[5].owners["a"].first().unwrap();
            db.db
                .execute(
                    "UPDATE orchestration_v2_projection_nodes SET thread_id='b' WHERE node_id='n'",
                    [],
                )
                .unwrap();
            let pending = Pending::read(&db.db, &db.path, Some(&BTreeSet::from(["b".into()])), &mut cache.pool).unwrap();
            cache.apply(pending);
            assert!(cache.tables[5].owners["a"].is_empty());
            assert_eq!(cache.tables[5].owners["b"], BTreeSet::from([id]));
            assert_eq!(cache.tables[5].rows[&id].owner.as_ref(), "b");
            db.db
                .execute(
                    "DELETE FROM orchestration_v2_projection_nodes WHERE node_id='n'",
                    [],
                )
                .unwrap();
            let pending = Pending::read(&db.db, &db.path, Some(&BTreeSet::from(["b".into()])), &mut cache.pool).unwrap();
            cache.apply(pending);
            assert!(cache.tables[5].rows.is_empty());
        }
    }
}

/// Benchmark only: glibc's heap in use and free bytes it keeps in its arenas.
pub fn heap_stats() -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "in_use": cache::heap_in_use()?,
        "free_in_arenas": cache::heap_free()?,
    }))
}

pub struct Reader {
    path: PathBuf,
    db: Connection,
    identity: Identity,
    /// Sampled once the connection has read (and so mapped the WAL index).
    sidecars: Sidecars,
    version: Option<(i64, i64)>,
    environment: Option<String>,
    previous: Option<Snapshot>,
    valid_until: f64,
    self_check: bool,
    mismatches: u64,
    feed: Option<Feed>,
    cache: Option<cache::Cache>,
    /// Next poll treats one selected row's text read as drifted.
    force_text_drift: bool,
    request_plans: BTreeMap<(i64, usize), Option<cache::EncounterOrder>>,
    event_plans: BTreeMap<(i64, usize), Option<cache::EncounterOrder>>,
    sub_plans: BTreeMap<(i64, usize), Option<cache::EncounterOrder>>,
    plan_plans: BTreeMap<(i64, usize), Option<cache::PlanOrder>>,
    item_plans: BTreeMap<(i64, usize, usize), Option<cache::ItemOrder>>,
    span_plans: BTreeMap<(i64, usize), Option<cache::EncounterOrder>>,
    thread_scan_plan: Option<(i64, bool)>,
    item_inline_plan: Option<(i64, bool)>,
    /// Publish `status_view` of every agent and read no item text.
    status_only: bool,
    /// For tests: run once in the next poll, between its probe and its read.
    between_probe_and_read: Option<Box<dyn FnOnce() + Send>>,
}
impl Reader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SourceError> {
        let path = path.as_ref().to_owned();
        let before = identity(&path)?;
        let db = open(&path)?;
        if before != identity(&path)? {
            return Err(error(&path, "database replaced while opening"));
        }
        let sidecars = settle(&db, &path);
        Ok(Self {
            path,
            db,
            identity: before,
            sidecars,
            version: None,
            environment: None,
            previous: None,
            valid_until: f64::NEG_INFINITY,
            self_check: std::env::var_os("T3_AGENTS_LIVE_SELF_CHECK").is_some(),
            mismatches: 0,
            feed: None,
            cache: None,
            force_text_drift: false,
            request_plans: BTreeMap::new(),
            event_plans: BTreeMap::new(),
            sub_plans: BTreeMap::new(),
            plan_plans: BTreeMap::new(),
            item_plans: BTreeMap::new(),
            span_plans: BTreeMap::new(),
            thread_scan_plan: None,
            item_inline_plan: None,
            status_only: false,
            between_probe_and_read: None,
        })
    }
    /// For the bar's status watcher: every published agent is `status_view` of
    /// the full reader's, on every path, and no label, command, file or plan
    /// is read. The self-check compares with `status_view` of the full reader.
    pub fn status_only(mut self) -> Self {
        self.status_only = true;
        self.cache = None;
        self
    }
    pub fn with_self_check(mut self, enabled: bool) -> Self {
        self.self_check = enabled;
        self
    }
    pub fn mismatches(&self) -> u64 {
        self.mismatches
    }
    /// The next poll's by-rowid text read disagrees with the staged keys, so
    /// that poll falls back to the oracle and keeps the staged cache.
    pub fn force_text_drift(&mut self) {
        self.force_text_drift = true;
    }
    /// For tests: the next poll runs `action` after asking SQLite whether anything was
    /// written and before it opens its read transaction. A writer's commit can land
    /// exactly there, and no test can otherwise put one there on purpose.
    pub fn between_probe_and_read(&mut self, action: impl FnOnce() + Send + 'static) {
        self.between_probe_and_read = Some(Box::new(action));
    }
    /// Per-table retention of the typed cache and the published snapshot.
    /// `None` before the first successful stage.
    pub fn cache_account(&self) -> Option<serde_json::Value> {
        self.cache.as_ref().map(|cache| cache::account(cache, self.previous.as_ref()))
    }
    /// Benchmark only: free the reader one structure at a time and report how
    /// much heap each held (see `cache::Ledger`). `None` off glibc.
    pub fn heap_teardown(self) -> Option<serde_json::Value> {
        let Self {
            db,
            previous,
            cache,
            request_plans,
            event_plans,
            sub_plans,
            plan_plans,
            item_plans,
            span_plans,
            feed,
            ..
        } = self;
        crate::clock::release_memory();
        let start = cache::heap_in_use()?;
        let free = cache::heap_free();
        let mut ledger = cache::Ledger::new()?;
        drop(previous);
        ledger.mark("published snapshot");
        if let Some(cache) = cache {
            cache::teardown(cache, &mut ledger);
        }
        drop((request_plans, event_plans, sub_plans, plan_plans, item_plans, span_plans, feed));
        ledger.mark("plan classifications and feed");
        drop(db);
        ledger.mark("SQLite connection (page and statement caches)");
        Some(serde_json::json!({
            "heap_in_use_start": start,
            "heap_free_in_arenas_start": free,
            "heap_in_use_left": ledger.left(),
            "entries": ledger.entries.iter().map(|(name, bytes)| serde_json::json!([name, bytes])).collect::<Vec<_>>(),
        }))
    }
    pub fn poll(&mut self, now: f64) -> Result<Update, SourceError> {
        let start = Instant::now();
        if !now.is_finite() {
            return Err(error(&self.path, "invalid snapshot clock"));
        }
        let current = identity(&self.path)?;
        let replaced = current != self.identity || sidecars(&self.path) != self.sidecars;
        if replaced {
            // Close before opening. SQLite shares one WAL index per file within a process,
            // so a new connection opened beside the old one would inherit its mapping of a
            // deleted `-shm`.
            drop(std::mem::replace(
                &mut self.db,
                Connection::open_in_memory().map_err(|e| error(&self.path, e))?,
            ));
            self.db = open(&self.path)?;
            self.sub_plans.clear();
            self.event_plans.clear();
            self.request_plans.clear();
            self.plan_plans.clear();
            self.item_plans.clear();
            self.span_plans.clear();
            self.thread_scan_plan = None;
            self.item_inline_plan = None;
            if current != identity(&self.path)? {
                return Err(error(&self.path, "database replaced while reopening"));
            }
            self.identity = current;
            self.sidecars = settle(&self.db, &self.path);
            self.version = None;
            self.feed = None;
        }
        let mut counters = Counters::default();
        // Probe BEFORE the read transaction: a racing commit can cause an extra
        // reread next poll, but can never mark stale results with a newer version.
        let version = (
            scalar(&self.db, "PRAGMA data_version", &self.path)?,
            scalar(&self.db, "PRAGMA schema_version", &self.path)?,
        );
        let environment = crate::integration::environment_id(&self.path);
        let mut skip = self.version == Some(version)
            && environment == self.environment
            && self
                .previous
                .as_ref()
                .is_some_and(|p| now >= p.at && (now < self.valid_until || now == p.at));
        counters.probe = start.elapsed();
        if let Some(action) = self.between_probe_and_read.take() {
            action();
        }
        self.db
            .execute_batch("BEGIN")
            .map_err(|e| sql_error(&self.path, e))?;
        let result = (|| {
            // Pin a single SQLite read snapshot, also for the canary. Otherwise a
            // writer between two oracle calls could produce a false mismatch.
            let pinned_schema = scalar(&self.db, "PRAGMA schema_version", &self.path)?;
            // If a writer committed between our probe and pinned transaction,
            // abandon the skip. Store the earlier version (safe extra reread).
            let pinned_data = scalar(&self.db, "PRAGMA data_version", &self.path)?;
            skip &= pinned_schema == version.1 && pinned_data == version.0;
            // Whether nothing was committed since the last poll, as this transaction
            // sees the file. The probe cannot say: a commit can land after it and
            // before the read, and this read then holds rows the probe never saw.
            let uncommitted = self.version == Some((pinned_data, pinned_schema));
            let read_start = Instant::now();
            // Schema/environment/clock invalidation cannot use an earlier feed
            // as an incremental baseline. Optional feed errors fall back safely.
            let baseline = (pinned_schema == version.1
                && self.version.is_some_and(|v| v.1 == version.1)
                && environment == self.environment
                && self.previous.as_ref().is_some_and(|p| now >= p.at))
            .then_some(self.feed.as_ref())
            .flatten();
            let feed_phase = source::timing::phase("cursor_feed");
            let feed = if skip {
                None
            } else {
                match read_feed(&self.db, baseline, &mut counters.fallback_reason) {
                    Ok(feed) => feed,
                    Err(_) => {
                        counters.fallback_reason = Some(FallbackReason::FeedUnavailable);
                        None
                    }
                }
            };
            if let Some(feed) = &feed {
                counters.event_rows_read = feed.rows;
                counters.feed_complete = feed.complete;
                if feed.complete {
                    counters.changed_threads = feed.threads.len();
                }
            }
            drop(feed_phase);
            // A forward clock with no commit can expire canonical rows from the
            // index. So can a commit that added no event, when the feed shows that
            // nothing it relies on moved (the ledger count among them): such a commit
            // changed no table the dashboard reads. Anything else with an incomplete
            // feed still full-reads.
            let quiet = feed.as_ref().is_some_and(|f| f.quiet);
            let clock_only = !skip
                && !replaced
                && self.previous.is_some()
                && self.version.is_some_and(|v| v.1 == version.1)
                && (uncommitted || quiet)
                && environment == self.environment
                && pinned_schema == version.1
                && counters.fallback_reason == Some(FallbackReason::NoNewEvents)
                && self.cache.as_ref().is_some_and(|c| !c.blocks_clock(now));
            // Stage all cache changes until transaction teardown and file checks
            // succeed. A failed optional cache read leaves the oracle authoritative.
            let owners = if skip || clock_only {
                None
            } else {
                Some(
                    feed.as_ref()
                        .filter(|f| f.complete && self.cache.as_ref().is_some_and(|c| c.index_ready()))
                        .map(|f| &f.threads),
                )
            };
            let item_inline_ok = if skip || clock_only {
                false
            } else {
                match self.item_inline_plan {
                    Some((schema, ok)) if schema == pinned_schema => ok,
                    _ => {
                        let ok = poll1::item_inline_query_ok(&self.db);
                        self.item_inline_plan = Some((pinned_schema, ok));
                        ok
                    }
                }
            };
            let refresh = source::timing::phase("owner_reread");
            let pending = if let Some(owners) = owners {
                let created = self.cache.is_none();
                let lean = self.status_only;
                let read = {
                    let cache = self.cache.get_or_insert_with(|| cache::Cache::new(lean));
                    cache.read_pending(&self.db, &self.path, owners, now, item_inline_ok).ok()
                };
                if read.is_none() && created {
                    self.cache = None;
                }
                read
            } else {
                None
            };
            let cache_kept = clock_only || pending.is_some();
            if let Some(cached) = &pending {
                counters.cache_rows_read = cached.rows;
                counters.cache_ready = true;
                counters.cache_full = cached.full;
            } else if clock_only {
                counters.cache_ready = true;
            }
            drop(refresh);
            let staging = source::timing::phase("cache_clone_apply");
            let mut undo = None;
            if let Some(pending) = pending {
                let full = pending.full;
                let lean = self.status_only;
                let cache = self.cache.get_or_insert_with(|| cache::Cache::new(lean));
                let staged = cache.stage(&self.db, pending, now, item_inline_ok);
                counters.cache_rows_read += staged.read;
                undo = Some(staged);
                if full {
                    counters.cache_full = true;
                }
            } else if clock_only {
                if let Some(cache) = self.cache.as_mut() {
                    cache.roll_window(now);
                }
            }
            drop(staging);
            let mut report = source::ReadReport::default();
            let plan_check = source::timing::phase("thread_plan");
            let thread_scan = if skip {
                false
            } else {
                match self.thread_scan_plan {
                    Some((schema, ok))
                        if schema == pinned_schema
                            && self.cache.as_ref().is_some_and(|c| c.index_ready()) =>
                    {
                        ok
                    }
                    _ => {
                        let ok = self
                            .db
                            .prepare(&format!(
                                "EXPLAIN QUERY PLAN SELECT {} AS payload FROM orchestration_v2_projection_threads WHERE deleted_at IS NULL AND archived_at IS NULL",
                                source::projection("threads")
                            ))
                            .and_then(|mut stmt| {
                                stmt.query_map([], |r| r.get::<_, String>(3))?
                                    .collect::<rusqlite::Result<Vec<_>>>()
                            })
                            .is_ok_and(|p| p == ["SCAN orchestration_v2_projection_threads"]);
                        self.thread_scan_plan = Some((pinned_schema, ok));
                        ok
                    }
                }
            };
            drop(plan_check);
            let invalidation = if skip {
                None
            } else if replaced {
                Some(FallbackReason::DatabaseReplaced)
            } else if self.previous.is_none() {
                Some(FallbackReason::InitialBaseline)
            } else if pinned_schema != version.1 || self.version.is_some_and(|v| v.1 != version.1)
            {
                Some(FallbackReason::SchemaChanged)
            } else if environment != self.environment {
                Some(FallbackReason::EnvironmentChanged)
            } else if self.previous.as_ref().is_some_and(|p| now < p.at) {
                Some(FallbackReason::BackwardsClock)
            } else {
                None
            };
            macro_rules! orders {
                ($count:expr, $runs:expr, $runless:expr) => {{
                    let count = $count;
                    let runs = $runs;
                    let runless = $runless;
                    let plans_phase = source::timing::phase("encounter_plans");
                    self.sub_plans.retain(|(schema, _), _| *schema == pinned_schema);
                    let order = self
                        .sub_plans
                        .entry((pinned_schema, count))
                        .or_insert_with(|| cache::subagent_order(&self.db, count))
                        .clone();
                    self.request_plans.retain(|(schema, _), _| *schema == pinned_schema);
                    let request_order = self
                        .request_plans
                        .entry((pinned_schema, count))
                        .or_insert_with(|| cache::request_order(&self.db, count))
                        .clone();
                    self.event_plans.retain(|(schema, _), _| *schema == pinned_schema);
                    let event_order = self
                        .event_plans
                        .entry((pinned_schema, count))
                        .or_insert_with(|| cache::event_order(&self.db, count))
                        .clone();
                    self.plan_plans.retain(|(schema, _), _| *schema == pinned_schema);
                    let plan_order = self
                        .plan_plans
                        .entry((pinned_schema, runs))
                        .or_insert_with(|| cache::plan_order(&self.db, runs))
                        .clone();
                    self.item_plans
                        .retain(|(schema, _, _), _| *schema == pinned_schema);
                    let item_order = self
                        .item_plans
                        .entry((pinned_schema, runs, runless))
                        .or_insert_with(|| cache::item_order(&self.db, runs, runless))
                        .clone();
                    self.span_plans.retain(|(schema, _), _| *schema == pinned_schema);
                    let span_order = self
                        .span_plans
                        .entry((pinned_schema, count))
                        .or_insert_with(|| cache::span_order(&self.db, count))
                        .clone();
                    drop(plans_phase);
                    (order, request_order, event_order, plan_order, item_order, span_order)
                }};
            }
            let changed_owners = if clock_only {
                BTreeSet::new()
            } else {
                feed.as_ref()
                    .filter(|f| f.complete)
                    .map(|f| f.threads.clone())
                    .unwrap_or_default()
            };
            let mut text_fault = false;
            let text_phase = source::timing::phase("text_sync");
            if !skip && cache_kept {
                if self.force_text_drift {
                    if let Some(cache) = self.cache.as_mut() {
                        cache.arm_text_drift();
                        self.force_text_drift = false;
                    }
                }
                if let Some(cache) = self.cache.as_mut() {
                    match cache.sync_text(&self.db, &self.path, &changed_owners) {
                        Ok(rows) => counters.cache_rows_read += rows,
                        Err(reason) => {
                            text_fault = true;
                            counters.fallback_reason = Some(reason);
                        }
                    }
                }
            }
            drop(text_phase);
            let mut rebuilt = None;
            let mut incremental = false;
            let full_cache_reason = if invalidation.is_some() {
                invalidation
            } else if !counters.feed_complete && !clock_only {
                Some(if uncommitted {
                    FallbackReason::WindowExpired
                } else {
                    counters
                        .fallback_reason
                        .unwrap_or(FallbackReason::FeedUnavailable)
                })
            } else {
                None
            };
            let assembled = if skip {
                None
            } else if let Some(reason) = full_cache_reason {
                // The rows were just read in full, in this transaction, into a new cache.
                // Assemble from them, as an incremental poll does, in place of a second read
                // of the same tables. It still counts as a full read (`Served::Full`), and
                // anything the cache refuses is read as the reference reads it.
                if !text_fault {
                    counters.fallback_reason = Some(reason);
                }
                if !text_fault
                    && cache_kept
                    && thread_scan
                    && self.cache.as_ref().is_some_and(|c| c.index_ready())
                {
                    let count = self.cache.as_ref().unwrap().active_count();
                    let runs = self.cache.as_ref().unwrap().run_ref_count();
                    let runless = self.cache.as_ref().unwrap().runless_count();
                    let (order, request_order, event_order, plan_order, item_order, span_order) =
                        orders!(count, runs, runless);
                    self.cache.as_mut().unwrap().mark_inputs_stale();
                    match self.cache.as_mut().unwrap().assemble_fast(
                        &self.path,
                        now,
                        None,
                        order,
                        request_order,
                        event_order,
                        plan_order,
                        item_order,
                        span_order,
                        &BTreeSet::new(),
                        &mut report,
                    ) {
                        Ok(out) => {
                            report.threads = self.cache.as_ref().unwrap().active_count();
                            rebuilt = Some(out.rebuilt);
                            Some(out.snapshot)
                        }
                        Err(_) => {
                            self.cache.as_mut().unwrap().mark_inputs_stale();
                            None
                        }
                    }
                } else {
                    None
                }
            } else if text_fault {
                None
            } else if !thread_scan {
                counters.fallback_reason = Some(FallbackReason::ThreadScanPlan);
                None
            } else if !cache_kept {
                counters.fallback_reason = Some(FallbackReason::CacheUnavailable);
                None
            } else if self.cache.is_some() {
                let count = self.cache.as_ref().unwrap().active_count();
                let runs = self.cache.as_ref().unwrap().run_ref_count();
                let runless = self.cache.as_ref().unwrap().runless_count();
                let (order, request_order, event_order, plan_order, item_order, span_order) =
                    orders!(count, runs, runless);
                let fast = self.cache.as_mut().unwrap().assemble_fast(
                    &self.path,
                    now,
                    self.previous.as_ref(),
                    order.clone(),
                    request_order.clone(),
                    event_order.clone(),
                    plan_order.clone(),
                    item_order.clone(),
                    span_order.clone(),
                    &changed_owners,
                    &mut report,
                );
                match fast {
                    Ok(out) => {
                        rebuilt = Some(out.rebuilt);
                        incremental = true;
                        counters.fallback_reason = None;
                        Some(out.snapshot)
                    }
                    Err(reason) if reason.refuses_cached_assembly() => {
                        // An unproved link shape. The cache's whole-snapshot
                        // assembly is not proved for it either: read in full.
                        if let Some(cache) = self.cache.as_mut() {
                            cache.mark_inputs_stale();
                        }
                        counters.fallback_reason = Some(reason);
                        None
                    }
                    Err(reason) => {
                        if let Some(cache) = self.cache.as_mut() {
                            // Membership recorded by a failed fast path would be stale.
                            cache.mark_inputs_stale();
                            match cache.snapshot(
                                &self.path,
                                now,
                                &mut report,
                                order,
                                request_order,
                                event_order,
                                plan_order,
                                item_order,
                                span_order,
                            ) {
                                Ok(snapshot) => {
                                    incremental = true;
                                    counters.fallback_reason = None;
                                    Some(snapshot)
                                }
                                Err(reason) => {
                                    counters.fallback_reason = Some(reason);
                                    None
                                }
                            }
                        } else {
                            counters.fallback_reason = Some(reason);
                            None
                        }
                    }
                }
            } else {
                counters.fallback_reason = Some(FallbackReason::CacheUnavailable);
                None
            };
            let mut snapshot = if skip {
                let mut s = self.previous.as_ref().unwrap().clone();
                s.at = now;
                s
            } else {
                match assembled {
                    Some(mut snapshot) => {
                        let asks_phase = source::timing::phase("ask_counts");
                        let mut cards = HashMap::new();
                        let mut estimates = HashMap::new();
                        if incremental {
                            // Thread ask counts depend on rows outside the cached item
                            // window. The verified suffix covers card, run and message events.
                            let owners: Vec<_> = changed_owners.iter().cloned().collect();
                            for batch in owners.chunks(500) {
                                let counts = (|| -> rusqlite::Result<()> {
                                    cards.extend(source::read_ask_cards(&self.db, Some(batch))?);
                                    estimates.extend(source::read_estimated_asks(&self.db, Some(batch))?);
                                    Ok(())
                                })();
                                if let Err(e) = counts {
                                    if let (Some(cache), Some(undo)) = (self.cache.as_mut(), undo.take()) {
                                        cache.restore(undo);
                                    }
                                    return Err(error(&self.path, e));
                                }
                            }
                        } else {
                            let counts = (|| -> rusqlite::Result<()> {
                                cards = source::read_ask_cards(&self.db, None)?;
                                estimates = source::read_estimated_asks(&self.db, None)?;
                                Ok(())
                            })();
                            if let Err(e) = counts {
                                if let (Some(cache), Some(undo)) = (self.cache.as_mut(), undo.take()) {
                                    cache.restore(undo);
                                }
                                return Err(error(&self.path, e));
                            }
                        }
                        counters.cache_rows_read += cards.len() + estimates.len();
                        source::apply_thread_asks(
                            &mut snapshot,
                            &cards,
                            &estimates,
                            incremental
                                .then(|| self.previous.as_ref().map(|previous| (previous, &changed_owners)))
                                .flatten(),
                        );
                        drop(asks_phase);
                        snapshot
                    }
                    None => match source::read_tracked(&self.db, &self.path, now, &mut report) {
                        Ok(mut snapshot) => {
                            if self.status_only {
                                view_snapshot(&mut snapshot);
                            }
                            snapshot
                        }
                        Err(error) => {
                            if let Some(undo) = undo.take() {
                                if let Some(cache) = self.cache.as_mut() {
                                    cache.restore(undo);
                                }
                            }
                            return Err(error);
                        }
                    },
                }
            };
            // A poll that did not paint from the shells staged rows they never
            // absorbed, and any full read leaves them behind the snapshot. Rebuild
            // shells, routes and targets from nothing so the next changed poll
            // starts sealed. A refusal leaves them unsealed; the next poll asks again.
            if !skip
                && cache_kept
                && rebuilt.is_none()
                && self.cache.as_ref().is_some_and(|c| c.index_ready())
            {
                let count = self.cache.as_ref().unwrap().active_count();
                let runs = self.cache.as_ref().unwrap().run_ref_count();
                let runless = self.cache.as_ref().unwrap().runless_count();
                let (order, request_order, event_order, plan_order, item_order, span_order) =
                    orders!(count, runs, runless);
                let sealing = source::timing::phase("seal");
                let _ = self.cache.as_mut().unwrap().seal(
                    &self.path,
                    now,
                    order,
                    request_order,
                    event_order,
                    plan_order,
                    item_order,
                    span_order,
                );
                drop(sealing);
            }
            counters.read = read_start.elapsed();
            counters.rows_read = report.rows + counters.cache_rows_read;
            counters.threads_reread = report.threads;
            let reconcile = Instant::now();
            let changed = if skip {
                Changed::Agents(Vec::new())
            } else if let Some(rebuilt) = rebuilt.as_ref() {
                reconcile_rebuilt(self.previous.as_ref(), &mut snapshot, rebuilt)
            } else {
                reconcile_agents(self.previous.as_ref(), &mut snapshot)
            };
            counters.reconcile = reconcile.elapsed();
            if source::timing::on() {
                counters.phases = Some(source::timing::take());
            }
            let check = Instant::now();
            let mismatch = if self.self_check {
                let mut oracle = source::read(&self.db, &self.path, now)?;
                if self.status_only {
                    view_snapshot(&mut oracle);
                }
                serde_json::to_value(&snapshot).unwrap() != serde_json::to_value(&oracle).unwrap()
                    || snapshot.agents.iter().zip(&oracle.agents)
                        .any(|(a, b)| a.open_asks != b.open_asks
                            || a.estimated_asks != b.estimated_asks
                            || a.storage != b.storage)
            } else {
                false
            };
            if self.self_check {
                counters.self_check = check.elapsed();
            }
            Ok::<_, SourceError>((
                snapshot,
                changed,
                report.valid_until,
                mismatch,
                feed.map(|f| f.next),
                undo,
                incremental,
                cache_kept,
                counters.cache_full,
            ))
        })();
        let rollback = self
            .db
            .execute_batch("ROLLBACK")
            .map_err(|e| sql_error(&self.path, e));
        let (snapshot, changed, deadline, mismatch, feed, undo, incremental, cache_kept, cache_full) =
            match result {
                Ok(v) => {
                    rollback?;
                    v
                }
                Err(e) => {
                    let _ = rollback;
                    self.version = None;
                    self.feed = None;
                    return Err(e);
                }
            };
        // A full read's temporaries are gone. Return the arenas they peaked in
        // so the resident size follows the cache, not that one allocation spike.
        #[cfg(target_os = "linux")]
        if cache_full {
            unsafe { malloc_trim(0) };
        }
        #[cfg(not(target_os = "linux"))]
        let _ = cache_full;
        if identity(&self.path)? != self.identity {
            if let Some(undo) = undo {
                if let Some(cache) = self.cache.as_mut() {
                    cache.restore(undo);
                }
            }
            self.version = None;
            return Err(error(&self.path, "database replaced during poll; retry"));
        }
        self.mismatches += u64::from(mismatch);
        counters.mismatches = self.mismatches;
        self.version = if mismatch { None } else { Some(version) };
        self.environment = environment;
        let publish = counters.phases.as_ref().map(|_| Instant::now());
        if !skip {
            if mismatch || !cache_kept {
                self.cache = None;
            }
            self.feed = if mismatch { None } else { feed };
            self.valid_until = deadline;
        }
        self.previous = Some(snapshot.clone());
        if let Some(start) = publish {
            counters
                .phases
                .as_mut()
                .unwrap()
                .phases_ns
                .insert("publish_release".into(), start.elapsed().as_nanos() as u64);
        }
        counters.total = start.elapsed();
        Ok(Update {
            snapshot,
            changed,
            served: if skip {
                Served::Skipped
            } else if incremental {
                Served::Incremental
            } else {
                Served::Full
            },
            counters,
            mismatch,
        })
    }
}
fn reconcile_agents(previous: Option<&Snapshot>, snapshot: &mut Snapshot) -> Changed {
    let Some(previous) = previous else {
        return Changed::Everything;
    };
    let mut old: HashMap<_, _> = previous.agents.iter().map(|a| (a.id.as_str(), a)).collect();
    let mut changed = Vec::new();
    for agent in &mut snapshot.agents {
        if let Some(before) = old.remove(agent.id.as_str()) {
            if Arc::ptr_eq(before, agent) || **before == **agent {
                *agent = before.clone();
            } else {
                changed.push(agent.id.clone());
            }
        } else {
            changed.push(agent.id.clone());
        }
    }
    changed.extend(old.keys().map(|id| id.to_string()));
    changed.sort();
    changed.dedup();
    Changed::Agents(changed)
}
/// Compare only agents the fast path rebuilt. Clean agents are already the previous `Arc`.
fn reconcile_rebuilt(
    previous: Option<&Snapshot>,
    snapshot: &mut Snapshot,
    rebuilt: &[String],
) -> Changed {
    let Some(previous) = previous else {
        return Changed::Everything;
    };
    let old: HashMap<_, _> = previous.agents.iter().map(|a| (a.id.as_str(), a)).collect();
    let rebuilt: HashSet<&str> = rebuilt.iter().map(String::as_str).collect();
    let mut changed = Vec::new();
    for agent in &mut snapshot.agents {
        if let Some(before) = old.get(agent.id.as_str()) {
            if rebuilt.contains(agent.id.as_str()) && (Arc::ptr_eq(before, agent) || ***before == **agent)
            {
                *agent = Arc::clone(before);
            } else if rebuilt.contains(agent.id.as_str()) || !Arc::ptr_eq(before, agent) {
                changed.push(agent.id.clone());
            }
        } else {
            changed.push(agent.id.clone());
        }
    }
    let present: HashSet<&str> = snapshot.agents.iter().map(|agent| agent.id.as_str()).collect();
    for id in old.keys() {
        if !present.contains(id) {
            changed.push((*id).to_string());
        }
    }
    changed.sort();
    changed.dedup();
    Changed::Agents(changed)
}

/// Nonblocking latest-value mailbox. Drop signals shutdown without waiting for
/// an in-flight SQLite read; the worker exits after that bounded unit of work.
pub struct Handle {
    latest: Arc<Mutex<Option<Result<Update, SourceError>>>>,
    stop: Option<mpsc::Sender<()>>,
}
impl Handle {
    pub fn take(&self) -> Option<Result<Update, SourceError>> {
        self.latest.lock().unwrap().take()
    }
    /// A mailbox with no reader thread behind it, for the presenter's deterministic stream
    /// check: the check polls its own [`Reader`] at a scripted clock and posts the result with
    /// [`Handle::offer`], so the host takes it exactly as it takes the worker's.
    #[cfg(feature = "frame-stream")]
    pub fn detached() -> Self {
        Self { latest: Arc::new(Mutex::new(None)), stop: None }
    }
    /// Put a finished poll in the mailbox, replacing one not yet taken, as the worker does.
    #[cfg(feature = "frame-stream")]
    pub fn offer(&self, result: Result<Update, SourceError>) {
        *self.latest.lock().unwrap() = Some(result);
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.stop.take();
    }
}
pub fn spawn(
    path: impl AsRef<Path>,
    interval: Duration,
    wake: impl Fn() + Send + 'static,
) -> Handle {
    spawn_inner(path, None, interval, wake)
}
pub fn spawn_with_reader(
    path: impl AsRef<Path>,
    reader: Reader,
    interval: Duration,
    wake: impl Fn() + Send + 'static,
) -> Handle {
    spawn_inner(path, Some(reader), interval, wake)
}
fn spawn_inner(
    path: impl AsRef<Path>,
    initial: Option<Reader>,
    interval: Duration,
    wake: impl Fn() + Send + 'static,
) -> Handle {
    let path = path.as_ref().to_owned();
    let latest = Arc::new(Mutex::new(None));
    let mailbox = latest.clone();
    let (stop, stopped) = mpsc::channel();
    let has_initial = initial.is_some();
    thread::spawn(move || {
        let mut reader = initial;
        if has_initial
            && stopped.recv_timeout(interval.max(Duration::from_millis(1)))
                != Err(mpsc::RecvTimeoutError::Timeout)
        {
            return;
        }
        loop {
            if stopped.try_recv() != Err(mpsc::TryRecvError::Empty) {
                break;
            }
            let result = match reader.as_mut() {
                Some(r) => Reader::poll(r, crate::clock::wall_now()),
                None => match Reader::open(&path) {
                    Ok(mut r) => {
                        let result = r.poll(crate::clock::wall_now());
                        reader = Some(r);
                        result
                    }
                    Err(e) => Err(e),
                },
            };
            if stopped.try_recv() != Err(mpsc::TryRecvError::Empty) {
                break;
            }
            // A full read decodes every row before the cache keeps its keys.
            if matches!(&result, Ok(update) if update.served == Served::Full) {
                crate::clock::release_memory();
            }
            *mailbox.lock().unwrap() = Some(result);
            wake();
            if stopped.recv_timeout(interval.max(Duration::from_millis(1)))
                != Err(mpsc::RecvTimeoutError::Timeout)
            {
                break;
            }
        }
    });
    Handle {
        latest,
        stop: Some(stop),
    }
}

#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod privacy_support;

#[cfg(test)]
mod privacy_tests {
    use super::privacy_support as support;
    use super::*;
    // sqlite3's authorizer inspects column access at prepare time, including
    // CTEs/window functions. It denies any access outside the oracle columns.
    unsafe extern "C" fn allow(
        _: *mut std::ffi::c_void,
        action: i32,
        table: *const std::ffi::c_char,
        column: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
    ) -> i32 {
        if action != rusqlite::ffi::SQLITE_READ {
            return rusqlite::ffi::SQLITE_OK;
        }
        let table = unsafe { std::ffi::CStr::from_ptr(table) }.to_str().unwrap();
        let column = unsafe { std::ffi::CStr::from_ptr(column) }
            .to_str()
            .unwrap();
        if column == "ROWID"
            && [
                "threads",
                "runs",
                "subagents",
                "provider_threads",
                "provider_turns",
                "nodes",
                "runtime_requests",
                "turn_items",
            ]
            .iter()
            .any(|name| table == format!("orchestration_v2_projection_{name}"))
        {
            return rusqlite::ffi::SQLITE_OK;
        }
        let allowed: &[&str] = match table {
            "sqlite_master" => &["name", "type", "ROWID"],
            "pragma_table_info" => &["name"],
            "orchestration_v2_events" => &["sequence", "thread_id"],
            "orchestration_events" => &[
                "sequence",
                "stream_id",
                "aggregate_kind",
                "application_event_version",
            ],
            "orchestration_v2_projection_metadata" => &["projection_name", "last_sequence"],
            // The number of rows and nothing else: SQLite names no column when a
            // query only counts a table's rows (the empty name below).
            "effect_sql_migrations" => &[""],
            "projection_projects" => &["project_id", "title", "workspace_root"],
            "orchestration_v2_projection_threads" => {
                &["payload_json", "thread_id", "archived_at", "deleted_at"]
            }
            "orchestration_v2_projection_runs" => &[
                "payload_json",
                "thread_id",
                "ordinal",
                "requested_at",
                "completed_at",
                "status",
            ],
            "orchestration_v2_projection_subagents"
            | "orchestration_v2_projection_provider_threads" => &["payload_json", "thread_id"],
            "orchestration_v2_projection_nodes" => {
                &["node_id", "parent_node_id", "run_id", "thread_id"]
            }
            "orchestration_v2_projection_provider_turns" => &[
                "thread_id",
                "provider_thread_id",
                "ordinal",
                "started_at",
                "completed_at",
                "payload_json",
                "ROWID",
            ],
            "orchestration_v2_projection_runtime_requests" => &[
                "thread_id",
                "node_id",
                "kind",
                "status",
                "created_at",
                "resolved_at",
                "payload_json",
            ],
            "orchestration_v2_projection_messages" => &[
                "thread_id",
                "message_id",
                "node_id",
                "role",
                "streaming",
                "created_at",
                "updated_at",
                "payload_json",
            ],
            "orchestration_v2_projection_turn_items" => &[
                "thread_id",
                "run_id",
                "node_id",
                "provider_thread_id",
                "type",
                "status",
                "updated_at",
                "payload_json",
                "ordinal",
            ],
            _ => &[],
        };
        if allowed.contains(&column) {
            rusqlite::ffi::SQLITE_OK
        } else {
            rusqlite::ffi::SQLITE_DENY
        }
    }
    #[test]
    fn per_thread_projection_reads_are_index_searches_under_the_allowlist() {
        let mut db = support::Db::new();
        db.write("a", 0);
        unsafe {
            assert_eq!(
                rusqlite::ffi::sqlite3_set_authorizer(
                    db.db.handle(),
                    Some(allow),
                    std::ptr::null_mut()
                ),
                rusqlite::ffi::SQLITE_OK
            );
        }
        assert_eq!(
            cache::subagent_order(&db.db, 3),
            Some(cache::EncounterOrder::ThreadIndex(
                "orchestration_v2_projection_subagents_thread_idx".into()
            ))
        );
        assert_eq!(
            cache::request_order(&db.db, 3),
            Some(cache::EncounterOrder::ThreadIndex(
                "orchestration_v2_projection_runtime_requests_thread_status_idx".into()
            ))
        );
        assert_eq!(
            cache::event_order(&db.db, 3),
            Some(cache::EncounterOrder::ThreadIndex(
                "orchestration_v2_projection_turn_items_thread_run_idx".into()
            ))
        );
        assert_eq!(
            cache::plan_order(&db.db, 3),
            Some(cache::PlanOrder::RunOrdinal(
                "orchestration_v2_projection_turn_items_run_ordinal_idx".into()
            ))
        );
        assert_eq!(cache::plan_order(&db.db, 0), cache::plan_order(&db.db, 3));
        assert_eq!(
            cache::item_order(&db.db, 2, 1),
            Some(cache::ItemOrder::RunOr {
                run_index: "orchestration_v2_projection_turn_items_run_ordinal_idx".into(),
                thread_index: "orchestration_v2_projection_turn_items_thread_run_idx".into(),
            })
        );
        assert_eq!(cache::item_order(&db.db, 0, 0), cache::item_order(&db.db, 2, 1));
        assert_eq!(
            cache::span_order(&db.db, 3),
            Some(cache::EncounterOrder::ThreadIndex(
                "orchestration_v2_projection_runs_thread_ordinal_idx".into()
            ))
        );
        assert_eq!(cache::span_order(&db.db, 0), cache::span_order(&db.db, 3));
        for (name, sql) in cache::queries() {
            let sql = format!("{sql} WHERE thread_id=?");
            let plans: Vec<String> = db
                .db
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(["a"], |row| row.get(3))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            println!("per-thread {name}: {}", plans.join("; "));
            assert_eq!(plans.len(), 1, "{name}: {plans:?}");
            assert!(plans[0].starts_with("SEARCH "), "{name}: {plans:?}");
            assert!(
                plans[0].contains("INDEX ") && plans[0].contains("(thread_id=?)"),
                "{name}: {plans:?}"
            );
            let mut query = db.db.prepare(&sql).unwrap();
            let mut rows = query.query(["a"]).unwrap();
            while rows.next().unwrap().is_some() {}
        }
        // A run missing from its previous owner is checked by the already
        // approved rowid, without adding a primary key or payload field.
        let sql = format!("{} WHERE rowid=?", cache::queries()[1].1);
        let plans: Vec<String> = db.db.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap().query_map([1i64], |row| row.get(3)).unwrap()
            .collect::<Result<_, _>>().unwrap();
        assert_eq!(plans, ["SEARCH orchestration_v2_projection_runs USING INTEGER PRIMARY KEY (rowid=?)"]);
        let mut query = db.db.prepare(&sql).unwrap();
        let mut rows = query.query([1i64]).unwrap();
        while rows.next().unwrap().is_some() {}
        // A newly selected run's rows, read by run id: the turn-item key
        // projection on the (run_id, ordinal) index, under the same authorizer.
        let sql = cache::run_query();
        let plans: Vec<String> = db
            .db
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(["r"], |row| row.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        println!("per-run turn_items: {}", plans.join("; "));
        assert_eq!(
            plans,
            ["SEARCH orchestration_v2_projection_turn_items USING INDEX orchestration_v2_projection_turn_items_run_ordinal_idx (run_id=?)"]
        );
        let mut query = db.db.prepare(&sql).unwrap();
        let mut rows = query.query(["r"]).unwrap();
        while rows.next().unwrap().is_some() {}
    }

    #[test]
    fn explicit_primary_keys_remain_outside_the_privacy_boundary() {
        let db = support::Db::new();
        unsafe {
            assert_eq!(
                rusqlite::ffi::sqlite3_set_authorizer(
                    db.db.handle(),
                    Some(allow),
                    std::ptr::null_mut()
                ),
                rusqlite::ffi::SQLITE_OK
            );
        }
        for (table, key) in [
            ("runs", "run_id"),
            ("subagents", "subagent_id"),
            ("provider_threads", "provider_thread_id"),
            ("turn_items", "turn_item_id"),
            ("runtime_requests", "runtime_request_id"),
        ] {
            assert!(
                db.db
                    .prepare(&format!(
                        "SELECT {key} FROM orchestration_v2_projection_{table} WHERE thread_id=?"
                    ))
                    .is_err(),
                "unexpectedly allowed {table}.{key}"
            );
        }
    }

    #[test]
    fn event_suffix_is_a_primary_key_range_search() {
        let db = support::Db::new();
        let mut statement = db
            .db
            .prepare(&format!("EXPLAIN QUERY PLAN {EVENT_SUFFIX}"))
            .unwrap();
        let plans: Vec<String> = statement
            .query_map([1], |row| row.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        println!("event suffix plan: {}", plans.join("; "));
        assert_eq!(
            plans,
            ["SEARCH orchestration_v2_events USING INTEGER PRIMARY KEY (rowid>?)"]
        );
        // The real log has partial and composite indexes on `sequence`. The suffix must
        // still be a range over the rowid, so a poll costs what was appended and no more.
        let mut statement = db
            .db
            .prepare(&format!("EXPLAIN QUERY PLAN {APPLICATION_SUFFIX}"))
            .unwrap();
        let plans: Vec<String> = statement
            .query_map([1], |row| row.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            plans,
            ["SEARCH orchestration_events USING INTEGER PRIMARY KEY (rowid>?)"]
        );
    }

    #[test]
    fn feed_cursor_and_thread_ids_share_the_pinned_snapshot() {
        let mut db = support::Db::new();
        db.write("a", 0);
        let reader = Reader::open(&db.path).unwrap();
        reader.db.execute_batch("BEGIN").unwrap();
        let first = read_feed(&reader.db, None, &mut None).unwrap().unwrap();
        assert_eq!(first.next.cursor, 1);
        db.write("b", 1);
        let pinned = read_feed(&reader.db, Some(&first.next), &mut None)
            .unwrap()
            .unwrap();
        assert_eq!(pinned.next.cursor, 1);
        assert!(pinned.threads.is_empty());
        reader.db.execute_batch("ROLLBACK; BEGIN").unwrap();
        let next = read_feed(&reader.db, Some(&first.next), &mut None)
            .unwrap()
            .unwrap();
        assert!(next.complete);
        assert_eq!(next.next.cursor, 2);
        assert_eq!(next.threads, BTreeSet::from(["b".to_owned()]));
        reader.db.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn only_integer_command_exit_codes_cross_the_dashboard_projection() {
        let db = support::Db::new();
        db.db.execute_batch(r#"
            INSERT INTO orchestration_v2_projection_turn_items
                (turn_item_id,thread_id,ordinal,type,status,updated_at,payload_json)
            VALUES ('tool','a',1,'dynamic_tool','failed','','{"exitCode":"EXIT_SENTINEL"}'),
                   ('bad-command','a',2,'command_execution','failed','','{"exitCode":"EXIT_SENTINEL"}'),
                   ('valid-command','a',3,'command_execution','failed','','{"exitCode":17}'),
                   ('numeric-tool','a',4,'dynamic_tool','failed','','{"exitCode":18}'),
                   ('oversized','a',5,'command_execution','failed','','{"exitCode":2147483648}'),
                   ('boolean','a',6,'command_execution','failed','','{"exitCode":true}');
        "#).unwrap();
        let keys = format!("{} ORDER BY ordinal", cache::queries()[7].1);
        let items = format!("SELECT {} FROM orchestration_v2_projection_turn_items ORDER BY ordinal", source::item_fields());
        for sql in [keys, items] {
            let mut stmt = db.db.prepare(&sql).unwrap();
            let exit = stmt.column_index("exit_code").unwrap();
            let values = stmt.query_map([], |row| row.get::<_, rusqlite::types::Value>(exit))
                .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
            use rusqlite::types::Value::{Integer, Null};
            assert_eq!(values, [Null, Null, Integer(17), Null, Null, Null], "{sql}");
        }
    }

    #[test]
    fn sql_column_allowlist_is_enforced() {
        let mut db = support::Db::new();
        db.write("a", 0);
        db.db.execute_batch(r#"
            INSERT INTO orchestration_v2_projection_runs(run_id,thread_id,ordinal,provider,status,requested_at,payload_json)
            VALUES('r','a',1,'codex','running','','{"id":"r","threadId":"a","status":"running","prompt":"SECRET"}');
            INSERT INTO orchestration_v2_projection_subagents(subagent_id,thread_id,parent_node_id,provider,origin,status,updated_at,payload_json)
            VALUES('s','a','n','codex','native','running','','{"id":"s","threadId":"a","parentNodeId":"n","prompt":"SECRET"}');
            INSERT INTO orchestration_v2_projection_provider_threads(provider_thread_id,thread_id,provider,status,updated_at,payload_json)
            VALUES('pt','a','codex','running','','{"id":"pt","contextUsage":{"usedTokens":4},"prompt":"SECRET"}');
            INSERT INTO orchestration_v2_projection_provider_turns(provider_turn_id,thread_id,provider_thread_id,node_id,ordinal,status,payload_json)
            VALUES('turn','a','pt','n',1,'running','{"providerThreadId":"pt","model":"synthetic","tokenUsage":{"usedTokens":4,"maxTokens":100},"prompt":"SECRET"}');
            INSERT INTO orchestration_v2_projection_nodes(node_id,thread_id,root_node_id,kind,status,payload_json)
            VALUES('n','a','n','run','running','{"prompt":"SECRET"}');
            INSERT INTO orchestration_v2_projection_runtime_requests(runtime_request_id,thread_id,node_id,kind,status,created_at,payload_json)
            VALUES('req','a','n','approval','pending','','{"prompt":"SECRET"}');
            INSERT INTO orchestration_v2_projection_turn_items(turn_item_id,thread_id,run_id,node_id,ordinal,type,status,updated_at,payload_json)
            VALUES('item','a','r','n',1,'command_execution','running','','{"input":"synthetic command","prompt":"SECRET"}');
        "#).unwrap();
        db.db.execute_batch(r#"
            INSERT INTO orchestration_v2_projection_turn_items(turn_item_id,thread_id,run_id,node_id,ordinal,type,status,updated_at,payload_json)
            VALUES('plan','a','r','n',2,'todo_list','completed','2026-11-12T12:00:00.000Z','{"steps":[{"text":"Synthetic step","status":"pending"}]}');
        "#).unwrap();
        db.db
            .execute_batch(
                "INSERT INTO effect_sql_migrations(migration_id,name) VALUES(1,'SECRET_MIGRATION'),(2,'SECRET_MIGRATION_2')",
            )
            .unwrap();
        let mut reader = Reader::open(&db.path).unwrap().with_self_check(true);
        // SAFETY: callback captures no state; handle remains alive throughout.
        unsafe {
            assert_eq!(
                rusqlite::ffi::sqlite3_set_authorizer(
                    reader.db.handle(),
                    Some(allow),
                    std::ptr::null_mut()
                ),
                rusqlite::ffi::SQLITE_OK
            );
        }
        let mut pool = cache::Pool::default();
        let pending = cache::Pending::read(&reader.db, &reader.path, None, &mut pool).unwrap();
        assert!(!format!("{:?}", pending).contains("SECRET"));
        cache::Pending::read(
            &reader.db,
            &reader.path,
            Some(&BTreeSet::from(["a".into()])),
            &mut pool,
        )
        .unwrap();
        for table in ["subagents", "runtime_requests", "turn_items"] {
            let sql = format!("SELECT DISTINCT thread_id FROM orchestration_v2_projection_{table}");
            let mut query = reader.db.prepare(&sql).unwrap();
            let mut rows = query.query([]).unwrap();
            while rows.next().unwrap().is_some() {}
        }
        {
            let mut text = reader.db.prepare(&cache::item_text_query(1)).unwrap();
            let mut rows = text.query([1i64]).unwrap();
            while rows.next().unwrap().is_some() {}
        }
        let u = reader.poll(1_794_484_800_000.).unwrap();
        assert!(
            !serde_json::to_string(&u.snapshot)
                .unwrap()
                .contains("SECRET")
        );
        db.write("a", 1);
        let update = reader.poll(1_794_484_800_000.).unwrap();
        assert!(!update.mismatch);
        assert_eq!(update.served, Served::Incremental);
        assert!(update.counters.feed_complete);
        assert_eq!(update.counters.changed_threads, 1);
        // The ledger's row count is read under the authorizer, and no column of it.
        assert_eq!(ledger_count(&reader.db), Some(2));
        db.db
            .execute("INSERT INTO orchestration_command_receipts VALUES('c','accepted')", [])
            .unwrap();
        let quiet = reader.poll(1_794_484_800_000.).unwrap();
        assert!(!quiet.mismatch);
        assert_eq!(quiet.served, Served::Incremental, "{:?}", quiet.counters.fallback_reason);
        assert!(!quiet.counters.feed_complete);
        db.migrate("SecretRepair", "");
        assert_eq!(
            reader.poll(1_794_484_800_000.).unwrap().counters.fallback_reason,
            Some(FallbackReason::LedgerChanged)
        );
        for sql in [
            "SELECT migration_id FROM effect_sql_migrations",
            "SELECT name FROM effect_sql_migrations",
            "SELECT created_at FROM effect_sql_migrations",
            "SELECT rowid FROM effect_sql_migrations",
            "SELECT MAX(migration_id) FROM effect_sql_migrations",
            "SELECT COUNT(name) FROM effect_sql_migrations",
            "SELECT COUNT(*) FROM effect_sql_migrations WHERE migration_id > 1",
            "SELECT COUNT(*) FROM effect_sql_migrations WHERE name LIKE 'S%'",
        ] {
            assert!(reader.db.prepare(sql).is_err(), "unexpectedly allowed: {sql}");
        }
        assert!(
            reader
                .db
                .prepare("SELECT updated_at FROM orchestration_v2_projection_metadata")
                .is_err()
        );
        assert!(
            reader
                .db
                .prepare("SELECT payload_json FROM orchestration_v2_events")
                .is_err()
        );
        assert!(
            reader
                .db
                .prepare("SELECT payload_json FROM orchestration_events")
                .is_err()
        );
        assert!(
            reader
                .db
                .prepare("SELECT default_provider FROM orchestration_v2_projection_threads")
                .is_err()
        );
    }

    #[test]
    fn moved_run_from_archived_thread_recovers_with_zero_mismatches() {
        let mut db = support::Db::new();
        db.write("arch", 0);
        db.write("dest", 1);
        db.db
            .execute_batch(
                r#"
                UPDATE orchestration_v2_projection_threads
                   SET archived_at='2026-11-12T10:00:00.000Z'
                 WHERE thread_id='arch';
                INSERT INTO orchestration_v2_projection_runs
                    (run_id,thread_id,ordinal,provider,status,requested_at,payload_json)
                VALUES
                    ('r_move','arch',1,'codex','running','2026-11-12T11:00:00.000Z',
                     '{"id":"r_move","threadId":"arch","status":"running","startedAt":"2026-11-12T11:00:00.000Z"}');
                INSERT INTO orchestration_v2_projection_turn_items
                    (turn_item_id,thread_id,run_id,ordinal,type,status,updated_at,payload_json)
                VALUES
                    ('ti_move','dest','r_move',1,'command_execution','running','2026-11-12T11:30:00.000Z',
                     '{"input":"cargo test"}');
                "#,
            )
            .unwrap();
        let now = 1_794_484_800_000.;
        let mut reader = Reader::open(&db.path).unwrap().with_self_check(true);
        let mut lean_reader = Reader::open(&db.path).unwrap().status_only().with_self_check(true);
        let u1 = reader.poll(now).unwrap();
        assert!(!u1.mismatch);
        assert_eq!(u1.counters.mismatches, 0);
        assert_eq!(u1.snapshot.agents.len(), 1);
        assert_eq!(u1.snapshot.agents[0].id, "dest");
        assert_eq!(u1.snapshot.agents[0].status, "idle");
        let l1 = lean_reader.poll(now).unwrap();
        assert!(!l1.mismatch);
        assert_eq!(l1.counters.mismatches, 0);

        // Move the run out of the archived thread into `dest`, emitting an event ONLY on `arch`.
        db.touch(
            "arch",
            2,
            r#"
            UPDATE orchestration_v2_projection_runs
               SET thread_id='dest',
                   payload_json='{"id":"r_move","threadId":"dest","status":"running","startedAt":"2026-11-12T11:00:00.000Z"}'
             WHERE run_id='r_move';
            "#,
        );
        let u2 = reader.poll(now).unwrap();
        assert!(!u2.mismatch, "moved run from archived thread caused self-check mismatch");
        assert_eq!(u2.counters.mismatches, 0);
        assert_eq!(u2.served, Served::Incremental);
        let dest = u2.snapshot.agents.iter().find(|a| a.id == "dest").unwrap();
        assert_eq!(dest.status, "running");
        let l2 = lean_reader.poll(now).unwrap();
        assert!(!l2.mismatch, "lean reader moved run caused self-check mismatch");
        assert_eq!(l2.counters.mismatches, 0);
        assert_eq!(l2.served, Served::Incremental);
    }

    /// The status reader's own statements (the inline turn-item columns, by owner and by run,
    /// and the keys read by rowid) are held to the same column allowlist as the full reader's.
    #[test]
    fn status_only_reads_stay_inside_the_column_allowlist() {
        let mut db = support::Db::new();
        db.write("a", 0);
        db.write("b", 1);
        let now = 1_794_484_800_000.;
        let mut reader = Reader::open(&db.path).unwrap().status_only().with_self_check(true);
        // SAFETY: callback captures no state; handle remains alive throughout.
        unsafe {
            assert_eq!(
                rusqlite::ffi::sqlite3_set_authorizer(
                    reader.db.handle(),
                    Some(allow),
                    std::ptr::null_mut()
                ),
                rusqlite::ffi::SQLITE_OK
            );
        }
        // The authorizer rules at prepare time, so preparing a statement is the whole check.
        for sql in [
            poll1::item_keys_query(1),
            poll1::item_keys_query(3),
            poll1::run_inline_query(),
            cache::owner_query(7, &poll1::item_inline_query()),
        ] {
            reader.db.prepare(&sql).unwrap();
        }
        let first = reader.poll(now).unwrap();
        assert!(!first.mismatch);
        assert!(first.counters.cache_ready && first.counters.cache_full);
        db.write("a", 2);
        let next = reader.poll(now).unwrap();
        assert!(!next.mismatch);
        assert_eq!(next.served, Served::Incremental, "{:?}", next.counters.fallback_reason);
    }

    /// The launch read's reader goes to the worker, cache and all. The worker's first poll,
    /// one interval later, is then a skipped or an incremental one. A reader it had to open
    /// itself would read everything again.
    #[test]
    fn a_handed_over_reader_is_not_read_in_full_again() {
        let mut db = support::Db::new();
        db.write("a", 0);
        db.write("b", 1);
        let mut reader = Reader::open(&db.path).unwrap();
        let first = reader.poll(crate::clock::wall_now()).unwrap();
        assert_eq!(first.served, Served::Full);
        let (woke, woken) = mpsc::channel();
        let handle = spawn_with_reader(&db.path, reader, Duration::from_millis(20), move || {
            let _ = woke.send(());
        });
        woken
            .recv_timeout(Duration::from_secs(60))
            .expect("the worker polls one interval after the handoff");
        let update = handle.take().expect("the worker's poll is in the mailbox").unwrap();
        assert_ne!(update.served, Served::Full, "{:?}", update.counters.fallback_reason);
        assert_eq!(update.snapshot.agents.len(), first.snapshot.agents.len());
        assert_eq!(
            serde_json::to_value(&update.snapshot.agents).unwrap(),
            serde_json::to_value(&first.snapshot.agents).unwrap()
        );
    }

    #[test]
    fn live_error_and_sql_error_match_source_snapshot_parity() {
        let db = support::Db::new();
        let dir = db.path.parent().unwrap();
        let now = 1_794_484_800_000.;

        // 1) Missing path -> kind: "missing", detail: ""
        let missing = dir.join("does-not-exist.sqlite");
        let src_err = source::snapshot(&missing, now).unwrap_err();
        let live_err = Reader::open(&missing).err().unwrap();
        assert_eq!(live_err.kind, "missing");
        assert_eq!(live_err.detail, "");
        assert_eq!(live_err.kind, src_err.kind);
        assert_eq!(live_err.detail, src_err.detail);

        // 2) Directory path -> kind: "unreadable", detail: "it is a folder, not a file"
        let folder = dir.join("folder.sqlite");
        std::fs::create_dir(&folder).unwrap();
        let src_err = source::snapshot(&folder, now).unwrap_err();
        let live_err = Reader::open(&folder).err().unwrap();
        assert_eq!(live_err.kind, "unreadable");
        assert_eq!(live_err.detail, "it is a folder, not a file");
        assert_eq!(live_err.kind, src_err.kind);
        assert_eq!(live_err.detail, src_err.detail);

        // 3) Non-SQLite file -> kind: "unreadable", detail: "not a SQLite database"
        let bogus = dir.join("not-a-db.sqlite");
        std::fs::write(&bogus, b"this is not a SQLite database\n").unwrap();
        let src_err = source::snapshot(&bogus, now).unwrap_err();
        let live_err = Reader::open(&bogus)
            .and_then(|mut r| r.poll(now).map(|_| ()))
            .unwrap_err();
        assert_eq!(live_err.kind, "unreadable");
        assert_eq!(live_err.detail, "not a SQLite database");
        assert_eq!(live_err.kind, src_err.kind);
        assert_eq!(live_err.detail, src_err.detail);
    }

    #[test]
    fn descending_thread_index_is_rejected_by_index_xinfo_guard() {
        let mut db = support::Db::new();
        db.write("a", 0);
        db.write("b", 1);
        db.db
            .execute_batch(
                r#"
                DROP INDEX orchestration_v2_projection_turn_items_thread_run_idx;
                CREATE INDEX idx_turn_items_thread_desc
                    ON orchestration_v2_projection_turn_items(thread_id DESC, ordinal ASC);
                INSERT INTO orchestration_v2_projection_runs
                    (run_id,thread_id,ordinal,provider,status,requested_at,payload_json)
                VALUES
                    ('r1','a',1,'codex','running','2026-11-12T11:00:00.000Z',
                     '{"id":"r1","threadId":"a","status":"running"}');
                INSERT INTO orchestration_v2_projection_turn_items
                    (turn_item_id,thread_id,run_id,ordinal,type,status,updated_at,payload_json)
                VALUES
                    ('i1','a','r1',1,'command_execution','completed','2026-11-12T11:50:00.000Z','{"input":"cmd1"}'),
                    ('i2','a','r1',2,'command_execution','completed','2026-11-12T11:50:00.000Z','{"input":"cmd2"}');
                "#,
            )
            .unwrap();
        assert!(!poll1::index_first_col_asc_binary(
            &db.db,
            "idx_turn_items_thread_desc"
        ));
        let now = 1_794_484_800_000.;
        let mut reader = Reader::open(&db.path).unwrap().with_self_check(true);
        let u = reader.poll(now).unwrap();
        assert!(!u.mismatch);
        assert_eq!(u.counters.mismatches, 0);
    }
}
