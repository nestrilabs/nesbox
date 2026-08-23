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
    # Quoted when it needs to be, because what this prints is meant to be
    # pasteable. `$*` turned `nmcli con modify "Wired connection 1" ...` into
    # four bare words -- a line that ran correctly here and fails for anyone who
    # copies it, which is the worst of both.
    local shown="" arg
    for arg in "$@"; do
        if [[ "$arg" =~ ^[A-Za-z0-9_./:=@%^+-]+$ ]]; then
            shown+="${arg} "
        else
            shown+="'${arg//\'/\'\\\'\'}' "
        fi
    done
    printf '  %s%s%s\n' "$DIM" "${shown% }" "$RESET"
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

# Where interface state is read from. A variable so the checks that read it can
# be exercised against a fabricated tree -- the loop check below is the kind of
# thing that must be right the first time, and testing it for real means
# building a loop on a live network.
SYSFS_NET="${SYSFS_NET:-/sys/class/net}"

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
say "  ${BOLD}1${RESET}) bridged onto a network — guests get real addresses on it"
say "  ${BOLD}2${RESET}) routed behind NAT     — private subnet, works on any network"
MODE="$(ask 'Choice?' '2')"

case "$MODE" in
  1)
    UPLINK="$(ask 'Which interface faces the switch?' "$(ip -o route get 1.1.1.1 2>/dev/null | grep -oP 'dev \K\S+' || echo eth0)")"
    # Checked here rather than discovered three questions later. Every lookup
    # below reads this device, and under `set -euo pipefail` a missing one ends
    # the script with no message at all.
    [[ -e "${SYSFS_NET}/${UPLINK}" ]] || die "no interface named ${UPLINK} on this host"
    # The one question that decides the whole shape, and the one people get
    # wrong, because both halves are individually correct and incompatible.
    #
    # A switch port presents each VLAN either tagged or untagged, not both. If
    # the guests' VLAN is the port's native/untagged network, frames from a
    # tagged sub-interface are dropped at the switch with no trace on the host,
    # and the guest reports "Destination Host Unreachable" about its own
    # address -- an ARP failure for its gateway. If the VLAN is tagged, a
    # sub-interface is the only thing that reaches it.
    #
    # Which one a port uses is somebody's deliberate choice, usually about
    # where the *host* lives: making the guests' VLAN native is what keeps the
    # host off the default LAN, because a port's untagged traffic falls back to
    # VLAN 1 the moment nothing else claims it.
    say ""
    say "How does the switch port present the guests' network?"
    say "  ${BOLD}1${RESET}) tagged   — the host is on some other untagged network"
    say "  ${BOLD}2${RESET}) untagged — it is this port's native network (the host is on it too)"
    TAGGING="$(ask 'Choice?' '1')"
    case "$TAGGING" in
      1)
        VLAN_ID="$(ask 'Which VLAN id should the guests use?' '128')"
        VLAN_IF="${UPLINK}.${VLAN_ID}"
        ;;
      2)
        # No sub-interface: the physical interface itself joins the bridge. That
        # moves the host's own address onto the bridge, which ends any session
        # running over it -- see the warnings below, which are not decoration.
        VLAN_IF=""
        UPLINK_SLAVE="${BRIDGE}-uplink"
        ;;
      *) die "pick 1 or 2" ;;
    esac
    ;;
  2)
    HOST_ADDR="$(ask "Address for the host end of ${BRIDGE}?" '172.30.0.1/24')"
    ;;
  *) die "pick 1 or 2" ;;
esac

# ── The host's own addressing, in the untagged case ───────
# Read before anything changes, because it is about to move to the bridge and
# there is no second chance to look it up once the interface is enslaved. A box
# that comes back up without a default route is a box somebody drives to.
if [[ -n "${UPLINK_SLAVE:-}" ]]; then
    HOST_PROFILE="$(nmcli -g GENERAL.CONNECTION device show "$UPLINK" 2>/dev/null || true)"
    HOST_METHOD="$(nmcli -g ipv4.method con show "$HOST_PROFILE" 2>/dev/null || echo auto)"
    # `|| true` on each: an interface with no address, no default route or no
    # DNS is a fact to report, not a reason to abort. pipefail would otherwise
    # turn each empty answer into an exit.
    HOST_ADDR="$(ip -4 -o addr show dev "$UPLINK" 2>/dev/null | awk '{print $4; exit}' || true)"
    HOST_GW="$(ip -4 route show default dev "$UPLINK" 2>/dev/null | awk '{print $3; exit}' || true)"
    HOST_DNS="$(nmcli -g IP4.DNS device show "$UPLINK" 2>/dev/null | tr '\n' ',' | sed 's/,$//' || true)"
    say ""
    step "What ${UPLINK} holds today, and what ${BRIDGE} takes over"
    say "  profile:  ${HOST_PROFILE:-none}"
    say "  method:   ${HOST_METHOD:-auto}"
    say "  address:  ${HOST_ADDR:-none}"
    say "  gateway:  ${HOST_GW:-none}"
    say "  dns:      ${HOST_DNS:-none}"
    if [[ "$HOST_METHOD" == manual && -z "$HOST_ADDR" ]]; then
        die "${UPLINK} is configured static but has no address; fix that first"
    fi
    # ── The loop this script created once ────────────────
    # A bridge holding both an interface and a VLAN sub-interface *of that same
    # interface* is a loop, and a real one: a frame arriving on ${UPLINK} is
    # flooded out ${UPLINK}.<vlan>, which re-injects it onto ${UPLINK} tagged,
    # and back to the switch. STP is off on this bridge -- deliberately, for
    # the forwarding delay -- so nothing on the host catches it. The switch
    # does, by blocking the port, which takes the host off the network
    # entirely.
    #
    # This is exactly what a box switching from the tagged topology to this one
    # walks into, because the sub-interface from the old arrangement is still
    # enslaved. Checked rather than warned about: the failure costs a trip to
    # the machine.
    LOOPS=""
    if [[ -d "${SYSFS_NET}/${BRIDGE}/brif" ]]; then
        for port in "${SYSFS_NET}/${BRIDGE}/brif/"*; do
            [[ -e "$port" ]] || continue
            port="$(basename "$port")"
            # A VLAN sub-interface of this uplink, by the kernel's own record
            # of what it was made from -- not by the name, which is only a
            # convention.
            if [[ -e "${SYSFS_NET}/${port}/lower_${UPLINK}" ]]; then
                LOOPS="${LOOPS} ${port}"
            fi
        done
    fi
    if [[ -n "$LOOPS" ]]; then
        say ""
        warn "${BRIDGE} already carries${LOOPS} — a VLAN on ${UPLINK} itself."
        warn "Adding ${UPLINK} to the same bridge makes a loop: frames flooded"
        warn "out${LOOPS} come back in on ${UPLINK} tagged. STP is off here, so"
        warn "the switch is what notices, by blocking the port."
        say ""
        say "Remove the tagged uplink first, then run this again:"
        for loop in $LOOPS; do
            say "  ${DIM}nmcli con down ${loop} && nmcli con modify ${loop} connection.autoconnect no${RESET}"
        done
        die "stopped before making a loop"
    fi

    say ""
    warn "This moves the host's address off ${UPLINK} and onto ${BRIDGE}."
    warn "Any session running over ${UPLINK} -- including this SSH one -- drops"
    warn "while that happens. Run it from a console, or over a path that does"
    warn "not use ${UPLINK}: Tailscale, IPMI, a second NIC."
    if ! confirm "Understood, and you have another way in?"; then
        die "stopped before touching anything"
    fi
fi

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
    if [[ -e "${SYSFS_NET}/${BRIDGE}" ]] && ! nm_profile_exists "$BRIDGE"; then
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
        if [[ -n "${UPLINK_SLAVE:-}" ]]; then
            # The untagged case: the bridge inherits the host's own address,
            # because the interface holding it is about to become a bridge port
            # and a bridge port cannot hold an address. The same method the host
            # uses today, so the box keeps the address it already had.
            if [[ "$HOST_METHOD" == manual ]]; then
                run nmcli con modify "$BRIDGE" ipv4.method manual \
                    ipv4.addresses "$HOST_ADDR" ipv4.gateway "$HOST_GW" \
                    ipv4.dns "$HOST_DNS"
            else
                run nmcli con modify "$BRIDGE" ipv4.method auto
            fi
            run nmcli con modify "$BRIDGE" ipv6.method ignore
        elif [[ "$MODE" == 1 ]]; then
            # Bridged and tagged: the host keeps its address on the untagged
            # interface and the bridge carries only guest traffic. Left at NM's
            # default of `auto` it would DHCP an address on the guests' VLAN.
            run nmcli con modify "$BRIDGE" ipv4.method disabled ipv6.method ignore
        else
            run nmcli con modify "$BRIDGE" ipv4.method manual ipv4.addresses "$HOST_ADDR"
            run nmcli con modify "$BRIDGE" ipv6.method ignore
        fi
        run nmcli con up "$BRIDGE"
    fi
elif [[ -d "${SYSFS_NET}/${BRIDGE}/bridge" ]]; then
    say "  already exists"
elif [[ -e "${SYSFS_NET}/${BRIDGE}" ]]; then
    die "${BRIDGE} exists but is not a bridge"
else
    run ip link add "$BRIDGE" type bridge
    run ip link set "$BRIDGE" type bridge stp_state 0
    run ip link set "$BRIDGE" up
fi

# ── Uplink or address ────────────────────────────────────
if [[ -n "${UPLINK_SLAVE:-}" ]]; then
    step "${UPLINK} itself into ${BRIDGE}"
    # No sub-interface. The guests' network is this port's untagged one, so
    # guests must speak untagged too, which means the physical interface joins
    # the bridge and the host's address moves with it.
    #
    # Ordering matters and is not obvious: the old profile has to go down
    # before the slave comes up, or NM has two profiles claiming one device and
    # resolves it by flapping. Autoconnect is turned off rather than the
    # profile deleted, so there is something to put back by hand from a console
    # if this goes wrong.
    if nm_active; then
        say "  NetworkManager is managing this host, so it has to do this:"
        say ""
        say "    nmcli con add type ethernet con-name ${UPLINK_SLAVE} ifname ${UPLINK} \\"
        say "        master ${BRIDGE} slave-type bridge"
        say "    nmcli con modify '${HOST_PROFILE}' connection.autoconnect no"
        say "    nmcli con down '${HOST_PROFILE}'"
        say "    nmcli con up ${UPLINK_SLAVE}"
        say "    nmcli con up ${BRIDGE}"
        say ""
        say "  ${BOLD}The session running over ${UPLINK} drops at the third line.${RESET}"
        say "  To undo from a console: nmcli con modify '${HOST_PROFILE}' \\"
        say "      connection.autoconnect yes && nmcli con up '${HOST_PROFILE}'"
        if confirm "Run those now?"; then
            run nmcli con add type ethernet con-name "$UPLINK_SLAVE" ifname "$UPLINK" \
                master "$BRIDGE" slave-type bridge
            if [[ -n "$HOST_PROFILE" ]]; then
                run nmcli con modify "$HOST_PROFILE" connection.autoconnect no
                run nmcli con down "$HOST_PROFILE"
            fi
            run nmcli con up "$UPLINK_SLAVE"
            run nmcli con up "$BRIDGE"
        else
            warn "skipped — guests reach each other and nothing else until it is done"
        fi
    else
        run ip link set "$UPLINK" master "$BRIDGE"
        run ip link set "$UPLINK" up
        if [[ -n "$HOST_ADDR" ]]; then
            run ip addr del "$HOST_ADDR" dev "$UPLINK"
            run ip addr add "$HOST_ADDR" dev "$BRIDGE"
            [[ -n "$HOST_GW" ]] && run ip route add default via "$HOST_GW" dev "$BRIDGE"
        fi
        warn "not persistent — put ${UPLINK} and the address into whatever"
        warn "configures this host's interfaces, or the next boot undoes it"
    fi
elif [[ "$MODE" == 1 ]]; then
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
        if [[ -e "${SYSFS_NET}/${VLAN_IF}" ]]; then
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
    if [[ -e "${SYSFS_NET}/${tap}" ]]; then
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
    # The real path, not \${SYSFS_NET}: this runs at boot from NetworkManager,
    # where nothing sets that variable.
    [ -e "/sys/class/net/\$tap" ] || ip tuntap add dev "\$tap" mode tap user ${TAP_USER}
    ip link set "\$tap" master ${BRIDGE}
    ip link set "\$tap" up
done
DISPATCHER
        chmod 755 "$DISPATCH"
    fi
    # Says what actually happened. A dry run that reports "installed" is a dry
    # run somebody stops trusting.
    if [[ "${DRY_RUN:-}" == 1 ]]; then
        say "  would install ${DISPATCH}"
    else
        say "  installed ${DISPATCH}"
    fi
else
    warn "skipped — the taps are gone after a reboot and nesbox will not start"
fi

step "Done"
say "  bridge:  ${BRIDGE}"
say "  taps:    nesbox0..nesbox$((TAP_COUNT - 1)), owned by ${TAP_USER}"
if [[ -n "${UPLINK_SLAVE:-}" ]]; then
    say "  uplink:  ${UPLINK} (untagged, the port's native network)"
    say "  host:    ${HOST_ADDR:-from DHCP} on ${BRIDGE}"
    say ""
    say "The host and the guests are on the same untagged network now. Check that"
    say "the switch port carries nothing else: every VLAN trunked to it tagged is"
    say "one an escaped guest can reach by creating a sub-interface for the tag."
    say ""
    say "Give each guest an address on that network. nessh does this; a"
    say "hand-written config needs it on the kernel command line:"
    say "  ${DIM}nestri.ip=<addr>/<prefix> nestri.gw=<gateway>${RESET}"
elif [[ "$MODE" == 1 ]]; then
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
