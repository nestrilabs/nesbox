//! One worker per virtqueue: drains the ring, keeps as many requests in flight
//! as the guest offers, and completes them out of order.
//!
//! The old device did the opposite of each of those. It served one request at a
//! time with `seek` + `read_exact`, so the file offset was shared state and
//! depth was structurally 1; and it read into a freshly allocated host buffer
//! and copied that into guest memory, which is already mapped in this process.
//! A guest submitting 128 deep got them executed in single file, with an
//! allocation and a copy each.
//!
//! Here a request's guest pages *are* the I/O buffer, and the only thing that
//! serialises requests is the disk.

use super::disk::{Disk, SECTOR_SIZE};
use super::engine::{self, Done, Engine, IoVec, Job};
use super::request::{self, BLK_S_IOERR, BLK_S_OK, DISK_ID, Op, ParseError, Request};
use super::{Irq, Queue};
use crate::common::QState;
use crate::common::{
    pop_avail, push_used, set_avail_event, set_used_no_notify, used_needs_interrupt,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

/// `fallocate` modes. Not in `libc` on every target we build for, and both are
/// stable kernel ABI.
const FALLOC_FL_KEEP_SIZE: i32 = 0x01;
const FALLOC_FL_PUNCH_HOLE: i32 = 0x02;
const FALLOC_FL_ZERO_RANGE: i32 = 0x10;

/// An aligned buffer, used when a request cannot be issued against the guest's
/// own pages.
///
/// With `O_DIRECT` the kernel checks *each* `iovec` for alignment, so a single
/// odd segment in an otherwise fine request would fail the whole thing with
/// `EINVAL`. Rather than refuse, the request is staged through one of these.
/// A Linux guest essentially never needs it -- its block layer hands out
/// sector-aligned segments -- so this is the path that keeps a strange driver
/// working, not a hot path.
struct Bounce {
    ptr: std::ptr::NonNull<u8>,
    layout: std::alloc::Layout,
    /// Where the request's own data starts inside the buffer. Non-zero when the
    /// I/O had to be widened to an aligned window.
    skip: usize,
}

// SAFETY: the allocation is owned solely by the slot that holds it and is not
// shared between threads; the pointer is only what makes it not automatically
// `Send`.
unsafe impl Send for Bounce {}

impl Bounce {
    fn new(len: usize, align: usize, skip: usize) -> Option<Self> {
        let layout = std::alloc::Layout::from_size_align(len.max(1), align.max(1)).ok()?;
        // SAFETY: a non-zero-sized layout.
        let ptr = unsafe { std::alloc::alloc(layout) };
        Some(Self {
            ptr: std::ptr::NonNull::new(ptr)?,
            layout,
            skip,
        })
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn len(&self) -> usize {
        self.layout.size()
    }

    /// The bytes belonging to the request itself.
    ///
    /// # Safety
    ///
    /// Only valid once the I/O staging through this buffer has completed.
    unsafe fn data(&self, len: usize) -> &[u8] {
        // SAFETY: `skip + len` is within the allocation by construction.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(self.skip), len) }
    }

    /// # Safety
    ///
    /// Only valid before the I/O staging through this buffer is submitted.
    unsafe fn data_mut(&mut self, len: usize) -> &mut [u8] {
        // SAFETY: as above.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(self.skip), len) }
    }

    /// Zero everything from `at` to the end of the window. Used when a read
    /// stopped short of it.
    fn zero_from(&mut self, at: usize) {
        let at = at.min(self.layout.size());
        // SAFETY: `at` is inside the allocation, and no I/O is in flight
        // against it -- this runs on the completion of the one that was.
        unsafe { std::ptr::write_bytes(self.ptr.as_ptr().add(at), 0, self.layout.size() - at) };
    }
}

impl Drop for Bounce {
    fn drop(&mut self) {
        // SAFETY: allocated with this exact layout in `new`.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

/// One request in flight. The `iovec`s here are what the kernel is reading, so
/// nothing in a live slot may be moved or rewritten until it completes.
struct Slot {
    head: u16,
    op: Op,
    status_addr: u64,
    /// The guest segments, in order, for copying a bounced read back and for
    /// zero-filling a short one.
    segments: Vec<(u64, u32)>,
    iovs: Vec<IoVec>,
    bounce: Option<Bounce>,
    /// Where the *remaining* part of the transfer starts in the file.
    offset: u64,
    /// Bytes of the request satisfied so far.
    done: u64,
    /// Bytes the guest asked for.
    total: u64,
    /// Bytes the I/O itself moves, which is larger than `total` when a bounce
    /// buffer widened it to an aligned window.
    io_len: u64,
    /// Set once a request has been retried through a bounce buffer, so a
    /// second `EINVAL` is reported rather than retried forever.
    bounced: bool,
}

pub struct Worker {
    mem: Arc<GuestMemoryMmap>,
    disk: Arc<Disk>,
    queue: Arc<Queue>,
    irq: Arc<Mutex<Irq>>,
    /// Built by [`Worker::run`], on the thread that will submit to it.
    ///
    /// `IORING_SETUP_SINGLE_ISSUER` binds a ring to the task that first touches
    /// it, and every later submission from another thread is refused with
    /// `EEXIST`. Building the ring where the `Worker` is constructed -- the
    /// main thread -- and then moving it to its worker is exactly that mistake,
    /// and it fails at the first request rather than at setup.
    engine: Option<Box<dyn Engine>>,
    slots: Vec<Option<Slot>>,
    free: Vec<u32>,
    depth: usize,
    /// How long to keep looking at the ring before sleeping. Zero sleeps at
    /// once. See [`Worker::wait`].
    poll_us: u64,
    stop: Arc<AtomicBool>,
    /// There is, or may be, more in the avail ring than we have taken.
    want_drain: bool,
    /// The used index as the guest last saw it, carried across a batch so the
    /// `EVENT_IDX` interrupt decision has both ends of it.
    used_idx: u16,
    /// Whether the driver has been asked to stop kicking. Tracked so the flag
    /// is written only when it changes.
    notify_suppressed: bool,
    /// One line, once, when the host turns out not to support hole punching.
    discard_unsupported: bool,
    /// Requests taken from the ring, and interrupts raised for them. Reported
    /// on the way out: the ratio between them, and between them and the
    /// guest's notify count, is what says whether suppressing either more
    /// finely -- `VIRTIO_F_RING_EVENT_IDX` -- would be worth anything.
    requests: u64,
    interrupts: u64,
    /// Request count at the last summary line, so a long-running guest gets
    /// one occasionally rather than one per drain.
    reported: u64,
    /// How many requests have had to be staged through a bounce buffer, and
    /// whether that has been reported yet.
    ///
    /// A Linux guest is not supposed to produce one: its segments are whole
    /// pages, and the block size this device advertises is the alignment the
    /// filesystem asked for. It can still happen -- a buffer that is not
    /// page-aligned splits into segments shorter than a block, and on a
    /// filesystem wanting 4096 those cannot go directly -- and when it does it
    /// is a copy per request that nobody asked for. Worth one line rather than
    /// being invisible.
    staged: u64,
    staged_reported: bool,
}

impl Worker {
    pub fn new(
        mem: Arc<GuestMemoryMmap>,
        disk: Arc<Disk>,
        queue: Arc<Queue>,
        irq: Arc<Mutex<Irq>>,
        depth: usize,
        poll_us: u64,
        stop: Arc<AtomicBool>,
    ) -> Self {
        let mut slots = Vec::with_capacity(depth);
        slots.resize_with(depth, || None);
        Self {
            mem,
            disk,
            queue,
            irq,
            engine: None,
            depth,
            poll_us,
            slots,
            free: (0..depth as u32).rev().collect(),
            stop,
            want_drain: true,
            used_idx: 0,
            notify_suppressed: false,
            discard_unsupported: false,
            requests: 0,
            interrupts: 0,
            reported: 0,
            staged: 0,
            staged_reported: false,
        }
    }

    pub fn run(mut self) {
        let engine = engine::build(self.disk.raw_fd(), self.depth, self.queue.kick.clone());
        log::debug!(
            "virtio-blk: queue {} worker using {} at depth {}",
            self.queue.index,
            engine.name(),
            self.depth
        );
        self.engine = Some(engine);

        let mut completions: Vec<Done> = Vec::new();
        loop {
            // Always blocks: the only reason to stop taking from the ring is a
            // full engine, and then a completion is exactly what we are waiting
            // for. An idle disk costs one sleeping thread and nothing else.
            let kicked = match self.wait(&mut completions) {
                Ok(k) => k,
                Err(e) => {
                    log::error!("virtio-blk: queue {} engine failed: {e}", self.queue.index);
                    return;
                }
            };
            if self.stop.load(Ordering::Acquire) {
                if self.staged > 0 {
                    log::info!(
                        "virtio-blk: queue {} staged {} requests through a bounce buffer",
                        self.queue.index,
                        self.staged
                    );
                }
                return;
            }
            if kicked {
                self.want_drain = true;
            }

            let queue = self.queue.clone();
            let mut q = queue.state.lock().unwrap();
            if !q.enabled || q.desc == 0 {
                // The driver has taken the queue away, most likely a reset.
                // Nothing may be written into a ring that is gone, but the
                // slots those requests hold still have to come back or the
                // queue comes up after the reset with fewer than it started
                // with.
                for done in completions.drain(..) {
                    self.release(done.token as u32);
                }
                self.want_drain = false;
                continue;
            }

            let batch_start = self.used_idx;
            let mut used = 0usize;
            for done in completions.drain(..) {
                used += self.complete(&q, done);
            }
            if self.want_drain {
                used += self.drain(&mut q);
            }
            if used > 0 && used_needs_interrupt(&self.mem, &q, batch_start, self.used_idx) {
                let mut irq = self.irq.lock().unwrap();
                irq.isr |= 1;
                irq.msix.trigger(q.vec);
                self.interrupts += 1;
            }
        }
    }

    /// Wait for something to do: a completion, or a request the guest added.
    ///
    /// A worker that goes straight to sleep pays a thread wakeup for every
    /// request a guest submits on its own -- and at queue depth one that wakeup
    /// is a large share of what the guest experiences as disk latency, because
    /// there is no other request in flight to hide it behind. Looking at the
    /// ring for a moment first turns that wakeup into a memory read, and can
    /// see the request before the notify that announces it has even finished
    /// trapping.
    ///
    /// The cost is bounded and lands only where there is work: a worker spins
    /// for at most `poll_us` after each wake and then sleeps, so an idle disk
    /// spins once and stops, and nothing is burned by a guest that is not doing
    /// I/O. Under a steady stream of requests it will spend most of that window
    /// spinning between them, which approaches a busy core per active queue --
    /// which is why it is off by default and belongs to drives that need the
    /// latency more than the box needs the core.
    ///
    /// Measured, 4 KiB reads one at a time: 23.6 -> 12.6 us per request served
    /// from host cache, 38.0 -> 28.5 us against an NVMe. See
    /// `docs/BENCHMARKS.md` §14.2.
    fn wait(&mut self, completions: &mut Vec<Done>) -> std::io::Result<bool> {
        if self.poll_us > 0 {
            let deadline = Instant::now() + Duration::from_micros(self.poll_us);
            loop {
                if self.engine().run(false, completions)? {
                    return Ok(true);
                }
                if !completions.is_empty() {
                    return Ok(false);
                }
                if self.ring_has_work() {
                    // Not a kick, but the same thing the kick would have told
                    // us, and it arrived first.
                    return Ok(true);
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::hint::spin_loop();
            }
        }
        self.engine().run(true, completions)
    }

    /// Is there anything in the ring the guest has offered and we have not
    /// taken? Answered without disturbing the queue.
    fn ring_has_work(&self) -> bool {
        let queue = self.queue.clone();
        let q = queue.state.lock().unwrap();
        q.enabled && q.desc != 0 && has_avail(&self.mem, &q)
    }

    /// Give a slot back without completing it to the guest.
    fn release(&mut self, token: u32) {
        if let Some(slot) = self.slots.get_mut(token as usize)
            && slot.take().is_some()
        {
            self.free.push(token);
        }
    }

    /// The engine, which exists from the first line of [`Worker::run`].
    fn engine(&mut self) -> &mut dyn Engine {
        self.engine
            .as_deref_mut()
            .expect("the engine is built before the loop starts")
    }

    /// Room for one more request, both in the slab and in the engine.
    fn can_accept(&self) -> bool {
        !self.free.is_empty()
            && self
                .engine
                .as_deref()
                .is_some_and(|engine| engine.room() > 0)
    }

    /// Take everything the guest has offered, or as much as will fit.
    ///
    /// Returns how many used entries were pushed for requests that finished
    /// without touching the disk at all.
    fn drain(&mut self, q: &mut QState) -> usize {
        let mut used = 0;
        loop {
            // Every kick is a VM exit, and while we are in here we will pick up
            // whatever the driver adds anyway, so the exits buy nothing.
            self.suppress_notify(q, true);
            while self.can_accept() {
                let Some((head, descs)) = pop_avail(&self.mem, q) else {
                    break;
                };
                self.requests += 1;
                used += self.start(q, head, &descs);
            }
            if !self.can_accept() {
                // Full. Leave notifications suppressed -- a kick would find us
                // with nowhere to put the request -- and come back after the
                // completions that free room.
                return used;
            }
            self.suppress_notify(q, false);
            // The driver is free to ignore the hint, but it is also free to
            // have honoured it for something added just now. Re-check before
            // going to sleep, or that request sits there with no kick coming.
            if !has_avail(&self.mem, q) {
                self.want_drain = false;
                self.report();
                return used;
            }
        }
    }

    /// A line about what this queue has been doing, on going idle, and not
    /// more than once per `REPORT_EVERY` requests.
    ///
    /// The two ratios are the point. Notifies per request says what the guest
    /// is spending on doorbells, interrupts per request what it is spending on
    /// being woken -- and both are what `VIRTIO_F_RING_EVENT_IDX` exists to
    /// reduce, so they say whether offering it would buy anything here.
    ///
    /// Reported here rather than on the way out because the VMM exits with
    /// `process::exit` and destructors do not run, and because a guest that
    /// runs for days never exits at all.
    fn report(&mut self) {
        const REPORT_EVERY: u64 = 4096;
        if self.requests < self.reported + REPORT_EVERY {
            return;
        }
        self.reported = self.requests;
        let notifies = self.engine.as_deref().map(|e| e.notifies()).unwrap_or(0);
        log::debug!(
            "virtio-blk: queue {}: {} requests, {} notifies ({:.2}/req), {} interrupts \
             ({:.2}/req)",
            self.queue.index,
            self.requests,
            notifies,
            notifies as f64 / self.requests as f64,
            self.interrupts,
            self.interrupts as f64 / self.requests as f64,
        );
    }

    /// Ask the driver to stop kicking, or to start again.
    ///
    /// With `EVENT_IDX` the two sides trade indices rather than flags, so
    /// "stop" is a point far enough ahead that the driver will not reach it
    /// while we are draining, and "start" is the next entry we have not taken.
    /// The flag is not touched in that case: with the feature negotiated the
    /// driver does not read it, and writing both would be describing the queue
    /// two ways at once.
    fn suppress_notify(&mut self, q: &QState, on: bool) {
        if q.event_idx {
            // `q.last` is the next avail entry we would take, so naming it is
            // "tell me about the very next one"; half a ring ahead is "not for
            // a while", and it is re-armed the moment we go idle.
            let want = if on {
                q.last.wrapping_add(q.size / 2).wrapping_add(1)
            } else {
                q.last
            };
            set_avail_event(&self.mem, q, want);
            self.notify_suppressed = on;
            return;
        }
        if self.notify_suppressed != on {
            set_used_no_notify(&self.mem, q, on);
            self.notify_suppressed = on;
        }
    }

    /// Parse one chain and either issue it or finish it here.
    fn start(&mut self, q: &QState, head: u16, descs: &[(u64, u32, u16)]) -> usize {
        let req = match request::parse(&self.mem, &self.disk, head, descs) {
            Ok(r) => r,
            Err(ParseError::Malformed(why)) => {
                log::warn!("virtio-blk: dropping a malformed request: {why}");
                self.used_idx = push_used(&self.mem, q, head, 0);
                return 1;
            }
            Err(ParseError::Rejected(addr, status)) => {
                self.write_status(addr, status);
                self.used_idx = push_used(&self.mem, q, head, 1);
                return 1;
            }
        };

        // The only request that is answered out of this device's own memory.
        if req.op == Op::GetId {
            let mut written = 0u32;
            for &(addr, len) in &req.segments {
                let n = (len as usize).min(DISK_ID.len() - written as usize);
                if n == 0 {
                    break;
                }
                let _ = self.mem.write_slice(
                    &DISK_ID[written as usize..written as usize + n],
                    GuestAddress(addr),
                );
                written += n as u32;
            }
            self.write_status(req.status_addr, BLK_S_OK);
            self.used_idx = push_used(&self.mem, q, head, written + 1);
            return 1;
        }

        self.issue(q, req)
    }

    /// Hand a request to the engine, or complete it here if it cannot go.
    fn issue(&mut self, q: &QState, req: Request) -> usize {
        let Some(token) = self.free.pop() else {
            // `can_accept` is checked before every pop, so this cannot happen;
            // completing the chain beats leaving the guest waiting forever.
            self.write_status(req.status_addr, BLK_S_IOERR);
            self.used_idx = push_used(&self.mem, q, req.head, 1);
            return 1;
        };

        let mut slot = match self.prepare(req) {
            Ok(s) => s,
            Err((head, status_addr, status)) => {
                self.free.push(token);
                self.write_status(status_addr, status);
                self.used_idx = push_used(&self.mem, q, head, 1);
                return 1;
            }
        };

        match self.submit(token, &mut slot) {
            Ok(()) => {
                self.slots[token as usize] = Some(slot);
                0
            }
            Err(e) => {
                log::error!("virtio-blk: could not submit a request: {e}");
                self.free.push(token);
                self.write_status(slot.status_addr, BLK_S_IOERR);
                self.used_idx = push_used(&self.mem, q, slot.head, 1);
                1
            }
        }
    }

    /// Build the `iovec`s for a request, staging it through a bounce buffer if
    /// the guest's own pages cannot be handed to direct I/O as they are.
    ///
    /// On failure returns what the guest needs to be told: `(head, status
    /// address, status)`.
    fn prepare(&mut self, req: Request) -> Result<Slot, (u16, u64, u8)> {
        let Request {
            head,
            op,
            offset,
            segments,
            total,
            status_addr,
        } = req;

        let mut slot = Slot {
            head,
            op,
            status_addr,
            segments,
            iovs: Vec::new(),
            bounce: None,
            offset,
            done: 0,
            total,
            io_len: total,
            bounced: false,
        };

        if !matches!(op, Op::Read | Op::Write) {
            return Ok(slot);
        }

        let direct = self.iovs_for(&slot.segments);
        // The two describe the same bytes -- parsing checked every segment
        // against guest memory, so the lookup cannot have dropped one. If it
        // ever did, the transfer would be shorter than the request claims and
        // the guest would be told a read succeeded into pages nothing wrote.
        if direct.len() != slot.segments.len() {
            return Err((head, status_addr, BLK_S_IOERR));
        }
        if self.disk.direct_ok(offset, &direct) {
            slot.iovs = direct;
            return Ok(slot);
        }
        self.stage(&mut slot)
            .map_err(|status| (head, status_addr, status))?;
        Ok(slot)
    }

    /// Point `iovec`s straight at the guest's pages.
    ///
    /// Every segment was checked against guest memory during parsing, so the
    /// lookup here cannot fail; a segment that somehow did not resolve is
    /// dropped rather than turned into a wild pointer.
    fn iovs_for(&self, segments: &[(u64, u32)]) -> Vec<IoVec> {
        let mut out = Vec::with_capacity(segments.len());
        for &(addr, len) in segments {
            let Ok(slice) = self.mem.get_slice(GuestAddress(addr), len as usize) else {
                continue;
            };
            out.push(IoVec::new(slice.ptr_guard_mut().as_ptr(), len as usize));
        }
        out
    }

    /// Rebuild a request around an aligned bounce buffer.
    fn stage(&mut self, slot: &mut Slot) -> Result<(), u8> {
        let offset_align = self.disk.offset_align();
        let (align, head_pad, io_len) = self.disk.bounce_shape(slot.offset, slot.total);

        // A request whose byte range is not aligned needs the I/O widened to
        // the blocks around it. That is fine for a read and is a
        // read-modify-write for a write -- which no compliant driver can
        // produce, since data lengths are sector multiples and we advertise
        // the alignment as the block size. Refuse rather than grow a
        // read-modify-write path that would never be exercised or tested.
        if (head_pad != 0 || io_len != slot.total) && slot.op == Op::Write {
            log::warn!(
                "virtio-blk: refusing a write of {} bytes at offset {}, which direct I/O on this \
                 image cannot express (alignment {offset_align}). The driver is ignoring the \
                 block size this device advertises.",
                slot.total,
                slot.offset
            );
            return Err(BLK_S_IOERR);
        }

        let Some(mut bounce) = Bounce::new(io_len as usize, align as usize, head_pad as usize)
        else {
            return Err(BLK_S_IOERR);
        };

        self.staged += 1;
        if !self.staged_reported {
            self.staged_reported = true;
            log::info!(
                "virtio-blk: a request could not be issued against the guest's own pages and \
                 was copied through an aligned buffer ({} bytes at offset {}, alignment {}). \
                 A guest whose segments are whole pages does not need this; if it is happening \
                 often, every request here is paying for a copy.",
                slot.total,
                slot.offset,
                offset_align
            );
        }

        if slot.op == Op::Write {
            // SAFETY: nothing has been submitted for this slot yet.
            let dst = unsafe { bounce.data_mut(slot.total as usize) };
            let mut at = 0usize;
            for &(addr, len) in &slot.segments {
                let end = at + len as usize;
                if self
                    .mem
                    .read_slice(&mut dst[at..end], GuestAddress(addr))
                    .is_err()
                {
                    return Err(BLK_S_IOERR);
                }
                at = end;
            }
        }

        slot.offset -= head_pad;
        slot.io_len = io_len;
        slot.iovs = vec![IoVec::new(bounce.as_mut_ptr(), bounce.len())];
        slot.bounce = Some(bounce);
        slot.bounced = true;
        Ok(())
    }

    fn submit(&mut self, token: u32, slot: &mut Slot) -> std::io::Result<()> {
        let job = match slot.op {
            Op::Read => Job::Readv {
                iovs: &mut slot.iovs,
                offset: slot.offset,
            },
            Op::Write => Job::Writev {
                iovs: &mut slot.iovs,
                offset: slot.offset,
            },
            Op::Flush => Job::FlushData,
            Op::Discard { sector, sectors } => Job::Fallocate {
                mode: FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
                offset: sector * SECTOR_SIZE,
                len: sectors as u64 * SECTOR_SIZE,
            },
            Op::WriteZeroes {
                sector,
                sectors,
                unmap,
            } => Job::Fallocate {
                // `unmap` is the driver saying it does not care whether the
                // range keeps its allocation, which makes a hole the cheapest
                // correct answer. Without it the range must stay allocated.
                mode: if unmap {
                    FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE
                } else {
                    FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE
                },
                offset: sector * SECTOR_SIZE,
                len: sectors as u64 * SECTOR_SIZE,
            },
            Op::GetId => unreachable!("GET_ID never reaches the engine"),
        };
        // SAFETY: the slot outlives the completion -- it is parked in `slots`
        // under this token -- and nothing touches its `iovs` or bounce buffer
        // until then.
        unsafe { self.engine().submit(token as u64, job) }
    }

    /// Handle one completion: finish the request, or resubmit what is left of
    /// it.
    ///
    /// Returns how many used entries were pushed.
    fn complete(&mut self, q: &QState, done: Done) -> usize {
        let token = done.token as u32;
        let Some(mut slot) = self.slots.get_mut(token as usize).and_then(Option::take) else {
            log::error!("virtio-blk: completion for an unknown token {token}");
            return 0;
        };

        let finish = |w: &mut Self, slot: Slot, status: u8| -> usize {
            let len = match (status, slot.op) {
                (BLK_S_OK, Op::Read) => slot.total as u32 + 1,
                // For everything else only the status byte is written into a
                // device-writable buffer, and that is what the used length
                // counts.
                _ => 1,
            };
            w.write_status(slot.status_addr, status);
            w.used_idx = push_used(&w.mem, q, slot.head, len);
            w.free.push(token);
            1
        };

        if done.result < 0 {
            let err = -done.result;
            match err {
                // Interrupted or told to come back. Neither is a failure.
                libc::EINTR | libc::EAGAIN => {
                    return self.resubmit(q, token, slot);
                }
                // Direct I/O rejecting the shape of the request. The alignment
                // check should have caught it, so this is either a kernel we
                // read wrong or a filesystem that lied: stage it and try once
                // more before telling the guest its disk failed.
                // Staging rebuilds the whole transfer, so it is only an
                // option before any of it has happened.
                libc::EINVAL
                    if !slot.bounced
                        && slot.done == 0
                        && matches!(slot.op, Op::Read | Op::Write) =>
                {
                    log::warn!(
                        "virtio-blk: direct I/O refused a request at offset {}; retrying staged",
                        slot.offset
                    );
                    if self.stage(&mut slot).is_err() {
                        return finish(self, slot, BLK_S_IOERR);
                    }
                    return self.resubmit(q, token, slot);
                }
                libc::EOPNOTSUPP
                    if matches!(slot.op, Op::Discard { .. } | Op::WriteZeroes { .. }) =>
                {
                    if !self.discard_unsupported {
                        self.discard_unsupported = true;
                        log::warn!(
                            "virtio-blk: the filesystem under this image cannot punch holes, so \
                             discard and write-zeroes are refused. The image will only grow."
                        );
                    }
                    return finish(self, slot, request::BLK_S_UNSUPP);
                }
                _ => {
                    log::error!(
                        "virtio-blk: {:?} at offset {} failed: {}",
                        slot.op,
                        slot.offset,
                        std::io::Error::from_raw_os_error(err)
                    );
                    return finish(self, slot, BLK_S_IOERR);
                }
            }
        }

        if !matches!(slot.op, Op::Read | Op::Write) {
            return finish(self, slot, BLK_S_OK);
        }

        let moved = done.result as u64;
        slot.done += moved;
        slot.offset += moved;

        if slot.done < slot.io_len {
            if moved == 0 {
                // Nothing moved and no error. For a read that is the end of a
                // file shorter than its own capacity claims; the rest is
                // zeroes, which is what a real disk would return. For a write
                // it is a failure with no errno, and retrying would spin.
                if slot.op == Op::Read {
                    match slot.bounce.as_mut() {
                        // The window is what gets copied out, so zeroing its
                        // tail is what the guest ends up seeing.
                        Some(b) => b.zero_from(slot.done as usize),
                        None => self.zero_fill(&slot),
                    }
                    if slot.bounce.is_some() && self.copy_out(&slot).is_err() {
                        return finish(self, slot, BLK_S_IOERR);
                    }
                    return finish(self, slot, BLK_S_OK);
                }
                log::error!("virtio-blk: a write stopped short with no error reported");
                return finish(self, slot, BLK_S_IOERR);
            }
            advance(&mut slot.iovs, moved as usize);
            return self.resubmit(q, token, slot);
        }

        if slot.op == Op::Read && slot.bounce.is_some() && self.copy_out(&slot).is_err() {
            return finish(self, slot, BLK_S_IOERR);
        }
        finish(self, slot, BLK_S_OK)
    }

    /// Put a partially-completed request back in the engine.
    fn resubmit(&mut self, q: &QState, token: u32, mut slot: Slot) -> usize {
        match self.submit(token, &mut slot) {
            Ok(()) => {
                self.slots[token as usize] = Some(slot);
                0
            }
            Err(e) => {
                log::error!("virtio-blk: could not resubmit a request: {e}");
                self.write_status(slot.status_addr, BLK_S_IOERR);
                self.used_idx = push_used(&self.mem, q, slot.head, 1);
                self.free.push(token);
                1
            }
        }
    }

    /// Copy a staged read back into the guest's pages.
    fn copy_out(&self, slot: &Slot) -> Result<(), ()> {
        let bounce = slot.bounce.as_ref().ok_or(())?;
        // SAFETY: the I/O through this buffer has completed.
        let src = unsafe { bounce.data(slot.total as usize) };
        let mut at = 0usize;
        for &(addr, len) in &slot.segments {
            let end = at + len as usize;
            self.mem
                .write_slice(&src[at..end], GuestAddress(addr))
                .map_err(|_| ())?;
            at = end;
        }
        Ok(())
    }

    /// Fill what a short read did not reach with zeroes, so the guest is not
    /// handed whatever those pages held before.
    fn zero_fill(&self, slot: &Slot) {
        let mut skip = slot.done;
        let zeroes = [0u8; 4096];
        for &(addr, len) in &slot.segments {
            let len = len as u64;
            if skip >= len {
                skip -= len;
                continue;
            }
            let mut at = skip;
            skip = 0;
            while at < len {
                let n = (len - at).min(zeroes.len() as u64) as usize;
                if self
                    .mem
                    .write_slice(&zeroes[..n], GuestAddress(addr + at))
                    .is_err()
                {
                    return;
                }
                at += n as u64;
            }
        }
    }

    fn write_status(&self, addr: u64, status: u8) {
        let _ = self.mem.write_obj(status, GuestAddress(addr));
    }
}

/// Is there anything in the avail ring we have not taken?
fn has_avail(mem: &GuestMemoryMmap, q: &QState) -> bool {
    match mem.read_obj::<u16>(GuestAddress(q.avail + 2)) {
        Ok(idx) => u16::from_le(idx) != q.last,
        Err(_) => false,
    }
}

/// Drop `n` bytes from the front of a scatter-gather list.
fn advance(iovs: &mut Vec<IoVec>, n: usize) {
    let mut left = n;
    let mut drop_upto = 0;
    for iov in iovs.iter_mut() {
        if left == 0 {
            break;
        }
        if left >= iov.len() {
            left -= iov.len();
            drop_upto += 1;
        } else {
            iov.advance(left);
            left = 0;
        }
    }
    iovs.drain(..drop_upto);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iov(base: usize, len: usize) -> IoVec {
        IoVec::new(base as *mut u8, len)
    }

    /// A short `writev` has to be resumed from exactly where it stopped, or the
    /// disk gets the same bytes twice and misses the rest.
    #[test]
    fn advancing_past_whole_segments_keeps_the_remainder() {
        let mut iovs = vec![iov(0x1000, 512), iov(0x2000, 512), iov(0x3000, 512)];
        advance(&mut iovs, 640);
        assert_eq!(iovs.len(), 2);
        assert_eq!(iovs[0].base() as usize, 0x2000 + 128);
        assert_eq!(iovs[0].len(), 512 - 128);
        assert_eq!(iovs[1].base() as usize, 0x3000);
    }

    #[test]
    fn advancing_by_everything_leaves_nothing() {
        let mut iovs = vec![iov(0x1000, 512), iov(0x2000, 512)];
        advance(&mut iovs, 1024);
        assert!(iovs.is_empty());
    }

    #[test]
    fn advancing_by_nothing_changes_nothing() {
        let mut iovs = vec![iov(0x1000, 512)];
        advance(&mut iovs, 0);
        assert_eq!(iovs.len(), 1);
        assert_eq!(iovs[0].len(), 512);
    }

    /// The whole point of the buffer: what the request asked for sits at an
    /// offset inside a window that was widened for alignment.
    #[test]
    fn a_bounce_buffer_addresses_the_request_inside_its_window() {
        let mut b = Bounce::new(1024, 512, 100).unwrap();
        // SAFETY: nothing is in flight against this buffer.
        unsafe { b.data_mut(8) }.copy_from_slice(b"01234567");
        assert_eq!(b.len(), 1024);
        assert_eq!(b.as_mut_ptr() as usize % 512, 0);
        // SAFETY: as above.
        assert_eq!(unsafe { b.data(8) }, b"01234567");
    }
}
