//! The two ways this device reaches the disk.
//!
//! [`UringEngine`] is the one that matters: requests are handed to `io_uring`
//! and completed out of order, so a guest that submits 128 deep gets 128 in
//! flight instead of one at a time. The queue's kick eventfd is armed *inside*
//! the ring, so one `io_uring_enter` waits for both new work and finished work
//! and an idle disk still costs nothing.
//!
//! [`SyncEngine`] exists because `io_uring` is not always available -- an older
//! kernel, or the increasingly common `kernel.io_uring_disabled=2` -- and a VMM
//! that will not start on such a host is worse than one that runs a request at
//! a time there. It is still positional (`preadv`/`pwritev`) and still writes
//! straight into guest pages, so it keeps the copy and the shared file offset
//! gone even where the depth is not.

use anyhow::{Context, Result};
use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use vmm_sys_util::eventfd::EventFd;

/// A scatter-gather segment.
///
/// `libc::iovec` holds a raw pointer and so is not `Send`, which would keep the
/// whole request slab off a worker thread. The pointer is into guest memory
/// that outlives every worker, and each segment belongs to exactly one slot on
/// one thread, so wrapping it is sound and saying so once here beats scattering
/// the reasoning.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct IoVec(pub libc::iovec);

// SAFETY: see above -- the pointee is guest memory or a bounce buffer owned by
// the slot that holds this segment, and neither is shared across threads.
unsafe impl Send for IoVec {}

impl IoVec {
    pub fn new(base: *mut u8, len: usize) -> Self {
        Self(libc::iovec {
            iov_base: base as *mut libc::c_void,
            iov_len: len,
        })
    }

    pub fn base(&self) -> *mut u8 {
        self.0.iov_base as *mut u8
    }

    pub fn len(&self) -> usize {
        self.0.iov_len
    }

    /// Drop `n` bytes from the front of this segment.
    pub fn advance(&mut self, n: usize) {
        debug_assert!(n <= self.0.iov_len);
        // SAFETY: `n` is inside the segment, so the result is inside the same
        // allocation.
        self.0.iov_base = unsafe { self.base().add(n) } as *mut libc::c_void;
        self.0.iov_len -= n;
    }
}

/// One I/O to issue. The segments point into guest memory (or a bounce buffer)
/// owned by the caller, which must keep them alive and untouched until the
/// matching completion comes back.
pub enum Job<'a> {
    Readv {
        iovs: &'a mut [IoVec],
        offset: u64,
    },
    Writev {
        iovs: &'a mut [IoVec],
        offset: u64,
    },
    /// `fdatasync`. Not `fsync`: the guest asked for its data to be durable,
    /// not for our timestamps to be.
    FlushData,
    /// `fallocate`, for discard and write-zeroes.
    Fallocate {
        mode: i32,
        offset: u64,
        len: u64,
    },
}

/// What came back. `result` follows the kernel's convention: bytes transferred,
/// or a negative errno.
#[derive(Clone, Copy, Debug)]
pub struct Done {
    pub token: u64,
    pub result: i32,
}

pub trait Engine: Send {
    /// Hand one job over. The token comes back in the completion.
    ///
    /// # Safety
    ///
    /// The memory the job's `iovec`s point at must stay valid and unmoved until
    /// that token is completed.
    unsafe fn submit(&mut self, token: u64, job: Job<'_>) -> io::Result<()>;

    /// How many more jobs may be submitted before the next [`Engine::run`].
    fn room(&self) -> usize;

    /// Push what is queued and collect what has finished, appending to `out`.
    ///
    /// With `block`, wait until either a job completes or the queue is kicked.
    /// Returns whether a kick arrived.
    fn run(&mut self, block: bool, out: &mut Vec<Done>) -> io::Result<bool>;

    fn name(&self) -> &'static str;

    /// How many notifications the guest has sent, in total.
    ///
    /// An eventfd counts: a read returns how many times it was written since
    /// the last one, so this is the guest's notify count and not ours. Used to
    /// answer whether suppressing notifications more finely would be worth
    /// anything.
    fn notifies(&self) -> u64;
}

// ── io_uring ────────────────────────────────────────────────────────────────

/// Token reserved for the eventfd read that carries queue kicks. Real requests
/// are slab indices, which are bounded by the ring depth and so never reach it.
const KICK_TOKEN: u64 = u64::MAX;

pub struct UringEngine {
    ring: io_uring::IoUring,
    kick: Arc<EventFd>,
    /// Landing pad for the 8 bytes an eventfd read returns. Read by the kernel
    /// while the SQE is in flight, which is why it is boxed and never moved.
    kick_buf: Box<u64>,
    kick_armed: bool,
    depth: usize,
    in_flight: usize,
    /// Submitted but not yet handed to the kernel, so that a batch of requests
    /// costs one `io_uring_enter` rather than one each.
    unsubmitted: usize,
    notifies: u64,
}

impl UringEngine {
    pub fn new(fd: i32, depth: usize, kick: Arc<EventFd>) -> Result<Self> {
        // The completion queue holds one entry per in-flight request plus the
        // armed kick; sizing it at twice the depth means a full ring can never
        // overflow it, and an overflowed CQ is silently dropped completions.
        let mut builder = io_uring::IoUring::builder();
        builder.setup_cqsize((depth as u32 * 2).next_power_of_two());
        // Only this worker thread touches its own ring, which lets the kernel
        // skip the interrupt it would otherwise send to wake a submitter, and
        // run completion work when we ask for it rather than by IPI. On a box
        // whose whole point is not disturbing vCPU threads, that is the flag
        // to have.
        builder.setup_single_issuer();
        builder.setup_coop_taskrun();

        let ring = match builder.build(depth as u32) {
            Ok(r) => r,
            // DEFER/SINGLE_ISSUER and friends are refused by older kernels
            // rather than ignored, so a plain ring is worth one more attempt
            // before falling all the way back to synchronous I/O.
            Err(e) => {
                log::debug!("virtio-blk: io_uring with SINGLE_ISSUER failed ({e}); retrying plain");
                io_uring::IoUring::new(depth as u32).context("io_uring_setup")?
            }
        };

        // The disk fd, registered once, so every request skips the fd lookup
        // and the refcount that goes with it.
        ring.submitter()
            .register_files(&[fd])
            .context("io_uring register_files")?;

        Ok(Self {
            ring,
            kick,
            kick_buf: Box::new(0),
            kick_armed: false,
            depth,
            in_flight: 0,
            unsubmitted: 0,
            notifies: 0,
        })
    }

    /// Keep an 8-byte read of the kick eventfd in the ring at all times.
    ///
    /// An eventfd's counter is sticky, so a kick that lands before the read is
    /// armed still completes it -- there is no window in which a notification
    /// can be lost.
    fn arm_kick(&mut self) {
        if self.kick_armed {
            return;
        }
        let entry = io_uring::opcode::Read::new(
            io_uring::types::Fd(self.kick.as_raw_fd()),
            &mut *self.kick_buf as *mut u64 as *mut u8,
            8,
        )
        .build()
        .user_data(KICK_TOKEN);
        // SAFETY: `kick_buf` is boxed, outlives the ring, and is not touched
        // again until this entry completes.
        if unsafe { self.ring.submission().push(&entry) }.is_ok() {
            self.kick_armed = true;
            self.unsubmitted += 1;
        }
    }

    fn push(&mut self, entry: io_uring::squeue::Entry) -> io::Result<()> {
        // SAFETY: the caller of `submit` guarantees the buffers this entry
        // points at outlive its completion.
        unsafe { self.ring.submission().push(&entry) }
            .map_err(|_| io::Error::from(io::ErrorKind::WouldBlock))?;
        self.in_flight += 1;
        self.unsubmitted += 1;
        Ok(())
    }
}

impl Engine for UringEngine {
    unsafe fn submit(&mut self, token: u64, job: Job<'_>) -> io::Result<()> {
        use io_uring::opcode;
        use io_uring::types::{Fixed, FsyncFlags};

        let fd = Fixed(0);
        let entry = match job {
            Job::Readv { iovs, offset } => opcode::Readv::new(
                fd,
                iovs.as_mut_ptr() as *const libc::iovec,
                iovs.len() as u32,
            )
            .offset(offset)
            .build(),
            Job::Writev { iovs, offset } => opcode::Writev::new(
                fd,
                iovs.as_mut_ptr() as *const libc::iovec,
                iovs.len() as u32,
            )
            .offset(offset)
            .build(),
            Job::FlushData => opcode::Fsync::new(fd).flags(FsyncFlags::DATASYNC).build(),
            Job::Fallocate { mode, offset, len } => opcode::Fallocate::new(fd, len)
                .offset(offset)
                .mode(mode)
                .build(),
        };
        self.push(entry.user_data(token))
    }

    fn room(&self) -> usize {
        // One slot is always held back for the kick read; losing that to a full
        // batch of requests would mean no way left to hear the next kick.
        self.depth.saturating_sub(self.in_flight).saturating_sub(1)
    }

    fn run(&mut self, block: bool, out: &mut Vec<Done>) -> io::Result<bool> {
        self.arm_kick();

        if block {
            self.ring.submit_and_wait(1)?;
        } else if self.unsubmitted > 0 {
            self.ring.submit()?;
        }
        self.unsubmitted = 0;

        let mut kicked = false;
        let mut cq = self.ring.completion();
        cq.sync();
        for cqe in &mut cq {
            if cqe.user_data() == KICK_TOKEN {
                kicked = true;
                self.kick_armed = false;
                // The eventfd's counter: how many notifies the guest sent
                // while we were not looking.
                if cqe.result() == 8 {
                    self.notifies += *self.kick_buf;
                }
                continue;
            }
            self.in_flight -= 1;
            out.push(Done {
                token: cqe.user_data(),
                result: cqe.result(),
            });
        }
        Ok(kicked)
    }

    fn name(&self) -> &'static str {
        "io_uring"
    }

    fn notifies(&self) -> u64 {
        self.notifies
    }
}

// ── Synchronous fallback ────────────────────────────────────────────────────

pub struct SyncEngine {
    fd: i32,
    kick: Arc<EventFd>,
    done: Vec<Done>,
    notifies: u64,
}

impl SyncEngine {
    pub fn new(fd: i32, kick: Arc<EventFd>) -> Self {
        Self {
            fd,
            kick,
            done: Vec::new(),
            notifies: 0,
        }
    }
}

impl Engine for SyncEngine {
    unsafe fn submit(&mut self, token: u64, job: Job<'_>) -> io::Result<()> {
        // SAFETY: the caller guarantees the buffers outlive the call, and here
        // the call is where the I/O happens, so that is trivially true.
        let rc: i64 = unsafe {
            match job {
                Job::Readv { iovs, offset } => libc::preadv(
                    self.fd,
                    iovs.as_ptr() as *const libc::iovec,
                    iovs.len() as i32,
                    offset as i64,
                ) as i64,
                Job::Writev { iovs, offset } => libc::pwritev(
                    self.fd,
                    iovs.as_ptr() as *const libc::iovec,
                    iovs.len() as i32,
                    offset as i64,
                ) as i64,
                Job::FlushData => libc::fdatasync(self.fd) as i64,
                Job::Fallocate { mode, offset, len } => {
                    libc::fallocate(self.fd, mode, offset as i64, len as i64) as i64
                }
            }
        };
        let result = if rc < 0 {
            -io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO)
        } else {
            rc as i32
        };
        self.done.push(Done { token, result });
        Ok(())
    }

    fn room(&self) -> usize {
        // Executed inline, so the only bound is the caller's slab.
        usize::MAX
    }

    fn run(&mut self, block: bool, out: &mut Vec<Done>) -> io::Result<bool> {
        let had = !self.done.is_empty();
        out.append(&mut self.done);
        if had || !block {
            // Something to report, or the caller does not want to wait. Either
            // way, do not block on a kick that may never come.
            return Ok(false);
        }
        // Nothing in flight -- everything here completes inline -- so the only
        // thing left to wait for is the guest.
        self.kick.read().map(|count| {
            self.notifies += count;
            true
        })
    }

    fn name(&self) -> &'static str {
        "synchronous"
    }

    fn notifies(&self) -> u64 {
        self.notifies
    }
}

/// Build the best engine this host will give us.
pub fn build(fd: i32, depth: usize, kick: Arc<EventFd>) -> Box<dyn Engine> {
    match UringEngine::new(fd, depth, kick.clone()) {
        Ok(e) => Box::new(e),
        Err(err) => {
            log::warn!(
                "virtio-blk: io_uring unavailable ({err:#}); falling back to synchronous I/O. \
                 Requests will not overlap. Check kernel.io_uring_disabled."
            );
            Box::new(SyncEngine::new(fd, kick))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::IntoRawFd;

    fn scratch(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("nesbox-blk-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}-{}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Both engines have to answer the same way, because which one a host gives
    /// us is not something the device above them gets to know.
    fn round_trip(mut engine: Box<dyn Engine>, fd: i32) {
        let mut buf = [0u8; 8];
        let mut iovs = [IoVec::new(buf.as_mut_ptr(), 8)];
        // SAFETY: `iovs` and `buf` outlive the completion collected below.
        unsafe {
            engine
                .submit(
                    7,
                    Job::Readv {
                        iovs: &mut iovs,
                        offset: 4,
                    },
                )
                .unwrap()
        };
        let mut out = Vec::new();
        while out.is_empty() {
            engine.run(true, &mut out).unwrap();
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].token, 7);
        assert_eq!(out[0].result, 8);
        assert_eq!(&buf, b"45678901");
        drop(engine);
        // SAFETY: the engine no longer holds the fd.
        unsafe { libc::close(fd) };
    }

    #[test]
    fn the_sync_engine_reads_at_an_offset() {
        let path = scratch("engine-sync", b"0123456789012345");
        let fd = std::fs::File::open(&path).unwrap().into_raw_fd();
        let kick = Arc::new(EventFd::new(0).unwrap());
        round_trip(Box::new(SyncEngine::new(fd, kick)), fd);
    }

    #[test]
    fn the_uring_engine_reads_at_an_offset() {
        let path = scratch("engine-uring", b"0123456789012345");
        let fd = std::fs::File::open(&path).unwrap().into_raw_fd();
        let kick = Arc::new(EventFd::new(0).unwrap());
        match UringEngine::new(fd, 8, kick) {
            Ok(e) => round_trip(Box::new(e), fd),
            Err(e) => {
                // A host with io_uring switched off is a host this test cannot
                // run on, and that is the case the fallback exists for.
                eprintln!("skipping: io_uring unavailable here ({e:#})");
                // SAFETY: nothing took the fd.
                unsafe { libc::close(fd) };
            }
        }
    }

    /// A kick that lands before the read is armed must still wake the engine.
    #[test]
    fn a_kick_delivered_early_is_not_lost() {
        let path = scratch("engine-kick", b"0123456789012345");
        let file = std::fs::File::open(&path).unwrap();
        let fd = file.as_raw_fd();
        let kick = Arc::new(EventFd::new(0).unwrap());
        kick.write(1).unwrap();
        let Ok(mut engine) = UringEngine::new(fd, 8, kick.clone()) else {
            eprintln!("skipping: io_uring unavailable here");
            return;
        };
        let mut out = Vec::new();
        assert!(engine.run(true, &mut out).unwrap());
        assert!(out.is_empty());
        drop(engine);
        drop(file);
    }
}
