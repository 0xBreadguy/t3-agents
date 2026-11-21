//! Typed fast-path table readers (runs, provider_turns, nodes, projects,
//! runtime_requests, spans, and indexed todo-list lookup) plus lightweight
//! rowid/byte hashers shared across snapshot reading.
use super::{
    CONVERSATION, Json, PREFIX, SourceError, db_error, each_row, each_row_cached,
    each_row_uncached, marks, parse_json_ref, plan, problem, projection, str_at, strings, timing,
    value_json,
};
use crate::model::Item;
use rusqlite::{
    Connection, params_from_iter,
    types::{Value as SqlValue, ValueRef},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    time::{Duration, Instant},
};

/// Row ids are already well spread; skip SipHash for internal rowid joins.
#[derive(Default)]
pub(super) struct RowidHasher(u64);
impl std::hash::Hasher for RowidHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn write_i64(&mut self, value: i64) {
        self.0 = (value as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

/// Fast non-cryptographic FNV-1a hasher for internal ID byte-slice maps.
#[derive(Default)]
pub(super) struct FnvHasher(u64);
impl std::hash::Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut h = if self.0 == 0 {
            0xcbf2_9ce4_8422_2325
        } else {
            self.0
        };
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 = h;
    }
}

pub(super) type NodeMap = HashMap<String, String>;

pub(super) struct ProjectRow {
    pub(super) title: Option<String>,
    pub(super) workspace_root: Option<String>,
}

pub(super) struct RequestRow {
    pub(super) thread_id: String,
    pub(super) node_id: Option<String>,
    pub(super) kind: String,
    pub(super) created_at: Option<String>,
    pub(super) async_input: bool,
}

pub(super) struct HistoryRow {
    pub(super) provider_thread_id: String,
    pub(super) thread_id: String,
    pub(super) started_at: Option<String>,
    pub(super) completed_at: Option<String>,
    pub(super) used: f64,
    pub(super) max: f64,
    pub(super) model: Option<String>,
}

pub(super) struct SpanRow {
    pub(super) thread_id: String,
    pub(super) requested_at: Option<String>,
    pub(super) completed_at: Option<String>,
    pub(super) started_at: Option<String>,
}

pub(super) struct PlanRow {
    pub(super) thread_id: String,
    pub(super) run_id: Option<String>,
    pub(super) node_id: Option<String>,
    pub(super) provider_thread_id: Option<String>,
    pub(super) steps: Option<String>,
}

#[derive(Clone)]
pub(super) struct FetchedTurnItem {
    pub(super) thread_id: Option<String>,
    pub(super) run_id: Option<String>,
    pub(super) node_id: Option<String>,
    pub(super) provider_thread_id: Option<String>,
    pub(super) kind: Option<String>,
    pub(super) status: Option<String>,
    pub(super) updated_at: Option<String>,
    pub(super) started_at: Option<String>,
    pub(super) tool: Option<String>,
    pub(super) exit_code: Option<i32>,
    pub(super) label: Option<String>,
    pub(super) command: String,
    pub(super) file: String,
    pub(super) steps: Option<String>,
    item_err: Option<&'static str>,
}

impl FetchedTurnItem {
    pub(super) fn from_row(
        row: &rusqlite::Row<'_>,
        col_offset: usize,
        with_steps: bool,
    ) -> rusqlite::Result<Self> {
        let item_err: std::cell::Cell<Option<&'static str>> = std::cell::Cell::new(None);
        let set_err = |err: &'static str| {
            if item_err.get().is_none() {
                item_err.set(Some(err));
            }
        };
        let req_str = |idx: usize, err: &'static str| -> rusqlite::Result<Option<String>> {
            match row.get_ref(idx)? {
                ValueRef::Text(b) => Ok(Some(String::from_utf8_lossy(b).into_owned())),
                _ => {
                    set_err(err);
                    Ok(None)
                }
            }
        };
        let opt_str = |idx: usize, err: &'static str| -> rusqlite::Result<Option<String>> {
            match row.get_ref(idx)? {
                ValueRef::Null | ValueRef::Blob(_) => Ok(None),
                ValueRef::Text(b) => Ok(Some(String::from_utf8_lossy(b).into_owned())),
                _ => {
                    set_err(err);
                    Ok(None)
                }
            }
        };
        let null_empty_str = |idx: usize, err: &'static str| -> rusqlite::Result<String> {
            match row.get_ref(idx)? {
                ValueRef::Null | ValueRef::Blob(_) => Ok(String::new()),
                ValueRef::Text(b) => Ok(String::from_utf8_lossy(b).into_owned()),
                _ => {
                    set_err(err);
                    Ok(String::new())
                }
            }
        };
        let thread_id = req_str(col_offset, "invalid thread_id")?;
        let run_id = opt_str(col_offset + 1, "invalid run_id")?;
        let node_id = opt_str(col_offset + 2, "invalid node_id")?;
        let provider_thread_id = opt_str(col_offset + 3, "invalid provider_thread_id")?;
        let kind = req_str(col_offset + 4, "invalid type")?;
        let status = req_str(col_offset + 5, "invalid status")?;
        let updated_at = req_str(col_offset + 6, "invalid updated_at")?;
        let started_at = opt_str(col_offset + 7, "invalid started_at")?;
        let tool = opt_str(col_offset + 8, "invalid tool")?;
        let exit_code = match row.get_ref(col_offset + 9)? {
            ValueRef::Null | ValueRef::Blob(_) => None,
            ValueRef::Integer(v) => match i32::try_from(v) {
                Ok(n) => Some(n),
                Err(_) => {
                    set_err("exit_code out of range");
                    None
                }
            },
            _ => {
                set_err("invalid exit_code");
                None
            }
        };
        let label = req_str(col_offset + 10, "invalid label")?;
        let command = null_empty_str(col_offset + 11, "invalid command")?;
        let file = null_empty_str(col_offset + 12, "invalid file")?;
        let steps = if with_steps {
            sql_opt_text(row.get_ref(col_offset + 13)?)
        } else {
            None
        };
        Ok(Self {
            thread_id,
            run_id,
            node_id,
            provider_thread_id,
            kind,
            status,
            updated_at,
            started_at,
            tool,
            exit_code,
            label,
            command,
            file,
            steps,
            item_err: item_err.get(),
        })
    }

    pub(super) fn to_item(&self, path: &Path) -> Result<Item, SourceError> {
        if let Some(err) = self.item_err {
            return Err(problem(path, "schema", err));
        }
        Ok(Item {
            thread_id: self.thread_id.clone().unwrap_or_default(),
            run_id: self.run_id.clone(),
            node_id: self.node_id.clone(),
            provider_thread_id: self.provider_thread_id.clone(),
            kind: self.kind.clone().unwrap_or_default(),
            status: self.status.clone().unwrap_or_default(),
            updated_at: self.updated_at.clone().unwrap_or_default(),
            started_at: self.started_at.clone(),
            label: self.label.clone().unwrap_or_default(),
            command: self.command.clone(),
            file: self.file.clone(),
            tool: self.tool.clone(),
            exit_code: self.exit_code,
        })
    }
}

pub(super) fn sql_opt_text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Text(b) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

pub(super) fn sql_number(value: ValueRef<'_>) -> f64 {
    match value {
        ValueRef::Integer(v) => v as f64,
        ValueRef::Real(v) => v,
        ValueRef::Text(b) => {
            let s = String::from_utf8_lossy(b);
            let t = s.trim();
            if t.is_empty() {
                0.
            } else {
                t.parse().unwrap_or(f64::NAN)
            }
        }
        ValueRef::Null | ValueRef::Blob(_) => 0.,
    }
}

pub(super) fn table_names(db: &Connection) -> rusqlite::Result<HashSet<String>> {
    let mut out = HashSet::new();
    each_row_cached(
        db,
        "SELECT name FROM sqlite_master WHERE type='table'",
        &[],
        |row| {
            if let Some(name) = sql_opt_text(row.get_ref(0)?) {
                out.insert(name);
            }
            Ok(())
        },
    )?;
    Ok(out)
}

pub(super) fn ordered_columns(db: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    db.prepare_cached("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?
        .query_map([table], |row| row.get(0))?
        .collect()
}

pub(super) fn is_rowid_alias(name: &str) -> bool {
    name.eq_ignore_ascii_case("rowid")
        || name.eq_ignore_ascii_case("_rowid_")
        || name.eq_ignore_ascii_case("oid")
}

pub(super) fn rowid_unshadowed(db: &Connection, table: &str) -> rusqlite::Result<bool> {
    let cols = ordered_columns(db, table)?;
    Ok(!cols.is_empty() && !cols.iter().any(|c| is_rowid_alias(c)))
}

/// Proves via EXPLAIN QUERY PLAN + EXPLAIN bytecode that `table(col)` has a
/// non-partial single-column UNIQUE / PRIMARY KEY covering index (so at most
/// one row can match `col=?` without any loop). Uses only `table.col`.
fn single_column_unique(db: &Connection, table: &str, col: &str) -> rusqlite::Result<bool> {
    let probe = format!("SELECT {col} FROM {table} WHERE {col}=?");
    let arg = [SqlValue::Text(String::new())];
    let steps = plan(db, &probe, &arg)?;
    let needle = format!("({col}=?)");
    if steps.len() != 1
        || !steps[0].1.starts_with("SEARCH ")
        || !steps[0].1.contains("USING COVERING INDEX ")
        || !steps[0].1.contains(&needle)
    {
        return Ok(false);
    }
    let mut has_loop = false;
    let mut stmt = db.prepare(&format!("EXPLAIN {probe}"))?;
    let mut rows = stmt.query(params_from_iter(arg.iter()))?;
    while let Some(row) = rows.next()? {
        if let ValueRef::Text(op) = row.get_ref(1)? {
            if op == b"Next" || op == b"Prev" {
                has_loop = true;
                break;
            }
        }
    }
    Ok(!has_loop)
}

pub(super) fn read_latest_runs_fast(
    db: &Connection,
    where_ids: &str,
    params: &[SqlValue],
) -> rusqlite::Result<Option<(HashMap<String, Json>, Vec<String>, usize)>> {
    let table = format!("{PREFIX}runs");
    if !rowid_unshadowed(db, &table)? {
        return Ok(None);
    }
    let mut best: HashMap<String, (Option<i64>, i64, bool)> = HashMap::new();
    let mut usable = true;
    // The reference projects every matching run before choosing the latest.
    // Preserve its validation of older runs, including JSON5 and legacy BLOB
    // inputs accepted by SQLite. Let the reference report any errors.
    let sql = format!(
        "SELECT rowid, thread_id, ordinal, json_error_position(payload_json)
         FROM {table} {where_ids} -- runs_idx"
    );
    each_row_cached(db, &sql, params, |row| {
        if !matches!(row.get_ref(3)?, ValueRef::Integer(0) | ValueRef::Null) {
            usable = false;
            return Ok(());
        }
        let rid = match row.get_ref(0)? {
            ValueRef::Integer(id) => id,
            _ => {
                usable = false;
                return Ok(());
            }
        };
        let tid = match row.get_ref(1)? {
            ValueRef::Text(b) => match std::str::from_utf8(b) {
                Ok(s) => s,
                Err(_) => {
                    usable = false;
                    return Ok(());
                }
            },
            _ => {
                usable = false;
                return Ok(());
            }
        };
        let ord = match row.get_ref(2)? {
            ValueRef::Integer(v) => Some(v),
            ValueRef::Null => None,
            _ => {
                usable = false;
                return Ok(());
            }
        };
        if let Some(entry) = best.get_mut(tid) {
            if ord > entry.0 {
                *entry = (ord, rid, false);
            } else if ord == entry.0 {
                entry.2 = true;
            }
        } else {
            best.insert(tid.to_owned(), (ord, rid, false));
        }
        Ok(())
    })?;
    if !usable || best.values().any(|v| v.2) {
        return Ok(None);
    }
    let mut winners: Vec<(i64, Option<i64>, String)> = best
        .into_iter()
        .map(|(tid, (ord, rid, _))| (rid, ord, tid))
        .collect();
    // Preserve oracle's ORDER BY ordinal DESC order for latest_order.
    winners.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let latest_order: Vec<String> = winners.iter().map(|(_, _, tid)| tid.clone()).collect();
    let mut expected_by_rid: HashMap<i64, String, std::hash::BuildHasherDefault<RowidHasher>> =
        HashMap::with_capacity_and_hasher(winners.len(), Default::default());
    let mut sorted_rids: Vec<i64> = Vec::with_capacity(winners.len());
    for (rid, _, tid) in winners {
        sorted_rids.push(rid);
        expected_by_rid.insert(rid, tid);
    }
    sorted_rids.sort_unstable();
    let mut latest = HashMap::with_capacity(sorted_rids.len());
    let proj = projection("runs");
    let mut matched = true;
    let mut fetched = 0usize;
    let mut json_dur = Duration::ZERO;
    for chunk in sorted_rids.chunks(900) {
        let fetch_sql = format!(
            "SELECT rowid, {proj} FROM {table} WHERE rowid IN ({})",
            marks(chunk.len())
        );
        let args: Vec<SqlValue> = chunk.iter().copied().map(SqlValue::Integer).collect();
        each_row_uncached(db, &fetch_sql, &args, |row| {
            let rid = match row.get_ref(0)? {
                ValueRef::Integer(id) => id,
                _ => {
                    matched = false;
                    return Ok(());
                }
            };
            let j0 = timing::on().then(Instant::now);
            let run = parse_json_ref(row.get_ref(1)?)?;
            if let Some(t) = j0 {
                json_dur += t.elapsed();
            }
            let id = str_at(&run, "threadId");
            if expected_by_rid.get(&rid).map(String::as_str) != Some(id) {
                matched = false;
                return Ok(());
            }
            latest.insert(id.to_owned(), run);
            fetched += 1;
            Ok(())
        })?;
        if !matched {
            return Ok(None);
        }
    }
    if timing::on() && !json_dur.is_zero() {
        if let Some(t) = Instant::now().checked_sub(json_dur) {
            timing::add_json(t);
        }
    }
    if fetched != sorted_rids.len() {
        return Ok(None);
    }
    Ok(Some((latest, latest_order, fetched)))
}

pub(super) fn read_turns_and_history_fast(
    db: &Connection,
    where_ids: &str,
    params: &[SqlValue],
    path: &Path,
) -> Result<Option<(HashMap<String, Json>, Vec<HistoryRow>, usize)>, SourceError> {
    let table = format!("{PREFIX}provider_turns");
    if !rowid_unshadowed(db, &table).map_err(|e| db_error(path, e))? {
        return Ok(None);
    }
    let mut groups: BTreeMap<Option<String>, Vec<(Option<i64>, i64)>> = BTreeMap::new();
    let mut usable = true;
    let meta_sql =
        format!("SELECT rowid, provider_thread_id, ordinal FROM {table} {where_ids} -- pt_meta");
    each_row_cached(db, &meta_sql, params, |row| {
        let rid = match row.get_ref(0)? {
            ValueRef::Integer(id) => id,
            _ => {
                usable = false;
                return Ok(());
            }
        };
        let pt = match row.get_ref(1)? {
            ValueRef::Null => None,
            ValueRef::Text(b) => match std::str::from_utf8(b) {
                Ok(s) => Some(s.to_owned()),
                Err(_) => {
                    usable = false;
                    return Ok(());
                }
            },
            _ => {
                usable = false;
                return Ok(());
            }
        };
        let ord = match row.get_ref(2)? {
            ValueRef::Null => None,
            ValueRef::Integer(v) => Some(v),
            _ => {
                usable = false;
                return Ok(());
            }
        };
        groups.entry(pt).or_default().push((ord, rid));
        Ok(())
    })
    .map_err(|e| db_error(path, e))?;
    if !usable {
        return Ok(None);
    }

    let mut rank1_order: Vec<i64> = Vec::with_capacity(groups.len());
    let mut rank1_set: HashSet<i64, std::hash::BuildHasherDefault<RowidHasher>> =
        HashSet::with_capacity_and_hasher(groups.len(), Default::default());
    let mut history_order: Vec<i64> = Vec::new();
    for (_, mut list) in groups {
        list.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        let cap = list.len().min(160);
        // If two turns in the same partition share an ordinal (either inside top 160
        // or at the 160/161 boundary), oracle's `ORDER BY ordinal DESC` tie order
        // depends on scan order; fall back cleanly.
        if (list.len() > 160 && list[159].0 == list[160].0)
            || list[..cap].windows(2).any(|w| w[0].0 == w[1].0)
        {
            return Ok(None);
        }
        if let Some(&(_, r1)) = list.first() {
            rank1_order.push(r1);
            rank1_set.insert(r1);
        }
        for &(_, rid) in &list[..cap] {
            history_order.push(rid);
        }
    }

    let mut rank1_sorted = rank1_order.clone();
    rank1_sorted.sort_unstable();
    let mut non_rank1_sorted: Vec<i64> = history_order
        .iter()
        .copied()
        .filter(|rid| !rank1_set.contains(rid))
        .collect();
    non_rank1_sorted.sort_unstable();

    let mut rank1_json: HashMap<i64, Json, std::hash::BuildHasherDefault<RowidHasher>> =
        HashMap::with_capacity_and_hasher(rank1_sorted.len(), Default::default());
    let mut history_by_rid: HashMap<i64, HistoryRow, std::hash::BuildHasherDefault<RowidHasher>> =
        HashMap::with_capacity_and_hasher(history_order.len(), Default::default());

    let proj = projection("provider_turns");
    let mut json_dur = Duration::ZERO;
    for chunk in rank1_sorted.chunks(900) {
        let sql = format!(
            "SELECT rowid, provider_thread_id, thread_id, started_at, completed_at, {proj},
                    json_extract(payload_json,'$.tokenUsage.usedTokens'),
                    json_extract(payload_json,'$.tokenUsage.maxTokens'),
                    json_extract(payload_json,'$.model')
             FROM {table} WHERE rowid IN ({}) -- pt_rank1",
            marks(chunk.len())
        );
        let args: Vec<SqlValue> = chunk.iter().copied().map(SqlValue::Integer).collect();
        each_row_uncached(db, &sql, &args, |row| {
            let ValueRef::Integer(rid) = row.get_ref(0)? else {
                return Ok(());
            };
            let provider_thread_id = sql_opt_text(row.get_ref(1)?).unwrap_or_default();
            let thread_id = sql_opt_text(row.get_ref(2)?).unwrap_or_default();
            let started_at = sql_opt_text(row.get_ref(3)?);
            let completed_at = sql_opt_text(row.get_ref(4)?);
            let j0 = timing::on().then(Instant::now);
            let p = parse_json_ref(row.get_ref(5)?)?;
            if let Some(t) = j0 {
                json_dur += t.elapsed();
            }
            history_by_rid.insert(
                rid,
                HistoryRow {
                    provider_thread_id,
                    thread_id,
                    started_at,
                    completed_at,
                    used: sql_number(row.get_ref(6)?),
                    max: sql_number(row.get_ref(7)?),
                    model: sql_opt_text(row.get_ref(8)?),
                },
            );
            rank1_json.insert(rid, p);
            Ok(())
        })
        .map_err(|e| db_error(path, e))?;
    }
    if timing::on() && !json_dur.is_zero() {
        if let Some(t) = Instant::now().checked_sub(json_dur) {
            timing::add_json(t);
        }
    }

    for chunk in non_rank1_sorted.chunks(900) {
        let sql = format!(
            "SELECT rowid, provider_thread_id, thread_id, started_at, completed_at,
                    json_extract(payload_json,'$.tokenUsage.usedTokens'),
                    json_extract(payload_json,'$.tokenUsage.maxTokens'),
                    json_extract(payload_json,'$.model')
             FROM {table} WHERE rowid IN ({}) -- pt_hist",
            marks(chunk.len())
        );
        let args: Vec<SqlValue> = chunk.iter().copied().map(SqlValue::Integer).collect();
        each_row_uncached(db, &sql, &args, |row| {
            let ValueRef::Integer(rid) = row.get_ref(0)? else {
                return Ok(());
            };
            history_by_rid.insert(
                rid,
                HistoryRow {
                    provider_thread_id: sql_opt_text(row.get_ref(1)?).unwrap_or_default(),
                    thread_id: sql_opt_text(row.get_ref(2)?).unwrap_or_default(),
                    started_at: sql_opt_text(row.get_ref(3)?),
                    completed_at: sql_opt_text(row.get_ref(4)?),
                    used: sql_number(row.get_ref(5)?),
                    max: sql_number(row.get_ref(6)?),
                    model: sql_opt_text(row.get_ref(7)?),
                },
            );
            Ok(())
        })
        .map_err(|e| db_error(path, e))?;
    }

    let mut turns = HashMap::with_capacity(rank1_order.len());
    for rid in &rank1_order {
        if let Some(p) = rank1_json.remove(rid) {
            turns.insert(str_at(&p, "providerThreadId").to_owned(), p);
        }
    }
    let mut history = Vec::with_capacity(history_order.len());
    for rid in &history_order {
        if let Some(h) = history_by_rid.remove(rid) {
            history.push(h);
        }
    }
    let count = rank1_order.len() + history.len();
    Ok(Some((turns, history, count)))
}

pub(super) fn read_history_oracle_typed(
    db: &Connection,
    where_ids: &str,
    params: &[SqlValue],
) -> rusqlite::Result<Vec<HistoryRow>> {
    let sql = format!(
        "SELECT provider_thread_id,thread_id,started_at,completed_at,json_extract(payload_json,'$.tokenUsage.usedTokens') AS used,json_extract(payload_json,'$.tokenUsage.maxTokens') AS max,json_extract(payload_json,'$.model') AS model FROM (SELECT provider_thread_id,thread_id,started_at,completed_at,payload_json,ROW_NUMBER() OVER(PARTITION BY provider_thread_id ORDER BY ordinal DESC) AS rank FROM {PREFIX}provider_turns {where_ids}) WHERE rank<=160"
    );
    let mut out = Vec::new();
    each_row_cached(db, &sql, params, |row| {
        out.push(HistoryRow {
            provider_thread_id: sql_opt_text(row.get_ref(0)?).unwrap_or_default(),
            thread_id: sql_opt_text(row.get_ref(1)?).unwrap_or_default(),
            started_at: sql_opt_text(row.get_ref(2)?),
            completed_at: sql_opt_text(row.get_ref(3)?),
            used: sql_number(row.get_ref(4)?),
            max: sql_number(row.get_ref(5)?),
            model: sql_opt_text(row.get_ref(6)?),
        });
        Ok(())
    })?;
    Ok(out)
}

pub(super) fn read_nodes_typed(
    db: &Connection,
    where_ids: &str,
    params: &[SqlValue],
    active_set: &HashSet<&str>,
    subs: &[Json],
    path: &Path,
) -> Result<(NodeMap, usize), SourceError> {
    if subs.is_empty() {
        return Ok((NodeMap::new(), 0));
    }
    if let Ok(Some(found)) = nodes_for_subagents(db, active_set, subs) {
        return Ok(found);
    }
    if let Ok(Some(found)) = nodes_from_indexes_typed(db, where_ids, params) {
        return Ok(found);
    }
    let mut nodes = NodeMap::new();
    // Keep the original projection so duplicate node IDs follow the same
    // index visit order as the reference query.
    let sql = format!("SELECT node_id,parent_node_id,run_id FROM {PREFIX}nodes {where_ids}");
    let count = each_row_cached(db, &sql, params, |row| {
        let key = sql_opt_text(row.get_ref(0)?).unwrap_or_default();
        if key.is_empty() {
            return Ok(());
        }
        match sql_opt_text(row.get_ref(1)?).filter(|p| !p.is_empty()) {
            Some(parent) => {
                nodes.insert(key, parent);
            }
            None => {
                nodes.remove(&key);
            }
        }
        Ok(())
    })
    .map_err(|e| db_error(path, e))?;
    Ok((nodes, count))
}

/// Only nodes that descend from a subagent id via `parent_node_id` can ever
/// resolve to `Some(...)` in `Attributor::owner`. When `node_id` is unique and
/// `parent_node_id` is indexed, walk downward from `sub_ids` instead of reading
/// every node in the database.
fn nodes_for_subagents(
    db: &Connection,
    active_set: &HashSet<&str>,
    subs: &[Json],
) -> rusqlite::Result<Option<(NodeMap, usize)>> {
    let table = format!("{PREFIX}nodes");
    if !single_column_unique(db, &table, "node_id")? {
        return Ok(None);
    }
    let probe = format!(
        "SELECT node_id, parent_node_id, thread_id FROM {table} WHERE parent_node_id IN (?)"
    );
    let steps = plan(db, &probe, &[SqlValue::Text(String::new())])?;
    if steps.len() != 1
        || !steps[0].1.starts_with("SEARCH ")
        || !steps[0].1.contains("INDEX ")
        || !steps[0].1.contains("(parent_node_id=?)")
    {
        return Ok(None);
    }
    let mut sub_ids: HashSet<String> = HashSet::with_capacity(subs.len());
    let mut frontier: Vec<String> = Vec::with_capacity(subs.len());
    for sub in subs {
        let id = str_at(sub, "id");
        if !id.is_empty() && sub_ids.insert(id.to_owned()) {
            frontier.push(id.to_owned());
        }
    }
    if frontier.is_empty() {
        return Ok(Some((NodeMap::new(), 0)));
    }
    let mut visited: HashSet<String> = HashSet::new();
    let mut nodes: NodeMap = NodeMap::new();
    let mut selected = 0usize;
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for chunk in frontier.chunks(900) {
            let sql = format!(
                "SELECT node_id, parent_node_id, thread_id FROM {table} WHERE parent_node_id IN ({}) -- nodes_sub",
                marks(chunk.len())
            );
            let args = strings(chunk.iter().cloned());
            each_row_uncached(db, &sql, &args, |row| {
                selected += 1;
                let tid = match row.get_ref(2)? {
                    ValueRef::Text(b) => match std::str::from_utf8(b) {
                        Ok(s) => s,
                        Err(_) => return Ok(()),
                    },
                    _ => return Ok(()),
                };
                if !active_set.contains(tid) {
                    return Ok(());
                }
                if let (Some(nid), Some(pid)) =
                    (sql_opt_text(row.get_ref(0)?), sql_opt_text(row.get_ref(1)?))
                {
                    if !nid.is_empty() && !pid.is_empty() {
                        nodes.insert(nid.clone(), pid);
                        if !sub_ids.contains(&nid) && visited.insert(nid.clone()) {
                            next.push(nid);
                        }
                    }
                }
                Ok(())
            })?;
            if visited.len() > 16_384 {
                return Ok(None);
            }
        }
        frontier = next;
    }
    Ok(Some((nodes, selected)))
}

fn nodes_from_indexes_typed(
    db: &Connection,
    where_ids: &str,
    params: &[SqlValue],
) -> rusqlite::Result<Option<(NodeMap, usize)>> {
    let table = format!("{PREFIX}nodes");
    if !rowid_unshadowed(db, &table)? {
        return Ok(None);
    }
    let mut rowids = Vec::new();
    let mut usable = true;
    each_row_cached(
        db,
        &format!("SELECT rowid FROM {table} {where_ids}"),
        params,
        |row| {
            match row.get_ref(0)? {
                ValueRef::Integer(id) => rowids.push(id),
                _ => usable = false,
            }
            Ok(())
        },
    )?;
    if !usable {
        return Ok(None);
    }
    let n = rowids.len();
    if n == 0 {
        return Ok(Some((NodeMap::new(), 0)));
    }
    let mut position: HashMap<i64, usize, std::hash::BuildHasherDefault<RowidHasher>> =
        HashMap::with_capacity_and_hasher(n, Default::default());
    for (i, &id) in rowids.iter().enumerate() {
        if position.insert(id, i).is_some() {
            return Ok(None);
        }
    }
    let mut node_ids: Vec<Option<String>> = vec![None; n];
    let mut parents: Vec<Option<String>> = vec![None; n];
    for (column, target) in [("node_id", &mut node_ids), ("parent_node_id", &mut parents)] {
        let mut found = 0usize;
        each_row_cached(
            db,
            &format!("SELECT rowid,{column} FROM {table}"),
            &[],
            |row| {
                if let ValueRef::Integer(id) = row.get_ref(0)? {
                    if let Some(&i) = position.get(&id) {
                        target[i] = sql_opt_text(row.get_ref(1)?);
                        found += 1;
                    }
                }
                Ok(())
            },
        )?;
        if found != n {
            return Ok(None);
        }
    }
    let mut nodes = NodeMap::with_capacity(n);
    let mut seen_keys = HashSet::with_capacity(n);
    for (node_id, parent_node_id) in node_ids.into_iter().zip(parents) {
        let Some(key) = node_id.filter(|k| !k.is_empty()) else {
            continue;
        };
        if !seen_keys.insert(key.clone()) {
            return Ok(None);
        }
        if let Some(parent) = parent_node_id.filter(|p| !p.is_empty()) {
            nodes.insert(key, parent);
        }
    }
    Ok(Some((nodes, n)))
}

pub(super) fn read_projects_typed(
    db: &Connection,
) -> rusqlite::Result<(HashMap<String, ProjectRow>, usize)> {
    let mut out = HashMap::new();
    let count = each_row_cached(
        db,
        "SELECT project_id,title,workspace_root FROM projection_projects",
        &[],
        |row| {
            let key = sql_opt_text(row.get_ref(0)?).unwrap_or_default();
            let title = sql_opt_text(row.get_ref(1)?);
            let workspace_root = sql_opt_text(row.get_ref(2)?).filter(|s| !s.is_empty());
            out.insert(
                key,
                ProjectRow {
                    title,
                    workspace_root,
                },
            );
            Ok(())
        },
    )?;
    Ok((out, count))
}

pub(super) fn read_requests_typed(
    db: &Connection,
    where_ids: &str,
    req_fields: &str,
    params: &[SqlValue],
) -> rusqlite::Result<Vec<RequestRow>> {
    let sql = format!(
        "SELECT thread_id,{req_fields} FROM {PREFIX}runtime_requests {where_ids} AND {}",
        super::REQUEST_FILTER
    );
    let mut out = Vec::new();
    each_row_cached(db, &sql, params, |row| {
        // Columns: 0=thread_id, 1=node_id, 2=kind, 3=status, 4=created_at, 5=resolved_at, 6=async_input
        out.push(RequestRow {
            thread_id: sql_opt_text(row.get_ref(0)?).unwrap_or_default(),
            node_id: sql_opt_text(row.get_ref(1)?),
            kind: sql_opt_text(row.get_ref(2)?).unwrap_or_default(),
            created_at: sql_opt_text(row.get_ref(4)?).filter(|s| !s.is_empty()),
            async_input: matches!(row.get_ref(6)?, ValueRef::Integer(1)),
        });
        Ok(())
    })?;
    Ok(out)
}

pub(super) fn read_spans_typed(
    db: &Connection,
    where_ids: &str,
    since_params: &[SqlValue],
) -> rusqlite::Result<Vec<SpanRow>> {
    let sql = format!(
        "SELECT thread_id,requested_at,completed_at,json_extract(payload_json,'$.startedAt') AS started_at FROM {PREFIX}runs {where_ids} AND (completed_at IS NULL OR completed_at>=?)"
    );
    let mut out = Vec::new();
    each_row_cached(db, &sql, since_params, |row| {
        out.push(SpanRow {
            thread_id: sql_opt_text(row.get_ref(0)?).unwrap_or_default(),
            requested_at: sql_opt_text(row.get_ref(1)?),
            completed_at: sql_opt_text(row.get_ref(2)?),
            started_at: sql_opt_text(row.get_ref(3)?),
        });
        Ok(())
    })?;
    Ok(out)
}

/// The oracle's todo-list query (`oracle`), with its candidates found first.
///
/// The candidates (todo lists of those runs) are found from T3's conversation
/// index alone, then the oracle's own query runs with `+rowid IN (candidates)`
/// added when SQLite plans it identically to `oracle`, preserving tie order.
pub(super) fn todo_lists(
    db: &Connection,
    oracle: &str,
    run_marks: &str,
    args: &[SqlValue],
) -> rusqlite::Result<Option<Vec<Json>>> {
    let find = format!(
        "SELECT rowid FROM {PREFIX}turn_items INDEXED BY {PREFIX}turn_items_conversation_idx WHERE type='todo_list' AND run_id IN ({run_marks}) AND {CONVERSATION}"
    );
    // A missing or redefined index fails to prepare under INDEXED BY.
    let covering =
        format!("SCAN {PREFIX}turn_items USING COVERING INDEX {PREFIX}turn_items_conversation_idx");
    let steps = plan(db, &find, args)?;
    if steps.len() != 1 || steps[0].1 != covering {
        return Ok(None);
    }
    let mut ids = Vec::new();
    let mut usable = true;
    each_row(db, &find, args, |_, row| {
        match row.get_ref(0)? {
            ValueRef::Integer(id) => ids.push(SqlValue::Integer(id)),
            // A column named rowid would shadow the row id.
            _ => usable = false,
        }
        Ok(())
    })?;
    if !usable {
        return Ok(None);
    }
    if ids.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let narrowed = oracle.replace(
        " ORDER BY updated_at DESC",
        &format!(
            " AND +rowid IN ({}) ORDER BY updated_at DESC",
            marks(ids.len())
        ),
    );
    let mut narrowed_args = args.to_vec();
    narrowed_args.extend(ids);
    if plan(db, oracle, args)? != plan(db, &narrowed, &narrowed_args)? {
        return Ok(None);
    }
    let mut out = Vec::new();
    each_row(
        db,
        &format!("{narrowed} -- plans"),
        &narrowed_args,
        |names, row| {
            let mut value = serde_json::Map::new();
            for (i, name) in names.iter().enumerate() {
                value.insert(name.clone(), value_json(row.get_ref(i)?));
            }
            out.push(Value::Object(value));
            Ok(())
        },
    )?;
    Ok(Some(out))
}
