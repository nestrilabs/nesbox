//! Shared constants and types for virtio-pci devices.
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
        let addr: u64 = match mem.read_obj(base) { Ok(v) => u64::from_le(v), Err(_) => break };
        let len: u32 = match mem.read_obj(vm_memory::GuestAddress(base.0 + 8)) { Ok(v) => u32::from_le(v), Err(_) => break };
        let fl: u16 = match mem.read_obj(vm_memory::GuestAddress(base.0 + 12)) { Ok(v) => u16::from_le(v), Err(_) => break };
        let nx: u16 = match mem.read_obj(vm_memory::GuestAddress(base.0 + 14)) { Ok(v) => u16::from_le(v), Err(_) => break };
        descs.push((addr, len, fl));
        if fl & VRING_DESC_F_NEXT == 0 { break; }
        idx = nx;
    }
    descs
}

/// Read one avail descriptor from the queue. Returns None if empty.
pub fn pop_avail(mem: &GuestMemoryMmap, q: &mut QState) -> Option<(u16, Vec<(u64, u32, u16)>)> {
    let a = vm_memory::GuestAddress(q.avail + 2);
    let idx: u16 = match mem.read_obj(a) { Ok(v) => u16::from_le(v), Err(_) => return None };
    if idx == q.last { return None; }
    let slot = (q.last % q.size) as u64;
    let r = vm_memory::GuestAddress(q.avail + 4 + slot * 2);
    let h: u16 = match mem.read_obj(r) { Ok(v) => u16::from_le(v), Err(_) => return None };
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
    if let Some(f) = fd { let _ = f.write(1); }
}

// ── MSI-X table helpers ─────────────────────────────────────────────────────

pub struct MsixTable<const N: usize> {
    pub entries: [[u8; 16]; N],
    pub pba: u64,
    pub enabled: bool,
}

impl<const N: usize> Default for MsixTable<N> {
    fn default() -> Self { Self { entries: [[0u8; 16]; N], pba: 0, enabled: false } }
}

impl<const N: usize> MsixTable<N> {
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        let ei = (offset / 16) as usize;
        if ei >= N { data.fill(0); return; }
        let so = (offset % 16) as usize;
        let end = (so + data.len()).min(16);
        data[..end - so].copy_from_slice(&self.entries[ei][so..end]);
        data[end - so..].fill(0);
    }

    /// Write to the MSI-X table. Returns true if a previously-masked vector is now unmasked AND had a PBA bit set.
    pub fn write(&mut self, offset: u64, data: &[u8]) -> bool {
        let ei = (offset / 16) as usize;
        if ei >= N { return false; }
        let so = (offset % 16) as usize;
        let end = (so + data.len()).min(16);
        let was_masked = self.entries[ei][12] & 1 != 0;
        self.entries[ei][so..end].copy_from_slice(&data[..end - so]);
        let now_unmasked = self.entries[ei][12] & 1 == 0;
        let had_pending = (self.pba >> ei) & 1 != 0;
        let result = was_masked && now_unmasked && had_pending;
        if result { self.pba &= !(1 << ei); }
        result
    }

    pub fn read_pba(&self, offset: u64, data: &mut [u8]) {
        let b = self.pba.to_le_bytes();
        let s = offset as usize;
        let e = (s + data.len()).min(8);
        if s < 8 { data[..e - s].copy_from_slice(&b[s..e]); data[e - s..].fill(0); } else { data.fill(0); }
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
    qsel: u64, qsize: u64, qvec: u64, qen: u64, qnoff: u64,
    qdlo: u64, qdhi: u64, qalo: u64, qahi: u64, qulo: u64, quhi: u64,
) -> u64 {
    match off {
        CFG_DEVICE_FEAT_SEL => com.dfs as u64,
        CFG_DEVICE_FEAT => if com.dfs == 0 { device_features & 0xFFFF_FFFF } else { device_features >> 32 },
        CFG_DRIVER_FEAT_SEL => com.dff as u64,
        CFG_DRIVER_FEAT => if com.dff == 0 { com.df & 0xFFFF_FFFF } else { com.df >> 32 },
        CFG_MSIX_CONFIG => msix_config_vec,
        CFG_NUM_QUEUES => num_queues,
        CFG_STATUS => com.st as u64,
        CFG_QUEUE_SEL => qsel,
        CFG_QUEUE_SIZE => qsize,
        CFG_QUEUE_MSIX => qvec,
        CFG_QUEUE_ENABLE => qen,
        CFG_QUEUE_NOTIFY_OFF => qnoff,
        CFG_QUEUE_DESC_LO => qdlo, CFG_QUEUE_DESC_HI => qdhi,
        CFG_QUEUE_AVAIL_LO => qalo, CFG_QUEUE_AVAIL_HI => qahi,
        CFG_QUEUE_USED_LO => qulo, CFG_QUEUE_USED_HI => quhi,
        _ => 0,
    }
}
