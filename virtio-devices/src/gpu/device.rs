//! Virtio GPU over the PCI transport.
//!
//! Replaces the virtio-MMIO `device.rs`, `event_handler.rs` and the event
//! manager the old implementation was built around. The shape here matches the
//! other devices in this crate: config space and queue registers in BAR0, and
//! MSI-X for interrupts.
//!
//! What is different is BAR2. Blob resources need a window of host memory the
//! guest can address directly, so BAR2 is a large 64-bit prefetchable BAR
//! backed by a host mapping registered with KVM. Guest accesses to it never
//! reach this process — that is the point of it.

use crate::common::*;
use crate::gpu::display::DisplayInfo;
use crate::gpu::uapi::virtio_gpu_config;
use crate::gpu::metrics::{GpuMetrics, GpuSnapshot};
use crate::gpu::worker::Worker;
use crate::gpu::{
    CTL_INDEX, CUR_INDEX, Descriptor, GpuQueues, HostMemoryMapper, NUM_QUEUES, QUEUE_SIZE,
    VirtioShmRegion, uapi,
};
use anyhow::{Context, Result};
use pci::config::{PCIE_TYPE_RC_INTEGRATED, PciConfig};
use pci::{BarType, MsiRouter, MsiVector, PciDevice};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;

/// virtio device type 16; PCI device id is 0x1040 + type.
const PCI_DEVICE_ID: u16 = 0x1040 + uapi::VIRTIO_ID_GPU as u16;

/// One vector per queue plus a config vector.
const MSIX_VECTORS: u16 = 4;

/// BAR0 holds the virtio registers; BAR2 is the shared memory window. BAR1 is
/// skipped because BAR0 is 32-bit and BAR2's high half occupies BAR3.
const SHM_BAR: usize = 2;

/// The shared-memory id the virtio-gpu spec assigns to the host-visible region.
/// It is 1, not 0 — 0 is `VIRTIO_GPU_SHM_ID_UNDEFINED`, and a guest that finds
/// the capability under that id discards it without a word.
const VIRTIO_GPU_SHM_ID_HOST_VISIBLE: u8 = 1;

/// Size of the host-visible window, as advertised to the guest through BAR2.
///
/// Nothing on the host is reserved for it. The guest allocates offsets within
/// the window itself, and each mapped resource becomes its own memory slot
/// pointing at virglrenderer's mapping of that resource.
const SHM_SIZE: u64 = 8 << 30; // 8 GiB

/// Features we offer. `VIRTIO_GPU_F_VIRGL` is what makes this a 3D device;
/// without `RESOURCE_BLOB` and `CONTEXT_INIT` there is no native context.
const AVAIL_FEATURES: u64 = (1u64 << uapi::VIRTIO_F_VERSION_1)
    | (1u64 << uapi::VIRTIO_GPU_F_VIRGL)
    | (1u64 << uapi::VIRTIO_GPU_F_EDID)
    | (1u64 << uapi::VIRTIO_GPU_F_RESOURCE_UUID)
    | (1u64 << uapi::VIRTIO_GPU_F_RESOURCE_BLOB)
    | (1u64 << uapi::VIRTIO_GPU_F_CONTEXT_INIT);

/// How to set up the GPU.
#[derive(Clone, Debug)]
pub struct GpuConfig {
    /// Host render node, e.g. `/dev/dri/renderD128`.
    pub render_node: PathBuf,
    /// Virtual displays to advertise.
    pub displays: Vec<DisplayInfo>,
    /// Device memory this guest may hold, in bytes. `None` lets it allocate
    /// until the card is exhausted, which is only safe for a sole tenant.
    pub vram_limit_bytes: Option<u64>,
    /// Bytes the guest may map into the host-visible window. 0 is unbounded.
    pub window_limit_bytes: u64,
    /// Live mappings allowed. 0 is unbounded; each one is a KVM memory slot.
    pub window_max_mappings: u32,
}

/// The queue state and interrupt plumbing the worker and the fence handler
/// share. Held behind an `Arc` because rutabaga's fence callbacks outlive any
/// borrow of the device.
struct Queues {
    mem: Arc<GuestMemoryMmap>,
    ctl: Mutex<QState>,
    msix: Mutex<MsixTable<4>>,
}

impl GpuQueues for Queues {
    fn pop_ctl(&self) -> Option<(u16, Vec<Descriptor>)> {
        let mut ctl = self.ctl.lock().unwrap();
        if !ctl.enabled || ctl.desc == 0 {
            return None;
        }
        pop_avail(&self.mem, &mut ctl)
    }

    fn complete_ctl(&self, completed: &[(u16, u32)]) {
        if completed.is_empty() {
            return;
        }
        let vec = {
            let ctl = self.ctl.lock().unwrap();
            for &(head, len) in completed {
                push_used(&self.mem, &ctl, head, len);
            }
            ctl.vec
        };
        self.msix.lock().unwrap().trigger(vec);
    }
}

struct Inner {
    com: ComCfg,
    qs: u16,
    /// Queue state the driver programs. The control queue is copied into
    /// `queues` when the device is activated.
    pending: [QState; NUM_QUEUES],
    isr: u8,
    cfg_vec: u16,
    cfg: [u8; 256],
    msix_cap: u16,
    /// Set once the driver reaches DRIVER_OK and the worker is running.
    notify: Option<Sender<u64>>,
    num_capsets: Arc<AtomicU32>,
    displays: Box<[DisplayInfo]>,
    render_node: PathBuf,
    vram_limit_bytes: Option<u64>,
    window_limit_bytes: u64,
    window_max_mappings: u32,
    metrics: Arc<GpuMetrics>,
    shm_guest_addr: u64,
    mapper: Option<Arc<dyn HostMemoryMapper>>,
    queues: Arc<Queues>,
    running: bool,
}

impl Inner {
    fn features(&self) -> u64 {
        AVAIL_FEATURES
    }

    /// Start the worker. Called when the driver sets DRIVER_OK, by which point
    /// it has programmed every queue.
    fn activate(&mut self) -> Result<()> {
        if self.running {
            return Ok(());
        }
        let mapper = self
            .mapper
            .clone()
            .context("no host memory mapper attached")?;
        anyhow::ensure!(
            self.shm_guest_addr != 0,
            "the shared window has no guest address; BAR2 was never reported"
        );

        // Deliberately *not* registering the whole window here. Each blob is
        // published as its own memory slot when the guest asks for it to be
        // mapped, backed by virglrenderer's own mapping of that resource; a
        // slot covering the whole window would overlap those and KVM refuses
        // overlapping slots. Guest reads of unmapped parts of the window come
        // back to us as ordinary MMIO and read as zero.
        // Hand the control queue to the worker's view of the world.
        *self.queues.ctl.lock().unwrap() = self.pending[CTL_INDEX].clone();

        let (tx, rx) = channel();
        let worker = Worker::new(
            rx,
            (*self.queues.mem).clone(),
            self.queues.clone(),
            VirtioShmRegion {
                guest_addr: self.shm_guest_addr,
                size: SHM_SIZE as usize,
            },
            self.displays.clone(),
            self.num_capsets.clone(),
            self.render_node.clone(),
            mapper,
            self.vram_limit_bytes,
            self.window_limit_bytes,
            self.window_max_mappings,
            self.metrics.clone(),
        );
        worker.run();
        self.notify = Some(tx);
        self.running = true;
        log::info!(
            "virtio-gpu: worker started, shared window {SHM_SIZE:#x} bytes at guest {:#x}",
            self.shm_guest_addr
        );
        Ok(())
    }

    fn reset(&mut self) {
        if self.running {
            // Dropping the sender ends the worker's loop, which drops rutabaga
            // and with it every resource mapping it published.
            self.notify = None;
            self.running = false;
        }
        self.pending = new_queues();
        self.qs = 0;
    }

    fn sq(&self) -> &QState {
        &self.pending[(self.qs as usize).min(NUM_QUEUES - 1)]
    }

    fn sqm(&mut self) -> &mut QState {
        &mut self.pending[(self.qs as usize).min(NUM_QUEUES - 1)]
    }

    /// Device config space: the four 32-bit fields of `virtio_gpu_config`.
    fn config_bytes(&self) -> [u8; 16] {
        let cfg = virtio_gpu_config {
            events_read: 0,
            events_clear: 0,
            num_scanouts: self.displays.len() as u32,
            num_capsets: self.num_capsets.load(Ordering::Acquire),
        };
        let mut out = [0u8; 16];
        out[0..4].copy_from_slice(&cfg.events_read.to_le_bytes());
        out[4..8].copy_from_slice(&cfg.events_clear.to_le_bytes());
        out[8..12].copy_from_slice(&cfg.num_scanouts.to_le_bytes());
        out[12..16].copy_from_slice(&cfg.num_capsets.to_le_bytes());
        out
    }
}

fn new_queues() -> [QState; NUM_QUEUES] {
    std::array::from_fn(|i| QState {
        size: QUEUE_SIZE,
        vec: i as u16,
        ..Default::default()
    })
}

pub struct GpuDevice {
    inner: Mutex<Inner>,
    queues: Arc<Queues>,
    /// Held here as well as in `Inner` so a watcher can read metrics without
    /// taking the device lock the vCPU thread uses.
    metrics: Arc<GpuMetrics>,
}

impl GpuDevice {
    /// Prepare the device. Rutabaga is not built until the guest driver
    /// activates it, because building it takes long enough to be worth
    /// deferring and needs the queues in place.
    pub fn new(config: &GpuConfig, mem: Arc<GuestMemoryMmap>) -> Result<Self> {
        anyhow::ensure!(
            config.render_node.exists(),
            "GPU render node {:?} does not exist",
            config.render_node
        );
        let metrics = Arc::new(GpuMetrics::new());
        let displays: Box<[DisplayInfo]> = config.displays.clone().into_boxed_slice();
        anyhow::ensure!(!displays.is_empty(), "the GPU needs at least one display");

        let queues = Arc::new(Queues {
            mem: mem.clone(),
            ctl: Mutex::new(QState { size: QUEUE_SIZE, ..Default::default() }),
            msix: Mutex::new(MsixTable::default()),
        });
        let (cfg, msix_cap) = Self::build_pci_config();

        Ok(Self {
            inner: Mutex::new(Inner {
                com: ComCfg::default(),
                qs: 0,
                pending: new_queues(),
                isr: 0,
                cfg_vec: VIRTQ_MSI_NO_VECTOR,
                cfg,
                msix_cap,
                notify: None,
                // Corrected once rutabaga reports the real count; the driver
                // reads this before the worker has started.
                num_capsets: Arc::new(AtomicU32::new(1)),
                displays,
                render_node: config.render_node.clone(),
                vram_limit_bytes: config.vram_limit_bytes,
                window_limit_bytes: config.window_limit_bytes,
                window_max_mappings: config.window_max_mappings,
                metrics: metrics.clone(),
                // Filled in by `set_shm_guest_addr` once the bus has placed
                // BAR2; the device cannot be activated before that.
                shm_guest_addr: 0,
                mapper: None,
                queues: queues.clone(),
                running: false,
            }),
            queues,
            metrics,
        })
    }

    /// A snapshot of what this device is doing, for a supervisor rather than a
    /// log reader ([0027]).
    pub fn metrics(&self) -> GpuSnapshot {
        self.metrics.snapshot()
    }

    pub fn bind_interrupts(
        &self,
        vectors: Vec<MsiVector>,
        router: Arc<dyn MsiRouter>,
        intx: Arc<EventFd>,
    ) {
        self.queues
            .msix
            .lock()
            .unwrap()
            .bind(vectors, router, intx);
    }

    /// Attach the thing that can turn host memory into guest memory. Required
    /// before the guest activates the device.
    pub fn bind_mapper(&self, mapper: Arc<dyn HostMemoryMapper>) {
        self.inner.lock().unwrap().mapper = Some(mapper);
    }

    /// Tell the device where the bus put BAR2, which is the guest address its
    /// shared window has to appear at.
    pub fn set_shm_guest_addr(&self, addr: u64) {
        self.inner.lock().unwrap().shm_guest_addr = addr;
    }

    /// Where the shared window must be placed in guest physical memory: the
    /// base of BAR2, which the bus assigns.
    pub fn shm_bar_size() -> u64 {
        SHM_SIZE
    }

    fn build_pci_config() -> ([u8; 256], u16) {
        // Class 0x030000: display controller, VGA compatible.
        let mut cfg = PciConfig::new(
            0x1AF4,
            PCI_DEVICE_ID,
            0x01,
            0x03_00_00,
            0x1AF4,
            uapi::VIRTIO_ID_GPU as u16,
        );
        cfg.set_bar_mem(0, BAR0_SIZE);
        cfg.set_bar_mem64(SHM_BAR, SHM_SIZE);
        cfg.set_irq_pin(1);
        cfg.add_virtio_cap(1, 0, OFF_COMMON as u32, 0x38);
        cfg.add_virtio_notify_cap(0, OFF_NOTIFY as u32, 0x100, NOTIFY_MULT);
        cfg.add_virtio_cap(3, 0, OFF_ISR as u32, 1);
        cfg.add_virtio_cap(4, 0, OFF_DEVICE as u32, 16);
        // Tell the guest BAR2 is a window it may map blob resources through.
        cfg.add_virtio_shm_cap(
            VIRTIO_GPU_SHM_ID_HOST_VISIBLE,
            SHM_BAR as u8,
            0,
            SHM_SIZE,
        );
        let msix_cap =
            cfg.add_msix_cap(MSIX_VECTORS - 1, OFF_MSIX_TABLE as u32, OFF_MSIX_PBA as u32);
        cfg.add_pcie_cap(PCIE_TYPE_RC_INTEGRATED);
        (cfg.build(), msix_cap)
    }

    fn com_read(&self, off: u64, d: &mut [u8]) {
        let i = self.inner.lock().unwrap();
        let q = i.sq();
        let v = com_read(
            &i.com,
            off,
            i.features(),
            NUM_QUEUES as u64,
            i.cfg_vec as u64,
            i.qs as u64,
            q.size as u64,
            q.vec as u64,
            q.enabled as u64,
            i.qs as u64,
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
            CFG_DRIVER_FEAT => {
                if i.com.dff == 0 {
                    i.com.df = (i.com.df & 0xFFFF_FFFF_0000_0000) | (v3 as u64)
                } else {
                    i.com.df = (i.com.df & 0xFFFF_FFFF) | ((v3 as u64) << 32)
                }
            }
            CFG_MSIX_CONFIG => i.cfg_vec = v2,
            CFG_STATUS => {
                i.com.st = v1;
                if v1 == 0 {
                    i.reset();
                } else if v1 & STATUS_DRIVER_OK != 0 {
                    if let Err(err) = i.activate() {
                        log::error!("failed to start virtio-gpu: {err:#}");
                    }
                }
            }
            CFG_QUEUE_SEL => i.qs = v2,
            CFG_QUEUE_SIZE => i.sqm().size = v2,
            CFG_QUEUE_MSIX => i.sqm().vec = v2,
            CFG_QUEUE_ENABLE => i.sqm().enabled = v2 != 0,
            _ => write_queue_addr(i.sqm(), off, v3),
        }
    }

    fn bar0_read(&self, o: u64, d: &mut [u8]) {
        if o < OFF_ISR {
            self.com_read(o - OFF_COMMON, d);
        } else if o < OFF_DEVICE {
            let mut i = self.inner.lock().unwrap();
            if !d.is_empty() {
                d[0] = i.isr;
                i.isr = 0;
            }
        } else if o < OFF_NOTIFY {
            let i = self.inner.lock().unwrap();
            let bytes = i.config_bytes();
            let s = (o - OFF_DEVICE) as usize;
            let e = (s + d.len()).min(bytes.len());
            if s < bytes.len() {
                d[..e - s].copy_from_slice(&bytes[s..e]);
                d[e - s..].fill(0);
            } else {
                d.fill(0);
            }
        } else if o < OFF_MSIX_TABLE {
            d.fill(0);
        } else if o < OFF_MSIX_PBA {
            self.queues
                .msix
                .lock()
                .unwrap()
                .read(o - OFF_MSIX_TABLE, d);
        } else if o < BAR0_SIZE {
            self.queues
                .msix
                .lock()
                .unwrap()
                .read_pba(o - OFF_MSIX_PBA, d);
        } else {
            d.fill(0);
        }
    }

    fn bar0_write(&self, o: u64, d: &[u8]) {
        if o < OFF_ISR {
            self.com_write(o - OFF_COMMON, d);
        } else if o < OFF_NOTIFY {
            // ISR and device config are read-only here.
        } else if o < OFF_MSIX_TABLE {
            // The notify address identifies the queue; wake the worker.
            let idx = (o - OFF_NOTIFY) / NOTIFY_MULT as u64;
            let i = self.inner.lock().unwrap();
            if let Some(tx) = &i.notify {
                match idx as usize {
                    CTL_INDEX | CUR_INDEX => {
                        if tx.send(idx).is_err() {
                            log::warn!("virtio-gpu: worker is gone, dropping notification");
                        }
                    }
                    _ => {}
                }
            }
        } else if o < OFF_MSIX_PBA {
            let mut msix = self.queues.msix.lock().unwrap();
            if msix.write(o - OFF_MSIX_TABLE, d) {
                msix.trigger_unmasked(((o - OFF_MSIX_TABLE) / 16) as usize);
            }
        }
    }
}

impl PciDevice for GpuDevice {
    fn read_config(&self, o: u32, d: &mut [u8]) {
        let i = self.inner.lock().unwrap();
        read_cfg_space(&i.cfg, o, d);
    }
    fn write_config(&self, o: u32, d: &[u8]) {
        let mut i = self.inner.lock().unwrap();
        let cap = i.msix_cap;
        write_msix_control(&mut i.cfg, cap, o, d);
        let enabled = msix_enabled(&i.cfg, cap);
        self.queues.msix.lock().unwrap().enabled = enabled;
    }
    fn read_bar(&self, bi: usize, o: u64, d: &mut [u8]) -> bool {
        if bi == 0 {
            self.bar0_read(o, d);
            true
        } else {
            // BAR2 is backed by a real memory mapping; the guest reaches it
            // without trapping, so anything arriving here is a stray access to
            // a page rutabaga has not mapped.
            d.fill(0);
            bi == SHM_BAR
        }
    }
    fn write_bar(&self, bi: usize, o: u64, d: &[u8]) -> bool {
        if bi == 0 {
            self.bar0_write(o, d);
            true
        } else {
            bi == SHM_BAR
        }
    }
    fn bar_size(&self, bi: usize) -> u64 {
        match bi {
            0 => BAR0_SIZE,
            SHM_BAR => SHM_SIZE,
            _ => 0,
        }
    }
    fn bar_type(&self, bi: usize) -> BarType {
        if bi == SHM_BAR {
            BarType::Mem64
        } else {
            BarType::Mem32
        }
    }
}
