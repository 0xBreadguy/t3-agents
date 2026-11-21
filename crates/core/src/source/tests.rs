use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub(super) fn path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "t3-rune-source-{}-{}-{name}.sqlite",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

pub(super) fn schema(db: &Connection) {
    db.execute_batch(&format!(
        "CREATE TABLE {PREFIX}threads(thread_id TEXT,payload_json TEXT,deleted_at TEXT,archived_at TEXT);
        CREATE TABLE {PREFIX}runs(thread_id TEXT,ordinal INTEGER,payload_json TEXT,requested_at TEXT,completed_at TEXT);
        CREATE TABLE {PREFIX}subagents(thread_id TEXT,payload_json TEXT);
        CREATE TABLE {PREFIX}nodes(thread_id TEXT,node_id TEXT,parent_node_id TEXT,run_id TEXT);
        CREATE TABLE {PREFIX}provider_threads(thread_id TEXT,payload_json TEXT);
        CREATE TABLE {PREFIX}provider_turns(thread_id TEXT,provider_thread_id TEXT,ordinal INTEGER,payload_json TEXT,started_at TEXT,completed_at TEXT);
        CREATE TABLE {PREFIX}turn_items(thread_id TEXT,run_id TEXT,node_id TEXT,provider_thread_id TEXT,type TEXT,status TEXT,updated_at TEXT,ordinal INTEGER,payload_json TEXT);
        CREATE TABLE {PREFIX}runtime_requests(thread_id TEXT,node_id TEXT,kind TEXT,status TEXT,resolved_at TEXT);
        CREATE TABLE projection_projects(project_id TEXT,title TEXT,workspace_root TEXT);
        CREATE TABLE auth_tokens(value TEXT);"
    ))
    .unwrap();
}

#[test]
fn missing_paths_are_not_created_and_unknown_schema_stays_unchanged() {
    let missing = path("missing");
    assert_eq!(snapshot(&missing, 0.).unwrap_err().kind, "missing");
    assert!(!missing.exists());
    let unknown = path("unknown");
    let db = Connection::open(&unknown).unwrap();
    db.execute_batch("CREATE TABLE untouched(value TEXT)")
        .unwrap();
    drop(db);
    let before = std::fs::read(&unknown).unwrap();
    assert_eq!(snapshot(&unknown, 0.).unwrap_err().kind, "schema");
    assert_eq!(std::fs::read(&unknown).unwrap(), before);
    std::fs::remove_file(unknown).unwrap();
}

#[test]
fn narrow_projection_excludes_private_payload_fields_and_never_writes() {
    let file = path("privacy");
    let db = Connection::open(&file).unwrap();
    schema(&db);
    let put = |table: &str, cols: &str, payload: Value| {
        db.execute(
            &format!("INSERT INTO {PREFIX}{table} {cols}"),
            [payload.to_string()],
        )
        .unwrap();
    };
    put(
        "threads",
        "VALUES('t',?,NULL,NULL)",
        json!({"id":"t","projectId":"p","title":"Thread","activeProviderThreadId":"pt","delegatedFrom":{"prompt":"SECRET-delegated"},"forkedFrom":"SECRET-fork"}),
    );
    put(
        "runs",
        "VALUES('t',1,?,NULL,NULL)",
        json!({"id":"r","threadId":"t","status":"running","providerThreadId":"pt","delegatedCompletion":{"result":"SECRET-completion"}}),
    );
    put(
        "subagents",
        "VALUES('t',?)",
        json!({"id":"s","threadId":"t","runId":"r","status":"running","title":"Sub","prompt":"SECRET-prompt","result":"SECRET-result","progress":"SECRET-progress","remoteChild":{"label":"other machine","title":"SECRET-remote-title"}}),
    );
    put(
        "provider_threads",
        "VALUES('t',?)",
        json!({"id":"pt","goal":"SECRET-goal","nativeMetadata":{"note":"SECRET-native"},"contextUsage":{"usedTokens":1,"maxTokens":10}}),
    );
    put(
        "provider_turns",
        "VALUES('t','pt',1,?,NULL,NULL)",
        json!({"providerThreadId":"pt","model":"m","nativeTurnRef":"SECRET-turn-ref","tokenUsage":{"usedTokens":2,"maxTokens":10}}),
    );
    db.execute("INSERT INTO auth_tokens VALUES('SECRET-auth')", [])
        .unwrap();
    db.execute(
        &format!("INSERT INTO {PREFIX}turn_items VALUES('t','r',NULL,'pt','command_execution','completed','2026-11-12T12:00:00Z',1,?)"),
        [json!({"input":"cargo test -- synthetic-only","title":"Test","output":"SECRET-tool-output","result":"SECRET-tool-result","reasoning":"SECRET-reasoning-body"}).to_string()],
    )
    .unwrap();
    db.execute(
        &format!(
            "INSERT INTO {PREFIX}runtime_requests VALUES('t',NULL,'user_input','pending',NULL)"
        ),
        [],
    )
    .unwrap();
    db.execute(
        &format!("INSERT INTO {PREFIX}turn_items VALUES('t','r','node','pt','link_request','waiting','2026-11-12T12:00:00Z',2,'{{}}')"),
        [],
    )
    .unwrap();
    drop(db);
    let before = std::fs::read(&file).unwrap();
    let result = snapshot(&file, 1_794_484_800_000.).unwrap();
    let rendered = serde_json::to_string(&result).unwrap();
    assert_eq!(result.agents.len(), 2);
    let thread = result.agents.iter().find(|a| a.id == "t").unwrap();
    assert_eq!(thread.open_asks.blocking, 1);
    assert_eq!(thread.open_asks.cards.link, 1);
    assert!(thread.asked.is_none());

    assert!(
        !rendered.contains("SECRET"),
        "private marker escaped SQL projection"
    );
    assert_eq!(
        result
            .agents
            .iter()
            .find(|a| a.id == "t")
            .unwrap()
            .context
            .used_tokens,
        Some(2.)
    );
    assert!(rendered.contains("cargo test -- synthetic-only"));
    assert!(rendered.contains("other machine"));
    assert_eq!(std::fs::read(&file).unwrap(), before);
    std::fs::remove_file(file).unwrap();
}

#[test]
fn cross_machine_retries_references_attribution_and_cycles() {
    let directory = path("cross-dir");
    std::fs::create_dir(&directory).unwrap();
    let file = directory.join("synthetic.sqlite");
    std::fs::write(directory.join("environment-id"), "machine-a\n").unwrap();
    let db = Connection::open(&file).unwrap();
    schema(&db);
    let threads = vec![
        json!({"id":"root"}),
        json!({"id":"leaf","delegatedFrom":{"environmentId":"machine-b","threadId":"parent"}}),
        json!({"id":"local","delegatedFrom":{"environmentId":"machine-a","threadId":"root"}}),
        json!({"id":"missing","delegatedFrom":{"environmentId":"machine-c","threadId":"unknown","title":"Absent","prompt":"SECRET"}}),
        json!({"id":"missing2","delegatedFrom":{"environmentId":"machine-c","threadId":"unknown"}}),
        json!({"id":"bad","delegatedFrom":{"environmentId":"../bad","threadId":"root"}}),
        json!({"id":"incomplete","delegatedFrom":{"environmentId":"machine-b"}}),
        json!({"id":"a","delegatedFrom":{"environmentId":"machine-a","threadId":"b"}}),
        json!({"id":"b","delegatedFrom":{"environmentId":"machine-a","threadId":"a"}}),
    ];
    for t in threads {
        db.execute(
            &format!("INSERT INTO {PREFIX}threads VALUES(?,?,NULL,NULL)"),
            [str_at(&t, "id"), &t.to_string()],
        )
        .unwrap();
    }
    for n in 0..3 {
        let id = format!("retry-{n}");
        let pid = format!("provider-{n}");
        let sub = json!({"id":id,"threadId":"root","runId":"r","providerThreadId":pid,"status":if n==2 {"completed"} else {"running"},"updatedAt":format!("2026-11-14T11:00:0{n}Z"),"remoteChild":{"environmentId":"machine-b","threadId":"parent","label":"B","result":"SECRET"}});
        db.execute(
            &format!("INSERT INTO {PREFIX}subagents VALUES('root',?)"),
            [sub.to_string()],
        )
        .unwrap();
        db.execute(
            &format!(
                "INSERT INTO {PREFIX}runtime_requests VALUES('root',?,'approval','pending',NULL)"
            ),
            [&id],
        )
        .unwrap();
        db.execute(
            &format!("INSERT INTO {PREFIX}provider_turns VALUES('root',?,?,?,'2026-11-14T11:00:00Z',NULL)"),
            rusqlite::params![pid, n, json!({"providerThreadId":pid,"tokenUsage":{"usedTokens":n+1,"maxTokens":100}}).to_string()],
        )
        .unwrap();
    }
    drop(db);
    let before = std::fs::read(&file).unwrap();
    let result = snapshot(&file, model::ms(Some("2026-11-14T12:00:00Z")).unwrap()).unwrap();
    let get = |id: &str| result.agents.iter().find(|a| a.id == id).unwrap();
    let remote = get("retry-2");
    assert_eq!(
        remote.subagent_ids.as_ref().unwrap(),
        &["retry-0", "retry-1", "retry-2"]
    );
    assert_eq!(remote.status, "completed");
    assert_eq!(remote.requests.len(), 3);
    assert_eq!(remote.history.len(), 3);
    assert_eq!(get("leaf").parent.as_deref(), Some("retry-2"));
    assert_eq!(get("local").parent.as_deref(), Some("root"));
    let reference = result
        .agents
        .iter()
        .find(|a| a.reference_only == Some(true))
        .unwrap();
    assert_eq!(reference.status, "unknown");
    assert!(reference.shelf.is_none());
    assert!(reference.started.is_none());
    assert_eq!(get("missing").parent, get("missing2").parent);
    assert_eq!(
        get("missing").parent.as_deref(),
        Some(reference.id.as_str())
    );
    assert!(get("bad").parent.is_none());
    assert!(get("incomplete").parent.is_none());
    assert!(get("a").parent.is_none());
    assert_eq!(get("b").parent.as_deref(), Some("a"));
    assert!(!serde_json::to_string(&result).unwrap().contains("SECRET"));
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(
        crate::integration::agent_thread_link(remote, None).as_deref(),
        Some("t3code://threads/machine-b/parent")
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn synthetic_runs_preserve_other_day_and_ongoing_timing() {
    let directory = path("timing-dir");
    std::fs::create_dir(&directory).unwrap();
    let file = directory.join("synthetic.sqlite");
    let db = Connection::open(&file).unwrap();
    schema(&db);
    for (id, status, start, end) in [
        (
            "done",
            "completed",
            "2026-11-10T23:50:00Z",
            Some("2026-11-11T00:12:00Z"),
        ),
        ("live", "running", "2026-11-12T13:51:00Z", None),
        ("unknown", "idle", "2026-11-12T13:51:00Z", None),
    ] {
        db.execute(
            &format!("INSERT INTO {PREFIX}threads VALUES(?,?,NULL,NULL)"),
            [id, &json!({"id":id}).to_string()],
        )
        .unwrap();
        db.execute(
            &format!("INSERT INTO {PREFIX}runs VALUES(?,1,?,?,?)"),
            rusqlite::params![
                id,
                json!({"id":id,"threadId":id,"status":status,"startedAt":start,"completedAt":end})
                    .to_string(),
                start,
                end
            ],
        )
        .unwrap();
    }
    drop(db);
    let now = model::ms(Some("2026-11-12T14:30:00Z")).unwrap();
    let snap = snapshot(&file, now).unwrap();
    let get = |id: &str| snap.agents.iter().find(|a| a.id == id).unwrap();
    let done = get("done");
    assert_eq!(crate::ui::run_length(done, now, false), "22m");
    assert_eq!(crate::ui::run_length(done, now + 86400000., false), "22m");
    let live = get("live");
    assert_eq!(crate::ui::run_length(live, now, true), "39m");
    assert_eq!(crate::ui::run_length(live, now + 60000., true), "40m");
    assert_eq!(crate::ui::run_length(get("unknown"), now, false), "");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn sql_cutoff_clock_roundtrips_utc_including_negative_epoch() {
    for stamp in [0., -1., 1_794_484_800_123., 1_794_484_800_123. - WINDOW] {
        assert_eq!(model::ms(Some(&iso(stamp))), Some(stamp));
    }
}

#[test]
fn untied_out_of_cut_malformed_json_succeeds_without_displaced_row() {
    let file = path("out-of-cut-malformed");
    let db = Connection::open(&file).unwrap();
    schema(&db);
    let now = 1_794_484_800_000.;
    db.execute_batch("BEGIN").unwrap();
    db.execute(
        &format!("INSERT INTO {PREFIX}threads VALUES('t1',?,NULL,NULL)"),
        [json!({"id":"t1","projectId":"p1","title":"Thread 1"}).to_string()],
    )
    .unwrap();
    db.execute(
        &format!("INSERT INTO {PREFIX}runs VALUES('t1',1,?,?,NULL)"),
        [
            json!({"id":"r1","threadId":"t1","status":"running","startedAt":iso(now - 600_000.)})
                .to_string(),
            iso(now - 600_000.),
        ],
    )
    .unwrap();
    // Insert 3,005 strictly ordered (untied) valid items, plus one older malformed row
    // displaced outside both the 3,000-item cut and the 3-hour event window.
    let mut stmt = db
        .prepare(&format!(
            "INSERT INTO {PREFIX}turn_items VALUES('t1','r1',NULL,NULL,'command_execution','completed',?,?,?)"
        ))
        .unwrap();
    stmt.execute(rusqlite::params![
        iso(now - 5. * 3_600_000.),
        0i64,
        r#"{"startedAt":"x","title":"#
    ])
    .unwrap();
    for i in 1..=3005i64 {
        let ts = iso(now - 100_000. + (i as f64) * 10.);
        stmt.execute(rusqlite::params![
            ts,
            i,
            json!({"startedAt": ts, "title": format!("cmd-{i}"), "input": format!("echo {i}")})
                .to_string()
        ])
        .unwrap();
    }
    drop(stmt);
    db.execute_batch("COMMIT").unwrap();
    drop(db);

    let with_corrupt = snapshot(&file, now).expect("out-of-cut malformed row must be skipped");
    assert_eq!(with_corrupt.agents.len(), 1);
    assert_eq!(with_corrupt.agents[0].items.len(), 3000);
    assert_eq!(with_corrupt.agents[0].items[0].label, "cmd-3005");
    assert_eq!(with_corrupt.agents[0].items[2999].label, "cmd-6");

    // Deleting the displaced malformed row must yield the exact same snapshot.
    let db = Connection::open(&file).unwrap();
    db.execute(
        &format!("DELETE FROM {PREFIX}turn_items WHERE ordinal=0"),
        [],
    )
    .unwrap();
    drop(db);
    let clean = snapshot(&file, now).unwrap();
    assert_eq!(
        serde_json::to_value(&with_corrupt).unwrap(),
        serde_json::to_value(&clean).unwrap()
    );
    std::fs::remove_file(file).unwrap();
}

#[test]
fn in_cut_malformed_json_fails_with_schema_error() {
    let file = path("in-cut-malformed");
    let db = Connection::open(&file).unwrap();
    schema(&db);
    let now = 1_794_484_800_000.;
    db.execute_batch("BEGIN").unwrap();
    db.execute(
        &format!("INSERT INTO {PREFIX}threads VALUES('t1',?,NULL,NULL)"),
        [json!({"id":"t1","projectId":"p1","title":"Thread 1"}).to_string()],
    )
    .unwrap();
    db.execute(
        &format!("INSERT INTO {PREFIX}runs VALUES('t1',1,?,?,NULL)"),
        [
            json!({"id":"r1","threadId":"t1","status":"running","startedAt":iso(now - 600_000.)})
                .to_string(),
            iso(now - 600_000.),
        ],
    )
    .unwrap();
    let mut stmt = db
        .prepare(&format!(
            "INSERT INTO {PREFIX}turn_items VALUES('t1','r1',NULL,NULL,'command_execution','completed',?,?,?)"
        ))
        .unwrap();
    for i in 1..=3005i64 {
        let ts = iso(now - 5. * 3_600_000. + (i as f64) * 10.);
        let payload = if i == 2500 {
            // Inside the top-3000 cut.
            r#"{"startedAt":"x","title":"#.to_string()
        } else {
            json!({"startedAt": ts, "title": format!("cmd-{i}")}).to_string()
        };
        stmt.execute(rusqlite::params![ts, i, payload]).unwrap();
    }
    drop(stmt);
    db.execute_batch("COMMIT").unwrap();
    drop(db);

    let err = snapshot(&file, now).expect_err("in-cut malformed payload_json must fail");
    assert_eq!(err.kind, "schema");

    // Also verify when a tie at the 3,000th cut forces the two-phase tie fallback:
    // the in-cut malformed row must still fail cleanly with a schema error.
    let db = Connection::open(&file).unwrap();
    let cut_ts = iso(now - 5. * 3_600_000. + 60.);
    db.execute(
        &format!(
            "UPDATE {PREFIX}turn_items SET updated_at=?, ordinal=6 WHERE ordinal IN (5, 6, 7)"
        ),
        [&cut_ts],
    )
    .unwrap();
    drop(db);
    let err2 = snapshot(&file, now).expect_err("in-cut malformed row under cut tie must fail");
    assert_eq!(err2.kind, "schema");
    std::fs::remove_file(file).unwrap();
}

#[test]
fn malformed_superseded_run_still_fails_with_schema_error() {
    let file = path("malformed-older-run");
    let db = Connection::open(&file).unwrap();
    schema(&db);
    db.execute_batch(&format!(
        "INSERT INTO {PREFIX}threads VALUES('t1','{{\"id\":\"t1\"}}',NULL,NULL);
         INSERT INTO {PREFIX}runs VALUES
            ('t1',1,'invalid JSON','2026-01-01T00:00:00.000Z','2026-01-01T00:01:00.000Z'),
            ('t1',2,'{{\"id\":\"r2\",\"threadId\":\"t1\",\"status\":\"running\"}}',NULL,NULL);"
    ))
    .unwrap();
    drop(db);

    let err = snapshot(&file, 1_794_484_800_000.).expect_err("the older run must be validated");
    assert_eq!(err.kind, "schema");
    std::fs::remove_file(file).unwrap();
}

#[test]
fn cross_run_internal_ties_stay_on_fast_path_and_match_oracle() {
    let file = path("cross-run-tie");
    let db = Connection::open(&file).unwrap();
    schema(&db);
    let now = 1_794_484_800_000.;
    db.execute_batch("BEGIN").unwrap();
    for (tid, rid) in [("t1", "r1"), ("t2", "r2")] {
        db.execute(
            &format!("INSERT INTO {PREFIX}threads VALUES(?,?,NULL,NULL)"),
            [
                tid,
                &json!({"id":tid,"projectId":"p1","title":tid}).to_string(),
            ],
        )
        .unwrap();
        db.execute(
            &format!("INSERT INTO {PREFIX}runs VALUES(?,1,?,?,NULL)"),
            [
                tid,
                &json!({"id":rid,"threadId":tid,"status":"running","startedAt":iso(now - 600_000.)})
                    .to_string(),
                &iso(now - 600_000.),
            ],
        )
        .unwrap();
    }
    // Two items from DIFFERENT runs sharing the exact same (updated_at, ordinal) inside top 3000.
    let shared_ts = iso(now - 5_000.);
    db.execute(
        &format!("INSERT INTO {PREFIX}turn_items VALUES('t1','r1',NULL,NULL,'command_execution','completed',?,1,?)"),
        [&shared_ts, &json!({"startedAt":shared_ts,"title":"from-r1"}).to_string()],
    )
    .unwrap();
    db.execute(
        &format!("INSERT INTO {PREFIX}turn_items VALUES('t2','r2',NULL,NULL,'command_execution','completed',?,1,?)"),
        [&shared_ts, &json!({"startedAt":shared_ts,"title":"from-r2"}).to_string()],
    )
    .unwrap();
    db.execute_batch("COMMIT").unwrap();
    drop(db);

    timing::enable(true);
    timing::reset();
    let snap = snapshot(&file, now).unwrap();
    let data = timing::take();
    timing::enable(false);

    assert!(
        data.queries.contains_key("turn_items_leaf_scan"),
        "expected leaf scan"
    );
    assert!(
        !data.queries.contains_key("items"),
        "cross-run internal tie should not fall back to the SQL items query: {:?}",
        data.queries.keys().collect::<Vec<_>>()
    );
    let a1 = snap.agents.iter().find(|a| a.id == "t1").unwrap();
    let a2 = snap.agents.iter().find(|a| a.id == "t2").unwrap();
    assert_eq!(a1.items.len(), 1);
    assert_eq!(a1.items[0].label, "from-r1");
    assert_eq!(a2.items.len(), 1);
    assert_eq!(a2.items[0].label, "from-r2");
    std::fs::remove_file(file).unwrap();
}

#[test]
fn estimated_asks_non_positive_rowid_and_non_string_multi_phase_match_oracle() {
    let dir = path("asks-audit-edge");
    std::fs::create_dir(&dir).unwrap();
    let file = dir.join("synthetic.sqlite");
    let db = Connection::open(&file).unwrap();
    db.execute_batch(include_str!(
        "../../../../checks/fixtures/t3-schema-main-096.sql"
    ))
    .unwrap();
    // Drop migration 094's message index so the scan's AsstItemIndex hint path runs;
    // keep migration 095's trailing `inline_work_output` column to verify leaf-scan compat.
    db.execute_batch(
        "DROP INDEX orchestration_v2_projection_turn_items_message_ordinal_idx;",
    )
    .unwrap();
    db.execute_batch(
        r#"
        INSERT INTO projection_projects(project_id,title,workspace_root,scripts_json,created_at,updated_at)
        VALUES('p','Synth','/synth','[]','','');
        INSERT INTO orchestration_v2_projection_threads(thread_id,project_id,title,default_provider,runtime_mode,interaction_mode,created_at,updated_at,payload_json)
        VALUES
          ('t_num','p','Num','codex','full-access','default','','','{"id":"t_num","projectId":"p","title":"Num"}'),
          ('t_bool','p','Bool','codex','full-access','default','','','{"id":"t_bool","projectId":"p","title":"Bool"}'),
          ('t_arr','p','Arr','codex','full-access','default','','','{"id":"t_arr","projectId":"p","title":"Arr"}'),
          ('t_obj','p','Obj','codex','full-access','default','','','{"id":"t_obj","projectId":"p","title":"Obj"}'),
          ('t_null','p','Null','codex','full-access','default','','','{"id":"t_null","projectId":"p","title":"Null"}');
        -- Multi assistant_message rows per (node_id, thread_id) so AsstSlot::Many is exercised.
        INSERT INTO orchestration_v2_projection_turn_items(turn_item_id,thread_id,node_id,ordinal,type,status,updated_at,payload_json)
        VALUES
          ('ti_num_1','t_num','n_num',1,'assistant_message','completed','','{"messageId":"m_num","phase":"commentary"}'),
          ('ti_num_2','t_num','n_num',2,'assistant_message','completed','','{"messageId":"m_num","phase":1}'),
          ('ti_bool_1','t_bool','n_bool',1,'assistant_message','completed','','{"messageId":"m_bool","phase":"commentary"}'),
          ('ti_bool_2','t_bool','n_bool',2,'assistant_message','completed','','{"messageId":"m_bool","phase":true}'),
          ('ti_arr_1','t_arr','n_arr',1,'assistant_message','completed','','{"messageId":"m_arr","phase":"commentary"}'),
          ('ti_arr_2','t_arr','n_arr',2,'assistant_message','completed','','{"messageId":"m_arr","phase":[]}'),
          ('ti_obj_1','t_obj','n_obj',1,'assistant_message','completed','','{"messageId":"m_obj","phase":"commentary"}'),
          ('ti_obj_2','t_obj','n_obj',2,'assistant_message','completed','','{"messageId":"m_obj","phase":{}}'),
          ('ti_null_1','t_null','n_null',1,'assistant_message','completed','','{"messageId":"m_null","phase":"partial_answer"}'),
          ('ti_null_2','t_null','n_null',2,'assistant_message','completed','','{"messageId":"m_null","phase":null}');
        INSERT INTO orchestration_v2_projection_messages(message_id,thread_id,node_id,role,streaming,created_at,updated_at,payload_json)
        VALUES
          ('m_num','t_num','n_num','assistant',0,'2026-11-12T10:00:00.000Z','','{"text":"Should I deploy?"}'),
          ('m_bool','t_bool','n_bool','assistant',0,'2026-11-12T10:00:00.000Z','','{"text":"Should I deploy?"}'),
          ('m_arr','t_arr','n_arr','assistant',0,'2026-11-12T10:00:00.000Z','','{"text":"Should I deploy?"}'),
          ('m_obj','t_obj','n_obj','assistant',0,'2026-11-12T10:00:00.000Z','','{"text":"Should I deploy?"}'),
          ('m_null','t_null','n_null','assistant',0,'2026-11-12T10:00:00.000Z','','{"text":"Should I deploy?"}');
        "#,
    )
    .unwrap();
    let now = 1_794_484_800_000.;
    crate::asks::register(&db).unwrap();
    let oracle = read_estimated_asks(&db, None).unwrap();
    drop(db);
    let snap = snapshot(&file, now).unwrap();
    let got: HashMap<&str, u64> = snap
        .agents
        .iter()
        .map(|a| (a.id.as_str(), a.estimated_asks))
        .collect();
    // Non-string non-null JSON phases stop COALESCE -> not in ('commentary','final_answer') -> 0.
    assert_eq!(got["t_num"], 0);
    assert_eq!(got["t_bool"], 0);
    assert_eq!(got["t_arr"], 0);
    assert_eq!(got["t_obj"], 0);
    // Explicit JSON null falls through COALESCE to 'final_answer' -> 1.
    assert_eq!(got["t_null"], 1);
    for (id, est) in &got {
        assert_eq!(
            *est,
            oracle.get(*id).copied().unwrap_or(0),
            "mismatch on {id}"
        );
    }

    // Now insert explicit non-positive rowids (0 and -1) on assistant_message items.
    let db = Connection::open(&file).unwrap();
    db.execute_batch(
        r#"
        INSERT INTO orchestration_v2_projection_threads(thread_id,project_id,title,default_provider,runtime_mode,interaction_mode,created_at,updated_at,payload_json)
        VALUES
          ('t_zero','p','Zero','codex','full-access','default','','','{"id":"t_zero","projectId":"p","title":"Zero"}'),
          ('t_neg','p','Neg','codex','full-access','default','','','{"id":"t_neg","projectId":"p","title":"Neg"}');
        INSERT INTO orchestration_v2_projection_turn_items(rowid,turn_item_id,thread_id,node_id,ordinal,type,status,updated_at,payload_json)
        VALUES
          (0,'ti_zero','t_zero','n_zero',1,'assistant_message','completed','','{"messageId":"m_zero","phase":"partial_answer"}'),
          (-1,'ti_neg','t_neg','n_neg',1,'assistant_message','completed','','{"messageId":"m_neg","phase":"partial_answer"}');
        INSERT INTO orchestration_v2_projection_messages(message_id,thread_id,node_id,role,streaming,created_at,updated_at,payload_json)
        VALUES
          ('m_zero','t_zero','n_zero','assistant',0,'2026-11-12T10:00:00.000Z','','{"text":"Should I ship?"}'),
          ('m_neg','t_neg','n_neg','assistant',0,'2026-11-12T10:00:00.000Z','','{"text":"Should I ship?"}');
        "#,
    )
    .unwrap();
    let oracle2 = {
        crate::asks::register(&db).unwrap();
        read_estimated_asks(&db, None).unwrap()
    };
    drop(db);
    let snap2 = snapshot(&file, now).unwrap();
    for a in &snap2.agents {
        assert_eq!(
            a.estimated_asks,
            oracle2.get(a.id.as_str()).copied().unwrap_or(0),
            "non-positive rowid mismatch on {}",
            a.id
        );
    }
    assert_eq!(
        snap2
            .agents
            .iter()
            .find(|a| a.id == "t_zero")
            .unwrap()
            .estimated_asks,
        0
    );
    assert_eq!(
        snap2
            .agents
            .iter()
            .find(|a| a.id == "t_neg")
            .unwrap()
            .estimated_asks,
        0
    );
    std::fs::remove_dir_all(dir).unwrap();
}
