#!/usr/bin/env bash
# Prepare a host's networking for nesbox guests.
#
# nesbox itself does none of this, on purpose. It is a VMM; installing firewall
# rules and rearranging interfaces is host administration, and a VMM that
# needed CAP_NET_ADMIN to run would put "net admin on a binary" in front of
# everyone who self-hosts. A tap that already exists and is owned by the user
# nesbox runs as needs no privilege at all to open -- see tun_not_capable() in
# the kernel's drivers/net/tun.c -- so this creates the taps up front and nesbox
# stays unprivileged.
#
# Idempotent: everything here checks before it acts, so re-running after a
# reboot or a config change is safe.
set -euo pipefail

BOLD=$'\e[1m'; DIM=$'\e[2m'; RESET=$'\e[0m'; WARN=$'\e[33m'

say()  { printf '%s\n' "$*"; }
step() { printf '\n%s==>%s %s\n' "$BOLD" "$RESET" "$*"; }
warn() { printf '%s[warn]%s %s\n' "$WARN" "$RESET" "$*" >&2; }
die()  { printf '[error] %s\n' "$*" >&2; exit 1; }

ask() {
    local prompt="$1" default="${2:-}" answer
    if [[ -n "$default" ]]; then
        read -rp "$prompt [$default] " answer
        printf '%s' "${answer:-$default}"
    else
        read -rp "$prompt " answer
        printf '%s' "$answer"
    fi
}

confirm() {
    local prompt="$1" default="${2:-N}" answer
    case "$default" in
        [Yy]*) read -rp "$prompt [Y/n] " answer; [[ ! "${answer:-y}" =~ ^[Nn] ]] ;;
        *)     read -rp "$prompt [y/N] " answer; [[   "${answer:-n}" =~ ^[Yy] ]] ;;
    esac
}

# Show every command before running it. This rearranges shared host state, and
# somebody sharing their gaming machine has a right to see what is about to
# happen to their network.
run() {
    printf '  %s%s%s\n' "$DIM" "$*" "$RESET"
    if [[ "${DRY_RUN:-}" == 1 ]]; then return 0; fi
    "$@"
}

need_root() {
    [[ ${EUID} -eq 0 ]] || die "needs root: re-run with sudo (nothing has changed yet)"
}

nm_active() { systemctl is-active --quiet NetworkManager 2>/dev/null; }

# ── Gather ───────────────────────────────────────────────
say "${BOLD}nesbox host network setup${RESET}"
say "Nothing changes until you are asked. Ctrl-C is safe at any point."

if [[ "${1:-}" == "--dry-run" ]]; then
    DRY_RUN=1
    say "${DIM}Dry run: commands are printed, not executed.${RESET}"
else
    need_root
fi

BRIDGE="$(ask 'Bridge name to put guests on?' 'br-nestri')"
TAP_COUNT="$(ask 'How many guests should this box be able to run at once?' '2')"
TAP_USER="$(ask 'Which user does nesbox run as?' "${SUDO_USER:-$USER}")"
id "$TAP_USER" >/dev/null 2>&1 || die "no such user: $TAP_USER"

# Two ways for guests to reach a network, and the choice changes everything
# after it.
#
#   bridged  the guests join a real network and get addresses on it. Needs a
#            network somebody set aside, so it is the better option where it
#            exists and impossible where it does not.
#   routed   the guests sit on a private subnet behind NAT. Works behind any
#            router, which is why it is the fallback rather than the goal.
say ""
say "How should guests reach the network?"
say "  ${BOLD}1${RESET}) bridged onto a VLAN — guests get real addresses (needs a trunked port)"
say "  ${BOLD}2${RESET}) routed behind NAT   — private subnet, works on any network"
MODE="$(ask 'Choice?' '2')"

case "$MODE" in
  1)
    if ! confirm 'Do you have VLAN(s) configured on the switch port for this host?'; then
        say ""
        warn "Bridged mode needs the switch port to carry the guests' VLAN tagged."
        warn "An access port hands this host one untagged network, and no VLAN"
        warn "sub-interface can be made from it. Configure the port as a trunk"
        warn "first, or choose routed instead."
        exit 1
    fi
    UPLINK="$(ask 'Which interface faces the switch?' "$(ip -o route get 1.1.1.1 2>/dev/null | grep -oP 'dev \K\S+' || echo eth0)")"
    VLAN_ID="$(ask 'Which VLAN id should the guests use?' '128')"
    VLAN_IF="${UPLINK}.${VLAN_ID}"
    ;;
  2)
    HOST_ADDR="$(ask "Address for the host end of ${BRIDGE}?" '172.30.0.1/24')"
    ;;
  *) die "pick 1 or 2" ;;
esac

# ── Bridge ───────────────────────────────────────────────
# Created with whatever manages this host, not with `ip link`.
#
# On a NetworkManager host the two are not interchangeable: a bridge made with
# `ip link` is a device NM does not own, and asking it to enslave anything to
# that device fails with "controller doesn't refer to any existing profile of
# type 'bridge'". It would also be undone the next time NM reconciles.
step "Bridge ${BRIDGE}"

nm_profile_exists() { nmcli -t -f NAME con show 2>/dev/null | grep -qx "$1"; }

if nm_active; then
    if [[ -e "/sys/class/net/${BRIDGE}" ]] && ! nm_profile_exists "$BRIDGE"; then
        warn "${BRIDGE} exists but NetworkManager does not manage it — probably"
        warn "made with \`ip link\`. NM cannot enslave anything to it in that state."
        if confirm "Remove it and recreate it through NetworkManager?" y; then
            run ip link del "$BRIDGE"
        else
            die "cannot continue: ${BRIDGE} is unmanaged"
        fi
    fi

    if nm_profile_exists "$BRIDGE"; then
        say "  profile already exists"
    else
        run nmcli con add type bridge con-name "$BRIDGE" ifname "$BRIDGE"
        # STP costs ~15s of forwarding delay before a guest can pass traffic,
        # and buys nothing here: the only things on this bridge are taps and one
        # uplink, so there is no loop to detect.
        run nmcli con modify "$BRIDGE" bridge.stp no
        if [[ "$MODE" == 1 ]]; then
            # Bridged: the host keeps its address on the untagged interface and
            # the bridge carries only guest traffic. Left at NM's default of
            # `auto` it would DHCP an address of its own on the guests' VLAN.
            run nmcli con modify "$BRIDGE" ipv4.method disabled ipv6.method ignore
        else
            run nmcli con modify "$BRIDGE" ipv4.method manual ipv4.addresses "$HOST_ADDR"
            run nmcli con modify "$BRIDGE" ipv6.method ignore
        fi
        run nmcli con up "$BRIDGE"
    fi
elif [[ -d "/sys/class/net/${BRIDGE}/bridge" ]]; then
    say "  already exists"
elif [[ -e "/sys/class/net/${BRIDGE}" ]]; then
    die "${BRIDGE} exists but is not a bridge"
else
    run ip link add "$BRIDGE" type bridge
    run ip link set "$BRIDGE" type bridge stp_state 0
    run ip link set "$BRIDGE" up
fi

# ── Uplink or address ────────────────────────────────────
if [[ "$MODE" == 1 ]]; then
    step "VLAN ${VLAN_ID} on ${UPLINK} into ${BRIDGE}"
    # A tagged sub-interface, never the physical interface. Bridging the
    # interface that carries the host's own address moves that address onto the
    # bridge, which ends any session running over it -- and this script is
    # usually run over SSH. A sub-interface leaves it alone entirely.
    if nm_active; then
        say "  NetworkManager is managing this host, so it has to do this --"
        say "  anything done with \`ip\` here is undone next time it reconciles:"
        say ""
        say "    nmcli con add type vlan con-name ${VLAN_IF} ifname ${VLAN_IF} \\"
        say "        dev ${UPLINK} id ${VLAN_ID} master ${BRIDGE} slave-type bridge"
        say "    nmcli con up ${VLAN_IF}"
        say ""
        say "  The host's own address stays on ${UPLINK}, so nothing reconnects."
        if confirm "Run those now?"; then
            run nmcli con add type vlan con-name "$VLAN_IF" ifname "$VLAN_IF" \
                dev "$UPLINK" id "$VLAN_ID" master "$BRIDGE" slave-type bridge
            run nmcli con up "$VLAN_IF"
        else
            warn "skipped — guests will reach each other and nothing else until it is done"
        fi
    else
        if [[ -e "/sys/class/net/${VLAN_IF}" ]]; then
            say "  ${VLAN_IF} already exists"
        else
            run ip link add link "$UPLINK" name "$VLAN_IF" type vlan id "$VLAN_ID"
        fi
        run ip link set "$VLAN_IF" master "$BRIDGE"
        run ip link set "$VLAN_IF" up
        warn "not persistent — add ${VLAN_IF} to whatever configures this host's interfaces"
    fi
else
    step "Address and NAT for ${BRIDGE}"
    if nm_active; then
        say "  ${HOST_ADDR} set on the bridge profile above"
    elif ip -4 addr show dev "$BRIDGE" 2>/dev/null | grep -q "${HOST_ADDR%%/*}"; then
        say "  ${HOST_ADDR} already set"
    else
        run ip addr add "$HOST_ADDR" dev "$BRIDGE"
    fi

    if [[ "$(cat /proc/sys/net/ipv4/ip_forward)" == 1 ]]; then
        say "  IP forwarding already on"
    elif confirm "Enable IP forwarding? (host-wide, not just nesbox)" y; then
        run sysctl -w net.ipv4.ip_forward=1
        warn "not persistent — put it in /etc/sysctl.d/ to survive a reboot"
    fi

    SUBNET="$(ip -4 route show dev "$BRIDGE" 2>/dev/null | grep -oP '^\S+/\d+' | head -1 || true)"
    SUBNET="${SUBNET:-$HOST_ADDR}"
    if nft list table ip nesbox >/dev/null 2>&1 &&
       nft list table ip nesbox | grep -q "$SUBNET"; then
        say "  masquerade for ${SUBNET} already present"
    elif confirm "Add a masquerade rule for ${SUBNET}?" y; then
        # Its own table, so re-running touches nobody else's rules and removal
        # is one command.
        run nft add table ip nesbox
        run nft add chain ip nesbox postrouting \
            '{ type nat hook postrouting priority srcnat; policy accept; }'
        run nft add rule ip nesbox postrouting ip saddr "$SUBNET" oifname != "$BRIDGE" masquerade
        warn "not persistent — nft rules are lost on reboot unless saved"
    fi
fi

# ── Taps ─────────────────────────────────────────────────
# The point of the whole script. A persistent tap owned by the user nesbox runs
# as can be opened without CAP_NET_ADMIN: the kernel only demands that
# capability when creating a device, or when the opener is not its owner.
step "Persistent taps owned by ${TAP_USER}"
for ((i = 0; i < TAP_COUNT; i++)); do
    tap="nesbox${i}"
    if [[ -e "/sys/class/net/${tap}" ]]; then
        say "  ${tap} already exists"
    else
        run ip tuntap add dev "$tap" mode tap user "$TAP_USER"
    fi
    # Re-enslaving an already-enslaved tap is a no-op, so this needs no check.
    run ip link set "$tap" master "$BRIDGE"
    run ip link set "$tap" up
done

# ── Persistence ──────────────────────────────────────────
# Taps are not persistent state anywhere: `ip tuntap` makes a device that lives
# until the next reboot, and no network manager has a concept of one to
# describe. So something has to remake them at boot.
#
# A NetworkManager dispatcher script, rather than a systemd unit. It fires when
# the bridge comes up, which is exactly the right moment and the right ordering
# for free -- and it is init-agnostic, so it keeps working on a host that is not
# running systemd at all.
step "Persisting the taps"
DISPATCH="/etc/NetworkManager/dispatcher.d/50-nesbox-taps"
if ! nm_active; then
    warn "no NetworkManager: recreate the taps at boot yourself, with"
    warn "  ip tuntap add dev nesboxN mode tap user ${TAP_USER}"
    warn "  ip link set nesboxN master ${BRIDGE}"
elif [[ -x "$DISPATCH" ]]; then
    say "  ${DISPATCH} already installed"
    say "  ${DIM}(delete it to stop the taps being recreated at boot)${RESET}"
elif confirm "Install a dispatcher script so the taps come back after a reboot?" y; then
    if [[ "${DRY_RUN:-}" != 1 ]]; then
        mkdir -p "$(dirname "$DISPATCH")"
        cat > "$DISPATCH" <<DISPATCHER
#!/bin/sh
# Recreate nesbox's taps when ${BRIDGE} comes up. Written by
# nestri-net-setup.sh; delete this file to stop it.
#
# Taps are not persistent devices and no network manager describes them, so they
# have to be remade. Hooked to the bridge coming up rather than to boot, because
# enslaving a tap to a bridge that does not exist yet fails.
[ "\$1" = "${BRIDGE}" ] || exit 0
[ "\$2" = "up" ] || exit 0

for i in \$(seq 0 $((TAP_COUNT - 1))); do
    tap="nesbox\$i"
    [ -e "/sys/class/net/\$tap" ] || ip tuntap add dev "\$tap" mode tap user ${TAP_USER}
    ip link set "\$tap" master ${BRIDGE}
    ip link set "\$tap" up
done
DISPATCHER
        chmod 755 "$DISPATCH"
    fi
    say "  installed ${DISPATCH}"
else
    warn "skipped — the taps are gone after a reboot and nesbox will not start"
fi

step "Done"
say "  bridge:  ${BRIDGE}"
say "  taps:    nesbox0..nesbox$((TAP_COUNT - 1)), owned by ${TAP_USER}"
if [[ "$MODE" == 1 ]]; then
    say "  uplink:  ${VLAN_IF} (VLAN ${VLAN_ID} on ${UPLINK})"
    say ""
    say "Give each guest an address on that VLAN. nessh does this; a hand-written"
    say "config needs it on the kernel command line:"
    say "  ${DIM}nestri.ip=<addr>/<prefix> nestri.gw=<gateway>${RESET}"
else
    say "  host:    ${HOST_ADDR} on ${BRIDGE}, masquerading"
fi
say ""
say "nesbox needs no capability now: name a tap in its config and it opens it."
say "If you granted one before, it is no longer needed:"
say "  ${DIM}sudo setcap -r <path to nesbox>${RESET}"
