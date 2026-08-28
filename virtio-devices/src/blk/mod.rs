//! Virtio block device over PCI transport (virtio 1.0 / modern).
//!
//! Requests are served on worker threads rather than on the vCPU that submitted
//! them. The guest notifies by writing to the notify register, which traps to
//! us; doing the read there means the vCPU sits in `KVM_RUN` until the disk
//! answers, and a guest loading a few hundred megabytes of assets stalls one of
//! its cores for the duration. Kicking a worker instead lets the vCPU return
//! immediately and go on running guest code.
//!
//! What each worker then does is in [`worker`]: `io_uring`, requests in flight
//! against each other, and the guest's own pages as the I/O buffers. The disk
//! itself -- direct I/O, alignment, capacity -- is in [`disk`].
//!
//! Queues are per-vCPU-ish rather than singular. One queue means every guest
//! CPU contends on one ring, one lock and one worker; `VIRTIO_BLK_F_MQ` lets
//! the guest's block layer keep a submission queue per CPU, which is what its
//! own multiqueue path is written for.

pub mod disk;
mod engine;
mod request;
mod worker;

use crate::common::*;
use anyhow::{Context, Result};
use disk::{CacheMode, Disk, SECTOR_SIZE};
use pci::config::{PCIE_TYPE_RC_INTEGRATED, PciConfig};
use pci::{Doorbell, MsiRouter, MsiVector, PciDevice};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;

const QUEUE_SIZE: u16 = 256;
/// The most queues this device will ever build, and so the size of its MSI-X
/// table: one vector per queue plus one for configuration changes.
pub const MAX_QUEUES: u16 = 8;
const MSIX_VECTORS: usize = MAX_QUEUES as usize + 1;

// Feature bits. Numbers are the spec's; the reasons are ours.
/// `size_max`: the largest single segment we accept.
const F_SIZE_MAX: u64 = 1 << 1;
/// `seg_max`: the most segments in one request.
const F_SEG_MAX: u64 = 1 << 2;
/// The disk is read-only, and the driver should know that before it tries.
const F_RO: u64 = 1 << 5;
/// `blk_size`: the logical block size, which is how the guest is told to issue
/// I/O that direct I/O on the backing file can actually take.
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;
const F_MQ: u64 = 1 << 12;
const F_DISCARD: u64 = 1 << 13;
const F_WRITE_ZEROES: u64 = 1 << 14;

/// The largest single segment. A guest's segments come from its page cache, so
/// this is far above anything one will be; it exists because advertising
/// `SIZE_MAX` with a zero in the field -- which this device used to do -- tells
/// the guest its maximum segment is nothing.
const SIZE_MAX: u32 = 1 << 20;
/// Sectors one discard or write-zeroes request may cover.
const MAX_DISCARD_SECTORS: u32 = 1 << 21;
/// One range per request. More would mean one virtio request fanning out into
/// several `fallocate`s and a completion counter to join them; the guest
/// splits them for us instead, and each still overlaps every other request.
const MAX_DISCARD_SEG: u32 = 1;

/// How a drive is to be opened.
pub struct BlkConfig {
    pub path: PathBuf,
    pub read_only: bool,
    /// `None` takes direct I/O where the host supports it. See [`CacheMode`].
    pub direct: Option<bool>,
    /// Queues to offer. Clamped to [`MAX_QUEUES`]; zero means one.
    pub num_queues: u16,
    /// Microseconds a worker keeps looking at its ring before sleeping.
    ///
    /// Zero -- the default -- sleeps immediately. A non-zero value trades CPU
    /// for the thread wakeup on each request: about 10 us per request of
    /// latency, which is a third of an NVMe read and half of a cached one. The
    /// spinning is bounded to this window after each wake, so an idle guest
    /// costs nothing either way.
    pub poll_us: u64,
}

/// One virtqueue: the guest's view of it, and the doorbell for its worker.
struct Queue {
    index: u16,
    state: Mutex<QState>,
    /// Written by the vCPU on notify, waited on by the worker. Deliberately not
    /// behind the queue lock: the whole point is that a notify never waits on a
    /// request already in flight.
    kick: Arc<EventFd>,
}

/// Interrupt state, shared by every worker.
struct Irq {
    isr: u8,
    msix: MsixTable<MSIX_VECTORS>,
}

/// Configuration-space state, touched only by vCPUs.
struct Inner {
    com: ComCfg,
    qs: u16,
    cfg_vec: u16,
    /// Config space, built once; the guest can write MSI-X Message Control.
    cfg: [u8; 256],
    /// Config-space offset of the MSI-X capability.
    msix_cap: u16,
}

pub struct BlkDevice {
    inner: Mutex<Inner>,
    irq: Arc<Mutex<Irq>>,
    queues: Vec<Arc<Queue>>,
    disk: Arc<Disk>,
    features: u64,
    stop: Arc<AtomicBool>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Set the first time a notify arrives as an MMIO exit rather than through
    /// the kernel. Diagnostic only.
    notified_in_userspace: AtomicBool,
}

impl BlkDevice {
    pub fn new(config: &BlkConfig, mem: Arc<GuestMemoryMmap>) -> Result<Self> {
        let disk = Arc::new(Disk::open(
            &config.path,
            config.read_only,
            CacheMode::from_flag(config.direct),
        )?);

        let num_queues = config.num_queues.clamp(1, MAX_QUEUES);
        if config.num_queues > MAX_QUEUES {
            log::warn!(
                "virtio-blk: {} queues asked for, {MAX_QUEUES} is the most this device builds",
                config.num_queues
            );
        }

        let mut features =
            VIRTIO_F_VERSION_1 | VIRTIO_F_RING_INDIRECT_DESC | F_SIZE_MAX | F_SEG_MAX | F_BLK_SIZE;
        if config.read_only {
            features |= F_RO;
        } else {
            // A read-only disk has nothing to flush and nothing to punch, and
            // offering either would be offering something that can only fail.
            features |= F_FLUSH | F_DISCARD | F_WRITE_ZEROES;
        }
        if num_queues > 1 {
            features |= F_MQ;
        }

        let (cfg, msix_cap) = Self::build_pci_config();
        let irq = Arc::new(Mutex::new(Irq {
            isr: 0,
            msix: MsixTable::default(),
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let mut queues = Vec::with_capacity(num_queues as usize);
        let mut workers = Vec::with_capacity(num_queues as usize);
        for index in 0..num_queues {
            let kick = Arc::new(EventFd::new(0).context("failed to create the blk kick eventfd")?);
            let queue = Arc::new(Queue {
                index,
                state: Mutex::new(QState {
                    size: QUEUE_SIZE,
                    vec: VIRTQ_MSI_NO_VECTOR,
                    ..Default::default()
                }),
                kick,
            });
            let w = worker::Worker::new(
                mem.clone(),
                disk.clone(),
                queue.clone(),
                irq.clone(),
                QUEUE_SIZE as usize,
                config.poll_us,
                stop.clone(),
            );
            workers.push(
                std::thread::Builder::new()
                    .name(format!("virtio-blk-{index}"))
                    .spawn(move || w.run())
                    .context("failed to start a virtio-blk worker")?,
            );
            queues.push(queue);
        }

        log::info!(
            "virtio-blk: {num_queues} queue(s) of {QUEUE_SIZE}, indirect descriptors offered{}",
            if config.poll_us > 0 {
                format!(", {}us ring polling", config.poll_us)
            } else {
                String::new()
            }
        );

        Ok(Self {
            inner: Mutex::new(Inner {
                com: ComCfg::default(),
                qs: 0,
                cfg_vec: VIRTQ_MSI_NO_VECTOR,
                cfg,
                msix_cap,
            }),
            irq,
            queues,
            disk,
            features,
            stop,
            workers: Mutex::new(workers),
            notified_in_userspace: AtomicBool::new(false),
        })
    }

    /// How many MSI-X vectors this device needs: one per queue, one for
    /// configuration changes.
    pub fn msix_vectors(&self) -> usize {
        self.queues.len() + 1
    }

    /// Attach the host interrupt resources: one MSI-X vector per table entry
    /// and the legacy INTx line used before the guest enables MSI-X.
    pub fn bind_interrupts(
        &self,
        vectors: Vec<MsiVector>,
        router: Arc<dyn MsiRouter>,
        intx: Arc<EventFd>,
    ) {
        self.irq.lock().unwrap().msix.bind(vectors, router, intx);
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
        let msix_cap = cfg.add_msix_cap(
            MSIX_VECTORS as u16 - 1,
            OFF_MSIX_TABLE as u32,
            OFF_MSIX_PBA as u32,
        );
        cfg.add_pcie_cap(PCIE_TYPE_RC_INTEGRATED);
        (cfg.build(), msix_cap)
    }

    /// The 60-byte `virtio_blk_config` the guest reads.
    fn device_config(&self) -> [u8; 60] {
        let mut d = [0u8; 60];
        d[0..8].copy_from_slice(&self.disk.sectors.to_le_bytes());
        d[8..12].copy_from_slice(&SIZE_MAX.to_le_bytes());
        // Header and status take one descriptor each out of a chain.
        d[12..16].copy_from_slice(&(QUEUE_SIZE as u32 - 2).to_le_bytes());
        d[20..24].copy_from_slice(&self.disk.block_size.to_le_bytes());
        d[34..36].copy_from_slice(&(self.queues.len() as u16).to_le_bytes());
        if !self.disk.read_only {
            let align_sectors = (self.disk.block_size as u64 / SECTOR_SIZE) as u32;
            d[36..40].copy_from_slice(&MAX_DISCARD_SECTORS.to_le_bytes());
            d[40..44].copy_from_slice(&MAX_DISCARD_SEG.to_le_bytes());
            d[44..48].copy_from_slice(&align_sectors.to_le_bytes());
            d[48..52].copy_from_slice(&MAX_DISCARD_SECTORS.to_le_bytes());
            d[52..56].copy_from_slice(&MAX_DISCARD_SEG.to_le_bytes());
            // A hole reads back as zeroes, so the guest is free to let us make
            // one -- which is the difference between an image that shrinks and
            // one that only grows.
            d[56] = 1;
        }
        d
    }

    fn com_read(&self, off: u64, d: &mut [u8]) {
        let i = self.inner.lock().unwrap();
        let (q, notify_off) = match self.queues.get(i.qs as usize) {
            Some(queue) => (queue.state.lock().unwrap().clone(), i.qs as u64),
            None => (QState::default(), 0),
        };
        let v = com_read(
            &i.com,
            off,
            self.features,
            self.queues.len() as u64,
            i.cfg_vec as u64,
            i.qs as u64,
            q.size as u64,
            q.vec as u64,
            q.enabled as u64,
            notify_off,
            q.desc & 0xFFFF_FFFF,
            q.desc >> 32,
            q.avail & 0xFFFF_FFFF,
            q.avail >> 32,
            q.used & 0xFFFF_FFFF,
            q.used >> 32,
        );
        write_val(d, v);
    }

    fn com_write(&self, off: u64, d: &[u8]) {
        let (v3, v2, v1) = parse_write(d);
        let mut i = self.inner.lock().unwrap();
        match off {
            CFG_DEVICE_FEAT_SEL => i.com.dfs = v3,
            CFG_DRIVER_FEAT_SEL => i.com.dff = v3,
            CFG_DRIVER_FEAT => write_driver_feature(&mut i.com, v3),
            CFG_MSIX_CONFIG => i.cfg_vec = v2,
            CFG_STATUS => {
                let old = i.com.st;
                i.com.st = v1;
                // Indirect descriptors may only be followed once the driver has
                // accepted the feature, and it has by the time it acknowledges
                // the feature set. Latched here rather than read per request so
                // that a driver cannot change the answer mid-flight.
                let negotiated = i.com.df & self.features;
                let indirect = negotiated & VIRTIO_F_RING_INDIRECT_DESC != 0;
                for queue in &self.queues {
                    queue.state.lock().unwrap().indirect = indirect;
                }
                if old & STATUS_DRIVER_OK == 0 && v1 & STATUS_DRIVER_OK != 0 {
                    log::info!(
                        "virtio-blk: Driver OK, features {negotiated:#x}, indirect={indirect}"
                    );
                }
                if v1 == 0 {
                    // A reset. Every queue goes back to where it started, or
                    // the next driver inherits this one's ring positions.
                    for queue in &self.queues {
                        let mut q = queue.state.lock().unwrap();
                        *q = QState {
                            size: QUEUE_SIZE,
                            vec: VIRTQ_MSI_NO_VECTOR,
                            ..Default::default()
                        };
                    }
                }
            }
            CFG_QUEUE_SEL => i.qs = v2,
            _ => {
                let Some(queue) = self.queues.get(i.qs as usize) else {
                    return;
                };
                let mut q = queue.state.lock().unwrap();
                match off {
                    CFG_QUEUE_SIZE => set_queue_size(&mut q, v2, QUEUE_SIZE),
                    CFG_QUEUE_MSIX => q.vec = v2,
                    CFG_QUEUE_ENABLE => q.enabled = v2 != 0,
                    _ => write_queue_addr(&mut q, off, v3),
                }
            }
        }
    }

    fn bar0_read(&self, o: u64, d: &mut [u8]) {
        if o < OFF_ISR {
            self.com_read(o - OFF_COMMON, d);
        } else if o < OFF_DEVICE {
            let mut irq = self.irq.lock().unwrap();
            if !d.is_empty() {
                d[0] = irq.isr;
                irq.isr = 0;
            }
        } else if o < OFF_NOTIFY {
            let dd = self.device_config();
            let s = (o - OFF_DEVICE) as usize;
            let e = (s + d.len()).min(dd.len());
            if s < dd.len() {
                d[..e - s].copy_from_slice(&dd[s..e]);
                d[e - s..].fill(0);
            } else {
                d.fill(0);
            }
        } else if o < OFF_MSIX_TABLE {
            d.fill(0);
        } else if o < OFF_MSIX_PBA {
            self.irq.lock().unwrap().msix.read(o - OFF_MSIX_TABLE, d);
        } else if o < BAR0_SIZE {
            self.irq.lock().unwrap().msix.read_pba(o - OFF_MSIX_PBA, d);
        } else {
            d.fill(0);
        }
    }

    fn bar0_write(&self, o: u64, d: &[u8]) {
        if o < OFF_ISR {
            self.com_write(o - OFF_COMMON, d);
        } else if o < OFF_NOTIFY {
            // The ISR register is read-to-clear and the device-config area is
            // read-only; a driver writing to either is ignored rather than
            // obeyed.
        } else if o < OFF_MSIX_TABLE {
            // Hand the work to the worker and return to the guest at once.
            // Which worker is the register the guest wrote: with several
            // queues, a notification for one must not wake the rest.
            //
            // Reaching here at all means the kernel is not answering this
            // doorbell -- see `doorbells` below -- so say so once. It is not an
            // error; it is the difference between a notify costing a VM exit
            // and costing nothing.
            if !self.notified_in_userspace.swap(true, Ordering::Relaxed) {
                log::debug!(
                    "virtio-blk: queue notifications are arriving as MMIO exits, so no doorbell \
                     was registered with the kernel for this device"
                );
            }
            let index = (o - OFF_NOTIFY) / NOTIFY_MULT as u64;
            if let Some(queue) = self.queues.get(index as usize) {
                let _ = queue.kick.write(1);
            }
        } else if o < OFF_MSIX_PBA {
            let mut irq = self.irq.lock().unwrap();
            if irq.msix.write(o - OFF_MSIX_TABLE, d) {
                irq.msix
                    .trigger_unmasked(((o - OFF_MSIX_TABLE) / 16) as usize);
            }
        }
    }
}

impl Drop for BlkDevice {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake every worker so each notices; they are blocked on their rings.
        for queue in &self.queues {
            let _ = queue.kick.write(1);
        }
        for worker in self.workers.lock().unwrap().drain(..) {
            let _ = worker.join();
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
        let enabled = msix_enabled(&i.cfg, cap);
        self.irq.lock().unwrap().msix.enabled = enabled;
    }
    fn read_bar(&self, bi: usize, o: u64, d: &mut [u8]) -> bool {
        if bi == 0 {
            self.bar0_read(o, d);
            true
        } else {
            false
        }
    }
    fn write_bar(&self, bi: usize, o: u64, d: &[u8]) -> bool {
        if bi == 0 {
            self.bar0_write(o, d);
            true
        } else {
            false
        }
    }
    fn bar_size(&self, bi: usize) -> u64 {
        if bi == 0 { BAR0_SIZE } else { 0 }
    }
    /// One doorbell per queue, at the notify register the guest was told to use
    /// for it.
    ///
    /// A notify write carries the queue index, which the address already
    /// encodes -- `queue_notify_off` times [`NOTIFY_MULT`] -- so there is
    /// nothing in the value this device needs and the kernel can answer the
    /// write by signalling the queue's worker directly. `bar0_write` still
    /// handles the same offsets, for the host where registration fails.
    fn doorbells(&self) -> Vec<Doorbell> {
        self.queues
            .iter()
            .map(|queue| Doorbell {
                bar_idx: 0,
                offset: OFF_NOTIFY + queue.index as u64 * NOTIFY_MULT as u64,
                fd: queue.kick.clone(),
            })
            .collect()
    }
}
