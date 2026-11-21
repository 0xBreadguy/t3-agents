//! Single-pass leaf-only `turn_items` scan + forward-sorted rowid payload fetch
//! for bounded items (LIMIT 3000), events (LIMIT 8000 within 3h window), plans
//! (`todo_list`), and the `AsstItemIndex` hint map.
//!
//! The read is in stages (`scan`, `rank`, `fetch_chunk`, `build`, `finish`) so that
//! `fan` can run the scan as several row-id ranges and the fetch as several chunks
//! on connections that hold one snapshot. `read_turn_items_combined` runs the same
//! stages in order on one connection.
//!
//! Deliberate difference from `oracle/` (Decision 1): displaced rows outside
//! the 3,000-item / 8,000-event cut never have their `payload_json` parsed,
//! both on the primary leaf-scan path and on the two-phase key-only tie
//! fallback. In-cut rows still validate `payload_json` identically to `oracle/`.
use super::{
    ITEM_EXIT_CODE, PREFIX, SourceError, TOOLISH,
    asks_hint::{AsstItemIndex, AsstSlot, TidIndex},
    db_error, each_row_cached, each_row_uncached,
    fast_reads::{
        FetchedTurnItem, FnvHasher, PlanRow, RowidHasher, is_rowid_alias, ordered_columns,
        rowid_unshadowed, sql_opt_text, todo_lists,
    },
    item_fields, marks, optional, plan, str_at, strings, timing,
};
use crate::model::{self, Item};
use rusqlite::{
    Connection,
    types::{Value as SqlValue, ValueRef},
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

/// True when `scan_sql` walks `turn_items` by rowid without following `payload_json`
/// overflow pages. SQLite stores columns in `cid` order and `OP_Column` (`sqlite3.c`)
/// stops at the highest requested column, so trailing columns added after `payload_json`
/// (such as T3 migration 095's `inline_work_output`) never force `payload_json` reads
/// as long as every scanned column precedes `payload_json`.
pub(super) fn leaf_scan_safe(db: &Connection) -> rusqlite::Result<bool> {
    let table = format!("{PREFIX}turn_items");
    let cols = ordered_columns(db, &table)?;
    if cols.iter().any(|c| is_rowid_alias(c)) {
        return Ok(false);
    }
    let Some(payload_idx) = cols.iter().position(|c| c == "payload_json") else {
        return Ok(false);
    };
    for scanned in [
        "thread_id",
        "run_id",
        "node_id",
        "type",
        "updated_at",
        "ordinal",
    ] {
        if !cols.iter().position(|c| c == scanned).is_some_and(|i| i < payload_idx) {
            return Ok(false);
        }
    }
    for fetched_only in ["provider_thread_id", "status"] {
        if !cols.iter().any(|c| c == fetched_only) {
            return Ok(false);
        }
    }
    let scan_sql =
        format!("SELECT rowid, thread_id, run_id, node_id, type, updated_at, ordinal FROM {table}");
    let steps = plan(db, &scan_sql, &[])?;
    Ok(steps.len() == 1 && steps[0].1 == format!("SCAN {table}"))
}

#[derive(Clone, Eq, PartialEq)]
pub(super) enum CompactStamp {
    Null,
    Iso24([u8; 24]),
    Other(Box<[u8]>),
}

impl CompactStamp {
    #[inline]
    fn from_bytes(b: &[u8]) -> Self {
        if b.len() == 24 {
            let mut arr = [0u8; 24];
            arr.copy_from_slice(b);
            Self::Iso24(arr)
        } else {
            Self::Other(b.to_vec().into_boxed_slice())
        }
    }
    #[inline]
    fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Null => None,
            Self::Iso24(a) => Some(a.as_slice()),
            Self::Other(b) => Some(b),
        }
    }
}

impl Ord for CompactStamp {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_bytes().cmp(&other.as_bytes())
    }
}
impl PartialOrd for CompactStamp {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[inline]
fn is_toolish_bytes(kind: &[u8]) -> bool {
    matches!(
        kind,
        b"command_execution"
            | b"dynamic_tool"
            | b"file_change"
            | b"web_search"
            | b"subagent"
            | b"handoff"
            | b"notification"
            | b"user_input_request"
            | b"secret_request"
            | b"link_request"
            | b"reasoning"
            | b"assistant_message"
    )
}

#[derive(Eq, PartialEq)]
pub(super) struct ItemRankEntry {
    updated_at: CompactStamp,
    ordinal: Option<i64>,
    /// Index into `run_ids` for non-null `run_id`, or `u32::MAX` for `run_id IS NULL`.
    /// Because `attach_item` matches `item.run_id == agent.run.id`, two items with
    /// distinct non-null `run_id`s can never land in the same agent's `items` slice.
    owner_key: u32,
    rowid: i64,
}

#[derive(Eq, PartialEq)]
pub(super) struct EventRankEntry {
    updated_at: CompactStamp,
    rowid: i64,
}

fn events_ties_safe(events: &[FetchedTurnItem]) -> bool {
    let mut i = 0;
    while i < events.len() {
        let mut j = i + 1;
        while j < events.len() && events[j].updated_at == events[i].updated_at {
            j += 1;
        }
        if j - i > 1 {
            // Assembly stably sorts events by their displayed time and keeps
            // recent failures in query order. A tie matters only when it can
            // reorder distinguishable entries in either list.
            let mut failed = false;
            let mut by_time = HashMap::new();
            for event in &events[i..j] {
                let is_failed = event.status.as_deref() == Some("failed");
                let time = model::ms(if is_failed {
                    event.updated_at.as_deref()
                } else {
                    event.started_at.as_deref()
                })
                .or_else(|| model::ms(event.updated_at.as_deref()));
                let Some(time) = time else { continue };
                if is_failed && std::mem::replace(&mut failed, true) {
                    return false;
                }
                let category =
                    model::category(event.kind.as_deref().unwrap_or(""), event.tool.as_deref());
                let signature = (is_failed, category);
                if by_time
                    .insert(time.to_bits(), signature)
                    .is_some_and(|old| old != signature)
                {
                    return false;
                }
            }
        }
        i = j;
    }
    true
}

fn normalize_plan(steps: Vec<(Option<usize>, String)>) -> Vec<(Option<usize>, String)> {
    steps
        .into_iter()
        .map(|(p, detail)| (p, detail.replace("USING COVERING INDEX ", "USING INDEX ")))
        .collect()
}

/// Two-phase tie fallback for bounded `turn_items` queries: runs the exact same
/// `WHERE ... ORDER BY ... LIMIT N` without extracting JSON, then fetches `item_fields()`
/// only for the winning rowids in monotonic rowid order and re-orders them to
/// SQLite's exact output order.
///
/// Matching plans keep the visit order. Try a key-only sort first, then a
/// projection that references the same columns as the reference query so
/// covering indexes remain eligible in the same way.
fn fetch_by_rowid_two_phase(
    db: &Connection,
    path: &Path,
    tail: &str,
    oracle_sql: &str,
    args: &[SqlValue],
) -> Result<Option<Vec<FetchedTurnItem>>, SourceError> {
    let table = format!("{PREFIX}turn_items");
    if !rowid_unshadowed(db, &table).unwrap_or(false) {
        return Ok(None);
    }
    let oracle_plan = match plan(db, oracle_sql, args) {
        Ok(p) => normalize_plan(p),
        Err(_) => return Ok(None),
    };
    let projections = [
        "rowid",
        "rowid,thread_id,run_id,node_id,provider_thread_id,type,status,updated_at,typeof(payload_json)",
    ];
    let key_sql = projections
        .iter()
        .map(|fields| format!("SELECT {fields} {tail}"))
        .find(|sql| plan(db, sql, args).is_ok_and(|p| normalize_plan(p) == oracle_plan));
    let Some(key_sql) = key_sql else {
        return Ok(None);
    };

    let mut ordered_rids: Vec<i64> = Vec::new();
    let mut usable = true;
    each_row_cached(db, &key_sql, args, |row| {
        match row.get_ref(0)? {
            ValueRef::Integer(rid) => ordered_rids.push(rid),
            _ => usable = false,
        }
        Ok(())
    })
    .map_err(|e| db_error(path, e))?;
    if !usable {
        return Ok(None);
    }
    if ordered_rids.is_empty() {
        return Ok(Some(Vec::new()));
    }

    let mut sorted_rids = ordered_rids.clone();
    sorted_rids.sort_unstable();
    sorted_rids.dedup();

    let fields = item_fields();
    let mut by_rid: HashMap<i64, FetchedTurnItem, std::hash::BuildHasherDefault<RowidHasher>> =
        HashMap::with_capacity_and_hasher(sorted_rids.len(), Default::default());
    for chunk in sorted_rids.chunks(900) {
        let fetch_sql = format!(
            "SELECT rowid, {fields} FROM {table} WHERE rowid IN ({}) -- ti_fetch",
            marks(chunk.len())
        );
        let fetch_args: Vec<SqlValue> = chunk.iter().copied().map(SqlValue::Integer).collect();
        each_row_uncached(db, &fetch_sql, &fetch_args, |row| {
            if let ValueRef::Integer(rid) = row.get_ref(0)? {
                let item = FetchedTurnItem::from_row(row, 1, false)?;
                by_rid.insert(rid, item);
            }
            Ok(())
        })
        .map_err(|e| db_error(path, e))?;
    }
    if by_rid.len() != sorted_rids.len() {
        return Ok(None);
    }

    let mut out = Vec::with_capacity(ordered_rids.len());
    for rid in ordered_rids {
        if let Some(row) = by_rid.remove(&rid) {
            out.push(row);
        } else {
            return Ok(None);
        }
    }
    Ok(Some(out))
}

type Fnv = std::hash::BuildHasherDefault<FnvHasher>;

/// What a scan sorts rows by: the active threads, the selected runs, the threads with no
/// run, and the start of the event window. Thread and run positions are those in `ids` and
/// `run_ids`, so every connection that scans a part of the table numbers them alike.
pub(super) struct ScanKeys<'a> {
    active: HashMap<&'a [u8], u32, Fnv>,
    runs: HashMap<&'a [u8], u32, Fnv>,
    runless: HashSet<&'a [u8], Fnv>,
    since: &'a [u8],
}

impl<'a> ScanKeys<'a> {
    pub(super) fn new(
        ids: &'a [String],
        run_ids: &'a [String],
        runless: &'a [String],
        since: &'a str,
    ) -> Self {
        let mut active: HashMap<&[u8], u32, Fnv> =
            HashMap::with_capacity_and_hasher(ids.len(), Default::default());
        for (idx, tid) in ids.iter().enumerate() {
            active.insert(tid.as_bytes(), idx as u32);
        }
        let mut runs: HashMap<&[u8], u32, Fnv> =
            HashMap::with_capacity_and_hasher(run_ids.len(), Default::default());
        for (idx, r) in run_ids.iter().enumerate() {
            runs.insert(r.as_bytes(), idx as u32);
        }
        let mut runless_set: HashSet<&[u8], Fnv> =
            HashSet::with_capacity_and_hasher(runless.len(), Default::default());
        for t in runless {
            runless_set.insert(t.as_bytes());
        }
        Self {
            active,
            runs,
            runless: runless_set,
            since: since.as_bytes(),
        }
    }
}

/// The thread positions `ScanKeys::new` gives, owned, for the `AsstItemIndex`.
pub(super) fn tid_index(ids: &[String]) -> TidIndex {
    let mut out: TidIndex = HashMap::with_capacity_and_hasher(ids.len(), Default::default());
    for (idx, tid) in ids.iter().enumerate() {
        out.insert(tid.as_bytes().to_vec().into_boxed_slice(), idx as u32);
    }
    out
}

/// A part of the table by row id. The parts `scan_ranges` returns cover every row once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Range {
    All,
    /// `rowid < .0`
    Below(i64),
    /// `.0 <= rowid < .1`
    Between(i64, i64),
    /// `rowid >= .0`
    From(i64),
}

/// The table cut into `parts` ranges of about as many row ids each, for scans on several
/// connections that hold one snapshot. One range when the table is small or SQLite would
/// not walk a range by row id. The caller has checked `leaf_scan_safe`.
pub(super) fn scan_ranges(db: &Connection, parts: usize) -> Vec<Range> {
    let table = format!("{PREFIX}turn_items");
    let edge = |order: &str| -> Option<i64> {
        let sql = format!("SELECT rowid FROM {table} ORDER BY rowid {order} LIMIT 1");
        let mut found = None;
        each_row_cached(db, &sql, &[], |row| {
            if let ValueRef::Integer(id) = row.get_ref(0)? {
                found = Some(id);
            }
            Ok(())
        })
        .ok()?;
        found
    };
    let (Some(low), Some(high)) = (edge("ASC"), edge("DESC")) else {
        return vec![Range::All];
    };
    let span = (high as i128) - (low as i128) + 1;
    if parts < 2 || span < (parts as i128) * 64 {
        return vec![Range::All];
    }
    let cut = |k: usize| (low as i128 + span * k as i128 / parts as i128) as i64;
    let mut ranges = Vec::with_capacity(parts);
    ranges.push(Range::Below(cut(1)));
    for k in 1..parts - 1 {
        ranges.push(Range::Between(cut(k), cut(k + 1)));
    }
    ranges.push(Range::From(cut(parts - 1)));
    // A range has to be a walk of the table by row id, as the whole scan is.
    let by_rowid = format!("SEARCH {table} USING INTEGER PRIMARY KEY (rowid");
    for range in [
        ranges[0],
        ranges[ranges.len() - 1],
        Range::Between(low, high),
    ] {
        let (sql, args) = scan_sql(range);
        match plan(db, &sql, &args) {
            Ok(steps) if steps.len() == 1 && steps[0].1.starts_with(&by_rowid) => {}
            _ => return vec![Range::All],
        }
    }
    ranges
}

fn scan_sql(range: Range) -> (String, Vec<SqlValue>) {
    let table = format!("{PREFIX}turn_items");
    let (filter, args) = match range {
        Range::All => ("", Vec::new()),
        Range::Below(end) => (" WHERE rowid<?1", vec![SqlValue::Integer(end)]),
        Range::Between(start, end) => (
            " WHERE rowid>=?1 AND rowid<?2",
            vec![SqlValue::Integer(start), SqlValue::Integer(end)],
        ),
        Range::From(start) => (" WHERE rowid>=?1", vec![SqlValue::Integer(start)]),
    };
    (
        format!(
            "SELECT rowid, thread_id, run_id, node_id, type, updated_at, ordinal
             FROM {table}{filter} -- ti_leaf_scan"
        ),
        args,
    )
}

/// What one scan found: every candidate row's sort keys, by list. Nothing here depends on
/// the order rows were visited in, so the parts of a table can be added together.
pub(super) struct Scanned {
    items: Vec<ItemRankEntry>,
    events: Vec<EventRankEntry>,
    plans: Vec<(CompactStamp, Box<[u8]>, i64)>,
    asst: HashMap<Box<[u8]>, AsstSlot, Fnv>,
    /// A value the scan does not know how to order: the lists below are not used at all.
    usable: bool,
    items_ok: bool,
    asst_ok: bool,
}

impl Scanned {
    fn new() -> Self {
        // Flat Vec + O(N) select_nth_unstable avoids O(N log K) heap sift-downs on
        // chronologically ordered rowids and keeps 24-byte ISO timestamps inline.
        Self {
            items: Vec::with_capacity(4096),
            events: Vec::with_capacity(256),
            plans: Vec::new(),
            asst: HashMap::with_capacity_and_hasher(4096, Default::default()),
            usable: true,
            items_ok: true,
            asst_ok: true,
        }
    }

    /// Add another part of the same table, scanned with the same keys.
    pub(super) fn absorb(&mut self, other: Scanned) {
        self.usable &= other.usable;
        self.items_ok &= other.items_ok;
        self.asst_ok &= other.asst_ok;
        self.items.extend(other.items);
        self.events.extend(other.events);
        self.plans.extend(other.plans);
        for (node, theirs) in other.asst {
            match self.asst.entry(node) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(theirs);
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    let mine = slot.get_mut();
                    let mut all = match std::mem::replace(mine, AsstSlot::Many(Vec::new())) {
                        AsstSlot::One { tid, ord, rowid } => vec![(tid, ord, rowid)],
                        AsstSlot::Many(entries) => entries,
                    };
                    match theirs {
                        AsstSlot::One { tid, ord, rowid } => all.push((tid, ord, rowid)),
                        AsstSlot::Many(entries) => all.extend(entries),
                    }
                    *mine = AsstSlot::Many(all);
                }
            }
        }
    }
}

/// One pass over the rows of `range`, reading only what the table's leaf pages hold.
pub(super) fn scan(
    db: &Connection,
    keys: &ScanKeys<'_>,
    range: Range,
) -> rusqlite::Result<Scanned> {
    let mut out = Scanned::new();
    let mut scan_usable = true;
    let (scan_sql, args) = scan_sql(range);
    each_row_cached(db, &scan_sql, &args, |row| {
        if !scan_usable {
            return Ok(());
        }
        let typ = match row.get_ref(4)? {
            ValueRef::Text(b) => b,
            ValueRef::Null => return Ok(()),
            _ => {
                scan_usable = false;
                return Ok(());
            }
        };
        if typ == b"checkpoint" {
            return Ok(());
        }
        let rid = match row.get_ref(0)? {
            ValueRef::Integer(id) => id,
            _ => {
                scan_usable = false;
                return Ok(());
            }
        };

        if typ == b"todo_list" {
            match row.get_ref(2)? {
                ValueRef::Text(r) => {
                    if keys.runs.contains_key(r) {
                        let ua = match row.get_ref(5)? {
                            ValueRef::Text(b) => CompactStamp::from_bytes(b),
                            ValueRef::Null => CompactStamp::Null,
                            _ => {
                                scan_usable = false;
                                return Ok(());
                            }
                        };
                        out.plans.push((ua, r.to_vec().into_boxed_slice(), rid));
                    }
                }
                ValueRef::Null => {}
                _ => scan_usable = false,
            }
            return Ok(());
        }

        let ua = match row.get_ref(5)? {
            ValueRef::Text(b) => Some(b),
            ValueRef::Null => None,
            _ => {
                scan_usable = false;
                return Ok(());
            }
        };

        // Check the time window before looking up the event's thread. `get_tid` never sets
        // `scan_usable` itself so each list (`events`, `asst`, `items`) only taints its own
        // guard, keeping `Scanned::absorb` identical across any rowid partition.
        #[derive(Clone, Copy)]
        enum Tid<'a> {
            Text(&'a [u8]),
            Null,
            Other,
        }
        let mut cached_tid: Option<Tid<'_>> = None;
        let mut get_tid = || -> rusqlite::Result<Tid<'_>> {
            if let Some(t) = cached_tid {
                return Ok(t);
            }
            let t = match row.get_ref(1)? {
                ValueRef::Text(b) => Tid::Text(b),
                ValueRef::Null => Tid::Null,
                _ => Tid::Other,
            };
            cached_tid = Some(t);
            Ok(t)
        };

        {
            if let Some(u) = ua {
                if u >= keys.since {
                    match get_tid()? {
                        Tid::Text(t) => {
                            if keys.active.contains_key(t) {
                                out.events.push(EventRankEntry {
                                    updated_at: CompactStamp::from_bytes(u),
                                    rowid: rid,
                                });
                            }
                        }
                        Tid::Null => {}
                        Tid::Other => scan_usable = false,
                    }
                }
            }
        }

        if is_toolish_bytes(typ) {
            if out.asst_ok && typ == b"assistant_message" {
                if rid <= 0 {
                    // Non-positive rowids could collide with t3_asst_hint sentinels 0 / -1.
                    out.asst_ok = false;
                } else {
                    match get_tid()? {
                        Tid::Text(t) => {
                            if let Some(&tid_idx) = keys.active.get(t) {
                                match row.get_ref(3)? {
                                    ValueRef::Text(nid) => {
                                        let ord = match row.get_ref(6)? {
                                            ValueRef::Integer(o) => Some(Some(o)),
                                            ValueRef::Null => Some(None),
                                            _ => {
                                                out.asst_ok = false;
                                                None
                                            }
                                        };
                                        if let Some(ord) = ord {
                                            if let Some(slot) = out.asst.get_mut(nid) {
                                                match slot {
                                                    AsstSlot::One {
                                                        tid,
                                                        ord: o0,
                                                        rowid: r0,
                                                    } => {
                                                        let v = vec![
                                                            (*tid, *o0, *r0),
                                                            (tid_idx, ord, rid),
                                                        ];
                                                        *slot = AsstSlot::Many(v);
                                                    }
                                                    AsstSlot::Many(v) => {
                                                        v.push((tid_idx, ord, rid))
                                                    }
                                                }
                                            } else {
                                                out.asst.insert(
                                                    nid.to_vec().into_boxed_slice(),
                                                    AsstSlot::One {
                                                        tid: tid_idx,
                                                        ord,
                                                        rowid: rid,
                                                    },
                                                );
                                            }
                                        }
                                    }
                                    ValueRef::Null => {}
                                    _ => out.asst_ok = false,
                                }
                            }
                        }
                        Tid::Null => {}
                        Tid::Other => out.asst_ok = false,
                    }
                }
            }

            if out.items_ok {
                let owner_key = match row.get_ref(2)? {
                    ValueRef::Text(r) => keys.runs.get(r).copied(),
                    ValueRef::Null => {
                        if !keys.runless.is_empty() {
                            match get_tid()? {
                                Tid::Text(t) if keys.runless.contains(t) => Some(u32::MAX),
                                Tid::Text(_) | Tid::Null => None,
                                Tid::Other => {
                                    out.items_ok = false;
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    }
                    _ => {
                        out.items_ok = false;
                        None
                    }
                };
                if let Some(owner_key) = owner_key {
                    let ord = match row.get_ref(6)? {
                        ValueRef::Integer(v) => Some(v),
                        ValueRef::Null => None,
                        _ => {
                            out.items_ok = false;
                            None
                        }
                    };
                    if out.items_ok {
                        out.items.push(ItemRankEntry {
                            updated_at: match ua {
                                Some(b) => CompactStamp::from_bytes(b),
                                None => CompactStamp::Null,
                            },
                            ordinal: ord,
                            owner_key,
                            rowid: rid,
                        });
                    }
                }
            }
        }
        Ok(())
    })?;
    out.usable = scan_usable;
    Ok(out)
}

/// The rows that make the item, event and plan lists, by row id; `None` where the scan
/// cannot decide a list and the reference query has to.
#[derive(Default)]
pub(super) struct Ranked {
    items: Option<Vec<ItemRankEntry>>,
    events: Option<Vec<EventRankEntry>>,
    plans: Option<Vec<(CompactStamp, Box<[u8]>, i64)>>,
    pub(super) asst: Option<AsstItemIndex>,
    /// Every row to fetch, ascending.
    pub(super) rowids: Vec<i64>,
}

#[cfg(test)]
impl Ranked {
    pub(super) fn decided(&self) -> (bool, bool, bool, bool) {
        (
            self.items.is_some(),
            self.events.is_some(),
            self.plans.is_some(),
            self.asst.is_some(),
        )
    }
}

pub(super) fn rank(scanned: Scanned, tid_to_idx: TidIndex) -> Ranked {
    let Scanned {
        items: mut item_cands,
        events: mut event_cands,
        plans: mut plan_cands,
        asst: mut asst_by_node,
        usable,
        mut items_ok,
        asst_ok,
    } = scanned;
    if !usable {
        return Ranked::default();
    }
    let mut events_ok = true;
    let mut plans_ok = true;
    let asst = asst_ok.then(|| {
        for slot in asst_by_node.values_mut() {
            if let AsstSlot::Many(vec) = slot {
                vec.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.2.cmp(&a.2)));
            }
        }
        AsstItemIndex {
            tid_to_idx,
            by_node: asst_by_node,
        }
    });

    // Finalize ranked winning rowids + tie-break safety checks.
    let item_winners: Vec<ItemRankEntry> = if items_ok {
        let cmp = |a: &ItemRankEntry, b: &ItemRankEntry| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.ordinal.cmp(&a.ordinal))
                .then_with(|| a.owner_key.cmp(&b.owner_key))
                .then_with(|| b.rowid.cmp(&a.rowid))
        };
        if item_cands.len() > 3001 {
            item_cands.select_nth_unstable_by(3000, cmp);
            item_cands.truncate(3001);
        }
        item_cands.sort_unstable_by(cmp);
        // Cut-boundary tie: SQLite's index visit order decides which rows make the top 3000.
        if item_cands.len() > 3000
            && item_cands[2999].updated_at == item_cands[3000].updated_at
            && item_cands[2999].ordinal == item_cands[3000].ordinal
        {
            items_ok = false;
        }
        item_cands.truncate(3000);
        // Internal tie only matters when two tied rows share an owner (`run_id`, or both
        // `run_id IS NULL`), since `attach_item` partitions items strictly by `run_id`.
        // Sorting by `(updated_at DESC, ordinal DESC, owner_key ASC)` places any same-owner
        // ties adjacently.
        if items_ok
            && item_cands.windows(2).any(|w| {
                w[0].updated_at == w[1].updated_at
                    && w[0].ordinal == w[1].ordinal
                    && w[0].owner_key == w[1].owner_key
            })
        {
            items_ok = false;
        }
        if items_ok { item_cands } else { Vec::new() }
    } else {
        Vec::new()
    };

    let event_winners: Vec<EventRankEntry> = if events_ok {
        let cmp = |a: &EventRankEntry, b: &EventRankEntry| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.rowid.cmp(&a.rowid))
        };
        if event_cands.len() > 8001 {
            event_cands.select_nth_unstable_by(8000, cmp);
            event_cands.truncate(8001);
        }
        event_cands.sort_unstable_by(cmp);
        if event_cands.len() > 8000 && event_cands[7999].updated_at == event_cands[8000].updated_at
        {
            events_ok = false;
        }
        event_cands.truncate(8000);
        if events_ok { event_cands } else { Vec::new() }
    } else {
        Vec::new()
    };

    plan_cands.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.2.cmp(&a.2)));
    {
        let mut seen_run_ts: HashSet<(&[u8], Option<&[u8]>)> =
            HashSet::with_capacity(plan_cands.len());
        for (ua, rid_bytes, _) in &plan_cands {
            if !seen_run_ts.insert((rid_bytes.as_ref(), ua.as_bytes())) {
                plans_ok = false;
                break;
            }
        }
    }
    if !plans_ok {
        plan_cands.clear();
    }

    // Collect union of winning rowids and fetch in monotonic rowid order.
    let mut union_rids: Vec<i64> =
        Vec::with_capacity(item_winners.len() + event_winners.len() + plan_cands.len());
    for w in &item_winners {
        union_rids.push(w.rowid);
    }
    for w in &event_winners {
        union_rids.push(w.rowid);
    }
    for &(_, _, rid) in &plan_cands {
        union_rids.push(rid);
    }
    union_rids.sort_unstable();
    union_rids.dedup();

    Ranked {
        items: items_ok.then_some(item_winners),
        events: events_ok.then_some(event_winners),
        plans: plans_ok.then_some(plan_cands),
        asst,
        rowids: union_rids,
    }
}

pub(super) type Fetched = HashMap<i64, FetchedTurnItem, std::hash::BuildHasherDefault<RowidHasher>>;

/// The display fields of up to 900 rows, by row id.
pub(super) fn fetch_chunk(
    db: &Connection,
    path: &Path,
    chunk: &[i64],
    mut found: impl FnMut(i64, FetchedTurnItem),
) -> Result<(), SourceError> {
    let table = format!("{PREFIX}turn_items");
    let fetch_sql = format!(
        "SELECT rowid,thread_id,run_id,node_id,provider_thread_id,type,status,updated_at,
         CASE WHEN type<>'todo_list' THEN json_extract(payload_json,'$.startedAt') END AS started_at,
         CASE WHEN type NOT IN ('secret_request','link_request','todo_list') THEN json_extract(payload_json,'$.toolName') END AS tool,
         {ITEM_EXIT_CODE},
         CASE WHEN type IN ('secret_request','link_request','todo_list') THEN ''
         ELSE substr(COALESCE(json_extract(payload_json,'$.title'),json_extract(payload_json,'$.toolName'),json_extract(payload_json,'$.label'),''),1,240) END AS label,
         CASE WHEN type='command_execution' THEN substr(json_extract(payload_json,'$.input'),1,240) ELSE '' END AS command,
         CASE WHEN type='file_change' THEN substr(json_extract(payload_json,'$.fileName'),1,240) ELSE '' END AS file,
         CASE WHEN type='todo_list' THEN json_extract(payload_json,'$.steps') END AS steps
         FROM {table} WHERE rowid IN ({}) -- ti_fetch",
        marks(chunk.len())
    );
    let args: Vec<SqlValue> = chunk.iter().copied().map(SqlValue::Integer).collect();
    each_row_uncached(db, &fetch_sql, &args, |row| {
        if let ValueRef::Integer(rid) = row.get_ref(0)? {
            found(rid, FetchedTurnItem::from_row(row, 1, true)?);
        }
        Ok(())
    })
    .map_err(|e| db_error(path, e))?;
    Ok(())
}

/// The lists the scan decided, and how many rows they hold.
#[derive(Default)]
pub(super) struct Built {
    items: Option<Vec<Item>>,
    events: Option<Vec<FetchedTurnItem>>,
    plans: Option<Vec<PlanRow>>,
    rows: usize,
}

pub(super) fn build(ranked: &Ranked, fetched: &Fetched, path: &Path) -> Result<Built, SourceError> {
    let mut out = Built::default();
    if let Some(winners) = &ranked.items {
        let mut built = Vec::with_capacity(winners.len());
        for w in winners {
            if let Some(row) = fetched.get(&w.rowid) {
                built.push(row.to_item(path)?);
            }
        }
        out.rows += built.len();
        out.items = Some(built);
    }
    if let Some(winners) = &ranked.events {
        let mut built = Vec::with_capacity(winners.len());
        for w in winners {
            if let Some(row) = fetched.get(&w.rowid) {
                built.push(row.clone());
            }
        }
        if events_ties_safe(&built) {
            out.rows += built.len();
            out.events = Some(built);
        }
    }
    if let Some(winners) = &ranked.plans {
        let mut built = Vec::with_capacity(winners.len());
        for (_, _, rid) in winners {
            if let Some(row) = fetched.get(rid) {
                built.push(PlanRow {
                    thread_id: row.thread_id.clone().unwrap_or_default(),
                    run_id: row.run_id.clone(),
                    node_id: row.node_id.clone(),
                    provider_thread_id: row.provider_thread_id.clone(),
                    steps: row.steps.clone(),
                });
            }
        }
        out.rows += built.len();
        out.plans = Some(built);
    }
    Ok(out)
}

/// How many row ids one fetch statement binds.
pub(super) const FETCH_CHUNK: usize = 900;

#[allow(clippy::too_many_arguments)]
pub(super) fn read_turn_items_combined(
    db: &Connection,
    path: &Path,
    where_ids: &str,
    params: &[SqlValue],
    ids: &[String],
    run_ids: &[String],
    runless: &[String],
    since: &str,
) -> Result<
    (
        Vec<Item>,
        Vec<FetchedTurnItem>,
        Vec<PlanRow>,
        Option<AsstItemIndex>,
        usize,
    ),
    SourceError,
> {
    let mut built = Built::default();
    let mut asst_index: Option<AsstItemIndex> = None;
    if leaf_scan_safe(db).unwrap_or(false) {
        let _scan_phase = timing::phase("read_items");
        let keys = ScanKeys::new(ids, run_ids, runless, since);
        // A scan that fails or meets a value it cannot order leaves every list to the
        // reference queries below.
        if let Some(scanned) = scan(db, &keys, Range::All)
            .ok()
            .filter(|found| found.usable)
        {
            let mut ranked = rank(scanned, tid_index(ids));
            asst_index = ranked.asst.take();
            let mut fetched: Fetched =
                HashMap::with_capacity_and_hasher(ranked.rowids.len(), Default::default());
            for chunk in ranked.rowids.chunks(FETCH_CHUNK) {
                fetch_chunk(db, path, chunk, |rid, item| {
                    fetched.insert(rid, item);
                })?;
            }
            built = build(&ranked, &fetched, path)?;
        }
    }
    let (items, events, plans, rows) =
        finish(db, path, where_ids, params, run_ids, runless, since, built)?;
    Ok((items, events, plans, asst_index, rows))
}

/// The lists the scan left undecided, by the reference queries on `db`.
#[allow(clippy::too_many_arguments)]
pub(super) fn finish(
    db: &Connection,
    path: &Path,
    where_ids: &str,
    params: &[SqlValue],
    run_ids: &[String],
    runless: &[String],
    since: &str,
    built: Built,
) -> Result<(Vec<Item>, Vec<FetchedTurnItem>, Vec<PlanRow>, usize), SourceError> {
    let Built {
        items: items_out,
        events: events_out,
        plans: plans_out,
        rows: mut total_rows,
    } = built;

    let run_marks = marks(run_ids.len());
    let items = match items_out {
        Some(v) => v,
        None => {
            let _phase = timing::phase("read_items");
            let mut item_params = strings(run_ids.iter().cloned());
            item_params.extend(strings(runless.iter().cloned()));
            let runless_marks = marks(runless.len());
            let tail = format!(
                "FROM {PREFIX}turn_items WHERE (run_id IN ({run_marks}) OR (run_id IS NULL AND thread_id IN ({runless_marks}))) AND type IN ({TOOLISH}) ORDER BY updated_at DESC,ordinal DESC LIMIT 3000"
            );
            let oracle_sql = format!("SELECT {} {tail}", item_fields());
            let raw = match fetch_by_rowid_two_phase(db, path, &tail, &oracle_sql, &item_params)? {
                Some(rows) => rows,
                None => {
                    let mut rows = Vec::new();
                    each_row_cached(db, &oracle_sql, &item_params, |row| {
                        rows.push(FetchedTurnItem::from_row(row, 0, false)?);
                        Ok(())
                    })
                    .map_err(|e| db_error(path, e))?;
                    rows
                }
            };
            let mut out = Vec::with_capacity(raw.len());
            for r in raw {
                out.push(r.to_item(path)?);
            }
            total_rows += out.len();
            out
        }
    };

    let events = match events_out {
        Some(v) => v,
        None => {
            let _phase = timing::phase("read_events");
            let mut since_params = params.to_vec();
            since_params.push(SqlValue::Text(since.to_owned()));
            let tail = format!(
                "FROM {PREFIX}turn_items {where_ids} AND updated_at>=? AND type NOT IN ('checkpoint','todo_list') ORDER BY updated_at DESC LIMIT 8000"
            );
            let oracle_sql = format!("SELECT {} {tail}", item_fields());
            let out = match fetch_by_rowid_two_phase(db, path, &tail, &oracle_sql, &since_params)? {
                Some(rows) => rows,
                None => {
                    let mut rows = Vec::new();
                    each_row_cached(db, &oracle_sql, &since_params, |row| {
                        rows.push(FetchedTurnItem::from_row(row, 0, false)?);
                        Ok(())
                    })
                    .map_err(|e| db_error(path, e))?;
                    rows
                }
            };
            total_rows += out.len();
            out
        }
    };

    let plans = match plans_out {
        Some(v) => v,
        None => {
            let _phase = timing::phase("read_plans");
            let plans_sql = format!(
                "SELECT thread_id,run_id,node_id,provider_thread_id,updated_at,json_extract(payload_json,'$.steps') AS steps FROM {PREFIX}turn_items WHERE type='todo_list' AND run_id IN ({run_marks}) ORDER BY updated_at DESC"
            );
            let run_params = strings(run_ids.iter().cloned());
            let found = match todo_lists(db, &plans_sql, &run_marks, &run_params) {
                Ok(Some(found)) => found
                    .into_iter()
                    .map(|r| PlanRow {
                        thread_id: str_at(&r, "thread_id").to_owned(),
                        run_id: optional(&r, "run_id"),
                        node_id: optional(&r, "node_id"),
                        provider_thread_id: optional(&r, "provider_thread_id"),
                        steps: optional(&r, "steps"),
                    })
                    .collect(),
                _ => {
                    let mut out = Vec::new();
                    each_row_cached(db, &plans_sql, &run_params, |row| {
                        out.push(PlanRow {
                            thread_id: sql_opt_text(row.get_ref(0)?).unwrap_or_default(),
                            run_id: sql_opt_text(row.get_ref(1)?),
                            node_id: sql_opt_text(row.get_ref(2)?),
                            provider_thread_id: sql_opt_text(row.get_ref(3)?),
                            steps: sql_opt_text(row.get_ref(5)?),
                        });
                        Ok(())
                    })
                    .map_err(|e| db_error(path, e))?;
                    out
                }
            };
            total_rows += found.len();
            found
        }
    };

    Ok((items, events, plans, total_rows))
}
