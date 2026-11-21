#!/usr/bin/env python3
"""Snapshot-consistency torture test for the full read (`source::snapshot`).

A writer process commits as fast as it is allowed to. Every transaction N touches one row
in every table the full read reads and stamps N into a field of each row that reaches the
program's output through a different query:

  query (source.rs)            row stamped                      where N shows up
  threads payloads             thread T payload title           agents[T].title, --once, --status
  runs payloads (latest run)   run R modelSelection.model       agents[T].run.modelSelection.model
  spans (runs again)           run R payload startedAt          agents[T].spans[0][0], run.startedAt
  subagents payloads           subagent S payload title         agents[S].title, --once, --status
  provider_threads payloads    S's provider thread contextUsage agents[S].context.usedTokens
  latest provider turn         latest turn payload model        agents[T].turn.model
  history (provider turns)     latest turn tokenUsage           agents[T].history[-1].used
  nodes                        node F's parent flips by parity  item X sits under S (N odd) or T (N even)
  projects                     project title                    agents[T].project
  runtime requests             pending request created_at       agents[T].asked
  items (LIMIT 3000)           dynamic_tool toolName            item label/tool, item X label
  items, every run id          one tool item in the latest run  each thread's items[].label
                               of every active thread           (catches slices from different snapshots)
  items                        command_execution input          item command
  events (LIMIT 8000)          x_probe item updated_at          agents[T].events[cat=other].t
  plans (todo lists)           todo list first step text        agents[T].plan[0].text
  ask cards                    link_request status by parity    --status openAsks (N odd: 1)
  estimated asks (messages)    assistant text, 1+N%3 questions  --status estimatedAsks

A reader loop runs the binary (`--dump-snapshot`, `--once`, `--status` in turn) against the
database again and again while the writer runs, and checks that every stamp in one output
carries the same N (or the same N modulo 2 or 3 for the parity stamps). A read that mixes
snapshots shows two values. The writer also checkpoints with TRUNCATE (and now and then
RESTART) every --checkpoint-every commits, so the WAL is restarted under the readers.

Phases (each binary): writer flat out / every 1 ms / bursts, reader pinned to one core
and to six. --reads is the total per binary, split across the phases.

  python3 snapshot-torture.py --cand BIN [--base BIN] [--scratch DIR] [--reads 20000]
                              [--env NAME=VALUE ...] [--stop-on-first] [--phases flat:8:1,...]
                              [--tier s] [--cpus-one 8] [--cpus-many 8-13]

--cand is the binary under test; --base, when given, is torture-tested the same way for
comparison (a baseline that mixes snapshots is a finding about the baseline, not a failure of
this check on the candidate). The database is built here (scale-db.ts, then moved onto the
main-096 schema); a path under ~/.t3 is refused. The output is counts only.

Exit 0 and PASS when no output mixed snapshots. Read errors (for example SQLITE_BUSY
while a TRUNCATE checkpoint holds its locks) are counted and reported, not failed on,
unless --max-error-rate is exceeded.
"""
import argparse, json, multiprocessing as mp, os, re, sqlite3, sys, threading, time
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from snapshot_verify import (NOW, P, check_binary, clean_env, copy_db, iso, remove_db, resolve_scratch, run,  # noqa: E402
                             scale_db, schema_version_db)

# The stamped rows. Chosen from the database by select_targets(); these are the s tier's.
T, R, S, PT, PROJECT = "t-00000", "r-t-00000-2", "s-00000-000", "pt-t-00000", "atlas"
SPT = "pt-s-00000-000"  # S's provider thread: its turns lose tokenUsage, so contextUsage is what S shows
ROOT_NODE = "n-t-00000"
TOOLISH = "'command_execution','dynamic_tool','file_change','web_search','subagent','handoff','notification','user_input_request','secret_request','link_request','reasoning','assistant_message'"
TURN = "turn-pt-t-00000-5"


def select_targets(c):
    """T: an active thread whose latest run R is running and has a subagent S with a provider thread."""
    for t, r in c.execute(f"""SELECT r.thread_id, r.run_id FROM {P}runs r WHERE r.status='running'
            AND r.ordinal=(SELECT max(ordinal) FROM {P}runs x WHERE x.thread_id=r.thread_id)
            AND EXISTS(SELECT 1 FROM {P}threads t WHERE t.thread_id=r.thread_id AND t.archived_at IS NULL AND t.deleted_at IS NULL)
            ORDER BY r.thread_id""").fetchall():
        sub = c.execute(f"SELECT subagent_id, provider_thread_id FROM {P}subagents WHERE thread_id=? AND run_id=? AND provider_thread_id IS NOT NULL ORDER BY subagent_id LIMIT 1", (t, r)).fetchone()
        pt, project = c.execute(f"SELECT active_provider_thread_id, project_id FROM {P}threads WHERE thread_id=?", (t,)).fetchone()
        turn = c.execute(f"SELECT provider_turn_id FROM {P}provider_turns WHERE provider_thread_id=? ORDER BY ordinal DESC, rowid DESC LIMIT 1", (pt,)).fetchone()
        root = c.execute(f"SELECT node_id FROM {P}nodes WHERE thread_id=? AND parent_node_id IS NULL ORDER BY run_id=? DESC LIMIT 1", (t, r)).fetchone()
        if sub and pt and turn and root:
            return {"T": t, "R": r, "S": sub[0], "SPT": sub[1], "PT": pt, "PROJECT": project, "TURN": turn[0], "ROOT_NODE": root[0]}
    raise SystemExit("no thread with a running latest run, a subagent and a provider turn")


TARGETS = None


def set_targets(tg):
    globals().update(tg)
BASE = NOW - 2 * 3600_000  # inside the 3-hour event window
MARK = re.compile(r"~(\d+)~")


def mark(n):
    return f"~{n}~"


def build(scratch, tier="s"):
    """A scale-db.ts tier, moved onto T3's main-096 schema (messages table and its indexes),
    plus the rows the stamps need. Returns (path, targets)."""
    seed = scale_db(tier, scratch / "dbs" / f"{tier}.sqlite", scratch)
    db_path = scratch / "torture" / f"base-{tier}.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    schema_version_db(seed, db_path, "main-096")
    c = sqlite3.connect(db_path)
    tg = select_targets(c)
    set_targets(tg)
    (db_path.parent / f"base-{tier}.json").write_text(json.dumps(tg))
    q = c.execute
    # T's only non-tool event is x-event (category "other"); S's context comes from its provider thread.
    q(f"DELETE FROM {P}turn_items WHERE thread_id=? AND type NOT IN ({TOOLISH},'checkpoint','todo_list')", (T,))
    q(f"UPDATE {P}provider_turns SET payload_json=json_remove(payload_json,'$.tokenUsage') WHERE provider_thread_id=?", (SPT,))
    q(f"""INSERT INTO {P}nodes(node_id,thread_id,run_id,parent_node_id,root_node_id,kind,status,payload_json)
          VALUES ('n-flip',?,?,?,?,'tool','running','{{}}')""", (T, R, S, ROOT_NODE))
    items = [
        # id, node, type, status, updated_at, ordinal, payload
        ("x-tool", None, "dynamic_tool", "running", iso(NOW - 60_000), 9001, {"toolName": "tool"}),
        ("x-cmd", None, "command_execution", "running", iso(NOW - 61_000), 9002, {"input": "echo", "startedAt": iso(NOW - 70_000)}),
        ("x-flip", "n-flip", "dynamic_tool", "running", iso(NOW - 62_000), 9003, {"toolName": "flip"}),
        ("x-event", None, "x_probe", "completed", iso(BASE), 9004, {"startedAt": iso(BASE)}),
        ("x-todo", None, "todo_list", "running", iso(NOW - 30_000), 9005, {"steps": [{"text": "step", "status": "in_progress"}]}),
        ("x-link", ROOT_NODE, "link_request", "waiting", iso(NOW - 3600_000), 9006, {"startedAt": iso(NOW - 3600_000)}),
    ]
    for iid, node, typ, status, at, ordinal, payload in items:
        q(f"""INSERT INTO {P}turn_items(turn_item_id,thread_id,run_id,node_id,provider_thread_id,ordinal,type,status,updated_at,payload_json)
              VALUES (?,?,?,?,?,?,?,?,?,?)""", (iid, T, R, node, PT, ordinal, typ, status, at, json.dumps(payload)))
    # One stamped tool item in the latest run of every active thread: a read that takes the item
    # query's run-id slices from different snapshots shows different N across agents.
    latest = c.execute(f"""SELECT r.thread_id, r.run_id FROM {P}runs r JOIN {P}threads t ON t.thread_id=r.thread_id
        WHERE t.archived_at IS NULL AND t.deleted_at IS NULL
          AND r.ordinal=(SELECT max(ordinal) FROM {P}runs x WHERE x.thread_id=r.thread_id) ORDER BY r.run_id""").fetchall()
    for k, (tid, rid) in enumerate(latest):
        q(f"""INSERT INTO {P}turn_items(turn_item_id,thread_id,run_id,node_id,provider_thread_id,ordinal,type,status,updated_at,payload_json)
              VALUES (?,?,?,NULL,NULL,?,'dynamic_tool','running',?,?)""", (f"x-slice-{k}", tid, rid, 9100 + k, iso(NOW - 50_000 - k), json.dumps({"toolName": "slice"})))
    tg["SLICES"] = len(latest)
    (db_path.parent / f"base-{tier}.json").write_text(json.dumps(tg))
    q(f"""INSERT INTO {P}runtime_requests(runtime_request_id,thread_id,node_id,kind,status,created_at,payload_json)
          VALUES ('x-req',?,?,'approval','pending',?,'{{}}')""", (T, ROOT_NODE, iso(BASE)))
    q(f"""INSERT INTO {P}messages(message_id,thread_id,run_id,node_id,role,streaming,created_at,updated_at,payload_json)
          VALUES ('x-msg',?,?,NULL,'assistant',0,?,?,?)""", (T, R, iso(NOW - 120_000), iso(NOW - 120_000),
                                                            json.dumps({"text": "Shall I go on?", "createdBy": "assistant"})))
    c.commit()
    c.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    c.close()
    return db_path, tg


def stamp(c, n):
    """One transaction: every stamped row set to N."""
    m = mark(n)
    q = c.execute
    q("BEGIN IMMEDIATE")
    q(f"UPDATE {P}threads SET title=?, payload_json=json_set(payload_json,'$.title',?) WHERE thread_id=?", (m + " thread", m + " thread", T))
    q(f"""UPDATE {P}runs SET payload_json=json_set(payload_json,'$.modelSelection.model',?,'$.startedAt',?) WHERE run_id=?""",
      ("model" + m, iso(BASE + n), R))
    q(f"UPDATE {P}subagents SET payload_json=json_set(payload_json,'$.title',?) WHERE subagent_id=?", (m + " sub", S))
    q(f"UPDATE {P}provider_threads SET payload_json=json_set(payload_json,'$.contextUsage.usedTokens',?) WHERE provider_thread_id=?", (n, SPT))
    q(f"""UPDATE {P}provider_turns SET payload_json=json_set(payload_json,'$.model',?,'$.tokenUsage.usedTokens',?)
          WHERE provider_turn_id=?""", ("turn" + m, n, TURN))
    q(f"UPDATE {P}nodes SET parent_node_id=? WHERE node_id='n-flip'", (S if n % 2 else ROOT_NODE,))
    q("UPDATE projection_projects SET title=? WHERE project_id=?", ("proj" + m, PROJECT))
    q(f"UPDATE {P}runtime_requests SET created_at=? WHERE runtime_request_id='x-req'", (iso(BASE + n),))
    q(f"UPDATE {P}turn_items SET payload_json=json_set(payload_json,'$.toolName',?) WHERE turn_item_id='x-tool'", ("tool" + m,))
    q(f"UPDATE {P}turn_items SET payload_json=json_set(payload_json,'$.toolName',?) WHERE turn_item_id='x-flip'", ("flip" + m,))
    q(f"UPDATE {P}turn_items SET payload_json=json_set(payload_json,'$.toolName',?) WHERE turn_item_id LIKE 'x-slice-%'", ("slice" + m,))
    q(f"UPDATE {P}turn_items SET payload_json=json_set(payload_json,'$.input',?) WHERE turn_item_id='x-cmd'", ("echo " + m,))
    q(f"UPDATE {P}turn_items SET updated_at=?, payload_json=json_set(payload_json,'$.startedAt',?) WHERE turn_item_id='x-event'",
      (iso(BASE + n), iso(BASE + n)))
    q(f"UPDATE {P}turn_items SET payload_json=json_set(payload_json,'$.steps[0].text',?) WHERE turn_item_id='x-todo'", ("step" + m,))
    q(f"UPDATE {P}turn_items SET status=? WHERE turn_item_id='x-link'", ("waiting" if n % 2 else "completed",))
    text = " ".join(["Shall I go on?"] * (1 + n % 3))
    q(f"UPDATE {P}messages SET payload_json=json_set(payload_json,'$.text',?) WHERE message_id='x-msg'", (text,))
    q("COMMIT")


# ---------------------------------------------------------------- reading the stamps
def agent_map(snap):
    return {a["id"]: a for a in snap.get("agents", [])}


def dump_stamps(out):
    """Returns (full: {name: N}, mod: {name: (k, residue)}, missing: [names])."""
    snap = json.loads(out)
    if snap.get("at") == 0 and not snap.get("agents"):
        return None
    a = agent_map(snap)
    t, s = a.get(T), a.get(S)
    full, mod, missing = {}, {}, []

    def take(name, value, number=False):
        if value is None:
            missing.append(name)
            return
        if number:
            full[name] = int(round(float(value)))
            return
        m = MARK.findall(str(value))
        if len(m) != 1:
            missing.append(name)
        else:
            full[name] = int(m[0])

    if t is None or s is None:
        return {}, {}, ["agent T or S"]
    take("thread.title", t.get("title"))
    take("run.model", (t.get("run") or {}).get("modelSelection", {}).get("model"))
    started = (t.get("run") or {}).get("startedAt")
    full_started = None
    if started:
        full_started = started
    spans = t.get("spans") or []
    take("spans", spans[0][0] - BASE if spans and spans[0] else None, True)
    take("subagent.title", s.get("title"))
    take("provider_thread.context", (s.get("context") or {}).get("usedTokens"), True)
    take("turn.model", (t.get("turn") or {}).get("model"))
    hist = t.get("history") or []
    take("history.used", hist[-1].get("used") if hist else None, True)
    take("project", t.get("project"))
    asked = t.get("asked")
    if asked:
        take("request.created_at", (iso_ms(asked) - BASE), True)
    else:
        missing.append("request.created_at")
    items = {i.get("label"): i for i in t.get("items", [])}
    labels = [i.get("label", "") for i in t.get("items", [])]
    take("item.tool", next((l for l in labels if l.startswith("tool~")), None))
    cmds = [i.get("command", "") for i in t.get("items", []) if i.get("command", "").startswith("echo ~")]
    take("item.command", cmds[0] if cmds else None)
    ev = [e["t"] for e in t.get("events", []) if e.get("cat") == "other"]
    if len(ev) > 1:
        missing.append("event.unique")
    take("event.t", (max(ev) - BASE) if ev else None, True)
    plan = t.get("plan") or []
    take("plan.step", plan[0].get("text") if plan else None)
    # The node flip: item X sits under S when N is odd, under T when N is even.
    in_t = [i for i in t.get("items", []) if i.get("label", "").startswith("flip~")]
    in_s = [i for i in s.get("items", []) if i.get("label", "").startswith("flip~")]
    if len(in_t) + len(in_s) != 1:
        missing.append("node.flip")
    else:
        x = (in_t or in_s)[0]
        take("flip.label", x.get("label"))
        mod["node.parent"] = (2, 1 if in_s else 0)
    # The per-run stamps, wherever they landed (each under its own thread's agent).
    found = 0
    for ag in snap.get("agents", []):
        for i in ag.get("items", []):
            if str(i.get("label", "")).startswith("slice~"):
                found += 1
                take(f"slice.{ag['id']}.{i.get('run_id')}", i.get("label"))
    if found != globals().get("SLICES", found):
        missing.append(f"slices {found}/{SLICES}")
    del items, full_started
    return full, mod, missing


def iso_ms(s):
    import calendar
    base, frac = s.rstrip("Z").split(".")
    return calendar.timegm(time.strptime(base, "%Y-%m-%dT%H:%M:%S")) * 1000 + int(frac[:3])


def status_stamps(out):
    line = json.loads(out)
    full, mod, missing = {}, {}, []
    by = {}
    for a in line.get("agents", []):
        m = MARK.findall(a.get("title", ""))
        if m:
            by[a["title"].split(" ", 1)[1] if " " in a["title"] else a["title"]] = (int(m[0]), a)
    if "thread" in by:
        n, a = by["thread"]
        full["status.thread.title"] = n
        mod["status.openAsks"] = (2, a.get("openAsks", -1) % 2 if isinstance(a.get("openAsks"), int) else -1)
        est = a.get("estimatedAsks")
        mod["status.estimatedAsks"] = (3, (est - 1) % 3 if isinstance(est, int) and est > 0 else -1)
    else:
        missing.append("status.thread")
    if "sub" in by:
        full["status.sub.title"] = by["sub"][0]
    return full, mod, missing


def once_stamps(out):
    text = out.decode("utf-8", "replace") if isinstance(out, bytes) else out
    values = [int(x) for x in MARK.findall(text)]
    full = {f"once.{i}": v for i, v in enumerate(values)}
    return full, {}, ([] if values else ["once.any"])


def check(mode, rc, out, err):
    """Returns (verdict, n, detail). verdict: ok | mixed | error | nostamp."""
    if rc != 0:
        return "error", None, (err or b"")[-300:].decode("utf-8", "replace")
    try:
        if mode == "dump":
            r = dump_stamps(out)
            if r is None:
                return "error", None, "dump: empty snapshot at 0 (read failed)"
        elif mode == "status":
            r = status_stamps(out)
        else:
            r = once_stamps(out)
    except Exception as e:  # unparsable output is a failure of its own
        return "mixed", None, f"unparsable {mode} output: {e}: {out[:200]!r}"
    full, mod, missing = r
    values = set(full.values())
    if len(values) > 1:
        return "mixed", None, json.dumps({"full": full, "mod": mod})
    if not values:
        return "nostamp", None, ",".join(missing)
    n = values.pop()
    bad = {k: v for k, v in mod.items() if n % v[0] != v[1]}
    if bad:
        return "mixed", n, json.dumps({"full": full, "mod": mod, "bad": bad})
    if mode == "dump" and missing:
        return "nostamp", n, "missing " + ",".join(missing)
    return "ok", n, ""


# ---------------------------------------------------------------- writer
def writer_main(db, rate, counter, stop, checkpoint_every, errors, tg, restarts):
    set_targets(tg)  # the child may be a fresh interpreter (forkserver): pass the targets in
    c = sqlite3.connect(db, isolation_level=None, timeout=0.1)
    # Short: a TRUNCATE checkpoint waits for readers while holding the writer lock; with a long
    # timeout it stalls the commit stream behind six busy readers.
    c.execute("PRAGMA busy_timeout=100")
    c.execute("PRAGMA synchronous=NORMAL")
    n = counter.value
    commits = 0
    while not stop.is_set():
        n += 1
        try:
            stamp(c, n)
        except sqlite3.Error:
            errors.value += 1
            try:
                c.execute("ROLLBACK")
            except sqlite3.Error:
                pass
            n -= 1
            continue
        counter.value = n
        commits += 1
        if checkpoint_every and commits % checkpoint_every == 0:
            mode = "RESTART" if (commits // checkpoint_every) % 4 == 3 else "TRUNCATE"
            try:
                busy = c.execute(f"PRAGMA wal_checkpoint({mode})").fetchone()[0]
                if busy == 0:
                    restarts.value += 1  # the WAL was fully checkpointed and restarted (or truncated)
            except sqlite3.Error:
                errors.value += 1
        if rate == "1ms":
            time.sleep(0.001)
        elif rate == "burst" and commits % 200 == 0:
            time.sleep(0.05)
    c.close()


# ---------------------------------------------------------------- one phase
def phase(binary, db, scratch, env, rate, cpus, parallel, reads, checkpoint_every, stop_on_first, log):
    counter, stop, werr, restarts = mp.Value("q", 0), mp.Event(), mp.Value("q", 0), mp.Value("q", 0)
    c = sqlite3.connect(db)
    start_n = c.execute(f"SELECT CAST(substr(title,2,instr(substr(title,2),'~')-1) AS INTEGER) FROM {P}threads WHERE thread_id=?", (T,)).fetchone()[0]
    c.close()
    counter.value = start_n or 0
    w = mp.Process(target=writer_main, args=(str(db), rate, counter, stop, checkpoint_every, werr, TARGETS, restarts))
    w.start()
    while counter.value <= (start_n or 0) + 5:
        time.sleep(0.01)
    lock = threading.Lock()
    stats = Counter()
    seen = []  # (start_counter, end_counter, n)
    first_mixed = []
    issued = [0]
    modes = ["dump", "once", "status"]

    def worker(k):
        while True:
            with lock:
                if issued[0] >= reads or (stop_on_first and first_mixed):
                    return
                i = issued[0]
                issued[0] += 1
            mode = modes[i % 3]
            flag = {"dump": "--dump-snapshot", "once": "--once", "status": "--status"}[mode]
            before = counter.value
            rc, out, err = run(binary, ["--db", db, "--now-ms", NOW, flag], env, cpus=cpus, timeout=120)
            after = counter.value
            verdict, n, detail = check(mode, rc, out, err)
            with lock:
                stats[verdict] += 1
                stats[f"{mode}.{verdict}"] += 1
                if after != before:
                    stats["overlapped_commit"] += 1
                if n is not None:
                    seen.append((before, after, n))
                if verdict == "mixed" and not first_mixed:
                    first_mixed.append({"read": i + 1, "mode": mode, "detail": detail})
                    (log / f"mixed-{binary.name}-{rate}-{cpus}.out").write_bytes(out)
                if verdict in ("error", "nostamp") and stats[verdict] <= 3:
                    log.joinpath("notes.txt").open("a").write(f"{binary.name} {rate} {cpus} {mode} {verdict}: {detail}\n")
    threads = [threading.Thread(target=worker, args=(k,)) for k in range(parallel)]
    t0 = time.time()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    stop.set()
    w.join(timeout=30)
    if w.is_alive():
        w.terminate()
    elapsed = time.time() - t0
    commits = counter.value - (start_n or 0)
    # Reads ordered by start, counting neighbours that saw different N: a lower bound on how
    # often the database changed between reads.
    ordered = sorted(seen)
    changes = sum(1 for x, y in zip(ordered, ordered[1:]) if x[2] != y[2])
    return {"rate": rate, "cpus": cpus, "parallel": parallel, "reads": sum(stats[v] for v in ("ok", "mixed", "error", "nostamp")),
            "ok": stats["ok"], "mixed": stats["mixed"], "errors": stats["error"], "nostamp": stats["nostamp"],
            "reads_overlapping_a_commit": stats["overlapped_commit"], "consecutive_reads_with_different_n": changes,
            "commits": commits, "commits_per_s": round(commits / elapsed, 1),
            "writer_errors": werr.value, "wal_restarts": restarts.value, "seconds": round(elapsed, 1), "first_mixed": first_mixed[0] if first_mixed else None,
            "by_mode": {k: v for k, v in stats.items() if "." in k}}


def default_phases(one, many):
    n = len(cpu_list(many))
    return [("flat", one, 1), ("flat", many, n), ("1ms", one, 1), ("1ms", many, n), ("burst", one, 1), ("burst", many, n)]


def cpu_list(spec):
    out = []
    for part in spec.split(","):
        lo, _, hi = part.partition("-")
        out.extend(range(int(lo), int(hi or lo) + 1))
    return out


def torture(name, binary, scratch, env, reads, checkpoint_every, stop_on_first, phases, tier="s"):
    global TARGETS
    log = scratch / "torture" / f"log-{name}"
    log.mkdir(parents=True, exist_ok=True)
    base = scratch / "torture" / f"base-{tier}.sqlite"
    if base.exists() and base.with_suffix(".json").exists():
        TARGETS = json.loads(base.with_suffix(".json").read_text())
        set_targets(TARGETS)
    else:
        base, TARGETS = build(scratch, tier)
    db = scratch / "torture" / f"live-{name}.sqlite"
    copy_db(base, db)
    c = sqlite3.connect(db, isolation_level=None)
    stamp(c, 1)
    c.close()
    # Coverage: every stamp must reach the output once, on a quiet database.
    cover = {}
    for mode, flag in (("dump", "--dump-snapshot"), ("once", "--once"), ("status", "--status")):
        rc, out, err = run(binary, ["--db", db, "--now-ms", NOW, flag], env)
        verdict, n, detail = check(mode, rc, out, err)
        if mode == "dump":
            full, mod, missing = dump_stamps(out) if rc == 0 else ({}, {}, ["read failed"])
            cover["dump"] = {"verdict": verdict, "stamps": sorted(full) + sorted(mod), "missing": missing}
        elif mode == "status":
            full, mod, missing = status_stamps(out) if rc == 0 else ({}, {}, ["read failed"])
            cover["status"] = {"verdict": verdict, "stamps": sorted(full) + sorted(mod), "missing": missing}
        else:
            cover["once"] = {"verdict": verdict, "marks": len(MARK.findall(out.decode("utf-8", "replace")))}
    results = []
    per = max(1, reads // len(phases))
    for rate, cpus, parallel in phases:
        r = phase(binary, db, scratch, env, rate, cpus, parallel, per, checkpoint_every, stop_on_first, log)
        print(f"  {name:9} writer {rate:5} reader cpus {cpus:5} x{parallel}: reads {r['reads']} ok {r['ok']} mixed {r['mixed']} "
              f"errors {r['errors']} nostamp {r['nostamp']} overlapped {r['reads_overlapping_a_commit']} "
              f"commits {r['commits']} ({r['commits_per_s']}/s) WAL restarts {r['wal_restarts']} {r['seconds']}s"
              + (f"  FIRST MIXED at read {r['first_mixed']['read']} ({r['first_mixed']['mode']})" if r["first_mixed"] else ""), flush=True)
        results.append(r)
        if stop_on_first and r["mixed"]:
            break
    remove_db(db)
    return {"binary": str(binary), "coverage": cover, "phases": results}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", type=Path, help="a baseline binary, tortured the same way for comparison (optional)")
    ap.add_argument("--cand", type=Path, required=True, help="the binary under test")
    ap.add_argument("--scratch", type=Path, help="default $TMPDIR/snapshot-verify")
    ap.add_argument("--reads", type=int, default=20000, help="reads per binary, split over the six phases")
    ap.add_argument("--checkpoint-every", type=int, default=150)
    ap.add_argument("--env", action="append", default=[], help="NAME=VALUE for the candidate only")
    ap.add_argument("--stop-on-first", action="store_true", help="stop a binary at its first mixed read")
    ap.add_argument("--phases", help="subset, e.g. flat:8:1,burst:8-13:6")
    ap.add_argument("--cpus-one", default="8", help="the core a single reader is pinned to")
    ap.add_argument("--cpus-many", default="8-13", help="the cores six readers share")
    ap.add_argument("--max-error-rate", type=float, default=0.05)
    ap.add_argument("--tier", default="s", help="scale-db.ts tier to build on (s: ~10 ms reads; m: ~80 ms)")
    a = ap.parse_args()
    scratch = resolve_scratch(a.scratch, "snapshot-verify")
    (scratch / "torture").mkdir(exist_ok=True)
    phases = default_phases(a.cpus_one, a.cpus_many) if not a.phases else [
        (p.split(":")[0], p.split(":")[1], int(p.split(":")[2])) for p in a.phases.split(",")]
    extra = dict(e.split("=", 1) for e in a.env)
    arms = [("cand", check_binary(a.cand, "--cand"), clean_env(scratch, extra))]
    if a.base:
        arms.insert(0, ("base", check_binary(a.base, "--base"), clean_env(scratch)))
    report, failed = {}, False
    t0 = time.time()
    for name, binary, env in arms:
        print(f"{name}: {binary.name}", flush=True)
        r = torture(name, binary, scratch, env, a.reads, a.checkpoint_every, a.stop_on_first, phases, a.tier)
        report[name] = r
        reads = sum(p["reads"] for p in r["phases"])
        mixed = sum(p["mixed"] for p in r["phases"])
        errors = sum(p["errors"] for p in r["phases"])
        overl = sum(p["reads_overlapping_a_commit"] for p in r["phases"])
        # --status lists a handful of agents; on bigger tiers T may not be among them.
        cov_missing = r["coverage"]["dump"]["missing"] + (r["coverage"]["status"]["missing"] if a.tier == "s" else [])
        print(f"{name}: coverage dump stamps {len(r['coverage']['dump']['stamps'])}, status stamps "
              f"{len(r['coverage']['status']['stamps'])}, once marks {r['coverage']['once']['marks']}"
              + (f", MISSING {cov_missing}" if cov_missing else ""))
        print(f"{name}: reads {reads}, mixed {mixed}, errors {errors}, reads overlapping a commit {overl}")
        bad = bool(mixed or cov_missing or (reads and errors / reads > a.max_error_rate))
        if name == "cand":
            failed = bad
        elif bad:
            print("base: a finding about the baseline; it does not decide this check")
    (scratch / "torture" / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"report: {scratch / 'torture' / 'report.json'}; {round(time.time() - t0)}s")
    print("FAIL" if failed else "PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
