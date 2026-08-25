#!/usr/bin/env bash
# Does a cgroup on the VMM process bound the guest inside it?
#
# A guest gets CPU, memory and disk through the VMM's own threads, so the kernel's
# cgroup controllers should bound a guest without nesbox implementing anything.
# "Should" is the reason this script exists.
#
#   scripts/envelope.sh              # all three
#   scripts/envelope.sh cpu          # one
#
# Runs unprivileged. systemd delegates cpu, io, memory and cpuset to the user
# session, so `systemd-run --user --scope` applies a limit with no root and no
# hand-built cgroup -- which is also the shape a supervisor would use.
set -uo pipefail
cd "$(dirname "$0")/.."

ART=$PWD/artifacts
KERNEL=$ART/vmlinux
ROOTFS=$ART/rootfs.ext4
RENDERER=$ART/virgl-nvalid/lib
WHICH="${1:-all}"

[[ -x ./target/release/nesbox ]] || { echo "build first: cargo build --release" >&2; exit 1; }
for f in "$KERNEL" "$ROOTFS"; do [[ -e $f ]] || { echo "missing: $f" >&2; exit 1; }; done

# The disk io.max applies to is the whole device, not the partition.
DISK=$(lsblk -no PKNAME "$(df --output=source "$ART" | tail -1)" 2>/dev/null | head -1)
DEVNO=$(cat "/sys/block/$DISK/dev" 2>/dev/null)
[[ -n ${DEVNO:-} ]] || { echo "could not resolve a backing device for $ART" >&2; exit 1; }

RUN=$(mktemp -d); trap 'rm -rf "$RUN"' EXIT

# Drop the host page cache for the disk image between runs. Without this an
# io.max cap measures nothing: the VMM serves the guest from cache, no device I/O
# happens, and the cap never applies. Needs no root -- POSIX_FADV_DONTNEED evicts
# only this file's pages.
evict() {
  python3 -c 'import os,sys
fd = os.open(sys.argv[1], os.O_RDONLY)
os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
os.close(fd)' "$1"
}

# $1 name  $2 mem_mib  $3 guest commands (one per line, run after boot)
make_cfg() {
  cat > "$RUN/$1.json" <<JSON
{
  "boot-source": { "kernel_image_path": "$KERNEL",
    "boot_args": "console=hvc0 root=/dev/vda ro init=/bin/bash" },
  "drives": [ { "drive_id": "rootfs", "path_on_host": "$ROOTFS",
      "is_root_device": true, "is_read_only": true } ],
  "machine-config": { "vcpu_count": 2, "mem_size_mib": $2, "cpu_affinity": [2,3] }
}
JSON
}

# $1 cfg name  $2 seconds to allow  $3.. systemd-run properties
run_guest() {
  local cfg=$1 secs=$2; shift 2
  local props=()
  for p in "$@"; do props+=(-p "$p"); done
  {
    sleep 12
    echo 'mount -t proc proc /proc; mount -t sysfs sysfs /sys'
    echo 'mount -t devtmpfs devtmpfs /dev 2>/dev/null'
    echo 'mount -t tmpfs tmpfs /tmp'
    cat "$RUN/$cfg.cmds"
    sleep "$secs"
  } | LD_LIBRARY_PATH=$RENDERER timeout $((secs + 45)) \
        systemd-run --user --scope -q --collect "${props[@]}" -- \
        script -qec "env RUST_LOG=warn ./target/release/nesbox $RUN/$cfg.json" /dev/null \
        2>&1 | sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g'
}

hr() { printf '\n== %s\n' "$1"; }

# ── CPU ───────────────────────────────────────────────────────────────────────
# openssl is in the guest image; it reports a throughput number, which is what a
# quota should scale.
bench_cpu() {
  hr "cpu.max -- does a quota on the VMM scale the guest's CPU throughput?"
  echo 'openssl speed -elapsed -seconds 4 sha256 2>&1 | tail -2' > "$RUN/cpu.cmds"
  make_cfg cpu 1024
  local uncapped capped
  uncapped=$(run_guest cpu 14 | grep -a "^sha256" | tail -1)
  echo "  unlimited : ${uncapped:-FAILED}"
  # 50% of one core, against two vCPUs pinned to two cores.
  capped=$(run_guest cpu 14 CPUQuota=50% | grep -a "^sha256" | tail -1)
  echo "  CPUQuota=50%: ${capped:-FAILED}"
  echo "  (openssl reports k-bytes/sec per block size; the ratio is what matters)"
}

# ── I/O ───────────────────────────────────────────────────────────────────────
# Reads the raw disk, not a file, so nothing in the guest caches it. The host
# page cache is dropped by reading a distinct region each time where possible.
bench_io() {
  hr "io.max -- does a bandwidth cap on the VMM bound guest disk reads?"
  echo 'dd if=/dev/vda of=/dev/null bs=1M count=300 iflag=direct 2>&1 | tail -1' > "$RUN/io.cmds"
  make_cfg io 1024
  echo "  device: $DISK ($DEVNO)"
  local uncapped capped warm
  evict "$ROOTFS"
  uncapped=$(run_guest io 20 | grep -aE "copied|bytes" | tail -1)
  echo "  unlimited, cold  : ${uncapped:-FAILED}"

  evict "$ROOTFS"
  capped=$(run_guest io 30 "IOReadBandwidthMax=/dev/$DISK 20M" | grep -aE "copied|bytes" | tail -1)
  echo "  20 MB/s cap, cold: ${capped:-FAILED}"

  # No evict: the previous run left the image in the host page cache.
  warm=$(run_guest io 30 "IOReadBandwidthMax=/dev/$DISK 20M" | grep -aE "copied|bytes" | tail -1)
  echo "  20 MB/s cap, warm: ${warm:-FAILED}"
  echo
  echo "  The third line is the finding. io.max bounds *device* I/O, and a read the"
  echo "  host page cache satisfies never reaches the device. iflag=direct stops the"
  echo "  GUEST caching; nothing here stops the HOST caching the backing file. So an"
  echo "  I/O bound on a guest holds only while its working set misses host cache --"
  echo "  which is the same missing O_DIRECT that makes every guest byte cost host"
  echo "  memory twice."
}

# ── Memory ────────────────────────────────────────────────────────────────────
# The dangerous one, and the point is to find out *how* it fails.
bench_mem() {
  hr "memory.max -- what does a cap below the guest's RAM actually do?"
  # Write into guest tmpfs, which is guest RAM, so the VMM really faults the
  # pages in. The throughput is the interesting number, not survival.
  echo 'dd if=/dev/zero of=/tmp/fill bs=1M count=700 2>&1 | tail -1' > "$RUN/mem.cmds"
  make_cfg mem 1024

  local swap
  swap=$(swapon --show=NAME,SIZE --noheadings 2>/dev/null | tr '\n' ' ')
  echo "  host swap: ${swap:-none}"

  local roomy tight
  roomy=$(run_guest mem 25 MemoryMax=2G | grep -aE "copied" | tail -1)
  echo "  MemoryMax=2G   (above guest RAM): ${roomy:-FAILED}"
  tight=$(run_guest mem 40 MemoryMax=384M | grep -aE "copied" | tail -1)
  echo "  MemoryMax=384M (below guest RAM): ${tight:-FAILED}"
  echo
  echo "  Guest RAM is anonymous memory in the VMM, so memory.max counts it. What a"
  echo "  cap below guest RAM does depends entirely on whether the host has swap:"
  echo
  echo "    with swap    -- the VMM is not killed and the guest is not shrunk. Guest"
  echo "                    RAM is reclaimed into host swap, and the guest stutters"
  echo "                    on faults it cannot see or account for. Degradation with"
  echo "                    no error anywhere, which is the worst of the three."
  echo "    without swap -- the VMM is OOM-killed once the guest touches enough"
  echo "                    pages. The box dies; the host survives."
  echo
  echo "  Either way memory.max is a blast radius and not a dial: it cannot make a"
  echo "  guest use less memory, only punish it for using what it was given. Guest"
  echo "  RAM has to be right at boot."
}

case "$WHICH" in
  cpu) bench_cpu;;
  io)  bench_io;;
  mem|memory) bench_mem;;
  all) bench_cpu; bench_io; bench_mem;;
  *) echo "unknown: $WHICH (cpu|io|mem|all)" >&2; exit 1;;
esac
echo
