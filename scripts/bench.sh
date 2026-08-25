#!/usr/bin/env bash
# Benchmark this host, and write a result somebody else can check.
#
#   scripts/bench.sh                    # everything, ~8 minutes
#   scripts/bench.sh --only gpu
#   scripts/bench.sh --list
#
# Writes benchmarks/<host>.json. Commit it: a committed result is what turns "it
# feels slower" into a diff.
#
# This script runs no measurement of its own. Every number comes from a harness
# that already existed and can still be run by hand -- nesprobe, probe-sweep.sh,
# envelope.sh -- and bench.sh is the thing that runs them all, attaches provenance,
# and writes JSON. If you find yourself adding a probe here, it belongs in its own
# harness first.
#
# Read the numbers as ratios, not absolutes. See docs/BENCHMARKS.md.
set -uo pipefail
cd "$(dirname "$0")/.."
source scripts/bench-provenance.sh

ART=$PWD/artifacts
KERNEL=$ART/vmlinux
ROOTFS=$ART/rootfs.ext4
PROBE=$ART/probe-share
RENDERER=${NESBOX_RENDERER_LIB:-$ART/virgl-nvalid/lib}
NODE=$(bench_render_node)

COST=${BENCH_COST:-400}
SECS=${BENCH_SECONDS:-20}
WARMUP=${BENCH_WARMUP:-8}
OUT=""
SECTIONS=(gpu scaling seccomp envelope)
ONLY=""

while (($#)); do case $1 in
    --only) ONLY="${2:?--only needs a section}"; shift;;
    -o) OUT="${2:?-o needs a path}"; shift;;
    --list) printf '%s\n' "${SECTIONS[@]}"; exit 0;;
    -h|--help) sed -n '2,16p' "$0"; exit 0;;
    *) echo "unknown argument: $1" >&2; exit 2;;
esac; shift; done

[[ -x ./target/release/nesbox ]] || { echo "build first: cargo build --release" >&2; exit 1; }
for f in "$KERNEL" "$ROOTFS" "$PROBE/nesprobe"; do
    [[ -e $f ]] || { echo "missing: $f" >&2; exit 1; }
done
[[ -n $NODE ]] || { echo "no DRM render node found; set NESBOX_RENDER_NODE" >&2; exit 1; }

RUN=$(mktemp -d); trap 'rm -rf "$RUN"' EXIT
say() { printf '%s\n' "$*" >&2; }

# ── One guest, driven over its console ───────────────────────────────────────
# $1 json fragment for extra gpu keys  $2 extra top-level keys  -> stdout: log
run_probe_guest() {
    local gpu_extra="${1:-}" top_extra="${2:-}"
    local sock="$RUN/stats-$RANDOM.sock"
    cat > "$RUN/g.json" <<JSON
{
  "boot-source": { "kernel_image_path": "$KERNEL",
    "boot_args": "console=hvc0 root=/dev/vda ro init=/bin/bash" },
  "drives": [ { "drive_id": "rootfs", "path_on_host": "$ROOTFS",
      "is_root_device": true, "is_read_only": true } ],
  "machine-config": { "vcpu_count": 2, "mem_size_mib": 1536, "cpu_affinity": [2, 3] },
  "gpu": { "render-node": "$NODE", "width": 1920, "height": 1080$gpu_extra },
  "shared-directories": [ { "tag": "probe", "path-on-host": "$PROBE", "read-only": true } ],
  "stats-socket": "$sock"$top_extra
}
JSON
    {
        sleep 13
        echo 'mount -t proc proc /proc; mount -t sysfs sysfs /sys'
        echo 'mount -t devtmpfs devtmpfs /dev 2>/dev/null; mount -t tmpfs tmpfs /tmp'
        echo 'mount -t virtiofs probe /mnt || echo BENCH-SHARE-FAIL'
        sleep 2
        echo "/mnt/nesprobe --cost $COST --seconds $SECS --warmup $WARMUP; echo BENCH-EXIT=\$?"
        sleep $((SECS + WARMUP + 12))
    } | LD_LIBRARY_PATH=$RENDERER timeout $((SECS + WARMUP + 70)) \
          script -qec "env RUST_LOG=warn ./target/release/nesbox $RUN/g.json" /dev/null \
          2>&1 | sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' | tr -d '\r'
}

# Markers are matched at the start of a line. The guest console echoes each
# command before running it, so an unanchored grep for a marker finds the command
# that *would* print it and reports a failure that did not happen.
probe_ok() {
    grep -q '^BENCH-EXIT=0' <<<"$1" && ! grep -q '^BENCH-SHARE-FAIL' <<<"$1"
}
probe_complain() {
    grep -q '^BENCH-SHARE-FAIL' <<<"$1" && say "    WARNING: the virtiofs share did not mount"
    grep -q '^BENCH-EXIT=' <<<"$1" || say "    WARNING: the probe never finished -- the run was cut short"
    local rc; rc=$(awk -F= '/^BENCH-EXIT=/ { print $2; exit }' <<<"$1")
    [[ -n ${rc:-} && $rc != 0 ]] && say "    WARNING: the probe exited $rc"
    return 0
}

# Pull one "frame_ms mean X p50 Y p99 Z max W" field out of a probe log.
probe_field() {
    awk -v k="$2" '/^frame_ms/ { for (i = 1; i <= NF; i++) if ($i == k) { print $(i+1); exit } }' <<<"$1"
}
probe_scalar() { awk -v k="$2" '$1 == k { v = $2 } END { if (v != "") print v }' <<<"$1"; }
jnum() { [[ ${1:-} =~ ^[0-9]+([.][0-9]+)?$ ]] && printf '%s' "$1" || printf 'null'; }

# ── Sections ─────────────────────────────────────────────────────────────────

section_gpu() {
    say "  gpu: one guest, cost=$COST, ${SECS}s after a ${WARMUP}s discard"
    local log; log=$(run_probe_guest ', "vram-limit-mib": 512')
    probe_complain "$log"
    cat <<JSON
{
  "cost": $COST, "seconds": $SECS, "warmup_seconds": $WARMUP,
  "completed": $(probe_ok "$log" && echo true || echo false),
  "frames": $(jnum "$(probe_scalar "$log" frames)"),
  "fps": $(jnum "$(probe_scalar "$log" fps)"),
  "frame_ms": {
    "mean": $(jnum "$(probe_field "$log" mean)"),
    "p50": $(jnum "$(probe_field "$log" p50)"),
    "p99": $(jnum "$(probe_field "$log" p99)"),
    "max": $(jnum "$(probe_field "$log" max)")
  }
}
JSON
}

section_seccomp() {
    say "  seccomp: the same run confined and unconfined"
    local a b
    a=$(run_probe_guest '' ', "seccomp": "off"')
    b=$(run_probe_guest '' ', "seccomp": "enforce"')
    probe_complain "$a"; probe_complain "$b"
    grep -q '^nesbox: seccomp refused syscall' <<<"$b" &&
        say "    WARNING: the policy refused a syscall -- see docs/SECURITY.md"
    cat <<JSON
{
  "off":     { "fps": $(jnum "$(probe_scalar "$a" fps)"), "p50_ms": $(jnum "$(probe_field "$a" p50)"), "p99_ms": $(jnum "$(probe_field "$a" p99)") },
  "enforce": { "fps": $(jnum "$(probe_scalar "$b" fps)"), "p50_ms": $(jnum "$(probe_field "$b" p50)"), "p99_ms": $(jnum "$(probe_field "$b" p99)") },
  "completed": $(probe_ok "$a" && probe_ok "$b" && echo true || echo false),
  "refusals": $(grep -c '^nesbox: seccomp refused syscall' <<<"$b")
}
JSON
}

# probe-sweep.sh expects an assembled directory -- vmlinux, rootfs.ext4,
# probe-share/ and the nesbox binary together, with a patched virglrenderer
# installed system-wide. That is the right contract for a machine that is not this
# one, and it is not how a checkout is laid out. Rather than copy a 5 GiB image or
# install a library, stage a directory of symlinks and point the overrides at it.
stage_sweep_dir() {
    local d="$RUN/sweep"
    mkdir -p "$d"
    ln -sfn "$KERNEL" "$d/vmlinux"
    ln -sfn "$ROOTFS" "$d/rootfs.ext4"
    # Copied, not linked: virtiofsd cannot serve a symlinked share root, and the
    # guest gets "Input/output error" reading a file that is plainly there. It is
    # a few MiB.
    cp -r "$PROBE" "$d/probe-share"
    ln -sfn "$PWD/target/release/nesbox" "$d/nesbox"
    echo "$d"
}

section_scaling() {
    say "  scaling: 1, 2 and 4 guests on one GPU (probe-sweep.sh)"
    local dir; dir=$(stage_sweep_dir)
    local first=1 out="{" all_ok=true
    for n in 1 2 4; do
        say "    n=$n"
        local raw
        raw=$(NESBOX_ARTIFACTS="$dir" NESBOX_BIN="$dir/nesbox" \
              NESBOX_VIRGLRENDERER="$RENDERER" \
              ./scripts/probe-sweep.sh -n "$n" -c "$COST" -s "$SECS" -w "$WARMUP" 2>&1)
        if ! grep -q '^sum fps' <<<"$raw"; then
            say "    WARNING: probe-sweep produced no table: $(tail -1 <<<"$raw")"
        fi
        # A failed guest prints an em dash, not a number. Feeding that straight
        # into a JSON array produces a file that will not parse -- at exactly the
        # moment something has gone wrong, which is the worst time to lose the
        # result. Non-numeric fields become null.
        local sum; sum=$(awk '/^sum fps/ { print $3 }' <<<"$raw")
        local p50s p99s
        p50s=$(awk 'NF==6 && $1 ~ /^[0-9]+$/ { printf "%s%s", sep, ($4 ~ /^[0-9.]+$/ ? $4 : "null"); sep="," }' <<<"$raw")
        p99s=$(awk 'NF==6 && $1 ~ /^[0-9]+$/ { printf "%s%s", sep, ($5 ~ /^[0-9.]+$/ ? $5 : "null"); sep="," }' <<<"$raw")
        # A sweep that produced a table is not a sweep that produced results: a
        # guest that failed prints an em dash, and a sweep that lost every guest
        # still yields "sum fps 0". Without this the section emitted zeros and
        # nulls that look exactly like a measurement, and the merge below then
        # overwrote a good result with them.
        local rows nulls ok=true
        rows=$(awk -v s="$p50s" 'BEGIN { print (s == "") ? 0 : split(s, a, ",") }')
        nulls=$(grep -o null <<<"$p50s,$p99s" | wc -l)
        [[ $rows -eq $n && $nulls -eq 0 ]] || ok=false
        [[ ${sum:-0} != 0 && -n ${sum:-} ]] || ok=false
        $ok || { say "    WARNING: n=$n did not complete"; all_ok=false; }

        ((first)) || out+=","; first=0
        out+="\"$n\":{\"completed\":$ok,\"sum_fps\":$(jnum "$sum"),\"p50_ms\":[${p50s}],\"p99_ms\":[${p99s}]}"
    done
    echo "{\"completed\":$all_ok,\"by_guest_count\":$out}}"
}

section_envelope() {
    say "  envelope: does a cgroup on the VMM bound the guest (envelope.sh)"
    local raw; raw=$(./scripts/envelope.sh all 2>&1)
    # Prose output, deliberately: envelope.sh is written to be read by a person.
    # Only the numbers are lifted, and a miss is null.
    #
    # dd reports "1.9 GB/s" or "53.9 MB/s" and the unit is not decoration. Taking
    # the number and dropping it recorded 1.2 into a field named io_mb_per_s when
    # the truth was 1200 -- a benchmark that looked a thousand times better than
    # reality, in the direction that flatters us. Normalise here.
    # Two traps in three lines of output, both of which produced a confident wrong
    # number before this comment existed:
    #
    #   "  20 MB/s cap, cold: ... copied, 5.8 s, 53.9 MB/s\r"
    #
    # The *label* contains "MB/s", so matching the first unit on the line reports
    # the cap back as though it were the result -- 20, every time, for every cap.
    # And dd's own figure is followed by a carriage return, so an anchored match on
    # "MB/s" misses it entirely and the field goes null. Strip the CR, and take the
    # last unit on the line rather than the first.
    dd_mb_per_s() {
        tr -d '\r' <<<"$1" | awk -v pat="$2" '
            $0 ~ pat {
                v = ""
                for (i = 1; i <= NF; i++)
                    if ($i == "MB/s" || $i == "GB/s") {
                        v = $(i - 1) + 0
                        if ($i == "GB/s") v *= 1000
                    }
                if (v != "") { print v; exit }
            }'
    }
    local mbu mb mbw
    mbu=$(dd_mb_per_s "$raw" "unlimited, cold")
    mb=$(dd_mb_per_s "$raw" "cap, cold")
    mbw=$(dd_mb_per_s "$raw" "cap, warm")
    # All three figures or none: a missing one means a sub-run failed or the
    # prose changed shape, and either way the remaining numbers cannot be compared
    # against each other, which is the entire point of this section.
    local ok=true
    for v in "$mbu" "$mb" "$mbw"; do [[ -n ${v:-} ]] || ok=false; done
    $ok || say "    WARNING: envelope did not complete -- an io figure is missing"

    cat <<JSON
{
  "completed": $ok,
  "io_mb_per_s": {
    "uncapped_cold": $(jnum "$mbu"),
    "capped_20mb_cold": $(jnum "$mb"),
    "capped_20mb_warm_host_cache": $(jnum "$mbw")
  },
  "note": "cpu and memory results are prose in envelope.sh output; see docs/BENCHMARKS.md 12"
}
JSON
}

# ── Run ──────────────────────────────────────────────────────────────────────
declare -A RESULTS
to_run=("${SECTIONS[@]}")
if [[ -n $ONLY ]]; then
    [[ " ${SECTIONS[*]} " == *" $ONLY "* ]] || { echo "unknown section: $ONLY" >&2; exit 2; }
    to_run=("$ONLY")
fi

say "benchmarking on $(uname -n), render node $NODE"
for s in "${to_run[@]}"; do RESULTS[$s]=$("section_$s"); done

HOSTSLUG=$(uname -n | tr -cd 'A-Za-z0-9._-')
[[ -n $OUT ]] || OUT="benchmarks/${HOSTSLUG:-unknown}.json"
mkdir -p "$(dirname "$OUT")"

{
    echo '{'
    echo '  "schema": 1,'
    printf '  "disclaimer": "%s",\n' \
      "One host, one GPU, one synthetic workload. Ratios compare a guest to this same host and are the durable results; absolute figures are this machine's and not the hardware's. No comparison against another hypervisor was run, so nothing here supports a claim of being faster than one."
    echo '  "provenance":'
    bench_provenance_json | sed 's/^/  /'
    echo '  ,'
    echo '  "not_measured": {'
    echo '    "network": "no iperf3 in the guest image; adding one is image work",'
    echo '    "storage_random_io": "no fio in the guest image; only sequential dd via envelope.sh",'
    echo '    "boot_time": "not implemented; would be a new measurement rather than an orchestration",'
    echo '    "other_hypervisors": "none run; this is what a comparative claim would need"'
    echo '  },'
    echo '  "results": {'
    sep=""
    for s in "${to_run[@]}"; do
        printf '%s    "%s": %s' "$sep" "$s" "$(sed 's/^/    /;1s/^ *//' <<<"${RESULTS[$s]}")"
        sep=$',\n'
    done
    echo
    echo '  }'
    echo '}'
} > "$OUT.tmp"

# Merge into whatever is already there rather than replacing it. Without this,
# `--only gpu` silently reduces a committed full result to a single section, which
# is a nasty way to lose measurements that took a quarter of an hour.
MERGE='
import json, os, sys
tmp, out, ran = sys.argv[1], sys.argv[2], sys.argv[3].split()
fresh = json.load(open(tmp))
if os.path.exists(out):
    try:
        old = json.load(open(out))
        merged = dict(old.get("results", {}))
        kept = []
        for name, new_section in fresh.get("results", {}).items():
            was = merged.get(name)
            # A run that failed still emits a shaped section. Overwriting a good
            # measurement with it loses the good one and leaves something that
            # reads like a result, which is the worse of the two outcomes.
            if (isinstance(was, dict) and was.get("completed") is True
                    and isinstance(new_section, dict)
                    and new_section.get("completed") is not True):
                kept.append(name)
                continue
            merged[name] = new_section
        if kept:
            fresh["kept_earlier_result_for"] = kept
            sys.stderr.write(
                "  kept the earlier completed result for: %s\n" % ", ".join(kept))
        fresh["results"] = merged
        # Sections carried over were measured under an earlier provenance. Say so
        # rather than implying one sitting.
        stale = sorted(set(merged) - set(ran))
        if stale:
            fresh["carried_from_earlier_run"] = stale
    except (OSError, ValueError):
        pass  # an unreadable previous result is no reason to lose this one
fresh["sections_run"] = ran
with open(out, "w") as f:
    json.dump(fresh, f, indent=2)
    f.write("\n")
'
if python3 -c "$MERGE" "$OUT.tmp" "$OUT" "${to_run[*]}"; then
    rm -f "$OUT.tmp"
    say ""
    say "wrote $OUT"
else
    # Deliberately not written over $OUT. A committed result is worth more than
    # this run's output, and replacing it with something that would not parse
    # loses both.
    say "WARNING: could not assemble valid JSON. $OUT is untouched;"
    say "         the unparsable output is at $OUT.tmp"
    exit 1
fi
