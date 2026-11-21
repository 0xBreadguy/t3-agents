#!/usr/bin/env python3
"""Base against candidate on the same cases, in rotating order: time, CPU, peak memory, output hashes.

Cases (one process per observation, run to completion unless noted):

  status warm, once warm, recap warm, recap this week warm, tools warm
  watcher first line      `--status --watch`: time to the first line, CPU by then and once settled
  once cold, recap cold   after the file cache dropped the database (a database only; --db)
  board screen            the calm board in a private tmux at the fixed clock: a hash of the pane
  board open              the live board in a private tmux: launch CPU, then CPU over a window

Each case runs every arm once per round, the order rotating from round to round after a warm-up
round that is not kept. Program output is hashed in memory and never printed or stored: a
report holds times, CPU, memory and SHA-256 digests only, and `same output` says whether both
arms produced the same digest and exit code in every round.

CPU of a running process is the sum of its threads' run time in nanoseconds from
/proc/PID/task/*/schedstat (the kernel's own accounting, finer than the 10 ms tick of
/proc/PID/stat, which is used only when schedstat cannot be read). It counts the threads alive
when sampled. One-shot runs use the child's resource usage (user plus system).

  python3 compare.py --base BIN --cand BIN (--demo | --db PATH --now-ms MS)
                     [--quick] [--lock FILE] [--cpus 1-4,6,7] [--out FILE.json]

--demo   the synthetic demo data (--now-ms optional; default 1795163100000). There is no cold read.
--db     a database to read, read-only. Never one under ~/.t3: the script refuses it.
--quick  one round per case and short windows: a smoke run, not a measurement.
--lock   a file the caller holds with flock (`flock FILE python3 compare.py ...`); checked.
--cpus   pin this script, and so every process it starts, to these cores (ranges allowed).
Scratch folders are made under $TMPDIR (it must be set, and short enough for a tmux socket).
"""
import argparse
import fcntl
import hashlib
import json
import os
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

DEMO_NOW = 1795163100000
TICK = os.sysconf("SC_CLK_TCK")
NAMES = ["base", "cand"]


def refuse_t3(path, what):
    t3 = Path(os.path.expanduser("~/.t3")).resolve()
    resolved = Path(path).expanduser().resolve()
    if resolved == t3 or t3 in resolved.parents:
        raise SystemExit(f"refusing {what} under {t3}: {resolved}")
    return resolved


def cpu_set(spec):
    out = set()
    for part in spec.split(","):
        lo, _, hi = part.partition("-")
        out.update(range(int(lo), int(hi or lo) + 1))
    return out


def log(msg):
    print(time.strftime("%H:%M:%S"), msg, file=sys.stderr, flush=True)


def proc_cpu_ms(pid):
    """CPU time of a running process in milliseconds: schedstat nanoseconds over its threads."""
    total = 0
    seen = False
    for path in Path(f"/proc/{pid}/task").glob("*/schedstat"):
        try:
            total += int(path.read_text().split()[0])
            seen = True
        except (OSError, ValueError, IndexError):
            continue  # a thread that ended between the listing and the read
    if seen:
        return total / 1e6
    with open(f"/proc/{pid}/stat") as f:  # no schedstat: the tick-resolution total
        rest = f.read().rsplit(")", 1)[1].split()
    return (int(rest[11]) + int(rest[12])) * 1000.0 / TICK


def proc_mem(pid):
    out = {"peak_mib": None, "rss_mib": None, "pss_mib": None}
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmHWM:"):
                out["peak_mib"] = int(line.split()[1]) / 1024.0
            elif line.startswith("VmRSS:"):
                out["rss_mib"] = int(line.split()[1]) / 1024.0
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                if line.startswith("Pss:"):
                    out["pss_mib"] = int(line.split()[1]) / 1024.0
    except OSError:
        pass
    return out


class Bench:
    def __init__(self, a, binaries, data_args, now_ms, db, scratch):
        self.quick = a.quick
        self.bin = binaries
        self.data = data_args
        self.now = str(now_ms)
        self.db = db
        self.scratch = scratch
        self.env = {"PATH": "/usr/bin:/bin", "HOME": str(scratch / "home"), "T3CODE_HOME": str(scratch / "t3"),
                    "TMPDIR": str(scratch / "tmp"), "LC_ALL": "C.UTF-8", "TZ": "UTC", "TERM": "xterm-256color",
                    "COLUMNS": "120", "LINES": "40"}
        self.sock = scratch / "b.s"
        if len(str(self.sock)) > 100:
            raise SystemExit(f"TMPDIR is too deep for a tmux socket: {self.sock}")

    def args(self, *head):
        return [*head, *self.data, "--now-ms", self.now]

    # ------------------------------------------------------------------ one-shot runs
    def one_shot(self, name, args):
        """One run to completion: wall ms, CPU ms, peak RSS, hashes of both streams, exit code."""
        err_path = self.scratch / "tmp" / f"err.{os.getpid()}"
        with open(err_path, "wb") as err:
            t0 = time.perf_counter()
            p = subprocess.Popen([str(self.bin[name]), *args], env=self.env, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=err)
            out = p.stdout.read()
            _, status, ru = os.wait4(p.pid, 0)
            t1 = time.perf_counter()
        p.returncode = os.waitstatus_to_exitcode(status)
        err_bytes = err_path.read_bytes()
        err_path.unlink()
        return {"ms": (t1 - t0) * 1000.0, "cpu_ms": (ru.ru_utime + ru.ru_stime) * 1000.0, "peak_mib": ru.ru_maxrss / 1024.0,
                "out": hashlib.sha256(out).hexdigest(), "out_bytes": len(out),
                "err": hashlib.sha256(err_bytes).hexdigest(), "exit": p.returncode}

    def evict(self):
        """Push the database out of the file cache (no root needed for its owner)."""
        for suffix in ("", "-wal", "-shm"):
            try:
                fd = os.open(str(self.db) + suffix, os.O_RDONLY)
            except OSError:
                continue
            try:
                os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            finally:
                os.close(fd)

    def cached_fraction(self):
        try:
            out = subprocess.run(["fincore", "--bytes", "--noheadings", "--output", "RES,SIZE", str(self.db)],
                                 capture_output=True, text=True, timeout=20).stdout.split()
            return round(int(out[0]) / int(out[1]), 4)
        except Exception:
            return None

    # ------------------------------------------------------------------ the watcher
    @staticmethod
    def stop(p):
        try:
            os.kill(p.pid, signal.SIGTERM)
            p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.kill(p.pid, signal.SIGKILL)
            p.wait()
        except ProcessLookupError:
            pass

    def watcher(self, name, settle_s):
        """The bar's watcher: time to its first line, then what it has cost by `settle_s`."""
        t0 = time.perf_counter()
        p = subprocess.Popen([str(self.bin[name]), *self.args("--status", "--watch")], env=self.env,
                             stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        try:
            line = p.stdout.readline()
            t1 = time.perf_counter()
            if not line:
                raise RuntimeError(f"{name}: the watcher printed nothing, exit {p.wait(timeout=5)}")
            cpu_first = proc_cpu_ms(p.pid)
            while time.perf_counter() - t0 < settle_s:
                time.sleep(0.05)
            mem = proc_mem(p.pid)
            return {"ms": (t1 - t0) * 1000.0, "cpu_ms_at_first_line": cpu_first, "cpu_ms_settled": proc_cpu_ms(p.pid),
                    "peak_mib": mem["peak_mib"], "pss_mib": mem["pss_mib"],
                    "out": hashlib.sha256(line).hexdigest(), "out_bytes": len(line)}
        finally:
            self.stop(p)

    # ------------------------------------------------------------------ the board in tmux
    def tmux(self, *args):
        env = {"PATH": "/usr/bin:/bin", "HOME": self.env["HOME"], "TMPDIR": self.env["TMPDIR"],
               "LC_ALL": "C.UTF-8", "TERM": "xterm-256color"}
        return subprocess.run(["tmux", "-f", "/dev/null", "-S", str(self.sock), *args], env=env, capture_output=True)

    def board_start(self, name, extra_args=(), extra_env=None, cols=190, rows=60):
        self.sock.unlink(missing_ok=True)
        env = {"PATH": "/usr/bin:/bin", "HOME": self.env["HOME"], "T3CODE_HOME": self.env["T3CODE_HOME"],
               "TMPDIR": self.env["TMPDIR"], "LC_ALL": "C.UTF-8", "TZ": "UTC", "TERM": "tmux-256color",
               "COLORTERM": "truecolor"}
        env.update(extra_env or {})
        cmd = " ".join(["exec", "env", "-i", *[f"{k}={v}" for k, v in env.items()], str(self.bin[name]),
                        *self.data, *extra_args])
        t0 = time.perf_counter()
        r = self.tmux("new-session", "-d", "-s", "b", "-x", str(cols), "-y", str(rows), cmd)
        if r.returncode != 0:
            raise RuntimeError(f"tmux did not start: {r.stderr[:200]!r}")
        pid = int(self.tmux("list-panes", "-t", "b", "-F", "#{pane_pid}").stdout.split()[0])
        return t0, pid

    def board_stop(self, pid):
        self.tmux("kill-server")
        deadline = time.time() + 5
        while time.time() < deadline and os.path.exists(f"/proc/{pid}"):
            time.sleep(0.05)
        if os.path.exists(f"/proc/{pid}"):
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        time.sleep(0.2)

    def board_open(self, name, settle_s, window_s):
        """The open board, default motion, wall clock: launch CPU, then CPU over a window."""
        t0, pid = self.board_start(name)
        try:
            while time.perf_counter() - t0 < settle_s:
                time.sleep(0.05)
            cpu_a, ta = proc_cpu_ms(pid), time.perf_counter()
            while time.perf_counter() - ta < window_s:
                time.sleep(0.05)
            cpu_b, tb = proc_cpu_ms(pid), time.perf_counter()
            mem = proc_mem(pid)
            return {"launch_cpu_ms": cpu_a, "open_cpu_ms": cpu_b - cpu_a,
                    "open_pct_of_core": (cpu_b - cpu_a) / ((tb - ta) * 1000.0) * 100.0,
                    "peak_mib": mem["peak_mib"], "pss_mib": mem["pss_mib"]}
        finally:
            self.board_stop(pid)

    def board_screen(self, name):
        """The settled calm board at the fixed clock: a hash of what the pane shows."""
        t0, pid = self.board_start(name, extra_args=("--calm",), extra_env={"T3_FIXED_NOW": self.now})
        try:
            time.sleep(3.0 if self.quick else 6.0)
            a = self.tmux("capture-pane", "-t", "b", "-p", "-e", "-N").stdout
            time.sleep(1.0)
            b = self.tmux("capture-pane", "-t", "b", "-p", "-e", "-N").stdout
            return {"out": hashlib.sha256(b).hexdigest(), "out_bytes": len(b), "stable": a == b}
        finally:
            self.board_stop(pid)

    # ------------------------------------------------------------------ the rounds
    def series(self, label, fn, rounds, warmup=1, before=None):
        """`rounds` measured rounds in rotating order, after `warmup` discarded ones."""
        log(f"{label}: {warmup} warm-up + {rounds} rounds")
        data = {n: [] for n in NAMES}
        for r in range(-warmup, rounds):
            k = (r + warmup) % len(NAMES)
            for n in NAMES[k:] + NAMES[:k]:
                if before:
                    before()
                s = fn(n)
                if r >= 0:
                    data[n].append(s)
        return data


def med(values):
    values = [v for v in values if v is not None]
    if not values:
        return "n/a"
    return f"{statistics.median(values):.1f} [{min(values):.1f}-{max(values):.1f}]"


def report(cases):
    for case, data in cases.items():
        print(case)
        base_out = {(s.get("out"), s.get("err"), s.get("exit")) for s in data["base"]}
        fields = [k for k, v in data["base"][0].items() if isinstance(v, (int, float)) and not isinstance(v, bool)
                  and k not in ("out_bytes", "exit")]
        for n in NAMES:
            seen = {(s.get("out"), s.get("err"), s.get("exit")) for s in data[n]}
            if "out" not in data[n][0]:
                same = ""
            elif seen == base_out and len(seen) == 1:
                same = "same output"
            else:
                same = f"OUTPUT DIFFERS ({len(seen)} distinct)"
            print(f"  {n:5s} {same:22s} " + " | ".join(f"{k} {med([s[k] for s in data[n]])}" for k in fields))
        ratios = []
        for k in fields:
            b, c = (statistics.median([s[k] for s in data[n] if s[k] is not None] or [0]) for n in NAMES)
            if b:
                ratios.append(f"{k} {c / b:.2f}x")
        print("  cand/base medians: " + ", ".join(ratios))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", type=Path, required=True)
    ap.add_argument("--cand", type=Path, required=True)
    what = ap.add_mutually_exclusive_group(required=True)
    what.add_argument("--demo", action="store_true")
    what.add_argument("--db", type=Path)
    ap.add_argument("--now-ms", type=int)
    ap.add_argument("--quick", action="store_true")
    ap.add_argument("--lock", type=Path)
    ap.add_argument("--cpus", help="core list, e.g. 1-4,6,7")
    ap.add_argument("--out", type=Path)
    a = ap.parse_args()

    binaries = {"base": refuse_t3(a.base, "--base"), "cand": refuse_t3(a.cand, "--cand")}
    for name, path in binaries.items():
        if not (path.is_file() and os.access(path, os.X_OK)):
            raise SystemExit(f"--{name} is not an executable file: {path}")
    if a.out:
        refuse_t3(a.out, "--out")
    db = None
    if a.db:
        db = refuse_t3(a.db, "--db")
        if not db.is_file():
            raise SystemExit(f"no such database: {db}")
        if a.now_ms is None:
            raise SystemExit("--db needs --now-ms")
        data_args, now_ms = ["--db", str(db)], a.now_ms
    else:
        data_args, now_ms = ["--demo"], a.now_ms if a.now_ms is not None else DEMO_NOW
    if not os.environ.get("TMPDIR"):
        raise SystemExit("set TMPDIR (scratch folders are made there)")
    stage = refuse_t3(os.environ["TMPDIR"], "TMPDIR")
    if a.cpus:
        os.sched_setaffinity(0, cpu_set(a.cpus))
    if a.lock:
        with a.lock.open("rb") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                pass  # held by the caller, as it should be
            else:
                raise SystemExit("the lock is not held: run this under `flock FILE`")

    scratch = Path(tempfile.mkdtemp(prefix="c-", dir=stage))
    try:
        for d in ("home", "t3", "tmp"):
            (scratch / d).mkdir()
        bench = Bench(a, binaries, data_args, now_ms, db, scratch)
        have_tmux = shutil.which("tmux") is not None
        results = {"quick": a.quick, "data": "demo" if a.demo else "db", "now_ms": now_ms,
                   "cores": sorted(os.sched_getaffinity(0)), "started": time.strftime("%Y-%m-%d %H:%M:%S %z"),
                   "load_start": os.getloadavg(), "bins": {}, "cases": {}}
        for n, path in binaries.items():
            with open(path, "rb") as f:
                results["bins"][n] = {"sha256": hashlib.file_digest(f, "sha256").hexdigest(), "bytes": path.stat().st_size}
        rounds = 1 if a.quick else 7
        cold_rounds = 1 if a.quick else 5
        cases = results["cases"]

        def shot(*head):
            return lambda n: bench.one_shot(n, bench.args(*head))
        cases["status warm"] = bench.series("status warm", shot("--status"), rounds)
        cases["once warm"] = bench.series("once warm", shot("--once"), rounds)
        cases["recap last week warm"] = bench.series("recap warm", shot("--recap", "--json"), rounds)
        cases["recap this week warm"] = bench.series("recap this week warm", shot("--recap", "--this-week", "--json"), rounds)
        cases["tools last week warm"] = bench.series("tools warm", shot("--tools", "--json"), rounds)
        cases["watcher first line"] = bench.series("watcher", lambda n: bench.watcher(n, 2.0 if a.quick else 6.0), rounds)
        if db:
            results["cached_before_cold"] = bench.cached_fraction()
            bench.evict()
            results["cached_after_evict"] = bench.cached_fraction()
            cases["once cold"] = bench.series("once cold", shot("--once"), cold_rounds, warmup=0, before=bench.evict)
            cases["recap last week cold"] = bench.series("recap cold", shot("--recap", "--json"), cold_rounds,
                                                         warmup=0, before=bench.evict)
            bench.one_shot("base", bench.args("--once"))  # warm the file again before the board cases
        if have_tmux:
            cases["board screen (calm, fixed clock)"] = {n: [bench.board_screen(n)] for n in NAMES}
            cases["board open"] = bench.series("board open", lambda n: bench.board_open(n, 4.0 if a.quick else 12.0,
                                                                                        5.0 if a.quick else 30.0),
                                               cold_rounds, warmup=0)
        else:
            results["skipped"] = ["board screen", "board open: tmux is not installed"]
        results["finished"] = time.strftime("%Y-%m-%d %H:%M:%S %z")
        results["load_end"] = os.getloadavg()
        if a.out:
            a.out.write_text(json.dumps(results, indent=1) + "\n")
        print()
        report(cases)
        print("\nloads", results["load_start"], results["load_end"],
              "cached before/after evict", results.get("cached_before_cold"), results.get("cached_after_evict"))
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    main()
