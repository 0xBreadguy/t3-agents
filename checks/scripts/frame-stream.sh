#!/usr/bin/env bash
# Prove that a change to how the dashboard makes frames leaves every written byte as it was.
#
# `t3-agents-term --frame-stream` (built only with `--features frame-stream`) runs the
# dashboard's frame loop with no terminal, on a scripted clock, and prints counts and SHA-256
# digests of the encoder output, the composed canvases and the wake the host asked for. This
# script builds a baseline and a candidate, each into its own target folder, runs the same
# schedules on both and fails on any difference:
#
#   default    the fixed script (keys, sizes and clock steps)
#   extended   the fixed script that adds themes, pause, mouse, wheel, resizes, terminal answers,
#              Storage (database) or Insights (demo)
#   random     a seeded mix of all of it (--seed, --frames)
#   wide       the extended script with wide characters, emoji, combining marks and a weight
#              flip written over some rows before five more encoders (--wide-overlay)
#
#   checks/scripts/frame-stream.sh [--base DIR|REV] [--cand DIR] [--demo | --db PATH --now-ms MS]
#                                  [--scratch DIR] [--cpus LIST] [--build-cpus LIST] [--lock FILE]
#                                  [--seed N] [--frames N] [--keep]
#
# --base      a source folder or a git revision of this repository (default v1.4.0). A source
#             that predates the check gets the check's own commit applied to a copy first, so
#             the baseline is the released code plus only the instrument.
# --cand      the source folder to test (default: this checkout).
# --demo      the synthetic demo data (default). --now-ms pins its clock (default 1795163100000).
# --db        a database to read, read-only. Never one under $HOME/.t3: the check refuses it.
# --scratch   where the exports, target folders and run folders go (default $TMPDIR/frame-stream;
#             the script never uses /tmp on its own, so set TMPDIR or pass --scratch).
# --cpus      cores for the runs (default 8-19). --lock FILE: take flock FILE around each run (not
#             the builds, which use --build-cpus, default 8-19, and no lock).
#
# Output is one line per schedule with counts and digests. No screen text leaves the process.
# Exit 0 and PASS when every digest matches.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)

base=v1.4.0
cand=$repo
db=
now_ms=1795163100000
scratch=
cpus=8-19
build_cpus=8-19
lock=
seed=7
frames=4000
keep=0

die() { echo "frame-stream.sh: $*" >&2; exit 2; }

while [ $# -gt 0 ]; do
  case $1 in
    --base) base=${2:?}; shift 2 ;;
    --cand) cand=${2:?}; shift 2 ;;
    --demo) db=; shift ;;
    --db) db=${2:?}; shift 2 ;;
    --now-ms) now_ms=${2:?}; shift 2 ;;
    --scratch) scratch=${2:?}; shift 2 ;;
    --cpus) cpus=${2:?}; shift 2 ;;
    --build-cpus) build_cpus=${2:?}; shift 2 ;;
    --lock) lock=${2:?}; shift 2 ;;
    --seed) seed=${2:?}; shift 2 ;;
    --frames) frames=${2:?}; shift 2 ;;
    --keep) keep=1; shift ;;
    -h|--help) sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option $1" ;;
  esac
done

case $now_ms in ''|*[!0-9]*) die "--now-ms must be whole milliseconds" ;; esac
case $seed$frames in ''|*[!0-9]*) die "--seed and --frames must be whole numbers" ;; esac

t3_home=$(realpath -m -- "${HOME:?}/.t3")
inside_t3() {
  local p
  p=$(realpath -m -- "$1")
  [ "$p" = "$t3_home" ] || [ "${p#"$t3_home"/}" != "$p" ]
}
if [ -n "$db" ]; then
  inside_t3 "$db" && die "refusing a database under $t3_home"
  [ -f "$db" ] || die "no such database: $db"
  db=$(realpath -- "$db")
fi

if [ -z "$scratch" ]; then
  [ -n "${TMPDIR:-}" ] || die "set TMPDIR or pass --scratch"
  scratch=$TMPDIR/frame-stream
fi
inside_t3 "$scratch" && die "refusing a scratch folder under $t3_home"
mkdir -p -- "$scratch"
scratch=$(realpath -- "$scratch")
run=$(mktemp -d -- "$scratch/run.XXXXXX")
cleanup() { [ "$keep" = 1 ] || rm -rf -- "$run"; }
trap cleanup EXIT
mkdir -p -- "$run/empty-t3" "$run/tmp"

[ -d "$cand/crates/term" ] || die "--cand is not a T3 Agents source folder: $cand"
cand=$(realpath -- "$cand")

# What wraps a build (cores, low priority) and what wraps a run (the same, and the lock if given).
build_wrap=(taskset -c "$build_cpus" nice -n 10)
wrap=(taskset -c "$cpus" nice -n 10)
if [ -n "$lock" ]; then wrap=(flock "$lock" "${wrap[@]}"); fi

# --- sources ----------------------------------------------------------------------------------
export_base() {
  local dest=$run/src-base
  mkdir -p -- "$dest"
  if [ -d "$base" ]; then
    (cd "$base" && tar --exclude=./target --exclude=./.git -cf - .) | tar -xf - -C "$dest"
  else
    git -C "$repo" rev-parse --verify --quiet "$base^{commit}" >/dev/null || die "no such revision: $base"
    git -C "$repo" archive "$base" | tar -xf - -C "$dest"
  fi
  if ! grep -q '^frame-stream' "$dest/crates/term/Cargo.toml"; then
    # The baseline predates the instrument: add exactly the commit that introduced it.
    local c
    c=$(git -C "$repo" log --diff-filter=A --format=%H -1 -- crates/term/src/stream_check.rs)
    [ -n "$c" ] || die "the baseline has no frame-stream feature and this repository has no commit that adds it"
    git -C "$repo" show --format= "$c" -- \
      crates/core/Cargo.toml crates/core/src/live.rs crates/term/Cargo.toml crates/term/src/main.rs \
      crates/term/src/insights_screen.rs crates/term/src/stream_check.rs crates/term/src/stream_check/sched.rs \
      >"$run/instrument.patch"
    (cd "$dest" && git apply --whitespace=nowarn "$run/instrument.patch") \
      || die "could not add the instrument to the baseline ($c)"
    echo "baseline: $base plus the frame-stream instrument from ${c:0:12}" >&2
  else
    echo "baseline: $base" >&2
  fi
  printf '%s' "$dest"
}

build() { # SOURCE TARGET-DIR NAME  -> prints the binary's path
  local src=$1 target=$2 name=$3
  local log=$run/build-$name.log
  if ! (cd "$src" && env TMPDIR="$run/tmp" T3CODE_HOME="$run/empty-t3" "${build_wrap[@]}" \
      cargo build --release --locked --features frame-stream -p t3-agents-term --target-dir "$target" >"$log" 2>&1); then
    tail -n 30 "$log" >&2
    die "build failed in $src"
  fi
  if grep -q '^warning' "$log"; then
    grep -A6 '^warning' "$log" | head -n 40 >&2
    die "build warned in $src"
  fi
  [ -x "$target/release/t3-agents-term" ] || die "no binary at $target/release/t3-agents-term"
  printf '%s' "$target/release/t3-agents-term"
}

base_src=$(export_base)
echo "building baseline..."
base_bin=$(build "$base_src" "$scratch/target-base" base)
echo "building candidate ($cand)..."
cand_bin=$(build "$cand" "$scratch/target-cand" cand)
echo "baseline binary sha256 $(sha256sum "$base_bin" | cut -d' ' -f1)"
echo "candidate binary sha256 $(sha256sum "$cand_bin" | cut -d' ' -f1)"

# --- runs -------------------------------------------------------------------------------------
if [ -n "$db" ]; then data=(--db "$db" --now-ms "$now_ms"); else data=(--demo --now-ms "$now_ms"); fi

stream() { # BINARY TAG ARGS...  -> JSON line on stdout, written to a file
  local bin=$1 tag=$2; shift 2
  local home=$run/home-$tag
  rm -rf -- "$home"; mkdir -p -- "$home/home" "$home/t3" "$home/tmp"
  env -i HOME="$home/home" PATH=/usr/bin:/bin T3CODE_HOME="$home/t3" TMPDIR="$home/tmp" \
    LANG=C.UTF-8 TZ=UTC TERM=xterm-256color \
    "${wrap[@]}" "$bin" --frame-stream "${data[@]}" "$@" </dev/null
}

cat >"$run/normal.py" <<'PY'
import json, sys
d = json.loads(sys.stdin.read())
d.pop("segments", None)  # timings
print(json.dumps(d, sort_keys=True))
PY

cat >"$run/compare.py" <<'PY'
import json, sys

label, b, c = sys.argv[1:4]
base, cand = json.loads(b), json.loads(c)
# The baseline's digests (the candidate's are equal unless this says DIFFERENT).
print(f"{label:9} frames={base.get('frames')} written={base.get('written')} bytes={base.get('bytes')}")
for k in ("stream_sha256", "canvas_sha256", "wake_sha256"):
    print(f"          {k[:-7]:7} {base[k]}")
for group in ("encoders", "overlay"):
    for e in base.get(group, []):
        print(f"          {group}/{e['name']:16} {e['bytes']:>9} bytes {e['sha256']}")
if base != cand:
    print("          DIFFERENT from the baseline:")
    for k in sorted(set(base) | set(cand)):
        if base.get(k) != cand.get(k):
            print(f"            {k}: baseline  {str(base.get(k))[:200]}")
            print(f"            {k}: candidate {str(cand.get(k))[:200]}")
    sys.exit(1)
PY

failed=0
check() { # LABEL ARGS...
  local label=$1; shift
  local b c
  b=$(stream "$base_bin" base "$@" | python3 -I "$run/normal.py")
  c=$(stream "$cand_bin" cand "$@" | python3 -I "$run/normal.py")
  python3 -I "$run/compare.py" "$label" "$b" "$c" || failed=1
}

check default
check extended --schedule extended
check random --schedule random --seed "$seed" --frames "$frames"
check wide --schedule extended --wide-overlay

if [ "$failed" = 0 ]; then echo PASS; else echo FAIL; exit 1; fi
