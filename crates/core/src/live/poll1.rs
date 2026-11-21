//! Poll-1 fast-scan helpers, lean decoders, and covering-index queries for `live::cache`.
//!
//! Privacy: uses only `source.rs`'s column/JSON allowlist and approved projection rowids.
use super::{
    FallbackReason,
    cache::{
        Body, Data, EncounterOrder, Item, ItemPayloadRead, Node, Payload, Pending, Pool, Project,
        Request, Row, Run, Select, Stamp, TABLES, Text, Thread, Turn, blank, classify,
        distinct_owners, eligibility_rows, empty_json, item_text_query, owner_query, queries,
        request_query, stamp_ge, stamp_opt, thread_active,
    },
    error, sql_error,
};
use crate::source::{self, REQUEST_FILTER, SourceError};
use rusqlite::{Connection, types::Value as SqlValue};
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Instant,
};

pub(super) fn parse_canonical_utc_ms(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() != 24
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'.'
        || b[23] != b'Z'
    {
        return None;
    }
    let digits = |start: usize, len: usize| -> Option<i64> {
        let mut val = 0i64;
        for &ch in &b[start..start + len] {
            if !ch.is_ascii_digit() {
                return None;
            }
            val = val * 10 + i64::from(ch - b'0');
        }
        Some(val)
    };
    let year = digits(0, 4)?;
    let month = digits(5, 2)?;
    let day = digits(8, 2)?;
    let hour = digits(11, 2)?;
    let minute = digits(14, 2)?;
    let second = digits(17, 2)?;
    let ms = digits(20, 3)?;
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if !(1..=max_day).contains(&day) {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1_000 + ms)
}

/// Fast multiplicative hasher for internal SQLite `i64` rowid lookup tables.
#[derive(Default)]
pub(super) struct RowidHasher(u64);
impl std::hash::Hasher for RowidHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn write_i64(&mut self, value: i64) {
        self.0 = (value as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

/// Fast non-cryptographic FNV-1a hasher for internal local ID maps/sets.
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

/// Inline-only columns of `orchestration_v2_projection_turn_items` (never
/// touches `payload_json` overflow pages).
pub(super) fn item_inline_query() -> String {
    "SELECT rowid AS cache_rowid,thread_id,run_id,node_id,provider_thread_id,type,status,updated_at,ordinal FROM orchestration_v2_projection_turn_items".to_string()
}

pub(super) fn run_inline_query() -> String {
    format!("{} WHERE run_id=?", item_inline_query())
}

/// Check once per schema whether omitting `payload_json` keeps the exact
/// same `WHERE thread_id=?` index order as `queries()[7]`.
pub(super) fn item_inline_query_ok(db: &Connection) -> bool {
    let table = "orchestration_v2_projection_turn_items";
    let full = classify(
        db,
        table,
        " (thread_id=?)",
        owner_query(7, &queries()[7].1),
        vec![SqlValue::Text(String::new())],
    );
    let inline = classify(
        db,
        table,
        " (thread_id=?)",
        owner_query(7, &item_inline_query()),
        vec![SqlValue::Text(String::new())],
    );
    full.is_some() && full == inline
}

/// By-rowid keys-only read for status-only polls (`startedAt`, `toolName`,
/// `exitCode`), skipping `label`, `command`, `file`, and `steps`.
pub(super) fn item_keys_query(n: usize) -> String {
    let marks = if n == 0 {
        "NULL".to_string()
    } else {
        vec!["?"; n.max(1)].join(",")
    };
    format!(
        "SELECT rowid,{fields},ordinal FROM orchestration_v2_projection_turn_items WHERE rowid IN ({marks})",
        fields = source::item_keys()
    )
}

pub(super) struct ItemGate<'a> {
    pub(super) since: String,
    pub(super) active: &'a HashSet<String>,
    pub(super) extra_active: HashSet<String>,
    pub(super) run_refs: &'a HashSet<String>,
    pub(super) extra_runs: HashSet<String>,
    pub(super) existing_items: Option<&'a BTreeMap<i64, Row>>,
}

impl<'a> ItemGate<'a> {
    fn has_owner(&self, owner: &str) -> bool {
        self.active.contains(owner) || self.extra_active.contains(owner)
    }
    fn has_run(&self, run: &str) -> bool {
        self.run_refs.contains(run) || self.extra_runs.contains(run)
    }
    fn keeps_fields(
        &self,
        id: i64,
        owner: &str,
        run_id: Option<&str>,
        kind: &str,
        updated_at: &str,
    ) -> (bool, Option<Stamp>) {
        if self.existing_items.is_some_and(|m| m.contains_key(&id)) {
            return (true, None);
        }
        let run = run_id.filter(|r| !r.is_empty());
        let selected = run.is_some_and(|r| self.has_run(r));
        if source::is_toolish(kind) {
            if selected || (run_id.is_none() && self.has_owner(owner)) {
                return (true, None);
            }
        } else if kind == "todo_list" && selected {
            return (true, None);
        }
        if source::is_event_kind(kind) && self.has_owner(owner) {
            let stamp = Stamp::parse(updated_at);
            if stamp_ge(&stamp, &self.since) {
                return (true, Some(stamp));
            }
        }
        (false, None)
    }
}

fn sql_opt_text<'a>(row: &'a rusqlite::Row<'_>, i: usize) -> rusqlite::Result<Option<Cow<'a, str>>> {
    match row.get_ref(i)? {
        rusqlite::types::ValueRef::Null => Ok(None),
        rusqlite::types::ValueRef::Text(v) => Ok(Some(String::from_utf8_lossy(v))),
        other => Err(rusqlite::Error::InvalidColumnType(
            i,
            "text".into(),
            other.data_type(),
        )),
    }
}

fn sql_must_text<'a>(row: &'a rusqlite::Row<'_>, i: usize) -> rusqlite::Result<Cow<'a, str>> {
    sql_opt_text(row, i)?.ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(i, "text".into(), rusqlite::types::Type::Null)
    })
}

fn sql_payload(row: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<Value> {
    let bytes = match row.get_ref(i)? {
        rusqlite::types::ValueRef::Text(v) => v,
        other => {
            return Err(rusqlite::Error::InvalidColumnType(
                i,
                "text".into(),
                other.data_type(),
            ));
        }
    };
    match std::str::from_utf8(bytes) {
        Ok(text) => serde_json::from_str(text),
        Err(_) => {
            let lossy = String::from_utf8_lossy(bytes);
            serde_json::from_str(&lossy)
        }
    }
    .map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(err))
    })
}

fn eqp_lines(db: &Connection, sql: &str, args: &[SqlValue]) -> Option<Vec<String>> {
    db.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .ok()?
        .query_map(rusqlite::params_from_iter(args.iter()), |r| r.get::<_, String>(3))
        .ok()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ok()
}

fn eqp_is_covering_index(db: &Connection, table: &str, sql: &str) -> bool {
    let Some(plans) = eqp_lines(db, sql, &[]) else {
        return false;
    };
    plans.len() == 1 && plans[0].starts_with(&format!("SCAN {table} USING COVERING INDEX "))
}

/// Verify that `idx`'s leading column (`seqno == 0`) is ascending (`desc == 0`)
/// with `BINARY` collation, so `INDEXED BY idx ORDER BY thread_id` scans the
/// index forward in the exact same intra-owner order as `WHERE thread_id=?`.
pub(super) fn index_first_col_asc_binary(db: &Connection, idx: &str) -> bool {
    if idx.is_empty()
        || !idx
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
    {
        return false;
    }
    let Ok(mut stmt) = db.prepare(&format!("PRAGMA index_xinfo(\"{idx}\")")) else {
        return false;
    };
    let Ok(mut rows) = stmt.query([]) else {
        return false;
    };
    while let Ok(Some(row)) = rows.next() {
        let Ok(seqno) = row.get::<_, i64>(0) else {
            return false;
        };
        if seqno == 0 {
            let desc = row.get::<_, i64>(3).ok();
            let coll = row.get::<_, String>(4).ok();
            return desc == Some(0) && coll.as_deref() == Some("BINARY");
        }
    }
    false
}

fn thread_rows(
    db: &Connection,
    sql: &str,
    args: &[SqlValue],
    pool: &mut Pool,
) -> rusqlite::Result<Vec<Row>> {
    use rusqlite::params_from_iter;
    let timed = source::timing::on().then(Instant::now);
    let mut stmt = db.prepare_cached(sql)?;
    let stepped = timed.map(|_| Instant::now());
    let mut out = Vec::new();
    let mut rows = stmt.query(params_from_iter(args.iter()))?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let owner = pool.intern(&sql_must_text(row, 1)?);
        let payload = sql_payload(row, 2)?;
        let archived_at = sql_opt_text(row, 3)?.map(Cow::into_owned);
        let deleted_at = sql_opt_text(row, 4)?.map(Cow::into_owned);
        out.push(Row {
            id,
            owner,
            data: Data::Thread(Box::new(Thread {
                data: Payload { payload },
                archived_at,
                deleted_at,
            })),
            position: 0,
        });
    }
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        source::timing::query(sql, prepare, stepped, Instant::now(), out.len());
    }
    Ok(out)
}

fn run_rows(
    db: &Connection,
    sql: &str,
    args: &[SqlValue],
    pool: &mut Pool,
    active: Option<&HashSet<String>>,
) -> rusqlite::Result<Vec<Row>> {
    use rusqlite::params_from_iter;
    let timed = source::timing::on().then(Instant::now);
    let mut stmt = db.prepare_cached(sql)?;
    let stepped = timed.map(|_| Instant::now());
    let mut out = Vec::new();
    let mut rows = stmt.query(params_from_iter(args.iter()))?;
    while let Some(row) = rows.next()? {
        let owner_ref = sql_must_text(row, 1)?;
        if active.is_some_and(|set| !set.contains(owner_ref.as_ref())) {
            continue;
        }
        let id: i64 = row.get(0)?;
        let owner = pool.intern(&owner_ref);
        let payload = sql_payload(row, 2)?;
        let ordinal: i64 = row.get(3)?;
        let requested_at = sql_must_text(row, 4)?.into_owned();
        let completed_at = sql_opt_text(row, 5)?.map(Cow::into_owned);
        out.push(Row {
            id,
            owner,
            data: Data::Run(Box::new(Run {
                data: Payload { payload },
                ordinal,
                requested_at,
                completed_at,
            })),
            position: 0,
        });
    }
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        source::timing::query(sql, prepare, stepped, Instant::now(), out.len());
    }
    Ok(out)
}

fn payload_rows(
    db: &Connection,
    index: usize,
    sql: &str,
    args: &[SqlValue],
    pool: &mut Pool,
    active: Option<&HashSet<String>>,
) -> rusqlite::Result<Vec<Row>> {
    use rusqlite::params_from_iter;
    let timed = source::timing::on().then(Instant::now);
    let mut stmt = db.prepare_cached(sql)?;
    let stepped = timed.map(|_| Instant::now());
    let mut out = Vec::new();
    let mut rows = stmt.query(params_from_iter(args.iter()))?;
    while let Some(row) = rows.next()? {
        let owner_ref = sql_must_text(row, 1)?;
        if active.is_some_and(|set| !set.contains(owner_ref.as_ref())) {
            continue;
        }
        let id: i64 = row.get(0)?;
        let owner = pool.intern(&owner_ref);
        let payload = sql_payload(row, 2)?;
        let data = if index == 2 {
            Data::Subagent(Box::new(Payload { payload }))
        } else {
            Data::Provider(Box::new(Payload { payload }))
        };
        out.push(Row {
            id,
            owner,
            data,
            position: 0,
        });
    }
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        source::timing::query(sql, prepare, stepped, Instant::now(), out.len());
    }
    Ok(out)
}

fn request_rows(
    db: &Connection,
    sql: &str,
    args: &[SqlValue],
    pool: &mut Pool,
) -> rusqlite::Result<Vec<Row>> {
    use rusqlite::params_from_iter;
    let timed = source::timing::on().then(Instant::now);
    let mut stmt = db.prepare_cached(sql)?;
    let stepped = timed.map(|_| Instant::now());
    let mut out = Vec::new();
    let mut rows = stmt.query(params_from_iter(args.iter()))?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let owner = pool.intern(&sql_must_text(row, 1)?);
        let node_id = sql_must_text(row, 2)?.into_owned();
        let kind = sql_must_text(row, 3)?.into_owned();
        let status = sql_must_text(row, 4)?.into_owned();
        let created_at = sql_opt_text(row, 5)?.map(Cow::into_owned);
        let resolved_at = sql_opt_text(row, 6)?.map(Cow::into_owned);
        let async_input: i64 = row.get(7)?;
        out.push(Row {
            id,
            owner,
            data: Data::Request(Box::new(Request {
                node_id,
                kind,
                status,
                created_at,
                resolved_at,
                async_input,
            })),
            position: 0,
        });
    }
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        source::timing::query(sql, prepare, stepped, Instant::now(), out.len());
    }
    Ok(out)
}

fn project_rows(db: &Connection) -> rusqlite::Result<(BTreeMap<String, Project>, usize)> {
    let sql = "SELECT project_id,title,workspace_root FROM projection_projects";
    let timed = source::timing::on().then(Instant::now);
    let mut stmt = db.prepare_cached(sql)?;
    let stepped = timed.map(|_| Instant::now());
    let mut projects = BTreeMap::new();
    let mut count = 0usize;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let project_id = sql_must_text(row, 0)?.into_owned();
        let title = sql_must_text(row, 1)?.into_owned();
        let workspace_root = sql_must_text(row, 2)?.into_owned();
        count += 1;
        projects.insert(
            project_id.clone(),
            Project {
                project_id,
                title,
                workspace_root,
            },
        );
    }
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        source::timing::query(sql, prepare, stepped, Instant::now(), count);
    }
    Ok((projects, count))
}

/// Node rows straight from SQLite. Decoding through `source::all` built a
/// temporary JSON object per row, interleaved with the node boxes that stay.
/// Those allocations kept extra heap resident after the read. Accepts and
/// rejects what `source::all` plus serde did: text is lossy UTF-8, NULL and blob
/// are absent, a number in an id column is an error.
pub(super) fn node_rows(
    db: &Connection,
    sql: &str,
    args: &[SqlValue],
    pool: &mut Pool,
) -> rusqlite::Result<Vec<Row>> {
    use rusqlite::{params_from_iter, types::ValueRef};
    let timed = source::timing::on().then(Instant::now);
    let mut stmt = db.prepare_cached(sql)?;
    let stepped = timed.map(|_| Instant::now());
    let mut out = Vec::new();
    let mut rows = stmt.query(params_from_iter(args.iter()))?;
    while let Some(row) = rows.next()? {
        let text = |i: usize| -> rusqlite::Result<Option<Cow<'_, str>>> {
            match row.get_ref(i)? {
                ValueRef::Null | ValueRef::Blob(_) => Ok(None),
                ValueRef::Text(v) => Ok(Some(String::from_utf8_lossy(v))),
                other => Err(rusqlite::Error::InvalidColumnType(
                    i,
                    "text".into(),
                    other.data_type(),
                )),
            }
        };
        let must = |i: usize| {
            text(i)?.ok_or_else(|| {
                rusqlite::Error::InvalidColumnType(i, "text".into(), rusqlite::types::Type::Null)
            })
        };
        let id: i64 = row.get(0)?;
        let owner = pool.intern(&must(1)?);
        let node_id = pool.intern(&must(2)?);
        let parent = text(3)?.map(|parent| pool.intern(&parent));
        // Checked like the old decoder did; a node's run id reaches no snapshot.
        text(4)?;
        out.push(Row {
            id,
            owner,
            data: Data::Node(Box::new(Node {
                node_id,
                parent_node_id: parent,
            })),
            position: 0,
        });
    }
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        source::timing::query(sql, prepare, stepped, Instant::now(), out.len());
    }
    Ok(out)
}

fn nodes_from_covering_indexes(
    db: &Connection,
    active: &HashSet<String>,
    pool: &mut Pool,
) -> rusqlite::Result<Option<Vec<Row>>> {
    use rusqlite::types::ValueRef;
    let table = "orchestration_v2_projection_nodes";
    let q1 = "SELECT rowid,thread_id,run_id FROM orchestration_v2_projection_nodes";
    let q2 = "SELECT rowid,node_id FROM orchestration_v2_projection_nodes";
    let q3 = "SELECT rowid,parent_node_id FROM orchestration_v2_projection_nodes";
    if !eqp_is_covering_index(db, table, q1)
        || !eqp_is_covering_index(db, table, q2)
        || !eqp_is_covering_index(db, table, q3)
    {
        return Ok(None);
    }
    let timed = source::timing::on().then(Instant::now);
    let mut out: Vec<Row> = Vec::new();
    let mut by_id: HashMap<i64, usize, std::hash::BuildHasherDefault<RowidHasher>> =
        HashMap::default();
    let mut total1 = 0usize;
    {
        let mut stmt = db.prepare_cached(q1)?;
        let stepped = timed.map(|_| Instant::now());
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            total1 += 1;
            let id = match row.get_ref(0)? {
                ValueRef::Integer(v) => v,
                _ => return Ok(None),
            };
            match row.get_ref(2)? {
                ValueRef::Null | ValueRef::Blob(_) | ValueRef::Text(_) => {}
                other => {
                    return Err(rusqlite::Error::InvalidColumnType(
                        2,
                        "text".into(),
                        other.data_type(),
                    ));
                }
            }
            let owner_ref = match row.get_ref(1)? {
                ValueRef::Text(v) => String::from_utf8_lossy(v),
                ValueRef::Null | ValueRef::Blob(_) => return Ok(None),
                other => {
                    return Err(rusqlite::Error::InvalidColumnType(
                        1,
                        "text".into(),
                        other.data_type(),
                    ));
                }
            };
            if !active.contains(owner_ref.as_ref()) {
                continue;
            }
            let idx = out.len();
            if by_id.insert(id, idx).is_some() {
                return Ok(None);
            }
            out.push(Row {
                id,
                owner: pool.intern(&owner_ref),
                data: Data::Node(Box::new(Node {
                    node_id: blank(),
                    parent_node_id: None,
                })),
                position: 0,
            });
        }
        if let (Some(prepare), Some(stepped)) = (timed, stepped) {
            source::timing::query(q1, prepare, stepped, Instant::now(), out.len());
        }
    }
    let mut total2 = 0usize;
    let mut matched2 = 0usize;
    {
        let timed2 = source::timing::on().then(Instant::now);
        let mut stmt = db.prepare_cached(q2)?;
        let stepped2 = timed2.map(|_| Instant::now());
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            total2 += 1;
            let id = match row.get_ref(0)? {
                ValueRef::Integer(v) => v,
                _ => return Ok(None),
            };
            if let Some(&idx) = by_id.get(&id) {
                let node_id = match row.get_ref(1)? {
                    ValueRef::Text(v) => pool.intern(&String::from_utf8_lossy(v)),
                    ValueRef::Null | ValueRef::Blob(_) => return Ok(None),
                    other => {
                        return Err(rusqlite::Error::InvalidColumnType(
                            1,
                            "text".into(),
                            other.data_type(),
                        ));
                    }
                };
                if let Data::Node(node) = &mut out[idx].data {
                    node.node_id = node_id;
                }
                matched2 += 1;
            }
        }
        if let (Some(prepare), Some(stepped)) = (timed2, stepped2) {
            source::timing::query(q2, prepare, stepped, Instant::now(), matched2);
        }
    }
    let mut total3 = 0usize;
    let mut matched3 = 0usize;
    {
        let timed3 = source::timing::on().then(Instant::now);
        let mut stmt = db.prepare_cached(q3)?;
        let stepped3 = timed3.map(|_| Instant::now());
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            total3 += 1;
            let id = match row.get_ref(0)? {
                ValueRef::Integer(v) => v,
                _ => return Ok(None),
            };
            if let Some(&idx) = by_id.get(&id) {
                let parent = match row.get_ref(1)? {
                    ValueRef::Null | ValueRef::Blob(_) => None,
                    ValueRef::Text(v) => Some(pool.intern(&String::from_utf8_lossy(v))),
                    other => {
                        return Err(rusqlite::Error::InvalidColumnType(
                            1,
                            "text".into(),
                            other.data_type(),
                        ));
                    }
                };
                if let Data::Node(node) = &mut out[idx].data {
                    node.parent_node_id = parent;
                }
                matched3 += 1;
            }
        }
        if let (Some(prepare), Some(stepped)) = (timed3, stepped3) {
            source::timing::query(q3, prepare, stepped, Instant::now(), matched3);
        }
    }
    if total1 != total2 || total2 != total3 || matched2 != out.len() || matched3 != out.len() {
        return Ok(None);
    }
    Ok(Some(out))
}

fn turn_rows(
    db: &Connection,
    sql: &str,
    args: &[SqlValue],
    pool: &mut Pool,
    active: Option<&HashSet<String>>,
    lean: bool,
) -> rusqlite::Result<Vec<Row>> {
    use rusqlite::{params_from_iter, types::ValueRef};
    let timed = source::timing::on().then(Instant::now);
    let mut stmt = db.prepare_cached(sql)?;
    let stepped = timed.map(|_| Instant::now());
    let mut out = Vec::new();
    let mut rows = stmt.query(params_from_iter(args.iter()))?;
    while let Some(row) = rows.next()? {
        let text = |i: usize| match row.get_ref(i)? {
            ValueRef::Null => Ok(None),
            ValueRef::Text(v) => Ok(Some(String::from_utf8_lossy(v).into_owned())),
            other => Err(rusqlite::Error::InvalidColumnType(
                i,
                "text".into(),
                other.data_type(),
            )),
        };
        let must = |i: usize| {
            text(i)?.ok_or_else(|| {
                rusqlite::Error::InvalidColumnType(i, "text".into(), rusqlite::types::Type::Null)
            })
        };
        let owner_ref = match row.get_ref(1)? {
            ValueRef::Text(v) => String::from_utf8_lossy(v),
            other => {
                return Err(rusqlite::Error::InvalidColumnType(
                    1,
                    "text".into(),
                    other.data_type(),
                ));
            }
        };
        if active.is_some_and(|set| !set.contains(owner_ref.as_ref())) {
            continue;
        }
        let json = must(2)?;
        let provider_thread_id = must(3)?;
        let parsed: Value = serde_json::from_str(&json).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(err),
            )
        })?;
        let model = &parsed["model"];
        let model_bad = !model.is_null() && !model.is_string();
        let model = model.as_str().map(|text| pool.intern(text));
        let identity_bad = parsed["providerThreadId"].as_str() != Some(provider_thread_id.as_str());
        let usage = parsed.get("tokenUsage").filter(|value| value.is_object());
        let (used, max) = usage
            .map(|usage| (source::number(usage, "usedTokens"), source::number(usage, "maxTokens")))
            .unwrap_or((0., 0.));
        let (lean_context, context_bad) = if lean {
            if let Some(u) = usage.filter(|u| u.get("usedTokens").is_some_and(|v| !v.is_null())) {
                match serde_json::from_value::<crate::model::Context>(u.clone()) {
                    Ok(ctx) => (Some(Box::new(ctx)), false),
                    Err(_) => (None, true),
                }
            } else {
                (None, false)
            }
        } else {
            (None, false)
        };
        out.push(Row {
            id: row.get(0)?,
            owner: pool.intern(&owner_ref),
            position: 0,
            data: Data::Turn(Box::new(Turn {
                json: if lean { empty_json() } else { Arc::from(json) },
                provider_thread_id: pool.intern(&provider_thread_id),
                ordinal: row.get(4)?,
                started_at: stamp_opt(text(5)?),
                completed_at: stamp_opt(text(6)?),
                model_bad,
                identity_bad,
                context_bad,
                lean_context,
                model,
                used,
                max,
            })),
        });
    }
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        source::timing::query(sql, prepare, stepped, Instant::now(), out.len());
    }
    Ok(out)
}

/// Turn-items skip the generic JSON map and skip `payload_json` when
/// `inline == true`. Payload-derived keys and text are loaded later by rowid
/// only for selected rows.
fn decode_item_row(
    row: &rusqlite::Row<'_>,
    inline: bool,
    position: usize,
    pool: &mut Pool,
    gate: Option<&ItemGate<'_>>,
) -> rusqlite::Result<Option<Row>> {
    use rusqlite::types::ValueRef;
    let id: i64 = row.get(0)?;
    let owner_ref = sql_must_text(row, 1)?;
    let (run_col, node_col, pid_col, kind_col, status_col, upd_col, ord_col) = if inline {
        (2, 3, 4, 5, 6, 7, 8)
    } else {
        // Legacy full-key projection duplicates thread_id at col 2; validate it.
        let _ = sql_must_text(row, 2)?;
        (3, 4, 5, 6, 7, 8, 12)
    };
    let run_ref = sql_opt_text(row, run_col)?;
    let kind_ref = sql_must_text(row, kind_col)?;
    let upd_ref = sql_must_text(row, upd_col)?;
    let mut pre_stamp = None;
    if let Some(gate) = gate {
        let (keep, stamp) = gate.keeps_fields(
            id,
            owner_ref.as_ref(),
            run_ref.as_deref(),
            kind_ref.as_ref(),
            upd_ref.as_ref(),
        );
        if !keep {
            return Ok(None);
        }
        pre_stamp = stamp;
    }
    let node_id = pool.intern_opt_ref(sql_opt_text(row, node_col)?.as_deref());
    let provider_thread_id = pool.intern_opt_ref(sql_opt_text(row, pid_col)?.as_deref());
    let status = pool.intern(&sql_must_text(row, status_col)?);
    if !inline {
        let _ = sql_opt_text(row, 9)?;
        let _ = sql_opt_text(row, 10)?;
        match row.get_ref(11)? {
            ValueRef::Null => {}
            ValueRef::Integer(v) => {
                i32::try_from(v).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        11,
                        rusqlite::types::Type::Integer,
                        Box::new(err),
                    )
                })?;
            }
            other => {
                return Err(rusqlite::Error::InvalidColumnType(
                    11,
                    "integer".into(),
                    other.data_type(),
                ));
            }
        }
    }
    let updated_at = pre_stamp.unwrap_or_else(|| Stamp::parse(upd_ref.as_ref()));
    let ordinal: i64 = row.get(ord_col)?;
    Ok(Some(Row {
        id,
        owner: pool.intern(owner_ref.as_ref()),
        position,
        data: Data::Item(Box::new(Item {
            run_id: pool.intern_opt_ref(run_ref.as_deref()),
            node_id,
            provider_thread_id,
            kind: pool.intern(kind_ref.as_ref()),
            status,
            updated_at,
            ordinal,
            payload: None,
        })),
    }))
}

pub(super) fn item_rows_scoped(
    db: &Connection,
    sql: &str,
    args: &[SqlValue],
    inline: bool,
    pool: &mut Pool,
    gate: Option<&ItemGate<'_>>,
) -> rusqlite::Result<Vec<Row>> {
    use rusqlite::params_from_iter;
    let timed = source::timing::on().then(Instant::now);
    let mut stmt = db.prepare_cached(sql)?;
    let stepped = timed.map(|_| Instant::now());
    let mut out = Vec::new();
    let mut rows = stmt.query(params_from_iter(args.iter()))?;
    let mut pos = 0usize;
    while let Some(row) = rows.next()? {
        if let Some(decoded) = decode_item_row(row, inline, pos, pool, gate)? {
            out.push(decoded);
        }
        pos += 1;
    }
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        source::timing::query(sql, prepare, stepped, Instant::now(), out.len());
    }
    Ok(out)
}

pub(super) fn item_rows(
    db: &Connection,
    sql: &str,
    args: &[SqlValue],
    pool: &mut Pool,
) -> rusqlite::Result<Vec<Row>> {
    item_rows_scoped(db, sql, args, false, pool, None)
}

#[allow(clippy::too_many_arguments)]
fn load_rows_scoped(
    db: &Connection,
    path: &Path,
    index: usize,
    sql: &str,
    args: &[SqlValue],
    pool: &mut Pool,
    active: Option<&HashSet<String>>,
    lean: bool,
) -> Result<Vec<Row>, SourceError> {
    let res = match index {
        0 => thread_rows(db, sql, args, pool),
        1 => run_rows(db, sql, args, pool, active),
        2 | 3 => payload_rows(db, index, sql, args, pool, active),
        4 => turn_rows(db, sql, args, pool, active, lean),
        5 => {
            let mut rows = node_rows(db, sql, args, pool).map_err(|e| error(path, e))?;
            if let Some(active) = active {
                rows.retain(|r| active.contains(r.owner.as_ref()));
            }
            return Ok(rows);
        }
        6 => request_rows(db, sql, args, pool),
        7 => item_rows(db, sql, args, pool),
        _ => unreachable!(),
    };
    res.map_err(|e| error(path, e))
}

pub(super) fn load_rows(
    db: &Connection,
    path: &Path,
    index: usize,
    sql: &str,
    args: &[SqlValue],
    pool: &mut Pool,
) -> Result<Vec<Row>, SourceError> {
    load_rows_scoped(db, path, index, sql, args, pool, None, false)
}

/// Single-pass owner position tracking for `subagents` and `turn_items`
/// during a full read (`owners.is_none()`), matching `WHERE thread_id=?`
/// encounter order when proved, else falling back to per-owner queries.
/// For inline `turn_items` on an ascending `BINARY` `thread_id` index, scans
/// the tiny covering index for `rowid -> per-owner position` and then scans
/// the table sequentially in `rowid` leaf order to avoid random table probes.
#[allow(clippy::too_many_arguments)]
fn owner_positions_from_index(
    db: &Connection,
    path: &Path,
    index: usize,
    sql: &str,
    inline: bool,
    pool: &mut Pool,
    active: Option<&HashSet<String>>,
    gate: Option<&ItemGate<'_>>,
) -> Result<(Vec<Row>, usize), SourceError> {
    let table = format!("orchestration_v2_projection_{}", TABLES[index]);
    let order = classify(
        db,
        &table,
        " (thread_id=?)",
        owner_query(index, sql),
        vec![SqlValue::Text(String::new())],
    );
    let mut cover_sql: Option<String> = None;
    let (scan_sql, grouped) = match order {
        Some(EncounterOrder::ThreadIndex(idx)) if index_first_col_asc_binary(db, &idx) => {
            if index == 7 && inline {
                let cover = format!(
                    "SELECT rowid,thread_id FROM {table} INDEXED BY {idx} ORDER BY thread_id"
                );
                let cover_expected = [format!("SCAN {table} USING COVERING INDEX {idx}")];
                let table_expected = [format!("SCAN {table}")];
                if eqp_lines(db, &cover, &[]).as_deref() == Some(&cover_expected)
                    && eqp_lines(db, sql, &[]).as_deref() == Some(&table_expected)
                {
                    cover_sql = Some(cover);
                }
            }
            let candidate = format!("{sql} INDEXED BY {idx} ORDER BY thread_id");
            let expected = [format!("SCAN {table} USING INDEX {idx}")];
            if eqp_lines(db, &candidate, &[]).as_deref() == Some(&expected) {
                (Some(candidate), true)
            } else {
                (None, false)
            }
        }
        Some(EncounterOrder::Rowid) => {
            let expected = [format!("SCAN {table}")];
            if eqp_lines(db, sql, &[]).as_deref() == Some(&expected) {
                (Some(sql.to_string()), false)
            } else {
                (None, false)
            }
        }
        _ => (None, false),
    };
    if let Some(cover_sql) = cover_sql {
        let two_pass: rusqlite::Result<Option<Vec<Row>>> = (|| {
            use rusqlite::types::ValueRef;
            let mut pos_map: HashMap<i64, u32, std::hash::BuildHasherDefault<RowidHasher>> =
                HashMap::default();
            let mut cover_ok = true;
            {
                let timed = source::timing::on().then(Instant::now);
                let mut stmt = db.prepare_cached(&cover_sql)?;
                let stepped = timed.map(|_| Instant::now());
                let mut prev_owner: Option<Vec<u8>> = None;
                let mut cur_pos: u32 = 0;
                let mut n = 0usize;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let ValueRef::Integer(id) = row.get_ref(0)? else {
                        cover_ok = false;
                        break;
                    };
                    let ValueRef::Text(owner) = row.get_ref(1)? else {
                        cover_ok = false;
                        break;
                    };
                    if std::str::from_utf8(owner).is_err() {
                        cover_ok = false;
                        break;
                    }
                    if prev_owner.as_deref() == Some(owner) {
                        let Some(next) = cur_pos.checked_add(1) else {
                            cover_ok = false;
                            break;
                        };
                        cur_pos = next;
                    } else {
                        cur_pos = 0;
                        match &mut prev_owner {
                            Some(buf) => {
                                buf.clear();
                                buf.extend_from_slice(owner);
                            }
                            None => prev_owner = Some(owner.to_vec()),
                        }
                    }
                    if pos_map.insert(id, cur_pos).is_some() {
                        cover_ok = false;
                        break;
                    }
                    n += 1;
                }
                if let (Some(prepare), Some(stepped)) = (timed, stepped) {
                    source::timing::query(&cover_sql, prepare, stepped, Instant::now(), n);
                }
            }
            if !cover_ok {
                return Ok(None);
            }
            let timed = source::timing::on().then(Instant::now);
            let mut stmt = db.prepare_cached(sql)?;
            let stepped = timed.map(|_| Instant::now());
            let mut out = Vec::new();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let Some(&pos) = pos_map.get(&id) else {
                    cover_ok = false;
                    break;
                };
                if let Some(decoded) = decode_item_row(row, true, pos as usize, pool, gate)? {
                    out.push(decoded);
                }
            }
            if let (Some(prepare), Some(stepped)) = (timed, stepped) {
                source::timing::query(sql, prepare, stepped, Instant::now(), out.len());
            }
            Ok(cover_ok.then_some(out))
        })();
        if let Some(out) = two_pass.map_err(|e| sql_error(path, e))? {
            let count = out.len();
            return Ok((out, count));
        }
    }
    if let Some(scan_sql) = scan_sql {
        let res: rusqlite::Result<Vec<Row>> = (|| {
            let timed = source::timing::on().then(Instant::now);
            let mut stmt = db.prepare_cached(&scan_sql)?;
            let stepped = timed.map(|_| Instant::now());
            let mut out = Vec::new();
            let mut prev_owner: Option<String> = None;
            let mut cur_pos: usize = 0;
            let mut positions: HashMap<String, usize> = HashMap::new();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let owner_ref = sql_must_text(row, 1)?;
                if index == 2 && active.is_some_and(|set| !set.contains(owner_ref.as_ref())) {
                    continue;
                }
                let pos = if grouped {
                    if prev_owner.as_deref() == Some(owner_ref.as_ref()) {
                        cur_pos += 1;
                        cur_pos
                    } else {
                        cur_pos = 0;
                        prev_owner = Some(owner_ref.as_ref().to_owned());
                        0
                    }
                } else if let Some(next) = positions.get_mut(owner_ref.as_ref()) {
                    let p = *next;
                    *next += 1;
                    p
                } else {
                    positions.insert(owner_ref.as_ref().to_owned(), 1);
                    0
                };
                if index == 7 {
                    if let Some(decoded) = decode_item_row(row, inline, pos, pool, gate)? {
                        out.push(decoded);
                    }
                } else {
                    let id: i64 = row.get(0)?;
                    let payload = sql_payload(row, 2)?;
                    out.push(Row {
                        id,
                        owner: pool.intern(owner_ref.as_ref()),
                        data: Data::Subagent(Box::new(Payload { payload })),
                        position: pos,
                    });
                }
            }
            if let (Some(prepare), Some(stepped)) = (timed, stepped) {
                source::timing::query(&scan_sql, prepare, stepped, Instant::now(), out.len());
            }
            Ok(out)
        })();
        let out = res.map_err(|e| error(path, e))?;
        let count = out.len();
        return Ok((out, count));
    }
    // Fallback for unusual schemas: per-owner queries in SQLite encounter order.
    let ids = distinct_owners(db, TABLES[index]).map_err(|e| error(path, e))?;
    let mut count = ids.len();
    let owner_sql = owner_query(index, sql);
    let mut decoded = Vec::new();
    for id in ids {
        if index == 2 && active.is_some_and(|set| !set.contains(&id)) {
            continue;
        }
        let batch = if index == 7 {
            item_rows_scoped(db, &owner_sql, &[SqlValue::Text(id)], inline, pool, gate)
                .map_err(|e| error(path, e))?
        } else {
            load_rows_scoped(db, path, index, &owner_sql, &[SqlValue::Text(id)], pool, active, false)?
        };
        decoded.extend(batch);
    }
    count += decoded.len();
    let mut positions = BTreeMap::<Arc<str>, usize>::new();
    for row in &mut decoded {
        let next = positions.entry(Arc::clone(&row.owner)).or_default();
        row.position = *next;
        *next += 1;
    }
    Ok((decoded, count))
}

/// By-rowid payload keys (and text when `!lean`).
pub(super) fn load_item_payload(
    db: &Connection,
    path: &Path,
    ids: &[i64],
    lean: bool,
) -> Result<Vec<ItemPayloadRead>, FallbackReason> {
    use rusqlite::{params_from_iter, types::ValueRef};
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = if lean {
        item_keys_query(ids.len())
    } else {
        item_text_query(ids.len())
    };
    let timed = source::timing::on().then(Instant::now);
    let mut stmt = db.prepare_cached(&sql).map_err(|err| {
        let _ = (path, err);
        FallbackReason::TextDrift
    })?;
    let stepped = timed.map(|_| Instant::now());
    let args = ids.iter().copied().map(SqlValue::Integer);
    let mut rows = stmt.query(params_from_iter(args)).map_err(|_| FallbackReason::TextDrift)?;
    let mut out = Vec::with_capacity(ids.len());
    while let Some(row) = rows.next().map_err(|_| FallbackReason::TextDrift)? {
        let text = |i: usize| match row.get_ref(i).map_err(|_| FallbackReason::TextDrift)? {
            ValueRef::Null => Ok(None),
            ValueRef::Text(v) => Ok(Some(String::from_utf8_lossy(v).into_owned())),
            _ => Err(FallbackReason::TextDrift),
        };
        let must = |i: usize| text(i)?.ok_or(FallbackReason::TextDrift);
        let exit = match row.get_ref(10).map_err(|_| FallbackReason::TextDrift)? {
            ValueRef::Null => None,
            ValueRef::Integer(v) => {
                Some(i32::try_from(v).map_err(|_| FallbackReason::TextDrift)?)
            }
            _ => return Err(FallbackReason::TextDrift),
        };
        let (ordinal, item_text) = if lean {
            (row.get(11).map_err(|_| FallbackReason::TextDrift)?, None)
        } else {
            let steps = match row.get_ref(15).map_err(|_| FallbackReason::TextDrift)? {
                ValueRef::Text(v) => Some(Arc::<str>::from(String::from_utf8_lossy(v).as_ref())),
                ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Blob(_) => None,
            };
            let ordinal = row.get(14).map_err(|_| FallbackReason::TextDrift)?;
            (
                ordinal,
                Some(Text {
                    body: Body {
                        label: Arc::from(text(11)?.unwrap_or_default()),
                        command: Arc::from(text(12)?.unwrap_or_default()),
                        file: Arc::from(text(13)?.unwrap_or_default()),
                    },
                    steps,
                }),
            )
        };
        let updated = must(7)?;
        out.push(ItemPayloadRead {
            rowid: row.get(0).map_err(|_| FallbackReason::TextDrift)?,
            thread_id: must(1)?,
            run_id: text(2)?,
            node_id: text(3)?,
            provider_thread_id: text(4)?,
            kind: must(5)?,
            status: must(6)?,
            updated_at: Stamp::parse(&updated),
            started_at: text(8)?.map(|value| Stamp::parse(&value)),
            tool: text(9)?,
            exit_code: exit,
            ordinal,
            text: item_text,
        });
    }
    if let (Some(prepare), Some(stepped)) = (timed, stepped) {
        source::timing::query(&sql, prepare, stepped, Instant::now(), out.len());
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn read_pending_scoped(
    db: &Connection,
    path: &Path,
    owners: Option<&BTreeSet<String>>,
    pool: &mut Pool,
    lean: bool,
    inline_ok: bool,
    now: Option<f64>,
    idx_and_items: Option<(&Select, &BTreeMap<i64, Row>)>,
) -> Result<Pending, SourceError> {
    let mut tables: [Vec<Row>; 8] = Default::default();
    let mut count = 0usize;
    let req_fields = source::request_fields(db).map_err(|e| error(path, e))?;
    let q = queries();
    let req_sql = request_query(&req_fields);
    let mut active_owners = HashSet::new();
    let mut extra_active = HashSet::new();
    let mut extra_runs = HashSet::new();
    if let Some(owners) = owners {
        for index in 0..8 {
            let base = if index == 6 {
                req_sql.as_str()
            } else if index == 7 && inline_ok {
                ""
            } else {
                q[index].1.as_str()
            };
            let item_inline_base;
            let sql = if index == 7 && inline_ok {
                item_inline_base = item_inline_query();
                owner_query(index, &item_inline_base)
            } else {
                owner_query(index, base)
            };
            let mut decoded = Vec::new();
            if index == 7 {
                let gate = idx_and_items.map(|(idx, items)| ItemGate {
                    since: idx.since.clone(),
                    active: &idx.active,
                    extra_active: std::mem::take(&mut extra_active),
                    run_refs: &idx.run_refs,
                    extra_runs: std::mem::take(&mut extra_runs),
                    existing_items: Some(items),
                });
                for owner in owners {
                    let batch = item_rows_scoped(
                        db,
                        &sql,
                        &[SqlValue::Text(owner.clone())],
                        inline_ok,
                        pool,
                        gate.as_ref(),
                    )
                    .map_err(|e| error(path, e))?;
                    decoded.extend(batch);
                }
            } else {
                for owner in owners {
                    decoded.extend(load_rows_scoped(
                        db,
                        path,
                        index,
                        &sql,
                        &[SqlValue::Text(owner.clone())],
                        pool,
                        None,
                        lean,
                    )?);
                }
                let mut positions = BTreeMap::<Arc<str>, usize>::new();
                for row in &mut decoded {
                    let next = positions.entry(Arc::clone(&row.owner)).or_default();
                    row.position = *next;
                    *next += 1;
                }
                if idx_and_items.is_some() && index <= 2 {
                    for row in &decoded {
                        match &row.data {
                            Data::Thread(t) if thread_active(t) => {
                                extra_active.insert(row.owner.to_string());
                                if let Some(id) = t.data.payload.get("id").and_then(Value::as_str) {
                                    extra_active.insert(id.to_owned());
                                }
                            }
                            Data::Run(r) => {
                                if let Some(id) = r.data.payload.get("id").and_then(Value::as_str) {
                                    if !id.is_empty() {
                                        extra_runs.insert(id.to_owned());
                                    }
                                }
                            }
                            Data::Subagent(s) => {
                                if let Some(id) = s.payload.get("runId").and_then(Value::as_str) {
                                    if !id.is_empty() {
                                        extra_runs.insert(id.to_owned());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            count += decoded.len();
            tables[index] = decoded;
        }
    } else {
        // Step 1: threads (active only), runs, subagents -> builds eligibility gate.
        let threads_sql = format!("{} WHERE deleted_at IS NULL AND archived_at IS NULL", q[0].1);
        tables[0] = thread_rows(db, &threads_sql, &[], pool).map_err(|e| error(path, e))?;
        for row in &tables[0] {
            active_owners.insert(row.owner.to_string());
            if let Data::Thread(t) = &row.data {
                if let Some(id) = t.data.payload.get("id").and_then(Value::as_str) {
                    if id != row.owner.as_ref() {
                        active_owners.insert(id.to_owned());
                    }
                }
            }
        }
        count += tables[0].len();
        // Retain all run rows (including archived/inactive owners) so Cache::stage's
        // missing-run recovery still finds runs moved out of an archived thread without
        // an event on the destination thread.
        tables[1] = run_rows(db, &q[1].1, &[], pool, None).map_err(|e| error(path, e))?;
        count += tables[1].len();
        let (subs, sub_count) = owner_positions_from_index(
            db,
            path,
            2,
            &q[2].1,
            false,
            pool,
            Some(&active_owners),
            None,
        )?;
        tables[2] = subs;
        count += sub_count;
        // Step 2: provider_threads, provider_turns, nodes, open requests.
        tables[3] = payload_rows(db, 3, &q[3].1, &[], pool, Some(&active_owners))
            .map_err(|e| error(path, e))?;
        count += tables[3].len();
        tables[4] = turn_rows(db, &q[4].1, &[], pool, Some(&active_owners), lean)
            .map_err(|e| error(path, e))?;
        count += tables[4].len();
        tables[5] = match nodes_from_covering_indexes(db, &active_owners, pool)
            .map_err(|e| error(path, e))?
        {
            Some(rows) => rows,
            None => {
                let mut rows = node_rows(db, &q[5].1, &[], pool).map_err(|e| error(path, e))?;
                rows.retain(|r| active_owners.contains(r.owner.as_ref()));
                rows
            }
        };
        count += tables[5].len();
        let req_distinct = format!(
            "SELECT DISTINCT thread_id FROM orchestration_v2_projection_runtime_requests WHERE {REQUEST_FILTER}"
        );
        let mut req_ids = Vec::new();
        {
            let mut stmt = db.prepare_cached(&req_distinct).map_err(|e| error(path, e))?;
            let mut rows = stmt.query([]).map_err(|e| error(path, e))?;
            while let Some(row) = rows.next().map_err(|e| error(path, e))? {
                let id: String = row.get(0).map_err(|e| error(path, e))?;
                if active_owners.contains(&id) {
                    req_ids.push(id);
                }
            }
        }
        count += req_ids.len();
        let req_owner_sql = owner_query(6, &req_sql);
        let mut req_decoded = Vec::new();
        for id in req_ids {
            req_decoded.extend(
                request_rows(db, &req_owner_sql, &[SqlValue::Text(id)], pool)
                    .map_err(|e| error(path, e))?,
            );
        }
        let mut req_pos = BTreeMap::<Arc<str>, usize>::new();
        for row in &mut req_decoded {
            let next = req_pos.entry(Arc::clone(&row.owner)).or_default();
            row.position = *next;
            *next += 1;
        }
        count += req_decoded.len();
        tables[6] = req_decoded;
        // Step 3: turn_items inline scan, early-filtered against the fresh eligibility gate.
        let elig = now.map(|at| {
            (
                eligibility_rows(tables[0].iter(), tables[1].iter(), tables[2].iter()),
                source::iso(at - source::WINDOW),
            )
        });
        let empty_set = HashSet::new();
        let gate = elig.as_ref().map(|(e, since)| ItemGate {
            since: since.clone(),
            active: &e.active,
            extra_active: empty_set.clone(),
            run_refs: &e.run_refs,
            extra_runs: empty_set.clone(),
            existing_items: None,
        });
        let item_sql = if inline_ok {
            item_inline_query()
        } else {
            q[7].1.clone()
        };
        let (items, item_count) = owner_positions_from_index(
            db,
            path,
            7,
            &item_sql,
            inline_ok,
            pool,
            None,
            gate.as_ref(),
        )?;
        tables[7] = items;
        count += item_count;
    }
    let (projects, proj_count) = project_rows(db).map_err(|e| error(path, e))?;
    count += proj_count;
    Ok(Pending {
        full: owners.is_none(),
        owners: owners.cloned().unwrap_or_default(),
        tables: tables.into(),
        projects,
        rows: count,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_canonical_utc_ms;
    use crate::{model, source};

    /// What `Stamp::parse` required before this parser: the text is the 24 bytes `source::iso`
    /// prints for the instant `model::ms` reads from it.
    fn prints_back(text: &str) -> Option<i64> {
        if text.len() != 24 {
            return None;
        }
        let ms = model::ms(Some(text))?;
        (source::iso(ms) == text).then_some(ms.floor() as i64)
    }

    /// A stamp kept as milliseconds is printed again with `source::iso` and timed with its
    /// integer. The parser may therefore accept only text that prints back unchanged, and must
    /// give the instant `model::ms` gives.
    #[test]
    fn canonical_stamp_parser_accepts_only_text_that_prints_back() {
        let first = -62_167_219_200_000i64; // 0000-01-01T00:00:00.000Z
        let last = 253_402_300_799_999i64; // 9999-12-31T23:59:59.999Z
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut instants = vec![
            first,
            last,
            -1,
            0,
            1,
            999,
            951_782_400_000,   // 2000-02-29
            4_107_542_399_999, // 2100-02-28T23:59:59.999Z, not a leap year
            1_795_163_100_000,
        ];
        for _ in 0..20_000 {
            instants.push(first + (next() % (last - first + 1) as u64) as i64);
        }
        let alphabet = b"0123456789-:.TZtz +";
        for ms in instants {
            let text = source::iso(ms as f64);
            assert_eq!(parse_canonical_utc_ms(&text), Some(ms), "{text}");
            assert_eq!(prints_back(&text), Some(ms), "{text}");
            // One or two bytes changed: usually no longer canonical, sometimes another instant.
            let mut bytes = text.into_bytes();
            for _ in 0..1 + next() % 2 {
                let at = (next() % 24) as usize;
                bytes[at] = alphabet[(next() % alphabet.len() as u64) as usize];
            }
            let changed = String::from_utf8(bytes).unwrap();
            assert_eq!(parse_canonical_utc_ms(&changed), prints_back(&changed), "{changed}");
        }
        for text in [
            "2026-02-29T00:00:00.000Z",
            "2026-04-31T00:00:00.000Z",
            "2026-11-12T24:00:00.000Z",
            "2026-11-12T10:60:00.000Z",
            "2026-11-12T10:00:60.000Z",
            "2026-13-01T00:00:00.000Z",
            "2026-00-10T00:00:00.000Z",
            "2026-11-00T00:00:00.000Z",
            "2026-11-12T10:00:00.000z",
            "2026-11-12 10:00:00.000Z",
            "2026-11-12T10:00:00.00Z",
            "2026-11-12T10:00:00.0000Z",
            "2026-11-12T10:00:00+00:00",
            "+002026-11-12T10:00:00.000Z",
            "",
        ] {
            assert_eq!(parse_canonical_utc_ms(text), None, "{text}");
            assert_eq!(prints_back(text), None, "{text}");
        }
    }
}
