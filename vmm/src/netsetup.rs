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
    /// Some other firewall drops forwarded packets by default. Docker does
    /// this, and so does ufw. We cannot fix it from our own table: in nftables
    /// a drop anywhere is final, so an accept of ours cannot rescue a packet
    /// another chain has already refused.
    ForwardingBlocked(Subnet),
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
            // Both directions, because replies arrive addressed to the
            // subnet once conntrack has undone the masquerade.
            Self::ForwardingBlocked(subnet) => format!(
                "allow the subnet through that firewall in both directions, e.g. for Docker \
                 `sudo iptables -I DOCKER-USER -s {subnet} -j ACCEPT` and \
                 `sudo iptables -I DOCKER-USER -d {subnet} -j ACCEPT`"
            ),
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
            Self::ForwardingBlocked(subnet) => write!(
                f,
                "another firewall drops forwarded packets by default, so traffic from \
                 {subnet} never leaves the host — nesbox cannot override this from its \
                 own table"
            ),
            Self::CannotTell(why) => {
                write!(f, "could not check the host's egress rules: {why}")
            }
        }
    }
}

/// Whether the person running `setup` has agreed to each change to their host.
///
/// These commands rewrite firewall rules and kernel settings that everything
/// else on the machine shares. Someone self-hosting a game server has every
/// right to see what is about to happen to their network before it does.
pub struct Consent {
    assume_yes: bool,
}

impl Consent {
    /// `assume_yes` comes from `--yes`, for install scripts that have already
    /// asked in their own words.
    pub fn new(assume_yes: bool) -> Self {
        Self { assume_yes }
    }

    /// Describe a change and the command that makes it, and wait for an answer.
    ///
    /// Refuses rather than assumes when there is nobody to ask: a setup step
    /// running unattended without `--yes` should stop, not quietly reconfigure
    /// the host.
    fn allow(&self, why: &str, command: &str) -> Result<bool> {
        if self.assume_yes {
            log::info!("{why}\n  {command}");
            return Ok(true);
        }
        // SAFETY: isatty only inspects the descriptor.
        if unsafe { libc::isatty(0) } != 1 {
            bail!(
                "{why}\n  {command}\n\
                 Refusing to change the host with nobody to ask — \
                 re-run with --yes to accept these changes non-interactively"
            );
        }

        eprintln!("\n{why}");
        eprintln!("  {command}");
        eprint!("Proceed? [Y/n] ");
        use std::io::Write;
        let _ = std::io::stderr().flush();

        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .context("failed to read your answer")?;
        let answer = answer.trim().to_lowercase();
        let agreed = answer.is_empty() || answer == "y" || answer == "yes";
        if !agreed {
            eprintln!("Skipped.");
        }
        Ok(agreed)
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
    privileged("nft", args)
}

/// As [`nft`], for the iptables side of the world. A host running Docker has
/// its forward rules there, and Docker documents `DOCKER-USER` as the chain to
/// put your own in, so that is where a fix has to go.
fn iptables(args: &[&str]) -> Result<std::process::Output> {
    privileged("iptables", args)
}

fn privileged(program: &str, args: &[&str]) -> Result<std::process::Output> {
    let mut command = Command::new(program);
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
        .with_context(|| format!("failed to run `{program}` — is it installed?"))
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
fn masquerade_present(ruleset: &str, subnet: Subnet) -> bool {
    let subnet = subnet.to_string();
    ruleset
        .lines()
        .any(|line| line.contains("masquerade") && line.contains(&subnet))
}

/// Does some other chain drop forwarded packets by default, with nothing
/// letting the guest's subnet back through?
///
/// Docker sets a dropping forward policy the moment it starts, and ufw ships
/// with one. Detecting it matters more than it looks: with forwarding on and a
/// masquerade rule in place, everything nesbox installs is correct and the
/// guest still reaches nothing, which is the hardest version of this failure to
/// place.
///
/// **Both directions have to be allowed.** An accept for the subnet as a source
/// passes the guest's outbound packets, and is not enough on its own: conntrack
/// has already reversed the masquerade by the time a reply reaches the forward
/// chain, so replies arrive addressed *to* the subnet and match nothing. That
/// was not a guess — a host with only the outbound rule dropped every reply.
///
/// Accepts are looked for anywhere in the ruleset rather than only inside the
/// dropping chain, because they usually live in a chain it jumps to, such as
/// Docker's `DOCKER-USER`.
fn forwarding_blocked(ruleset: &str, subnet: Subnet) -> bool {
    let drops = ruleset
        .lines()
        .any(|l| l.contains("hook forward") && l.contains("policy drop"));
    if !drops {
        return false;
    }
    let subnet = subnet.to_string();
    let accepts = |direction: &str| {
        let pattern = format!("{direction} {subnet}");
        ruleset
            .lines()
            .any(|l| l.contains(&pattern) && l.contains("accept"))
    };
    !(accepts("saddr") && accepts("daddr"))
}

/// Where a forward-accept rule can be put so the dropping chain sees it.
enum ForwardChain {
    /// Docker's chain for exactly this purpose. Preferred: it is documented,
    /// evaluated first, and survives Docker restarting.
    DockerUser,
    /// A plain iptables forward chain. Inserting at the top is heavier-handed
    /// but works where there is no better place.
    Forward,
    /// Something drops forwarded packets and we do not know where to put a
    /// rule that it would honour.
    Unknown,
}

fn forward_chain() -> ForwardChain {
    if iptables(&["-S", "DOCKER-USER"]).is_ok_and(|o| o.status.success()) {
        ForwardChain::DockerUser
    } else if iptables(&["-S", "FORWARD"]).is_ok_and(|o| o.status.success()) {
        ForwardChain::Forward
    } else {
        ForwardChain::Unknown
    }
}

impl ForwardChain {
    fn name(&self) -> Option<&'static str> {
        match self {
            Self::DockerUser => Some("DOCKER-USER"),
            Self::Forward => Some("FORWARD"),
            Self::Unknown => None,
        }
    }
}

/// Allow the subnet through an iptables chain, in one direction. Idempotent:
/// `-C` asks whether the rule is already there before adding it.
fn rule_present(chain: &str, direction: &str, subnet: &str) -> Result<bool> {
    let check = ["-C", chain, direction, subnet, "-j", "ACCEPT"];
    Ok(iptables(&check)?.status.success())
}

fn allow_through(chain: &str, direction: &str, subnet: &str) -> Result<bool> {
    let rule = [direction, subnet, "-j", "ACCEPT"];
    if rule_present(chain, direction, subnet)? {
        return Ok(false); // already allowed
    }
    let insert: Vec<&str> = std::iter::once("-I")
        .chain(std::iter::once(chain))
        .chain(rule.iter().copied())
        .collect();
    let out = iptables(&insert)?;
    if !out.status.success() {
        bail!(
            "iptables -I {chain} {direction} {subnet} -j ACCEPT failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(true)
}

fn read_ruleset() -> Result<String> {
    let out = nft(&["list", "ruleset"])?;
    if !out.status.success() {
        bail!(
            "nft list ruleset failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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

    match read_ruleset() {
        Ok(ruleset) => {
            if !masquerade_present(&ruleset, subnet) {
                problems.push(Problem::NoMasquerade(subnet));
            }
            if forwarding_blocked(&ruleset, subnet) {
                problems.push(Problem::ForwardingBlocked(subnet));
            }
        }
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
pub fn install(network: &Network, consent: &Consent) -> Result<()> {
    let subnet = Subnet::from_network(network);

    if forwarding_enabled()? {
        log::info!("IP forwarding already enabled");
    } else if consent.allow(
        "Packets have to be forwarded between interfaces before anything can leave \n\
         the guest's subnet. This affects the whole host, not just nesbox.",
        "sysctl -w net.ipv4.ip_forward=1",
    )? {
        std::fs::write(IP_FORWARD, "1\n").with_context(|| {
            format!("failed to enable IP forwarding — {IP_FORWARD} needs root")
        })?;
        log::info!("enabled IP forwarding");
        log::warn!(
            "this does not survive a reboot; to make it permanent add \
             `net.ipv4.ip_forward = 1` to /etc/sysctl.d/"
        );
    }

    if masquerade_present(&read_ruleset()?, subnet) {
        log::info!("a masquerade rule for {subnet} is already present");
        return finish(network, subnet, consent);
    }

    if !consent.allow(
        &format!(
            "The guest's addresses mean nothing outside this host, so its traffic has to\n\
             leave wearing the host's address instead. This adds an nftables table of\n\
             nesbox's own and touches no existing rules."
        ),
        &format!(
            "nft add table ip {TABLE} && \\\n  \
             nft add chain ip {TABLE} postrouting '{{ type nat hook postrouting priority 100 ; }}' && \\\n  \
             nft add rule ip {TABLE} postrouting ip saddr {subnet} ip daddr != {subnet} masquerade"
        ),
    )? {
        return finish(network, subnet, consent);
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
    let text = subnet.to_string();
    run_nft(&[
        "add", "rule", "ip", TABLE, "postrouting", "ip", "saddr", &text, "ip", "daddr", "!=",
        &text, "masquerade",
    ])?;
    log::info!("added a masquerade rule for {text} in table ip {TABLE}");
    log::warn!(
        "nftables rules do not survive a reboot either; persist them with \
         `nft list table ip {TABLE}` into your distribution's nftables config"
    );
    finish(network, subnet, consent)
}

/// Clear anything else standing between the guest and the network, then say
/// whether it worked.
///
/// A self-hoster should not have to know that Docker quietly set the forward
/// policy to drop, nor that replies need allowing separately from requests. If
/// we have the privileges to fix it, fix it.
fn finish(network: &Network, subnet: Subnet, consent: &Consent) -> Result<()> {
    let ruleset = read_ruleset()?;
    if !forwarding_blocked(&ruleset, subnet) {
        log::info!("nothing else is blocking forwarded traffic");
        return report_final(network);
    }

    let chain = forward_chain();
    let Some(chain) = chain.name() else {
        log::error!(
            "something on this host drops forwarded packets and nesbox cannot tell \
             where to add a rule it would honour. Allow {subnet} through it by hand, \
             in both directions."
        );
        return report_final(network);
    };

    log::info!("a firewall drops forwarded packets; allowing {subnet} through {chain}");
    let subnet = subnet.to_string();
    // Both directions: requests leave with the subnet as source, and replies
    // come back to it once conntrack has undone the masquerade.
    let already: Vec<&str> = ["-s", "-d"]
        .into_iter()
        .filter(|d| rule_present(chain, d, &subnet).unwrap_or(false))
        .collect();
    for direction in ["-s", "-d"] {
        if already.contains(&direction) {
            log::info!("{chain} {direction} {subnet} -j ACCEPT was already there");
        }
    }
    let missing: Vec<&str> = ["-s", "-d"]
        .into_iter()
        .filter(|d| !already.contains(d))
        .collect();
    if missing.is_empty() {
        return report_final(network);
    }
    let commands = missing
        .iter()
        .map(|d| format!("iptables -I {chain} {d} {subnet} -j ACCEPT"))
        .collect::<Vec<_>>()
        .join(" && \\\n  ");
    if !consent.allow(
        &format!(
            "A firewall on this host drops forwarded packets by default, which would\n\
             stop the guest reaching anything. This allows just {subnet} through it.\n\
             Both directions are needed: replies arrive addressed to the subnet, not from it."
        ),
        &commands,
    )? {
        return report_final(network);
    }
    for direction in missing {
        if allow_through(chain, direction, &subnet)? {
            log::info!("added {chain} {direction} {subnet} -j ACCEPT");
        }
    }
    log::warn!(
        "iptables rules do not survive a reboot; persist them alongside the rest, \
         and re-run `nesbox setup` if Docker is reinstalled"
    );
    report_final(network)
}

/// Say plainly whether the host is now ready, rather than leaving "setup
/// finished" to imply it.
fn report_final(network: &Network) -> Result<()> {
    let problems = preflight(network);
    if problems.is_empty() {
        log::info!(
            "host is ready: {} can reach the network",
            Subnet::from_network(network)
        );
        return Ok(());
    }
    for problem in &problems {
        log::error!("still not ready: {problem} — {}", problem.remedy());
    }
    bail!("setup could not make the guest's network work; see above")
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
pub fn uninstall(consent: &Consent) -> Result<()> {
    if !consent.allow(
        "This removes the nftables table nesbox added. Guests will no longer reach\n\
         the network. Forwarding and any firewall rules are left alone.",
        &format!("nft delete table ip {TABLE}"),
    )? {
        return Ok(());
    }
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
    fn yes_mode_agrees_without_asking() {
        // What an install script gets: the change is logged, not prompted.
        assert!(
            Consent::new(true)
                .allow("because", "some --command")
                .unwrap()
        );
    }

    #[test]
    fn without_a_terminal_and_without_yes_it_refuses() {
        // The test harness has no tty, which is the case being checked: a
        // setup step running unattended must stop rather than quietly
        // reconfigure someone's firewall.
        let err = Consent::new(false)
            .allow("because", "some --command")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--yes"), "the refusal must say how to proceed");
        assert!(err.contains("some --command"), "and what it wanted to run");
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

    /// What Docker leaves in the ruleset: a forward chain that drops by default.
    const DOCKER_LIKE: &str = r#"
table ip filter {
    chain FORWARD {
        type filter hook forward priority filter; policy drop;
        iifname "docker0" counter accept
    }
}
table ip nesbox {
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        ip saddr 172.30.0.0/24 ip daddr != 172.30.0.0/24 masquerade
    }
}
"#;

    /// A real host part-way fixed: outbound allowed, replies still dropped.
    /// This exact shape passed the first version of the check and dropped every
    /// reply packet, which is why the test exists.
    const DOCKER_LIKE_HALF_ALLOWED: &str = r#"
table ip filter {
    chain FORWARD {
        type filter hook forward priority filter; policy drop;
        counter jump DOCKER-USER
    }
    chain DOCKER-USER {
        ip saddr 172.30.0.0/24 counter accept
    }
}
"#;

    /// The same host once both directions are allowed. Verified working: the
    /// guest reached 1.1.1.1.
    const DOCKER_LIKE_ALLOWED: &str = r#"
table ip filter {
    chain FORWARD {
        type filter hook forward priority filter; policy drop;
        counter jump DOCKER-USER
    }
    chain DOCKER-USER {
        ip daddr 172.30.0.0/24 counter accept
        ip saddr 172.30.0.0/24 counter accept
    }
}
"#;

    /// A host with no firewall of its own.
    const OPEN: &str = r#"
table ip nesbox {
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        ip saddr 172.30.0.0/24 ip daddr != 172.30.0.0/24 masquerade
    }
}
"#;

    fn test_subnet() -> Subnet {
        Subnet::from_network(&network([172, 30, 0, 1], [255, 255, 255, 0]))
    }

    #[test]
    fn a_dropping_forward_chain_is_noticed() {
        // The case that actually bit: everything nesbox installs is correct and
        // the guest still cannot reach anything.
        assert!(forwarding_blocked(DOCKER_LIKE, test_subnet()));
    }

    #[test]
    fn allowing_both_directions_clears_it() {
        // Accepts live in a chain the forward chain jumps to, so the check
        // cannot only look inside the dropping chain.
        assert!(!forwarding_blocked(DOCKER_LIKE_ALLOWED, test_subnet()));
    }

    #[test]
    fn allowing_only_the_outbound_direction_is_still_blocked() {
        // Replies come back addressed *to* the subnet, because conntrack has
        // already undone the masquerade. Measured, not assumed: this host
        // dropped every reply until the second rule was added.
        assert!(forwarding_blocked(DOCKER_LIKE_HALF_ALLOWED, test_subnet()));
    }

    #[test]
    fn a_host_without_a_firewall_is_not_blocked() {
        assert!(!forwarding_blocked(OPEN, test_subnet()));
        assert!(!forwarding_blocked("", test_subnet()));
    }

    #[test]
    fn the_masquerade_rule_is_found_by_subnet() {
        assert!(masquerade_present(OPEN, test_subnet()));
        // A rule for somebody else's subnet must not count as ours.
        let other = Subnet::from_network(&network([10, 1, 2, 1], [255, 255, 255, 0]));
        assert!(!masquerade_present(OPEN, other));
    }

    #[test]
    fn a_blocked_forward_names_a_fix_mentioning_the_subnet() {
        let p = Problem::ForwardingBlocked(test_subnet());
        assert!(p.remedy().contains("172.30.0.0/24"));
        assert!(p.to_string().contains("172.30.0.0/24"));
    }

    #[test]
    fn every_problem_says_what_to_do_about_it() {
        let subnet = Subnet::from_network(&network([172, 30, 0, 1], [255, 255, 255, 0]));
        for problem in [
            Problem::ForwardingDisabled,
            Problem::NoMasquerade(subnet),
            Problem::ForwardingBlocked(subnet),
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
