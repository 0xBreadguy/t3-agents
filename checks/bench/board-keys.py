#!/usr/bin/env python3
"""The open dashboard in a private PTY, base against candidate: frame bytes, idle CPU, key cost.

Each trial starts one release binary in its own pseudo-terminal with an empty HOME and
T3CODE_HOME, answers its terminal probe, then records:

  - the probe, the first frame, the frame after a resize and the exit output, each as a byte
    count and a SHA-256 digest;
  - a silent hold on the fixed clock (the dashboard must write nothing) and its CPU time;
  - a few keys (`?`, `?`, `j`, `k`), each as the frame it draws (count and digest), the time to
    that frame and the CPU time they took, taken from /proc/PID/task/*/schedstat in nanoseconds.

Base and candidate alternate within each of --pairs pairs, the order flipping every pair. With
the clock pinned the frame digests of both must be identical, in every trial: a difference stops
the run. Nothing the dashboard drew is printed or kept; the report holds counts, times and digests.

  python3 board-keys.py --base BIN --cand BIN (--demo | --db PATH --now-ms MS)
                        [--pairs 5] [--seconds 30] [--key-repeats 8] [--calm] [--live-idle]
                        [--lock FILE] [--cpus 1,2,3,4,6,7] [--out FILE.json]

--demo       the synthetic demo data (--now-ms optional; default 1795163100000).
--db         a database to read. Never one under ~/.t3: the script refuses it.
--lock       a file the caller holds with flock (`flock FILE python3 board-keys.py ...`); the
             script checks that it is held and stops if it is not.
--cpus       pin this script, and so every trial, to these cores. Otherwise the current affinity.
--live-idle  measure the idle hold on the wall clock instead of the fixed one (frames then
             differ by time, so only the idle numbers are compared).
Scratch folders are made under $TMPDIR (it must be set) and removed after each trial.
"""
import argparse
import errno
import fcntl
import hashlib
import json
import os
import pty
import select
import signal
import statistics
import struct
import subprocess
import tempfile
import termios
import time
from pathlib import Path

END = b"\x1b[?2026l"
PROBE_END = b"\x1b[c"
PROBE_REPLY = b"\x1bP>|foot(1.28.0)\x1b\\\x1b[?2027;1$y\x1b[1;2R\x1b[?62;4c"
KEYS = [b"?", b"?", b"j", b"k"]
SIZES = [(120, 40), (180, 55)]
DEMO_NOW = 1795163100000


def refuse_t3(path, what):
    t3 = Path(os.path.expanduser("~/.t3")).resolve()
    resolved = Path(path).expanduser().resolve()
    if resolved == t3 or t3 in resolved.parents:
        raise SystemExit(f"refusing {what} under {t3}: {resolved}")
    return resolved


def charged_ns(pid):
    """Task runtime in nanoseconds, kernel time included: (all threads, the UI thread)."""
    tasks = {}
    for p in Path(f"/proc/{pid}/task").glob("*/schedstat"):
        try:
            tasks[int(p.parent.name)] = int(p.read_text().split()[0])
        except (OSError, ValueError):
            continue  # a thread that ended between the listing and the read
    return sum(tasks.values()), tasks[pid]


class Terminal:
    def __init__(self, master):
        self.master = master
        self.pending = b""
        self.frames = 0

    def read_until(self, marker, timeout=10):
        deadline = time.monotonic() + timeout
        while marker not in self.pending:
            remaining = max(0, deadline - time.monotonic())
            if not select.select([self.master], [], [], remaining)[0]:
                raise RuntimeError("terminal response timed out")
            self.pending += os.read(self.master, 32768)
            if len(self.pending) > 2**20:
                raise RuntimeError("unterminated terminal response")
        part, self.pending = self.pending.split(marker, 1)
        return part + marker

    def frame(self):
        data = self.read_until(END)
        self.frames += 1
        return {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}

    def silence(self, duration):
        if self.pending or select.select([self.master], [], [], duration)[0]:
            raise RuntimeError("unexpected terminal output during fixed-clock hold")

    def timed_output(self, duration):
        deadline = time.monotonic() + duration
        digest = hashlib.sha256()
        total = frames = 0
        suffix = b""
        part, self.pending = self.pending, b""
        while True:
            digest.update(part)
            total += len(part)
            scan = suffix + part
            frames += scan.count(END)
            suffix = scan[-(len(END) - 1):]  # a marker split across two reads
            remaining = max(0, deadline - time.monotonic())
            if not select.select([self.master], [], [], remaining)[0]:
                break
            part = os.read(self.master, 32768)
            if not part or time.monotonic() > deadline + 0.1:
                raise RuntimeError("could not drain the terminal at its live cadence")
        return {"bytes": total, "frames": frames, "sha256": digest.hexdigest()}

    def exit_output(self):
        digest = hashlib.sha256(self.pending)
        total = len(self.pending)
        self.pending = b""
        while select.select([self.master], [], [], 2)[0]:
            try:
                part = os.read(self.master, 32768)
            except OSError as e:
                if e.errno == errno.EIO:
                    break
                raise
            if not part:
                break
            digest.update(part)
            total += len(part)
        return {"bytes": total, "sha256": digest.hexdigest()}


def resize(master, size):
    cols, rows = size
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def trial(binary, data_args, now_ms, seconds, scratch, key_repeats, calm, live_idle):
    home, tmp, empty = (scratch / d for d in ("home", "tmp", "empty"))
    for d in (home, tmp, empty):
        d.mkdir()
    env = {"HOME": str(home), "T3CODE_HOME": str(empty), "TMPDIR": str(tmp), "PATH": "/usr/bin:/bin",
           "LANG": "C.UTF-8", "TZ": "UTC", "TERM": "xterm-256color", "COLORTERM": "truecolor"}
    args = [str(binary), *data_args]
    if not live_idle:
        env["T3_FIXED_NOW"] = str(now_ms)
        args += ["--now-ms", str(now_ms)]
    if calm:
        args.append("--calm")
    master, slave = pty.openpty()
    child = None
    try:
        resize(master, SIZES[0])
        started = time.monotonic_ns()
        child = subprocess.Popen(args, stdin=slave, stdout=slave, stderr=subprocess.DEVNULL,
                                 env=env, cwd=scratch, start_new_session=True)
        os.close(slave)
        slave = None
        tty = Terminal(master)
        probe = tty.read_until(PROBE_END)
        os.write(master, PROBE_REPLY)
        obs = {
            "probe": {"bytes": len(probe), "sha256": hashlib.sha256(probe).hexdigest()},
            "initial": tty.frame(),
            "initial_ms": (time.monotonic_ns() - started) / 1e6,
        }
        # Let the identical 2-second product reader finish before the idle interval.
        if live_idle:
            tty.timed_output(5)
        else:
            tty.silence(3)
        measure_at = time.monotonic_ns()
        cpu_before, ui_before = charged_ns(child.pid)
        if live_idle:
            obs["idle_output"] = tty.timed_output(seconds)
            if obs["idle_output"]["frames"] < seconds - 2:
                raise RuntimeError("live dashboard did not produce its clock frames")
        else:
            tty.silence(seconds)
        cpu_after, ui_after = charged_ns(child.pid)
        obs["idle_cpu_ns"] = cpu_after - cpu_before
        obs["idle_ui_cpu_ns"] = ui_after - ui_before
        obs["idle_elapsed_ns"] = time.monotonic_ns() - measure_at
        obs["keys"] = []
        cpu_before, ui_before = charged_ns(child.pid)
        for _ in range(0 if live_idle else key_repeats):
            for index, key in enumerate(KEYS):
                at = time.monotonic_ns()
                os.write(master, key)
                try:
                    frame = tty.frame()
                except RuntimeError as e:
                    raise RuntimeError(f"missing frame at key index {index}") from e
                obs["keys"].append({**frame, "latency_ns": time.monotonic_ns() - at})
        cpu_after, ui_after = charged_ns(child.pid)
        obs["keys_cpu_ns"] = cpu_after - cpu_before
        obs["keys_ui_cpu_ns"] = ui_after - ui_before
        if not live_idle:
            resize(master, SIZES[1])
            child.send_signal(signal.SIGWINCH)
            obs["resize"] = tty.frame()
        os.write(master, b"q")
        obs["exit"] = tty.exit_output()
        obs["exit_status"] = child.wait(timeout=2)
        obs["frames"] = tty.frames
        if obs["exit_status"] != 0:
            raise RuntimeError("terminal exited unsuccessfully")
        return obs
    finally:
        if child is not None and child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
        if slave is not None:
            os.close(slave)


def content(row):
    """What must be identical between arms and trials: every digest and size, no times."""
    return (row["probe"], row["initial"], row.get("resize"), row["exit"], row["frames"],
            [{k: key[k] for k in ("bytes", "sha256")} for key in row["keys"]])


def sha256_file(path):
    with open(path, "rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def spread(values):
    return {"median": statistics.median(values), "min": min(values), "max": max(values)} if values else None


def summary(rows):
    latency = [k["latency_ns"] / 1e6 for r in rows for k in r["keys"]]
    return {
        "trials": len(rows),
        "initial_ms": spread([r["initial_ms"] for r in rows]),
        "idle_cpu_ms_per_s": spread([r["idle_cpu_ns"] / r["idle_elapsed_ns"] * 1e3 for r in rows]),
        "idle_ui_cpu_ms_per_s": spread([r["idle_ui_cpu_ns"] / r["idle_elapsed_ns"] * 1e3 for r in rows]),
        "key_latency_ms": spread(latency),
        "keys_cpu_ms_per_key": spread([r["keys_cpu_ns"] / 1e6 / len(r["keys"]) for r in rows if r["keys"]]),
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", type=Path, required=True)
    ap.add_argument("--cand", type=Path, required=True)
    what = ap.add_mutually_exclusive_group(required=True)
    what.add_argument("--demo", action="store_true")
    what.add_argument("--db", type=Path)
    ap.add_argument("--now-ms", type=int)
    ap.add_argument("--pairs", type=int, default=5)
    ap.add_argument("--seconds", type=float, default=30)
    ap.add_argument("--key-repeats", type=int, default=8)
    ap.add_argument("--calm", action="store_true")
    ap.add_argument("--live-idle", action="store_true")
    ap.add_argument("--lock", type=Path)
    ap.add_argument("--cpus", help="comma list, e.g. 1,2,3,4,6,7")
    ap.add_argument("--out", type=Path)
    a = ap.parse_args()

    base, cand = (refuse_t3(p, n) for p, n in ((a.base, "--base"), (a.cand, "--cand")))
    if a.out:
        refuse_t3(a.out, "--out")
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
        raise SystemExit("set TMPDIR (trial folders are made there)")
    stage = refuse_t3(os.environ["TMPDIR"], "TMPDIR")
    if a.cpus:
        os.sched_setaffinity(0, {int(c) for c in a.cpus.split(",")})
    if a.pairs < 1:
        raise SystemExit("--pairs must be at least 1")
    if a.pairs * 2 * (a.seconds + 10) > 3600:
        raise SystemExit("batch would run over an hour; lower --pairs or --seconds")
    if a.lock:
        with a.lock.open("rb") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                pass  # held by the caller, as it should be
            else:
                raise SystemExit("the lock is not held: run this under `flock FILE`")

    binaries = {"base": base, "cand": cand}
    report = {
        "binary_sha256": {arm: sha256_file(p) for arm, p in binaries.items()},
        "data": "demo" if a.demo else "db", "pinned_now_ms": None if a.live_idle else now_ms,
        "calm": a.calm, "live_idle": a.live_idle, "seconds": a.seconds, "key_repeats": a.key_repeats,
        "cpu_affinity": sorted(os.sched_getaffinity(0)), "load_begin": os.getloadavg(), "pairs": [],
    }
    if a.db:
        report["db_bytes"] = db.stat().st_size
    expected = None
    for pair in range(a.pairs):
        rows = {}
        for arm in list(binaries)[::1 if pair % 2 == 0 else -1]:
            with tempfile.TemporaryDirectory(prefix="pty-", dir=stage) as scratch:
                row = trial(binaries[arm], data_args, now_ms, a.seconds, Path(scratch), a.key_repeats, a.calm, a.live_idle)
            if expected is None:
                expected = content(row)
            if not a.live_idle and content(row) != expected:
                raise RuntimeError(f"{arm}: terminal frame digest or output size mismatch in pair {pair + 1}")
            rows[arm] = row
        report["pairs"].append(rows)
    report["load_end"] = os.getloadavg()
    report["frames_identical"] = not a.live_idle
    report["summary"] = {arm: summary([p[arm] for p in report["pairs"]]) for arm in binaries}
    text = json.dumps(report, indent=2)
    if a.out:
        a.out.write_text(text + "\n")
    print(text)


if __name__ == "__main__":
    main()
