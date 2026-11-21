//! The full reader under adversarial ties, against output recorded from the TypeScript original
//! (main `2f1d553`): the 39 cases of `checks/scripts/full-read-parity.mjs` across five schema
//! and plan variants and three rounds of deletes, retimes and inserts, at three clocks each.
//!
//! Each variant's database travels once, as SQL text; the mutations between rounds travel as the
//! statements the original's builder ran. Each case compares the digest of the whole normalized
//! snapshot, plus counts. The displaced malformed case uses the recorded clean
//! snapshot at the same clock.
#[path = "golden_support/mod.rs"]
mod support;
use rusqlite::types::Value as SqlValue;
use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_t3-agents-core");

#[test]
fn canonical_digest_matches_the_typescript_vectors() {
    // Recorded by `canonicalDigest` in checks/scripts/golden.ts.
    assert_eq!(support::canonical_digest(&serde_json::json!({"b":[1,2],"a":"x","c":null,"d":true,"e":1.5,"f":0,"g":0})), "f80a4bfe24a01ef3");
    assert_eq!(support::canonical_digest(&Value::String("héllo 🍞".into())), "351547bed66f88e3");
}

fn apply(conn: &rusqlite::Connection, log: &Value) {
    for step in log.as_array().unwrap() {
        let sql = step["sql"].as_str().unwrap();
        match step["args"].as_array() {
            Some(args) => {
                let values: Vec<SqlValue> = args
                    .iter()
                    .map(|arg| match arg {
                        Value::Null => SqlValue::Null,
                        Value::String(text) => SqlValue::Text(text.clone()),
                        Value::Bool(flag) => SqlValue::Integer(i64::from(*flag)),
                        Value::Number(number) => match number.as_i64() {
                            Some(integer) => SqlValue::Integer(integer),
                            None => SqlValue::Real(number.as_f64().unwrap()),
                        },
                        other => panic!("mutation argument {other}"),
                    })
                    .collect();
                conn.execute(sql, rusqlite::params_from_iter(values)).unwrap_or_else(|e| panic!("{sql}: {e}"));
            }
            None => conn.execute_batch(sql).unwrap_or_else(|e| panic!("{sql}: {e}")),
        }
    }
}

#[test]
fn full_reader_under_ties_matches_frozen_oracle() {
    let fixture = support::load("full-read.json.gz");
    let cases = fixture["cases"].as_array().expect("cases");
    let dir = support::scratch("full-read");
    let home = support::empty_home(&dir);
    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0;

    // Group in the recorded order (variant, then round, then clock).
    let mut variants: Vec<String> = Vec::new();
    for case in cases {
        let variant = case["variant"].as_str().unwrap().to_string();
        if !variants.contains(&variant) {
            variants.push(variant);
        }
    }
    for variant in &variants {
        let first = cases.iter().find(|case| case["variant"] == variant.as_str()).unwrap();
        let token = first["input"].as_str().unwrap().trim_start_matches('@');
        let path = dir.join(format!("{variant}.sqlite"));
        support::rebuild_database(&path, fixture["assets"][token]["sql"].as_str().unwrap());
        let conn = rusqlite::Connection::open(&path).unwrap();
        let mut round_now: Option<i64> = None;
        for case in cases.iter().filter(|case| case["variant"] == variant.as_str()) {
            let round = case["round"].as_i64().unwrap();
            if round_now != Some(round) {
                // Every case of a round carries the statements that produced it; apply them once.
                if round > 0 {
                    apply(&conn, &case["log"]);
                }
                round_now = Some(round);
            }
            checked += 1;
            let now = case["now"].as_f64().unwrap();
            let output = support::run(BINARY, &["--db", path.to_str().unwrap(), "--now-ms", &format!("{now}"), "--dump-snapshot"], &home, &[]);
            let named = format!("{variant} round {round} clock {:+}ms", case["clock"].as_i64().unwrap());
            let expected = if variant == "malformed" {
                assert!(case["error"].as_str().unwrap_or("").contains("malformed JSON"));
                // Deliberate difference from oracle/: skipping payload_json parsing on displaced
                // out-of-cut rows means Rust succeeds on `malformed` round 0 and matches the
                // recorded `t3` round-0 snapshot at the same clock.
                cases
                    .iter()
                    .find(|c| c["variant"] == "t3" && c["round"] == 0 && c["clock"] == case["clock"])
                    .expect("recorded t3 round 0 case at matching clock")
            } else if case["error"].as_str().is_some() {
                // The original failed: `--dump-snapshot` prints an empty snapshot stamped at 0.
                let failed = output.status.code() != Some(0)
                    || serde_json::from_slice::<Value>(&output.stdout).map(|v| v["at"].as_f64() == Some(0.0) && v["agents"].as_array().is_some_and(|a| a.is_empty())).unwrap_or(false);
                if !failed {
                    failures.push(format!("{named}: the original reported {:?}, the Rust reader returned a snapshot", case["error"].as_str()));
                }
                continue;
            } else {
                case
            };
            if output.status.code() != Some(0) {
                failures.push(format!("{named}: Rust failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
                continue;
            }
            let dump: Value = match serde_json::from_slice(&output.stdout) {
                Ok(value) => value,
                Err(error) => {
                    failures.push(format!("{named}: {error}"));
                    continue;
                }
            };
            if dump["at"].as_f64() == Some(0.0) && dump["agents"].as_array().is_some_and(|a| a.is_empty()) {
                failures.push(format!("{named}: Rust reported a failed read; expected a snapshot"));
                continue;
            }
            let snapshot = support::normalize(&dump, true);
            let digest = support::canonical_digest(&snapshot);
            if digest != expected["digest"].as_str().unwrap() {
                let agents = snapshot["agents"].as_array().unwrap();
                let items: usize = agents.iter().map(|a| a["items"].as_array().map_or(0, Vec::len)).sum();
                let events: usize = agents.iter().map(|a| a["events"].as_array().map_or(0, Vec::len)).sum();
                let plans = agents.iter().filter(|a| !a["plan"].is_null()).count();
                failures.push(format!(
                    "{named}: snapshot {digest} instead of {}; rust has {} agents, {items} items, {events} events, {plans} plans (recorded {}/{}/{}/{})",
                    expected["digest"],
                    agents.len(),
                    expected["agents"], expected["items"], expected["events"], expected["plans"]
                ));
            }
            // The counts are part of the recording too, so a digest collision cannot hide a shrunken read.
            let agents = snapshot["agents"].as_array().unwrap();
            let items: usize = agents.iter().map(|a| a["items"].as_array().map_or(0, Vec::len)).sum();
            let events: usize = agents.iter().map(|a| a["events"].as_array().map_or(0, Vec::len)).sum();
            let plans = agents.iter().filter(|a| !a["plan"].is_null()).count();
            if Some(agents.len() as i64) != expected["agents"].as_i64() || Some(items as i64) != expected["items"].as_i64() || Some(events as i64) != expected["events"].as_i64() || Some(plans as i64) != expected["plans"].as_i64() {
                failures.push(format!("{named}: counts {} agents, {items} items, {events} events, {plans} plans differ from the recording", agents.len()));
            }
        }
        drop(conn);
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "{} of {checked} full-reader cases differ (recorded from {}):\n{}",
        failures.len(),
        support::recorded_from(&fixture),
        failures.iter().take(6).cloned().collect::<Vec<_>>().join("\n")
    );
    assert_eq!(checked, 39, "the workload holds 39 cases");
    println!("{checked} full-reader cases match the recording");
}
