//! Shared constants and types for virtio-pci devices.
use pci::{MsiRouter, MsiVector};
use std::sync::Arc;
use vm_memory::{Bytes, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

// ── BAR layout ──────────────────────────────────────────────────────────────
pub const OFF_COMMON: u64 = 0x0000;
pub const OFF_ISR: u64 = 0x0100;
pub const OFF_DEVICE: u64 = 0x0200;
pub const OFF_NOTIFY: u64 = 0x0300;
pub const OFF_MSIX_TABLE: u64 = 0x0400;
pub const OFF_MSIX_PBA: u64 = 0x0500;
pub const BAR0_SIZE: u64 = 0x1000;
pub const NOTIFY_MULT: u32 = 4;

// ── Common config offsets ───────────────────────────────────────────────────
pub const CFG_DEVICE_FEAT_SEL: u64 = 0x00;
pub const CFG_DEVICE_FEAT: u64 = 0x04;
pub const CFG_DRIVER_FEAT_SEL: u64 = 0x08;
pub const CFG_DRIVER_FEAT: u64 = 0x0c;
pub const CFG_MSIX_CONFIG: u64 = 0x10;
pub const CFG_NUM_QUEUES: u64 = 0x12;
pub const CFG_STATUS: u64 = 0x14;
pub const CFG_QUEUE_SEL: u64 = 0x16;
pub const CFG_QUEUE_SIZE: u64 = 0x18;
pub const CFG_QUEUE_MSIX: u64 = 0x1a;
pub const CFG_QUEUE_ENABLE: u64 = 0x1c;
pub const CFG_QUEUE_NOTIFY_OFF: u64 = 0x1e;
pub const CFG_QUEUE_DESC_LO: u64 = 0x20;
pub const CFG_QUEUE_DESC_HI: u64 = 0x24;
pub const CFG_QUEUE_AVAIL_LO: u64 = 0x28;
pub const CFG_QUEUE_AVAIL_HI: u64 = 0x2c;
pub const CFG_QUEUE_USED_LO: u64 = 0x30;
pub const CFG_QUEUE_USED_HI: u64 = 0x34;

pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
pub const STATUS_DRIVER_OK: u8 = 4;

pub const VRING_DESC_F_NEXT: u16 = 1;
pub const VRING_DESC_F_WRITE: u16 = 2;
/// The descriptor's buffer is itself a table of descriptors.
pub const VRING_DESC_F_INDIRECT: u16 = 4;
pub const VIRTQ_MSI_NO_VECTOR: u16 = 0xFFFF;
pub const VRING_DESC_SIZE: u64 = 16;

/// In the avail ring's flags: the driver does not want an interrupt.
pub const VRING_AVAIL_F_NO_INTERRUPT: u16 = 1;
/// In the used ring's flags: the device does not want to be notified.
pub const VRING_USED_F_NO_NOTIFY: u16 = 1;

pub const VIRTIO_F_RING_INDIRECT_DESC: u64 = 1 << 28;
pub const VIRTIO_F_RING_EVENT_IDX: u64 = 1 << 29;

#[derive(Default, Clone)]
pub struct QState {
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    pub size: u16,
    pub enabled: bool,
    pub last: u16,
    pub vec: u16,
    /// Set once the driver has accepted `VIRTIO_F_RING_INDIRECT_DESC`. A
    /// descriptor may only be followed into an indirect table when the feature
    /// was negotiated, so this is per-queue state rather than a constant.
    pub indirect: bool,
    /// Set once the driver has accepted `VIRTIO_F_RING_EVENT_IDX`.
    ///
    /// It changes what both sides read: with it, the `flags` fields of both
    /// rings are ignored and each side publishes the index at which it wants to
    /// hear from the other instead. Half-applying it -- writing the event
    /// fields while still reading the flags, or the reverse -- is a queue that
    /// stalls, so everything gated on it is gated on this one bit.
    pub event_idx: bool,
}

#[derive(Default)]
pub struct ComCfg {
    pub dfs: u32,
    pub dff: u32,
    pub df: u64,
    pub st: u8,
}

/// Parse a common-cfg write into (u32, u16, u8).
pub fn parse_write(data: &[u8]) -> (u32, u16, u8) {
    let mut b = [0u8; 8];
    let n = data.len().min(8);
    b[..n].copy_from_slice(&data[..n]);
    let v = u64::from_le_bytes(b);
    (v as u32, v as u16, v as u8)
}

/// Write a u64 value to a byte slice as little-endian.
pub fn write_val(data: &mut [u8], val: u64) {
    let b = val.to_le_bytes();
    for (j, x) in data.iter_mut().enumerate() {
        *x = if j < 8 { b[j] } else { 0 };
    }
}

/// Apply a queue size the guest wrote, against the maximum this device
/// advertises.
///
/// Both ends need bounding and neither used to be checked.
///
/// **Zero** is stored as written, because zero legitimately means "this queue is
/// unused" -- but it can never reach `pop_avail`, where `q.last % q.size` is a
/// division, not an error. Three MMIO writes from inside a guest were enough to
/// panic the VMM. The refusal lives in `pop_avail` and `push_used` rather than
/// here so that it holds however a `QState` was built.
///
/// **Above the advertised maximum** is clamped. The spec says a driver MUST NOT
/// write more than `queue_size_max`, so a larger value is a broken or hostile
/// driver either way; clamping keeps a merely-confused one working. It matters
/// because `max` is what bounds the chain walk below, and a guest that can raise
/// it can make `collect_descs` walk a chain far longer than the ring it
/// advertised -- every descriptor of which is host memory `Reader` commits.
pub fn set_queue_size(q: &mut QState, requested: u16, max: u16) {
    if requested > max {
        log::warn!(
            "virtio: guest asked for a {requested}-entry queue against a maximum \
             of {max}; clamping"
        );
        q.size = max;
    } else {
        q.size = requested;
    }
}

/// Read one descriptor out of a table. `None` means guest memory did not have
/// it, which is a broken driver rather than an error we can report anywhere.
fn read_desc(mem: &GuestMemoryMmap, table: u64, idx: u16) -> Option<(u64, u32, u16, u16)> {
    let base = vm_memory::GuestAddress(table.checked_add(idx as u64 * VRING_DESC_SIZE)?);
    let addr: u64 = mem.read_obj(base).ok().map(u64::from_le)?;
    let len: u32 = mem
        .read_obj(vm_memory::GuestAddress(base.0.checked_add(8)?))
        .ok()
        .map(u32::from_le)?;
    let flags: u16 = mem
        .read_obj(vm_memory::GuestAddress(base.0.checked_add(12)?))
        .ok()
        .map(u16::from_le)?;
    let next: u16 = mem
        .read_obj(vm_memory::GuestAddress(base.0.checked_add(14)?))
        .ok()
        .map(u16::from_le)?;
    Some((addr, len, flags, next))
}

/// Walk one chain of descriptors, starting at `head`, into `out`.
///
/// `table` is the descriptor table to index, `max` its length in entries. The
/// walk stops at the first descriptor without `NEXT`, at `max` steps, or at a
/// `next` that points off the table -- the spec's rule, and the bound that keeps
/// a hostile ring from turning into an unbounded walk.
fn walk_chain(
    mem: &GuestMemoryMmap,
    table: u64,
    head: u16,
    max: u16,
    out: &mut Vec<(u64, u32, u16)>,
    allow_indirect: bool,
) {
    let mut idx = head;
    if idx >= max {
        return;
    }
    for _ in 0..max {
        let Some((addr, len, flags, next)) = read_desc(mem, table, idx) else {
            return;
        };
        if flags & VRING_DESC_F_INDIRECT != 0 {
            // Nested indirection is forbidden by the spec, and following it
            // would be the one place this walk could recurse without a bound.
            if !allow_indirect {
                return;
            }
            walk_indirect(mem, addr, len, max, out);
            return;
        }
        out.push((addr, len, flags));
        // A chain longer than the ring is a driver bug either way, but this is
        // also what bounds how much host memory a single request commits.
        if out.len() >= max as usize {
            return;
        }
        if flags & VRING_DESC_F_NEXT == 0 || next >= max {
            return;
        }
        idx = next;
    }
}

/// Follow an indirect descriptor into the table it points at.
///
/// The table is guest memory the driver filled in, so its length is the
/// driver's claim and not to be trusted: entries beyond `max` are the same
/// unbounded walk the direct path refuses, and a length that is not a whole
/// number of descriptors is a malformed table.
fn walk_indirect(
    mem: &GuestMemoryMmap,
    table: u64,
    byte_len: u32,
    max: u16,
    out: &mut Vec<(u64, u32, u16)>,
) {
    let entries = byte_len as u64 / VRING_DESC_SIZE;
    if entries == 0 {
        return;
    }
    // The ring the guest advertised is the ceiling on a request's segment
    // count -- it is what `seg_max` is derived from -- so an indirect table
    // claiming more entries than that is clamped rather than believed.
    let entries = entries.min(max as u64) as u16;
    walk_chain(mem, table, 0, entries, out, false);
}

/// Collect descriptors from a vring chain, without following indirect
/// descriptors. See [`collect_descs_with`] for the form that can.
pub fn collect_descs(
    mem: &GuestMemoryMmap,
    desc_base: u64,
    head: u16,
    max: u16,
) -> Vec<(u64, u32, u16)> {
    collect_descs_with(mem, desc_base, head, max, false)
}

/// Collect descriptors from a vring chain.
///
/// With `allow_indirect`, a descriptor carrying `VRING_DESC_F_INDIRECT` is
/// followed into the table it names and that table's chain is collected
/// instead. Only pass true when the driver accepted
/// `VIRTIO_F_RING_INDIRECT_DESC`: a driver that did not negotiate it cannot
/// have meant the flag, and honouring it anyway would read a buffer as a
/// descriptor table.
pub fn collect_descs_with(
    mem: &GuestMemoryMmap,
    desc_base: u64,
    head: u16,
    max: u16,
    allow_indirect: bool,
) -> Vec<(u64, u32, u16)> {
    let mut descs = Vec::new();
    walk_chain(mem, desc_base, head, max, &mut descs, allow_indirect);
    descs
}

/// Read one avail descriptor from the queue. Returns None if empty.
pub fn pop_avail(mem: &GuestMemoryMmap, q: &mut QState) -> Option<(u16, Vec<(u64, u32, u16)>)> {
    // A queue the guest sized to zero has no ring to index into, and the
    // remainder below would panic rather than report anything. See
    // `set_queue_size`.
    //
    // A ring address of zero is the other half of the same check: that is where
    // a queue starts and where a reset puts it back, and treating it as a ring
    // would read guest address zero as an avail index.
    if q.size == 0 || q.avail == 0 || q.desc == 0 {
        return None;
    }
    let a = vm_memory::GuestAddress(q.avail + 2);
    let idx: u16 = match mem.read_obj(a) {
        Ok(v) => u16::from_le(v),
        Err(_) => return None,
    };
    if idx == q.last {
        return None;
    }
    // The descriptors this index publishes were written by the guest before it
    // published the index. Reading them without an acquire here lets this CPU
    // observe the new index against stale descriptor contents.
    std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
    let slot = (q.last % q.size) as u64;
    let r = vm_memory::GuestAddress(q.avail + 4 + slot * 2);
    let h: u16 = match mem.read_obj(r) {
        Ok(v) => u16::from_le(v),
        Err(_) => return None,
    };
    q.last = q.last.wrapping_add(1);
    let descs = collect_descs_with(mem, q.desc, h, q.size, q.indirect);
    Some((h, descs))
}

/// Write a used element and advance the used index.
///
/// Returns the used index after the write, which is what the `EVENT_IDX`
/// interrupt decision compares against.
pub fn push_used(mem: &GuestMemoryMmap, q: &QState, head: u16, len: u32) -> u16 {
    // Same division, same reason. Nothing was taken from a zero-sized queue, so
    // there is nothing to complete on one.
    //
    // And nothing may be written into a used ring at address zero: a request
    // still in flight when the driver resets the device completes against a
    // queue that no longer has one, and this is where those bytes would land.
    if q.size == 0 || q.used == 0 {
        return 0;
    }
    let ua = vm_memory::GuestAddress(q.used + 2);
    let ui: u16 = mem.read_obj(ua).map(u16::from_le).unwrap_or(0);
    let us = (ui % q.size) as u64;
    let ue = vm_memory::GuestAddress(q.used + 4 + us * 8);
    let _ = mem.write_obj(u32::to_le(head as u32), ue);
    let _ = mem.write_obj(u32::to_le(len), vm_memory::GuestAddress(ue.0 + 4));
    // The guest must not see the new used index before the element it points
    // at, or it reads a slot we have not filled in yet.
    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    let next = ui.wrapping_add(1);
    let _ = mem.write_obj(u16::to_le(next), ua);
    next
}

/// Where the driver publishes the used index it wants an interrupt at: past
/// the avail ring's entries.
fn used_event_addr(q: &QState) -> u64 {
    q.avail + 4 + q.size as u64 * 2
}

/// Where the device publishes the avail index it wants a notification at: past
/// the used ring's entries.
fn avail_event_addr(q: &QState) -> u64 {
    q.used + 4 + q.size as u64 * 8
}

/// The spec's `vring_need_event`: has the index the other side is waiting for
/// been passed by this batch?
///
/// All three are free-running 16-bit counters that wrap, so this is written in
/// wrapping differences rather than comparisons -- `new > event` is wrong the
/// moment the ring wraps, and wrong once every 65536 requests is a queue that
/// stalls for no reason anyone will reproduce.
pub fn vring_need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}

/// Ask the driver to notify us when its avail index reaches `idx`.
///
/// The `EVENT_IDX` counterpart to [`set_used_no_notify`], and finer: rather
/// than all-or-nothing, it names the point at which we want to hear again.
pub fn set_avail_event(mem: &GuestMemoryMmap, q: &QState, idx: u16) {
    if q.used == 0 || q.size == 0 {
        return;
    }
    let _ = mem.write_obj(
        u16::to_le(idx),
        vm_memory::GuestAddress(avail_event_addr(q)),
    );
    // The same store-load barrier as `set_used_no_notify`, for the same race:
    // this is a hint the driver reads before deciding whether to kick.
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

/// Does the driver want an interrupt for the used entries just published?
///
/// `old` and `new` are the used index either side of the batch. With
/// `EVENT_IDX` the driver names the index it wants to be woken at and this
/// answers whether the batch passed it; without it, the answer is the avail
/// ring's flag.
pub fn used_needs_interrupt(mem: &GuestMemoryMmap, q: &QState, old: u16, new: u16) -> bool {
    if !q.event_idx {
        return avail_wants_interrupt(mem, q);
    }
    match mem.read_obj::<u16>(vm_memory::GuestAddress(used_event_addr(q))) {
        Ok(event) => vring_need_event(u16::from_le(event), new, old),
        // Unreadable means we cannot tell, and a suppressed interrupt the
        // driver wanted is a stalled queue. Wake it.
        Err(_) => true,
    }
}

/// Does the driver want an interrupt for what we just completed?
///
/// A driver polling its ring -- which Linux does under load, and which is where
/// the interrupts cost the most -- sets `VRING_AVAIL_F_NO_INTERRUPT` and does
/// not want to be woken. Reading it wrong in either direction is safe in only
/// one of them: suppressing an interrupt the driver wanted stalls the queue, so
/// a ring we cannot read is treated as wanting one.
pub fn avail_wants_interrupt(mem: &GuestMemoryMmap, q: &QState) -> bool {
    if q.avail == 0 {
        return true;
    }
    match mem.read_obj::<u16>(vm_memory::GuestAddress(q.avail)) {
        Ok(flags) => u16::from_le(flags) & VRING_AVAIL_F_NO_INTERRUPT == 0,
        Err(_) => true,
    }
}

/// Ask the driver not to notify us, or withdraw that request.
///
/// Every notification is a VM exit. While a worker is draining a queue it is
/// going to pick up whatever the driver adds anyway, so the exits in that
/// window buy nothing. The driver is free to ignore this -- it is a hint -- so
/// a queue must always be re-checked after clearing it, or a request added in
/// the gap sits there with no kick coming.
pub fn set_used_no_notify(mem: &GuestMemoryMmap, q: &QState, suppress: bool) {
    if q.used == 0 {
        return;
    }
    let flags: u16 = if suppress { VRING_USED_F_NO_NOTIFY } else { 0 };
    let _ = mem.write_obj(u16::to_le(flags), vm_memory::GuestAddress(q.used));
    // A full barrier, and it has to be one. x86 lets a store sit in the store
    // buffer past a later load, so without this the caller can clear the flag,
    // read the avail ring as empty and go to sleep, while the driver adds a
    // request and reads the flag *before* the clear reaches it and so does not
    // kick. Both sides then wait for the other. The window is small and the
    // failure is a hung disk, which is the worst kind of rare.
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

/// Write queue address fields.
pub fn write_queue_addr(q: &mut QState, off: u64, v32: u32) {
    match off {
        CFG_QUEUE_DESC_LO => q.desc = (q.desc & 0xFFFF_FFFF_0000_0000) | v32 as u64,
        CFG_QUEUE_DESC_HI => q.desc = (q.desc & 0x0000_0000_FFFF_FFFF) | (v32 as u64) << 32,
        CFG_QUEUE_AVAIL_LO => q.avail = (q.avail & 0xFFFF_FFFF_0000_0000) | v32 as u64,
        CFG_QUEUE_AVAIL_HI => q.avail = (q.avail & 0x0000_0000_FFFF_FFFF) | (v32 as u64) << 32,
        CFG_QUEUE_USED_LO => q.used = (q.used & 0xFFFF_FFFF_0000_0000) | v32 as u64,
        CFG_QUEUE_USED_HI => q.used = (q.used & 0x0000_0000_FFFF_FFFF) | (v32 as u64) << 32,
        _ => {}
    }
}

/// Fire an interrupt via eventfd. Handles MSI-X masked vectors.
pub fn fire_irq_intx(fd: &Option<Arc<EventFd>>) {
    if let Some(f) = fd {
        let _ = f.write(1);
    }
}

// ── PCI config-space helpers ────────────────────────────────────────────────

/// MSI-X Message Control sits two bytes into the capability.
const MSIX_MSG_CTL: u32 = 2;
const MSIX_ENABLE: u16 = 1 << 15;
/// Bits the guest is allowed to change: MSI-X Enable and Function Mask.
const MSIX_MSG_CTL_WRITABLE: u16 = MSIX_ENABLE | (1 << 14);

/// Serve a read from a device's 256-byte config space.
pub fn read_cfg_space(cfg: &[u8; 256], offset: u32, data: &mut [u8]) {
    let s = offset as usize;
    let e = (s + data.len()).min(256);
    if s < 256 {
        data[..e - s].copy_from_slice(&cfg[s..e]);
        data[e - s..].fill(0xff);
    } else {
        data.fill(0xff);
    }
}

/// Apply a config-space write, which for our devices means the writable bits
/// of MSI-X Message Control and nothing else.
pub fn write_msix_control(cfg: &mut [u8; 256], msix_cap: u16, offset: u32, data: &[u8]) {
    let ctl_off = msix_cap as u32 + MSIX_MSG_CTL;
    for (i, byte) in data.iter().enumerate() {
        let off = offset + i as u32;
        if off < ctl_off || off >= ctl_off + 2 {
            continue;
        }
        let lane = (off - ctl_off) as usize; // 0 = low byte, 1 = high byte
        let mask = (MSIX_MSG_CTL_WRITABLE >> (8 * lane)) as u8;
        let idx = off as usize;
        cfg[idx] = (cfg[idx] & !mask) | (byte & mask);
    }
}

/// Is MSI-X enabled in this config space?
pub fn msix_enabled(cfg: &[u8; 256], msix_cap: u16) -> bool {
    let idx = msix_cap as usize + MSIX_MSG_CTL as usize;
    let ctl = u16::from_le_bytes([cfg[idx], cfg[idx + 1]]);
    ctl & MSIX_ENABLE != 0
}

// ── MSI-X table ─────────────────────────────────────────────────────────────

/// A device's MSI-X table plus the host-side vectors it drives.
///
/// Entry layout, 16 bytes per vector: address_lo, address_hi, data, then
/// vector control whose bit 0 is the mask bit.
pub struct MsixTable<const N: usize> {
    pub entries: [[u8; 16]; N],
    pub pba: u64,
    pub enabled: bool,
    /// One host vector per table entry, filled in by [`MsixTable::bind`].
    vectors: Vec<MsiVector>,
    router: Option<Arc<dyn MsiRouter>>,
    /// Legacy INTx line, used until the guest enables MSI-X.
    intx: Option<Arc<EventFd>>,
}

impl<const N: usize> Default for MsixTable<N> {
    fn default() -> Self {
        Self {
            entries: [[0u8; 16]; N],
            pba: 0,
            enabled: false,
            vectors: Vec::new(),
            router: None,
            intx: None,
        }
    }
}

impl<const N: usize> MsixTable<N> {
    /// Attach host interrupt resources: one vector per table entry, the router
    /// that programs them, and the legacy INTx eventfd.
    pub fn bind(
        &mut self,
        vectors: Vec<MsiVector>,
        router: Arc<dyn MsiRouter>,
        intx: Arc<EventFd>,
    ) {
        self.vectors = vectors;
        self.router = Some(router);
        self.intx = Some(intx);
    }

    pub fn read(&self, offset: u64, data: &mut [u8]) {
        let ei = (offset / 16) as usize;
        if ei >= N {
            data.fill(0);
            return;
        }
        let so = (offset % 16) as usize;
        let end = (so + data.len()).min(16);
        data[..end - so].copy_from_slice(&self.entries[ei][so..end]);
        data[end - so..].fill(0);
    }

    /// Write to the MSI-X table, then reprogram the affected vector's route.
    ///
    /// Returns true if the entry went from masked to unmasked while an
    /// interrupt was pending in the PBA, meaning it should be delivered now.
    pub fn write(&mut self, offset: u64, data: &[u8]) -> bool {
        let ei = (offset / 16) as usize;
        if ei >= N {
            return false;
        }
        let so = (offset % 16) as usize;
        let end = (so + data.len()).min(16);
        let was_masked = self.entries[ei][12] & 1 != 0;
        self.entries[ei][so..end].copy_from_slice(&data[..end - so]);
        let now_unmasked = self.entries[ei][12] & 1 == 0;

        self.program_route(ei);

        let had_pending = (self.pba >> ei) & 1 != 0;
        let deliver_now = was_masked && now_unmasked && had_pending;
        if deliver_now {
            self.pba &= !(1 << ei);
        }
        deliver_now
    }

    /// Point this entry's GSI at the address/data pair the guest programmed.
    fn program_route(&self, ei: usize) {
        let e = &self.entries[ei];
        let addr = u64::from(u32::from_le_bytes([e[0], e[1], e[2], e[3]]))
            | (u64::from(u32::from_le_bytes([e[4], e[5], e[6], e[7]])) << 32);
        let msg_data = u32::from_le_bytes([e[8], e[9], e[10], e[11]]);
        if addr == 0 {
            return; // not programmed yet
        }
        let (Some(router), Some(vector)) = (self.router.as_ref(), self.vectors.get(ei)) else {
            return;
        };
        if let Err(err) = router.set_msi_route(vector.gsi, addr, msg_data) {
            log::error!(
                "failed to route MSI-X vector {} (gsi {}): {err:#}",
                ei,
                vector.gsi
            );
        }
    }

    /// Deliver an interrupt for `vector`, honouring the mask and MSI-X enable.
    ///
    /// Falls back to the legacy INTx line while the guest has not enabled
    /// MSI-X, which is how the device gets through early probing.
    pub fn trigger(&mut self, vector: u16) {
        if !self.enabled || vector == VIRTQ_MSI_NO_VECTOR {
            log::trace!(
                "INTx fallback: vector={vector} msix_enabled={}",
                self.enabled
            );
            if let Some(intx) = &self.intx {
                let _ = intx.write(1);
            }
            return;
        }
        let idx = vector as usize;
        if idx >= N {
            return;
        }
        if self.masked(idx) {
            // Record it in the Pending Bit Array; it fires on unmask.
            self.pba |= 1 << idx;
            return;
        }
        match self.vectors.get(idx) {
            Some(v) => v.trigger(),
            None => log::warn!("MSI-X vector {} has no host vector bound", idx),
        }
    }

    /// Deliver an interrupt that a table write just unmasked.
    pub fn trigger_unmasked(&self, idx: usize) {
        if let Some(v) = self.vectors.get(idx) {
            v.trigger();
        }
    }

    pub fn read_pba(&self, offset: u64, data: &mut [u8]) {
        let b = self.pba.to_le_bytes();
        let s = offset as usize;
        let e = (s + data.len()).min(8);
        if s < 8 {
            data[..e - s].copy_from_slice(&b[s..e]);
            data[e - s..].fill(0);
        } else {
            data.fill(0);
        }
    }

    /// The eventfd an interrupt for `vector` would be delivered on: its MSI-X
    /// vector, or the INTx line while MSI-X is disabled.
    ///
    /// Needed by devices whose queues are serviced by a vhost backend, since
    /// the kernel signals completion itself and must be handed the fd.
    pub fn call_fd(&self, vector: u16) -> Option<&Arc<EventFd>> {
        if !self.enabled || vector == VIRTQ_MSI_NO_VECTOR {
            return self.intx.as_ref();
        }
        self.vectors.get(vector as usize).map(|v| &v.irq_fd)
    }

    pub fn masked(&self, idx: usize) -> bool {
        idx < N && self.entries[idx][12] & 1 != 0
    }
}

/// Apply a write to `driver_feature`, honouring the selector the driver set.
///
/// The selector is a *word* index, and a driver walks it past the two words a
/// 64-bit feature set occupies: Linux writes selects 0, 1, 2 and 3, because the
/// spec's feature space is 128 bits wide and it writes all of it. Treating
/// "not zero" as "the high word" therefore does not mean what it looks like --
/// select 2 arrives carrying zero and wipes bits 32..63, and bit 32 is
/// `VIRTIO_F_VERSION_1`.
///
/// The symptom is nothing at all until something is gated on a bit up there,
/// and then a device that believes a modern driver is a legacy one.
pub fn write_driver_feature(com: &mut ComCfg, value: u32) {
    match com.dff {
        0 => com.df = (com.df & 0xFFFF_FFFF_0000_0000) | value as u64,
        1 => com.df = (com.df & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32),
        // Words this device has no features in. A driver acknowledging bits it
        // was never offered is its own business; storing them is not.
        _ => {}
    }
}

/// Read common-cfg fields.
pub fn com_read(
    com: &ComCfg,
    off: u64,
    device_features: u64,
    num_queues: u64,
    msix_config_vec: u64,
    qsel: u64,
    qsize: u64,
    qvec: u64,
    qen: u64,
    qnoff: u64,
    qdlo: u64,
    qdhi: u64,
    qalo: u64,
    qahi: u64,
    qulo: u64,
    quhi: u64,
) -> u64 {
    match off {
        CFG_DEVICE_FEAT_SEL => com.dfs as u64,
        // The same word indices as `write_driver_feature`, and the same trap:
        // a driver walks the selector past the two words a 64-bit feature set
        // occupies, and "not word zero" as "the high word" would answer word 2
        // with word 1's contents -- offering feature bits 64 and beyond that
        // this device has never heard of.
        CFG_DEVICE_FEAT => match com.dfs {
            0 => device_features & 0xFFFF_FFFF,
            1 => device_features >> 32,
            _ => 0,
        },
        CFG_DRIVER_FEAT_SEL => com.dff as u64,
        CFG_DRIVER_FEAT => match com.dff {
            0 => com.df & 0xFFFF_FFFF,
            1 => com.df >> 32,
            // Same word indices as `write_driver_feature`: there is nothing to
            // read back above them.
            _ => 0,
        },
        CFG_MSIX_CONFIG => msix_config_vec,
        CFG_NUM_QUEUES => num_queues,
        CFG_STATUS => com.st as u64,
        CFG_QUEUE_SEL => qsel,
        CFG_QUEUE_SIZE => qsize,
        CFG_QUEUE_MSIX => qvec,
        CFG_QUEUE_ENABLE => qen,
        CFG_QUEUE_NOTIFY_OFF => qnoff,
        CFG_QUEUE_DESC_LO => qdlo,
        CFG_QUEUE_DESC_HI => qdhi,
        CFG_QUEUE_AVAIL_LO => qalo,
        CFG_QUEUE_AVAIL_HI => qahi,
        CFG_QUEUE_USED_LO => qulo,
        CFG_QUEUE_USED_HI => quhi,
        _ => 0,
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;
    use vm_memory::GuestAddress;

    const MAX: u16 = 256;

    fn mem() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap()
    }

    /// A guest that sizes a queue to zero used to panic the VMM: `q.last %
    /// q.size` is a division. Three MMIO writes, from inside the guest, and the
    /// box is gone.
    #[test]
    fn a_zero_sized_queue_is_refused_rather_than_dividing_by_it() {
        // `last` differs from the avail index the guest published (zero), so
        // this gets past the is-it-empty check and reaches the remainder. With
        // `last == 0` the queue reads as empty and the division is never tried,
        // which would make this test pass against the bug it exists for.
        let mut q = QState {
            size: 0,
            enabled: true,
            last: 1,
            ..Default::default()
        };
        assert!(pop_avail(&mem(), &mut q).is_none());
        // And completing on one is a no-op rather than the same division.
        push_used(&mem(), &q, 0, 0);
    }

    /// Zero is kept as written -- it means "unused", and rewriting it to the
    /// maximum would hand a guest a live queue it asked not to have.
    #[test]
    fn zero_is_stored_not_rewritten() {
        let mut q = QState::default();
        set_queue_size(&mut q, 0, MAX);
        assert_eq!(q.size, 0);
    }

    /// The advertised maximum is what bounds the chain walk, so a guest must not
    /// be able to raise it: 65535 descriptors of up to 4 GiB each is host memory
    /// `Reader` would commit.
    #[test]
    fn a_size_above_the_advertised_maximum_is_clamped() {
        let mut q = QState::default();
        set_queue_size(&mut q, u16::MAX, MAX);
        assert_eq!(q.size, MAX);
    }

    #[test]
    fn an_ordinary_size_is_taken_verbatim() {
        let mut q = QState::default();
        set_queue_size(&mut q, 128, MAX);
        assert_eq!(q.size, 128);
        set_queue_size(&mut q, MAX, MAX);
        assert_eq!(q.size, MAX);
    }

    /// Write one descriptor into a table in guest memory.
    fn desc(m: &GuestMemoryMmap, table: u64, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let at = table + idx as u64 * VRING_DESC_SIZE;
        m.write_obj(u64::to_le(addr), GuestAddress(at)).unwrap();
        m.write_obj(u32::to_le(len), GuestAddress(at + 8)).unwrap();
        m.write_obj(u16::to_le(flags), GuestAddress(at + 12))
            .unwrap();
        m.write_obj(u16::to_le(next), GuestAddress(at + 14))
            .unwrap();
    }

    const TABLE: u64 = 0x1000;
    const INDIRECT: u64 = 0x4000;

    /// An indirect descriptor is a table of descriptors, and the chain the
    /// device serves is the one inside it.
    #[test]
    fn an_indirect_table_is_followed_when_the_driver_negotiated_it() {
        let m = mem();
        desc(&m, TABLE, 0, INDIRECT, 32, VRING_DESC_F_INDIRECT, 0);
        desc(&m, INDIRECT, 0, 0x8000, 512, VRING_DESC_F_NEXT, 1);
        desc(&m, INDIRECT, 1, 0x9000, 1, VRING_DESC_F_WRITE, 0);

        let descs = collect_descs_with(&m, TABLE, 0, MAX, true);
        assert_eq!(descs.len(), 2);
        assert_eq!(descs[0].0, 0x8000);
        assert_eq!(descs[1], (0x9000, 1, VRING_DESC_F_WRITE));
    }

    /// A driver that did not accept `VIRTIO_F_RING_INDIRECT_DESC` cannot have
    /// meant the flag. Following it anyway would read one of its buffers as a
    /// descriptor table.
    #[test]
    fn an_indirect_descriptor_is_ignored_when_the_feature_was_not_negotiated() {
        let m = mem();
        desc(&m, TABLE, 0, INDIRECT, 32, VRING_DESC_F_INDIRECT, 0);
        desc(&m, INDIRECT, 0, 0x8000, 512, 0, 0);

        assert!(collect_descs_with(&m, TABLE, 0, MAX, false).is_empty());
        // And the plain entry point never follows one.
        assert!(collect_descs(&m, TABLE, 0, MAX).is_empty());
    }

    /// Nesting is forbidden by the spec, and is the one place this walk could
    /// recurse without a bound.
    #[test]
    fn an_indirect_table_may_not_contain_another_one() {
        let m = mem();
        desc(&m, TABLE, 0, INDIRECT, 32, VRING_DESC_F_INDIRECT, 0);
        desc(&m, INDIRECT, 0, 0x6000, 32, VRING_DESC_F_INDIRECT, 0);
        desc(&m, 0x6000, 0, 0x8000, 512, 0, 0);

        assert!(collect_descs_with(&m, TABLE, 0, MAX, true).is_empty());
    }

    /// The table's length is the driver's claim about its own memory. A claim
    /// larger than the ring it advertised is clamped, not believed: it is what
    /// bounds how much this walk collects.
    #[test]
    fn an_indirect_table_longer_than_the_ring_is_clamped() {
        let m = mem();
        let small: u16 = 4;
        desc(&m, TABLE, 0, INDIRECT, u32::MAX, VRING_DESC_F_INDIRECT, 0);
        for i in 0..8u16 {
            desc(&m, INDIRECT, i, 0x8000, 16, VRING_DESC_F_NEXT, i + 1);
        }
        let descs = collect_descs_with(&m, TABLE, 0, small, true);
        assert!(descs.len() <= small as usize, "collected {}", descs.len());
    }

    /// A driver that wants no interrupts must not get one, and a ring we cannot
    /// read must not cost the guest a stalled queue.
    #[test]
    fn the_avail_flags_decide_whether_an_interrupt_is_wanted() {
        let m = mem();
        let q = QState {
            size: MAX,
            avail: 0x2000,
            ..Default::default()
        };
        m.write_obj(u16::to_le(0), GuestAddress(q.avail)).unwrap();
        assert!(avail_wants_interrupt(&m, &q));
        m.write_obj(
            u16::to_le(VRING_AVAIL_F_NO_INTERRUPT),
            GuestAddress(q.avail),
        )
        .unwrap();
        assert!(!avail_wants_interrupt(&m, &q));

        let unreadable = QState {
            size: MAX,
            avail: 0xFFFF_0000,
            ..Default::default()
        };
        assert!(avail_wants_interrupt(&m, &unreadable));
    }

    /// Linux writes feature-select words 0 through 3, and the two it has no
    /// features in carry zero. Treating every non-zero selector as "the high
    /// word" makes select 2 wipe `VIRTIO_F_VERSION_1` back out again, and the
    /// device then has a modern driver recorded as a legacy one.
    #[test]
    fn a_selector_past_the_feature_words_does_not_wipe_them() {
        let mut com = ComCfg {
            dff: 0,
            ..Default::default()
        };
        write_driver_feature(&mut com, 0x1000_1066);
        com.dff = 1;
        write_driver_feature(&mut com, 1); // VIRTIO_F_VERSION_1
        assert_eq!(com.df, 0x1_1000_1066);

        for select in 2..4 {
            com.dff = select;
            write_driver_feature(&mut com, 0);
        }
        assert_eq!(com.df, 0x1_1000_1066, "a high selector wrote through");
        assert!(com.df & VIRTIO_F_VERSION_1 != 0);
    }

    /// A driver reads device features word by word, and walks past the two a
    /// 64-bit set occupies. Answering word 2 with word 1's contents offers
    /// feature bits 64 and up -- bits no device here has, and no driver can
    /// make sense of.
    #[test]
    fn device_features_past_the_second_word_are_empty() {
        let features = VIRTIO_F_VERSION_1 | VIRTIO_F_RING_EVENT_IDX;
        let read = |dfs: u32| {
            let com = ComCfg {
                dfs,
                ..Default::default()
            };
            com_read(
                &com,
                CFG_DEVICE_FEAT,
                features,
                1,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            )
        };
        assert_eq!(read(0), 1 << 29, "EVENT_IDX lives in the low word");
        assert_eq!(read(1), 1, "VERSION_1 is bit 32, so bit 0 of the high word");
        assert_eq!(read(2), 0);
        assert_eq!(read(3), 0);
    }

    /// What the driver wrote is what it reads back, per word.
    #[test]
    fn driver_features_read_back_by_word() {
        let com = ComCfg {
            df: 0x1_1000_1066,
            dff: 0,
            ..Default::default()
        };
        assert_eq!(
            com_read(
                &com,
                CFG_DRIVER_FEAT,
                0,
                1,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0
            ),
            0x1000_1066
        );
        let com = ComCfg { dff: 1, ..com };
        assert_eq!(
            com_read(
                &com,
                CFG_DRIVER_FEAT,
                0,
                1,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0
            ),
            1
        );
    }

    /// The three indices are free-running 16-bit counters that wrap. Written
    /// as comparisons this is right for 65535 requests out of every 65536, and
    /// the one it is wrong for is a queue that stalls with nothing to see.
    #[test]
    fn the_event_check_survives_the_counter_wrapping() {
        // The ordinary case: the batch carried the used index past the point
        // the driver asked to be woken at.
        assert!(vring_need_event(5, 6, 4));
        // The driver wants a later index than this batch reached.
        assert!(!vring_need_event(9, 6, 4));
        // `event` is the index the driver has consumed up to, so reaching it
        // is not enough -- the batch has to carry past it. Getting this the
        // other way round means an interrupt per request, which is the cost
        // the feature exists to remove.
        assert!(!vring_need_event(6, 6, 5));
        assert!(vring_need_event(6, 7, 6));

        // Across the wrap: old 65534, new 1, driver waiting at 65535.
        assert!(vring_need_event(u16::MAX, 1, u16::MAX - 1));
        // And not yet, across the wrap: waiting at 3, batch reached 1.
        assert!(!vring_need_event(3, 1, u16::MAX - 1));
    }

    /// With `EVENT_IDX` the driver names the used index it wants an interrupt
    /// at, and the ring's flags stop meaning anything to either side.
    #[test]
    fn event_idx_replaces_the_interrupt_flag() {
        let m = mem();
        let q = QState {
            size: 8,
            avail: 0x2000,
            used: 0x3000,
            event_idx: true,
            ..Default::default()
        };
        // `used_event` sits past the avail ring's entries.
        let used_event = GuestAddress(q.avail + 4 + 8 * 2);
        m.write_obj(u16::to_le(4), used_event).unwrap();
        // The flag says "no interrupt", and with the feature negotiated that
        // must not be what decides.
        m.write_obj(
            u16::to_le(VRING_AVAIL_F_NO_INTERRUPT),
            GuestAddress(q.avail),
        )
        .unwrap();

        assert!(
            used_needs_interrupt(&m, &q, 3, 5),
            "batch passed used_event"
        );
        assert!(
            !used_needs_interrupt(&m, &q, 1, 2),
            "batch did not reach it"
        );

        // Without the feature, the flag is exactly what decides.
        let q = QState {
            event_idx: false,
            ..q
        };
        assert!(!used_needs_interrupt(&m, &q, 3, 5));
    }

    /// The device's half: it publishes the avail index it wants to hear about
    /// next, past the used ring's entries.
    #[test]
    fn the_device_publishes_where_it_wants_the_next_notification() {
        let m = mem();
        let q = QState {
            size: 8,
            avail: 0x2000,
            used: 0x3000,
            event_idx: true,
            ..Default::default()
        };
        set_avail_event(&m, &q, 12);
        let at = GuestAddress(q.used + 4 + 8 * 8);
        assert_eq!(u16::from_le(m.read_obj::<u16>(at).unwrap()), 12);
    }

    /// `next` must be below the queue size. An index past the ring is off the
    /// table the guest published, so the walk stops instead of following it.
    #[test]
    fn a_chain_head_past_the_ring_yields_nothing() {
        assert!(collect_descs(&mem(), 0x1000, MAX, MAX).is_empty());
        assert!(collect_descs(&mem(), 0x1000, u16::MAX, MAX).is_empty());
    }
}
