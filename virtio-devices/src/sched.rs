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
//! the process has neither, the thread stays at normal priority and one warning
//! says how to change that, in the same spirit as the advice about isolating
//! CPUs: the box runs either way, and this says what it is leaving on the table.
//!
//! It is a *nice* boost and not a realtime class on purpose. The GPU worker
//! spins for a short window after each wake, and a realtime thread that spins
//! on a CPU it shares would starve the threads beside it rather than merely
//! outrank them.
//!
//! The priority applies to the calling thread only, which is why each thread
//! calls it on itself: a nice value is inherited by threads a thread spawns, and
//! the GPU worker is spawned from a vCPU thread that must not be affected.

use std::sync::atomic::{AtomicBool, Ordering};

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

/// Make the calling thread a helper: it yields to the critical thread whenever
/// they contend. Needs no privilege.
pub fn lower_this_thread(role: &str) {
    if let Err(e) = set_nice(HELPER_NICE) {
        log::debug!("{role}: could not lower its own priority: {e}");
    }
}

/// Raise the calling thread above ordinary ones, if this process may. Returns
/// whether it did. A refusal is reported once per process, whichever thread asks.
pub fn raise_this_thread(role: &str) -> bool {
    match set_nice(CRITICAL_NICE) {
        Ok(()) => {
            log::info!("{role}: priority raised to nice {CRITICAL_NICE}");
            true
        }
        Err(e) => {
            static TOLD: AtomicBool = AtomicBool::new(false);
            if !TOLD.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "{role}: running at normal priority ({e}). With CAP_SYS_NICE \
                     (setcap cap_sys_nice+ep on the binary, or AmbientCapabilities= in \
                     a service) it would run ahead of the box's other threads, which \
                     matters when they share a CPU"
                );
            }
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
