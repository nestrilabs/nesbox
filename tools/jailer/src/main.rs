//! jailer -- runs one nesbox box inside a jail.
//!
//! This is nesbox's jailer, not a general-purpose one, and the difference is
//! the point. It takes the box's own config file, works out from it which
//! host paths that box will need, brings exactly those in, and execs nesbox
//! with the same config. There is no `-- <command>` to hand it: the program
//! it starts is nesbox, and knowing that is what lets it do the work a
//! caller would otherwise have to do by hand and keep in step by hand.
//! Firecracker's jailer is the model -- `--exec-file`, and a jail set up for
//! the one program it knows it is starting.
//!
//! nesbox cannot do any of this to itself. `chroot`, `mount` and `setuid` are
//! all on its own seccomp denylist (`vmm/src/seccomp.rs`,
//! `nothing_that_reconfigures_the_host_is_allowed_anywhere`), and an
//! unprivileged process cannot change its own uid regardless of what its
//! filter permits. So this is a separate binary, run before nesbox exists,
//! that does six things and then is gone:
//!
//!   1. read the box config, and derive every host path that box needs.
//!   2. unshare a mount namespace, and cut propagation to the host's, so
//!      nothing done here leaks back out.
//!   3. bind-mount those paths, at the same path inside the jail.
//!   4. `chroot` into the jail root -- already built and materialized
//!      elsewhere. This binary does not build, fetch or version it.
//!   5. drop from root to the uid/gid the caller names.
//!   6. `execve` nesbox, with the same config file.
//!
//! # The config decides what comes in
//!
//! `docs/SECURITY.md` considered a jailer once and rejected it because "the
//! DRM render node, virtiofs source directories, and the metrics socket path
//! all live outside any plausible jail." That is true of a jail built to
//! *exclude* the host; it is not true of one built to *include* exactly what
//! is needed. And the list is neither guesswork nor a pile of flags the
//! caller has to keep in step with the config -- it is read out of it:
//!
//!   * `boot-source.kernel_image_path`     the kernel nesbox loads
//!   * `drives[].path_on_host`             every disk image
//!   * `gpu.render-node`                   the DRM render node
//!   * `network`, if present               /dev/net/tun and /dev/vhost-net
//!   * `vsock`, if present                 /dev/vhost-vsock
//!   * `stats-socket`, if present          the directory it is created in
//!   * `shared-directories[].path-on-host` each virtiofs source
//!
//! plus `/dev/kvm`, `/proc` and `/sys`, which every box needs, and the config
//! file itself. [`Args::bind`] is the escape hatch for anything a config does
//! not name; needing it routinely means something belongs in the list above.
//!
//! Every path has to be absolute, and this refuses a config that gives a
//! relative one. nesbox opens them *after* this binary has chrooted and
//! `chdir`ed to `/`, so a relative path would resolve against a directory
//! that no longer exists -- better a refusal here, naming the field, than a
//! confusing ENOENT out of nesbox later.
//!
//! virtiofsd is the one thing nesbox opens that is deliberately *not* bound
//! in: nesbox spawns it, so the binary has to exist inside the jail, and the
//! jail image ships it (`build/Dockerfile`). Only the directories it serves
//! come from the host. It still runs `--sandbox none`, unsandboxed, exactly
//! as `docs/SECURITY.md` already records.
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
use serde::Deserialize;
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

/// Where nesbox lives inside the jail image `build/` produces. Overridable,
/// because an image built somewhere else may lay it out differently, but not
/// something a caller should have to say.
const DEFAULT_NESBOX_BIN: &str = "/usr/bin/nesbox";

/// Needed by every box, named by no config, and not worth an option: a VMM
/// without `/dev/kvm` is not a VMM.
const KVM: &str = "/dev/kvm";
/// `virtio-devices/src/tap.rs`'s `TUN_PATH`. nesbox opens an existing tap
/// through it; creating one is the host setup script's job.
const TUN: &str = "/dev/net/tun";
/// `virtio-devices/src/net.rs` opens this whenever a box has a network.
const VHOST_NET: &str = "/dev/vhost-net";
/// `virtio-devices/src/vsock.rs` opens this whenever a box has a vsock.
const VHOST_VSOCK: &str = "/dev/vhost-vsock";

const USAGE: &str = "Usage:
  jailer --config <box.json> --jail-root <path> --uid <uid> --gid <gid> [options]

Runs nesbox inside a jail. Reads <box.json> to work out which host paths that
box needs -- its kernel, its disk images, its render node, its tap and vhost
devices, its virtiofs sources, its metrics directory -- bind-mounts them into
<jail-root> at the same path, chroots, drops from root to <uid>:<gid>, and
execs nesbox with the same config.

Must run as root -- that is the whole point: it does the things nesbox's own
seccomp filter refuses to let nesbox do to itself. Nothing it does stays
privileged past the drop.

Options:
  --nesbox-bin <path>   nesbox inside the jail image. Default /usr/bin/nesbox.
  --bind <path>          an extra host path to bring in, at the same path
                         inside the jail; repeatable. An escape hatch for
                         something a config does not name -- everything a
                         config does name comes in without being asked for.

Every path in the config must be absolute: this chroots before nesbox opens
any of them, so a relative path would resolve against a directory that is no
longer there. /dev/kvm, /proc and /sys always come in. /proc and /sys are the
host's own, read-write -- see the module doc comment for what that does and
does not mean.";

#[derive(Debug)]
struct Args {
    config: PathBuf,
    jail_root: PathBuf,
    uid: u32,
    gid: u32,
    nesbox_bin: PathBuf,
    bind: Vec<PathBuf>,
}

fn parse(argv: &[String]) -> Result<Args, String> {
    let mut config = None;
    let mut jail_root = None;
    let mut uid = None;
    let mut gid = None;
    let mut nesbox_bin = PathBuf::from(DEFAULT_NESBOX_BIN);
    let mut bind = Vec::new();

    fn val(argv: &[String], i: usize) -> Result<&str, String> {
        argv.get(i + 1)
            .map(String::as_str)
            .ok_or_else(|| format!("{} needs a value", argv[i]))
    }

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--config" => {
                config = Some(PathBuf::from(val(argv, i)?));
                i += 2
            }
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
            "--nesbox-bin" => {
                nesbox_bin = PathBuf::from(val(argv, i)?);
                i += 2
            }
            "--bind" => {
                bind.push(PathBuf::from(val(argv, i)?));
                i += 2
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Args {
        config: config.ok_or("--config is required")?,
        jail_root: jail_root.ok_or("--jail-root is required")?,
        uid: uid.ok_or("--uid is required")?,
        gid: gid.ok_or("--gid is required")?,
        nesbox_bin,
        bind,
    })
}

// ── The config, as far as it names host paths ────────────────────────────
//
// A deliberately partial mirror of `vmm/src/config.rs`'s `VmConfig`, not a
// dependency on it: pulling the vmm crate in would put kvm-ioctls, vm-memory
// and the whole device model into a binary that runs as root, to read seven
// fields. Serde ignores everything not named here, so a new config field
// costs nothing until it is a path this has to bring in.
//
// The cost of the copy is drift, and `derives_every_host_path_in_the_example`
// is what catches it: it parses `examples/vm.json` and asserts the whole
// derived set, so renaming a field in one place without the other fails a
// test rather than producing a box that cannot find its kernel.
//
// Note the mixed case convention, which is nesbox's, not a mistake here:
// `VmConfig`, `Gpu`, `Network`, `Vsock` and `SharedDirectory` are
// `rename_all = "kebab-case"`, while `BootSource` and `Drive` are not and so
// keep their snake_case field names.

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct BoxConfig {
    boot_source: BootSource,
    #[serde(default)]
    drives: Vec<Drive>,
    #[serde(default)]
    gpu: Option<Gpu>,
    /// Presence is all that matters: a box with a network opens `/dev/net/tun`
    /// and `/dev/vhost-net`, whatever the tap is called.
    #[serde(default)]
    network: Option<serde_json::Value>,
    /// Likewise -- a box with a vsock opens `/dev/vhost-vsock`.
    #[serde(default)]
    vsock: Option<serde_json::Value>,
    #[serde(default)]
    shared_directories: Vec<SharedDirectory>,
    #[serde(default)]
    stats_socket: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct BootSource {
    kernel_image_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Drive {
    drive_id: String,
    path_on_host: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Gpu {
    #[serde(default = "default_render_node")]
    render_node: PathBuf,
}

fn default_render_node() -> PathBuf {
    // Kept in step with `vmm/src/config.rs`'s own default: a config with a
    // `gpu` section and no `render-node` gets this one there too, and a jail
    // missing the node nesbox will actually open is a black screen, not an
    // error.
    PathBuf::from("/dev/dri/renderD128")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct SharedDirectory {
    tag: String,
    path_on_host: PathBuf,
}

/// One path to bring in, and the config field that asked for it -- the
/// reason travels with the path so a failure can say which field is wrong
/// rather than just which path did not exist.
#[derive(Debug, PartialEq, Eq)]
struct Needed {
    path: PathBuf,
    why: String,
}

fn needed(path: impl Into<PathBuf>, why: impl Into<String>) -> Needed {
    Needed {
        path: path.into(),
        why: why.into(),
    }
}

/// Every host path this box needs, in the order they are mounted.
///
/// Order is insertion order and matters when one path nests inside another:
/// a directory has to be bound before something bound underneath it, or the
/// outer mount hides the inner one.
fn host_paths(config_path: &Path, cfg: &BoxConfig, extra: &[PathBuf]) -> Result<Vec<Needed>> {
    let mut out = vec![
        // nesbox re-reads its own config after the exec, from inside the jail.
        needed(config_path, "the config file itself"),
        needed(KVM, "every box needs /dev/kvm"),
        needed(
            &cfg.boot_source.kernel_image_path,
            "boot-source.kernel_image_path",
        ),
    ];

    for d in &cfg.drives {
        out.push(needed(
            &d.path_on_host,
            format!("drives[\"{}\"].path_on_host", d.drive_id),
        ));
    }
    if let Some(gpu) = &cfg.gpu {
        out.push(needed(&gpu.render_node, "gpu.render-node"));
    }
    if cfg.network.is_some() {
        out.push(needed(TUN, "network is set, so nesbox opens a tap"));
        out.push(needed(VHOST_NET, "network is set"));
    }
    if cfg.vsock.is_some() {
        out.push(needed(VHOST_VSOCK, "vsock is set"));
    }
    if let Some(socket) = &cfg.stats_socket {
        // The directory, not the socket: nesbox creates the socket itself, so
        // the file does not exist yet and there would be nothing to bind.
        let dir = socket
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .with_context(|| {
                format!(
                    "stats-socket {} has no directory to create it in",
                    socket.display()
                )
            })?;
        out.push(needed(dir, "the directory stats-socket lives in"));
    }
    for s in &cfg.shared_directories {
        out.push(needed(
            &s.path_on_host,
            format!("shared-directories[\"{}\"].path-on-host", s.tag),
        ));
    }
    for p in extra {
        out.push(needed(p, "--bind"));
    }

    // Absolute or nothing, and named as the field that broke it. Checked
    // here, over the whole set at once, so a config with three relative
    // paths does not take three runs to fix.
    let relative: Vec<String> = out
        .iter()
        .filter(|n| !n.path.is_absolute())
        .map(|n| format!("  {} ({})", n.path.display(), n.why))
        .collect();
    ensure!(
        relative.is_empty(),
        "these paths are relative, and this jailer chroots before nesbox \
         opens any of them -- make them absolute:\n{}",
        relative.join("\n")
    );
    Ok(out)
}

fn read_config(path: &Path) -> Result<BoxConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the box config at {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
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
        args.uid != 0 && args.gid != 0,
        "--uid/--gid 0 defeats the whole point of dropping out of root"
    );

    // Both canonicalized before anything is derived from them. A relative
    // --config would otherwise be bound at a path that does not mirror the
    // host's, and a relative --jail-root would be resolved against a working
    // directory that chroot is about to make meaningless.
    let config_path = std::fs::canonicalize(&args.config)
        .with_context(|| format!("--config {}", args.config.display()))?;
    let jail_root = std::fs::canonicalize(&args.jail_root)
        .with_context(|| format!("--jail-root {}", args.jail_root.display()))?;
    ensure!(
        jail_root.is_dir(),
        "--jail-root {} is not a directory",
        jail_root.display()
    );

    let cfg = read_config(&config_path)?;
    let paths = host_paths(&config_path, &cfg, &args.bind)?;

    refuse_if_uid_is_live(args.uid)?;

    unshare_mount_namespace()?;

    // Always, and before the derived set: cgroup self-reporting and Mesa's
    // own /proc reads both need them, and they are not meaningfully optional
    // the way a GPU or a vhost device is. Recursive, because /sys/fs/cgroup
    // is its own mount. See the module doc comment for what a host-sourced
    // /proc and /sys inside the jail does and does not mean.
    bind_mount(&jail_root, Path::new("/proc"), true)?;
    bind_mount(&jail_root, Path::new("/sys"), true)?;

    for n in &paths {
        bind_mount(&jail_root, &n.path, false)
            .with_context(|| format!("{} is needed because of {}", n.path.display(), n.why))?;
    }

    log::info!(
        "jailer: {} paths bound, chrooting into {}, dropping to uid={} gid={}, exec'ing {} {}",
        paths.len(),
        jail_root.display(),
        args.uid,
        args.gid,
        args.nesbox_bin.display(),
        config_path.display(),
    );

    enter_jail(&jail_root)?;
    drop_privileges(args.uid, args.gid)?;

    // nesbox's whole command line: the binary inside the jail, and the config
    // at the same path it had on the host, because that is where it was bound.
    // nesbox takes the first non-flag argument as its config
    // (`vmm/src/bin/nesbox.rs`) and has no other flags, which is why there is
    // nothing to forward here and no `--` to forward it with.
    let command = vec![
        args.nesbox_bin.to_string_lossy().into_owned(),
        config_path.to_string_lossy().into_owned(),
    ];
    exec_command(&command)
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Before `parse`, not inside it: asking for help is not a usage error, so
    // it prints once and exits 0.
    if argv.is_empty() || argv.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return;
    }
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

    fn args(pairs: &[&str]) -> Vec<String> {
        pairs.iter().map(|s| s.to_string()).collect()
    }

    fn parsed(cfg: &BoxConfig) -> Vec<PathBuf> {
        host_paths(Path::new("/boxes/1/box.json"), cfg, &[])
            .expect("derives")
            .into_iter()
            .map(|n| n.path)
            .collect()
    }

    // ── the CLI ─────────────────────────────────────────────────────────

    #[test]
    fn config_jail_root_uid_and_gid_are_all_required() {
        for missing in [
            args(&["--jail-root", "/jail", "--uid", "1", "--gid", "1"]),
            args(&["--config", "/b.json", "--uid", "1", "--gid", "1"]),
            args(&["--config", "/b.json", "--jail-root", "/jail", "--gid", "1"]),
            args(&["--config", "/b.json", "--jail-root", "/jail", "--uid", "1"]),
        ] {
            assert!(parse(&missing).is_err(), "{missing:?} should not parse");
        }
        assert!(
            parse(&args(&["--jail-root", "/jail"]))
                .unwrap_err()
                .contains("--config")
        );
    }

    #[test]
    fn nesbox_is_the_default_and_needs_no_command() {
        let a = parse(&args(&[
            "--config",
            "/boxes/1/box.json",
            "--jail-root",
            "/jail",
            "--uid",
            "60000",
            "--gid",
            "60000",
        ]))
        .unwrap();
        assert_eq!(a.config, PathBuf::from("/boxes/1/box.json"));
        assert_eq!(a.jail_root, PathBuf::from("/jail"));
        assert_eq!(a.uid, 60000);
        assert_eq!(a.gid, 60000);
        // The whole reason this is nesbox's jailer and not a generic one:
        // there is no command to name.
        assert_eq!(a.nesbox_bin, PathBuf::from("/usr/bin/nesbox"));
        assert!(a.bind.is_empty());
    }

    #[test]
    fn a_jail_image_can_put_nesbox_somewhere_else() {
        let a = parse(&args(&[
            "--config",
            "/b.json",
            "--jail-root",
            "/jail",
            "--uid",
            "1",
            "--gid",
            "1",
            "--nesbox-bin",
            "/opt/nesbox/bin/nesbox",
        ]))
        .unwrap();
        assert_eq!(a.nesbox_bin, PathBuf::from("/opt/nesbox/bin/nesbox"));
    }

    #[test]
    fn bind_is_repeatable_and_keeps_its_order() {
        let a = parse(&args(&[
            "--config",
            "/b.json",
            "--jail-root",
            "/jail",
            "--uid",
            "1",
            "--gid",
            "1",
            "--bind",
            "/opt/first",
            "--bind",
            "/opt/second",
        ]))
        .unwrap();
        assert_eq!(
            a.bind,
            vec![PathBuf::from("/opt/first"), PathBuf::from("/opt/second")]
        );
    }

    #[test]
    fn a_stray_command_is_a_usage_error_rather_than_something_to_exec() {
        // What the old generic interface would have accepted. There is no
        // `--` any more, so this is a mistake and says so.
        let err = parse(&args(&[
            "--config",
            "/b.json",
            "--jail-root",
            "/jail",
            "--uid",
            "1",
            "--gid",
            "1",
            "--",
            "/usr/bin/nesbox",
        ]))
        .unwrap_err();
        assert!(err.contains("unknown argument: --"), "{err}");
    }

    // ── deriving the mount set from a config ────────────────────────────

    #[test]
    fn derives_every_host_path_in_the_example() {
        // examples/vm.json is the documented config format, so it is also the
        // drift check: if a field is renamed in vmm/src/config.rs and here,
        // this keeps passing; if it is renamed in only one of them, serde
        // stops finding it and this fails.
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/vm.json");
        let cfg = read_config(&example).expect("the example config parses");
        assert_eq!(
            parsed(&cfg),
            vec![
                PathBuf::from("/boxes/1/box.json"),
                PathBuf::from("/dev/kvm"),
                PathBuf::from("/path/to/vmlinux"),
                PathBuf::from("/path/to/rootfs.ext4"),
                PathBuf::from("/dev/dri/renderD128"),
                PathBuf::from("/dev/net/tun"),
                PathBuf::from("/dev/vhost-net"),
                PathBuf::from("/dev/vhost-vsock"),
                PathBuf::from("/path/to/compat/prefix"),
                PathBuf::from("/path/to/game/install"),
            ],
            "the example's paths, in mount order"
        );
    }

    #[test]
    fn a_box_with_no_devices_brings_in_no_device_nodes() {
        let cfg: BoxConfig =
            serde_json::from_str(r#"{ "boot-source": { "kernel_image_path": "/k/vmlinux" } }"#)
                .expect("parses");
        assert_eq!(
            parsed(&cfg),
            vec![
                PathBuf::from("/boxes/1/box.json"),
                PathBuf::from("/dev/kvm"),
                PathBuf::from("/k/vmlinux"),
            ],
            "no gpu, network or vsock means no render node, tap or vhost node"
        );
    }

    #[test]
    fn a_gpu_section_without_a_render_node_still_binds_the_default_one() {
        let cfg: BoxConfig = serde_json::from_str(
            r#"{ "boot-source": { "kernel_image_path": "/k" }, "gpu": { "width": 1280 } }"#,
        )
        .expect("parses");
        assert!(
            parsed(&cfg).contains(&PathBuf::from("/dev/dri/renderD128")),
            "the default has to match vmm/src/config.rs's, or nesbox opens a \
             node that is not in the jail"
        );
    }

    #[test]
    fn the_metrics_socket_brings_in_its_directory_not_itself() {
        let cfg: BoxConfig = serde_json::from_str(
            r#"{ "boot-source": { "kernel_image_path": "/k" },
                 "stats-socket": "/run/nesbox/1/stats.sock" }"#,
        )
        .expect("parses");
        let p = parsed(&cfg);
        assert!(p.contains(&PathBuf::from("/run/nesbox/1")), "{p:?}");
        assert!(
            !p.contains(&PathBuf::from("/run/nesbox/1/stats.sock")),
            "nesbox creates the socket, so there is nothing there to bind yet"
        );
    }

    #[test]
    fn extra_binds_come_last_and_are_kept() {
        let cfg: BoxConfig =
            serde_json::from_str(r#"{ "boot-source": { "kernel_image_path": "/k" } }"#)
                .expect("parses");
        let paths = host_paths(
            Path::new("/boxes/1/box.json"),
            &cfg,
            &[PathBuf::from("/opt/extra")],
        )
        .expect("derives");
        assert_eq!(paths.last().unwrap().path, PathBuf::from("/opt/extra"));
        assert_eq!(paths.last().unwrap().why, "--bind");
    }

    #[test]
    fn a_relative_path_in_the_config_is_refused_and_says_which_field() {
        let cfg: BoxConfig = serde_json::from_str(
            r#"{ "boot-source": { "kernel_image_path": "vmlinux" },
                 "drives": [ { "drive_id": "rootfs", "path_on_host": "rootfs.ext4",
                               "is_root_device": true } ] }"#,
        )
        .expect("parses");
        let err = host_paths(Path::new("/boxes/1/box.json"), &cfg, &[]).unwrap_err();
        let msg = format!("{err:#}");
        // Both of them, in one message: fixing a config should not take one
        // run per relative path.
        assert!(msg.contains("boot-source.kernel_image_path"), "{msg}");
        assert!(msg.contains("drives[\"rootfs\"].path_on_host"), "{msg}");
    }

    #[test]
    fn a_config_that_names_no_kernel_is_refused_by_serde() {
        // boot-source is required here even though nesbox defaults it: a
        // default of "vmlinux" is relative, so a config relying on it could
        // never work inside a jail anyway, and saying so at parse time is
        // clearer than deriving a path that is then rejected.
        assert!(serde_json::from_str::<BoxConfig>(r#"{ "drives": [] }"#).is_err());
    }

    #[test]
    fn unknown_config_fields_are_ignored() {
        // This mirrors part of VmConfig, so every field it does not model has
        // to be harmless -- otherwise every nesbox config addition breaks the
        // jailer.
        let cfg: BoxConfig = serde_json::from_str(
            r#"{ "boot-source": { "kernel_image_path": "/k", "boot_args": "ro" },
                 "machine-config": { "vcpu_count": 4 },
                 "seccomp": "strict",
                 "something-added-next-year": { "nested": true } }"#,
        )
        .expect("parses");
        assert_eq!(cfg.boot_source.kernel_image_path, PathBuf::from("/k"));
    }

    // ── the mounting machinery ─────────────────────────────────────────

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
        // A relative host_path (should never happen -- host_paths refuses
        // them) still lands somewhere sane rather than escaping the jail root.
        assert_eq!(
            target_path(Path::new("/jail"), Path::new("weird")),
            PathBuf::from("/jail/weird")
        );
    }

    /// End to end up to the exec, in a forked child so a failure or a stray
    /// mount cannot affect the test process. Skips itself when not root, the
    /// same pattern `vmm/src/isolation.rs` uses for its own privileged tests.
    ///
    /// [`exec_command`] itself is deliberately not called: it replaces the
    /// process image, so there would be no child left to assert in. What it
    /// does beyond `execv` -- `PR_SET_NO_NEW_PRIVS` -- is a process-wide,
    /// irreversible flag, which is exactly why it is not set in a test
    /// process either.
    #[test]
    fn a_real_jail_chroots_binds_and_drops() {
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
                // Confined here on: only /proc and /marker are reachable, not
                // the rest of the host filesystem. Reaching /etc/passwd would
                // prove the chroot did nothing.
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
