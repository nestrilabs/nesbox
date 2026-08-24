#!/usr/bin/env bash
# Read a nesbox metrics snapshot.
#
#   scripts/stats.sh /run/nesbox/box.sock          # one snapshot, readable
#   scripts/stats.sh /run/nesbox/box.sock --raw    # the JSON, for jq
#   scripts/stats.sh /run/nesbox/box.sock -w 2     # every 2s, with rates
#
# See docs/STATS.md for the schema. Counters are monotonic; rates below are
# computed from consecutive reads, which is the reader's job by design.
set -uo pipefail

SOCK="${1:-}"
[[ -S "$SOCK" ]] || { echo "usage: $0 <socket> [--raw] [-w SECONDS]" >&2; exit 1; }
shift
RAW=0; WATCH=0
while (($#)); do case $1 in
  --raw) RAW=1;;
  -w) WATCH="${2:-2}"; shift;;
  *) echo "unknown argument: $1" >&2; exit 1;;
esac; shift; done

# No nc/socat dependency: python3 is already required to read this usefully.
read_snapshot() {
  python3 -c '
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
buf = b""
while True:
    chunk = s.recv(4096)
    if not chunk:
        break
    buf += chunk
sys.stdout.write(buf.decode())
' "$SOCK"
}

if ((RAW)); then read_snapshot; exit; fi

render() {
  python3 -c '
import json, sys
cur = json.loads(sys.argv[1])
prev = json.loads(sys.argv[2]) if len(sys.argv) > 2 and sys.argv[2] else None
g = cur.get("gpu")
if not g:
    print("no GPU device"); raise SystemExit
o = g.get("occupancy")
mib = lambda b: b / (1 << 20)
line = "vram %5.0f/%-5.0f MiB  peak %5.0f  refused %d  submits %-8d fences %-8d" % (
    mib(g["vram_bytes"]), mib(g["vram_limit_bytes"]), mib(g["vram_peak_bytes"]),
    g["vram_refusals"], g["submits"], g["fences"])
if o is None:
    line += "  occupancy: none yet (no DRM client)"
else:
    line += "  resident %.0f MiB" % mib(o["resident_vram_bytes"])
    if o["evicted_vram_bytes"]:
        line += "  EVICTED %.0f MiB -- quota above what the card will give" % mib(o["evicted_vram_bytes"])
if prev:
    dt = (cur["uptime_ms"] - prev["uptime_ms"]) / 1000.0
    pg, po = prev.get("gpu"), (prev.get("gpu") or {}).get("occupancy")
    if dt > 0 and o and po:
        dgfx = (o["gfx_ns"] - po["gfx_ns"]) / 1e9
        dfen = g["fences"] - pg["fences"]
        line += "\n  -> %.1f%% of the card" % (100 * dgfx / dt)
        if dfen:
            line += ", %.1f submits/s, %.2f ms GPU per submit" % (dfen / dt, 1000 * dgfx / dfen)
print(line)
' "$1" "${2:-}"
}

if [[ "$WATCH" == 0 ]]; then
  render "$(read_snapshot)"
else
  prev=""
  while :; do
    cur="$(read_snapshot)" || { echo "socket went away" >&2; exit 1; }
    render "$cur" "$prev"
    prev="$cur"
    sleep "$WATCH"
  done
fi
