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
step "Bridge ${BRIDGE}"
if [[ -d "/sys/class/net/${BRIDGE}/bridge" ]]; then
    say "  already exists"
elif [[ -e "/sys/class/net/${BRIDGE}" ]]; then
    die "${BRIDGE} exists but is not a bridge"
else
    run ip link add "$BRIDGE" type bridge
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
    if ip -4 addr show dev "$BRIDGE" 2>/dev/null | grep -q "${HOST_ADDR%%/*}"; then
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
