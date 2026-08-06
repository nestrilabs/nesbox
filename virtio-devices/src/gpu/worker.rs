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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;

use std::sync::mpsc::Receiver;
use log::{debug, error};
use rutabaga_gfx::{
    RUTABAGA_PIPE_BIND_RENDER_TARGET, RUTABAGA_PIPE_TEXTURE_2D, ResourceCreate3D,
    ResourceCreateBlob, RutabagaFence, Transfer3D,
};
use vm_memory::GuestAddress;

use vm_memory::GuestMemoryMmap;

use super::{CTL_INDEX, GpuQueues};
use super::descriptor_utils::{Reader, Writer};
use super::display::DisplayInfo;
use super::display::Rect;
use super::protocol::{
    GpuCommand, GpuResponse, VIRTIO_GPU_FLAG_FENCE, VIRTIO_GPU_FLAG_INFO_RING_IDX, VirtioGpuResult,
    virtio_gpu_ctrl_hdr, virtio_gpu_mem_entry,
};
use super::virtio_gpu::{VirtioGpu, VirtioGpuRing};
use super::VirtioShmRegion;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Worker struct
// ---------------------------------------------------------------------------

pub struct Worker {
    /// Receives queue-index notifications from the event handler.
    receiver: Receiver<u64>,
    mem: GuestMemoryMmap,
    /// The control queue, shared with the fence handler inside VirtioGpu.
    queues: Arc<dyn GpuQueues>,
    shm_region: VirtioShmRegion,
    displays: Box<[DisplayInfo]>,
    pub num_capsets: Arc<AtomicU32>,
    gpu_device_path: PathBuf,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        receiver: Receiver<u64>,
        mem: GuestMemoryMmap,
        queues: Arc<dyn GpuQueues>,
        shm_region: VirtioShmRegion,
        displays: Box<[DisplayInfo]>,
        num_capsets: Arc<std::sync::atomic::AtomicU32>,
        gpu_device_path: PathBuf,
    ) -> Self {
        Worker {
            receiver,
            mem,
            queues,
            shm_region,
            displays,
            num_capsets,
            gpu_device_path,
        }
    }

    /// Spawn the worker on a dedicated OS thread.
    pub fn run(self) {
        thread::Builder::new()
            .name("virtio-gpu worker".into())
            .spawn(|| self.work())
            .expect("virtio-gpu: failed to spawn worker thread");
    }

    // -----------------------------------------------------------------------
    // Main loop
    // -----------------------------------------------------------------------

    fn work(mut self) {
        let start = std::time::Instant::now();
        let Some(mut virtio_gpu) = VirtioGpu::new(
            self.queues.clone(),
            self.displays.clone(),
            self.gpu_device_path.clone(),
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
            // Block until the event handler signals a queue event.
            // The sent value is the queue index (CTL_INDEX or CUR_INDEX).
            let queue_index = match self.receiver.recv() {
                Ok(idx) => idx as usize,
                Err(_) => {
                    // Sender dropped means the device is being torn down.
                    debug!("virtio-gpu worker: channel closed, exiting");
                    break;
                }
            };

            if queue_index == CTL_INDEX {
                self.process_ctl_queue(&mut virtio_gpu);
            }
            // CUR queue: cursor commands are not implemented for headless operation.
        }
    }

    // -----------------------------------------------------------------------
    // CTL queue processing
    // -----------------------------------------------------------------------

    fn process_ctl_queue(&mut self, virtio_gpu: &mut VirtioGpu) -> bool {
        let mut used_any = false;
        let mem = self.mem.clone();

        loop {
            // Pop the next available descriptor chain.
            let Some((desc_index, descs)) = self.queues.pop_ctl() else {
                break;
            };

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
                    let resp = self.process_gpu_command(virtio_gpu, &mem, hdr, cmd, &mut reader);
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
                self.queues.complete_ctl(&[(desc_index, 0)]);
                used_any = true;
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
                self.queues.complete_ctl(&[(desc_index, len)]);
                used_any = true;
            }
        }

        debug!("virtio-gpu: process_ctl_queue done (used_any={used_any})");
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

            GpuCommand::ResourceUnref(info) => virtio_gpu.unref_resource(info.resource_id),

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
