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
use crate::gpu::metrics::{GpuMetrics, GpuSnapshot};
use crate::gpu::uapi::virtio_gpu_config;
use crate::gpu::worker::Worker;
use crate::gpu::{CTL_INDEX, Descriptor, GpuQueues, NUM_QUEUES, QUEUE_SIZE, VirtioShmRegion, uapi};
use crate::memmap::HostMemoryMapper;
use anyhow::{Context, Result};
use pci::config::{PCIE_TYPE_RC_INTEGRATED, PciConfig};
use pci::{BarType, Doorbell, MsiRouter, MsiVector, PciDevice};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    /// How long the worker looks at the control queue before it sleeps, in
    /// microseconds. Zero sleeps at once. See `Worker::wait`.
    pub poll_us: u64,
    /// Host CPUs the GPU worker thread may run on. Empty means no affinity.
    ///
    /// **The same set the vCPUs get, and that is the point.** Every forwarded
    /// command is a handoff between a vCPU thread and this one, and on a
    /// chiplet part a handoff across dies costs the fabric rather than the
    /// cache. Confining the vCPUs to one L3 domain and leaving the thread that
    /// answers them free to run on the other is half a placement.
    pub cpu_affinity: Vec<usize>,
}

/// The queue state and interrupt plumbing the worker and the fence handler
/// share. Held behind an `Arc` because rutabaga's fence callbacks outlive any
/// borrow of the device.
struct Queues {
    mem: Arc<GuestMemoryMmap>,
    ctl: Mutex<QState>,
    msix: Mutex<MsixTable<4>>,
    /// Held here so the retire path can be timed. It is reached from two
    /// threads -- the worker and rutabaga's fence thread -- and neither has the
    /// device.
    metrics: Arc<GpuMetrics>,
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
        let at = std::time::Instant::now();
        let vec = {
            let ctl = self.ctl.lock().unwrap();
            for &(head, len) in completed {
                push_used(&self.mem, &ctl, head, len);
            }
            ctl.vec
        };
        self.msix.lock().unwrap().trigger(vec);
        self.metrics.counters.complete.since(at);
    }

    fn ctl_has_work(&self) -> bool {
        let ctl = self.ctl.lock().unwrap();
        ctl.enabled && ctl.desc != 0 && has_avail(&self.mem, &ctl)
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
    /// The control queue's doorbell, so `bar0_write` can ring it on a host
    /// where KVM would not take the registration.
    ctl_kick: Arc<EventFd>,
    /// Set when the device is reset, then the doorbell is rung once so the
    /// worker wakes and sees it.
    ///
    /// The channel used to carry this: dropping the `Sender` ended the worker's
    /// loop. An eventfd has no such signal, so the shutdown is its own flag.
    stop: Arc<AtomicBool>,
    num_capsets: Arc<AtomicU32>,
    displays: Box<[DisplayInfo]>,
    render_node: PathBuf,
    vram_limit_bytes: Option<u64>,
    window_limit_bytes: u64,
    window_max_mappings: u32,
    /// How long the worker looks at the ring before sleeping, in microseconds.
    poll_us: u64,
    cpu_affinity: Vec<usize>,
    metrics: Arc<GpuMetrics>,
    shm_guest_addr: u64,
    mapper: Option<Arc<dyn HostMemoryMapper>>,
    queues: Arc<Queues>,
    /// The running worker, joined on reset. `None` before the first activation.
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Inner {
    fn features(&self) -> u64 {
        AVAIL_FEATURES
    }

    /// Start the worker. Called when the driver sets DRIVER_OK, by which point
    /// it has programmed every queue.
    fn activate(&mut self) -> Result<()> {
        if self.worker.is_some() {
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

        let worker = Worker::new(
            self.ctl_kick.clone(),
            self.stop.clone(),
            self.poll_us,
            self.cpu_affinity.clone(),
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
        self.worker = Some(worker.run());
        log::info!(
            "virtio-gpu: worker started, shared window {SHM_SIZE:#x} bytes at guest {:#x}",
            self.shm_guest_addr
        );
        Ok(())
    }

    fn reset(&mut self) {
        if let Some(worker) = self.worker.take() {
            // The flag ends the worker's loop, which drops rutabaga and with it
            // every resource mapping it published. The doorbell is rung after
            // it is set, because a worker asleep on the fd is not reading the
            // flag and would otherwise sleep until the guest kicked it.
            self.stop.store(true, Ordering::Release);
            let _ = self.ctl_kick.write(1);
            // **Joined, not merely asked.** The next activation hands a new
            // worker the same doorbell; two of them blocked on one eventfd
            // would each swallow kicks meant for the other. Bounded by whatever
            // command the worker is in the middle of -- it holds none of the
            // locks this thread does.
            if worker.join().is_err() {
                log::error!("virtio-gpu: the worker panicked; the device stays down");
            }
            self.stop.store(false, Ordering::Release);
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
    /// The control queue's doorbell.
    ///
    /// **Built here rather than at activation, because the bus asks for
    /// doorbells when the device joins it** -- long before the driver reaches
    /// `DRIVER_OK`. An fd created at activation would be registered with KVM
    /// never, or at an address the guest had not been told about yet.
    ctl_kick: Arc<EventFd>,
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
            ctl: Mutex::new(QState {
                size: QUEUE_SIZE,
                ..Default::default()
            }),
            msix: Mutex::new(MsixTable::default()),
            metrics: metrics.clone(),
        });
        let (cfg, msix_cap) = Self::build_pci_config();
        let ctl_kick =
            Arc::new(EventFd::new(libc::EFD_NONBLOCK).context("virtio-gpu: doorbell eventfd")?);
        let stop = Arc::new(AtomicBool::new(false));

        Ok(Self {
            inner: Mutex::new(Inner {
                com: ComCfg::default(),
                qs: 0,
                pending: new_queues(),
                isr: 0,
                cfg_vec: VIRTQ_MSI_NO_VECTOR,
                cfg,
                msix_cap,
                ctl_kick: ctl_kick.clone(),
                stop: stop.clone(),
                // Corrected once rutabaga reports the real count; the driver
                // reads this before the worker has started.
                num_capsets: Arc::new(AtomicU32::new(1)),
                displays,
                render_node: config.render_node.clone(),
                vram_limit_bytes: config.vram_limit_bytes,
                window_limit_bytes: config.window_limit_bytes,
                window_max_mappings: config.window_max_mappings,
                poll_us: config.poll_us,
                cpu_affinity: config.cpu_affinity.clone(),
                metrics: metrics.clone(),
                // Filled in by `set_shm_guest_addr` once the bus has placed
                // BAR2; the device cannot be activated before that.
                shm_guest_addr: 0,
                mapper: None,
                queues: queues.clone(),
                worker: None,
            }),
            queues,
            metrics,
            ctl_kick,
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
        self.queues.msix.lock().unwrap().bind(vectors, router, intx);
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
        cfg.add_virtio_shm_cap(VIRTIO_GPU_SHM_ID_HOST_VISIBLE, SHM_BAR as u8, 0, SHM_SIZE);
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
            CFG_DRIVER_FEAT => write_driver_feature(&mut i.com, v3),
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
            CFG_QUEUE_SIZE => set_queue_size(i.sqm(), v2, QUEUE_SIZE),
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
            self.queues.msix.lock().unwrap().read(o - OFF_MSIX_TABLE, d);
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
            //
            // **This is the slow path and it is meant to be unreachable.** The
            // same write is declared as a doorbell, so on a host where KVM took
            // the registration the kernel signals `ctl_kick` without ever
            // returning here. `doorbells()` says why a device must still serve
            // it: registration can fail, and then the guest's writes arrive the
            // ordinary way and have to work.
            let idx = (o - OFF_NOTIFY) / NOTIFY_MULT as u64;
            if idx as usize == CTL_INDEX {
                let _ = self.ctl_kick.write(1);
            }
            // The cursor queue is not served: cursor commands are unimplemented
            // in this headless port, so a kick for it has nothing to wake.
        } else if o < OFF_MSIX_PBA {
            let mut msix = self.queues.msix.lock().unwrap();
            if msix.write(o - OFF_MSIX_TABLE, d) {
                msix.trigger_unmasked(((o - OFF_MSIX_TABLE) / 16) as usize);
            }
        }
    }
}

impl GpuDevice {
    /// The control queue's notify register, for KVM to answer in the kernel.
    ///
    /// **This is the device that most needed one.** A guest kicks its disk when
    /// it has I/O; it kicks the GPU once per command submission, which under a
    /// game is tens of thousands of times a second. Every one of those was a
    /// VM exit into the vCPU thread, a scan of the MMIO regions, a `bar0_write`
    /// and a lock, to deliver a notification that carries no information beyond
    /// having happened -- the queue index is already in the address.
    ///
    /// The cursor queue gets no doorbell because nothing serves it.
    fn ctl_doorbell(&self) -> Doorbell {
        Doorbell {
            bar_idx: 0,
            offset: OFF_NOTIFY + CTL_INDEX as u64 * NOTIFY_MULT as u64,
            fd: self.ctl_kick.clone(),
        }
    }
}

impl PciDevice for GpuDevice {
    fn doorbells(&self) -> Vec<Doorbell> {
        vec![self.ctl_doorbell()]
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk the PCI capability list and return the virtio notify capability's
    /// `(bar, offset, multiplier)` -- which is the only description of the
    /// notify register the guest ever sees.
    fn notify_cap(cfg: &[u8; 256]) -> (u8, u32, u32) {
        let mut ptr = cfg[0x34] as usize;
        while ptr != 0 {
            let id = cfg[ptr];
            let next = cfg[ptr + 1] as usize;
            // Vendor-specific, and cfg_type 2 is Notify.
            if id == 0x09 && cfg[ptr + 3] == 2 {
                let at = |o: usize| {
                    u32::from_le_bytes([
                        cfg[ptr + o],
                        cfg[ptr + o + 1],
                        cfg[ptr + o + 2],
                        cfg[ptr + o + 3],
                    ])
                };
                return (cfg[ptr + 4], at(8), at(16));
            }
            ptr = next;
        }
        panic!("the GPU advertises no virtio notify capability");
    }

    /// **The doorbell has to be at the address the guest was told to write.**
    ///
    /// Nothing checks this at runtime and nothing can: a doorbell registered at
    /// the wrong address is answered by the kernel for writes that never come,
    /// every real notify exits to userspace as it did before, and the only
    /// symptom is that the device is slow again. Both halves are derived from
    /// the same constants here, so this fails the moment they stop agreeing.
    #[test]
    fn the_doorbell_sits_where_the_notify_capability_says_it_does() {
        let (cfg, _) = GpuDevice::build_pci_config();
        let (bar, offset, multiplier) = notify_cap(&cfg);

        // What a driver computes for the control queue: the capability's offset
        // plus its `queue_notify_off` times the multiplier.
        let guest_writes = offset as u64 + CTL_INDEX as u64 * multiplier as u64;

        assert_eq!(bar, 0, "the notify register is in BAR0");
        assert_eq!(
            guest_writes,
            OFF_NOTIFY + CTL_INDEX as u64 * NOTIFY_MULT as u64,
            "the capability and the doorbell disagree about where the control \
             queue is kicked"
        );
        assert!(
            guest_writes < BAR0_SIZE,
            "the notify register is outside BAR0"
        );
    }
}
