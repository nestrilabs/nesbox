//! Where the threads that are not vCPUs run.
//!
//! A thread inherits the CPU affinity of the thread that created it, and the
//! kernel's vhost workers inherit it from whichever thread asked for them.
//! Several devices here are activated by the guest's own write to them, which
//! runs on a vCPU thread -- so without care, a worker serving the guest is
//! born pinned to one of the guest's CPUs and halves it.
//!
//! One set for the whole process, held here rather than threaded through every
//! device's config: one VMM process serves one guest, so process scope is guest
//! scope, and the set is decided once before any device exists.

use std::sync::OnceLock;

static IO_CPUS: OnceLock<Vec<usize>> = OnceLock::new();

/// Descriptors on `cgroup.threads` of the vCPU cgroup and of the I/O one,
/// when the pinned CPUs are a cpuset partition that threads must join. See
/// `MachineConfig::vcpu_cgroup_fd`.
#[derive(Clone, Copy, Debug)]
pub struct CgroupFds {
    pub vcpu: i32,
    pub io: i32,
}

static CGROUP_FDS: OnceLock<CgroupFds> = OnceLock::new();

/// Record the cgroup descriptors. Called once, before any device is built.
pub fn set_cgroup_fds(fds: CgroupFds) {
    let _ = CGROUP_FDS.set(fds);
}

/// Record where the non-vCPU threads go. Called once, before any device is
/// built; a second call is ignored. Empty means no affinity.
pub fn set_io_cpus(cpus: &[usize]) {
    let _ = IO_CPUS.set(cpus.to_vec());
}

/// The set recorded by [`set_io_cpus`], or empty.
pub fn io_cpus() -> &'static [usize] {
    IO_CPUS.get().map_or(&[], Vec::as_slice)
}

/// A `cpu_set_t` holding `cpus`, or `None` if none of them can be named.
///
/// `None` rather than an empty set: an empty set is not "no restriction", it
/// is "no CPU at all", and `sched_setaffinity` rejects it. CPUs past
/// `CPU_SETSIZE` are dropped, because `CPU_SET` would write past the set.
pub fn cpu_set(cpus: &[usize]) -> Option<libc::cpu_set_t> {
    // SAFETY: all-zeros is a valid cpu_set_t; CPU_ZERO makes it explicit.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe { libc::CPU_ZERO(&mut set) };
    let mut named = 0usize;
    for &cpu in cpus {
        if cpu < libc::CPU_SETSIZE as usize {
            // SAFETY: FFI call, index bounds checked above.
            unsafe { libc::CPU_SET(cpu, &mut set) };
            named += 1;
        }
    }
    (named > 0).then_some(set)
}

/// Confine the calling thread to `set`.
pub fn apply(set: &libc::cpu_set_t) -> std::io::Result<()> {
    // SAFETY: FFI call; pid 0 is the calling thread and the size matches.
    let ret = unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), set) };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// The calling thread's current affinity.
pub fn current() -> std::io::Result<libc::cpu_set_t> {
    // SAFETY: all-zeros is a valid cpu_set_t for the kernel to fill.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: FFI call; pid 0 is the calling thread and the size matches.
    let ret =
        unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) };
    if ret == 0 {
        Ok(set)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Move the calling thread into the cgroup whose `cgroup.threads` is open on
/// `fd`. Writing `0` names the writer itself.
pub fn join_cgroup(fd: i32) -> std::io::Result<()> {
    move_to_cgroup(fd, 0)
}

/// Leave the vCPU cgroup, for a long-lived worker a vCPU thread spawned.
///
/// Such a thread is born inside the partition, on its parent vCPU's pin, and
/// no affinity outside the partition can be set until it leaves. Left there,
/// it shares one CPU with a vCPU that, dedicated, never gives the CPU back
/// by halting -- so the worker runs only when the scheduler takes the CPU
/// from the guest. A no-op when there is no partition.
pub fn leave_vcpu_cgroup(what: &str) {
    if let Some(fds) = CGROUP_FDS.get()
        && let Err(e) = join_cgroup(fds.io)
    {
        log::warn!("{what}: could not leave the vCPU cgroup: {e}");
    }
}

/// Move thread `tid` of this process into the cgroup open on `fd`.
fn move_to_cgroup(fd: i32, tid: libc::pid_t) -> std::io::Result<()> {
    let text = tid.to_string();
    // SAFETY: FFI call on a descriptor the caller vouches for, with a buffer
    // that outlives it.
    let n = unsafe { libc::write(fd, text.as_ptr().cast(), text.len()) };
    if n == text.len() as isize {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Run `f` with the calling thread moved to the I/O set, then put it back.
///
/// For the step that creates a worker from inside a vCPU thread, whether a
/// kernel worker or a thread of ours: the worker takes the affinity the thread
/// has at that moment, and keeps it.
/// With no I/O set recorded, `f` runs where the thread already is.
///
/// Under a cpuset partition the worker also takes the thread's cgroup, so the
/// thread steps out to the I/O cgroup for the call and back in afterwards --
/// back in before its pin is restored, because the pin names CPUs only the
/// partition has.
pub fn with_io_affinity<R>(what: &str, f: impl FnOnce() -> R) -> R {
    let Some(io) = cpu_set(io_cpus()) else {
        return f();
    };
    let saved = match current() {
        Ok(saved) => saved,
        Err(e) => {
            log::warn!("{what}: could not read this thread's affinity, so it stays: {e}");
            return f();
        }
    };
    let cgroups = CGROUP_FDS.get().copied();
    if let Some(fds) = cgroups
        && let Err(e) = join_cgroup(fds.io)
    {
        log::warn!("{what}: could not step out to the I/O cgroup: {e}");
    }
    if let Err(e) = apply(&io) {
        log::warn!("{what}: could not move to the I/O CPUs: {e}");
    }
    let result = f();
    if let Some(fds) = cgroups
        && let Err(e) = join_cgroup(fds.vcpu)
    {
        log::warn!("{what}: could not return to the vCPU cgroup: {e}");
    }
    if let Err(e) = apply(&saved) {
        // A vCPU thread left on the I/O CPUs still runs, just in the wrong
        // place, and the guest sees it only as a slower CPU.
        log::warn!("{what}: could not restore this thread's affinity: {e}");
    }
    result
}

/// Move this process's KVM worker threads to the I/O set. Returns how many.
///
/// KVM creates its own workers inside the VMM on the first `KVM_RUN` -- the
/// NX huge page recovery thread, where the host mitigates iTLB multihit --
/// and they are born on the vCPU thread that ran first, so under pins they
/// wake periodically on that vCPU's CPU. They exist only after the guest
/// starts, so this has to be called then, not before.
pub fn rehome_kvm_workers() -> usize {
    let Some(io) = cpu_set(io_cpus()) else {
        return 0;
    };
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    let mut moved = 0;
    for task in tasks.flatten() {
        let comm = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
        if !comm.starts_with("kvm-") {
            continue;
        }
        let Some(tid) = task
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        // Out of the vCPU cgroup first, where it was born: the I/O CPUs are
        // not in it, so the affinity below would be refused.
        if let Some(fds) = CGROUP_FDS.get()
            && let Err(e) = move_to_cgroup(fds.io, tid)
        {
            log::warn!("could not move {} to the I/O cgroup: {e}", comm.trim());
        }
        // SAFETY: FFI call; a tid of this process and a set of matching size.
        let ret =
            unsafe { libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &io) };
        if ret == 0 {
            moved += 1;
        } else {
            log::warn!(
                "could not move {} to the I/O CPUs: {}",
                comm.trim(),
                std::io::Error::last_os_error()
            );
        }
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members(set: &libc::cpu_set_t) -> Vec<usize> {
        (0..libc::CPU_SETSIZE as usize)
            // SAFETY: FFI call, index is within CPU_SETSIZE.
            .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, set) })
            .collect()
    }

    #[test]
    fn an_empty_list_is_no_set_rather_than_no_cpus() {
        assert!(cpu_set(&[]).is_none());
        assert!(cpu_set(&[usize::MAX]).is_none(), "nothing nameable");
    }

    #[test]
    fn cpus_past_the_set_size_are_dropped_and_the_rest_kept() {
        let set = cpu_set(&[1, 3, usize::MAX]).expect("two nameable");
        assert_eq!(members(&set), vec![1, 3]);
    }

    /// What `with_io_affinity` relies on: an affinity read with `current` and
    /// set again with `apply` puts a thread back exactly where it was.
    #[test]
    fn a_thread_is_put_back_where_it_was() {
        std::thread::spawn(|| {
            let before = members(&current().expect("readable"));
            let first = before[0];
            // Stands in for a recorded I/O set without touching the process-wide
            // one, which other tests share.
            let only_first = cpu_set(&[first]).expect("nameable");
            let saved = current().expect("readable");
            apply(&only_first).expect("a CPU this thread may already use");
            assert_eq!(members(&current().expect("readable")), vec![first]);
            apply(&saved).expect("restorable");
            assert_eq!(members(&current().expect("readable")), before);
        })
        .join()
        .expect("thread ran");
    }

    /// The other half: a thread spawned while its parent is on the I/O set
    /// keeps that set after the parent is put back. This is what makes
    /// spawning inside `with_io_affinity` place a worker a vCPU starts.
    #[test]
    fn a_thread_spawned_while_confined_stays_confined() {
        std::thread::spawn(|| {
            let saved = current().expect("readable");
            let first = members(&saved)[0];
            apply(&cpu_set(&[first]).expect("nameable")).expect("a CPU this thread may use");
            let (tx, rx) = std::sync::mpsc::channel();
            let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
            let child = std::thread::spawn(move || {
                go_rx.recv().unwrap();
                tx.send(members(&current().expect("readable"))).unwrap();
            });
            apply(&saved).expect("restorable");
            go_tx.send(()).unwrap();
            assert_eq!(
                rx.recv().unwrap(),
                vec![first],
                "the child kept its birth set"
            );
            child.join().unwrap();
        })
        .join()
        .expect("thread ran");
    }
}
