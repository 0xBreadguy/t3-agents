//! Read-only T3 orchestration projection adapter.
//!
//! This intentionally mirrors oracle/src/source.ts's SQL projection boundary:
//! no authentication tables, prompts, message bodies, results, tool output or
//! reasoning bodies are selected into Rust. The only command text is the same
//! bounded command-execution excerpt the terminal dashboard displays.
//! Estimated asks return a count from the SQLite assistant-text function.
mod asks_hint;
mod assemble;
mod fan;
mod fast_reads;
mod turn_items;

#[cfg(test)]
mod tests;

pub(crate) use asks_hint::read_estimated_asks;
pub(crate) use assemble::{
    assemble, attach_event, attach_history, attach_item, attach_request, attach_span,
    consider_plan, finish_agent_body, finish_agent_tail, link_structure, live_status,
    seal_parents, thread_agent,
};

use asks_hint::read_estimated_asks_inner;
use assemble::assemble_typed;
use fast_reads::{
    HistoryRow, read_history_oracle_typed, read_latest_runs_fast, read_nodes_typed,
    read_projects_typed, read_requests_typed, read_spans_typed, read_turns_and_history_fast,
    table_names,
};
use turn_items::read_turn_items_combined;

use crate::model::{self, Snapshot};
use rusqlite::{
    Connection, OpenFlags, params_from_iter,
    types::{Value as SqlValue, ValueRef},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const PREFIX: &str = "orchestration_v2_projection_";

/// Scale-lab phase timing, off unless `enable`d (`--bench-source` or opt-in reader-poll profiling does).
/// When off, each query pays one relaxed atomic load and no allocation; query
/// text and results never change.
pub mod timing {
    use serde::Serialize;
    use std::{
        collections::HashMap,
        sync::{
            Mutex, OnceLock,
            atomic::{AtomicBool, Ordering},
        },
        time::Instant,
    };
    #[derive(Clone, Copy, Debug, Default, Serialize)]
    pub struct Query {
        pub prepare_ns: u64,
        pub step_ns: u64,
        pub rows: u64,
        pub calls: u64,
    }
    #[derive(Clone, Debug, Default, Serialize)]
    pub struct Data {
        pub queries: HashMap<String, Query>,
        pub json_ns: u64,
        pub build_ns: u64,
        pub decorate_ns: u64,
        /// Inclusive named scopes; nested scopes must not be summed.
        pub phases_ns: HashMap<String, u64>,
    }
    static ON: AtomicBool = AtomicBool::new(false);
    #[cfg(test)]
    thread_local! {
        static TEST_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    static DATA: OnceLock<Mutex<Data>> = OnceLock::new();
    pub fn enable(on: bool) {
        DATA.get_or_init(|| Mutex::new(Data::default()));
        ON.store(on, Ordering::Relaxed);
        #[cfg(test)]
        TEST_ON.set(on);
    }
    /// Tests only: a worker thread times its queries when the thread that started it does.
    #[cfg(test)]
    pub fn inherit(on: bool) {
        TEST_ON.set(on);
    }
    pub fn on() -> bool {
        #[cfg(test)]
        return TEST_ON.get();
        #[cfg(not(test))]
        ON.load(Ordering::Relaxed)
    }
    pub fn reset() {
        if let Some(d) = DATA.get() {
            *d.lock().unwrap() = Data::default();
        }
    }
    pub fn take() -> Data {
        DATA.get()
            .map(|d| d.lock().unwrap().clone())
            .unwrap_or_default()
    }
    /// A disabled scope does not read the clock or allocate.
    pub struct Phase(Option<(&'static str, Instant)>);
    pub fn phase(name: &'static str) -> Phase {
        Phase(on().then(|| (name, Instant::now())))
    }
    impl Drop for Phase {
        fn drop(&mut self) {
            if let Some((name, start)) = self.0 {
                let elapsed = start.elapsed().as_nanos() as u64;
                if let Some(d) = DATA.get() {
                    *d.lock().unwrap().phases_ns.entry(name.into()).or_default() += elapsed;
                }
            }
        }
    }
    /// Short per-query label; the same rules exist in checks/bench/source-baseline.ts.
    fn label(sql: &str) -> String {
        let compact: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        for (needle, name) in [
            ("sqlite_master", "tables"),
            ("pragma_table_info", "table_info"),
            ("-- asks_score", "asks_score"),
            ("projection_threads ", "threads"),
            ("provider_turns", "provider_turns"), // refined below
            ("projection_runs", "runs"),          // refined below
            ("projection_subagents", "subagents"),
            ("projection_provider_threads", "provider_threads"),
            ("projection_nodes", "nodes"),
            ("projection_projects", "projects"),
            ("runtime_requests", "requests"),
            ("projection_messages", "messages"), // refined below
            ("turn_items", "turn_items"),        // refined below
        ] {
            if compact.contains(needle) {
                return match name {
                    "provider_turns" if compact.contains("-- pt_meta") => "turns_meta",
                    "provider_turns" if compact.contains("-- pt_rank1") => "latest_turns",
                    "provider_turns" if compact.contains("-- pt_hist") => "history",
                    "provider_turns" if compact.contains("rank=1") => "latest_turns",
                    "provider_turns" => "history",
                    "runs" if compact.contains("-- runs_idx") => "runs_idx",
                    "runs" if compact.contains("completed_at IS NULL OR") => "spans",
                    "nodes" if compact.contains("-- nodes_sub") => "nodes_subagents",
                    "messages" if compact.contains("-- asks_cutoffs") => "asks_cutoffs",
                    "messages" if compact.contains("-- asks_cands") => "asks_cands",
                    "messages" if compact.contains("-- asks_score") => "asks_score",
                    "turn_items" if compact.contains("-- ti_leaf_scan") => "turn_items_leaf_scan",
                    "turn_items" if compact.contains("-- ti_edge") => "turn_items_edge",
                    "turn_items" if compact.contains("-- ti_fetch") => "turn_items_fetch",
                    "turn_items" if compact.contains("-- ti_asst_phase") => "asks_item_phase",
                    "turn_items" if compact.contains("LIMIT 3000") => "items",
                    "turn_items" if compact.contains("LIMIT 8000") => "events",
                    "turn_items" => "plans",
                    other => other,
                }
                .into();
            }
        }
        compact.chars().take(80).collect()
    }
    pub fn query(sql: &str, prepare: Instant, stepped: Instant, done: Instant, rows: usize) {
        if let Some(d) = DATA.get() {
            let mut d = d.lock().unwrap();
            let q = d.queries.entry(label(sql)).or_default();
            q.prepare_ns += (stepped - prepare).as_nanos() as u64;
            q.step_ns += (done - stepped).as_nanos() as u64;
            q.rows += rows as u64;
            q.calls += 1;
        }
    }
    fn add(field: impl Fn(&mut Data) -> &mut u64, start: Instant) {
        if let Some(d) = DATA.get() {
            let mut d = d.lock().unwrap();
            *field(&mut d) += start.elapsed().as_nanos() as u64;
        }
    }
    pub fn add_json(start: Instant) {
        add(|d| &mut d.json_ns, start)
    }
    pub fn add_build(start: Instant) {
        add(|d| &mut d.build_ns, start)
    }
    pub fn add_decorate(start: Instant) {
        add(|d| &mut d.decorate_ns, start)
    }
}

const TABLES: [&str; 8] = [
    "threads",
    "runs",
    "subagents",
    "nodes",
    "provider_threads",
    "provider_turns",
    "turn_items",
    "runtime_requests",
];
pub(crate) const WINDOW: f64 = 10_800_000.;
type Json = Value;

#[derive(Clone, Debug)]
pub struct SourceError {
    pub kind: &'static str,
    pub path: PathBuf,
    pub detail: String,
}
impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            "missing" => write!(
                f,
                "No T3 database at {}. Is T3 Code installed and opened once?",
                self.path.display()
            ),
            "unreadable" => write!(
                f,
                "Can't read the T3 database at {}: {}",
                self.path.display(),
                self.detail
            ),
            _ => write!(f, "Unsupported T3 schema; {}", self.detail),
        }
    }
}
impl std::error::Error for SourceError {}

fn problem(path: &Path, kind: &'static str, detail: impl Into<String>) -> SourceError {
    SourceError {
        kind,
        path: path.to_owned(),
        detail: detail.into(),
    }
}

fn db_error(path: &Path, error: rusqlite::Error) -> SourceError {
    let not_db = matches!(&error, rusqlite::Error::SqliteFailure(e, _) if e.extended_code == 26);
    problem(
        path,
        if not_db { "unreadable" } else { "schema" },
        if not_db {
            "not a SQLite database".into()
        } else {
            error.to_string()
        },
    )
}

/// An explicit path is required; this function never discovers or opens a real DB by itself.
pub fn snapshot(path: &Path, now: f64) -> Result<Snapshot, SourceError> {
    if !now.is_finite() {
        return Err(problem(path, "schema", "invalid snapshot clock"));
    }
    let debug = debug_timing();
    if debug {
        timing::enable(true);
        timing::reset();
    }
    let started = Instant::now();
    // Several connections on one proven snapshot when that can be had; otherwise, and for
    // every error, the read on one connection.
    let threads = fan::threads();
    let mut trace = fan::Trace::default();
    let fanned = if threads > 1 {
        fan::read(path, now, threads, &mut trace)
    } else {
        None
    };
    let how = debug.then(|| trace.describe(threads, fanned.is_some()));
    let result = match fanned {
        Some(snapshot) => Ok(snapshot),
        None => snapshot_serial(path, now),
    };
    if let Some(how) = how {
        debug_report(started, &how);
    }
    result
}

/// `T3_AGENTS_FULL_READ_DEBUG`: one line of timings and counts on stderr per full read.
/// Query labels and schema names only; never row text.
fn debug_timing() -> bool {
    std::env::var_os("T3_AGENTS_FULL_READ_DEBUG").is_some()
}

fn debug_report(started: Instant, how: &str) {
    let total = started.elapsed().as_secs_f64() * 1000.;
    let data = timing::take();
    let mut queries: Vec<(String, f64, u64)> = data
        .queries
        .into_iter()
        .map(|(label, q)| (label, (q.prepare_ns + q.step_ns) as f64 / 1e6, q.rows))
        .collect();
    queries.sort_by(|a, b| b.1.total_cmp(&a.1));
    let queries: Vec<String> = queries
        .into_iter()
        .map(|(label, ms, rows)| format!("{label}={ms:.1}ms/{rows}"))
        .collect();
    let mut phases: Vec<(String, u64)> = data.phases_ns.into_iter().collect();
    phases.sort_by(|a, b| b.1.cmp(&a.1));
    let phases: Vec<String> = phases
        .into_iter()
        .map(|(name, ns)| format!("{name}={:.1}ms", ns as f64 / 1e6))
        .collect();
    eprintln!(
        "full-read: {how} total={total:.1} phases[{}] queries[{}]",
        phases.join(" "),
        queries.join(" ")
    );
}

fn open_reader(path: &Path) -> Result<Connection, SourceError> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        if !path.exists() {
            problem(path, "missing", "")
        } else if path.is_dir() {
            problem(path, "unreadable", "it is a folder, not a file")
        } else {
            problem(path, "unreadable", error.to_string())
        }
    })?;
    db.busy_timeout(Duration::from_millis(250))
        .map_err(|e| db_error(path, e))?;
    crate::asks::register(&db).map_err(|e| db_error(path, e))?;
    Ok(db)
}

/// The full read on one connection.
fn snapshot_serial(path: &Path, now: f64) -> Result<Snapshot, SourceError> {
    let db = open_reader(path)?;
    db.execute_batch("PRAGMA query_only=ON; BEGIN")
        .map_err(|e| db_error(path, e))?;
    let result = read(&db, path, now);
    // End the read transaction even when schema/JSON checks failed. No commit or migration exists.
    let rollback = db.execute_batch("ROLLBACK").map_err(|e| db_error(path, e));
    match result {
        Ok(value) => {
            rollback?;
            Ok(value)
        }
        Err(error) => {
            let _ = rollback;
            Err(error)
        }
    }
}

pub(crate) fn all(db: &Connection, sql: &str, args: &[SqlValue]) -> rusqlite::Result<Vec<Json>> {
    let timed = timing::on().then(Instant::now);
    let mut stmt = db.prepare_cached(sql)?;
    let stepped = timed.map(|_| Instant::now());
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let rows = stmt.query_map(params_from_iter(args.iter()), |row| {
        let mut value = serde_json::Map::new();
        for (i, name) in names.iter().enumerate() {
            let item = match row.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(v) => json!(v),
                ValueRef::Real(v) => json!(v),
                ValueRef::Text(v) => Value::String(String::from_utf8_lossy(v).into_owned()),
                ValueRef::Blob(_) => Value::Null,
            };
            value.insert(name.clone(), item);
        }
        Ok(Value::Object(value))
    })?;
    let out: Vec<Json> = rows.collect::<Result<_, _>>()?;
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        timing::query(sql, prepare, stepped, Instant::now(), out.len());
    }
    Ok(out)
}

/// Static, narrow field projection. Full payload_json is never returned by a query.
pub(crate) fn projection(table: &str) -> String {
    let (flat, nested): (&[&str], &[(&str, &[&str])]) = match table {
        "threads" => (
            &[
                "id",
                "projectId",
                "title",
                "activeProviderThreadId",
                "worktreePath",
                "branch",
                "updatedAt",
                "settledOverride",
                "snoozedUntil",
            ],
            &[
                ("delegatedFrom", &["environmentId", "threadId", "title"]),
                (
                    "lineage",
                    &["relationshipToParent", "parentThreadId", "sideChat"],
                ),
                ("modelSelection", &["model", "options"]),
            ],
        ),
        "runs" => (
            &[
                "id",
                "threadId",
                "status",
                "providerThreadId",
                "startedAt",
                "requestedAt",
                "completedAt",
            ],
            &[("modelSelection", &["model", "options"])],
        ),
        "subagents" => (
            &[
                "id",
                "threadId",
                "parentNodeId",
                "runId",
                "providerThreadId",
                "childThreadId",
                "status",
                "title",
                "model",
                "startedAt",
                "completedAt",
                "updatedAt",
            ],
            &[("remoteChild", &["environmentId", "threadId", "label"])],
        ),
        "provider_threads" => (&["id", "contextUsage"], &[]),
        "provider_turns" => (
            &["providerThreadId", "model", "tokenUsage", "turnTokenUsage"],
            &[],
        ),
        _ => unreachable!("only static projection tables"),
    };
    let mut parts: Vec<String> = flat
        .iter()
        .map(|k| format!("'{k}', json_extract(payload_json,'$.{k}')"))
        .collect();
    for (key, fields) in nested {
        let inner = fields
            .iter()
            .map(|field| format!("'{field}', json_extract(payload_json,'$.{key}.{field}')"))
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("'{key}', CASE WHEN json_type(payload_json,'$.{key}')='object' THEN json_object({inner}) END"));
    }
    format!("json_object({})", parts.join(","))
}

fn parse_json_ref(value: ValueRef<'_>) -> rusqlite::Result<Json> {
    let text: std::borrow::Cow<'_, str> = match value {
        ValueRef::Text(bytes) => String::from_utf8_lossy(bytes),
        _ => std::borrow::Cow::Borrowed(""),
    };
    serde_json::from_str(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn payloads(
    db: &Connection,
    table: &str,
    clause: &str,
    args: &[SqlValue],
) -> rusqlite::Result<Vec<Json>> {
    let sql = format!(
        "SELECT {} AS payload FROM {PREFIX}{table} {clause}",
        projection(table)
    );
    let timed = timing::on().then(Instant::now);
    let mut stmt = db.prepare_cached(&sql)?;
    let stepped = timed.map(|_| Instant::now());
    let mut rows = stmt.query(params_from_iter(args.iter()))?;
    let mut out = Vec::new();
    let mut json_dur = Duration::ZERO;
    while let Some(row) = rows.next()? {
        let j0 = timed.map(|_| Instant::now());
        out.push(parse_json_ref(row.get_ref(0)?)?);
        if let Some(t) = j0 {
            json_dur += t.elapsed();
        }
    }
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        let now = Instant::now();
        timing::query(
            &sql,
            prepare,
            stepped,
            now.checked_sub(json_dur).unwrap_or(stepped),
            out.len(),
        );
        if let Some(t) = now.checked_sub(json_dur) {
            timing::add_json(t);
        }
    }
    Ok(out)
}

pub(crate) fn is_toolish(kind: &str) -> bool {
    matches!(
        kind,
        "command_execution"
            | "dynamic_tool"
            | "file_change"
            | "web_search"
            | "subagent"
            | "handoff"
            | "notification"
            | "user_input_request"
            | "secret_request"
            | "link_request"
            | "reasoning"
            | "assistant_message"
    )
}
pub(crate) fn is_event_kind(kind: &str) -> bool {
    !matches!(kind, "checkpoint" | "todo_list")
}
pub(crate) fn str_at<'a>(row: &'a Json, key: &str) -> &'a str {
    row.get(key).and_then(Value::as_str).unwrap_or("")
}
pub(crate) fn optional(row: &Json, key: &str) -> Option<String> {
    row.get(key).and_then(Value::as_str).map(Into::into)
}
pub(crate) fn truthy_str(row: &Json, key: &str) -> Option<String> {
    optional(row, key).filter(|s| !s.is_empty())
}
pub(crate) fn obj(row: &Json, key: &str) -> Json {
    row.get(key)
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}))
}
pub(crate) fn number(row: &Json, key: &str) -> f64 {
    match row.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            if s.trim().is_empty() {
                0.
            } else {
                s.trim().parse().unwrap_or(f64::NAN)
            }
        }
        Some(Value::Bool(b)) => {
            if *b {
                1.
            } else {
                0.
            }
        }
        None | Some(Value::Null) => 0.,
        _ => f64::NAN,
    }
}
fn marks(n: usize) -> String {
    if n == 0 {
        "NULL".into()
    } else {
        vec!["?"; n].join(",")
    }
}
fn strings(values: impl IntoIterator<Item = String>) -> Vec<SqlValue> {
    values.into_iter().map(SqlValue::Text).collect()
}
pub(crate) fn iso(stamp: f64) -> String {
    let millis = stamp.floor() as i64;
    let seconds = millis.div_euclid(1000);
    let days = seconds.div_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let dayseconds = seconds.rem_euclid(86400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        dayseconds / 3600,
        (dayseconds % 3600) / 60,
        dayseconds % 60,
        millis.rem_euclid(1000)
    )
}

// The dashboard's exit-code model is i32. Reject other item kinds and malformed
// values in SQL so both readers receive only a command exit code or NULL.
const ITEM_EXIT_CODE: &str = "CASE WHEN type='command_execution'
 AND json_type(payload_json,'$.exitCode')='integer'
 AND json_extract(payload_json,'$.exitCode') BETWEEN -2147483648 AND 2147483647
 THEN json_extract(payload_json,'$.exitCode') END AS exit_code";

pub(crate) fn item_keys() -> String {
    format!("thread_id,run_id,node_id,provider_thread_id,type,status,updated_at,
 json_extract(payload_json,'$.startedAt') AS started_at,
 CASE WHEN type NOT IN ('secret_request','link_request') THEN json_extract(payload_json,'$.toolName') END AS tool,
 {ITEM_EXIT_CODE}")
}

pub(crate) fn item_fields() -> String {
    format!("{},
 CASE WHEN type IN ('secret_request','link_request') THEN ''
 ELSE substr(COALESCE(json_extract(payload_json,'$.title'),json_extract(payload_json,'$.toolName'),json_extract(payload_json,'$.label'),''),1,240) END AS label,
 CASE WHEN type='command_execution' THEN substr(json_extract(payload_json,'$.input'),1,240) ELSE '' END AS command,
 CASE WHEN type='file_change' THEN substr(json_extract(payload_json,'$.fileName'),1,240) ELSE '' END AS file", item_keys())
}

pub(crate) const TOOLISH: &str = "'command_execution','dynamic_tool','file_change','web_search','subagent','handoff','notification','user_input_request','secret_request','link_request','reasoning','assistant_message'";

pub(crate) fn read(db: &Connection, path: &Path, now: f64) -> Result<Snapshot, SourceError> {
    read_tracked(db, path, now, &mut ReadReport::default())
}

#[derive(Default)]
pub(crate) struct ReadReport {
    pub rows: usize,
    pub threads: usize,
    pub valid_until: f64,
}

pub(crate) fn read_tracked(
    db: &Connection,
    path: &Path,
    now: f64,
    report: &mut ReadReport,
) -> Result<Snapshot, SourceError> {
    let rows = std::cell::Cell::new(0usize);
    report.valid_until = f64::INFINITY;
    rows.set(rows.get() + check_tables(db, path)?);
    let load = |table: &str, clause: &str, args: &[SqlValue]| {
        let result = payloads(db, table, clause, args).map_err(|e| db_error(path, e))?;
        rows.set(rows.get() + result.len());
        Ok::<_, SourceError>(result)
    };
    let threads = load(
        "threads",
        "WHERE deleted_at IS NULL AND archived_at IS NULL",
        &[],
    )?;
    let ids: Vec<String> = threads.iter().map(|r| str_at(r, "id").into()).collect();
    report.threads = ids.len();
    report.rows = rows.get();
    if ids.is_empty() {
        return Ok(empty(path, now));
    }
    let active_set: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let params = strings(ids.clone());
    let where_ids = format!("WHERE thread_id IN ({})", marks(ids.len()));

    let (latest, latest_order, run_rows) = read_latest_runs(db, path, &where_ids, &params)?;
    rows.set(rows.get() + run_rows);

    let subs = load("subagents", &where_ids, &params)?;
    let providers: HashMap<String, Json> = load("provider_threads", &where_ids, &params)?
        .into_iter()
        .map(|p| (str_at(&p, "id").into(), p))
        .collect();

    let (turns, history, turn_rows) = read_turns_and_history(db, path, &where_ids, &params)?;
    rows.set(rows.get() + turn_rows);

    let read_nodes = timing::phase("read_nodes");
    let (nodes, node_count) = read_nodes_typed(db, &where_ids, &params, &active_set, &subs, path)?;
    rows.set(rows.get() + node_count);
    drop(read_nodes);

    let (projects, proj_count) = read_projects_typed(db).map_err(|e| db_error(path, e))?;
    rows.set(rows.get() + proj_count);

    let request_fields = request_fields(db).map_err(|e| db_error(path, e))?;
    let requests = read_requests_typed(db, &where_ids, &request_fields, &params)
        .map_err(|e| db_error(path, e))?;
    rows.set(rows.get() + requests.len());

    let (run_ids, runless) = run_scope(&ids, &latest, &latest_order, &subs);
    let since = iso(now - WINDOW);

    // Single-pass leaf-only turn_items scan + forward-sorted rowid payload fetch
    // for items, events, plans, and assistant_message node index.
    let (items, events, plans, asst_index, ti_rows) = read_turn_items_combined(
        db, path, &where_ids, &params, &ids, &run_ids, &runless, &since,
    )?;
    rows.set(rows.get() + ti_rows);

    let mut since_params = params;
    since_params.push(SqlValue::Text(since));
    let spans = read_spans_typed(db, &where_ids, &since_params).map_err(|e| db_error(path, e))?;
    rows.set(rows.get() + spans.len());

    report.valid_until = deadline_stamps(
        events
            .iter()
            .filter_map(|e| e.updated_at.as_deref())
            .chain(spans.iter().filter_map(|s| s.completed_at.as_deref())),
        now,
    );

    let cards = read_ask_cards(db, None).map_err(|e| db_error(path, e))?;
    let estimates =
        read_estimated_asks_inner(db, None, asst_index.as_ref()).map_err(|e| db_error(path, e))?;
    report.rows = rows.get() + cards.len() + estimates.len();
    let mut snapshot = assemble_typed(
        threads, latest, subs, providers, turns, &nodes, &projects, &requests, &items, &events,
        &history, &spans, &plans, path, now,
    )?;
    apply_thread_asks(&mut snapshot, &cards, &estimates, None);
    Ok(snapshot)
}

/// The tables the read needs are all there; how many tables the database has.
fn check_tables(db: &Connection, path: &Path) -> Result<usize, SourceError> {
    let existing = table_names(db).map_err(|e| db_error(path, e))?;
    let mut missing: Vec<_> = TABLES
        .iter()
        .map(|t| format!("{PREFIX}{t}"))
        .filter(|t| !existing.contains(t))
        .collect();
    if !missing.is_empty() {
        missing.sort();
        return Err(problem(
            path,
            "schema",
            format!("missing {}", missing.join(", ")),
        ));
    }
    Ok(existing.len())
}

/// No active thread.
fn empty(path: &Path, now: f64) -> Snapshot {
    Snapshot {
        agents: Vec::new(),
        at: now,
        path: path.to_string_lossy().into(),
    }
}

/// The latest run of each thread, and the threads in the reference's `ORDER BY ordinal DESC`
/// order: run payloads validated, their keys ranked, then the winning rows fetched.
fn read_latest_runs(
    db: &Connection,
    path: &Path,
    where_ids: &str,
    params: &[SqlValue],
) -> Result<(HashMap<String, Json>, Vec<String>, usize), SourceError> {
    if let Ok(Some(found)) = read_latest_runs_fast(db, where_ids, params) {
        return Ok(found);
    }
    let runs = payloads(
        db,
        "runs",
        &format!("{where_ids} ORDER BY ordinal DESC"),
        params,
    )
    .map_err(|e| db_error(path, e))?;
    let count = runs.len();
    let mut latest = HashMap::new();
    let mut latest_order = Vec::new();
    for run in runs {
        let id = str_at(&run, "threadId").to_string();
        if !latest.contains_key(&id) {
            latest_order.push(id.clone());
            latest.insert(id, run);
        }
    }
    Ok((latest, latest_order, count))
}

/// Combined provider_turns pass for both latest_turns (rank=1) and history (rank<=160).
fn read_turns_and_history(
    db: &Connection,
    path: &Path,
    where_ids: &str,
    params: &[SqlValue],
) -> Result<(HashMap<String, Json>, Vec<HistoryRow>, usize), SourceError> {
    if let Ok(Some(found)) = read_turns_and_history_fast(db, where_ids, params, path) {
        return Ok(found);
    }
    let mut count = 0;
    let mut turns = HashMap::new();
    {
        let turn_rows = all(
            db,
            &format!(
                "SELECT {} AS payload_json FROM (SELECT payload_json,ROW_NUMBER() OVER(PARTITION BY provider_thread_id ORDER BY ordinal DESC,rowid DESC) AS rank FROM {PREFIX}provider_turns {where_ids}) WHERE rank=1",
                projection("provider_turns")
            ),
            params,
        )
        .map_err(|e| db_error(path, e))?;
        count += turn_rows.len();
        let timed = timing::on().then(Instant::now);
        for row in turn_rows {
            let p: Json = serde_json::from_str(str_at(&row, "payload_json"))
                .map_err(|e| problem(path, "schema", e.to_string()))?;
            turns.insert(str_at(&p, "providerThreadId").to_string(), p);
        }
        if let Some(t) = timed {
            timing::add_json(t);
        }
    }
    let hist_rows =
        read_history_oracle_typed(db, where_ids, params).map_err(|e| db_error(path, e))?;
    count += hist_rows.len();
    Ok((turns, hist_rows, count))
}

/// The runs whose items are read (each thread's latest, then the subagents'), and the
/// threads that have no run.
fn run_scope(
    ids: &[String],
    latest: &HashMap<String, Json>,
    latest_order: &[String],
    subs: &[Json],
) -> (Vec<String>, Vec<String>) {
    let mut seen = HashSet::new();
    let mut run_ids = Vec::new();
    for id in latest_order {
        if let Some(run) = latest.get(id) {
            if let Some(id) = truthy_str(run, "id") {
                if seen.insert(id.clone()) {
                    run_ids.push(id);
                }
            }
        }
    }
    for sub in subs {
        if let Some(id) = truthy_str(sub, "runId") {
            if seen.insert(id.clone()) {
                run_ids.push(id);
            }
        }
    }
    let runless: Vec<String> = ids
        .iter()
        .filter(|id| !latest.contains_key(*id))
        .cloned()
        .collect();
    (run_ids, runless)
}

// T3 runtime requests use pending; retain the legacy aliases for approvals and
// other request kinds. Only the capability discriminator crosses the SQL boundary.
pub(crate) const REQUEST_FILTER: &str = "resolved_at IS NULL AND
    status IN ('pending','open','waiting','requested') AND (kind<>'user_input' OR status='pending')";

fn columns(db: &Connection, table: &str) -> rusqlite::Result<HashSet<String>> {
    db.prepare_cached("SELECT name FROM pragma_table_info(?1)")?
        .query_map([table], |row| row.get(0))?
        .collect()
}

pub(crate) fn request_fields(db: &Connection) -> rusqlite::Result<String> {
    let fields = columns(db, &format!("{PREFIX}runtime_requests"))?;
    let created_at = if fields.contains("created_at") {
        "created_at"
    } else {
        "NULL"
    };
    let capability = if fields.contains("payload_json") {
        "CASE WHEN kind='user_input' THEN
             CASE WHEN json_valid(payload_json)
                  THEN json_extract(payload_json,'$.responseCapability.type') IS 'message'
                  ELSE 0 END
         ELSE 0 END"
    } else {
        "0"
    };
    Ok(format!(
        "node_id,kind,status,{created_at} AS created_at,resolved_at,{capability} AS async_input"
    ))
}

/// Thread-owned card counts, independent of latest-run selection and item caps.
/// ProjectionStore.ts's shell uses these four live statuses and compares
/// human message updated_at to the card's startedAt (or updated_at).
pub(crate) fn read_ask_cards(
    db: &Connection,
    owners: Option<&[String]>,
) -> rusqlite::Result<HashMap<String, model::AskCards>> {
    let (owner_filter, args) = match owners {
        Some([]) => return Ok(HashMap::new()),
        Some(ids) => (
            format!("AND thread_id IN ({})", marks(ids.len())),
            strings(ids.to_vec()),
        ),
        None => (String::new(), Vec::new()),
    };
    let runs = columns(db, &format!("{PREFIX}runs"))?;
    let messages = columns(db, &format!("{PREFIX}messages"))?;
    let run_status = if runs.contains("status") {
        "r.status"
    } else {
        "CASE WHEN json_valid(r.payload_json) THEN json_extract(r.payload_json,'$.status') END"
    };
    // Run payload id is already in the allowlist. Use it only as the join key.
    let secret = format!(
        "SELECT i.thread_id, 'secret' AS kind
        FROM {PREFIX}runs r CROSS JOIN {PREFIX}turn_items i
        WHERE r.thread_id IN (SELECT thread_id FROM owners)
          AND {run_status} IN ('preparing','starting','running','waiting')
          AND i.thread_id=r.thread_id AND i.run_id=json_extract(r.payload_json,'$.id')
          AND i.type='secret_request' AND i.status='waiting' AND i.node_id IS NOT NULL"
    );
    // Older/synthetic schemas may lack message provenance. In that case links
    // clear on an explicit state transition, with no inferred human reply.
    let human_reply = if ["thread_id", "role", "updated_at", "payload_json"]
        .iter()
        .all(|name| messages.contains(*name))
    {
        format!(
            "AND NOT EXISTS (
            SELECT 1 FROM {PREFIX}messages m
            WHERE m.thread_id=i.thread_id AND m.role='user'
              AND m.updated_at > COALESCE(
                  CASE WHEN json_valid(i.payload_json)
                       THEN json_extract(i.payload_json,'$.startedAt') END, i.updated_at)
              AND CASE WHEN json_valid(m.payload_json) THEN
                  json_extract(m.payload_json,'$.createdBy')='user'
                  AND json_extract(m.payload_json,'$.idleCompaction') IS NULL END
        )"
        )
    } else {
        String::new()
    };
    let sql = format!(
        "WITH owners AS (
            SELECT thread_id FROM {PREFIX}threads
            WHERE deleted_at IS NULL AND archived_at IS NULL {owner_filter}
        ), cards AS (
            {secret}
            UNION ALL
            SELECT i.thread_id, 'link' AS kind FROM {PREFIX}turn_items i
            WHERE i.thread_id IN (SELECT thread_id FROM owners)
              AND i.type='link_request' AND i.status='waiting' AND i.node_id IS NOT NULL
              {human_reply}
        )
        SELECT thread_id, COUNT(*) FILTER (WHERE kind='secret'),
               COUNT(*) FILTER (WHERE kind='link') FROM cards GROUP BY thread_id"
    );
    db.prepare_cached(&sql)?
        .query_map(params_from_iter(&args), |row| {
            Ok((
                row.get(0)?,
                model::AskCards {
                    secret: row.get::<_, i64>(1)? as u64,
                    link: row.get::<_, i64>(2)? as u64,
                },
            ))
        })?
        .collect()
}

pub(crate) fn apply_thread_asks(
    snapshot: &mut Snapshot,
    cards: &HashMap<String, model::AskCards>,
    estimates: &HashMap<String, u64>,
    unchanged: Option<(&Snapshot, &std::collections::BTreeSet<String>)>,
) {
    let previous = unchanged.map(|(snapshot, _)| {
        snapshot
            .agents
            .iter()
            .filter(|a| a.open_asks.cards != model::AskCards::default() || a.estimated_asks > 0)
            .map(|a| (a.id.as_str(), (a.open_asks.cards, a.estimated_asks)))
            .collect::<HashMap<_, _>>()
    });
    for agent in &mut snapshot.agents {
        // The projection belongs to the thread shell even when its source
        // rows are outside the item window or the shell has a parent.
        if agent.id != agent.thread_id {
            continue;
        }
        let id = agent.id.as_str();
        let (card_counts, estimate) = if unchanged.is_some_and(|(_, owners)| !owners.contains(id)) {
            previous
                .as_ref()
                .and_then(|rows| rows.get(id))
                .copied()
                .unwrap_or_default()
        } else {
            (
                cards.get(id).copied().unwrap_or_default(),
                estimates.get(id).copied().unwrap_or_default(),
            )
        };
        if agent.open_asks.cards != card_counts || agent.estimated_asks != estimate {
            let agent = std::sync::Arc::make_mut(agent);
            agent.open_asks.cards = card_counts;
            agent.estimated_asks = estimate;
        }
    }
}

/// One SQL value as the oracle's row maps hold it.
fn value_json(value: ValueRef<'_>) -> Json {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(v) => json!(v),
        ValueRef::Real(v) => json!(v),
        ValueRef::Text(v) => Value::String(String::from_utf8_lossy(v).into_owned()),
        ValueRef::Blob(_) => Value::Null,
    }
}

/// Uncached statement for SQL whose text changes with its arity, timed under
/// the same labels as [`all`].
fn each_row(
    db: &Connection,
    sql: &str,
    args: &[SqlValue],
    mut f: impl FnMut(&[String], &rusqlite::Row<'_>) -> rusqlite::Result<()>,
) -> rusqlite::Result<usize> {
    let timed = timing::on().then(Instant::now);
    let mut stmt = db.prepare(sql)?;
    let stepped = timed.map(|_| Instant::now());
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query(params_from_iter(args.iter()))?;
    let mut n = 0;
    while let Some(row) = rows.next()? {
        f(&names, row)?;
        n += 1;
    }
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        timing::query(sql, prepare, stepped, Instant::now(), n);
    }
    Ok(n)
}

fn each_row_cached(
    db: &Connection,
    sql: &str,
    args: &[SqlValue],
    mut f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<()>,
) -> rusqlite::Result<usize> {
    let timed = timing::on().then(Instant::now);
    let mut stmt = db.prepare_cached(sql)?;
    let stepped = timed.map(|_| Instant::now());
    let mut rows = stmt.query(params_from_iter(args.iter()))?;
    let mut n = 0;
    while let Some(row) = rows.next()? {
        f(row)?;
        n += 1;
    }
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        timing::query(sql, prepare, stepped, Instant::now(), n);
    }
    Ok(n)
}

fn each_row_uncached(
    db: &Connection,
    sql: &str,
    args: &[SqlValue],
    mut f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<()>,
) -> rusqlite::Result<usize> {
    let timed = timing::on().then(Instant::now);
    let mut stmt = db.prepare(sql)?;
    let stepped = timed.map(|_| Instant::now());
    let mut rows = stmt.query(params_from_iter(args.iter()))?;
    let mut n = 0;
    while let Some(row) = rows.next()? {
        f(row)?;
        n += 1;
    }
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        timing::query(sql, prepare, stepped, Instant::now(), n);
    }
    Ok(n)
}

/// `EXPLAIN QUERY PLAN` as a tree: each step's detail and its parent's position.
fn plan(
    db: &Connection,
    sql: &str,
    args: &[SqlValue],
) -> rusqlite::Result<Vec<(Option<usize>, String)>> {
    let mut ids = Vec::new();
    let mut steps = Vec::new();
    each_row(db, &format!("EXPLAIN QUERY PLAN {sql}"), args, |_, row| {
        let id: i64 = row.get(0)?;
        let parent: i64 = row.get(1)?;
        let detail: String = row.get(3)?;
        steps.push((ids.iter().rposition(|i| *i == parent), detail));
        ids.push(id);
        Ok(())
    })?;
    Ok(steps)
}

/// T3's partial index over the turn items that are not tool calls. Its key is
/// `(thread_id, ordinal, type, run_id)`; the WHERE below is its definition,
/// which a query has to repeat for SQLite to use it.
const CONVERSATION: &str = "type NOT IN ( 'command_execution', 'reasoning', 'file_change', 'file_search', 'web_search', 'dynamic_tool' )";

/// Project a database constructed entirely from the authored demo scenario.
/// Kept crate-private so live callers still require the read-only path API.
pub(crate) fn synthetic_snapshot(db: &Connection, now: f64) -> Result<Snapshot, SourceError> {
    let path = Path::new("synthetic:demo");
    crate::asks::register(db).map_err(|e| db_error(path, e))?;
    db.execute_batch("PRAGMA query_only=ON; BEGIN")
        .map_err(|e| db_error(path, e))?;
    let result = read(db, path, now);
    let rollback = db.execute_batch("ROLLBACK").map_err(|e| db_error(path, e));
    match result {
        Ok(value) => {
            rollback?;
            Ok(value)
        }
        Err(error) => {
            let _ = rollback;
            Err(error)
        }
    }
}

pub(crate) fn deadline_stamps<'a>(stamps: impl Iterator<Item = &'a str>, now: f64) -> f64 {
    let mut valid_until = f64::INFINITY;
    for text in stamps {
        let deadline = model::ms(Some(text))
            .filter(|stamp| iso(*stamp) == text && text.len() == 24)
            .map(|stamp| stamp + WINDOW + 1.)
            .unwrap_or(now);
        valid_until = valid_until.min(deadline);
    }
    if iso(now - WINDOW).len() != 24 {
        valid_until = now;
    }
    valid_until
}
