#!/usr/bin/env bash
# rcache benchmark: a slow origin (a file on a slow filesystem, loop-mounted)
# versus the same origin behind rcache with the journal on fast storage.
#
# This measures the configuration rcache exists for: random writes that a
# spinning disk (or a slow network filesystem) handles badly.
#
# Usage:
#   sudo bench/bench.sh [--slow-dir /mnt/mergerfs/rcache-bench] \
#                       [--fast-dir /var/tmp] [--size 2G] [--journal 256M] \
#                       [--runtime 20] [--json out.json]
#
# Requires: root, fio, losetup, mkfs.ext4, ublk_drv loaded.
#
# The script never touches a block device other than the loop devices it
# creates itself.

set -euo pipefail

SLOW_DIR=${SLOW_DIR:-/mnt/mergerfs/rcache-bench}
FAST_DIR=${FAST_DIR:-/var/tmp/rcache-bench}
UBLK_ID=${UBLK_ID:--1}
SIZE=${SIZE:-2G}
JOURNAL=${JOURNAL:-256M}
RUNTIME=${RUNTIME:-20}
BLOCK_SIZE=${BLOCK_SIZE:-4k}
JSON=""
RCACHE=${RCACHE:-./target/release/rcache}
RCACHE_LOG=${RCACHE_LOG:-/var/tmp/rcache-bench.log}

while [ $# -gt 0 ]; do
    case "$1" in
        --slow-dir) SLOW_DIR=$2; shift 2 ;;
        --fast-dir) FAST_DIR=$2; shift 2 ;;
        --size) SIZE=$2; shift 2 ;;
        --journal) JOURNAL=$2; shift 2 ;;
        --runtime) RUNTIME=$2; shift 2 ;;
        --bs) BLOCK_SIZE=$2; shift 2 ;;
        --json) JSON=$2; shift 2 ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
done

size_bytes() {
    case "$1" in
        *G) echo $(( ${1%G} * 1024 * 1024 * 1024 )) ;;
        *M) echo $(( ${1%M} * 1024 * 1024 )) ;;
        *K) echo $(( ${1%K} * 1024 )) ;;
        *)  echo "$1" ;;
    esac
}

log() { printf '%s\n' "==> $*" >&2; }
die() {
    printf 'error: %s\n' "$*" >&2
    if [ -n "${RCACHE_LOG:-}" ] && [ -s "$RCACHE_LOG" ]; then
        printf -- '--- rcache log (tail) ---\n' >&2
        tail -8 "$RCACHE_LOG" >&2
    fi
    exit 1
}

[ "$(id -u)" -eq 0 ] || die "must run as root"
command -v fio >/dev/null || die "fio is required"
[ -x "$RCACHE" ] || die "rcache binary not found at $RCACHE (build with: cargo build --release)"
[ -e /dev/ublk-control ] || die "ublk_drv is not loaded (modprobe ublk_drv)\n  /dev/ublk-control is a character device created by the driver"

mkdir -p "$SLOW_DIR" "$FAST_DIR"
ORIGIN_IMG="$SLOW_DIR/origin.img"
JOURNAL_FILE="$FAST_DIR/cache.journal"
LOOP=""
MOUNT=""
RCACHE_PID=""
DEVICE=""

cleanup() {
    set +e
    [ -n "$MOUNT" ] && umount "$MOUNT" 2>/dev/null
    if [ -n "$DEVICE" ]; then
        "$RCACHE" stop --id "$DEVICE" >/dev/null 2>&1
    fi
    if [ -n "$RCACHE_PID" ]; then
        # SIGTERM only makes rcache drain its journal; it keeps serving IO on
        # purpose (a mounted filesystem still needs the device). `stop` is the
        # real teardown, so do that first and only then wait on the process.
        if [ -n "$DEVICE" ]; then
            "$RCACHE" stop --id "$DEVICE" >/dev/null 2>&1
        fi
        for _ in $(seq 1 20); do
            kill -0 "$RCACHE_PID" 2>/dev/null || break
            sleep 0.5
        done
        kill -9 "$RCACHE_PID" 2>/dev/null
        wait "$RCACHE_PID" 2>/dev/null
    fi
    [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null
    rm -rf "${MOUNT:-}" 2>/dev/null
    rm -f "$ORIGIN_IMG" "$JOURNAL_FILE"
    # The rcache log is kept on failure: it explains why startup went wrong.
    if [ "${KEEP_LOG:-0}" = "1" ]; then
        printf 'rcache log kept at %s\n' "$RCACHE_LOG" >&2
    else
        rm -f "$RCACHE_LOG"
    fi
}
trap cleanup EXIT INT TERM
# Report which command aborted a `set -e` run instead of exiting silently.
trap 'printf "error: command failed (rc=$?): %s\n" "$BASH_COMMAND" >&2' ERR

# --- origin: a sparse file on the SLOW filesystem, exposed as a block device.
log "origin image: $ORIGIN_IMG ($SIZE) on $SLOW_DIR"
rm -f "$ORIGIN_IMG"
truncate -s "$SIZE" "$ORIGIN_IMG" || die "cannot create the origin image (is $SLOW_DIR writable?)"
LOOP=$(losetup --find --show "$ORIGIN_IMG") || die "losetup failed"
log "origin device: $LOOP"

run_fio() {
    # $1 = name, $2 = target path, extra args follow.
    local name=$1 target=$2
    shift 2
    # Size is left to the caller: the baseline writes the raw device (whole
    # size) while the cached run writes a file inside a filesystem.
    if [ "${BENCH_DEBUG:-0}" = "1" ]; then
        printf '  fio: --name=%s --filename=%s %s\n' "$name" "$target" "$*" >&2
    fi
    fio --name="$name" --filename="$target" \
        --bs="$BLOCK_SIZE" --iodepth=16 --ioengine=libaio --direct=1 \
        --runtime="$RUNTIME" --time_based --group_reporting \
        --output-format=json "$@" 2>/dev/null
}

extract() {
    # Read a JSON report on stdin and print "iops bw_mib p99_us".
    python3 -c '
import json, sys
d = json.load(sys.stdin)
w = d["jobs"][0]["write"]
if not w["iops"]:
    w = d["jobs"][0]["read"]
iops = w["iops"]
bw = w["bw"] / 1024.0
p99 = w["clat_ns"]["percentile"]["99.000000"] / 1000.0
print("%.0f %.1f %.0f" % (iops, bw, p99))
'
}

echo
echo "================ baseline: origin directly (no cache) ================"
BASE=$(run_fio randwrite "$LOOP" --rw=randwrite --size="$SIZE")
if [ -z "$BASE" ]; then die "baseline fio produced no output"; fi
BASE_METRICS=$(printf '%s' "$BASE" | extract)
echo "random $BLOCK_SIZE write: IOPS=$(echo "$BASE_METRICS" | cut -d' ' -f1) BW=$(echo "$BASE_METRICS" | cut -d' ' -f2) MiB/s p99=$(echo "$BASE_METRICS" | cut -d' ' -f3) us"

# The baseline leaves the slow filesystem with a backlog of dirty pages.
# Measuring the cached run immediately would mostly measure that backlog, so
# let the writeback settle first.
log "waiting for the slow filesystem to settle after the baseline"
sync
# A network filesystem keeps flushing pages in the background well after
# `sync` returns. Without this pause the cached run competes with the
# baseline's backlog and the comparison is meaningless.
wait_for_idle() {
    # A network filesystem has no useful /proc/diskstats entry, so watch the
    # kernel's dirty page count instead: wait until writeback has caught up.
    local limit=${1:-120} stable=0 prev=""
    for _ in $(seq 1 $limit); do
        local dirty writeback
        dirty=$(awk '/^Dirty:/{print $2}' /proc/meminfo)
        writeback=$(awk '/^Writeback:/{print $2}' /proc/meminfo)
        if [ "$dirty" -lt 4096 ] && [ "$writeback" -lt 512 ]; then
            stable=$(( stable + 1 ))
            # Two consecutive quiet samples 2s apart.
            [ "$stable" -ge 2 ] && return 0
        else
            stable=0
        fi
        prev=$dirty
        sleep 2
    done
    log "warning: filesystem still busy after ${limit}s (dirty=${prev}kB)"
    return 0
}
sync
log "waiting for writeback to settle (fewer than 4 MiB dirty)"
wait_for_idle 120
log "settled"

echo
echo "================ with rcache ================"
# Use a fresh origin image for the cached run. Writing back into the image the
# baseline just filled with random data makes the network filesystem allocate
# blocks for the first time, which measures allocation, not caching.
if [ "${SEPARATE_ORIGIN:-1}" = "1" ]; then
    log "creating a fresh origin image for the cached run"
    losetup -d "$LOOP" 2>/dev/null
    rm -f "$ORIGIN_IMG"
    truncate -s "$SIZE" "$ORIGIN_IMG"
    LOOP=$(losetup --find --show "$ORIGIN_IMG") || die "losetup failed"
    log "origin device: $LOOP (fresh)"
fi
rm -f "$JOURNAL_FILE"
fallocate -l "$JOURNAL" "$JOURNAL_FILE" || die "cannot allocate the journal in $FAST_DIR"
log "journal: $JOURNAL_FILE ($JOURNAL) on $FAST_DIR"

: > "$RCACHE_LOG"
KEEP_LOG=1
"$RCACHE" run --origin "$LOOP" --cache "$JOURNAL_FILE" --id "$UBLK_ID" \
    --wb-idle 5 --metrics-listen 127.0.0.1:9101 >"$RCACHE_LOG" 2>&1 &
RCACHE_PID=$!

for _ in $(seq 1 120); do
    # `grep` exits 1 while the log is still empty; under `set -e` that would
    # abort the whole run, so tolerate it here.
    DEVICE=$(grep -o 'ublkb[0-9]*' "$RCACHE_LOG" 2>/dev/null | head -1 | sed 's/ublkb//' || true)
    [ -n "$DEVICE" ] && [ -b "/dev/ublkb$DEVICE" ] && break
    # rcache can report a device id while the kernel never exports the block
    # node (a stale ublk id). Give up early and retry with another id.
    if grep -q 'failed to create ublk device' "$RCACHE_LOG" 2>/dev/null; then
        DEVICE=""
        break
    fi
    sleep 0.5
done
if [ -z "$DEVICE" ] || [ ! -b "/dev/ublkb$DEVICE" ]; then
    tail -5 "$RCACHE_LOG" >&2 || true
    die "rcache did not create a device within 60s (log: $RCACHE_LOG)"
fi
log "rcache device: /dev/ublkb$DEVICE"

# A filesystem makes the comparison realistic (it issues flushes too).
MOUNT=$(mktemp -d /tmp/rcache-mnt.XXXXXX)
log "mkfs.ext4 on /dev/ublkb$DEVICE"
if ! mkfs.ext4 -q -F "/dev/ublkb$DEVICE"; then
    die "mkfs failed; rcache log: $(tail -3 $RCACHE_LOG)"
fi
if ! mount "/dev/ublkb$DEVICE" "$MOUNT"; then
    die "mount failed; rcache log: $(tail -3 $RCACHE_LOG)"
fi
log "mounted on $MOUNT"

# Keep the test file well inside the filesystem so it never runs out of space.
FS_BYTES=$(size_bytes "$SIZE")
FILE_BYTES=$(( FS_BYTES * 3 / 5 ))
FILE_SIZE="$(( FILE_BYTES / 1024 / 1024 ))M"
log "fio file size: $FILE_SIZE (filesystem is $SIZE)"
# Sample the journal while the cached run is in flight: if it sits at a
# watermark the whole time, the drain cannot keep up.
( for _ in $(seq 1 10); do
    sleep 2
    "$RCACHE" status --id "$DEVICE" 2>/dev/null | awk '/journal in use/{printf "    mid-run: %s %s\n", $4, $5}'
  done ) &
MONITOR=$!
CACHED=$(run_fio randwrite "$MOUNT/fio.bin" --rw=randwrite --size="$FILE_SIZE")
kill "$MONITOR" 2>/dev/null || true
if [ -z "$CACHED" ]; then die "cached fio produced no output (log: $RCACHE_LOG)"; fi
CACHED_METRICS=$(printf '%s' "$CACHED" | extract)
echo "random $BLOCK_SIZE write: IOPS=$(echo "$CACHED_METRICS" | cut -d' ' -f1) BW=$(echo "$CACHED_METRICS" | cut -d' ' -f2) MiB/s p99=$(echo "$CACHED_METRICS" | cut -d' ' -f3) us"

echo
echo "================ diagnostics ================"
echo "  cpu load: $(cut -d' ' -f1-3 /proc/loadavg)"
echo "  slow fs dirty: $(grep -E '^(Dirty|Writeback):' /proc/meminfo 2>/dev/null | tr -s ' ' | tr '\n' ' ')"
echo "  nfs pending: $(nfsstat -c 2>/dev/null | grep -A2 'RPC' | head -2 | tr -s ' ' | tr '\n' ' ')"
echo "  fio raw: $(printf '%s' "$CACHED" | python3 -c '
import json,sys
j=json.load(sys.stdin)["jobs"][0]["write"]
print("iops=%.0f bw=%.0f KiB/s runtime=%dms" % (j["iops"], j["bw"]/1024, j["runtime"]))
' 2>/dev/null)"
mount | grep "ublkb$DEVICE" || echo "  (device not mounted?)"
df -h "$MOUNT" 2>/dev/null | tail -1
echo "  fio json bytes: ${#CACHED}"

echo
echo "================ rcache statistics after the run ================"
"$RCACHE" status --id "$DEVICE" | sed 's/^/  /'

BASE_IOPS=$(echo "$BASE_METRICS" | cut -d' ' -f1)
CACHED_IOPS=$(echo "$CACHED_METRICS" | cut -d' ' -f1)
if [ "$BASE_IOPS" -gt 0 ]; then
    SPEEDUP=$(python3 -c "print(f'{$CACHED_IOPS/$BASE_IOPS:.1f}')")
else
    SPEEDUP="n/a"
fi

echo
echo "================ summary ================"
printf '%-28s %10s %12s %10s\n' "configuration" "IOPS" "MiB/s" "p99 us"
printf '%-28s %10s %12s %10s\n' "origin direct" \
    "$(echo "$BASE_METRICS" | cut -d' ' -f1)" \
    "$(echo "$BASE_METRICS" | cut -d' ' -f2)" \
    "$(echo "$BASE_METRICS" | cut -d' ' -f3)"
printf '%-28s %10s %12s %10s\n' "origin + rcache" \
    "$(echo "$CACHED_METRICS" | cut -d' ' -f1)" \
    "$(echo "$CACHED_METRICS" | cut -d' ' -f2)" \
    "$(echo "$CACHED_METRICS" | cut -d' ' -f3)"
printf '%-28s %10s\n' "speed-up (IOPS)" "$SPEEDUP"

if [ -n "$JSON" ]; then
    python3 - "$JSON" "$BASE_METRICS" "$CACHED_METRICS" "$SPEEDUP" "$SLOW_DIR" "$FAST_DIR" <<'PY'
import json, sys
out, base, cached, speedup, slow, fast = sys.argv[1:7]
b = base.split(); c = cached.split()
data = {
    "slow_dir": slow, "fast_dir": fast,
    "baseline": {"iops": float(b[0]), "bw_mib_s": float(b[1]), "p99_us": float(b[2])},
    "rcache":   {"iops": float(c[0]), "bw_mib_s": float(c[1]), "p99_us": float(c[2])},
    "speedup_iops": speedup,
}
json.dump(data, open(out, "w"), indent=2)
print(f"wrote {out}")
PY
fi
