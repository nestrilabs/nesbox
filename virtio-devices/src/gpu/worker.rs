//! The GPU command dispatcher, on its own thread.
// Background worker thread for the virtio-gpu device.
//
// The worker owns the `VirtioGpu` state machine (rutabaga context, resource
// table, etc.) and loops over incoming queue-index notifications from the
// event handler.  The CTL queue is processed here; because rutabaga GL calls
// can take arbitrarily long we keep GPU work off the VMM event loop.
//
// The CUR (cursor) queue is currently not processed – cursor commands are
// unimplemented in this headless port.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use std::os::fd::AsRawFd;
use vmm_sys_util::eventfd::EventFd;

use super::metrics::GpuMetrics;
use log::{debug, error};
use rutabaga_gfx::{
    RUTABAGA_PIPE_BIND_RENDER_TARGET, RUTABAGA_PIPE_TEXTURE_2D, ResourceCreate3D,
    ResourceCreateBlob, RutabagaFence, Transfer3D,
};
use vm_memory::GuestAddress;

use vm_memory::GuestMemoryMmap;

use super::VirtioShmRegion;
use super::descriptor_utils::{Reader, Writer};
use super::display::DisplayInfo;
use super::display::Rect;
use super::protocol::{
    GpuCommand, GpuResponse, VIRTIO_GPU_FLAG_FENCE, VIRTIO_GPU_FLAG_INFO_RING_IDX, VirtioGpuResult,
    virtio_gpu_ctrl_hdr, virtio_gpu_mem_entry,
};
use super::virtio_gpu::{VirtioGpu, VirtioGpuRing};
use super::{GpuQueues, HostMemoryMapper};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Worker struct
// ---------------------------------------------------------------------------

pub struct Worker {
    /// Rung by KVM when the guest writes the control queue's notify register,
    /// or by `bar0_write` on a host where that registration did not take.
    kick: Arc<EventFd>,
    /// Set when the device is reset. Read after every wake.
    stop: Arc<AtomicBool>,
    /// How long to look at the ring before sleeping. Zero sleeps at once.
    poll_us: u64,
    /// Host CPUs this thread may run on. Empty means no affinity.
    cpu_affinity: Vec<usize>,
    mem: GuestMemoryMmap,
    /// The control queue, shared with the fence handler inside VirtioGpu.
    queues: Arc<dyn GpuQueues>,
    shm_region: VirtioShmRegion,
    displays: Box<[DisplayInfo]>,
    pub num_capsets: Arc<AtomicU32>,
    gpu_device_path: PathBuf,
    mapper: Arc<dyn HostMemoryMapper>,
    /// Per-guest device-memory limit in bytes, or `None` for unbounded.
    vram_limit_bytes: Option<u64>,
    window_limit_bytes: u64,
    window_max_mappings: u32,
    metrics: Arc<GpuMetrics>,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kick: Arc<EventFd>,
        stop: Arc<AtomicBool>,
        poll_us: u64,
        cpu_affinity: Vec<usize>,
        mem: GuestMemoryMmap,
        queues: Arc<dyn GpuQueues>,
        shm_region: VirtioShmRegion,
        displays: Box<[DisplayInfo]>,
        num_capsets: Arc<std::sync::atomic::AtomicU32>,
        gpu_device_path: PathBuf,
        mapper: Arc<dyn HostMemoryMapper>,
        vram_limit_bytes: Option<u64>,
        window_limit_bytes: u64,
        window_max_mappings: u32,
        metrics: Arc<GpuMetrics>,
    ) -> Self {
        Worker {
            kick,
            stop,
            poll_us,
            cpu_affinity,
            mem,
            queues,
            shm_region,
            displays,
            num_capsets,
            gpu_device_path,
            mapper,
            vram_limit_bytes,
            window_limit_bytes,
            window_max_mappings,
            metrics,
        }
    }

    /// Spawn the worker on a dedicated OS thread.
    /// Start the worker, handing back the join handle.
    ///
    /// **The handle is not decoration.** The doorbell is one eventfd for the
    /// life of the device -- it has to be, since KVM is told about it when the
    /// device joins the bus -- so a worker from a previous activation that is
    /// still alive would be blocked on the same fd as the new one and would
    /// consume kicks meant for it. The reset path joins on this to make
    /// "stopped" mean stopped.
    pub fn run(self) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("virtio-gpu worker".into())
            .spawn(|| self.work())
            .expect("virtio-gpu: failed to spawn worker thread")
    }

    // -----------------------------------------------------------------------
    // Main loop
    // -----------------------------------------------------------------------

    fn work(mut self) {
        // Before anything else, and on this thread rather than the one that
        // spawned it: `sched_setaffinity` with pid 0 acts on the caller.
        self.confine();

        let start = std::time::Instant::now();
        let Some(mut virtio_gpu) = VirtioGpu::new(
            self.queues.clone(),
            self.displays.clone(),
            self.gpu_device_path.clone(),
            self.mapper.clone(),
            self.vram_limit_bytes,
            self.window_limit_bytes,
            self.window_max_mappings,
            self.metrics.clone(),
        ) else {
            log::error!(
                "virtio-gpu: backend failed to initialise; the device will accept \
                 no commands. Check that {:?} exists and is a render node.",
                self.gpu_device_path
            );
            return;
        };
        log::info!(
            "virtio-gpu worker: rutabaga init took {:?}",
            start.elapsed()
        );

        let actual = virtio_gpu.num_capsets;
        log::info!("virtio-gpu worker: rutabaga reports {actual} capsets");
        if actual != self.num_capsets.load(Ordering::Acquire) {
            log::error!(
                "virtio-gpu: capset count mismatch! config={}, actual={}. \
                 Guest may malfunction.",
                self.num_capsets.load(Ordering::Acquire),
                actual
            );
            // Update anyway so at least future reads are correct
            self.num_capsets.store(actual, Ordering::Release);
        }

        loop {
            self.wait();
            if self.stop.load(Ordering::Acquire) {
                debug!("virtio-gpu worker: asked to stop, exiting");
                break;
            }
            self.process_ctl_queue(&mut virtio_gpu);
            // CUR queue: cursor commands are not implemented for headless operation.
        }
    }

    /// Confine this thread to the CPUs the guest's vCPUs were given.
    ///
    /// A warning rather than a failure, for the same reason the vCPU threads
    /// treat it that way: placement is an optimisation, and a box that runs on
    /// the wrong cores is better than one that does not start. A set naming
    /// CPUs this host does not have is the usual cause and is worth seeing.
    fn confine(&self) {
        if self.cpu_affinity.is_empty() {
            return;
        }
        // SAFETY: all-zeros is a valid cpu_set_t; CPU_ZERO makes it explicit.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        unsafe { libc::CPU_ZERO(&mut set) };
        let mut named = 0usize;
        for &cpu in &self.cpu_affinity {
            if cpu < libc::CPU_SETSIZE as usize {
                // SAFETY: FFI call, index bounds checked above.
                unsafe { libc::CPU_SET(cpu, &mut set) };
                named += 1;
            }
        }
        if named == 0 {
            log::warn!("virtio-gpu: no CPU in the affinity set exists here; leaving it unset");
            return;
        }
        // SAFETY: FFI call; pid 0 is the calling thread and the size matches.
        let ret =
            unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) };
        if ret != 0 {
            log::warn!(
                "virtio-gpu: could not set the worker's CPU affinity: {}",
                std::io::Error::last_os_error()
            );
        } else {
            log::info!("virtio-gpu: worker confined to {} CPU(s)", named);
        }
    }

    /// Wait until there is something on the control queue, or the device stops.
    ///
    /// # Why it looks at the ring before it sleeps
    ///
    /// A guest under load kicks the control queue once per command submission
    /// -- measured at ~24,000 a second with one game running, a gap of about
    /// 42 us between them. A worker that sleeps in that gap pays a futex wake
    /// and a scheduler round trip to be woken again almost immediately, and the
    /// GPU is idle for all of it: the guest submits, waits for its fence, and
    /// submits again, so there is no second submission in flight to hide the
    /// wakeup behind.
    ///
    /// Reading the avail ring turns that wakeup into a memory read, and can see
    /// the submission before the doorbell announcing it has finished being
    /// delivered.
    ///
    /// The cost is bounded and lands only where there is work: the spin runs
    /// for at most `poll_us` after each wake and then blocks, so an idle guest
    /// spins once and stops. Under a steady stream it will spend most of the
    /// window spinning, which approaches a busy core -- which is the trade, and
    /// why the window is configurable rather than assumed.
    fn wait(&mut self) {
        if self.poll_us > 0 {
            let began = Instant::now();
            let deadline = began + Duration::from_micros(self.poll_us);
            loop {
                if self.queues.ctl_has_work() || self.stop.load(Ordering::Acquire) {
                    // The doorbell is drained whether or not it was what told
                    // us: the ring can show the submission before the kick
                    // announcing it has been delivered, and leaving a stale
                    // count behind would turn the next block into a spurious
                    // return. Non-blocking, so draining an empty one is a no-op
                    // rather than the sleep this branch is avoiding.
                    let _ = self.kick.read();
                    self.metrics.counters.spin.since(began);
                    return;
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::hint::spin_loop();
            }
            self.metrics.counters.spin.since(began);
        }
        let slept = Instant::now();

        // **`poll` and not `read`.** The doorbell is `EFD_NONBLOCK`, because
        // every other holder of it -- KVM's ioeventfd, `bar0_write` -- must
        // never block on a full counter. A `read` on it would return `EAGAIN`
        // at once and turn this into a busy loop burning a core, so the sleep
        // is done by waiting for the fd to become readable and the read is left
        // non-blocking.
        //
        // A kick that arrived during the spin has already raised the counter,
        // so this returns immediately rather than losing it.
        let mut fds = libc::pollfd {
            fd: self.kick.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialised `pollfd` describing an fd this worker owns,
        // and a negative timeout, which is `poll`'s documented "wait forever".
        let polled = unsafe { libc::poll(&mut fds, 1, -1) };
        self.metrics.counters.sleep.since(slept);
        if polled < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                error!("virtio-gpu: poll on the doorbell failed: {err}");
            }
            return;
        }
        let _ = self.kick.read();
    }

    // -----------------------------------------------------------------------
    // CTL queue processing
    // -----------------------------------------------------------------------

    /// Drain the control queue, then retire everything it produced at once.
    ///
    /// # One interrupt for the batch, not one per command
    ///
    /// `complete_ctl` ends in an MSI-X injection, and the guest runs its
    /// interrupt handler once for each however many descriptors it retires.
    /// This loop used to call it per descriptor, which under a game meant
    /// roughly one interrupt per command submission -- tens of thousands a
    /// second, each one stealing the vCPU that was trying to submit the next.
    ///
    /// The fence handler in `virtio_gpu.rs` has always collected first and
    /// retired once; this is the other caller of the same API agreeing with it.
    ///
    /// Retiring late is allowed: the used ring carries no ordering promise, and
    /// a fenced descriptor is already retired out of band by rutabaga's thread
    /// whenever its fence happens to signal.
    fn process_ctl_queue(&mut self, virtio_gpu: &mut VirtioGpu) -> bool {
        let mem = self.mem.clone();
        let mut completed: Vec<(u16, u32)> = Vec::new();
        let began = Instant::now();
        let mut taken = 0u64;

        loop {
            // Pop the next available descriptor chain.
            let Some((desc_index, descs)) = self.queues.pop_ctl() else {
                break;
            };

            taken += 1;
            let mut reader = match Reader::new(&mem, &descs) {
                Ok(r) => r,
                Err(e) => {
                    error!("virtio-gpu: failed to create Reader: {e:?}");
                    continue;
                }
            };
            let mut writer = match Writer::new(&mem, &descs) {
                Ok(w) => w,
                Err(e) => {
                    error!("virtio-gpu: failed to create Writer: {e:?}");
                    continue;
                }
            };

            // Decode the command.
            let (hdr, cmd, resp) = match GpuCommand::decode(&mut reader) {
                Ok((hdr, cmd)) => {
                    let at = Instant::now();
                    let resp = self.process_gpu_command(virtio_gpu, &mem, hdr, cmd, &mut reader);
                    self.metrics.counters.command.since(at);
                    self.metrics.counters.command_kind.record(cmd.kind(), at);
                    (Some(hdr), Some(cmd), resp)
                }
                Err(e) => {
                    debug!("virtio-gpu: decode error: {e:?}");
                    (None, None, Err(GpuResponse::ErrUnspec))
                }
            };

            let mut gpu_response = match resp {
                Ok(r) => r,
                Err(r) => {
                    debug!("{cmd:?} -> {r:?}");
                    r
                }
            };

            // Skip writing the response if no writable descriptors were provided.
            if writer.available_bytes() == 0 {
                completed.push((desc_index, 0));
                continue;
            }

            // Fence handling: if the command had a FENCE flag, the descriptor
            // must be retired only after rutabaga signals completion.
            let mut add_to_queue = true;
            let mut len = 0u32;

            let (flags, fence_id, ctx_id, ring_idx) = if let Some(hdr) = hdr {
                if hdr.flags & VIRTIO_GPU_FLAG_FENCE != 0 {
                    let fence = RutabagaFence {
                        flags: hdr.flags,
                        fence_id: hdr.fence_id,
                        ctx_id: hdr.ctx_id,
                        ring_idx: hdr.ring_idx,
                    };
                    gpu_response = match virtio_gpu.create_fence(fence) {
                        Ok(_) => gpu_response,
                        Err(fence_resp) => {
                            log::warn!("virtio-gpu: create_fence -> {fence_resp:?}");
                            fence_resp
                        }
                    };
                    (hdr.flags, hdr.fence_id, hdr.ctx_id, hdr.ring_idx)
                } else {
                    (0, 0, 0, 0)
                }
            } else {
                (0, 0, 0, 0)
            };

            // Encode the response into the writable descriptors.
            match gpu_response.encode(flags, fence_id, ctx_id, ring_idx, &mut writer) {
                Ok(l) => len = l,
                Err(e) => debug!("virtio-gpu: response encode error: {e:?}"),
            }

            // If this descriptor is fenced, hand it off to the fence tracker.
            if flags & VIRTIO_GPU_FLAG_FENCE != 0 {
                let ring = match flags & VIRTIO_GPU_FLAG_INFO_RING_IDX {
                    0 => VirtioGpuRing::Global,
                    _ => VirtioGpuRing::ContextSpecific { ctx_id, ring_idx },
                };
                add_to_queue = virtio_gpu.process_fence(ring, fence_id, desc_index, len);
            }

            if add_to_queue {
                completed.push((desc_index, len));
            }
        }

        let used_any = !completed.is_empty();
        if used_any {
            self.queues.complete_ctl(&completed);
        }
        if taken > 0 {
            self.metrics
                .counters
                .drained
                .fetch_add(taken, Ordering::Relaxed);
            self.metrics.counters.drain.since(began);
        }
        debug!(
            "virtio-gpu: process_ctl_queue done ({} retired in one interrupt)",
            completed.len()
        );
        used_any
    }

    // -----------------------------------------------------------------------
    // Per-command dispatch
    // -----------------------------------------------------------------------

    fn process_gpu_command(
        &mut self,
        virtio_gpu: &mut VirtioGpu,
        mem: &GuestMemoryMmap,
        hdr: virtio_gpu_ctrl_hdr,
        cmd: GpuCommand,
        reader: &mut Reader,
    ) -> VirtioGpuResult {
        virtio_gpu.force_ctx_0();

        match cmd {
            // ── Display info ────────────────────────────────────────────────
            GpuCommand::GetDisplayInfo => virtio_gpu.display_info(),

            GpuCommand::GetEdid(info) => virtio_gpu.get_edid(info.scanout),

            // ── 2-D resource management ─────────────────────────────────────
            GpuCommand::ResourceCreate2d(info) => {
                let resource_create_3d = ResourceCreate3D {
                    target: RUTABAGA_PIPE_TEXTURE_2D,
                    format: info.format,
                    bind: RUTABAGA_PIPE_BIND_RENDER_TARGET,
                    width: info.width,
                    height: info.height,
                    depth: 1,
                    array_size: 1,
                    last_level: 0,
                    nr_samples: 0,
                    flags: 0,
                };
                virtio_gpu.resource_create_3d(info.resource_id, resource_create_3d)
            }

            GpuCommand::ResourceUnref(info) => {
                virtio_gpu.unref_resource(info.resource_id, &self.shm_region)
            }

            GpuCommand::SetScanout(info) => virtio_gpu.set_scanout(
                info.scanout_id,
                info.resource_id,
                info.r.width,
                info.r.height,
            ),

            GpuCommand::SetScanoutBlob(_info) => {
                log::warn!("virtio-gpu: SetScanoutBlob is not implemented");
                Err(GpuResponse::ErrUnspec)
            }

            GpuCommand::ResourceFlush(info) => {
                let rect = Rect {
                    x: info.r.x,
                    y: info.r.y,
                    width: info.r.width,
                    height: info.r.height,
                };
                virtio_gpu.flush_resource(info.resource_id, rect)
            }

            GpuCommand::TransferToHost2d(info) => {
                let transfer = Transfer3D::new_2d(
                    info.r.x,
                    info.r.y,
                    info.r.width,
                    info.r.height,
                    info.offset,
                );
                virtio_gpu.transfer_write(0, info.resource_id, transfer)
            }

            GpuCommand::ResourceAttachBacking(info) => {
                if reader.available_bytes() == 0 {
                    error!("virtio-gpu: ResourceAttachBacking missing backing entries");
                    return Err(GpuResponse::ErrUnspec);
                }
                // `nr_entries` is a guest u32 and it used to size a host Vec
                // directly, so a guest could reserve 64 GiB before a single
                // entry was read. An entry cannot exist unless there are bytes
                // to read it from, so the chain is the bound -- exact, and no
                // constant to guess at.
                let fits = reader.available_bytes() / size_of::<virtio_gpu_mem_entry>();
                if info.nr_entries as usize > fits {
                    error!(
                        "virtio-gpu: ResourceAttachBacking claims {} entries, but only \
                         {fits} fit the descriptor chain",
                        info.nr_entries
                    );
                    return Err(GpuResponse::ErrUnspec);
                }
                let mut vecs = Vec::with_capacity(info.nr_entries as usize);
                for _ in 0..info.nr_entries {
                    let entry = match reader.read_obj::<virtio_gpu_mem_entry>() {
                        Ok(e) => e,
                        Err(_) => return Err(GpuResponse::ErrUnspec),
                    };
                    vecs.push((GuestAddress(entry.addr), entry.length as usize));
                }
                virtio_gpu.attach_backing(info.resource_id, mem, vecs)
            }

            GpuCommand::ResourceDetachBacking(info) => virtio_gpu.detach_backing(info.resource_id),

            // ── Cursor (headless – not implemented) ─────────────────────────
            GpuCommand::UpdateCursor(_) | GpuCommand::MoveCursor(_) => {
                log::warn!("virtio-gpu: cursor commands are not implemented in headless mode");
                Ok(GpuResponse::OkNoData)
            }

            // ── UUID ────────────────────────────────────────────────────────
            GpuCommand::ResourceAssignUuid(info) => {
                virtio_gpu.resource_assign_uuid(info.resource_id)
            }

            // ── Capability sets ─────────────────────────────────────────────
            GpuCommand::GetCapsetInfo(info) => virtio_gpu.get_capset_info(info.capset_index),

            GpuCommand::GetCapset(info) => {
                virtio_gpu.get_capset(info.capset_id, info.capset_version)
            }

            // ── Context management ──────────────────────────────────────────
            GpuCommand::CtxCreate(info) => {
                let name = String::from_utf8(info.debug_name.to_vec()).ok();
                virtio_gpu.create_context(hdr.ctx_id, info.context_init, name.as_deref())
            }

            GpuCommand::CtxDestroy(_) => virtio_gpu.destroy_context(hdr.ctx_id),

            GpuCommand::CtxAttachResource(info) => {
                virtio_gpu.context_attach_resource(hdr.ctx_id, info.resource_id)
            }

            GpuCommand::CtxDetachResource(info) => {
                virtio_gpu.context_detach_resource(hdr.ctx_id, info.resource_id)
            }

            // ── 3-D operations ──────────────────────────────────────────────
            GpuCommand::ResourceCreate3d(info) => {
                let rc3d = ResourceCreate3D {
                    target: info.target,
                    format: info.format,
                    bind: info.bind,
                    width: info.width,
                    height: info.height,
                    depth: info.depth,
                    array_size: info.array_size,
                    last_level: info.last_level,
                    nr_samples: info.nr_samples,
                    flags: info.flags,
                };
                virtio_gpu.resource_create_3d(info.resource_id, rc3d)
            }

            GpuCommand::TransferToHost3d(info) => {
                let transfer = Transfer3D {
                    x: info.box_.x,
                    y: info.box_.y,
                    z: info.box_.z,
                    w: info.box_.w,
                    h: info.box_.h,
                    d: info.box_.d,
                    level: info.level,
                    stride: info.stride,
                    layer_stride: info.layer_stride,
                    offset: info.offset,
                };
                virtio_gpu.transfer_write(hdr.ctx_id, info.resource_id, transfer)
            }

            GpuCommand::TransferFromHost3d(info) => {
                let transfer = Transfer3D {
                    x: info.box_.x,
                    y: info.box_.y,
                    z: info.box_.z,
                    w: info.box_.w,
                    h: info.box_.h,
                    d: info.box_.d,
                    level: info.level,
                    stride: info.stride,
                    layer_stride: info.layer_stride,
                    offset: info.offset,
                };
                virtio_gpu.transfer_read(hdr.ctx_id, info.resource_id, transfer, None)
            }

            GpuCommand::CmdSubmit3d(info) => {
                if reader.available_bytes() == 0 {
                    // Accept empty submit (useful for benchmarking).
                    return Ok(GpuResponse::OkNoData);
                }
                let num_fences = info.num_in_fences as usize;
                let cmd_size = info.size as usize;
                // Both are guest u32s that sized host allocations before
                // anything was read: 32 GiB of fence ids and 4 GiB of command
                // buffer, from one command. As above, what the chain actually
                // carries is the bound.
                let claimed = num_fences
                    .checked_mul(size_of::<u64>())
                    .and_then(|fences| fences.checked_add(cmd_size));
                if claimed.is_none_or(|n| n > reader.available_bytes()) {
                    error!(
                        "virtio-gpu: SUBMIT_3D claims {num_fences} fences and {cmd_size} \
                         command bytes, more than the {} the chain carries",
                        reader.available_bytes()
                    );
                    return Err(GpuResponse::ErrInvalidParameter);
                }
                let mut fence_ids: Vec<u64> = Vec::with_capacity(num_fences);
                for _ in 0..num_fences {
                    match reader.read_obj::<u64>() {
                        Ok(id) => fence_ids.push(id),
                        Err(_) => return Err(GpuResponse::ErrUnspec),
                    }
                }
                let mut cmd_buf = vec![0u8; cmd_size];
                if reader.read_exact(&mut cmd_buf).is_ok() {
                    virtio_gpu.submit_command(hdr.ctx_id, &mut cmd_buf, &fence_ids)
                } else {
                    Err(GpuResponse::ErrInvalidParameter)
                }
            }

            // ── Blob resources ──────────────────────────────────────────────
            GpuCommand::ResourceCreateBlob(info) => {
                let rc_blob = ResourceCreateBlob {
                    blob_mem: info.blob_mem,
                    blob_flags: info.blob_flags,
                    blob_id: info.blob_id,
                    size: info.size,
                };
                if reader.available_bytes() == 0 && info.nr_entries > 0 {
                    return Err(GpuResponse::ErrUnspec);
                }
                let mut vecs = Vec::with_capacity(info.nr_entries as usize);
                for _ in 0..info.nr_entries {
                    let entry = match reader.read_obj::<virtio_gpu_mem_entry>() {
                        Ok(e) => e,
                        Err(_) => return Err(GpuResponse::ErrUnspec),
                    };
                    vecs.push((GuestAddress(entry.addr), entry.length as usize));
                }
                virtio_gpu.resource_create_blob(hdr.ctx_id, info.resource_id, rc_blob, vecs, mem)
            }

            GpuCommand::ResourceMapBlob(info) => {
                #[cfg(target_os = "linux")]
                {
                    virtio_gpu.resource_map_blob(info.resource_id, &self.shm_region, info.offset)
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = info;
                    log::warn!("virtio-gpu: ResourceMapBlob is only supported on Linux");
                    Err(GpuResponse::ErrUnspec)
                }
            }

            GpuCommand::ResourceUnmapBlob(info) => {
                #[cfg(target_os = "linux")]
                {
                    virtio_gpu.resource_unmap_blob(info.resource_id, &self.shm_region)
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = info;
                    log::warn!("virtio-gpu: ResourceUnmapBlob is only supported on Linux");
                    Err(GpuResponse::ErrUnspec)
                }
            }
        }
    }
}
