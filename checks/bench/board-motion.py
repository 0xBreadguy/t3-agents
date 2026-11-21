#!/usr/bin/env python3
"""Every state and cell plane the dashboard draws in motion, base against candidate.

Runs `--behavior-script` on each binary: a fixed list of clock offsets (in milliseconds after the
pinned clock) at a fixed size, default motion on. Every frame it returns carries the cell planes
(characters, foreground, background, attributes) and the context meters' tween state. The check
fails unless

  - the displayed clocks are the requested ones (a database: --now-ms plus each offset; the demo:
    its own fixed epoch plus each offset), and no frame is calm or lacks its meters;
  - the cell planes are complete (width x height) and the run exercises change: at least two
    distinct grids and two distinct meter states (so a control that draws nothing cannot pass);
  - base and candidate return the same frames: same count, same planes, same tween state.

Each pair runs both binaries (order flipping every pair), so a binary that is not deterministic
from one run to the next fails as well. The output is counts and SHA-256 digests; no frame
content is printed or kept.

  python3 board-motion.py --base BIN --cand BIN (--demo | --db PATH --now-ms MS)
                          [--pairs 5] [--width 120] [--height 40] [--offsets 100,200,...]
                          [--lock FILE] [--cpus 1,2,3,4,6,7] [--out FILE.json]

--demo  the synthetic demo data, which keeps its own clock (--now-ms is accepted and ignored).
--db    a database to read. Never one under ~/.t3: the script refuses it.
--lock  a file the caller holds with flock; the script checks that it is held.
--cpus  pin this script, and so both binaries, to these cores.
Scratch folders are made under $TMPDIR (it must be set).
"""
import argparse
import fcntl
import hashlib
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

DEMO_NOW = 1795163100000
OFFSETS = [100, 200, 300, 500, 650, 700, 1000, 1100, 2000, 5000]


def refuse_t3(path, what):
    t3 = Path(os.path.expanduser("~/.t3")).resolve()
    resolved = Path(path).expanduser().resolve()
    if resolved == t3 or t3 in resolved.parents:
        raise SystemExit(f"refusing {what} under {t3}: {resolved}")
    return resolved


def check(binary, data_args, now, offsets, width, height, stage, pinned):
    with tempfile.TemporaryDirectory(dir=stage) as workspace:
        temp = Path(workspace)
        empty = temp / "empty"
        empty.mkdir()
        spec = temp / "events.json"
        spec.write_text(json.dumps(dict(calm=False, width=width, height=height, events=[{"ms": t} for t in offsets])))
        env = dict(HOME=workspace, PATH="/usr/bin:/bin", TMPDIR=workspace, T3CODE_HOME=str(empty),
                   T3_FIXED_NOW=str(now), LC_ALL="C.UTF-8", TZ="UTC")
        result = subprocess.run([str(binary), *data_args, "--now-ms", str(now), "--behavior-script", str(spec)],
                                env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    if result.returncode:
        raise SystemExit(f"{binary.name}: the timed-frame check exited with {result.returncode}")
    output = result.stdout
    try:
        frames = json.loads(output)["frames"]
    except (ValueError, KeyError, TypeError):
        raise SystemExit(f"{binary.name}: the timed-frame check did not return a frame sequence") from None
    # A database is read at --now-ms. The demo has its own fixed epoch (its first frame's clock).
    start = now if pinned else (frames[0]["now"] if frames else now)
    if [frame["now"] for frame in frames] != [start + n for n in [0, *offsets]]:
        raise SystemExit(f"{binary.name}: displayed clocks differ from the requested frame times")
    grids, tweens = set(), set()
    for frame in frames:
        if frame["calm"] or not frame["tween"]:
            raise SystemExit(f"{binary.name}: a frame is calm or has no moving context meters")
        planes = [frame[name] for name in ("ch", "fg", "bg", "attr")]
        if any(len(plane) != width * height for plane in planes):
            raise SystemExit(f"{binary.name}: incomplete cell plane")
        grids.add(hashlib.sha256(json.dumps(planes).encode()).digest())
        tweens.add(tuple(value for _, value in frame["tween"]))
    if min(len(grids), len(tweens)) < 2:
        raise SystemExit(f"{binary.name}: the default-motion control did not exercise changing output")
    return dict(frames=len(frames), bytes=len(output), sha256=hashlib.sha256(output).hexdigest(),
                distinct_grids=len(grids), distinct_meter_states=len(tweens))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", type=Path, required=True)
    ap.add_argument("--cand", type=Path, required=True)
    what = ap.add_mutually_exclusive_group(required=True)
    what.add_argument("--demo", action="store_true")
    what.add_argument("--db", type=Path)
    ap.add_argument("--now-ms", type=int)
    ap.add_argument("--pairs", type=int, default=5)
    ap.add_argument("--width", type=int, default=120)
    ap.add_argument("--height", type=int, default=40)
    ap.add_argument("--offsets", default=",".join(map(str, OFFSETS)), help="milliseconds after the clock, comma list")
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
        data_args, now = ["--db", str(db)], a.now_ms
    else:
        data_args, now = ["--demo"], a.now_ms if a.now_ms is not None else DEMO_NOW
    if not os.environ.get("TMPDIR"):
        raise SystemExit("set TMPDIR (scratch folders are made there)")
    stage = refuse_t3(os.environ["TMPDIR"], "TMPDIR")
    offsets = [int(x) for x in a.offsets.split(",")]
    if a.cpus:
        os.sched_setaffinity(0, {int(c) for c in a.cpus.split(",")})
    if a.lock:
        with a.lock.open("rb") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                pass  # held by the caller, as it should be
            else:
                raise SystemExit("the lock is not held: run this under `flock FILE`")

    binaries = {"base": base, "cand": cand}
    results = {arm: [] for arm in binaries}
    for pair in range(a.pairs):
        for arm in list(binaries)[::1 if pair % 2 == 0 else -1]:
            results[arm].append(check(binaries[arm], data_args, now, offsets, a.width, a.height, stage, bool(a.db)))
    summary = {}
    for arm, rows in results.items():
        if any(r != rows[0] for r in rows):
            raise SystemExit(f"{arm}: two runs of the same binary returned different frames")
        summary[arm] = rows[0]
    if summary["base"] != summary["cand"]:
        print(json.dumps(summary, indent=1))
        raise SystemExit("candidate: motion states or cell planes differ from the baseline")
    report = {"data": "demo" if a.demo else "db", "now_ms": now, "size": [a.width, a.height], "offsets": offsets,
              "pairs": a.pairs, "result": summary["base"], "identical": True}
    text = json.dumps(report)
    if a.out:
        a.out.write_text(text + "\n")
    print(text)


if __name__ == "__main__":
    try:
        main()
    except BrokenPipeError:
        sys.exit(1)
