//! Virtio console device over PCI transport (virtio 1.0 / modern).

use crate::common::*;
use pci::config::{PCIE_TYPE_RC_INTEGRATED, PciConfig};
use pci::{MsiRouter, MsiVector, PciDevice};
use std::io::Read;
use std::io::Write as IoWrite;
use std::sync::{Arc, Mutex};
use vm_memory::Bytes;
use vmm_sys_util::eventfd::EventFd;

const QUEUE_SIZE: u16 = 256;
/// rx, tx, and a config vector, rounded up to a power of two.
const MSIX_VECTORS: u16 = 4;

struct Inner {
    com: ComCfg,
    qs: u16,
    rx: QState,
    tx: QState,
    isr: u8,
    /// MSI-X vector for configuration-change interrupts.
    cfg_vec: u16,
    mem: Option<Arc<vm_memory::GuestMemoryMmap>>,
    msix: MsixTable<4>,
    cfg: [u8; 256],
    msix_cap: u16,
    stdin_buf: Arc<Mutex<Vec<u8>>>,
    cols: u16,
    rows: u16,
}

impl Inner {
    fn sq(&self) -> &QState { if self.qs == 1 { &self.tx } else { &self.rx } }
    fn sqm(&mut self) -> &mut QState { if self.qs == 1 { &mut self.tx } else { &mut self.rx } }

    fn process_tx(&mut self) {
        let mem = match self.mem.clone() { Some(m) => m, None => return };
        if !self.tx.enabled || self.tx.desc == 0 { return; }
        // Held across the batch so interleaved output cannot tear, and flushed
        // at the end: stdout is line-buffered on a terminal, which for a
        // console means the guest's echo of a keystroke sits here until the
        // guest happens to emit a newline. Typing looks dead, and anything
        // without a trailing newline — a shell prompt, `clear` — appears only
        // once the *next* command produces output.
        let mut out = std::io::stdout().lock();
        let mut wrote = false;
        loop {
            let Some((head, descs)) = pop_avail(&mem, &mut self.tx) else { break };
            let mut total = 0u32;
            for &(addr, len, flags) in &descs {
                if flags & VRING_DESC_F_WRITE == 0 {
                    let mut buf = vec![0u8; len as usize];
                    if mem.read_slice(&mut buf, vm_memory::GuestAddress(addr)).is_ok() {
                        let _ = out.write_all(&buf);
                        wrote = true;
                        total += len;
                    }
                }
            }
            push_used(&mem, &self.tx, head, total);
        }
        if wrote {
            let _ = out.flush();
        }
        self.isr |= 1;
        let vec = self.tx.vec;
        self.msix.trigger(vec);
    }

    /// Move buffered host input into the guest's receive queue.
    fn process_rx(&mut self) {
        let Some(mem) = self.mem.clone() else { return };
        if !self.rx.enabled || self.rx.desc == 0 {
            return;
        }
        let stdin_buf = self.stdin_buf.clone();
        let mut delivered = 0u32;

        loop {
            if stdin_buf.lock().unwrap().is_empty() {
                break;
            }
            // Only take a buffer off the queue when there is input for it;
            // completing spare buffers with zero length just burns them.
            let Some((head, descs)) = pop_avail(&mem, &mut self.rx) else { break };
            let mut written = 0u32;
            for &(addr, len, flags) in &descs {
                if flags & VRING_DESC_F_WRITE == 0 {
                    continue;
                }
                let data: Vec<u8> = {
                    let mut buf = stdin_buf.lock().unwrap();
                    if buf.is_empty() {
                        break;
                    }
                    let n = (len as usize).min(buf.len());
                    buf.drain(..n).collect()
                };
                if mem.write_slice(&data, vm_memory::GuestAddress(addr)).is_ok() {
                    written += data.len() as u32;
                }
            }
            push_used(&mem, &self.rx, head, written);
            delivered += written;
        }

        if delivered > 0 {
            // The ISR bit is only meaningful for INTx, and is cleared by the
            // guest reading it — which an MSI-X driver never does. Gating the
            // interrupt on it meant one byte of console output suppressed
            // every later keystroke.
            self.isr |= 1;
            let vec = self.rx.vec;
            self.msix.trigger(vec);
        }
    }
}

pub struct ConsoleDevice {
    inner: Arc<Mutex<Inner>>,
}

impl ConsoleDevice {
    pub fn new() -> Self {
        let stdin_buf = Arc::new(Mutex::new(Vec::new()));
        let (cfg, msix_cap) = Self::build_pci_config();
        let inner = Arc::new(Mutex::new(Inner {
            com: ComCfg::default(), qs: 0,
            rx: QState { size: QUEUE_SIZE, ..Default::default() },
            tx: QState { size: QUEUE_SIZE, vec: 1, ..Default::default() },
            isr: 0, cfg_vec: VIRTQ_MSI_NO_VECTOR, mem: None,
            msix: MsixTable::default(), cfg, msix_cap,
            stdin_buf: stdin_buf.clone(), cols: 80, rows: 25,
        }));

        // Stdin reader thread — reads from host stdin and pushes into guest RX queue
        let inner_clone = inner.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut buf = [0u8; 256];
            loop {
                match stdin.lock().read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut i = inner_clone.lock().unwrap();
                        i.stdin_buf.lock().unwrap().extend_from_slice(&buf[..n]);
                        i.process_rx();
                    }
                    Err(_) => break,
                }
            }
        });

        Self { inner }
    }
    pub fn set_mem(&self, m: Arc<vm_memory::GuestMemoryMmap>) { self.inner.lock().unwrap().mem = Some(m); }

    /// Attach the host interrupt resources: one MSI-X vector per table entry
    /// and the legacy INTx line used before the guest enables MSI-X.
    pub fn bind_interrupts(&self, vectors: Vec<MsiVector>, router: Arc<dyn MsiRouter>, intx: Arc<EventFd>) {
        self.inner.lock().unwrap().msix.bind(vectors, router, intx);
    }

    fn build_pci_config() -> ([u8; 256], u16) {
        let mut cfg = PciConfig::new(0x1AF4, 0x1043, 0x01, 0x07_80_00, 0x1AF4, 0x0003);
        cfg.set_bar_mem(0, BAR0_SIZE);
        cfg.set_irq_pin(1);
        cfg.set_irq_line(11);
        cfg.add_virtio_cap(1, 0, OFF_COMMON as u32, 0x38);
        cfg.add_virtio_notify_cap(0, OFF_NOTIFY as u32, 0x100, NOTIFY_MULT);
        cfg.add_virtio_cap(3, 0, OFF_ISR as u32, 1);
        cfg.add_virtio_cap(4, 0, OFF_DEVICE as u32, 12);
        let msix_cap = cfg.add_msix_cap(MSIX_VECTORS - 1, OFF_MSIX_TABLE as u32, OFF_MSIX_PBA as u32);
        cfg.add_pcie_cap(PCIE_TYPE_RC_INTEGRATED);
        (cfg.build(), msix_cap)
    }

    fn com_read(&self, off: u64, d: &mut [u8]) {
        let i = self.inner.lock().unwrap();
        let q = i.sq();
        let v = com_read(&i.com, off, VIRTIO_F_VERSION_1 | 1, 2, i.cfg_vec as u64,
            i.qs as u64, q.size as u64, q.vec as u64, q.enabled as u64, i.qs as u64,
            q.desc & 0xFFFF_FFFF, q.desc >> 32, q.avail & 0xFFFF_FFFF, q.avail >> 32, q.used & 0xFFFF_FFFF, q.used >> 32);
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
            CFG_STATUS => { i.com.st = v1; if v1 == 0 { i.rx = QState { size: QUEUE_SIZE, ..Default::default() }; i.tx = QState { size: QUEUE_SIZE, vec: 1, ..Default::default() }; i.qs = 0; } if v1 & STATUS_DRIVER_OK != 0 { log::info!("virtio-console: DRIVER_OK"); } }
            CFG_QUEUE_SEL => i.qs = v2,
            CFG_QUEUE_SIZE => i.sqm().size = v2,
            CFG_QUEUE_MSIX => i.sqm().vec = v2,
            CFG_QUEUE_ENABLE => i.sqm().enabled = v2 != 0,
            _ => write_queue_addr(i.sqm(), off, v3),
        }
    }

    fn bar0_read(&self, o: u64, d: &mut [u8]) {
        if o < OFF_ISR { self.com_read(o - OFF_COMMON, d); }
        else if o < OFF_DEVICE { let mut i = self.inner.lock().unwrap(); if !d.is_empty() { d[0] = i.isr; i.isr = 0; } }
        else if o < OFF_NOTIFY {
            let i = self.inner.lock().unwrap();
            let mut dd = [0u8; 12];
            dd[0..2].copy_from_slice(&i.cols.to_le_bytes()); dd[2..4].copy_from_slice(&i.rows.to_le_bytes()); dd[4..8].copy_from_slice(&1u32.to_le_bytes());
            let s = (o - OFF_DEVICE) as usize; let e = (s + d.len()).min(12);
            if s < 12 { d[..e-s].copy_from_slice(&dd[s..e]); d[e-s..].fill(0); } else { d.fill(0); }
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
        else if o < OFF_MSIX_TABLE {
            // The notify address identifies the queue.
            let idx = (o - OFF_NOTIFY) / NOTIFY_MULT as u64;
            let mut i = self.inner.lock().unwrap();
            match idx {
                0 => i.process_rx(), // buffers posted; deliver pending input
                1 => i.process_tx(),
                _ => {}
            }
        }
        else if o < OFF_MSIX_PBA {
            let mut i = self.inner.lock().unwrap();
            if i.msix.write(o - OFF_MSIX_TABLE, d) {
                i.msix.trigger_unmasked(((o - OFF_MSIX_TABLE) / 16) as usize);
            }
        }
    }
}

impl PciDevice for ConsoleDevice {
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
