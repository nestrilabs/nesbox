//! Host network prerequisites for guest egress, and checking they are there.
//!
//! A tap gets the guest as far as the host and no further. Reaching anything
//! beyond needs IP forwarding switched on and a masquerade rule for the guest's
//! subnet — host-global state that a running VM has no business installing, but
//! that has to come from somewhere.
//!
//! So it lives here, behind `nesbox setup`, run once by whoever installs
//! nesbox. The alternative was writing it down and hoping; a missing rule
//! surfaces as "the game never connects", which is several layers away from its
//! cause and lands on someone who did not write any of this.
//!
//! [`preflight`] is the other half: before the VM starts, say plainly which
//! piece is missing, so the failure names itself.

use crate::config::Network;
use anyhow::{Context, Result, bail};
use std::net::Ipv4Addr;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// The nftables table we own. Ours alone, so `nesbox setup` can be run again
/// without touching anyone else's rules.
const TABLE: &str = "nesbox";

/// The guest's subnet, derived from the host end of the tap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Subnet {
    pub network: Ipv4Addr,
    pub prefix_len: u32,
}

impl Subnet {
    pub fn from_network(net: &Network) -> Self {
        let ip = u32::from(net.host_ip);
        let mask = u32::from(net.netmask);
        Self {
            network: Ipv4Addr::from(ip & mask),
            prefix_len: mask.count_ones(),
        }
    }
}

impl std::fmt::Display for Subnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix_len)
    }
}

/// Something the host is missing before the guest can reach the outside world.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Problem {
    /// The kernel will not forward packets between interfaces at all.
    ForwardingDisabled,
    /// Forwarding is on, but nothing rewrites the guest's source address, so
    /// replies have nowhere to come back to.
    NoMasquerade(Subnet),
    /// We could not tell either way.
    CannotTell(String),
}

impl Problem {
    /// What a person should do about it.
    pub fn remedy(&self) -> String {
        match self {
            Self::ForwardingDisabled | Self::NoMasquerade(_) => {
                "run `nesbox setup <config.json>` once, as root".to_string()
            }
            Self::CannotTell(_) => {
                "check by hand: `sysctl net.ipv4.ip_forward` and `nft list table ip nesbox`"
                    .to_string()
            }
        }
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ForwardingDisabled => write!(
                f,
                "IP forwarding is disabled, so nothing leaves the guest's subnet"
            ),
            Self::NoMasquerade(subnet) => write!(
                f,
                "no masquerade rule for {subnet}, so replies to the guest have no route back"
            ),
            Self::CannotTell(why) => {
                write!(f, "could not check the host's egress rules: {why}")
            }
        }
    }
}

const IP_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";

fn forwarding_enabled() -> Result<bool> {
    let raw = std::fs::read_to_string(IP_FORWARD)
        .with_context(|| format!("failed to read {IP_FORWARD}"))?;
    Ok(raw.trim() == "1")
}

/// Run `nft`, lending it the network capability we already hold.
///
/// File capabilities do not survive `exec`: nesbox carries `CAP_NET_ADMIN` so
/// it can create a tap, but a plain `Command::spawn` hands `nft` a process with
/// none, and reading the nftables ruleset needs it. Without this the check
/// degrades to "could not tell", which is precisely the unhelpful answer this
/// module exists to avoid.
///
/// The capability is raised into the ambient set for the child only. It is the
/// same one we already have, and `nft` is doing read-only work with it.
fn nft(args: &[&str]) -> Result<std::process::Output> {
    let mut command = Command::new("nft");
    command.args(args);
    // SAFETY: pre_exec runs between fork and exec, where only async-signal-safe
    // work is allowed. prctl is a bare syscall and allocates nothing.
    unsafe {
        command.pre_exec(|| {
            lend_net_admin_to_child();
            Ok(())
        });
    }
    command
        .output()
        .context("failed to run `nft` — is nftables installed?")
}

/// Move `CAP_NET_ADMIN` into the inheritable and ambient sets so it survives
/// the coming `exec`.
///
/// Two steps, because a capability can only be raised into the ambient set if
/// it is already both permitted and inheritable, and file capabilities grant
/// permitted and effective but never inheritable.
///
/// Silent on failure: if we do not hold the capability there is nothing to
/// lend, and the caller reports the resulting permission error with far more
/// context than we could from inside a `pre_exec`.
fn lend_net_admin_to_child() {
    const CAP_NET_ADMIN: u32 = 12;
    const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: libc::c_int,
    }
    // Version 3 splits each set across two 32-bit words.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    let mut header = CapHeader { version: CAPABILITY_VERSION_3, pid: 0 };
    let mut data = [CapData::default(); 2];

    // SAFETY: capget fills exactly the two-element array version 3 expects, and
    // both pointers are to live stack storage. It is a bare syscall, so it is
    // safe to call between fork and exec.
    if unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) } != 0 {
        return;
    }

    // CAP_NET_ADMIN is 12, so it lives in the low word.
    let bit = 1u32 << CAP_NET_ADMIN;
    if data[0].permitted & bit == 0 {
        return; // nothing to lend
    }
    data[0].inheritable |= bit;

    // SAFETY: as above; the array is unchanged in shape.
    if unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } != 0 {
        return;
    }

    // SAFETY: a plain prctl with scalar arguments.
    unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_RAISE as libc::c_ulong,
            CAP_NET_ADMIN as libc::c_ulong,
            0,
            0,
        );
    }
}

/// Is there a masquerade rule covering this subnet, in our table or anyone's?
///
/// The whole ruleset is checked, not just ours: a host that already masquerades
/// this range through libvirt, Docker or a hand-written rule needs nothing from
/// us, and telling it otherwise would be wrong.
fn masquerade_present(subnet: Subnet) -> Result<bool> {
    let out = nft(&["list", "ruleset"])?;
    if !out.status.success() {
        bail!(
            "nft list ruleset failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let ruleset = String::from_utf8_lossy(&out.stdout);
    let subnet = subnet.to_string();
    Ok(ruleset
        .lines()
        .any(|line| line.contains("masquerade") && line.contains(&subnet)))
}

/// Check the host can actually carry the guest's traffic outward.
///
/// Returns what is missing. An empty list means egress should work; it does not
/// prove the guest can reach the internet, only that nothing we know how to
/// check is in the way.
pub fn preflight(network: &Network) -> Vec<Problem> {
    let subnet = Subnet::from_network(network);
    let mut problems = Vec::new();

    match forwarding_enabled() {
        Ok(true) => {}
        Ok(false) => problems.push(Problem::ForwardingDisabled),
        Err(err) => problems.push(Problem::CannotTell(format!("{err:#}"))),
    }

    match masquerade_present(subnet) {
        Ok(true) => {}
        Ok(false) => problems.push(Problem::NoMasquerade(subnet)),
        Err(err) => problems.push(Problem::CannotTell(format!("{err:#}"))),
    }

    problems
}

/// Report the result of [`preflight`] to the log.
///
/// Deliberately not fatal. A guest with no need to reach the internet is a
/// perfectly good guest, and refusing to boot over it would be worse than the
/// silence we are trying to fix.
pub fn report(network: &Network) {
    let problems = preflight(network);
    if problems.is_empty() {
        log::debug!(
            "host egress looks ready for {}",
            Subnet::from_network(network)
        );
        return;
    }
    for problem in &problems {
        log::warn!("guest egress will not work: {problem} — {}", problem.remedy());
    }
}

/// Install the host-side rules. Idempotent: running it again changes nothing.
pub fn install(network: &Network) -> Result<()> {
    let subnet = Subnet::from_network(network);

    if forwarding_enabled()? {
        log::info!("IP forwarding already enabled");
    } else {
        std::fs::write(IP_FORWARD, "1\n").with_context(|| {
            format!("failed to enable IP forwarding — {IP_FORWARD} needs root")
        })?;
        log::info!("enabled IP forwarding");
        log::warn!(
            "this does not survive a reboot; to make it permanent add \
             `net.ipv4.ip_forward = 1` to /etc/sysctl.d/"
        );
    }

    if masquerade_present(subnet)? {
        log::info!("a masquerade rule for {subnet} is already present");
        return Ok(());
    }

    // `add table` and `add chain` are no-ops if they already exist, so the
    // only thing that needs guarding is the rule itself, done above.
    run_nft(&["add", "table", "ip", TABLE])?;
    run_nft(&[
        "add",
        "chain",
        "ip",
        TABLE,
        "postrouting",
        "{ type nat hook postrouting priority 100 ; }",
    ])?;
    // Matching on the destination rather than the tap's name keeps this
    // independent of which tap a given VM ends up with — the name contains a
    // kernel-assigned number that is not known until a VM starts.
    let subnet = subnet.to_string();
    run_nft(&[
        "add", "rule", "ip", TABLE, "postrouting", "ip", "saddr", &subnet, "ip", "daddr", "!=",
        &subnet, "masquerade",
    ])?;
    log::info!("added a masquerade rule for {subnet} in table ip {TABLE}");
    log::warn!(
        "nftables rules do not survive a reboot either; persist them with \
         `nft list table ip {TABLE}` into your distribution's nftables config"
    );
    Ok(())
}

fn run_nft(args: &[&str]) -> Result<()> {
    let out = nft(args)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("nft {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}

/// Undo what [`install`] added. Leaves IP forwarding alone, since we cannot
/// know whether anything else on the host now depends on it.
pub fn uninstall() -> Result<()> {
    let out = nft(&["delete", "table", "ip", TABLE])?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("No such file or directory") {
            log::info!("table ip {TABLE} was not present");
            return Ok(());
        }
        bail!("failed to delete table ip {TABLE}: {}", stderr.trim());
    }
    log::info!("removed table ip {TABLE}");
    log::warn!("IP forwarding left enabled; other things on this host may rely on it");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network(host_ip: [u8; 4], netmask: [u8; 4]) -> Network {
        Network {
            tap_name: "nesbox%d".into(),
            host_ip: Ipv4Addr::from(host_ip),
            netmask: Ipv4Addr::from(netmask),
            mac: None,
        }
    }

    #[test]
    fn the_subnet_comes_from_the_host_address_and_mask() {
        let s = Subnet::from_network(&network([172, 30, 0, 1], [255, 255, 255, 0]));
        assert_eq!(s.to_string(), "172.30.0.0/24");
    }

    #[test]
    fn a_narrower_mask_gives_a_narrower_subnet() {
        let s = Subnet::from_network(&network([10, 0, 5, 130], [255, 255, 255, 192]));
        assert_eq!(s.to_string(), "10.0.5.128/26");
    }

    #[test]
    fn every_problem_says_what_to_do_about_it() {
        let subnet = Subnet::from_network(&network([172, 30, 0, 1], [255, 255, 255, 0]));
        for problem in [
            Problem::ForwardingDisabled,
            Problem::NoMasquerade(subnet),
            Problem::CannotTell("nft is missing".into()),
        ] {
            assert!(!problem.to_string().is_empty());
            assert!(!problem.remedy().is_empty());
        }
    }

    #[test]
    fn the_two_fixable_problems_point_at_setup() {
        // The distinction nessh asked for: these are actionable, and must not
        // read like an ordinary connection failure.
        let subnet = Subnet::from_network(&network([172, 30, 0, 1], [255, 255, 255, 0]));
        assert!(Problem::ForwardingDisabled.remedy().contains("nesbox setup"));
        assert!(Problem::NoMasquerade(subnet).remedy().contains("nesbox setup"));
        assert!(
            Problem::NoMasquerade(subnet)
                .to_string()
                .contains("172.30.0.0/24"),
            "the subnet has to appear, or the message is not actionable"
        );
    }
}
