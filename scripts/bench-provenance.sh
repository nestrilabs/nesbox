#!/usr/bin/env bash
# Emit this host's provenance as JSON. Sourced by bench.sh; runnable alone.
#
# A result without provenance is not a result. Two numbers in this file were added
# only after they invalidated a finding -- the CPU governor and the GPU's power
# states -- so it errs towards recording too much.
#
# Every field degrades to null rather than failing: this has to run on a machine
# nobody has seen yet.
set -uo pipefail

_first() { for f in "$@"; do [[ -r $f ]] && { tr -d '\n' < "$f"; return; }; done; }
_json_str() { [[ -n ${1:-} ]] && printf '"%s"' "${1//\"/\\\"}" || printf 'null'; }
_json_num() { [[ ${1:-} =~ ^[0-9]+$ ]] && printf '%s' "$1" || printf 'null'; }

# The render node to test. Prefers the one a caller names, else the first that a
# DRM driver has actually claimed -- on a multi-GPU host renderD128 may not be the
# one we mean.
bench_render_node() {
    if [[ -n ${NESBOX_RENDER_NODE:-} ]]; then echo "$NESBOX_RENDER_NODE"; return; fi
    local n
    for n in /dev/dri/renderD*; do
        [[ -e $n ]] || continue
        echo "$n"; return
    done
}

# PCI id and marketing name for whichever card backs that render node.
_gpu_ids() {
    local node="${1:-}" sys drv
    [[ -n $node ]] || return
    sys="/sys/class/drm/$(basename "$node")/device"
    [[ -d $sys ]] || return
    drv=$(basename "$(readlink -f "$sys/driver" 2>/dev/null)" 2>/dev/null)
    printf '%s %s %s' \
        "$(_first "$sys/vendor" | sed 's/^0x//')" \
        "$(_first "$sys/device" | sed 's/^0x//')" \
        "${drv:-unknown}"
}

bench_provenance_json() {
    local node vendor device driver
    node=$(bench_render_node)
    read -r vendor device driver <<<"$(_gpu_ids "$node")"

    # Power. The single most misleading thing about the first reference host was
    # that it was a laptop on battery at half its rated clock.
    local on_ac="null" f
    for f in /sys/class/power_supply/*/online; do
        [[ -r $f ]] || continue
        [[ $(cat "$f") == 1 ]] && on_ac=true || on_ac=false
        break
    done
    local battery="null"
    for f in /sys/class/power_supply/*/status; do
        [[ -r $f ]] && { battery=$(_json_str "$(tr -d '\n' < "$f")"); break; }
    done

    local cpu0=/sys/devices/system/cpu/cpu0/cpufreq
    local dpm=""
    for f in /sys/class/drm/card*/device/pp_dpm_sclk; do
        [[ -r $f ]] && { dpm=$(awk '{printf "%s ", $2}' "$f" | sed 's/ $//'); break; }
    done

    cat <<JSON
{
  "host": $(_json_str "$(uname -n)"),
  "kernel": $(_json_str "$(uname -r)"),
  "cpu": {
    "model": $(_json_str "$(awk -F': ' '/^model name/{print $2; exit}' /proc/cpuinfo)"),
    "online": $(_json_num "$(nproc 2>/dev/null)"),
    "governor": $(_json_str "$(_first $cpu0/scaling_governor)"),
    "energy_performance_preference": $(_json_str "$(_first $cpu0/energy_performance_preference)"),
    "scaling_max_khz": $(_json_num "$(_first $cpu0/scaling_max_freq)"),
    "scaling_min_khz": $(_json_num "$(_first $cpu0/scaling_min_freq)"),
    "amd_pstate": $(_json_str "$(_first /sys/devices/system/cpu/amd_pstate/status)")
  },
  "gpu": {
    "render_node": $(_json_str "$node"),
    "pci_id": $(_json_str "${vendor:-}${device:+:$device}"),
    "driver": $(_json_str "${driver:-}"),
    "dpm_sclk_mhz": $(_json_str "$dpm")
  },
  "power": {
    "on_ac": $on_ac,
    "battery_status": $battery,
    "platform_profile": $(_json_str "$(_first /sys/firmware/acpi/platform_profile)")
  },
  "memory": {
    "total_kb": $(_json_num "$(awk '/^MemTotal/{print $2}' /proc/meminfo)"),
    "swap_total_kb": $(_json_num "$(awk '/^SwapTotal/{print $2}' /proc/meminfo)"),
    "swap_devices": $(_json_str "$(swapon --show=NAME --noheadings 2>/dev/null | tr '\n' ' ' | sed 's/ $//')")
  },
  "storage": {
    "artifacts_fs": $(_json_str "$(df --output=fstype "$PWD/artifacts" 2>/dev/null | tail -1 | tr -d ' ')"),
    "artifacts_device": $(_json_str "$(lsblk -no PKNAME "$(df --output=source "$PWD/artifacts" 2>/dev/null | tail -1)" 2>/dev/null | head -1)")
  },
  "versions": {
    "nesbox": $(_json_str "$(git -C "$PWD" rev-parse --short HEAD 2>/dev/null)"),
    "nesbox_dirty": $([[ -n $(git -C "$PWD" status --porcelain 2>/dev/null) ]] && echo true || echo false),
    "virglrenderer": $(_json_str "$(git -C "${NESBOX_VIRGL_SRC:-$HOME/forks/virglrenderer}" rev-parse --short HEAD 2>/dev/null)"),
    "rustc": $(_json_str "$(rustc --version 2>/dev/null)")
  }
}
JSON
}

[[ ${BASH_SOURCE[0]} == "$0" ]] && bench_provenance_json
