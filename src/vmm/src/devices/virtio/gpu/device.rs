// Copyright 2024 - Firecracker GPU port
// Virtio GPU device struct and VirtioDevice trait implementation.
//
// Architecture:
//  - The `Gpu` struct is owned by the event loop (main VMM thread).
//  - On activation a single background worker thread is spawned; it owns
//    the `VirtioGpu` state machine and processes all commands from the CTL
//    queue.  This mirrors libkrun's design and is necessary because rutabaga
//    can block for arbitrarily long (GL draw calls, etc.).
//  - The CTL queue is also wrapped in `Arc<Mutex<Queue>>` so rutabaga's
//    internal fence-completion thread can call `add_used` /
//    `advance_used_ring_idx` and signal the interrupt without touching the
//    event loop.
//  - The outer `queues` Vec<Queue> satisfies the VirtioDevice trait; its CTL
//    slot is initialised and then cloned into the Arc<Mutex> on activation.
//    After activation only the Arc<Mutex> copy tracks live state.

use crate::{MutEventSubscriber, impl_device_type};
use std::io::Write;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Sender, unbounded};
use log::{error, warn};
use vmm_sys_util::eventfd::EventFd;

use crate::devices::virtio::ActivateError;
use crate::devices::virtio::device::{ActiveState, DeviceState, VirtioDevice, VirtioDeviceType};
use crate::devices::virtio::queue::Queue;
use crate::devices::virtio::transport::{VirtioInterrupt, VirtioInterruptType};
use crate::vstate::memory::GuestMemoryMmap;

use super::display::DisplayInfo;
use super::uapi::virtio_gpu_config;
use super::worker::Worker;
use super::{
    AVAIL_FEATURES, CTL_INDEX, CUR_INDEX, NUM_QUEUES, QUEUE_SIZES, VirtioShmRegion,
    uapi::VIRTIO_ID_GPU,
};
use vm_memory::ByteValued;

// ---------------------------------------------------------------------------
// Gpu struct
// ---------------------------------------------------------------------------

// Gpu must implement Debug because attach_pci_virtio_device requires
// `T: 'static + VirtioDevice + MutEventSubscriber + Debug`.
#[derive(Debug)]
pub struct Gpu {
    // ── VirtioDevice required fields ────────────────────────────────────────
    pub(crate) id: String,
    pub(crate) queues: Vec<Queue>,
    pub(crate) queue_evts: Vec<EventFd>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) device_state: DeviceState,
    pub(crate) activate_evt: EventFd,

    // ── GPU-specific ─────────────────────────────────────────────────────────
    /// The CTL queue is also held here (behind a Mutex) so the rutabaga fence
    /// handler thread can retire completed descriptors without holding the
    /// event-loop lock.
    pub(crate) queue_ctl: Arc<Mutex<Queue>>,

    /// Channel used by the event handler to tell the worker which queue
    /// index fired.
    pub(crate) sender: Option<Sender<u64>>,

    /// VirGL renderer creation flags.
    /// `pub(crate)` so persist.rs can read it for snapshotting.
    pub(crate) virgl_flags: u32,

    /// Host-visible SHM window used for blob resource mapping.
    pub(crate) shm_region: Option<VirtioShmRegion>,

    /// Per-scanout display configuration (width/height/EDID).
    /// `pub(crate)` so persist.rs can iterate it for snapshotting.
    pub(crate) displays: Box<[DisplayInfo]>,

    pub(crate) num_capsets: Arc<AtomicU32>,
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

impl Gpu {
    pub fn new(
        id: String,
        virgl_flags: u32,
        displays: Box<[DisplayInfo]>,
    ) -> Result<Self, super::GpuError> {
        let queues: Vec<Queue> = QUEUE_SIZES.iter().map(|&s| Queue::new(s)).collect();

        let mut queue_evts = Vec::with_capacity(NUM_QUEUES);
        for _ in 0..NUM_QUEUES {
            queue_evts.push(
                EventFd::new(vmm_sys_util::eventfd::EFD_NONBLOCK)
                    .map_err(super::GpuError::EventFd)?,
            );
        }

        // Create an initially-empty placeholder for queue_ctl; it is replaced
        // with the real initialised queue inside `activate`.
        let queue_ctl = Arc::new(Mutex::new(Queue::new(QUEUE_SIZES[CTL_INDEX])));

        Ok(Gpu {
            id,
            queues,
            queue_evts,
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            device_state: DeviceState::Inactive,
            activate_evt: EventFd::new(vmm_sys_util::eventfd::EFD_NONBLOCK)
                .map_err(super::GpuError::EventFd)?,
            queue_ctl,
            sender: None,
            virgl_flags,
            shm_region: None,
            displays,
            num_capsets: Arc::new(AtomicU32::new(1)),
        })
    }

    /// Provide the host-visible SHM region used for blob resource mapping.
    /// Must be called before `activate` if blob resources are used.
    pub fn set_shm_region(&mut self, shm_region: VirtioShmRegion) {
        self.shm_region = Some(shm_region);
    }
}

// ---------------------------------------------------------------------------
// VirtioDevice implementation
// ---------------------------------------------------------------------------

impl VirtioDevice for Gpu {
    impl_device_type!(VirtioDeviceType::Gpu);

    fn id(&self) -> &str {
        &self.id
    }

    fn shm_regions(&self) -> Vec<(u32, u64, u64)> {
        match &self.shm_region {
            Some(r) => vec![(1, r.guest_addr, r.size as u64)],
            None => vec![],
        }
    }

    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn queues(&self) -> &[Queue] {
        &self.queues
    }

    fn queues_mut(&mut self) -> &mut [Queue] {
        &mut self.queues
    }

    fn queue_events(&self) -> &[EventFd] {
        &self.queue_evts
    }

    fn prepare_save(&mut self) {
        if self.is_activated() {
            // The live CTL queue state is in queue_ctl (updated by the worker
            // and fence handler). Sync it back into self.queues so that
            // VirtioDeviceState::from_device reads current indices.
            self.queues[CTL_INDEX] = self.queue_ctl.lock().unwrap().clone();
        }
    }

    fn interrupt_trigger(&self) -> &dyn VirtioInterrupt {
        self.device_state
            .active_state()
            .expect("GPU interrupt_trigger called before activation")
            .interrupt
            .deref()
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config = virtio_gpu_config {
            events_read: 0,
            events_clear: 0,
            num_scanouts: self.displays.len() as u32,
            num_capsets: self.num_capsets.load(Ordering::Acquire),
        };

        let config_slice = config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("virtio-gpu: read_config offset {offset:#x} out of range");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            data.write_all(&config_slice[offset as usize..std::cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "virtio-gpu: guest attempted to write device config \
             (offset={offset:#x}, len={len:#x})",
            len = data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: Arc<dyn VirtioInterrupt>,
    ) -> Result<(), ActivateError> {
        if self.queues.len() != NUM_QUEUES {
            error!(
                "virtio-gpu: activate called with {} queues, expected {}",
                self.queues.len(),
                NUM_QUEUES
            );
            return Err(ActivateError::BadActivate);
        }

        // Initialise queues: maps guest-physical addresses to host raw pointers.
        for q in self.queues.iter_mut() {
            q.initialize(&mem)
                .map_err(ActivateError::QueueMemoryError)?;
        }

        // Optionally enable EVENT_IDX notification suppression.
        use crate::devices::virtio::generated::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
        if self.has_feature(u64::from(VIRTIO_RING_F_EVENT_IDX)) {
            for q in self.queues.iter_mut() {
                q.enable_notif_suppression();
            }
        }

        // The CTL queue is shared with the worker thread so the rutabaga fence
        // handler can retire descriptors without the event loop.
        // We clone the now-initialised Queue so both sides hold raw pointers
        // into the same guest memory region.
        //
        // SAFETY: Both the event loop (handling CUR queue) and the worker
        // (handling CTL queue) access *different* queues.  The only entity
        // that touches the CTL Queue concurrently is the fence handler inside
        // the Arc<Mutex>, which is serialised correctly.
        *self.queue_ctl.lock().unwrap() = self.queues[CTL_INDEX].clone();

        let shm_region = self.shm_region.clone().unwrap_or_else(|| {
            warn!("virtio-gpu: activating without SHM region – blob mapping will fail");
            VirtioShmRegion {
                host_addr: 0,
                guest_addr: 0,
                size: 0,
            }
        });

        let (sender, receiver) = unbounded();
        let worker = Worker::new(
            receiver,
            mem.clone(),
            self.queue_ctl.clone(),
            interrupt.clone(),
            shm_region,
            self.virgl_flags,
            self.displays.clone(),
            self.num_capsets.clone(),
        );
        worker.run();
        self.sender = Some(sender);

        if self.activate_evt.write(1).is_err() {
            error!("virtio-gpu: failed to write activate_evt");
            return Err(ActivateError::BadActivate);
        }

        self.device_state = DeviceState::Activated(ActiveState { mem, interrupt });
        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }
}
