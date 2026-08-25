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
pub const VIRTQ_MSI_NO_VECTOR: u16 = 0xFFFF;
pub const VRING_DESC_SIZE: u64 = 16;

#[derive(Default, Clone)]
pub struct QState {
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    pub size: u16,
    pub enabled: bool,
    pub last: u16,
    pub vec: u16,
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

/// Collect descriptors from a vring chain.
pub fn collect_descs(
    mem: &GuestMemoryMmap,
    desc_base: u64,
    head: u16,
    max: u16,
) -> Vec<(u64, u32, u16)> {
    let mut descs = Vec::new();
    let mut idx = head;
    for _ in 0..max {
        let base = vm_memory::GuestAddress(desc_base + idx as u64 * VRING_DESC_SIZE);
        let addr: u64 = match mem.read_obj(base) {
            Ok(v) => u64::from_le(v),
            Err(_) => break,
        };
        let len: u32 = match mem.read_obj(vm_memory::GuestAddress(base.0 + 8)) {
            Ok(v) => u32::from_le(v),
            Err(_) => break,
        };
        let fl: u16 = match mem.read_obj(vm_memory::GuestAddress(base.0 + 12)) {
            Ok(v) => u16::from_le(v),
            Err(_) => break,
        };
        let nx: u16 = match mem.read_obj(vm_memory::GuestAddress(base.0 + 14)) {
            Ok(v) => u16::from_le(v),
            Err(_) => break,
        };
        descs.push((addr, len, fl));
        if fl & VRING_DESC_F_NEXT == 0 {
            break;
        }
        idx = nx;
    }
    descs
}

/// Read one avail descriptor from the queue. Returns None if empty.
pub fn pop_avail(mem: &GuestMemoryMmap, q: &mut QState) -> Option<(u16, Vec<(u64, u32, u16)>)> {
    let a = vm_memory::GuestAddress(q.avail + 2);
    let idx: u16 = match mem.read_obj(a) {
        Ok(v) => u16::from_le(v),
        Err(_) => return None,
    };
    if idx == q.last {
        return None;
    }
    let slot = (q.last % q.size) as u64;
    let r = vm_memory::GuestAddress(q.avail + 4 + slot * 2);
    let h: u16 = match mem.read_obj(r) {
        Ok(v) => u16::from_le(v),
        Err(_) => return None,
    };
    q.last = q.last.wrapping_add(1);
    let descs = collect_descs(mem, q.desc, h, q.size);
    Some((h, descs))
}

/// Write a used element and advance the used index.
pub fn push_used(mem: &GuestMemoryMmap, q: &QState, head: u16, len: u32) {
    let ua = vm_memory::GuestAddress(q.used + 2);
    let ui: u16 = mem.read_obj(ua).map(u16::from_le).unwrap_or(0);
    let us = (ui % q.size) as u64;
    let ue = vm_memory::GuestAddress(q.used + 4 + us * 8);
    let _ = mem.write_obj(u32::to_le(head as u32), ue);
    let _ = mem.write_obj(u32::to_le(len), vm_memory::GuestAddress(ue.0 + 4));
    let _ = mem.write_obj(u16::to_le(ui.wrapping_add(1)), ua);
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
        CFG_DEVICE_FEAT => {
            if com.dfs == 0 {
                device_features & 0xFFFF_FFFF
            } else {
                device_features >> 32
            }
        }
        CFG_DRIVER_FEAT_SEL => com.dff as u64,
        CFG_DRIVER_FEAT => {
            if com.dff == 0 {
                com.df & 0xFFFF_FFFF
            } else {
                com.df >> 32
            }
        }
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
