//! What is actually confining this process, and the one thing it can do about it
//! by itself.
//!
//! seccomp bounds what a compromised device model may *call*. It does not bound
//! what it may *reach*: `openat` and `connect` are both on the policy, so the
//! process keeps its uid's whole filesystem and its network. Those are bounded
//! by things that live outside this program — a uid, a mount namespace, a
//! network namespace, a cgroup — and until now nesbox neither applied them nor
//! looked to see whether anyone else had.
//!
//! That gap had a specific cost. `virtio-devices/src/gpu/vram.rs` declines to
//! enforce a GTT limit on the grounds that "host system memory is bounded for the
//! whole VMM process by cgroups", and `docs/STATS.md` repeats it. Nothing in this
//! repository applies a cgroup. The claim was true only if whoever launched the
//! process remembered, and nothing checked, and nothing said.
//!
//! # Reporting, not assuming
//!
//! [`Report::gather`] answers "what is in effect right now" from `/proc` and
//! `/sys/fs/cgroup`, and [`Report::log`] says so at startup — including, loudly,
//! when a bound that other parts of this codebase assume is simply absent. A
//! supervisor that forgets a `MemoryMax=` now produces a line saying so on every
//! boot instead of a surprise at 3am.
//!
//! Reporting is the honest shape here rather than applying limits ourselves.
//! `docs/BENCHMARKS.md` §12 measured what cgroups do to a guest, and the answer
//! was that the supervisor is the right owner: a `cpu.max` holds, an `io.max`
//! holds only against cold host cache, and `memory.max` is not a bound on the
//! guest at all — it decides what happens when a guest exceeds the RAM it was
//! given, which on a host with swap is silent thrashing and on a host without is
//! an OOM kill of the VMM. Applying that from in here would put a second, worse
//! copy of a decision the supervisor already makes.
//!
//! # The one thing this can do unprivileged
//!
//! [`enter_network_namespace`] unshares a user namespace and a network namespace
//! together. That needs no privilege — the new user namespace grants
//! `CAP_SYS_ADMIN` over the network namespace created alongside it — and it takes
//! away every route out of this process while leaving the guest's own link
//! working, because the tap file descriptor was opened before the unshare and a
//! socket keeps the namespace it was created in.
//!
//! It is **off by default**, and the reasons are in `enter_network_namespace`.
//! Read them before turning it on: on a host whose render node is not
//! world-accessible it will stop the GPU from working.

use std::io;

use anyhow::{Context, Result};

// ── Reporting ────────────────────────────────────────────────────────────────

/// A cgroup limit, as the kernel reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Limit {
    /// A limit is set, verbatim from the file (`"2G"`, `"200000 100000"`).
    Set(String),
    /// The controller is there and unlimited — the kernel's literal `max`.
    Unlimited,
    /// The file is not there: controller not enabled for this cgroup.
    Absent,
}

impl Limit {
    /// The limit actually in force, which is not necessarily the one written in
    /// our own cgroup.
    ///
    /// cgroup v2 limits are **hierarchical**: a `MemoryMax=` on `user.slice`
    /// bounds every process beneath it, and our own `memory.max` still reads
    /// `max`. Reading only the leaf would report "nothing bounds this" to a box
    /// that is in fact bounded — a false alarm that teaches an operator to
    /// ignore the line, which is the one outcome worse than not printing it.
    ///
    /// So walk from our own cgroup up to the root and take the first ancestor
    /// that sets one. That answers "is anything bounding this, and where is it
    /// written", which is the question. It is deliberately not the *effective*
    /// limit — for memory that would be the minimum down the chain — because a
    /// nearer, looser limit under a tighter ancestor is a configuration worth
    /// looking at rather than quietly resolving.
    fn effective(cgroup: &str, file: &str) -> (Self, Option<String>) {
        let mut seen = Limit::Absent;
        for path in ancestors(cgroup) {
            let found = Self::read(&format!("/sys/fs/cgroup{path}"), file);
            if found.is_set() {
                let owner = if path.is_empty() { "/".into() } else { path };
                return (found, Some(owner));
            }
            // "the controller exists and is unlimited" beats "no controller
            // anywhere" as a summary, so remember the strongest thing seen.
            if matches!(found, Limit::Unlimited) {
                seen = Limit::Unlimited;
            }
        }
        (seen, None)
    }

    fn read(cgroup_dir: &str, file: &str) -> Self {
        match std::fs::read_to_string(format!("{cgroup_dir}/{file}")) {
            Ok(s) => {
                let s = s.trim();
                // Three different spellings of "no limit", and getting any of
                // them wrong reports a bound that is not there -- which is the
                // exact failure this module exists to stop.
                //
                //   memory.max, pids.max  `max`
                //   io.max                empty, when no device is limited
                //   cpu.max               `max 100000` -- quota first, then the
                //                         period, so the period is always a
                //                         number and the string is never just
                //                         `max`.
                //
                // So: unlimited when there is nothing, or when the *first* field
                // is `max`.
                let first = s.split_whitespace().next();
                match first {
                    None | Some("max") => Limit::Unlimited,
                    Some(_) => Limit::Set(s.to_string()),
                }
            }
            Err(_) => Limit::Absent,
        }
    }

    /// Is this a bound something could rely on?
    pub fn is_set(&self) -> bool {
        matches!(self, Limit::Set(_))
    }
}

impl std::fmt::Display for Limit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Limit::Set(v) => write!(f, "{v}"),
            Limit::Unlimited => write!(f, "unlimited"),
            Limit::Absent => write!(f, "no controller"),
        }
    }
}

/// What is confining this process.
#[derive(Debug, Clone)]
pub struct Report {
    pub uid: u32,
    pub gid: u32,
    /// The cgroup v2 path from `/proc/self/cgroup`, if there is one.
    pub cgroup: Option<String>,
    pub memory_max: Limit,
    /// Which cgroup actually sets `memory_max`, when one does. Not always our
    /// own: a limit on an ancestor bounds us just as well, and knowing where it
    /// is written is the difference between an actionable line and a riddle.
    pub memory_bound_by: Option<String>,
    pub cpu_max: Limit,
    pub cpu_bound_by: Option<String>,
    pub io_max: Limit,
    pub pids_max: Limit,
    /// Inode of `/proc/self/ns/net`, and whether it differs from pid 1's.
    pub net_ns: Option<u64>,
    /// `None` when pid 1's namespace could not be read, which is ordinary for
    /// an unprivileged process — reported as unknown rather than as "shared".
    pub net_ns_is_own: Option<bool>,
}

impl Report {
    pub fn gather() -> Self {
        // SAFETY: both are infallible and take no arguments.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        let cgroup = cgroup_path();
        // Hierarchical: a limit on an ancestor bounds us even though our own
        // cgroup reads `max`. `bound_by` records which cgroup actually sets it.
        let look = |file: &str| match cgroup.as_deref() {
            Some(c) => Limit::effective(c, file),
            None => (Limit::Absent, None),
        };
        let (memory_max, memory_bound_by) = look("memory.max");
        let (cpu_max, cpu_bound_by) = look("cpu.max");
        let (io_max, _) = look("io.max");
        let (pids_max, _) = look("pids.max");
        let net_ns = ns_inode("/proc/self/ns/net");
        Self {
            uid,
            gid,
            cgroup,
            memory_max,
            memory_bound_by,
            cpu_max,
            cpu_bound_by,
            io_max,
            pids_max,
            net_ns,
            net_ns_is_own: ns_inode("/proc/1/ns/net").map(|init| Some(init) != net_ns),
        }
    }

    /// Say what is in effect, and say plainly what is not.
    ///
    /// The warnings are the point. An operator reading this should be able to
    /// tell, without reading any source, whether the box they just started is
    /// bounded — and the codebase's own assumptions are named where they fail,
    /// so the line is actionable rather than merely true.
    pub fn log(&self) {
        log::info!(
            "isolation: uid={} gid={} cgroup={} memory.max={} cpu.max={} io.max={} pids.max={}",
            self.uid,
            self.gid,
            self.cgroup.as_deref().unwrap_or("none"),
            self.memory_max,
            self.cpu_max,
            self.io_max,
            self.pids_max,
        );

        if self.uid == 0 {
            log::warn!(
                "isolation: running as root. Nothing here needs it -- /dev/kvm and the \
                 render node are opened by permission, and the tap is opened, not created."
            );
        }

        match (&self.memory_max, &self.memory_bound_by) {
            (Limit::Set(v), Some(where_)) => log::info!(
                "isolation: host memory bounded at {v} by {where_}"
            ),
            _ => log::warn!(
                "isolation: no memory.max anywhere above this process. GTT allocations are \
                 counted and never refused (virtio-devices/src/gpu/vram.rs) because host \
                 memory is supposed to be bounded here -- it is not, so nothing bounds them. \
                 Set one in the supervisor, or accept that a guest's GTT growth is unbounded."
            ),
        }
        match (&self.cpu_max, &self.cpu_bound_by) {
            (Limit::Set(v), Some(where_)) => {
                log::info!("isolation: host CPU bounded at \"{v}\" by {where_}")
            }
            _ => log::info!(
                "isolation: no cpu.max anywhere above this process; this guest can use as \
                 much host CPU as the scheduler gives it. cpu_affinity places threads, it \
                 does not cap them."
            ),
        }

        match self.net_ns_is_own {
            Some(true) => log::info!("isolation: in its own network namespace"),
            Some(false) => log::warn!(
                "isolation: sharing the host's network namespace. seccomp allows socket and \
                 connect, so a compromised device model has the host's network. See \
                 \"unshare-network\" in docs/SECURITY.md."
            ),
            None => log::debug!(
                "isolation: could not compare network namespaces (/proc/1/ns/net is not \
                 readable unprivileged)"
            ),
        }
    }
}

/// The cgroup v2 path, e.g. `/user.slice/user-1000.slice/session-2.scope`.
///
/// Only the v2 unified line (`0::`) is read. A v1 hierarchy would need a
/// controller-by-controller walk to say anything true, and reporting a v1 path
/// as though it were v2 would produce limits for the wrong cgroup.
fn cgroup_path() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    raw.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|p| p.trim().to_string())
}

/// Every cgroup from `cgroup` up to the root, nearest first.
///
/// `"/user.slice/user-1000.slice/session-2.scope"` yields that, then
/// `"/user.slice/user-1000.slice"`, then `"/user.slice"`, then `""` — the root,
/// which is `/sys/fs/cgroup` itself.
fn ancestors(cgroup: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = cgroup.trim_end_matches('/');
    loop {
        out.push(cur.to_string());
        match cur.rfind('/') {
            // A single leading slash: the parent is the root, spelled "".
            Some(0) => {
                if !cur.is_empty() {
                    out.push(String::new());
                }
                break;
            }
            Some(i) => cur = &cur[..i],
            None => break,
        }
    }
    out
}

fn ns_inode(path: &str) -> Option<u64> {
    // `/proc/.../ns/net` reads as `net:[4026531840]`.
    let link = std::fs::read_link(path).ok()?;
    let text = link.to_str()?;
    let start = text.find('[')? + 1;
    let end = text.find(']')?;
    text.get(start..end)?.parse().ok()
}

// ── Entering a network namespace ─────────────────────────────────────────────

const CLONE_NEWUSER: libc::c_int = 0x1000_0000;
const CLONE_NEWNET: libc::c_int = 0x4000_0000;

/// Step one of two: enter a private user namespace, very early.
///
/// This exists only to obtain `CAP_SYS_ADMIN` over namespaces we create later.
/// An unprivileged process cannot unshare a network namespace, but it can
/// unshare a *user* namespace, and it is fully privileged inside the one it
/// just made.
///
/// # Why this is split from the network unshare
///
/// **`unshare(CLONE_NEWUSER)` requires the process to be single-threaded.** Do it
/// at the natural place — beside the network unshare, once everything is set up —
/// and it fails with `EINVAL`, because by then the block and console devices have
/// each spawned a worker (`virtio-devices/src/blk.rs`, `console.rs`). Measured,
/// not guessed: the combined call returned `EINVAL` on a real boot.
///
/// `CLONE_NEWNET` carries no such restriction, so the two halves go in the two
/// places each of them can work: the user namespace here, before any thread
/// exists, and the network namespace after the tap is open.
///
/// # What entering it costs
///
/// Every subsequent open resolves against the mapped credentials, and only this
/// uid and gid are mapped — supplementary groups do not survive. So this must run
/// before anything is opened *and* it changes whether those opens succeed: a
/// `/dev/kvm` at `0660 root:kvm`, or a render node at `0660 root:render`, is
/// reachable through group membership and stops being reachable here. Where they
/// are `0666` it costs nothing.
///
/// That is the whole reason `unshare-network` is off by default, and the reason
/// the config comment tells you to test it on the host it will run on.
pub fn enter_user_namespace() -> Result<()> {
    let threads = thread_count();
    anyhow::ensure!(
        threads == Some(1) || threads.is_none(),
        "unshare-network needs a private user namespace, and the kernel only allows a \
         single-threaded process to create one -- this process already has {} threads. \
         enter_user_namespace() must be called before anything spawns one.",
        threads.unwrap_or(0)
    );

    // SAFETY: both are infallible and take no arguments.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };

    // SAFETY: FFI call with a constant flag and no pointer arguments.
    if unsafe { libc::unshare(CLONE_NEWUSER) } != 0 {
        let err = io::Error::last_os_error();
        return Err(anyhow::Error::new(err).context(
            "could not unshare a user namespace. Unprivileged user namespaces may be \
             disabled on this host (check /proc/sys/user/max_user_namespaces)",
        ));
    }

    // `setgroups` must be denied before `gid_map` can be written by an
    // unprivileged process. This is the kernel making us give up the ability to
    // drop supplementary groups -- and is also why group-based access does not
    // survive, per the note above.
    write_map("/proc/self/setgroups", "deny").context("denying setgroups")?;
    write_map("/proc/self/uid_map", &format!("{uid} {uid} 1")).context("writing uid_map")?;
    write_map("/proc/self/gid_map", &format!("{gid} {gid} 1")).context("writing gid_map")?;
    Ok(())
}

/// Step two of two: drop this process into its own network namespace.
///
/// After this the process has no network interfaces at all beyond a `lo` that is
/// left down, so `connect` still exists as a syscall and reaches nothing. Egress
/// stops being a policy decision.
///
/// Requires [`enter_user_namespace`] to have run, which is where the privilege
/// for this comes from.
///
/// # Why the guest keeps its network
///
/// The tap file descriptor is opened before this runs, and an open fd keeps
/// working regardless of the namespace its holder later moves to — the device
/// stays where it was created, on the host's bridge. The same reasoning covers
/// the metrics socket: a socket belongs to the namespace it was *created* in, so
/// a listener bound before this call stays reachable from the host afterwards.
///
/// **Order therefore matters, and it is not checked by the type system.** This
/// must run after the tap is opened, after virtiofsd is spawned, and after the
/// stats socket is bound. In `main` it sits immediately before the seccomp
/// install, which is the same "everything is set up now" point.
///
/// `vsock` is the caveat: the kernel's vsock has namespace support, and whether a
/// guest CID registered beforehand survives has not been measured here. `main`
/// warns when both are configured.
pub fn enter_network_namespace() -> Result<()> {
    // SAFETY: FFI call with a constant flag and no pointer arguments.
    if unsafe { libc::unshare(CLONE_NEWNET) } != 0 {
        let err = io::Error::last_os_error();
        return Err(anyhow::Error::new(err).context(
            "could not unshare a network namespace, despite holding a private user \
             namespace. This should not happen; report it with the errno",
        ));
    }
    log::info!(
        "isolation: entered a private network namespace; this process has no route off \
         the host. The guest's tap was opened before this and is unaffected."
    );
    Ok(())
}

/// Threads in this process, from `/proc/self/status`.
///
/// `None` when `/proc` will not answer, which is treated as "do not block on
/// this" — the `unshare` below reports the real answer either way, and refusing
/// to start over an unreadable `/proc` would be the wrong trade.
fn thread_count() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Threads:"))?
        .trim()
        .parse()
        .ok()
}

fn write_map(path: &str, contents: &str) -> io::Result<()> {
    std::fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_can_always_be_gathered() {
        // It reads /proc and /sys and must never fail: a box that will not start
        // because it could not describe its own confinement is a worse outcome
        // than one that says "unknown".
        let r = Report::gather();
        assert_eq!(r.uid, unsafe { libc::geteuid() });
        r.log();
    }

    /// On any cgroup v2 host this must find the path. If it silently returns
    /// None the limits all read as `Absent` and the warnings say the opposite of
    /// the truth -- which is worse than not reporting at all.
    #[test]
    fn the_cgroup_path_parses_on_a_v2_host() {
        let raw = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        if raw.contains("0::") {
            let p = cgroup_path().expect("a 0:: line must yield a path");
            assert!(p.starts_with('/'), "{p:?}");
        }
    }

    /// The walk that makes a hierarchical limit visible. Getting this wrong
    /// means reporting "nothing bounds this" to a bounded box.
    #[test]
    fn ancestors_walk_from_the_leaf_to_the_root() {
        assert_eq!(
            ancestors("/user.slice/user-1000.slice/session-2.scope"),
            vec![
                "/user.slice/user-1000.slice/session-2.scope",
                "/user.slice/user-1000.slice",
                "/user.slice",
                "",
            ]
        );
        // A process directly in the root cgroup, and the root itself.
        assert_eq!(ancestors("/foo"), vec!["/foo", ""]);
        assert_eq!(ancestors("/"), vec![""]);
        assert_eq!(ancestors(""), vec![""]);
    }

    /// A limit on an ancestor bounds us, and the report has to say where it is
    /// written -- "you are unbounded" to a bounded box is a false alarm, and
    /// false alarms are how a real one gets ignored.
    #[test]
    fn a_limit_on_an_ancestor_is_found_and_attributed() {
        // Exercised through `ancestors` plus `read`, since `effective` is
        // hard-wired to /sys/fs/cgroup. The composition is what could break.
        let dir = std::env::temp_dir().join(format!("nesbox-hier-{}", std::process::id()));
        let leaf = dir.join("user.slice/session.scope");
        std::fs::create_dir_all(&leaf).unwrap();
        // Limit written on the parent, not the leaf.
        std::fs::write(dir.join("user.slice/memory.max"), "2147483648\n").unwrap();
        std::fs::write(leaf.join("memory.max"), "max\n").unwrap();

        assert_eq!(
            Limit::read(leaf.to_str().unwrap(), "memory.max"),
            Limit::Unlimited,
            "the leaf really does read as unlimited -- this is the trap"
        );
        assert_eq!(
            Limit::read(dir.join("user.slice").to_str().unwrap(), "memory.max"),
            Limit::Set("2147483648".into()),
            "and the parent is where the bound actually is"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_namespace_inode_parses() {
        let net = ns_inode("/proc/self/ns/net").expect("every process has a network namespace");
        assert!(net > 0);
        // And a path that is not a namespace link yields nothing rather than a
        // wrong number.
        assert!(ns_inode("/proc/self/cmdline").is_none());
    }

    #[test]
    fn limits_distinguish_unset_from_absent() {
        let dir = std::env::temp_dir().join(format!("nesbox-cg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();

        std::fs::write(dir.join("memory.max"), "max\n").unwrap();
        assert_eq!(Limit::read(d, "memory.max"), Limit::Unlimited);

        std::fs::write(dir.join("memory.max"), "2147483648\n").unwrap();
        assert_eq!(
            Limit::read(d, "memory.max"),
            Limit::Set("2147483648".into())
        );
        assert!(Limit::read(d, "memory.max").is_set());

        // io.max says nothing at all when no device is limited.
        std::fs::write(dir.join("io.max"), "").unwrap();
        assert_eq!(Limit::read(d, "io.max"), Limit::Unlimited);

        // cpu.max is `quota period`, and an unlimited quota is still followed by
        // a period -- so it never reads as bare `max`. Treating it as a set
        // limit reports a CPU bound that does not exist, which is worse than
        // saying nothing. Caught on a real host, where an unlimited cgroup
        // reported `cpu.max=max 100000`.
        std::fs::write(dir.join("cpu.max"), "max 100000\n").unwrap();
        assert_eq!(Limit::read(d, "cpu.max"), Limit::Unlimited);
        assert!(!Limit::read(d, "cpu.max").is_set());

        // And a real quota is a limit.
        std::fs::write(dir.join("cpu.max"), "200000 100000\n").unwrap();
        assert_eq!(Limit::read(d, "cpu.max"), Limit::Set("200000 100000".into()));
        assert!(Limit::read(d, "cpu.max").is_set());
        std::fs::remove_file(dir.join("cpu.max")).unwrap();

        // A controller that is not enabled is not the same as one that is
        // enabled and unlimited, and the warnings depend on the difference.
        assert_eq!(Limit::read(d, "cpu.max"), Limit::Absent);
        assert!(!Limit::Absent.is_set());
        assert!(!Limit::Unlimited.is_set());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The namespace really is private, and the process really does lose the
    /// network. Runs in a forked child so it cannot confine the test process.
    #[test]
    fn entering_a_namespace_removes_the_network() {
        let before = ns_inode("/proc/self/ns/net").unwrap();
        // SAFETY: the child only calls async-signal-safe functions and exits.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            let code = match enter_user_namespace().and_then(|()| enter_network_namespace()) {
                Ok(()) => {
                    let after = ns_inode("/proc/self/ns/net").unwrap_or(0);
                    // A different namespace, and no route out of it. Connecting
                    // to a routable address must fail outright rather than hang.
                    let reachable = std::net::TcpStream::connect_timeout(
                        &"1.1.1.1:53".parse().unwrap(),
                        std::time::Duration::from_millis(300),
                    )
                    .is_ok();
                    if after == before {
                        3
                    } else if reachable {
                        4
                    } else {
                        0
                    }
                }
                // Unprivileged user namespaces can be disabled host-wide. That
                // is a fact about the host, not a failure of this code, so it is
                // reported as a skip.
                Err(_) => 60,
            };
            // SAFETY: exiting without unwinding.
            unsafe { libc::syscall(libc::SYS_exit_group, code) };
            unreachable!();
        }
        let mut status = 0;
        // SAFETY: waiting on our own child.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(libc::WIFEXITED(status), "child died, status {status:#x}");
        match libc::WEXITSTATUS(status) {
            0 => {}
            60 => eprintln!("skipped: unprivileged user namespaces unavailable on this host"),
            3 => panic!("the network namespace did not change"),
            4 => panic!("still had a route out after unsharing"),
            n => panic!("unexpected exit {n}"),
        }
    }
}
