#!/usr/bin/env bash
# rcache control helper.
#
#   ./rcache.sh start   <ORIGIN_BLKDEV|FILE> <CACHE_FILE> [SIZE] [-- extra rcache args]
#   ./rcache.sh stop    <DEV_ID>       # stop the device (journal is drained)
#   ./rcache.sh status  <DEV_ID>
#   ./rcache.sh devname <DEV_ID>       # print /dev/ublkbN
#
# The origin device is never formatted or otherwise modified up front; it is
# read and written only as part of normal IO. After `stop` (or a clean
# shutdown) every write has been pushed to the origin, so the origin can be
# detached and used on another machine as-is.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
BIN=${RCACHE_BIN:-$HERE/target/release/rcache}

die() { echo "rcache.sh: $*" >&2; exit 1; }

usage() {
    sed -n '3,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 2
}

[[ $# -ge 1 ]] || usage
cmd=$1; shift

case $cmd in
start)
    [[ $# -ge 2 ]] || usage
    origin=$1; cache_file=$2; shift 2
    size=0
    if [[ $# -gt 0 && $1 != "--" ]]; then size=$1; shift; fi
    [[ ${1:-} == "--" ]] && shift

    [[ -e $origin ]] || die "origin $origin does not exist"
    [[ -x $BIN ]] || die "$BIN not found; run 'cargo build --release' first"

    # Cache journal: preallocate so the file never grows/shrinks under us.
    if [[ ! -e $cache_file ]]; then
        [[ $size -gt 0 ]] || die "cache file $cache_file does not exist; pass a SIZE in bytes"
        # Journal needs one 4 KiB header + records; keep it block aligned.
        fallocate -l "$size" "$cache_file"
        echo "rcache.sh: preallocated $cache_file ($size bytes)"
    else
        echo "rcache.sh: reusing existing journal $cache_file"
    fi

    # Keep the journal fsynced as a whole: rcache syncs the file itself.
    if [[ -b $origin ]]; then
        echo "rcache.sh: origin $(basename "$origin") size $(blockdev --getsize64 "$origin") bytes"
    fi

    echo "rcache.sh: starting rcache (foreground; use Ctrl-C or '$0 stop <id>' to stop)"
    exec "$BIN" run --origin "$origin" --cache "$cache_file" "$@"
    ;;
stop)
    [[ $# -ge 1 ]] || usage
    "$BIN" stop --id "$1"
    ;;
status)
    [[ $# -ge 1 ]] || usage
    "$BIN" status --id "$1"
    ;;
devname)
    [[ $# -ge 1 ]] || usage
    echo "/dev/ublkb$1"
    ;;
*)
    usage
    ;;
esac
