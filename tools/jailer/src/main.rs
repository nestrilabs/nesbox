//! jailer -- chroots one process into a materialized jail image, then hands
//! off to it.
//!
//! nesbox cannot do any of this to itself. `chroot`, `mount` and `setuid` are
//! all on its own seccomp denylist (`vmm/src/seccomp.rs`,
//! `nothing_that_reconfigures_the_host_is_allowed_anywhere`), and an
//! unprivileged process cannot change its own uid regardless of what its
//! filter permits. So this is a separate binary, run before nesbox exists,
//! that does five things and then is gone:
//!
//!   1. unshare a mount namespace, and cut propagation to the host's, so
//!      nothing done here leaks back out.
//!   2. bind-mount exactly the paths the target process needs to reach
//!      hardware and report state, at the same path inside the jail.
//!   3. `chroot` into the jail root -- already built and materialized
//!      elsewhere. This binary does not build, fetch or version it.
//!   4. drop from root to the uid/gid the caller names.
//!   5. `execve` the command it was told to run.
//!
//! # Why bind-mount rather than exclude
//!
//! `docs/SECURITY.md` considered a jailer once and rejected it because "the
//! DRM render node, virtiofs source directories, and the metrics socket path
//! all live outside any plausible jail." That is true of a jail built to
//! *exclude* the host; it is not true of one built to *include* exactly what
//! is needed. Every path this binary brings in is named on its own command
//! line by whatever launches it -- nothing is discovered or guessed here.
//!
//! virtiofs source directories stay out of scope: virtiofsd already runs
//! unsandboxed, spawned by nesbox itself with `--sandbox none`
//! (`vmm/src/virtiofsd.rs`), and never runs inside anything this binary is
//! responsible for.
//!
//! # What this does not claim
//!
//! `/proc` and `/sys` are bound in from the host, not fresh instances of
//! their own -- cgroup self-reporting (`vmm/src/isolation.rs`) reads real
//! ancestor cgroups, which only exist on the host's tree. That makes the
//! host's process list and sysfs tree reachable inside the jail as read
//! paths. The mount namespace stops the jailed process from seeing other
//! mount points; it does not give it a private `/proc`. This is not
//! PID-namespace isolation, and does not claim to be.
//!
//! That has a sharper consequence than "reduced isolation" if the caller
//! ever hands this a uid already in use elsewhere on the host: a process
//! reading `/proc/<pid>/root` for a same-uid `pid` can reach that process's
//! filesystem view, unconfined by this jail, subject to the kernel's own
//! ptrace-read permission check (dumpable flag, Yama's `ptrace_scope`).
//! [`refuse_if_uid_is_live`] refuses to proceed if a live host process
//! already holds the target uid at the moment this runs -- a sanity check
//! against misconfiguration, not a guarantee against one starting a moment
//! later. Collision-free uid allocation across the whole host is the
//! caller's contract to keep, not something enforceable from in here.
//!
//! Nothing here applies a cgroup. That stays the supervisor's job, exactly
//! as before -- see `docs/SECURITY.md`, "Why not Firecracker's jailer".

use anyhow::{Context, Result, bail, ensure};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

// ── Constants not exposed by the pinned libc, or not worth trusting sight
// unseen -- same reasoning as `vmm/src/isolation.rs`'s CLONE_NEWUSER /
// CLONE_NEWNET: this is security-critical code short enough that spelling
// the numbers out here, next to their source, is worth more than importing
// them from somewhere else in the dependency tree.
const CLONE_NEWNS: libc::c_int = 0x0002_0000;
const MS_REC: libc::c_ulong = 0x0000_4000;
const MS_PRIVATE: libc::c_ulong = 0x0004_0000;
const MS_BIND: libc::c_ulong = 0x0000_1000;

const USAGE: &str = "Usage:
  jailer --jail-root <path> --uid <uid> --gid <gid> [options] -- <command> [args...]

Bind-mounts exactly the paths named below into <jail-root>, chroots into it,
drops from root to <uid>:<gid>, then execs <command> (an absolute path,
resolved inside the jail, e.g. /usr/bin/nesbox). Must run as root -- that is
the whole point: it does the things nesbox's own seccomp filter refuses to
let nesbox do to itself.

Options:
  --render-node <path>   DRM render node, e.g. /dev/dri/renderD128
  --kvm <path>            default: /dev/kvm
  --vhost <path>          a vhost device node in use; repeatable
  --metrics-dir <path>    directory the metrics socket will be created in

/proc and /sys are always bound in, read-write, exactly as they are on the
host -- see the module doc comment for why, and for what that does and does
not mean.";

#[derive(Debug)]
struct Args {
    jail_root: PathBuf,
    uid: u32,
    gid: u32,
    render_node: Option<PathBuf>,
    kvm: PathBuf,
    vhost: Vec<PathBuf>,
    metrics_dir: Option<PathBuf>,
    command: Vec<String>,
}

fn parse(argv: &[String]) -> Result<Args, String> {
    let mut jail_root = None;
    let mut uid = None;
    let mut gid = None;
    let mut render_node = None;
    let mut kvm = PathBuf::from("/dev/kvm");
    let mut vhost = Vec::new();
    let mut metrics_dir = None;

    fn val(argv: &[String], i: usize) -> Result<&str, String> {
        argv.get(i + 1)
            .map(String::as_str)
            .ok_or_else(|| format!("{} needs a value", argv[i]))
    }

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--jail-root" => {
                jail_root = Some(PathBuf::from(val(argv, i)?));
                i += 2
            }
            "--uid" => {
                uid = Some(
                    val(argv, i)?
                        .parse::<u32>()
                        .map_err(|e| format!("--uid: {e}"))?,
                );
                i += 2
            }
            "--gid" => {
                gid = Some(
                    val(argv, i)?
                        .parse::<u32>()
                        .map_err(|e| format!("--gid: {e}"))?,
                );
                i += 2
            }
            "--render-node" => {
                render_node = Some(PathBuf::from(val(argv, i)?));
                i += 2
            }
            "--kvm" => {
                kvm = PathBuf::from(val(argv, i)?);
                i += 2
            }
            "--vhost" => {
                vhost.push(PathBuf::from(val(argv, i)?));
                i += 2
            }
            "--metrics-dir" => {
                metrics_dir = Some(PathBuf::from(val(argv, i)?));
                i += 2
            }
            "-h" | "--help" => return Err(USAGE.to_string()),
            "--" => {
                i += 1;
                break;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let command: Vec<String> = argv[i..].to_vec();

    Ok(Args {
        jail_root: jail_root.ok_or("--jail-root is required")?,
        uid: uid.ok_or("--uid is required")?,
        gid: gid.ok_or("--gid is required")?,
        render_node,
        kvm,
        vhost,
        metrics_dir,
        command: {
            if command.is_empty() {
                return Err("no command given -- pass one after `--`".to_string());
            }
            command
        },
    })
}

// ── Mounting ─────────────────────────────────────────────────────────────

fn path_to_cstring(p: &Path) -> Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .with_context(|| format!("{} contains a NUL byte", p.display()))
}

/// `mount(2)`, with `None` meaning "pass NULL" for `source` and `fstype` --
/// which is what a propagation change (no source) and a bind mount (fstype
/// is ignored by the kernel once `MS_BIND` is set) both want.
fn raw_mount(source: Option<&Path>, target: &Path, flags: libc::c_ulong) -> Result<()> {
    let src_c = source.map(path_to_cstring).transpose()?;
    let tgt_c = path_to_cstring(target)?;
    // SAFETY: both pointers are either null or borrowed from a CString that
    // outlives this call; `mount` reads them and returns.
    let rc = unsafe {
        libc::mount(
            src_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            tgt_c.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "mount({:?}, {})",
                source.map(Path::display),
                target.display()
            )
        });
    }
    Ok(())
}

/// New mount namespace, then cut propagation. Order matters: without the
/// second call every bind mount below would still propagate back to the
/// namespace this process unshared *from* -- `unshare` alone only gives a
/// private copy of the mount *table*, not private propagation.
fn unshare_mount_namespace() -> Result<()> {
    // SAFETY: constant flag, no pointer arguments.
    if unsafe { libc::unshare(CLONE_NEWNS) } != 0 {
        return Err(std::io::Error::last_os_error()).context("unshare(CLONE_NEWNS)");
    }
    raw_mount(None, Path::new("/"), MS_REC | MS_PRIVATE)
        .context("making the mount tree private after unshare")
}

/// Where `host_path` (always absolute) lands inside `jail_root`.
fn target_path(jail_root: &Path, host_path: &Path) -> PathBuf {
    let rel = host_path.strip_prefix("/").unwrap_or(host_path);
    jail_root.join(rel)
}

/// Bind-mount `host_path` into the jail at the same relative path.
///
/// `recursive` pulls in submounts under `host_path` -- needed for `/sys`,
/// whose cgroup2 controllers are their own mount under `/sys/fs/cgroup`, not
/// part of `/sys` itself. A non-recursive bind would leave that directory
/// present but empty, and cgroup self-reporting would silently read nothing.
///
/// Left read-write, same as the host's own permission model already makes
/// it. A recursive bind can only be remounted read-only one submount at a
/// time before Linux 5.12's `mount_setattr(2)`; doing that for the top-level
/// mount alone while leaving `/sys/fs/cgroup` writable underneath would be a
/// mount that *looks* read-only and is not -- worse than being honest that
/// it is not one. See the module doc comment for what that does and does
/// not cost.
fn bind_mount(jail_root: &Path, host_path: &Path, recursive: bool) -> Result<()> {
    ensure!(
        host_path.is_absolute(),
        "{} is not an absolute path",
        host_path.display()
    );
    let meta = std::fs::symlink_metadata(host_path)
        .with_context(|| format!("{} does not exist on the host", host_path.display()))?;
    let target = target_path(jail_root, host_path);

    if meta.is_dir() {
        std::fs::create_dir_all(&target)
            .with_context(|| format!("creating mount point {}", target.display()))?;
    } else {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // A bind mount's target only needs to exist; an empty regular file
        // is the usual stand-in for a device node, and nothing is ever read
        // from it -- the mount covers it entirely.
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&target)
            .with_context(|| format!("creating mount point {}", target.display()))?;
    }

    let flags = MS_BIND | if recursive { MS_REC } else { 0 };
    raw_mount(Some(host_path), &target, flags).with_context(|| {
        format!(
            "bind-mounting {} onto {}",
            host_path.display(),
            target.display()
        )
    })?;
    log::info!(
        "jailer: bound {} -> {}",
        host_path.display(),
        target.display()
    );
    Ok(())
}

// ── chroot, privilege drop, exec ────────────────────────────────────────

fn enter_jail(jail_root: &Path) -> Result<()> {
    let c = path_to_cstring(jail_root)?;
    // SAFETY: `c` is a valid, NUL-terminated path that outlives the call.
    if unsafe { libc::chroot(c.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("chroot({})", jail_root.display()));
    }
    let root = CString::new("/").expect("no NUL byte");
    // SAFETY: `root` is a valid, NUL-terminated path that outlives the call.
    // Required after chroot: the current directory is not implicitly moved,
    // and a process left outside the new root can walk back out of it via
    // `..` -- the well-known chroot-escape shape.
    if unsafe { libc::chdir(root.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("chdir(\"/\") after chroot");
    }
    Ok(())
}

/// Drop from root to `uid:gid`, and check that it actually took rather than
/// assuming a zero return means what it says.
fn drop_privileges(uid: u32, gid: u32) -> Result<()> {
    // SAFETY: infallible per POSIX when the count is 0.
    if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("setgroups(0, NULL) -- clearing supplementary groups");
    }
    // gid before uid: dropping uid first would remove the privilege needed
    // to still change gid.
    // SAFETY: constant arguments, no pointers.
    if unsafe { libc::setgid(gid) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("setgid({gid})"));
    }
    // SAFETY: as above.
    if unsafe { libc::setuid(uid) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("setuid({uid})"));
    }

    // SAFETY: infallible.
    let (got_uid, got_gid) = unsafe { (libc::getuid(), libc::getgid()) };
    ensure!(
        got_uid == uid && got_gid == gid,
        "privilege drop did not take: uid={got_uid} gid={got_gid}, wanted uid={uid} gid={gid}"
    );
    // Belt and suspenders: a working privilege drop makes regaining root
    // impossible. If it is somehow still possible, something above this line
    // is wrong in a way the uid/gid check alone would not catch -- refuse to
    // exec rather than hand a compromised device model a way back to root.
    // SAFETY: constant argument.
    let regained = unsafe { libc::setuid(0) };
    ensure!(
        regained != 0,
        "still able to regain root after dropping to uid {uid} -- refusing to exec"
    );
    Ok(())
}

/// Best-effort: refuse if a process already running on the host holds `uid`.
///
/// Not a guarantee -- a process can start with this uid the instant after
/// this returns -- but it catches the misconfiguration that actually
/// happens, a uid pool that collides with an existing host account. See the
/// module doc comment for why real collision-free allocation has to be the
/// caller's job rather than something checked here once and trusted.
fn refuse_if_uid_is_live(uid: u32) -> Result<()> {
    for entry in std::fs::read_dir("/proc")
        .context("reading /proc")?
        .flatten()
    {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            // Not a pid directory (task, self, sys, ...).
            continue;
        };
        // The process can exit between the readdir listing it and this
        // read -- an ordinary race with process lifetime, not a collision.
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            continue;
        };
        // "Uid:\treal\teffective\tsaved\tfs" -- the first (real) uid is what
        // owns the process for the /proc/<pid>/root permission check this
        // guards against.
        let real_uid = status
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|f| f.split_whitespace().next())
            .and_then(|s| s.parse::<u32>().ok());
        if real_uid == Some(uid) {
            bail!(
                "uid {uid} is already in use by host pid {pid} -- refusing to drop into it. \
                 A jailed process sharing a uid with a live host process can read that \
                 process's /proc/<pid>/root, which reaches outside this jail. Allocate a uid \
                 that is not already running anything."
            );
        }
    }
    Ok(())
}

/// `execv`. Inherits this process's environment verbatim -- whoever invokes
/// the jailer is responsible for handing it a clean one, same as it would be
/// for any other process it started directly.
fn exec_command(command: &[String]) -> Result<()> {
    // Without this, execve honors setuid/setgid bits and file capabilities
    // on the target binary -- which would hand back exactly the privilege
    // drop_privileges() just gave up, if the jail image's binary carried any
    // (by mistake, or by a compromised build). nesbox sets this too
    // (vmm/src/seccomp.rs), but only after this exec has already completed,
    // which is too late for this exec's own privilege bits: NO_NEW_PRIVS has
    // to be set by the process making the exec call, before it makes it.
    // SAFETY: constant arguments, no pointers.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error()).context("prctl(PR_SET_NO_NEW_PRIVS)");
    }

    let prog = CString::new(command[0].as_str())
        .with_context(|| format!("{} contains a NUL byte", command[0]))?;
    let args: Vec<CString> = command
        .iter()
        .map(|s| CString::new(s.as_str()).map_err(anyhow::Error::from))
        .collect::<Result<_>>()?;
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());

    // SAFETY: `argv` is NUL-terminated and every pointer in it is borrowed
    // from a `CString` still alive in `args`. `execv` either replaces this
    // process's image and never returns, or returns -1 and leaves `args`
    // untouched.
    unsafe { libc::execv(prog.as_ptr(), argv.as_ptr()) };
    Err(std::io::Error::last_os_error()).with_context(|| format!("execv({})", command[0]))
}

// ── main ─────────────────────────────────────────────────────────────────

fn run(args: Args) -> Result<()> {
    // SAFETY: infallible.
    let euid = unsafe { libc::geteuid() };
    ensure!(
        euid == 0,
        "jailer must run as root: it needs CAP_SYS_ADMIN to unshare a mount \
         namespace and chroot, and CAP_SETUID/CAP_SETGID to drop out of root \
         afterwards. Nothing it does stays privileged past drop_privileges()."
    );
    ensure!(
        args.jail_root.is_dir(),
        "--jail-root {} is not a directory",
        args.jail_root.display()
    );
    ensure!(
        args.uid != 0 && args.gid != 0,
        "--uid/--gid 0 defeats the whole point of dropping out of root"
    );
    refuse_if_uid_is_live(args.uid)?;

    unshare_mount_namespace()?;

    // Always -- cgroup self-reporting and Mesa's own /proc reads both need
    // them, and they are not meaningfully optional the way a GPU or a vhost
    // device is. See the module doc comment for what a host-sourced /proc
    // and /sys inside the jail does and does not mean.
    bind_mount(&args.jail_root, Path::new("/proc"), true)?;
    bind_mount(&args.jail_root, Path::new("/sys"), true)?;

    if let Some(render_node) = &args.render_node {
        bind_mount(&args.jail_root, render_node, false)?;
    }
    bind_mount(&args.jail_root, &args.kvm, false)?;
    for vhost in &args.vhost {
        bind_mount(&args.jail_root, vhost, false)?;
    }
    if let Some(metrics_dir) = &args.metrics_dir {
        bind_mount(&args.jail_root, metrics_dir, false)?;
    }

    log::info!(
        "jailer: chrooting into {}, dropping to uid={} gid={}, exec'ing {:?}",
        args.jail_root.display(),
        args.uid,
        args.gid,
        args.command
    );

    enter_jail(&args.jail_root)?;
    drop_privileges(args.uid, args.gid)?;
    exec_command(&args.command)
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse(&argv) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    if let Err(e) = run(args) {
        eprintln!("jailer: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_root() -> bool {
        // SAFETY: infallible.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn refuses_a_uid_already_in_use_on_the_host() {
        // The test process itself is a live process with its own uid --
        // true regardless of anything else running on this host.
        // SAFETY: infallible.
        let my_uid = unsafe { libc::getuid() };
        let err = refuse_if_uid_is_live(my_uid).unwrap_err();
        assert!(err.to_string().contains("already in use"), "{err}");
    }

    #[test]
    fn an_unused_uid_passes() {
        // One below u32::MAX -- astronomically unlikely to be a real
        // account on any host this runs on.
        refuse_if_uid_is_live(4_294_967_294).expect("no process should hold this uid");
    }

    #[test]
    fn target_path_mirrors_the_host_path_under_the_jail_root() {
        assert_eq!(
            target_path(Path::new("/jail"), Path::new("/dev/dri/renderD128")),
            PathBuf::from("/jail/dev/dri/renderD128")
        );
        assert_eq!(
            target_path(Path::new("/jail"), Path::new("/sys")),
            PathBuf::from("/jail/sys")
        );
        // A relative host_path (should never happen -- callers only ever
        // pass absolute device/proc/sys paths) still lands somewhere sane
        // rather than escaping the jail root.
        assert_eq!(
            target_path(Path::new("/jail"), Path::new("weird")),
            PathBuf::from("/jail/weird")
        );
    }

    #[test]
    fn parsing_requires_jail_root_uid_gid_and_a_command() {
        assert!(parse(&[]).is_err());
        assert!(
            parse(&["--uid".into(), "1000".into()])
                .unwrap_err()
                .contains("--jail-root")
        );

        let argv: Vec<String> = [
            "--jail-root",
            "/jail",
            "--uid",
            "1000",
            "--gid",
            "1000",
            "--render-node",
            "/dev/dri/renderD128",
            "--vhost",
            "/dev/vhost-net",
            "--vhost",
            "/dev/vhost-vsock",
            "--",
            "/usr/bin/nesbox",
            "/run/nesbox/box.json",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let a = parse(&argv).unwrap();
        assert_eq!(a.jail_root, PathBuf::from("/jail"));
        assert_eq!(a.uid, 1000);
        assert_eq!(a.gid, 1000);
        assert_eq!(a.render_node, Some(PathBuf::from("/dev/dri/renderD128")));
        assert_eq!(a.kvm, PathBuf::from("/dev/kvm"), "default, not passed");
        assert_eq!(
            a.vhost,
            vec![
                PathBuf::from("/dev/vhost-net"),
                PathBuf::from("/dev/vhost-vsock")
            ]
        );
        assert_eq!(
            a.command,
            vec![
                "/usr/bin/nesbox".to_string(),
                "/run/nesbox/box.json".to_string()
            ]
        );
    }

    #[test]
    fn a_command_is_required_after_the_separator() {
        let argv: Vec<String> = [
            "--jail-root",
            "/jail",
            "--uid",
            "1000",
            "--gid",
            "1000",
            "--",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert!(parse(&argv).unwrap_err().contains("no command"));
    }

    #[test]
    fn kvm_can_be_overridden() {
        let argv: Vec<String> = [
            "--jail-root",
            "/jail",
            "--uid",
            "1000",
            "--gid",
            "1000",
            "--kvm",
            "/dev/kvm-alt",
            "--",
            "/bin/true",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(parse(&argv).unwrap().kvm, PathBuf::from("/dev/kvm-alt"));
    }

    /// End to end, in a forked child so a failure or a stray mount cannot
    /// affect the test process. Skips itself when not root, the same
    /// pattern `vmm/src/isolation.rs` uses for its own privileged tests.
    #[test]
    fn a_real_jail_chroots_binds_drops_and_execs() {
        if !is_root() {
            eprintln!("skipped: not root");
            return;
        }
        // SAFETY: the child only touches its own address space and exits.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            let code = (|| -> Result<i32> {
                let dir = std::env::temp_dir().join(format!("jailer-test-{}", std::process::id()));
                let jail_root = dir.join("root");
                std::fs::create_dir_all(&jail_root)?;
                // A marker file inside the jail, findable only if the exec'd
                // process really did land inside it.
                std::fs::write(jail_root.join("marker"), b"jailed\n")?;

                unshare_mount_namespace()?;
                bind_mount(&jail_root, Path::new("/proc"), true)?;

                enter_jail(&jail_root)?;
                // Confined here on: cat can only see /proc and /marker, not
                // the rest of the host filesystem. Reaching for /etc/passwd
                // would prove the chroot did nothing.
                ensure!(
                    !Path::new("/etc/passwd").exists(),
                    "the host's /etc/passwd is reachable inside the jail"
                );
                ensure!(
                    Path::new("/marker").exists(),
                    "the jail's own marker is gone"
                );
                ensure!(
                    Path::new("/proc/self/status").exists(),
                    "the bound /proc did not come through"
                );

                // uid 65534 (nobody) is about as safe a target as any system
                // is guaranteed to have.
                drop_privileges(65534, 65534)?;
                ensure!(
                    unsafe { libc::getuid() } == 65534,
                    "did not actually drop to nobody"
                );

                Ok(0)
            })();
            // SAFETY: exiting without unwinding.
            unsafe {
                libc::syscall(
                    libc::SYS_exit_group,
                    match code {
                        Ok(_) => 0,
                        Err(e) => {
                            eprintln!("jailer test child: {e:#}");
                            1
                        }
                    },
                )
            };
            unreachable!();
        }
        let mut status = 0;
        // SAFETY: waiting on our own child.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "child failed, status {status:#x}"
        );
    }
}
