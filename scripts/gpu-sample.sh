#!/usr/bin/env bash
# Sample a guest's GPU occupancy and VRAM from the kernel's per-client DRM
# accounting.
#
# Read here rather than from fences on purpose: a fence measures submit-to-signal
# *latency*, which with more than one guest on the card includes time queued behind
# another guest's work. drm-engine-gfx measures *occupancy*. Solo the two agree,
# which is exactly how confusing them survives a single-guest experiment.
#
# One nesbox process is one DRM client, so per-process is per-guest.
#
# Usage:  scripts/gpu-sample.sh [pid] [interval_ms]
#         scripts/gpu-sample.sh            # auto-detect a single running nesbox
set -uo pipefail

PID="${1:-}"
INTERVAL_MS="${2:-500}"

if [[ -z "$PID" ]]; then
  mapfile -t pids < <(pgrep -x nesbox)
  case ${#pids[@]} in
    0) echo "no nesbox running; pass a pid explicitly" >&2; exit 1 ;;
    1) PID="${pids[0]}" ;;
    *) echo "several nesbox processes: ${pids[*]} -- pass one" >&2; exit 1 ;;
  esac
fi

[[ -d /proc/$PID ]] || { echo "no such pid: $PID" >&2; exit 1; }

# The DRM fd is whichever fdinfo advertises an engine counter.
find_drm_fd() {
  local f
  for f in /proc/$PID/fdinfo/*; do
    grep -qs '^drm-engine-gfx:' "$f" 2>/dev/null && { basename "$f"; return 0; }
  done
  return 1
}

FD="$(find_drm_fd)" || {
  echo "pid $PID has no DRM client fd with engine accounting yet." >&2
  echo "The guest has not touched the GPU. Run something in it first." >&2
  exit 1
}

# Re-resolve the fd every sample. A guest context is a host DRM client: the fd
# appears when the guest first touches the GPU and vanishes when that context is
# destroyed, so a number captured once goes stale mid-run.
field() {
  local v
  v=$(grep -m1 "^$1:" "/proc/$PID/fdinfo/$FD" 2>/dev/null | awk '{print $2}')
  if [[ -z "$v" ]]; then
    FD="$(find_drm_fd)" || return 1
    v=$(grep -m1 "^$1:" "/proc/$PID/fdinfo/$FD" 2>/dev/null | awk '{print $2}')
  fi
  printf '%s' "$v"
}

echo "pid $PID  fd $FD  pdev $(field drm-pdev)  interval ${INTERVAL_MS}ms"
printf '%12s %10s %10s %12s %12s %10s\n' \
  gfx_ms busy_% compute_ms vram_MiB resident_MiB evicted_MiB

prev_gfx=$(field drm-engine-gfx); prev_comp=$(field drm-engine-compute)
prev_ns=$(date +%s%N)
: "${prev_comp:=0}"

while sleep "$(awk "BEGIN{print $INTERVAL_MS/1000}")"; do
  [[ -d /proc/$PID ]] || { echo "pid $PID gone"; exit 0; }
  gfx=$(field drm-engine-gfx); comp=$(field drm-engine-compute); now_ns=$(date +%s%N)
  : "${gfx:=$prev_gfx}" "${comp:=$prev_comp}"

  # KiB values carry a unit suffix; take the number.
  vram=$(grep -m1 '^amd-requested-vram:' /proc/$PID/fdinfo/$FD | awk '{print $2}')
  res=$(grep -m1 '^drm-resident-vram:'  /proc/$PID/fdinfo/$FD | awk '{print $2}')
  evict=$(grep -m1 '^amd-evicted-vram:' /proc/$PID/fdinfo/$FD | awk '{print $2}')
  : "${vram:=0}" "${res:=0}" "${evict:=0}"

  awk -v g="$gfx" -v pg="$prev_gfx" -v c="$comp" -v pc="$prev_comp" \
      -v n="$now_ns" -v pn="$prev_ns" \
      -v v="$vram" -v r="$res" -v e="$evict" 'BEGIN{
    wall_ms = (n - pn) / 1e6
    gfx_ms  = (g - pg) / 1e6
    comp_ms = (c - pc) / 1e6
    busy    = wall_ms > 0 ? 100 * gfx_ms / wall_ms : 0
    printf "%12.2f %10.1f %10.2f %12.1f %12.1f %10.1f\n", \
      gfx_ms, busy, comp_ms, v/1024, r/1024, e/1024
  }'

  prev_gfx=$gfx; prev_comp=$comp; prev_ns=$now_ns
done
