#!/usr/bin/env bash
# What does the block device actually do, and does a change to it help?
#
#   scripts/bench-blk.sh                          # measure ./target/release/nesbox
#   scripts/bench-blk.sh --against /path/nesbox   # A/B it against another binary
#   scripts/bench-blk.sh --reps 5
#   scripts/bench-blk.sh --cache buffered      # the drive's "direct" flag
#
# Needs a kernel and an image. Either put them in artifacts/ as the other scripts
# do, or point NESBOX_KERNEL and NESBOX_IMAGE somewhere.
#
# Three cases, and each exists because it separates something the others confuse:
#
#   seq1m      300 MiB in 1 MiB direct reads. One request at a time, so it is a
#              measure of per-request cost and of the host's read path, not of
#              anything the device does concurrently.
#   par8       eight 32 MiB readers at once. This is the case a serialised
#              device cannot win: depth is the whole difference.
#   rand4k     8000 4 KiB direct reads at scattered offsets, one at a time. The
#              per-request floor -- notify, wake a worker, one read, interrupt --
#              with no throughput to hide behind.
#
# Reads start 1 GiB into the image, past everything the boot itself touched, and
# each case reads its own 1 GiB-apart region, so that a cold run is cold for all
# three rather than only the first.
#
# ── Reading the output ──────────────────────────────────────────────────────
#
# Runs alternate between binaries rather than running all of A then all of B:
# clocks drift over minutes -- thermally, or because something else on the box
# woke up -- and an A/B where all of one side ran first measures the drift. The median of `--reps` is reported, with the spread, and a
# spread wider than the difference between binaries means the run proved nothing.
#
# `--cold` evicts the image from the host page cache before each boot. It is off
# by default because it does not work everywhere: POSIX_FADV_DONTNEED evicts page
# cache, and on ZFS the data is in the ARC, which it does not touch. A "cold" run
# against a ZFS-backed image is a warm run with extra steps -- see
# docs/BENCHMARKS.md §12.2 on why a cache state that is not stated is a cache
# state that is being measured.
set -uo pipefail
cd "$(dirname "$0")/.."

ART=$PWD/artifacts
KERNEL=${NESBOX_KERNEL:-$ART/vmlinux}
IMAGE=${NESBOX_IMAGE:-$ART/rootfs.ext4}
BIN=${NESBOX_BIN:-./target/release/nesbox}
AGAINST=""
REPS=3
COLD=0
CACHE=auto
VCPUS=4
MEM=2048

while (($#)); do case $1 in
    --against) AGAINST="${2:?--against needs a path to a nesbox binary}"; shift;;
    --reps) REPS="${2:?--reps needs a count}"; shift;;
    --cold) COLD=1;;
    --cache) CACHE="${2:?--cache needs auto, direct or buffered}"; shift;;
    --vcpus) VCPUS="${2:?}"; shift;;
    -h|--help) sed -n '2,36p' "$0"; exit 0;;
    *) echo "unknown argument: $1" >&2; exit 2;;
esac; shift; done

for f in "$KERNEL" "$IMAGE" "$BIN"; do
    [[ -e $f ]] || { echo "missing: $f" >&2; exit 1; }
done
[[ -z $AGAINST || -x $AGAINST ]] || { echo "not executable: $AGAINST" >&2; exit 1; }

RUN=$(mktemp -d); trap 'rm -rf "$RUN"' EXIT

cat > "$RUN/vm.json" <<JSON
{
  "boot-source": {
    "kernel_image_path": "$KERNEL",
    "boot_args": "console=hvc0 root=/dev/vda ro init=/bin/sh panic=-1"
  },
  "drives": [
    { "drive_id": "rootfs", "path_on_host": "$IMAGE",
      "is_root_device": true, "is_read_only": true$(
        case $CACHE in
          direct) printf ', "direct": true';;
          buffered) printf ', "direct": false';;
        esac) }
  ],
  "machine-config": { "vcpu_count": $VCPUS, "mem_size_mib": $MEM }
}
JSON

# Evict this file from the host page cache. Needs no root: POSIX_FADV_DONTNEED
# drops only the pages of the file it is given.
evict() {
  python3 -c 'import os,sys
fd = os.open(sys.argv[1], os.O_RDONLY)
os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
os.close(fd)' "$1"
}

# The guest side. Each case prints one RESULT line, which is what gets parsed --
# dd is left to write its own summary to the log for a human to check against.
#
# Nothing here powers the guest off: `poweroff -f` wants a writable /run, which a
# read-only rootfs has not got. Instead the writer ends, the shell that is init
# reads EOF and exits, and the kernel panics with `Attempted to kill init!` --
# this repo's documented way of ending a scripted guest, and an immediate one.
# The EOF queues behind whatever the guest is still running, so it cannot cut a
# measurement short.
guest_script() {
  cat <<'SH'
# Each case reads a region of its own, 1 GiB apart. Sharing one offset makes a
# `--cold` run a lie: the first case pulls the region into host cache and every
# case after it is served warm from what the first one faulted in. Measured, that
# turned an eight-reader "cold" case into 26 GB/s.
SEQ_SKIP=1024
PAR_SKIP=2048
RAND_SKIP=3072
now() { date +%s%N; }
report() { echo "RESULT $1 $2 $((($4 - $3) / 1000000))"; }

# Unsuppressed on purpose: the timed runs send dd's summary to /dev/null, so this
# one line is the only evidence in the log that the reads below are the reads
# they claim to be. A direct read the guest refused would show up here as an
# error rather than as a suspiciously fast measurement.
echo "CHECK direct read:"
dd if=/dev/vda of=/dev/null bs=4k count=1 skip=$SEQ_SKIP iflag=direct

s=$(now)
dd if=/dev/vda of=/dev/null bs=1M count=300 skip=$SEQ_SKIP iflag=direct 2>/dev/null
e=$(now); report seq1m $((300 * 1048576)) $s $e

s=$(now)
for i in 0 1 2 3 4 5 6 7; do
  dd if=/dev/vda of=/dev/null bs=1M count=32 skip=$((PAR_SKIP + i * 32)) iflag=direct 2>/dev/null &
done
wait
e=$(now); report par8 $((8 * 32 * 1048576)) $s $e

s=$(now)
dd if=/dev/vda of=/dev/null bs=4k count=8000 skip=$((RAND_SKIP * 256)) iflag=direct 2>/dev/null
e=$(now); report rand4k $((8000 * 4096)) $s $e

echo RESULT-END
# Ends the guest. The shell is init, so this panics the kernel with `Attempted
# to kill init!`, and the config's `panic=-1` turns that panic into a reset,
# which nesbox exits on.
#
# Both halves are needed. Sent as a command rather than left to EOF, because
# `script` holds the pty open when its own stdin ends and the shell would sit at
# a prompt until the timeout. And with `panic=-1`, because a panicking guest
# does *not* stop the VMM on its own -- the kernel halts, KVM keeps running it,
# and every run would be ended by the timeout instead of by the guest, which is
# the difference between a 25-second run and a three-minute one.
exit
SH
}

# $1 binary, $2 log path. Prints "case bytes ms" lines.
run_once() {
  local bin=$1 log=$2
  ((COLD)) && evict "$IMAGE"
  # `script` because the console wants a pty; the first sleep is the guest
  # reaching a shell, which is boot time and not anything being measured.
  #
  # Fed by a process substitution rather than a pipe, so that this returns the
  # moment the VM is gone. Through a pipe the writer's trailing sleep -- there
  # to let the guest finish, and deliberately generous -- holds the pipeline
  # open long after the guest has powered off, and every run pays it.
  timeout -k 5 180 script -qec "$bin $RUN/vm.json" /dev/null > "$log" 2>&1 < <(
    sleep 12
    guest_script
  )
  # Strip the pty's bracketed-paste escapes before parsing.
  sed -e 's/\x1b\[?2004[hl]//g' -e 's/\r//g' "$log" | awk '/^RESULT [a-z]/ {print $2, $3, $4}'
}

median() { sort -n | awk '{v[NR]=$1} END {if (NR) print (NR % 2) ? v[(NR+1)/2] : int((v[NR/2]+v[NR/2+1])/2)}'; }

declare -A SAMPLES
CASES=(seq1m par8 rand4k)

echo "image:   $IMAGE"
echo "cache mode: $CACHE$( [[ $CACHE == auto ]] && echo " (direct where the filesystem supports it)")"
echo "cache:   $( ((COLD)) && echo "evicted before each boot" || echo "whatever the host had (warm)")"
echo "reps:    $REPS"
echo

for ((r = 1; r <= REPS; r++)); do
  for side in a b; do
    [[ $side == b && -z $AGAINST ]] && continue
    bin=$BIN; [[ $side == b ]] && bin=$AGAINST
    echo "run $r/$REPS: $bin"
    while read -r name bytes ms; do
      [[ -n ${ms:-} && $ms -gt 0 ]] || continue
      SAMPLES[$side,$name]+="$((bytes / 1000 / ms)) "
    done < <(run_once "$bin" "$RUN/$side-$r.log")
  done
done

echo
printf '%-8s %14s' case "$(basename "$BIN")"
[[ -n $AGAINST ]] && printf ' %14s %8s' "$(basename "$AGAINST")" ratio
printf '\n'

for c in "${CASES[@]}"; do
  a=$(tr ' ' '\n' <<<"${SAMPLES[a,$c]:-}" | grep -v '^$' | median)
  printf '%-8s %11s MB/s' "$c" "${a:-n/a}"
  if [[ -n $AGAINST ]]; then
    b=$(tr ' ' '\n' <<<"${SAMPLES[b,$c]:-}" | grep -v '^$' | median)
    printf ' %11s MB/s' "${b:-n/a}"
    [[ -n ${a:-} && -n ${b:-} && ${b:-0} -gt 0 ]] &&
      printf ' %7sx' "$(awk -v a="$a" -v b="$b" 'BEGIN{printf "%.2f", a/b}')"
  fi
  printf '\n'
  # The spread is not decoration: a difference smaller than it is not a result.
  printf '  %-6s samples: %s\n' "$(basename "$BIN")" "${SAMPLES[a,$c]:-none}"
  [[ -n $AGAINST ]] && printf '  %-6s samples: %s\n' "$(basename "$AGAINST")" "${SAMPLES[b,$c]:-none}"
done
