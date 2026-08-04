//! Virtio block device over PCI transport (virtio 1.0 / modern).

use crate::common::*;
use anyhow::{Context, Result};
use pci::config::PciConfig;
use pci::{MsiRouter, MsiVector, PciDevice};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};
use vm_memory::{Bytes, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

const QUEUE_SIZE: u16 = 256;
/// One vector for the queue, one for config changes.
const MSIX_VECTORS: u16 = 2;
const SECTOR_SIZE: u64 = 512;

const BLK_T_IN: u32 = 0;
const BLK_T_OUT: u32 = 1;
const BLK_T_FLUSH: u32 = 4;
const BLK_T_GET_ID: u32 = 8;
const BLK_S_OK: u8 = 0;
const BLK_S_IOERR: u8 = 1;
const BLK_S_UNSUPP: u8 = 2;

struct BlkInner {
    file: File,
    read_only: bool,
    disk_sectors: u64,
    com: ComCfg,
    qs: u16,
    q: QState,
    isr: u8,
    /// MSI-X vector for configuration-change interrupts.
    cfg_vec: u16,
    mem: Option<Arc<GuestMemoryMmap>>,
    msix: MsixTable<2>,
    /// Config space, built once; the guest can write MSI-X Message Control.
    cfg: [u8; 256],
    /// Config-space offset of the MSI-X capability.
    msix_cap: u16,
}

impl BlkInner {
    fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | (1 << 1) | (1 << 2) | (1 << 6) | (1 << 9)
    }

    fn process(&mut self) {
        let mem = match self.mem.clone() { Some(m) => m, None => return };
        if !self.q.enabled || self.q.desc == 0 { return; }
        loop {
            let Some((head, descs)) = pop_avail(&mem, &mut self.q) else { break };
            let n = self.do_request(&mem, head, &descs);
            push_used(&mem, &self.q, head, n);
        }
        self.isr |= 1;
        let vec = self.q.vec;
        self.msix.trigger(vec);
    }

    fn do_request(&mut self, mem: &GuestMemoryMmap, _head: u16, descs: &[(u64, u32, u16)]) -> u32 {
        if descs.len() < 2 { return 0; }
        let (ha, hl, _) = descs[0];
        if hl < 16 { return 0; }
        let t: u32 = match mem.read_obj(vm_memory::GuestAddress(ha)) { Ok(v) => u32::from_le(v), Err(_) => return 0 };
        let sec: u64 = match mem.read_obj(vm_memory::GuestAddress(ha + 8)) { Ok(v) => u64::from_le(v), Err(_) => return 0 };
        let (sa, _, _) = *descs.last().unwrap();
        let dd = &descs[1..descs.len() - 1];
        let st = self.io(mem, t, sec, dd);
        let _ = mem.write_obj(st, vm_memory::GuestAddress(sa));
        dd.iter().map(|(_, l, _)| *l).sum::<u32>() + 1
    }

    fn io(&mut self, mem: &GuestMemoryMmap, t: u32, sec: u64, dd: &[(u64, u32, u16)]) -> u8 {
        match t {
            BLK_T_IN => {
                if self.file.seek(SeekFrom::Start(sec * SECTOR_SIZE)).is_err() { return BLK_S_IOERR; }
                for &(addr, len, _) in dd {
                    let mut buf = vec![0u8; len as usize];
                    if self.file.read_exact(&mut buf).is_err() { return BLK_S_IOERR; }
                    if mem.write_slice(&buf, vm_memory::GuestAddress(addr)).is_err() { return BLK_S_IOERR; }
                }
                BLK_S_OK
            }
            BLK_T_OUT => {
                if self.read_only { return BLK_S_IOERR; }
                if self.file.seek(SeekFrom::Start(sec * SECTOR_SIZE)).is_err() { return BLK_S_IOERR; }
                for &(addr, len, _) in dd {
                    let mut buf = vec![0u8; len as usize];
                    if mem.read_slice(&mut buf, vm_memory::GuestAddress(addr)).is_err() { return BLK_S_IOERR; }
                    if self.file.write_all(&buf).is_err() { return BLK_S_IOERR; }
                }
                BLK_S_OK
            }
            BLK_T_FLUSH => { let _ = self.file.flush(); BLK_S_OK }
            BLK_T_GET_ID => {
                let id = b"nesbox-vda\0\0\0\0\0\0\0\0\0\0";
                for &(addr, len, _) in dd {
                    let n = (len as usize).min(id.len());
                    let _ = mem.write_slice(&id[..n], vm_memory::GuestAddress(addr));
                }
                BLK_S_OK
            }
            _ => BLK_S_UNSUPP,
        }
    }
}

pub struct BlkDevice { inner: Mutex<BlkInner> }

impl BlkDevice {
    pub fn new(path: &std::path::Path, read_only: bool) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)
            .with_context(|| format!("Failed to open block device: {:?}", path))?;
        let disk_sectors = file.metadata().context("Failed to stat disk image")?.len() / SECTOR_SIZE;
        log::info!("virtio-blk: {:?}  {} sectors  read_only={}", path, disk_sectors, read_only);
        let (cfg, msix_cap) = Self::build_pci_config();
        Ok(Self { inner: Mutex::new(BlkInner {
            file, read_only, disk_sectors, com: ComCfg::default(), qs: 0,
            q: QState { size: QUEUE_SIZE, ..Default::default() }, isr: 0,
            cfg_vec: VIRTQ_MSI_NO_VECTOR, mem: None, msix: MsixTable::default(), cfg, msix_cap,
        })})
    }
    pub fn set_mem(&self, m: Arc<GuestMemoryMmap>) { self.inner.lock().unwrap().mem = Some(m); }

    /// Attach the host interrupt resources: one MSI-X vector per table entry
    /// and the legacy INTx line used before the guest enables MSI-X.
    pub fn bind_interrupts(&self, vectors: Vec<MsiVector>, router: Arc<dyn MsiRouter>, intx: Arc<EventFd>) {
        self.inner.lock().unwrap().msix.bind(vectors, router, intx);
    }

    fn build_pci_config() -> ([u8; 256], u16) {
        let mut cfg = PciConfig::new(0x1AF4, 0x1042, 0x01, 0x01_00_00, 0x1AF4, 0x0002);
        cfg.set_bar_mem(0, BAR0_SIZE);
        cfg.set_irq_pin(1);
        cfg.set_irq_line(10);
        cfg.add_virtio_cap(1, 0, OFF_COMMON as u32, 0x38);
        cfg.add_virtio_notify_cap(0, OFF_NOTIFY as u32, 0x100, NOTIFY_MULT);
        cfg.add_virtio_cap(3, 0, OFF_ISR as u32, 1);
        cfg.add_virtio_cap(4, 0, OFF_DEVICE as u32, 0x3C);
        let msix_cap = cfg.add_msix_cap(MSIX_VECTORS - 1, OFF_MSIX_TABLE as u32, OFF_MSIX_PBA as u32);
        (cfg.build(), msix_cap)
    }

    fn com_read(&self, off: u64, d: &mut [u8]) {
        let i = self.inner.lock().unwrap();
        let v = com_read(&i.com, off, i.features(), 1, i.cfg_vec as u64,
            i.qs as u64, i.q.size as u64, i.q.vec as u64, i.q.enabled as u64, 0,
            i.q.desc & 0xFFFF_FFFF, i.q.desc >> 32, i.q.avail & 0xFFFF_FFFF, i.q.avail >> 32, i.q.used & 0xFFFF_FFFF, i.q.used >> 32);
        write_val(d, v);
    }

    fn com_write(&self, off: u64, d: &[u8]) {
        let (v3, v2, v1) = parse_write(d);
        let mut i = self.inner.lock().unwrap();
        match off {
            CFG_DEVICE_FEAT_SEL => i.com.dfs = v3,
            CFG_DRIVER_FEAT_SEL => i.com.dff = v3,
            CFG_DRIVER_FEAT => if i.com.dff == 0 { i.com.df = (i.com.df & 0xFFFF_FFFF_0000_0000) | (v3 as u64) } else { i.com.df = (i.com.df & 0xFFFF_FFFF) | ((v3 as u64) << 32) },
            CFG_MSIX_CONFIG => i.cfg_vec = v2,
            CFG_STATUS => { let old = i.com.st; i.com.st = v1; if old & 4 == 0 && v1 & STATUS_DRIVER_OK != 0 { log::info!("virtio-blk: Driver OK"); } },
            CFG_QUEUE_SEL => i.qs = v2,
            CFG_QUEUE_SIZE => if i.qs == 0 { i.q.size = v2; },
            CFG_QUEUE_MSIX => i.q.vec = v2,
            CFG_QUEUE_ENABLE => if i.qs == 0 { i.q.enabled = v2 != 0; },
            _ => write_queue_addr(&mut i.q, off, v3),
        }
    }

    fn bar0_read(&self, o: u64, d: &mut [u8]) {
        if o < OFF_ISR { self.com_read(o - OFF_COMMON, d); }
        else if o < OFF_DEVICE { let mut i = self.inner.lock().unwrap(); if !d.is_empty() { d[0] = i.isr; i.isr = 0; } }
        else if o < OFF_NOTIFY {
            let i = self.inner.lock().unwrap();
            let mut dd = [0u8; 56];
            dd[0..8].copy_from_slice(&i.disk_sectors.to_le_bytes());
            let seg_max = (QUEUE_SIZE as u32).saturating_sub(2);
            dd[12..16].copy_from_slice(&seg_max.to_le_bytes());
            dd[20..24].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
            let s = (o - OFF_DEVICE) as usize; let e = (s + d.len()).min(56);
            if s < 56 { d[..e-s].copy_from_slice(&dd[s..e]); d[e-s..].fill(0); } else { d.fill(0); }
        }
        else if o < OFF_MSIX_TABLE { d.fill(0); }
        else if o < OFF_MSIX_PBA { self.inner.lock().unwrap().msix.read(o - OFF_MSIX_TABLE, d); }
        else if o < BAR0_SIZE { self.inner.lock().unwrap().msix.read_pba(o - OFF_MSIX_PBA, d); }
        else { d.fill(0); }
    }

    fn bar0_write(&self, o: u64, d: &[u8]) {
        if o < OFF_ISR { self.com_write(o - OFF_COMMON, d); }
        else if o < OFF_DEVICE {}
        else if o < OFF_NOTIFY {}
        else if o < OFF_MSIX_TABLE { self.inner.lock().unwrap().process(); }
        else if o < OFF_MSIX_PBA {
            let mut i = self.inner.lock().unwrap();
            if i.msix.write(o - OFF_MSIX_TABLE, d) {
                i.msix.trigger_unmasked(((o - OFF_MSIX_TABLE) / 16) as usize);
            }
        }
    }
}

impl PciDevice for BlkDevice {
    fn read_config(&self, o: u32, d: &mut [u8]) {
        let i = self.inner.lock().unwrap();
        read_cfg_space(&i.cfg, o, d);
    }
    fn write_config(&self, o: u32, d: &[u8]) {
        let mut i = self.inner.lock().unwrap();
        let cap = i.msix_cap;
        write_msix_control(&mut i.cfg, cap, o, d);
        i.msix.enabled = msix_enabled(&i.cfg, cap);
    }
    fn read_bar(&self, bi: usize, o: u64, d: &mut [u8]) -> bool { if bi == 0 { self.bar0_read(o, d); true } else { false } }
    fn write_bar(&self, bi: usize, o: u64, d: &[u8]) -> bool { if bi == 0 { self.bar0_write(o, d); true } else { false } }
    fn bar_size(&self, bi: usize) -> u64 { if bi == 0 { BAR0_SIZE } else { 0 } }
}
