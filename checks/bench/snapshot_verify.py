"""Shared helpers for snapshot-consistency.py and snapshot-torture.py. Python 3 standard library only.

Every program run goes through `run()`: a clean environment (env -i style) whose HOME,
T3CODE_HOME and TMPDIR point into the scratch folder, pinned with taskset and niced.
Nothing here opens a real T3 database: every database is built by `checks/scripts/scale-db.ts`
or by these scripts, and a path under ~/.t3 is refused wherever one could be given.
"""
import os
import shutil
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
P = "orchestration_v2_projection_"
NOW = 1794484800000  # the fixed epoch of checks/fixtures/workloads.json
NODE = ["node", "--experimental-ffi", "--disable-warning=ExperimentalWarning"]
DEFAULT_CPUS = "8-13"


def refuse_t3(path, what="path"):
    """Exit when `path` is, or lies inside, ~/.t3 (T3 Code's data folder)."""
    t3 = Path(os.path.expanduser("~/.t3")).resolve()
    resolved = Path(path).expanduser().resolve()
    if resolved == t3 or t3 in resolved.parents:
        raise SystemExit(f"refusing {what} under {t3}: {resolved}")
    return resolved


def iso(ms):
    ms = int(ms)
    t = time.gmtime(ms // 1000)
    return time.strftime("%Y-%m-%dT%H:%M:%S", t) + ".%03dZ" % (ms % 1000)


def resolve_scratch(arg, name):
    """The scratch folder: --scratch, or $TMPDIR/NAME. Never /tmp on its own, never under ~/.t3."""
    if arg:
        scratch = Path(arg)
    elif os.environ.get("TMPDIR"):
        scratch = Path(os.environ["TMPDIR"]) / name
    else:
        raise SystemExit("set TMPDIR or pass --scratch")
    scratch = refuse_t3(scratch, "scratch folder")
    for d in ("home", "t3home", "tmp"):
        (scratch / d).mkdir(parents=True, exist_ok=True)
    return scratch


def clean_env(scratch, extra=None):
    scratch = Path(scratch)
    env = {"HOME": str(scratch / "home"), "PATH": "/usr/bin:/bin", "TERM": "xterm-256color",
           "T3CODE_HOME": str(scratch / "t3home"), "TMPDIR": str(scratch / "tmp"), "TZ": "UTC", "LANG": "C.UTF-8"}
    env.update(extra or {})
    return env


def run(binary, args, env, cpus=DEFAULT_CPUS, timeout=300, nice=True):
    argv = ["taskset", "-c", cpus] + (["nice", "-n", "10"] if nice else []) + [str(binary)] + [str(a) for a in args]
    try:
        p = subprocess.run(argv, env=env, capture_output=True, timeout=timeout)
        return p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired as e:
        return "timeout", e.stdout or b"", e.stderr or b""


def scale_db(tier, out, scratch, cpus=DEFAULT_CPUS):
    """checks/scripts/scale-db.ts, T3's schema, into `out` (cached by tier)."""
    out = Path(out)
    out.parent.mkdir(parents=True, exist_ok=True)
    if out.exists():
        return out
    tmp = out.with_suffix(".building.sqlite")
    remove_db(tmp)
    env = dict(os.environ, TMPDIR=str(Path(scratch) / "tmp"), T3CODE_HOME=str(Path(scratch) / "t3home"))
    subprocess.run(["taskset", "-c", cpus, "nice", "-n", "10"] + NODE + [str(ROOT / "checks/scripts/scale-db.ts"),
                    "--tier", tier, "--schema", "t3", "--db", str(tmp)], cwd=ROOT, env=env, check=True,
                   stdout=subprocess.DEVNULL)
    c = sqlite3.connect(tmp)
    c.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    c.close()
    for suffix in ("-wal", "-shm"):
        Path(str(tmp) + suffix).unlink(missing_ok=True)
    tmp.rename(out)
    return out


def copy_db(src, dst):
    """A consistent copy through the backup API (never a raw file copy of a live database)."""
    dst = Path(dst)
    remove_db(dst)
    s = sqlite3.connect(f"file:{src}?mode=ro", uri=True)
    d = sqlite3.connect(dst)
    s.backup(d)
    s.close()
    d.execute("PRAGMA journal_mode=WAL")
    d.close()
    return dst


def schema_version_db(seed, out, version="main-096"):
    """T3's schema at `version` (checks/fixtures/t3-schema-VERSION.sql), rows copied from `seed`."""
    out = Path(out)
    remove_db(out)
    db = sqlite3.connect(out)
    db.executescript((ROOT / "checks/fixtures" / f"t3-schema-{version}.sql").read_text())
    src = sqlite3.connect(f"file:{seed}?mode=ro", uri=True)
    for (table,) in src.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'").fetchall():
        if table == "effect_sql_migrations":
            continue
        cols = [r[1] for r in db.execute(f"PRAGMA table_info({table})")]
        old = {r[1] for r in src.execute(f"PRAGMA table_info({table})")}
        cols = [c for c in cols if c in old]
        if not cols:
            continue
        names = ",".join(cols)
        rows = src.execute(f"SELECT {names} FROM {table}").fetchall()
        db.executemany(f"INSERT INTO {table}({names}) VALUES ({','.join('?' for _ in cols)})", rows)
    db.commit()
    src.close()
    db.execute("PRAGMA journal_mode=WAL")
    db.close()
    return out


def remove_db(path):
    for suffix in ("", "-wal", "-shm", "-journal"):
        Path(str(path) + suffix).unlink(missing_ok=True)


def check_binary(path, what):
    path = refuse_t3(path, what)
    if not (path.is_file() and os.access(path, os.X_OK)):
        raise SystemExit(f"{what} is not an executable file: {path}")
    return path


def rmtree_force(path):
    """Remove a folder made of read-only parts (the special states chmod some of theirs)."""
    path = Path(path)
    if not path.exists():
        return
    for p in [path, *path.rglob("*")]:
        try:
            p.chmod(0o755 if p.is_dir() else 0o644)
        except OSError:
            pass
    shutil.rmtree(path)


def say(*parts):
    print(*parts, flush=True)
    sys.stdout.flush()
