#!/usr/bin/env bash
# Concurrency sweep: run N guests, each driving nesprobe at a fixed per-frame cost,
# and report what each achieved. This is the instrument for "how does one GPU behave
# when several microVMs render on it at once".
#
#   scripts/probe-sweep.sh [-n N] [-c COST] [-s SECONDS] [-m MIB]
#
# All guests share ONE read-only rootfs. Nothing in the guest needs to write: the
# probe arrives over virtiofs and /tmp is a tmpfs. So a sweep costs no disk and
# cannot corrupt an image.
set -uo pipefail

N=2; COST=400; SECS=30; MEM=1536; WARMUP=8
while getopts "n:c:s:m:w:h" o; do case $o in
  n) N=$OPTARG;; c) COST=$OPTARG;; s) SECS=$OPTARG;; m) MEM=$OPTARG;; w) WARMUP=$OPTARG;;
  h) sed -n '2,12p' "$0"; exit 0;;
esac; done

ART=$PWD
KERNEL=$ART/vmlinux
ROOTFS=$ART/rootfs.ext4
PROBE=$ART/probe-share
VIRGLRENDERER=/usr/lib
NESBOX=$ART/nesbox
RUN=$(mktemp -d)
trap 'rm -rf "$RUN"' EXIT

for f in "$KERNEL" "$ROOTFS" "$PROBE/nesprobe" "$VIRGLRENDERER/libvirglrenderer.so.1"; do
  [[ -e $f ]] || { echo "missing: $f" >&2; exit 1; }
done
[[ -x "$NESBOX" ]] || { echo "build nesbox first: cargo build --release" >&2; exit 1; }

echo "sweep: N=$N cost=$COST seconds=$SECS warmup=${WARMUP}s mem=${MEM}MiB"
# (dathorse): Below is not a good way to make scripts, pointing to something that "works on my machine" isn't valid
#echo "renderer: $(cd ~/forks/virglrenderer 2>/dev/null && git rev-parse --short HEAD || echo unknown) (patched)"
echo

# One physical core per guest, leaving core 0 for the host. Siblings pair as
# (0,1)(2,3)... on this machine -- check yours with
# /sys/devices/system/cpu/cpu0/topology/thread_siblings_list before trusting it.
for i in $(seq 0 $((N-1))); do
  a=$((2 + i*2)); b=$((a+1))
  cat > "$RUN/guest$i.json" <<JSON
{
  "boot-source": {
    "kernel_image_path": "$KERNEL",
    "boot_args": "console=hvc0 root=/dev/vda ro init=/bin/bash"
  },
  "drives": [
    { "drive_id": "rootfs", "path_on_host": "$ROOTFS",
      "is_root_device": true, "is_read_only": true }
  ],
  "machine-config": { "vcpu_count": 2, "mem_size_mib": $MEM, "cpu_affinity": [$a, $b] },
  "gpu": { "render-node": "/dev/dri/renderD128", "width": 1920, "height": 1080 },
  "shared-directories": [
    { "tag": "probe", "path-on-host": "$PROBE", "read-only": true }
  ]
}
JSON
  # The guest is init=/bin/bash, so drive it over the console.
  {
    sleep 14
    echo 'mount -t proc proc /proc; mount -t sysfs sysfs /sys'
    echo 'mount -t devtmpfs devtmpfs /dev 2>/dev/null; mkdir -p /dev/pts; mount -t devpts devpts /dev/pts'
    echo 'mount -t tmpfs tmpfs /tmp; mount -t tmpfs tmpfs /run'
    # Mount on /mnt, which already exists: the rootfs is read-only so a fresh
    # mount point cannot be created.
    echo 'mount -t virtiofs probe /mnt || echo SHARE-FAIL'
    sleep 3
    echo "/mnt/nesprobe --cost $COST --seconds $SECS --warmup $WARMUP"
    sleep $((SECS + WARMUP + 12))
  } | LD_LIBRARY_PATH=$VIRGLRENDERER timeout $((SECS + WARMUP + 65)) \
        script -qec "env RUST_LOG=warn "$NESBOX" $RUN/guest$i.json" /dev/null \
        > "$RUN/guest$i.log" 2>&1 &
  sleep 1
done

echo "waiting for $N guests (~$((SECS + WARMUP + 40))s)..."
wait

printf '%-6s %10s %10s %10s %10s %10s\n' guest frames fps p50_ms p99_ms max_ms
tf=0
for i in $(seq 0 $((N-1))); do
  L="$RUN/guest$i.log"
  clean=$(sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' "$L")
  fr=$(grep -a '^frames' <<<"$clean" | awk '{print $2}' | tail -1)
  fp=$(grep -a '^fps' <<<"$clean" | awk '{print $2}' | tail -1)
  ms=$(grep -a '^frame_ms' <<<"$clean" | tail -1)
  p50=$(awk '{for(i=1;i<=NF;i++) if($i=="p50") print $(i+1)}' <<<"$ms")
  p99=$(awk '{for(i=1;i<=NF;i++) if($i=="p99") print $(i+1)}' <<<"$ms")
  mx=$(awk  '{for(i=1;i<=NF;i++) if($i=="max") print $(i+1)}' <<<"$ms")
  printf '%-6s %10s %10s %10s %10s %10s\n' "$i" "${fr:-FAIL}" "${fp:-—}" "${p50:-—}" "${p99:-—}" "${mx:-—}"
  [[ -n ${fp:-} ]] && tf=$(awk -v a=$tf -v b=$fp 'BEGIN{print a+b}')
  cp "$L" "/tmp/probe-sweep-n${N}-c${COST}-guest${i}.log"
done
echo
echo "sum fps      $tf"
echo "logs         /tmp/probe-sweep-n${N}-c${COST}-guest*.log"
echo
echo "NOTE: figures exclude the first ${WARMUP}s. An idle AMD GPU sits in a low DPM"
echo "      state and takes seconds to reach full clocks -- measured 716 -> 2000 MHz"
echo "      over ~2.5s -- and those frames are numerous enough to BE the p99."
echo "      A p99 measured without a warmup discard measures the clock ramp."
