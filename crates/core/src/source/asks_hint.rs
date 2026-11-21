//! Pre-scanned assistant-message item index and SQLite scalar hint hooks for
//! `estimated_asks`, preserving strict SQL-boundary privacy: message bodies
//! never leave SQLite and only eligible assistant prose reaches `t3_ask_count`.
use super::{
    PREFIX, columns, each_row_uncached,
    fast_reads::{FnvHasher, RowidHasher},
    marks, strings, timing,
};
use rusqlite::{
    Connection,
    functions::FunctionFlags,
    params_from_iter,
    types::{Value as SqlValue, ValueRef},
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

pub(super) enum AsstSlot {
    One {
        tid: u32,
        ord: Option<i64>,
        rowid: i64,
    },
    Many(Vec<(u32, Option<i64>, i64)>),
}

/// An active thread's position in the read's list of threads, by id.
pub(super) type TidIndex = HashMap<Box<[u8]>, u32, std::hash::BuildHasherDefault<FnvHasher>>;

pub(super) struct AsstItemIndex {
    pub(super) tid_to_idx: TidIndex,
    pub(super) by_node: HashMap<Box<[u8]>, AsstSlot, std::hash::BuildHasherDefault<FnvHasher>>,
}

/// Get the latest positive estimate per active thread. Message bodies stay in
/// SQLite and only eligible assistant prose reaches the count function.
pub(crate) fn read_estimated_asks(
    db: &Connection,
    owners: Option<&[String]>,
) -> rusqlite::Result<HashMap<String, u64>> {
    read_estimated_asks_inner(db, owners, None)
}

pub(super) fn read_estimated_asks_inner(
    db: &Connection,
    owners: Option<&[String]>,
    asst_index: Option<&AsstItemIndex>,
) -> rusqlite::Result<HashMap<String, u64>> {
    let _phase = timing::phase("read_estimated_asks");
    if owners.is_some_and(|ids| ids.is_empty()) {
        return Ok(HashMap::new());
    }
    let messages = columns(db, &format!("{PREFIX}messages"))?;
    if ![
        "thread_id",
        "message_id",
        "role",
        "streaming",
        "created_at",
        "payload_json",
    ]
    .iter()
    .all(|name| messages.contains(*name))
    {
        return Ok(HashMap::new());
    }
    let items = columns(db, &format!("{PREFIX}turn_items"))?;
    let has_item_cols = ["thread_id", "type", "ordinal", "payload_json"]
        .iter()
        .all(|name| items.contains(*name));
    let msg_ord_idx = format!("{PREFIX}turn_items_message_ordinal_idx");
    let has_msg_ord_idx: bool = has_item_cols
        && db
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1)",
            )?
            .query_row([&msg_ord_idx], |r| r.get(0))?;

    if owners.is_none()
        && !has_msg_ord_idx
        && has_item_cols
        && messages.contains("node_id")
        && items.contains("node_id")
    {
        if let Some(idx) = asst_index {
            if let Ok(result) = read_estimated_asks_indexed(db, idx) {
                return Ok(result);
            }
        }
    }

    read_estimated_asks_oracle(
        db,
        owners,
        &messages,
        &items,
        has_item_cols,
        has_msg_ord_idx,
    )
}

/// The pre-scanned `AsstItemIndex` provides a rowid seek for single-item nodes,
/// a pre-fetched phase for multi-item nodes, and a NULL phase for nodes with
/// zero items. Message bodies stay in SQLite and `t3_ask_count` runs strictly after
/// `phase IN ('commentary','final_answer')`.
fn read_estimated_asks_indexed(
    db: &Connection,
    asst_index: &AsstItemIndex,
) -> rusqlite::Result<HashMap<String, u64>> {
    // Pre-fetch (messageId, phase) ONLY for nodes that have >1 assistant_message
    // turn item for the same (node_id, thread_id), eliminating O(K^2) scans.
    let mut multi_rowids: Vec<i64> = Vec::new();
    for slot in asst_index.by_node.values() {
        if let AsstSlot::Many(entries) = slot {
            for &(_, _, rid) in entries {
                multi_rowids.push(rid);
            }
        }
    }
    let mut multi_phases: HashMap<
        (Box<[u8]>, u32, Box<[u8]>),
        Option<String>,
        std::hash::BuildHasherDefault<FnvHasher>,
    > = HashMap::default();
    if !multi_rowids.is_empty() {
        multi_rowids.sort_unstable();
        multi_rowids.dedup();
        let mut raw_phase: HashMap<
            i64,
            (Option<Box<[u8]>>, Option<String>),
            std::hash::BuildHasherDefault<RowidHasher>,
        > = HashMap::with_capacity_and_hasher(multi_rowids.len(), Default::default());
        for chunk in multi_rowids.chunks(900) {
            let sql = format!(
                "SELECT rowid,
                    CASE WHEN json_valid(payload_json) THEN json_extract(payload_json,'$.messageId') END,
                    CASE WHEN json_valid(payload_json) THEN json_extract(payload_json,'$.phase') END
                 FROM {PREFIX}turn_items
                 WHERE rowid IN ({}) -- ti_asst_phase",
                marks(chunk.len())
            );
            let args: Vec<SqlValue> = chunk.iter().copied().map(SqlValue::Integer).collect();
            each_row_uncached(db, &sql, &args, |row| {
                if let ValueRef::Integer(rid) = row.get_ref(0)? {
                    let mid = match row.get_ref(1)? {
                        ValueRef::Text(b) => Some(b.to_vec().into_boxed_slice()),
                        _ => None,
                    };
                    let ph = match row.get_ref(2)? {
                        ValueRef::Null => None,
                        ValueRef::Text(b) => Some(String::from_utf8_lossy(b).into_owned()),
                        // Non-null non-string JSON values (e.g. numbers/booleans) are non-NULL in
                        // SQLite json_extract so COALESCE stops and IN ('commentary','final_answer')
                        // evaluates false. Store an empty string sentinel with identical SQL behavior.
                        ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Blob(_) => {
                            Some(String::new())
                        }
                    };
                    raw_phase.insert(rid, (mid, ph));
                }
                Ok(())
            })?;
        }
        // Populate in (ordinal DESC, rowid DESC) order: first matching row wins,
        // even when its phase is NULL (matches SQL LIMIT 1).
        for (nid, slot) in &asst_index.by_node {
            if let AsstSlot::Many(entries) = slot {
                for &(tid_idx, _, rid) in entries {
                    if let Some((Some(mid), ph)) = raw_phase.get(&rid) {
                        multi_phases
                            .entry((nid.clone(), tid_idx, mid.clone()))
                            .or_insert_with(|| ph.clone());
                    }
                }
            }
        }
    }

    struct HintData {
        tid_to_idx: HashMap<Box<[u8]>, u32, std::hash::BuildHasherDefault<FnvHasher>>,
        single_or_multi: HashMap<Box<[u8]>, HintSlot, std::hash::BuildHasherDefault<FnvHasher>>,
        multi_phases: HashMap<
            (Box<[u8]>, u32, Box<[u8]>),
            Option<String>,
            std::hash::BuildHasherDefault<FnvHasher>,
        >,
    }
    enum HintSlot {
        One { tid: u32, rowid: i64 },
        Multi { tids: Vec<(u32, i64)> }, // rowid > 0 if unique for that tid, else -1
    }

    let mut single_or_multi: HashMap<
        Box<[u8]>,
        HintSlot,
        std::hash::BuildHasherDefault<FnvHasher>,
    > = HashMap::with_capacity_and_hasher(asst_index.by_node.len(), Default::default());
    for (nid, slot) in &asst_index.by_node {
        match slot {
            AsstSlot::One { tid, rowid, .. } => {
                single_or_multi.insert(
                    nid.clone(),
                    HintSlot::One {
                        tid: *tid,
                        rowid: *rowid,
                    },
                );
            }
            AsstSlot::Many(entries) => {
                let mut per_tid: Vec<(u32, i64)> = Vec::new();
                for &(tid, _, rid) in entries {
                    if let Some(slot) = per_tid.iter_mut().find(|(t, _)| *t == tid) {
                        slot.1 = -1;
                    } else {
                        per_tid.push((tid, rid));
                    }
                }
                single_or_multi.insert(nid.clone(), HintSlot::Multi { tids: per_tid });
            }
        }
    }

    let data = Arc::new(HintData {
        tid_to_idx: asst_index.tid_to_idx.clone(),
        single_or_multi,
        multi_phases,
    });

    struct Guard<'a>(&'a Connection);
    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            let _ = self.0.remove_function("t3_asst_hint", 2);
            let _ = self.0.remove_function("t3_asst_multi_phase", 3);
        }
    }
    let _guard = Guard(db);

    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    let d1 = Arc::clone(&data);
    db.create_scalar_function("t3_asst_hint", 2, flags, move |ctx| {
        let (ValueRef::Text(nid), ValueRef::Text(tid)) = (ctx.get_raw(0), ctx.get_raw(1)) else {
            return Ok(None::<i64>);
        };
        let Some(&tid_idx) = d1.tid_to_idx.get(tid) else {
            return Ok(None::<i64>);
        };
        let Some(slot) = d1.single_or_multi.get(nid) else {
            return Ok(Some(0i64));
        };
        let code = match slot {
            HintSlot::One { tid: t, rowid } => {
                if *t == tid_idx {
                    *rowid
                } else {
                    0
                }
            }
            HintSlot::Multi { tids } => tids
                .iter()
                .find(|(t, _)| *t == tid_idx)
                .map_or(0, |&(_, r)| r),
        };
        Ok(Some(code))
    })?;

    let d2 = Arc::clone(&data);
    db.create_scalar_function("t3_asst_multi_phase", 3, flags, move |ctx| {
        let (ValueRef::Text(nid), ValueRef::Text(tid), ValueRef::Text(mid)) =
            (ctx.get_raw(0), ctx.get_raw(1), ctx.get_raw(2))
        else {
            return Ok(None::<String>);
        };
        let Some(&tid_idx) = d2.tid_to_idx.get(tid) else {
            return Ok(None::<String>);
        };
        let key = (
            nid.to_vec().into_boxed_slice(),
            tid_idx,
            mid.to_vec().into_boxed_slice(),
        );
        Ok(d2.multi_phases.get(&key).cloned().flatten())
    })?;

    let order = "ORDER BY i.ordinal DESC,i.rowid DESC LIMIT 1";
    let by_message = format!(
        "(SELECT CASE WHEN json_valid(i.payload_json) THEN json_extract(i.payload_json,'$.phase') END
          FROM {PREFIX}turn_items i
          WHERE i.thread_id=m.thread_id AND i.type='assistant_message'
            AND CASE WHEN json_valid(i.payload_json)
                     THEN json_extract(i.payload_json,'$.messageId')=m.message_id END
          {order})"
    );
    let by_node = format!(
        "(SELECT CASE WHEN json_valid(i.payload_json) THEN json_extract(i.payload_json,'$.phase') END
          FROM {PREFIX}turn_items i
          WHERE i.node_id=m.node_id AND +i.thread_id=m.thread_id AND i.type='assistant_message'
            AND CASE WHEN json_valid(i.payload_json)
                     THEN json_extract(i.payload_json,'$.messageId')=m.message_id END
          {order})"
    );
    let item_phase = format!(
        "CASE WHEN m.node_id IS NULL THEN {by_message}
              ELSE (SELECT CASE WHEN h > 0 THEN
                               (SELECT CASE WHEN json_valid(i.payload_json) THEN
                                           CASE WHEN json_extract(i.payload_json,'$.messageId')=m.message_id
                                                THEN json_extract(i.payload_json,'$.phase') END
                                       END
                                FROM {PREFIX}turn_items i
                                WHERE i.rowid = h)
                           WHEN h = -1 THEN t3_asst_multi_phase(m.node_id, m.thread_id, m.message_id)
                           WHEN h IS NULL THEN {by_node}
                           END
                    FROM (SELECT t3_asst_hint(m.node_id, m.thread_id) AS h))
         END"
    );

    let sql = format!(
        "WITH estimates AS MATERIALIZED (
        SELECT t.thread_id, (
          SELECT estimate FROM (
            SELECT CASE WHEN m.role='assistant' AND m.streaming=0 AND json_valid(m.payload_json)
                THEN CASE WHEN COALESCE({item_phase}, json_extract(m.payload_json,'$.phase'),
                                        'final_answer') IN ('commentary','final_answer')
                     THEN t3_ask_count(jsonb_extract(m.payload_json,'$.text')) END
                END AS estimate
            FROM {PREFIX}messages m
            WHERE m.thread_id=t.thread_id AND m.role='assistant' AND m.streaming=0
              AND julianday(m.created_at) IS NOT NULL
              AND m.created_at >= COALESCE((
                  SELECT h.created_at FROM {PREFIX}messages h
                  WHERE h.thread_id=t.thread_id AND h.role='user'
                    AND julianday(h.created_at) IS NOT NULL
                    AND CASE WHEN json_valid(h.payload_json) THEN
                        json_extract(h.payload_json,'$.createdBy')='user'
                        AND json_extract(h.payload_json,'$.idleCompaction') IS NULL END
                  ORDER BY h.created_at DESC,h.message_id DESC LIMIT 1
              ), '')
            ORDER BY m.created_at DESC,m.message_id DESC LIMIT -1
          ) WHERE estimate > 0 LIMIT 1
        ) AS estimate
        FROM {PREFIX}threads t
        WHERE t.deleted_at IS NULL AND t.archived_at IS NULL
    )
    SELECT thread_id, estimate FROM estimates WHERE estimate > 0 -- asks_score"
    );
    let mut out = HashMap::new();
    each_row_uncached(db, &sql, &[], |row| {
        out.insert(row.get(0)?, row.get::<_, i64>(1)? as u64);
        Ok(())
    })?;
    Ok(out)
}

fn read_estimated_asks_oracle(
    db: &Connection,
    owners: Option<&[String]>,
    messages: &HashSet<String>,
    items: &HashSet<String>,
    has_item_cols: bool,
    has_index: bool,
) -> rusqlite::Result<HashMap<String, u64>> {
    let (owner_filter, args) = match owners {
        Some([]) => return Ok(HashMap::new()),
        Some(ids) => (
            format!("AND t.thread_id IN ({})", marks(ids.len())),
            strings(ids.iter().cloned()),
        ),
        None => (String::new(), Vec::new()),
    };
    // Conversation messages have no phase in the T3 contract. ProjectionStore.ts
    // joins message items via $.messageId; keep migration 094's partial-index
    // predicate and expression intact. Force the installed index when present:
    // otherwise SQLite can prefer a reverse ordinal scan of the whole thread.
    let item_phase = if has_item_cols {
        let index = format!("{PREFIX}turn_items_message_ordinal_idx");
        let order = "ORDER BY i.ordinal DESC,i.rowid DESC LIMIT 1";
        if has_index {
            format!(
                "(SELECT json_extract(i.payload_json,'$.phase')
                FROM {PREFIX}turn_items i INDEXED BY {index}
                WHERE i.thread_id=m.thread_id AND i.type IN ('user_message', 'assistant_message')
                  AND json_valid(i.payload_json)
                  AND json_extract(i.payload_json, '$.messageId')=+m.message_id
                  AND CASE i.type WHEN 'assistant_message' THEN 1 ELSE 0 END
                {order})"
            )
        } else {
            // The adapters store the same node ID on both projections. Older
            // installations can seek by node. Disable the competing thread
            // index on that path so SQLite doesn't scan the thread by ordinal.
            let phase = |owner_filter: &str| {
                format!("(SELECT
                    CASE WHEN json_valid(i.payload_json) THEN json_extract(i.payload_json,'$.phase') END
                FROM {PREFIX}turn_items i
                WHERE {owner_filter} AND i.type='assistant_message'
                  AND CASE WHEN json_valid(i.payload_json)
                           THEN json_extract(i.payload_json, '$.messageId')=m.message_id END
                {order})")
            };
            let by_message = phase("i.thread_id=m.thread_id");
            if messages.contains("node_id") && items.contains("node_id") {
                let by_node = phase("i.node_id=m.node_id AND +i.thread_id=m.thread_id");
                format!("CASE WHEN m.node_id IS NOT NULL THEN {by_node} ELSE {by_message} END")
            } else {
                by_message
            }
        }
    } else {
        "NULL".into()
    };
    // The thread-created index supplies both ordered walks. The human cutoff
    // is inclusive for assistants at the exact same timestamp, as in the
    // count-only prototype. IDs deterministically order equal-time assistants.
    // LIMIT -1 keeps the ordered walk lazy and prevents the outer filter from
    // evaluating the classifier twice. JSONB keeps arrays/objects distinct
    // from string text when the function checks the SQLite argument type.
    let sql = format!(
        "WITH estimates AS MATERIALIZED (
        SELECT t.thread_id, (
          SELECT estimate FROM (
            SELECT CASE WHEN m.role='assistant' AND m.streaming=0 AND json_valid(m.payload_json)
                THEN CASE WHEN COALESCE({item_phase}, json_extract(m.payload_json,'$.phase'),
                                        'final_answer') IN ('commentary','final_answer')
                     THEN t3_ask_count(jsonb_extract(m.payload_json,'$.text')) END
                END AS estimate
            FROM {PREFIX}messages m
            WHERE m.thread_id=t.thread_id AND m.role='assistant' AND m.streaming=0
              AND julianday(m.created_at) IS NOT NULL
              AND m.created_at >= COALESCE((
                  SELECT h.created_at FROM {PREFIX}messages h
                  WHERE h.thread_id=t.thread_id AND h.role='user'
                    AND julianday(h.created_at) IS NOT NULL
                    AND CASE WHEN json_valid(h.payload_json) THEN
                        json_extract(h.payload_json,'$.createdBy')='user'
                        AND json_extract(h.payload_json,'$.idleCompaction') IS NULL END
                  ORDER BY h.created_at DESC,h.message_id DESC LIMIT 1
              ), '')
            ORDER BY m.created_at DESC,m.message_id DESC LIMIT -1
          ) WHERE estimate > 0 LIMIT 1
        ) AS estimate
        FROM {PREFIX}threads t
        WHERE t.deleted_at IS NULL AND t.archived_at IS NULL {owner_filter}
    )
    SELECT thread_id, estimate FROM estimates WHERE estimate > 0"
    );
    db.prepare_cached(&sql)?
        .query_map(params_from_iter(&args), |row| {
            Ok((row.get(0)?, row.get::<_, i64>(1)? as u64))
        })?
        .collect()
}
