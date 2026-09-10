#!/bin/bash
# adbrsync benchmark.
#
# One unattended pass that measures what a run is actually bound by, and leaves
# a directory of results you can keep or send on. For whatever device and link
# you point it at, it answers:
#
#   * how throughput scales with --streams, so the default can be checked
#     rather than assumed -- it differs sharply between wi-fi and USB
#   * how far small files fall short of the link
#   * whether per-file cost is costing anything once concurrency is applied,
#     which is what decides whether an on-device agent would buy anything
#
# Everything it creates is removed again, on the device and locally, including
# on interrupt. It needs only adb and an adbrsync binary -- no Rust toolchain --
# so it can be copied to whichever machine has the device attached.

set -uo pipefail

usage() {
	cat <<'USAGE'
usage: bench.sh [-h]

Environment:
  ADBRSYNC      path to the adbrsync binary (default: found automatically)
  OUT           results directory (default: ./adbrsync-bench-<timestamp>)
  LARGE_MIB     size of each large-corpus file, in MiB          (default 64)
  LARGE_COUNT   number of large files                           (default 24)
  SMALL_KIB     size of each small-corpus file, in KiB          (default 4)
  SMALL_COUNT   number of small files                           (default 4000)
  SWEEP         stream counts to try, in order                  (default "16 1 2 4 8 24 32 16)"
                A value repeated at both ends acts as a drift control.
  DEVICE_ROOT   scratch directory on the device   (default /sdcard/.adbrsync-bench)
  REAL_TREE     tree to scan for its shape, never transferred   (default /sdcard)

Takes roughly 12-20 minutes at the defaults. Shrink LARGE_COUNT and SWEEP for a
quick check that the plumbing works.
USAGE
}

case "${1:-}" in
-h | --help)
	usage
	exit 0
	;;
esac

# ---------------------------------------------------------------- settings --
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# The binary may sit anywhere: beside this script when it has been carried to
# another machine, or wherever cargo was told to put it, which since the target
# directory can be configured is often outside the repository.
find_adbrsync() {
	if [ -n "${ADBRSYNC:-}" ]; then
		printf '%s' "$ADBRSYNC"
		return
	fi
	if [ -x "$HERE/adbrsync" ]; then
		printf '%s' "$HERE/adbrsync"
		return
	fi
	local dir profile
	if command -v cargo >/dev/null 2>&1; then
		dir=$(cd "$HERE/.." && cargo metadata --format-version 1 --no-deps 2>/dev/null |
			sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
		for profile in release debug; do
			if [ -n "$dir" ] && [ -x "$dir/$profile/adbrsync" ]; then
				printf '%s' "$dir/$profile/adbrsync"
				return
			fi
		done
	fi
	for profile in release debug; do
		if [ -x "$HERE/../target/$profile/adbrsync" ]; then
			printf '%s' "$HERE/../target/$profile/adbrsync"
			return
		fi
	done
}
ADBRSYNC=$(find_adbrsync)
OUT="${OUT:-$PWD/adbrsync-bench-$(date +%Y%m%d-%H%M%S)}"

# Large corpus: files big enough that per-file cost is negligible, so the
# sweep measures the link and nothing else.
LARGE_MIB="${LARGE_MIB:-64}"
LARGE_COUNT="${LARGE_COUNT:-24}"

# Small corpus: files small enough that per-file cost is nearly all of it.
# 4 KiB keeps byte time far below the fixed cost we are trying to see.
SMALL_KIB="${SMALL_KIB:-4}"
SMALL_COUNT="${SMALL_COUNT:-4000}"

# The first value runs again at the end; if the two disagree, something drifted
# (page cache, thermal throttling) and the sweep in between is suspect.
SWEEP="${SWEEP:-16 1 2 4 8 24 32 16}"

DEVICE_ROOT="${DEVICE_ROOT:-/sdcard/.adbrsync-bench}"
# Scanned, never transferred, purely to learn the real corpus shape.
REAL_TREE="${REAL_TREE:-/sdcard}"

# ------------------------------------------------------------------- setup --
red()  { printf '\033[31m%s\033[0m\n' "$*"; }
bold() { printf '\033[1m%s\033[0m\n' "$*"; }
say()  { printf '%s\n' "$*"; }

cleanup() {
  say ""
  say "cleaning up..."
  adb shell "rm -rf $DEVICE_ROOT" >/dev/null 2>&1
  rm -rf "$OUT/pull" 2>/dev/null
  say "device corpus removed; results kept in $OUT"
}
trap cleanup EXIT INT TERM

fail() { red "error: $*"; exit 1; }

# --------------------------------------------------------------- preflight --
bold "== preflight =="
command -v adb >/dev/null || fail "adb not found in PATH"
[ -n "$ADBRSYNC" ] && [ -x "$ADBRSYNC" ] || fail "no adbrsync binary found.
   Build one with \`cargo build --release\`, set ADBRSYNC=/path/to/adbrsync,
   or put the binary beside this script."

DEVICES=$(adb devices | awk 'NR>1 && $2=="device" {print $1}')
COUNT=$(printf '%s\n' "$DEVICES" | grep -c . )
[ "$COUNT" -eq 1 ] || fail "need exactly one device in state 'device', found $COUNT"
SERIAL=$(printf '%s' "$DEVICES")

# A wi-fi serial looks like 192.168.0.108:41567. The whole point is USB.
if printf '%s' "$SERIAL" | grep -qE '^[0-9]+(\.[0-9]+){3}:[0-9]+$'; then
  red "WARNING: $SERIAL looks like a wireless connection, not USB."
  red "         Results will describe wi-fi and answer neither question."
  printf 'continue anyway? [y/N] '
  read -r reply
  [ "$reply" = "y" ] || exit 1
fi

say "adb        $(adb version | head -1)"
say "adbrsync   $("$ADBRSYNC" --version)"
say "device     $SERIAL  $(adb shell getprop ro.product.model | tr -d '\r')"
say "android    $(adb shell getprop ro.build.version.release | tr -d '\r') (sdk $(adb shell getprop ro.build.version.sdk | tr -d '\r'))"

LARGE_TOTAL_MIB=$((LARGE_MIB * LARGE_COUNT))
SMALL_TOTAL_MIB=$((SMALL_KIB * SMALL_COUNT / 1024))
NEED_MIB=$((LARGE_TOTAL_MIB + SMALL_TOTAL_MIB + 512))
FREE_MIB=$(df -m "$(dirname "$OUT")" | awk 'NR==2 {print $4}')
[ "$FREE_MIB" -gt "$NEED_MIB" ] || fail "need ~${NEED_MIB} MiB free here, have ${FREE_MIB} MiB"

RUNS=$(printf '%s\n' $SWEEP | grep -c .)
say ""
say "large corpus  ${LARGE_COUNT} x ${LARGE_MIB} MiB = ${LARGE_TOTAL_MIB} MiB"
say "small corpus  ${SMALL_COUNT} x ${SMALL_KIB} KiB = ${SMALL_TOTAL_MIB} MiB"
say "sweep         $SWEEP  (${RUNS} runs)"
say "expect roughly 12-20 minutes unattended."
say ""

mkdir -p "$OUT/reports" "$OUT/pull" || fail "cannot create $OUT"

# ------------------------------------------------------------------ corpus --
bold "== building corpus on device =="
adb shell "rm -rf $DEVICE_ROOT; mkdir -p $DEVICE_ROOT/large $DEVICE_ROOT/small" \
  || fail "cannot write to $DEVICE_ROOT"

# One random seed, then copy it. Copies are separate files with separate page
# cache entries, so this is only a shortcut for creation, not for reading.
say "large: seeding ${LARGE_MIB} MiB of random data..."
adb shell "dd if=/dev/urandom of=$DEVICE_ROOT/large/f000 bs=1M count=$LARGE_MIB" 2>&1 | tail -1
say "large: copying to ${LARGE_COUNT} files..."
adb shell "cd $DEVICE_ROOT/large && for i in \$(seq 1 $((LARGE_COUNT - 1))); do cp f000 \$(printf 'f%03d' \$i); done"

say "small: splitting a seed into ${SMALL_COUNT} files of ${SMALL_KIB} KiB..."
SMALL_SEED_KIB=$((SMALL_KIB * SMALL_COUNT))
adb shell "dd if=/dev/urandom of=$DEVICE_ROOT/seed bs=1024 count=$SMALL_SEED_KIB" 2>&1 | tail -1
adb shell "cd $DEVICE_ROOT/small && split -b ${SMALL_KIB}k ../seed p && rm -f ../seed"

ACTUAL_LARGE=$(adb shell "ls $DEVICE_ROOT/large | wc -l" | tr -d ' \r')
ACTUAL_SMALL=$(adb shell "ls $DEVICE_ROOT/small | wc -l" | tr -d ' \r')
say "on device: $ACTUAL_LARGE large, $ACTUAL_SMALL small"
[ "$ACTUAL_LARGE" -ge 2 ] || fail "large corpus was not created"
[ "$ACTUAL_SMALL" -ge 2 ] || fail "small corpus was not created"

# --------------------------------------------------------------- run helper --
# run <label> <streams> <device-subdir> -> prints "bytes/s files bytes"
run_case() {
  local label="$1" streams="$2" sub="$3"
  local dest="$OUT/pull/$label" json="$OUT/reports/$label.json" txt="$OUT/reports/$label.txt"
  rm -rf "$dest"; mkdir -p "$dest"
  "$ADBRSYNC" -a --streams "$streams" --stats \
      --perf-report "$json" "$SERIAL:${DEVICE_ROOT}${sub:+/$sub}/" "$dest" >"$txt" 2>&1
  local rc=$?
  rm -rf "$dest"
  if [ $rc -ne 0 ]; then
    red "  run '$label' failed (exit $rc); see $txt"
    printf '0 0 0\n'
    return
  fi
  awk '/aggregate rate/ {gsub("/s","",$3); rate=$3}
       /transferred/    {files=$2; bytes=$5}
       END              {printf "%s %s %s\n", rate+0, files+0, bytes+0}' "$txt"
}

mibs() { awk -v r="$1" 'BEGIN{printf "%.2f", r/1048576}'; }

# -------------------------------------------------------- phase 1: streams --
bold ""
bold "== phase 1: stream sweep (large files, link-bound) =="
printf '%8s  %12s  %8s\n' streams MB/s seconds
SWEEP_LOG="$OUT/sweep.txt"
: > "$SWEEP_LOG"
n=0
for streams in $SWEEP; do
  n=$((n + 1))
  label=$(printf 'sweep-%02d-s%s' "$n" "$streams")
  t0=$SECONDS
  read -r rate files bytes <<<"$(run_case "$label" "$streams" large)"
  secs=$((SECONDS - t0))
  printf '%8s  %12s  %8s\n' "$streams" "$(mibs "$rate")" "$secs"
  printf '%s %s %s %s\n' "$streams" "$rate" "$files" "$secs" >> "$SWEEP_LOG"
done

# ---------------------------------------------------- phase 2: small files --
BEST=$(sort -k2 -rn "$SWEEP_LOG" | head -1 | awk '{print $1}')
[ -n "$BEST" ] || BEST=16
bold ""
bold "== phase 2: small files at the sweep's best stream count ($BEST) =="
say "This measures how far small files fall short of the link, not the fixed"
say "cost itself: every file here is the same size, so the measured rate already"
say "contains that cost and the two cannot be separated. Phase 3 supplies it."
read -r SMALL_RATE SMALL_FILES SMALL_BYTES <<<"$(run_case small "$BEST" small)"
say "aggregate $(mibs "$SMALL_RATE") MB/s over $SMALL_FILES files"
grep -E 'stream busy' "$OUT/reports/small.txt" || true

# --------------------------------------------------------- phase 3: mixed --
bold ""
bold "== phase 3: mixed corpus (the gate metric, and where fixed cost is measurable) =="
read -r MIXED_RATE MIXED_FILES MIXED_BYTES <<<"$(run_case mixed "$BEST" "")"
say "aggregate $(mibs "$MIXED_RATE") MB/s over $MIXED_FILES files"
grep -E 'per-file fixed|fixed cost share|stream busy' "$OUT/reports/mixed.txt" || true

# ------------------------------------------------ phase 4: real tree shape --
# A dry run scans the device tree and transfers nothing, so for a few seconds
# of wall time we learn the shape of the corpus this tool actually exists for.
# Combined with the measured numbers above, that answers the gate question for
# real data without moving 12 GB across the link.
bold ""
bold "== phase 4: real tree shape (dry run, nothing transferred) =="
REAL_JSON="$OUT/reports/real-scan.json"
# A dry run writes nothing, but a destination still has to exist: adbrsync
# refuses to create one, so that an unmounted drive cannot be mistaken for an
# empty backup target.
mkdir -p "$OUT/pull/scan-only"
if "$ADBRSYNC" -a -n --perf-report "$REAL_JSON" "$SERIAL:${REAL_TREE}/" "$OUT/pull/scan-only" \
     >"$OUT/reports/real-scan.txt" 2>&1; then
  REAL_FILES=$(awk '/to transfer/ {print $1}' "$OUT/reports/real-scan.txt")
  REAL_BYTES=$(awk '/to transfer/ {gsub(/[(),]/,"",$4); print $4}' "$OUT/reports/real-scan.txt")
  say "$REAL_TREE: ${REAL_FILES:-?} files, ${REAL_BYTES:-?} bytes"
  [ -n "$REAL_BYTES" ] || red "  could not parse the byte count; projection will be skipped"
else
  red "  scan of $REAL_TREE failed; projection will be skipped"
  say "  $(tail -2 "$OUT/reports/real-scan.txt")"
  REAL_FILES=""; REAL_BYTES=""
fi

# -------------------------------------------------------------- summary ----
BULK_RATE=$(sort -k2 -rn "$SWEEP_LOG" | head -1 | awk '{print $2}')
# Take the fixed cost from the isolated small-file run: every file there is
# tiny and nothing large is competing for the link, so its stream time divided
# by its file count is the cost per file with no queueing mixed in. Reading it
# off the mixed run instead would fold in the wait behind large transfers.
FIXED_MS=$(awk '/transferred/ {files=$2}
                /stream busy/ {gsub("s","",$3); busy=$3}
                END {if (files > 0) printf "%.2f", busy*1000/files}' "$OUT/reports/small.txt")
FIXED_MIXED=$(awk '/per-file fixed/ && $3 ~ /^[0-9.]+$/ {print $3}' "$OUT/reports/mixed.txt")

# Whichever stream count was run twice is the drift control.
REPEATED=$(awk '{c[$1]++} END {for (s in c) if (c[s] > 1) {print s; exit}}' "$SWEEP_LOG")
if [ -n "$REPEATED" ]; then
  DRIFT_FIRST=$(awk -v s="$REPEATED" '$1==s {print $2; exit}' "$SWEEP_LOG")
  DRIFT_LAST=$(awk -v s="$REPEATED" '$1==s {r=$2} END {print r}' "$SWEEP_LOG")
  DRIFT_LINE="  drift check: ${REPEATED}-stream run measured $(mibs "$DRIFT_FIRST") MB/s first, $(mibs "$DRIFT_LAST") MB/s last"
else
  DRIFT_LINE="  drift check: skipped (no stream count was run twice)"
fi

{
  echo "adbrsync USB benchmark"
  echo "date      $(date)"
  echo "device    $SERIAL $(adb shell getprop ro.product.model | tr -d '\r')"
  echo "adbrsync  $("$ADBRSYNC" --version)"
  echo "corpus    large ${ACTUAL_LARGE}x${LARGE_MIB}MiB, small ${ACTUAL_SMALL}x${SMALL_KIB}KiB"
  echo ""
  echo "stream sweep (large files):"
  printf '  %8s  %12s  %8s\n' streams MB/s seconds
  while read -r s r f secs; do
    printf '  %8s  %12s  %8s\n' "$s" "$(mibs "$r")" "$secs"
  done < "$SWEEP_LOG"
  echo ""
  echo "$DRIFT_LINE"
  echo "  (a large gap means page cache or thermal drift contaminated the sweep)"
  echo ""
  echo "gate metric (both at $BEST streams):"
  printf '  %-24s %s MB/s\n' "large files only"  "$(mibs "$BULK_RATE")"
  printf '  %-24s %s MB/s over %s files\n' "mixed corpus" "$(mibs "$MIXED_RATE")" "$MIXED_FILES"
  printf '  %-24s %s MB/s over %s files\n' "small files only" "$(mibs "$SMALL_RATE")" "$SMALL_FILES"
  echo ""
  echo "  If mixed is close to large-only, per-file cost is hidden by"
  echo "  concurrency and an on-device agent would buy nothing."
  echo ""
  echo "per-file fixed cost:"
  printf '  %-34s %s ms\n' "isolated small-file run" "${FIXED_MS:-?}"
  printf '  %-34s %s ms\n' "mixed run, adbrsync estimate" "${FIXED_MIXED:-not measurable}"
  echo "  (the isolated figure is the one used below: nothing large is competing"
  echo "   for the link there, so no queueing time is folded into it)"
  echo ""
  echo "projection onto the real tree ($REAL_TREE):"
  if [ -n "$REAL_FILES" ] && [ -n "$REAL_BYTES" ] && [ -n "$FIXED_MS" ]; then
    awk -v files="$REAL_FILES" -v bytes="$REAL_BYTES" -v fixed_ms="$FIXED_MS" \
        -v rate="$BULK_RATE" -v streams="$BEST" '
      BEGIN {
        printf "  %-28s %d files, %.2f GiB\n", "corpus", files, bytes/1073741824
        byte_s  = (rate > 0) ? bytes / rate : 0
        fixed_s = files * (fixed_ms/1000) / streams
        printf "  %-28s %.0f s\n", "bytes at the bulk rate", byte_s
        printf "  %-28s %.0f s\n", "per-file cost over " streams " streams", fixed_s
        if (byte_s + fixed_s > 0)
          printf "  %-28s %.1f%%\n", "per-file share of the run", 100*fixed_s/(byte_s+fixed_s)
        print  "  (an on-device agent could recover at most that last figure)"
      }'
    echo ""
    echo "  cross-check, measured directly and without any model:"
    awk -v mixed="$MIXED_RATE" -v bulk="$BULK_RATE" 'BEGIN {
        if (bulk <= 0) exit
        pct = 100 * mixed / bulk
        lost = (mixed < bulk) ? 100 * (1 - mixed / bulk) : 0
        printf "    mixed reached %.0f%% of the large-file rate, so per-file cost\n", pct
        printf "    is costing about %.0f%% of the run\n", lost
      }'
  else
    echo "  not available (scan or fixed-cost measurement did not produce a number)"
  fi
} | tee "$OUT/summary.txt"

bold ""
bold "== done =="
say "summary      $OUT/summary.txt"
say "perf reports $OUT/reports/*.json"
say ""
say "Send back the whole $OUT directory (it is small; the pulled files are gone)."
