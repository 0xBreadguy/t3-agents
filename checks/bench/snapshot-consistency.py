#!/usr/bin/env python3
"""Base against candidate, byte for byte, on synthetic database states.

For each state both binaries run `--dump-snapshot`, `--once` and `--status` at the same fixed
clock (`--now-ms`) on a quiescent database. stdout, stderr and the exit code must be identical.
Every database is built here, by `checks/scripts/scale-db.ts` and plain SQL. Nothing reads a real
T3 database, and a path under ~/.t3 is refused. Families (`--families`, default all):

  tiers      the s and m tiers, and the m tier on each older schema (checks/fixtures/t3-schema-*.sql)
  stream     scale-db.ts --stream on a copy of the m tier: --stream-writes writes (default 1,000),
             compared after each batch of 1-3 writes (seeded)
  malformed  7 kinds of bad payload_json (truncated, not JSON, JSON5 quotes, JSON5 trailing comma,
             number for string, array, blob) in the first, a middle and the last row of each table
             the read parses
  special    rollback journal, read-only -shm with empty WAL, read-only directory (with and
             without WAL files), WAL header with no WAL file, no rows, only archived threads,
             an empty file, a non-SQLite file, a folder, a missing path, a missing table

  python3 snapshot-consistency.py --base BIN --cand BIN [--scratch DIR] [--env NAME=VALUE ...]
                                  [--families a,b] [--stream-writes 1000] [--cpus 8-13] [--keep-going]

Prints counts only. Exit 0 and PASS when every comparison is identical. Differences are saved
under SCRATCH/consistency/failures/ with both sides' stdout, stderr and exit code (those files
hold what the programs printed for a database built here, never for a real one).
"""
import argparse
import json
import os
import random
import shutil
import sqlite3
import subprocess
import sys
import threading
import time
from collections import Counter, defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from snapshot_verify import (NODE, NOW, P, ROOT, check_binary, clean_env, copy_db, iso, refuse_t3,  # noqa: E402
                             remove_db, resolve_scratch, rmtree_force, run, say, scale_db, schema_version_db)

MODES = (("dump", "--dump-snapshot"), ("once", "--once"), ("status", "--status"))


class Harness:
    def __init__(self, a, scratch):
        self.base, self.cand = check_binary(a.base, "--base"), check_binary(a.cand, "--cand")
        self.scratch = scratch
        self.cpus = a.cpus
        self.benv = clean_env(scratch)
        self.cenv = clean_env(scratch, dict(e.split("=", 1) for e in a.env))
        self.stats = Counter()
        self.states = Counter()
        self.failures = []
        self.keep_going = a.keep_going
        self.fail_dir = scratch / "consistency" / "failures"
        self.notes = defaultdict(list)

    def compare(self, family, label, db, clocks=(NOW,)):
        self.states[family] += 1
        for now in clocks:
            jobs = {}
            for mode, flag in MODES:
                for side, binary, env in (("base", self.base, self.benv), ("cand", self.cand, self.cenv)):
                    jobs[(mode, side)] = (binary, ["--db", db, "--now-ms", now, flag], env)
            results = {}

            def go(key):
                binary, args, env = jobs[key]
                results[key] = run(binary, args, env, cpus=self.cpus, timeout=600)
            threads = [threading.Thread(target=go, args=(k,)) for k in jobs]
            for t in threads:
                t.start()
            for t in threads:
                t.join()
            for mode, _ in MODES:
                b, c = results[(mode, "base")], results[(mode, "cand")]
                self.stats[family] += 1
                if b != c:
                    self.fail(family, label, mode, now, b, c)

    def fail(self, family, label, mode, now, b, c):
        n = len(self.failures) + 1
        what = [name for i, name in enumerate(("exit code", "stdout", "stderr")) if b[i] != c[i]]
        entry = {"n": n, "family": family, "state": label, "mode": mode, "now": now, "differs": what,
                 "base_rc": b[0], "cand_rc": c[0]}
        self.failures.append(entry)
        if n <= 40:
            d = self.fail_dir / f"{n:03d}-{family}-{label}-{mode}".replace("/", "_")[:120]
            d.mkdir(parents=True, exist_ok=True)
            for side, r in (("base", b), ("cand", c)):
                (d / f"{side}.out").write_bytes(r[1])
                (d / f"{side}.err").write_bytes(r[2])
                (d / f"{side}.rc").write_text(str(r[0]))
            (d / "state.json").write_text(json.dumps(entry, indent=1))
        say(f"    DIFF {family}/{label} {mode} now={now}: {', '.join(what)}")
        if not self.keep_going and len(self.failures) >= 200:
            raise SystemExit("too many differences; stopping")


# ---------------------------------------------------------------- tiers
def fam_tiers(h):
    for tier in ("s", "m"):
        db = scale_db(tier, h.scratch / "dbs" / f"{tier}.sqlite", h.scratch, h.cpus)
        h.compare("tiers", tier, db, (NOW, NOW + 3_600_000, NOW + 30 * 60_000))
    m = h.scratch / "dbs" / "m.sqlite"
    work = h.scratch / "consistency" / "tiers"
    work.mkdir(parents=True, exist_ok=True)
    for version in ("main-096", "older-074", "replayed-083", "legacy-054"):
        d = schema_version_db(m, work / f"schema-{version}.sqlite", version)
        h.compare("tiers", f"schema-{version}", d)
        remove_db(d)


# ---------------------------------------------------------------- stream
def fam_stream(h, writes, seed=7):
    base = scale_db("m", h.scratch / "dbs" / "m.sqlite", h.scratch, h.cpus)
    work = h.scratch / "consistency" / "stream"
    rmtree_force(work)
    work.mkdir(parents=True)
    db = work / "m.sqlite"
    copy_db(base, db)
    env_id = base.parent / "environment-id"
    if env_id.exists():
        shutil.copy(env_id, work / "environment-id")
    argv = ["taskset", "-c", h.cpus, "nice", "-n", "10"] + NODE + [str(ROOT / "checks/scripts/scale-db.ts"), "--db", str(db),
                                                                  "--stream", str(seed), "--serve"]
    log = open(work / "stream.log", "w")
    p = subprocess.Popen(argv, cwd=ROOT, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True,
                         env=dict(os.environ, TMPDIR=str(h.scratch / "tmp"), T3CODE_HOME=str(h.scratch / "t3home")))
    ready = json.loads(p.stdout.readline())
    assert ready.get("ready"), ready
    rng = random.Random(seed)
    done, batches, shapes = 0, 0, Counter()
    h.compare("stream", "start", db, (ready["now_ms"],))
    try:
        while done < writes:
            p.stdin.write(f"{rng.choice((1, 1, 2, 3))}\n")
            p.stdin.flush()
            while True:
                rec = json.loads(p.stdout.readline())
                if "batch" in rec:
                    break
                shapes[rec.get("shape")] += 1
            done += rec["writes"]
            batches += 1
            h.compare("stream", f"step{rec['step']}", db, (rec["now_ms"],))
            if batches % 100 == 0:
                say(f"    stream: {done} writes, {batches} comparisons")
    finally:
        p.stdin.close()
        p.wait(timeout=60)
        log.close()
    h.notes["stream"].append({"writes": done, "batches": batches, "shapes": dict(shapes)})
    rmtree_force(work)


# ---------------------------------------------------------------- malformed payloads
BAD = {
    "truncated": '{"title":"x',
    "not-json": "garbage",
    "json5-quotes": "{'title':'x'}",
    "json5-comma": '{"title":"x",}',
    "number-for-string": '{"title":5,"status":7}',
    "array": "[1,2]",
    "blob": b"\x01\x02\x03",
}
PAYLOAD_TABLES = ("threads", "runs", "subagents", "provider_threads", "provider_turns", "turn_items", "nodes", "messages",
                  "runtime_requests")


def fam_malformed(h):
    seed = scale_db("s", h.scratch / "dbs" / "s.sqlite", h.scratch, h.cpus)
    work = h.scratch / "consistency" / "malformed"
    rmtree_force(work)
    work.mkdir(parents=True)
    # The messages table is in the main-096 schema.
    base = schema_version_db(seed, work / "base.sqlite", "main-096")
    for table in PAYLOAD_TABLES:
        c = sqlite3.connect(base)
        try:
            n = c.execute(f"SELECT count(*) FROM {P}{table}").fetchone()[0]
        except sqlite3.Error:
            c.close()
            continue
        c.close()
        if n == 0:
            continue
        for pick, offset in (("first", 0), ("middle", n // 2), ("last", n - 1)):
            for kind, value in BAD.items():
                db = work / "case.sqlite"
                copy_db(base, db)
                c = sqlite3.connect(db)
                try:
                    c.execute(f"UPDATE {P}{table} SET payload_json=? WHERE rowid=(SELECT rowid FROM {P}{table} ORDER BY rowid LIMIT 1 OFFSET ?)",
                              (value, offset))
                    c.commit()
                except sqlite3.Error as e:
                    # The schema itself refuses this payload (a CHECK on json_valid): nothing to compare.
                    h.notes["not_applicable"].append(f"malformed/{table}-{pick}-{kind}: {e}")
                    c.close()
                    remove_db(db)
                    continue
                c.close()
                h.compare("malformed", f"{table}-{pick}-{kind}", db)
                remove_db(db)
    remove_db(base)


# ---------------------------------------------------------------- special databases
def fam_special(h):
    work = h.scratch / "consistency" / "special"
    rmtree_force(work)
    work.mkdir(parents=True)
    s = scale_db("s", h.scratch / "dbs" / "s.sqlite", h.scratch, h.cpus)
    m = scale_db("m", h.scratch / "dbs" / "m.sqlite", h.scratch, h.cpus)

    def fresh(name, src=s):
        return copy_db(src, work / name)

    for src, tag in ((s, "s"), (m, "m")):
        d = fresh(f"delete-{tag}.sqlite", src)
        c = sqlite3.connect(d)
        c.execute("PRAGMA journal_mode=DELETE")
        c.close()
        h.compare("special", f"rollback-journal-{tag}", d)
    # Read-only -shm and an empty WAL, as a frozen copy of a live database looks.
    d = fresh("roshm.sqlite")
    c = sqlite3.connect(d)
    c.execute(f"UPDATE {P}threads SET title=title")
    c.commit()
    c.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    keep = sqlite3.connect(d)  # a second connection, so closing the first leaves -wal and -shm
    keep.execute("SELECT 1 FROM sqlite_master").fetchone()
    c.close()
    copy = work / "roshm-copy.sqlite"
    for suffix in ("", "-wal", "-shm"):
        shutil.copy(str(d) + suffix, str(copy) + suffix)
    keep.close()
    remove_db(d)
    for suffix in ("-shm", "-wal", ""):
        os.chmod(str(copy) + suffix, 0o444)
    h.compare("special", "readonly-shm-empty-wal", copy)
    # Same, with a non-empty WAL holding a commit the main file lacks.
    d2 = fresh("roshm2.sqlite")
    c = sqlite3.connect(d2)
    c.execute("PRAGMA wal_autocheckpoint=0")
    keep = sqlite3.connect(d2)
    keep.execute("SELECT 1 FROM sqlite_master").fetchone()
    c.execute(f"UPDATE {P}threads SET payload_json=json_set(payload_json,'$.title','only in the WAL')")
    c.commit()
    c.close()
    dst = work / "roshm2-copy.sqlite"
    for suffix in ("", "-wal", "-shm"):
        shutil.copy(str(d2) + suffix, str(dst) + suffix)
    keep.close()
    remove_db(d2)
    for suffix in ("", "-wal", "-shm"):
        os.chmod(str(dst) + suffix, 0o444)
    h.compare("special", "readonly-shm-wal-with-commit", dst)
    # Read-only directories.
    for label, journal, leftovers in (("rodir-wal-no-files", "WAL", False), ("rodir-wal-with-files", "WAL", True),
                                      ("rodir-delete", "DELETE", False)):
        dd = work / label
        dd.mkdir()
        d = copy_db(s, dd / "state.sqlite")
        c = sqlite3.connect(d)
        c.execute(f"PRAGMA journal_mode={journal}")
        keep = None
        if leftovers:
            keep = sqlite3.connect(d)
            keep.execute("SELECT 1 FROM sqlite_master").fetchone()
            c.execute(f"UPDATE {P}threads SET title=title")
            c.commit()
            for suffix in ("", "-wal", "-shm"):
                shutil.copy(str(d) + suffix, str(dd / "copy.sqlite") + suffix)
        c.close()
        if keep:
            keep.close()
            remove_db(d)
            d = dd / "copy.sqlite"
        dd.chmod(0o555)
        h.compare("special", label, d)
        dd.chmod(0o755)
    # WAL header, no WAL file, writable folder (a fresh file copy).
    d = fresh("nowal.sqlite")
    shutil.copy(d, work / "nowal-copy.sqlite")
    remove_db(d)
    h.compare("special", "wal-header-no-wal-file", work / "nowal-copy.sqlite")
    # No rows at all; only archived threads; only deleted threads.
    d = work / "empty-schema.sqlite"
    c = sqlite3.connect(d)
    c.executescript((ROOT / "checks/fixtures/t3-schema.sql").read_text())
    c.commit()
    c.close()
    h.compare("special", "no-rows", d)
    for label, col in (("only-archived", "archived_at"), ("only-deleted", "deleted_at")):
        d = fresh(f"{label}.sqlite")
        c = sqlite3.connect(d)
        c.execute(f"UPDATE {P}threads SET {col}=?", (iso(NOW - 1000),))
        c.commit()
        c.close()
        h.compare("special", label, d)
    d = work / "empty.sqlite"
    sqlite3.connect(d).close()
    h.compare("special", "empty-sqlite-file", d)
    d = work / "zero-bytes.sqlite"
    d.write_bytes(b"")
    h.compare("special", "zero-byte-file", d)
    d = work / "not-sqlite.sqlite"
    d.write_bytes(b"this is not a database\n" * 100)
    h.compare("special", "not-a-database", d)
    (work / "folder.sqlite").mkdir()
    h.compare("special", "a-folder", work / "folder.sqlite")
    h.compare("special", "missing-path", work / "does-not-exist.sqlite")
    d = fresh("one-table-missing.sqlite")
    c = sqlite3.connect(d)
    c.execute(f"DROP TABLE {P}runtime_requests")
    c.commit()
    c.close()
    h.compare("special", "one-table-missing", d)
    rmtree_force(work)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", type=Path, required=True, help="the baseline binary")
    ap.add_argument("--cand", type=Path, required=True, help="the candidate binary")
    ap.add_argument("--scratch", type=Path, help="default $TMPDIR/snapshot-verify")
    ap.add_argument("--env", action="append", default=[], help="NAME=VALUE for the candidate only (repeatable)")
    ap.add_argument("--families", default="tiers,special,malformed,stream")
    ap.add_argument("--stream-writes", type=int, default=1000)
    ap.add_argument("--cpus", default="8-13", help="cores for every run (taskset -c list)")
    ap.add_argument("--keep-going", action="store_true", help="do not stop after 200 differences")
    a = ap.parse_args()
    scratch = resolve_scratch(a.scratch, "snapshot-verify")
    (scratch / "consistency").mkdir(exist_ok=True)
    h = Harness(a, scratch)
    t0 = time.time()
    times = {}
    for f in a.families.split(","):
        t = time.time()
        say(f"{f} ...")
        {"tiers": lambda: fam_tiers(h), "stream": lambda: fam_stream(h, a.stream_writes), "special": lambda: fam_special(h),
         "malformed": lambda: fam_malformed(h)}.get(f, lambda: sys.exit(f"unknown family {f}"))()
        times[f] = round(time.time() - t, 1)
        say(f"{f}: {h.states[f]} states, {h.stats[f]} comparisons, "
            f"{sum(1 for x in h.failures if x['family'] == f)} differences, {times[f]}s")
    skipped = len(h.notes.get("not_applicable", []))
    if skipped:
        say(f"not applicable (refused by the schema itself): {skipped} states")
    report = {"base": str(h.base), "cand": str(h.cand), "states": dict(h.states), "comparisons": dict(h.stats),
              "seconds": times, "failures": h.failures, "notes": h.notes}
    (scratch / "consistency" / "report.json").write_text(json.dumps(report, indent=1, default=str) + "\n")
    by_family = Counter(x["family"] for x in h.failures)
    say(f"states {sum(h.states.values())}, comparisons {sum(h.stats.values())}, differences {len(h.failures)} "
        f"{dict(by_family)}, {round(time.time() - t0)}s")
    say("FAIL" if h.failures else "PASS")
    sys.exit(1 if h.failures else 0)


if __name__ == "__main__":
    main()
