//! Confine the VMM with seccomp-bpf.
//!
//! A guest that exploits a host GPU driver bug can reach the card, and through
//! it other guests. That risk is structural to the native-context design and is
//! not going away. What a filter buys is a bound on what a *compromised device
//! model* can do afterwards: no `execve`, no new files, no ptrace, no arbitrary
//! sockets. It does not prevent an exploit; it decides whether one is a bad day
//! or an incident report, and it costs nothing at runtime.
//!
//! # Filters stack, which is the whole design
//!
//! Every filter installed on a thread must allow a syscall for it to proceed, so
//! filters compose by intersection and can only ever tighten. That lets us do
//! two passes:
//!
//! 1. A **baseline** installed once with `TSYNC`, covering every thread. It is
//!    the union of what the VMM, the GPU worker, the block and net workers and
//!    the metrics thread need — necessarily broad, because it has to hold the
//!    superset.
//! 2. A **vCPU** filter installed by each vCPU thread before it enters
//!    `KVM_RUN`. A vCPU needs almost nothing: it runs the guest and talks to
//!    KVM. It has no business opening files or sockets, and after this it cannot.
//!
//! crosvm gets stronger isolation than this by running each device in its own
//! *process*, so each gets its own policy. nesbox is one process with threads, so
//! per-thread filters are the closest equivalent — better than a single union,
//! weaker than separate processes. Worth being clear about rather than implying
//! parity.
//!
//! # Where the lists come from
//!
//! Not from reading our own source, which is the way to miss things. The GPU set
//! is derived from crosvm's `jail/seccomp/x86_64/gpu_common.policy`, which is
//! maintained by the people who wrote rutabaga and virglrenderer and covers what
//! Mesa actually does — including things nobody would guess, like
//! `sched_setscheduler` and `kcmp` being needed specifically on AMD, and the
//! `inotify`/`flock`/`rename` family for Mesa's shader cache. The VMM and vCPU
//! sets are derived from Firecracker's `resources/seccomp/x86_64-*.json`.
//!
//! # What this does not yet do
//!
//! **No argument filtering.** Both upstreams constrain `ioctl` to specific
//! request numbers, `mmap`/`mprotect` to specific protection flags, and `prctl`
//! to specific options. That is a real tightening and it is the obvious next
//! step; a syscall-number allowlist is the floor, not the ceiling. Landing the
//! floor first means the mechanism is validated before the policy gets subtle.

use std::io;

/// What to do about a syscall that is not on the list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Install nothing.
    #[default]
    Off,
    /// Report the offending syscall and die.
    ///
    /// This exists because the honest way to build a profile is against a running
    /// VMM, and this host has `dmesg_restrict=1` and no auditd, so
    /// `SECCOMP_RET_LOG` output is unreachable without root. A trap plus a
    /// `SIGSYS` handler gets the same information with no privileges: run,
    /// read the syscall it names, add it, run again.
    Audit,
    /// Kill the process.
    Enforce,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Mode::Off),
            "audit" => Some(Mode::Audit),
            "enforce" => Some(Mode::Enforce),
            _ => None,
        }
    }

    fn action(self) -> u32 {
        match self {
            // Not installed, so the value is never used.
            Mode::Off => SECCOMP_RET_ALLOW,
            Mode::Audit => SECCOMP_RET_TRAP,
            Mode::Enforce => SECCOMP_RET_KILL_PROCESS,
        }
    }
}

// ── BPF, by hand ─────────────────────────────────────────────────────────────
//
// A dependency would be reasonable here. It is not taken because the whole
// installer is this file: a filter is a flat array of 8-byte instructions and the
// install is two syscalls. Security-critical code short enough to read in one
// sitting is worth more than security-critical code someone else audited.

const BPF_LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const BPF_JMP_JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const BPF_RET_K: u16 = 0x06; // BPF_RET | BPF_K

/// `struct seccomp_data`: `nr` at 0, `arch` at 4.
const SECCOMP_DATA_NR: u32 = 0;
const SECCOMP_DATA_ARCH: u32 = 4;

const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;

const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;

const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
const SECCOMP_FILTER_FLAG_TSYNC: libc::c_ulong = 1;

/// The kernel's ceiling. Our filters are an order of magnitude under it.
const BPF_MAX_LEN: usize = 4096;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

/// Build an allowlist filter.
///
/// Two instructions per syscall — compare, and return allow on the next line —
/// rather than jumping to a shared allow at the end. It is one instruction
/// larger per rule and it means **no jump offset ever exceeds 1**, so the program
/// cannot be silently wrong for a long list. Jump offsets are a single byte, and
/// a list of 120 syscalls is exactly where hand-computed offsets overflow.
fn build(allowed: &[libc::c_long], mismatch: u32) -> Vec<SockFilter> {
    let mut p = Vec::with_capacity(allowed.len() * 2 + 4);

    // Refuse anything that is not the architecture we compiled these numbers
    // for. Syscall numbers are per-ABI, so a 32-bit call arriving here would be
    // matched against the wrong table -- the classic seccomp bypass.
    p.push(SockFilter {
        code: BPF_LD_W_ABS,
        jt: 0,
        jf: 0,
        k: SECCOMP_DATA_ARCH,
    });
    p.push(SockFilter {
        code: BPF_JMP_JEQ_K,
        jt: 1,
        jf: 0,
        k: AUDIT_ARCH_X86_64,
    });
    p.push(SockFilter {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k: mismatch,
    });

    p.push(SockFilter {
        code: BPF_LD_W_ABS,
        jt: 0,
        jf: 0,
        k: SECCOMP_DATA_NR,
    });
    for &nr in allowed {
        p.push(SockFilter {
            code: BPF_JMP_JEQ_K,
            jt: 0,
            jf: 1,
            k: nr as u32,
        });
        p.push(SockFilter {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ALLOW,
        });
    }
    p.push(SockFilter {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k: mismatch,
    });
    p
}

/// Install a filter on the calling thread, or on every thread with `all_threads`.
fn install(prog: &[SockFilter], all_threads: bool) -> io::Result<()> {
    if prog.len() > BPF_MAX_LEN {
        return Err(io::Error::other("seccomp filter too large"));
    }
    let len = u16::try_from(prog.len()).map_err(io::Error::other)?;

    // Required before installing a filter without CAP_SYS_ADMIN, and worth
    // having regardless: it stops a setuid binary from regaining privilege, which
    // is the other half of confining a process that should never exec anything.
    // SAFETY: prctl with a constant option and no pointer arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let fprog = SockFprog {
        len,
        filter: prog.as_ptr(),
    };
    let flags = if all_threads {
        SECCOMP_FILTER_FLAG_TSYNC
    } else {
        0
    };

    // SAFETY: `fprog` points at `prog`, which outlives this call, and `len`
    // matches its length. The kernel copies the program.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            flags,
            &fprog as *const SockFprog,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ── Audit mode ───────────────────────────────────────────────────────────────

/// Report the syscall that was refused, then die.
///
/// Everything here must be async-signal-safe, so it formats by hand and uses
/// `write` directly rather than any of the machinery that would allocate or take
/// a lock.
extern "C" fn on_sigsys(_sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // `siginfo_t` for SIGSYS is `_sigsys { void *call_addr; int syscall;
    // unsigned arch; }`, and the union begins at offset 16 on x86_64, so
    // `si_syscall` sits at 24. libc does not expose it.
    let nr = if info.is_null() {
        -1
    } else {
        // SAFETY: the kernel guarantees a full siginfo_t for SIGSYS.
        unsafe { *(info.cast::<u8>().add(24).cast::<i32>()) }
    };
    // SAFETY: gettid is on both policies and takes no arguments.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };

    // Written as separate pieces rather than patched into one buffer at computed
    // offsets. The offset version was wrong on the first run and printed
    // "NNNN00317add", which is a silly way to lose an afternoon.
    say(b"nesbox: seccomp refused syscall ");
    say_num(nr as i64);
    say(b" on thread ");
    say_num(tid);
    say(b" -- add it to the policy in vmm/src/seccomp.rs\n");

    // SAFETY: exiting without unwinding, which is all that is safe here.
    unsafe { libc::syscall(libc::SYS_exit_group, 1) };
}

/// `write(2)` on stderr. The only output primitive that is async-signal-safe.
fn say(bytes: &[u8]) {
    // SAFETY: writing a borrowed slice with its own length.
    unsafe { libc::write(2, bytes.as_ptr().cast(), bytes.len()) };
}

fn say_num(mut n: i64) {
    if n < 0 {
        say(b"-");
        n = -n;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    say(&buf[i..]);
}

fn install_sigsys_reporter() -> io::Result<()> {
    // SAFETY: zeroed sigaction is valid; we then set the fields we need.
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = on_sigsys as *const () as usize;
    sa.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER;
    // SAFETY: `sa` is initialised and outlives the call.
    if unsafe { libc::sigaction(libc::SIGSYS, &sa, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ── Policies ─────────────────────────────────────────────────────────────────

/// Everything any thread in the process may need.
///
/// Broad by necessity: it has to hold the union, and the GPU worker drags in most
/// of Mesa. The point is not that this list is small, it is that `execve`,
/// `ptrace`, `mount`, `bpf`, `kexec_load`, `init_module` and every other way to
/// change the host are not on it.
fn baseline() -> Vec<libc::c_long> {
    let mut v = vec![
        // Process and thread lifetime. `clone` is needed because device workers
        // and the metrics thread start after this filter is installed.
        libc::SYS_clone,
        libc::SYS_clone3,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_futex,
        libc::SYS_set_robust_list,
        libc::SYS_rseq,
        libc::SYS_gettid,
        libc::SYS_getpid,
        libc::SYS_sched_yield,
        libc::SYS_membarrier,
        libc::SYS_restart_syscall,
        // Signals.
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_tgkill,
        libc::SYS_kill,
        // Reaping virtiofsd. `Virtiofsd::drop` kills each daemon and waits for
        // it, and that happens on the way out -- long after this filter is
        // installed. Without it every clean shutdown of a VM with a shared
        // directory dies of SIGSYS instead: the sockets are never removed, the
        // exit code is a signal rather than the reason the VM stopped, and the
        // one signal that should mean "a device model was compromised" fires on
        // every ordinary teardown. `Child::wait` reaches `wait4`, not `waitid`.
        libc::SYS_wait4,
        // Memory. Guest RAM, the BAR2 window and every Mesa allocation.
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_mprotect,
        libc::SYS_madvise,
        libc::SYS_brk,
        libc::SYS_mincore,
        libc::SYS_msync,
        libc::SYS_memfd_create,
        libc::SYS_ftruncate,
        libc::SYS_fallocate,
        // Descriptors.
        libc::SYS_close,
        libc::SYS_dup,
        libc::SYS_dup2,
        libc::SYS_dup3,
        libc::SYS_fcntl,
        libc::SYS_lseek,
        libc::SYS_pipe2,
        libc::SYS_flock,
        // I/O. Disk images, the console, the guest's virtio queues.
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_fsync,
        libc::SYS_fdatasync,
        // Waiting.
        libc::SYS_epoll_create1,
        libc::SYS_epoll_ctl,
        libc::SYS_epoll_wait,
        libc::SYS_epoll_pwait,
        libc::SYS_ppoll,
        libc::SYS_poll,
        libc::SYS_eventfd2,
        libc::SYS_timerfd_create,
        libc::SYS_timerfd_settime,
        libc::SYS_nanosleep,
        libc::SYS_clock_nanosleep,
        // The one that matters most, and the one we cannot yet constrain: KVM,
        // DRM, DMA-BUF and every virtio queue notification arrive as ioctl.
        libc::SYS_ioctl,
        // Paths. The DRM render node, /proc/self/fdinfo for occupancy, Mesa's
        // shader cache, the disk image. Opening is allowed; note that this is
        // exactly what the vCPU filter below takes away.
        libc::SYS_openat,
        libc::SYS_open,
        libc::SYS_statx,
        libc::SYS_newfstatat,
        libc::SYS_fstat,
        libc::SYS_stat,
        libc::SYS_lstat,
        libc::SYS_fstatfs,
        libc::SYS_getdents64,
        libc::SYS_readlink,
        libc::SYS_readlinkat,
        libc::SYS_access,
        libc::SYS_getcwd,
        libc::SYS_unlink,
        libc::SYS_unlinkat,
        libc::SYS_rename,
        libc::SYS_mkdir,
        libc::SYS_fchmod,
        libc::SYS_fchmodat,
        // Removing the per-VM runtime directory at shutdown. `std::fs::remove_dir`
        // reaches `rmdir`, not `unlinkat(AT_REMOVEDIR)`, so having the latter is
        // not enough -- found by running the teardown under `audit`, which named
        // syscall 84 rather than leaving a bare SIGSYS to guess at.
        libc::SYS_rmdir,
        // Mesa's shader cache watches its directory.
        libc::SYS_inotify_init1,
        libc::SYS_inotify_add_watch,
        libc::SYS_inotify_rm_watch,
        // The metrics socket, and Mesa talking to a compositor.
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept4,
        libc::SYS_getsockopt,
        libc::SYS_setsockopt,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_shutdown,
        // Time and identity.
        libc::SYS_clock_gettime,
        libc::SYS_gettimeofday,
        libc::SYS_getrandom,
        libc::SYS_uname,
        libc::SYS_sysinfo,
        libc::SYS_getuid,
        libc::SYS_geteuid,
        libc::SYS_getgid,
        libc::SYS_getegid,
        // Placement. vCPU pinning, and -- specifically on AMD, per crosvm --
        // Mesa's own scheduling calls.
        libc::SYS_sched_getaffinity,
        libc::SYS_sched_setaffinity,
        libc::SYS_sched_setscheduler,
        libc::SYS_setpriority,
        libc::SYS_kcmp,
        // prctl is needed for PR_SET_NAME on our own threads. Unconstrained for
        // now, which is one of the arguments this filter should grow.
        libc::SYS_prctl,
        libc::SYS_arch_prctl,
        // Allowing `seccomp` looks wrong in a hardening policy and is not.
        // Filters are **monotonic**: a thread can add one, and every filter on a
        // thread must allow a syscall for it to proceed, so an added filter can
        // only ever remove. With NO_NEW_PRIVS set and no CAP_SYS_ADMIN there is
        // no way to relax or remove one. The worst a compromised thread achieves
        // is confining itself further.
        //
        // It is needed because the vCPU threads install their tighter filter
        // themselves -- there is no way to do that for another thread. The
        // alternative, installing the vCPU filter before the baseline, fails for
        // a subtler reason: the GPU worker is spawned by device activation, which
        // runs on a vCPU thread in response to a guest MMIO write, so it would
        // inherit the vCPU filter and immediately die.
        libc::SYS_seccomp,
    ];
    v.sort_unstable();
    v.dedup();
    v
}

/// What a vCPU thread needs, which is startlingly little.
///
/// It enters `KVM_RUN` and stays there, returning to service MMIO and PIO
/// against device models already built. It never opens a path, never touches the
/// network, never creates a thread. Derived from Firecracker's `vcpu` filter.
fn vcpu() -> Vec<libc::c_long> {
    let mut v = vec![
        libc::SYS_ioctl,
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_futex,
        libc::SYS_sched_yield,
        libc::SYS_membarrier,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_tgkill,
        libc::SYS_gettid,
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mprotect,
        libc::SYS_madvise,
        libc::SYS_brk,
        libc::SYS_mremap,
        libc::SYS_close,
        libc::SYS_fcntl,
        libc::SYS_eventfd2,
        // Firecracker's vcpu filter carries these too: a vCPU servicing an MMIO
        // write can end up notifying a device backend over a socket.
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_epoll_ctl,
        libc::SYS_epoll_pwait,
        libc::SYS_epoll_wait,
        libc::SYS_ppoll,
        libc::SYS_poll,
        libc::SYS_clock_gettime,
        libc::SYS_clock_nanosleep,
        libc::SYS_nanosleep,
        libc::SYS_timerfd_settime,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_restart_syscall,
        libc::SYS_rseq,
        libc::SYS_set_robust_list,
        libc::SYS_sched_setaffinity,
        libc::SYS_prctl,
    ];
    v.sort_unstable();
    v.dedup();
    v
}

// ── Entry points ─────────────────────────────────────────────────────────────

/// Install the baseline on every thread. Call once, after setup is complete:
/// after virtiofsd has been spawned, since `execve` is not on the list.
pub fn apply_baseline(mode: Mode) -> io::Result<()> {
    if mode == Mode::Off {
        log::warn!("seccomp: disabled -- the VMM is unconfined");
        return Ok(());
    }
    if mode == Mode::Audit {
        install_sigsys_reporter()?;
    }
    let list = baseline();
    let prog = build(&list, mode.action());
    install(&prog, true)?;
    log::info!(
        "seccomp: baseline installed on all threads, {} syscalls allowed, mode {:?}",
        list.len(),
        mode
    );
    Ok(())
}

/// Install the tighter vCPU filter on the calling thread. Stacks on the
/// baseline, so it can only remove.
pub fn apply_vcpu(mode: Mode) -> io::Result<()> {
    if mode == Mode::Off {
        return Ok(());
    }
    let prog = build(&vcpu(), mode.action());
    install(&prog, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_program_is_well_formed_and_small() {
        let list = baseline();
        let prog = build(&list, SECCOMP_RET_KILL_PROCESS);
        // 2 per syscall, plus arch check (3) and the final mismatch return.
        assert_eq!(prog.len(), list.len() * 2 + 5);
        assert!(prog.len() < BPF_MAX_LEN, "{} instructions", prog.len());
        // Every jump offset must fit a byte with room to spare -- the reason for
        // the two-instruction-per-rule shape.
        assert!(prog.iter().all(|i| i.jt <= 1 && i.jf <= 1));
    }

    #[test]
    fn the_first_thing_checked_is_the_architecture() {
        let prog = build(&[libc::SYS_read], SECCOMP_RET_KILL_PROCESS);
        assert_eq!(prog[0].code, BPF_LD_W_ABS);
        assert_eq!(prog[0].k, SECCOMP_DATA_ARCH);
        assert_eq!(prog[1].k, AUDIT_ARCH_X86_64);
        // A mismatched architecture must fall into the deny return, not skip it.
        assert_eq!(prog[2].code, BPF_RET_K);
        assert_eq!(prog[2].k, SECCOMP_RET_KILL_PROCESS);
    }

    #[test]
    fn the_program_ends_in_a_denial() {
        // The failure mode that matters: a filter that falls through to allow
        // would look identical in every other test.
        let prog = build(&[libc::SYS_read], SECCOMP_RET_KILL_PROCESS);
        let last = prog.last().unwrap();
        assert_eq!(last.code, BPF_RET_K);
        assert_eq!(last.k, SECCOMP_RET_KILL_PROCESS);
    }

    #[test]
    fn the_vcpu_filter_is_a_subset_of_the_baseline() {
        // It stacks on the baseline, so anything it allows that the baseline
        // denies is dead weight and a lie about what a vCPU can do.
        let base = baseline();
        for nr in vcpu() {
            assert!(
                base.contains(&nr),
                "vcpu allows {nr} which the baseline denies"
            );
        }
    }

    #[test]
    fn a_vcpu_cannot_reach_the_filesystem_or_the_network() {
        // The point of the second filter. If these ever appear, something moved
        // work onto the vCPU thread that does not belong there.
        let v = vcpu();
        for nr in [
            libc::SYS_openat,
            libc::SYS_open,
            libc::SYS_socket,
            libc::SYS_connect,
            libc::SYS_clone,
            libc::SYS_execve,
            libc::SYS_getdents64,
        ] {
            assert!(!v.contains(&nr), "vcpu should not allow {nr}");
        }
    }

    #[test]
    fn nothing_that_reconfigures_the_host_is_allowed_anywhere() {
        let base = baseline();
        for nr in [
            libc::SYS_execve,
            libc::SYS_execveat,
            libc::SYS_ptrace,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_bpf,
            libc::SYS_kexec_load,
            libc::SYS_init_module,
            libc::SYS_finit_module,
            libc::SYS_delete_module,
            libc::SYS_pivot_root,
            libc::SYS_chroot,
            libc::SYS_setuid,
            libc::SYS_setgid,
            libc::SYS_reboot,
            libc::SYS_swapon,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_userfaultfd,
        ] {
            assert!(!base.contains(&nr), "baseline must not allow {nr}");
        }
    }

    /// `seccomp` is allowed on purpose, and the vCPU filter is why. If this ever
    /// flips, the vCPU threads silently die at startup.
    #[test]
    fn the_baseline_allows_installing_a_tighter_filter() {
        assert!(baseline().contains(&libc::SYS_seccomp));
        // And a thread that has tightened cannot tighten again, so the
        // capability does not propagate.
        assert!(!vcpu().contains(&libc::SYS_seccomp));
    }

    /// Reaping a child has to survive the filter, because `Virtiofsd::drop`
    /// does it on the way out -- after `apply_baseline`.
    ///
    /// A fork rather than `assert!(baseline().contains(&SYS_wait4))`, because
    /// the list is not the claim. The claim is that the shutdown path works,
    /// and which syscall `Child::wait` reaches is a std implementation detail
    /// (measured: `wait4`, not `waitid`). `wait4` is called directly here only
    /// so the child stays async-signal-safe after `fork`.
    #[test]
    fn a_child_can_still_be_reaped_under_the_baseline() {
        // SAFETY: the child calls only async-signal-safe functions.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // SAFETY: as above; the grandchild exits immediately.
            unsafe {
                let kid = libc::fork();
                if kid == 0 {
                    libc::syscall(libc::SYS_exit_group, 7);
                    unreachable!();
                }
                let prog = build(&baseline(), SECCOMP_RET_KILL_PROCESS);
                if install(&prog, false).is_err() {
                    libc::syscall(libc::SYS_exit_group, 42);
                }
                let mut st: libc::c_int = 0;
                let seen = libc::syscall(
                    libc::SYS_wait4,
                    kid as libc::c_long,
                    &mut st as *mut libc::c_int,
                    0 as libc::c_long,
                    std::ptr::null_mut::<libc::c_void>(),
                );
                let ok = seen == kid as i64 && libc::WEXITSTATUS(st) == 7;
                libc::syscall(libc::SYS_exit_group, if ok { 0 } else { 43 });
                unreachable!();
            }
        }
        let mut status = 0;
        // SAFETY: waiting on our own child.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "a VM with a shared directory must be able to reap virtiofsd at \
             shutdown; status {status:#x}"
        );
    }

    #[test]
    fn modes_parse_and_map_to_distinct_actions() {
        assert_eq!(Mode::parse("enforce"), Some(Mode::Enforce));
        assert_eq!(Mode::parse("audit"), Some(Mode::Audit));
        assert_eq!(Mode::parse("off"), Some(Mode::Off));
        assert_eq!(Mode::parse("Enforce"), None, "no silent case-folding");
        assert_eq!(Mode::parse("permissive"), None);
        assert_eq!(Mode::Enforce.action(), SECCOMP_RET_KILL_PROCESS);
        assert_eq!(Mode::Audit.action(), SECCOMP_RET_TRAP);
    }

    /// The filter really is installed and really does refuse. Runs in a forked
    /// child so it cannot confine the test process.
    #[test]
    fn an_installed_filter_actually_kills_on_a_denied_syscall() {
        // SAFETY: the child only calls async-signal-safe functions before exec-less exit.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // Allow only what is needed to exit, then attempt something denied.
            let prog = build(&[libc::SYS_exit_group], SECCOMP_RET_KILL_PROCESS);
            if install(&prog, false).is_err() {
                unsafe { libc::syscall(libc::SYS_exit_group, 42) };
            }
            // getpid is not on the list.
            unsafe {
                libc::syscall(libc::SYS_getpid);
                libc::syscall(libc::SYS_exit_group, 0)
            };
            unreachable!();
        }
        let mut status = 0;
        // SAFETY: waiting on our own child.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(
            libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSYS,
            "child should have died of SIGSYS, status {status:#x}"
        );
    }

    /// Audit mode has to actually report, or it is worse than useless: a run
    /// that dies with no message looks like a crash somewhere else entirely.
    #[test]
    fn audit_mode_reports_and_exits_rather_than_dying_of_the_signal() {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            if install_sigsys_reporter().is_err() {
                unsafe { libc::syscall(libc::SYS_exit_group, 42) };
            }
            let prog = build(
                &[libc::SYS_exit_group, libc::SYS_write, libc::SYS_gettid],
                SECCOMP_RET_TRAP,
            );
            if install(&prog, false).is_err() {
                unsafe { libc::syscall(libc::SYS_exit_group, 43) };
            }
            unsafe {
                libc::syscall(libc::SYS_getpid);
                libc::syscall(libc::SYS_exit_group, 0)
            };
            unreachable!();
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 1,
            "the handler should have reported and exited 1, not died of SIGSYS; \
             status {status:#x}"
        );
    }

    /// And the allowlist does not accidentally deny what it names.
    #[test]
    fn an_allowed_syscall_still_works_under_the_filter() {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            let prog = build(
                &[libc::SYS_getpid, libc::SYS_exit_group],
                SECCOMP_RET_KILL_PROCESS,
            );
            if install(&prog, false).is_err() {
                unsafe { libc::syscall(libc::SYS_exit_group, 42) };
            }
            let got = unsafe { libc::syscall(libc::SYS_getpid) };
            unsafe { libc::syscall(libc::SYS_exit_group, if got > 0 { 0 } else { 1 }) };
            unreachable!();
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "child should have exited cleanly, status {status:#x}"
        );
    }
}

#[cfg(test)]
mod nr_names {
    /// Kept as a test so the number in an audit message can be mapped back
    /// without leaving the repo. Add lines as the policy grows.
    #[test]
    fn print_numbers() {
        for (nr, name) in [
            (libc::SYS_sendmsg, "sendmsg"),
            (libc::SYS_recvmsg, "recvmsg"),
            (libc::SYS_sendto, "sendto"),
            (libc::SYS_recvfrom, "recvfrom"),
            (libc::SYS_getsockname, "getsockname"),
            (libc::SYS_getpeername, "getpeername"),
            (libc::SYS_socketpair, "socketpair"),
        ] {
            println!("{nr:5} {name}");
        }
    }
}
