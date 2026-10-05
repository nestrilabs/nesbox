//! Who runs first when the threads that are not vCPUs have to share CPUs.
//!
//! A box's non-vCPU threads -- the GPU worker, block workers, the console, the
//! metrics and control sockets -- usually share one or two CPUs, and when they
//! contend the order matters: the GPU worker is on the path of every frame, and
//! the others are not. So the ones that are not are lowered, and the one that is
//! can be raised.
//!
//! # Two halves, with different requirements
//!
//! **Lowering needs nothing.** Any thread may make itself nicer, so the helpers
//! always do. They still get the CPU whenever the critical thread is asleep,
//! which is most of the time; they only lose when both want it at once.
//!
//! **Raising needs `CAP_SYS_NICE`** (or an `RLIMIT_NICE` that allows it). When
//! the process has neither, the thread stays at normal priority. This module
//! does not say so: whoever starts the VMM knows what it was given and is the
//! one to tell an operator, once, instead of once per box.
//!
//! It is a *nice* boost and not a realtime class on purpose. The GPU worker
//! spins for a short window after each wake, and a realtime thread that spins
//! on a CPU it shares would starve the threads beside it rather than merely
//! outrank them.
//!
//! The priority applies to the calling thread only, which is why each thread
//! calls it on itself: a nice value is inherited by threads a thread spawns, and
//! the GPU worker is spawned from a vCPU thread that must not be affected.

/// How much nicer the non-critical helper threads make themselves.
pub const HELPER_NICE: i32 = 10;

/// What the critical thread asks for when it is allowed to. About three times
/// the weight of an ordinary thread, which is enough to be picked first on a
/// wake without being able to starve anything.
pub const CRITICAL_NICE: i32 = -5;

fn current_tid() -> libc::id_t {
    // SAFETY: gettid has no preconditions and cannot fail.
    unsafe { libc::syscall(libc::SYS_gettid) as libc::id_t }
}

fn set_nice(nice: i32) -> std::io::Result<()> {
    // SAFETY: FFI call; on Linux the id is a thread id, and this is our own.
    let ret = unsafe { libc::setpriority(libc::PRIO_PROCESS, current_tid(), nice) };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// This thread's nice value, or `None` if it cannot be read.
pub fn current_nice() -> Option<i32> {
    // getpriority can legitimately return -1, so errno is the only signal.
    // SAFETY: errno is thread-local and the call has no other effect.
    unsafe {
        *libc::__errno_location() = 0;
        let nice = libc::getpriority(libc::PRIO_PROCESS, current_tid());
        (*libc::__errno_location() == 0).then_some(nice)
    }
}

/// The kernel's `struct sched_attr`, as of the version that carries the
/// utilisation clamps. The syscall takes the size, so older kernels accept it.
#[repr(C)]
#[derive(Default)]
struct SchedAttr {
    size: u32,
    policy: u32,
    flags: u64,
    nice: i32,
    priority: u32,
    runtime: u64,
    deadline: u64,
    period: u64,
    util_min: u32,
    util_max: u32,
}

/// Ask the scheduler for a shorter slice, in microseconds, for the calling
/// thread. Needs no privilege, and takes no CPU share from anyone.
///
/// A fair-class thread's slice sets how soon after waking it is picked: the
/// deadline it is queued with is its slice past its eligible time, so a short
/// one is chosen ahead of threads holding the usual one. That is the part of
/// "run first" a thread can ask for without outranking anything -- the GPU
/// worker wakes tens of thousands of times a second and wants the CPU the
/// moment it does, not a share of it. Kernels that do not read the field
/// ignore it and the call still succeeds, so [`current_slice_us`] is how to
/// tell whether it took.
pub fn request_slice(role: &str, slice_us: u64) {
    // `sched_setattr` replaces the whole attribute set, nice value included,
    // so start from what the thread has and change only the slice. Read with
    // `sched_getattr` rather than `getpriority`, which the filter this process
    // runs under does not allow.
    let Some(mut attr) = read_attr() else {
        log::debug!("{role}: could not read its scheduling attributes");
        return;
    };
    attr.runtime = slice_us * 1000;
    // SAFETY: FFI call; pid 0 is the calling thread, and `attr` is a live
    // `sched_attr` whose size field the kernel filled in.
    let ret = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, &attr as *const SchedAttr, 0) };
    if ret == 0 {
        log::debug!("{role}: asked for a {slice_us} us slice");
    } else {
        log::debug!(
            "{role}: could not ask for a shorter slice: {}",
            std::io::Error::last_os_error()
        );
    }
}

fn read_attr() -> Option<SchedAttr> {
    let mut attr = SchedAttr::default();
    // SAFETY: FFI call; pid 0 is the calling thread, and `attr` is writable
    // for the size passed.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_sched_getattr,
            0,
            &mut attr as *mut SchedAttr,
            std::mem::size_of::<SchedAttr>() as u32,
            0,
        )
    };
    (ret == 0).then_some(attr)
}

/// The slice the calling thread runs with, in microseconds: what it asked for,
/// or the kernel's default if it has not. `None` where the kernel does not
/// report one.
pub fn current_slice_us() -> Option<u64> {
    read_attr()
        .map(|attr| attr.runtime)
        .filter(|&runtime| runtime > 0)
        .map(|runtime| runtime / 1000)
}

/// Make the calling thread a helper: it yields to the critical thread whenever
/// they contend. Needs no privilege.
pub fn lower_this_thread(role: &str) {
    if let Err(e) = set_nice(HELPER_NICE) {
        log::debug!("{role}: could not lower its own priority: {e}");
    }
}

/// Raise the calling thread above ordinary ones, if this process may. Returns
/// whether it did. A refusal is not an error and is not reported here.
pub fn raise_this_thread(role: &str) -> bool {
    match set_nice(CRITICAL_NICE) {
        Ok(()) => {
            log::debug!("{role}: priority raised to nice {CRITICAL_NICE}");
            true
        }
        Err(e) => {
            log::debug!("{role}: stays at normal priority ({e})");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lowering works for anyone, and only for the thread that asked.
    #[test]
    fn a_thread_can_lower_itself_without_privilege() {
        let seen = std::thread::spawn(|| {
            lower_this_thread("test");
            current_nice()
        })
        .join()
        .unwrap();
        assert_eq!(seen, Some(HELPER_NICE));
        // The thread that spawned it was not touched.
        assert!(current_nice().is_some_and(|n| n < HELPER_NICE));
    }

    /// A slice request is accepted without privilege and read back where the
    /// kernel reports it; where it does not, the call still returns.
    #[test]
    fn a_slice_can_be_asked_for_without_privilege() {
        let seen = std::thread::spawn(|| {
            request_slice("test", 200);
            current_slice_us()
        })
        .join()
        .unwrap();
        if let Some(us) = seen {
            assert_eq!(us, 200);
        }
        assert_ne!(current_slice_us(), Some(200), "the spawning thread was not touched");
    }

    /// Without the capability a raise is refused and reported, never fatal.
    /// With it, it succeeds; either way the call returns.
    #[test]
    fn raising_is_refused_politely_without_the_capability() {
        let (raised, nice) = std::thread::spawn(|| (raise_this_thread("test"), current_nice()))
            .join()
            .unwrap();
        if raised {
            assert_eq!(nice, Some(CRITICAL_NICE));
        } else {
            assert!(nice.is_some_and(|n| n >= 0));
        }
    }
}
