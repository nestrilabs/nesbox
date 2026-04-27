// Copyright 2024 - Firecracker GPU port
// Ported from libkrun's virtio_gpu.rs.  All krun_display / host-side scanout
// code has been removed.  The rutabaga backend and fence / resource management
// logic is kept as close to the original as possible.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::io::IoSliceMut;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rutabaga_gfx::{
    RUTABAGA_CHANNEL_TYPE_WAYLAND, RUTABAGA_MAP_CACHE_MASK, ResourceCreate3D, ResourceCreateBlob,
    Rutabaga, RutabagaBuilder, RutabagaChannel, RutabagaFence, RutabagaFenceHandler, RutabagaIovec,
    Transfer3D,
};
#[cfg(target_os = "linux")]
use rutabaga_gfx::{
    RUTABAGA_MAP_ACCESS_MASK, RUTABAGA_MAP_ACCESS_READ, RUTABAGA_MAP_ACCESS_RW,
    RUTABAGA_MAP_ACCESS_WRITE,
};
use vm_memory::{GuestAddress, GuestMemory};

use crate::devices::virtio::queue::Queue;
use crate::devices::virtio::transport::{VirtioInterrupt, VirtioInterruptType};
use crate::vstate::memory::GuestMemoryMmap;

use super::display::{DisplayInfo, Rect, ResourceFormat};
use super::protocol::GpuResponse::*;
use super::protocol::{
    GpuResponse, GpuResponsePlaneInfo, VIRTIO_GPU_BLOB_FLAG_CREATE_GUEST_HANDLE,
    VIRTIO_GPU_BLOB_MEM_HOST3D, VIRTIO_GPU_FLAG_INFO_RING_IDX, VIRTIO_GPU_MAX_SCANOUTS,
    VirtioGpuResult,
};
use super::{CTL_INDEX, GpuError, Result, VirtioShmRegion};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sglist_to_rutabaga_iovecs(
    vecs: &[(GuestAddress, usize)],
    mem: &GuestMemoryMmap,
) -> Result<Vec<RutabagaIovec>> {
    if vecs
        .iter()
        .any(|&(addr, len)| mem.get_slice(addr, len).is_err())
    {
        return Err(GpuError::GuestMemory);
    }
    let mut out = Vec::with_capacity(vecs.len());
    for &(addr, len) in vecs {
        let slice = mem.get_slice(addr, len).unwrap();
        out.push(RutabagaIovec {
            base: slice.ptr_guard_mut().as_ptr() as *mut libc::c_void,
            len,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Fence tracking
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum VirtioGpuRing {
    Global,
    ContextSpecific { ctx_id: u32, ring_idx: u8 },
}

#[derive(Debug)]
struct FenceDescriptor {
    ring: VirtioGpuRing,
    fence_id: u64,
    desc_index: u16,
    len: u32,
}

#[derive(Default, Debug)]
pub struct FenceState {
    descs: Vec<FenceDescriptor>,
    completed_fences: BTreeMap<VirtioGpuRing, u64>,
}

// ---------------------------------------------------------------------------
// Resource tracking
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, Default)]
struct AssociatedScanouts(u32);

impl AssociatedScanouts {
    fn enable(&mut self, scanout_id: u32) {
        self.0 |= 1 << scanout_id;
    }
    fn disable(&mut self, scanout_id: u32) {
        self.0 &= !(1 << scanout_id);
    }
    const fn has_any_enabled(self) -> bool {
        self.0 != 0
    }
    fn iter_enabled(self) -> impl Iterator<Item = u32> {
        (0..VIRTIO_GPU_MAX_SCANOUTS).filter(move |i| ((self.0 >> i) & 1) == 1)
    }
}

#[derive(Copy, Clone, Debug)]
struct VirtioGpuResource {
    id: u32,
    width: u32,
    height: u32,
    scanouts: AssociatedScanouts,
    format: Option<u32>,
    size: u64, // blob resources only
    shmem_offset: Option<u64>,
    rutabaga_external_mapping: bool,
}

impl VirtioGpuResource {
    fn new(id: u32, width: u32, height: u32, format: Option<u32>, size: u64) -> Self {
        VirtioGpuResource {
            id,
            width,
            height,
            scanouts: Default::default(),
            format,
            size,
            shmem_offset: None,
            rutabaga_external_mapping: false,
        }
    }
}

#[derive(Debug)]
struct VirtioGpuScanout {
    resource_id: u32,
}

// ---------------------------------------------------------------------------
// VirtioGpu – the main GPU state machine
// ---------------------------------------------------------------------------

pub struct VirtioGpu {
    rutabaga: Rutabaga,
    resources: BTreeMap<u32, VirtioGpuResource>,
    fence_state: Arc<Mutex<FenceState>>,
    scanouts: [Option<VirtioGpuScanout>; VIRTIO_GPU_MAX_SCANOUTS as usize],
    displays: Box<[DisplayInfo]>,
    pub num_capsets: u32,
}

impl fmt::Debug for VirtioGpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("")
            .field(&self.resources)
            .field(&self.fence_state)
            .field(&self.scanouts)
            .field(&self.displays)
            .finish()
    }
}

impl VirtioGpu {
    // -----------------------------------------------------------------------
    // Fence handler construction
    // -----------------------------------------------------------------------

    /// Build a [`RutabagaFenceHandler`] that runs on rutabaga's internal thread.
    ///
    /// When a fence completes the handler:
    ///   1. Looks up pending descriptor(s) waiting on that fence.
    ///   2. Calls `queue.add_used()` + `advance_used_ring_idx()` for each.
    ///   3. Fires the virtio interrupt.
    ///
    /// Note: Firecracker's `Queue::add_used` does *not* require a `mem`
    /// parameter (the queue manages its own raw pointers after `initialize`),
    /// which simplifies the closure considerably.
    fn create_fence_handler(
        queue_ctl: Arc<Mutex<Queue>>,
        fence_state: Arc<Mutex<FenceState>>,
        interrupt: Arc<dyn VirtioInterrupt>,
    ) -> RutabagaFenceHandler {
        RutabagaFenceHandler::new(move |completed_fence: RutabagaFence| {
            let ring = match completed_fence.flags & VIRTIO_GPU_FLAG_INFO_RING_IDX {
                0 => VirtioGpuRing::Global,
                _ => VirtioGpuRing::ContextSpecific {
                    ctx_id: completed_fence.ctx_id,
                    ring_idx: completed_fence.ring_idx,
                },
            };

            // Collect completed descriptors while holding fence_state.
            // Release the lock before touching the queue to maintain ordering
            // and avoid potential deadlocks with the main thread.
            let completed: Vec<FenceDescriptor> = {
                let mut fs = fence_state.lock().unwrap();
                let mut i = 0;
                let mut out = Vec::new();
                while i < fs.descs.len() {
                    if fs.descs[i].ring == ring && fs.descs[i].fence_id <= completed_fence.fence_id
                    {
                        out.push(fs.descs.remove(i));
                    } else {
                        i += 1;
                    }
                }
                fs.completed_fences.insert(ring, completed_fence.fence_id);
                out
            };

            if completed.is_empty() {
                return;
            }

            {
                let mut queue = queue_ctl.lock().unwrap();
                for desc in &completed {
                    if let Err(e) = queue.add_used(desc.desc_index, desc.len) {
                        log::error!("virtio-gpu fence: failed to add_used: {e:?}");
                    }
                }
                queue.advance_used_ring_idx();
            } // queue lock released before signalling

            if let Err(e) = interrupt.trigger(VirtioInterruptType::Queue(CTL_INDEX as u16)) {
                log::error!("virtio-gpu fence: failed to signal interrupt: {e:?}");
            }
        })
    }

    // -----------------------------------------------------------------------
    // Rutabaga builder helpers
    // -----------------------------------------------------------------------

    // NOTE: This are unneeded as everything runs in the guest
    // fn build_rutabaga_channels() -> Vec<RutabagaChannel> {
    //     let xdg = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
    //     let wl = env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    //     let mut channels = vec![RutabagaChannel {
    //         base_channel: PathBuf::from(format!("{xdg}/{wl}")),
    //         channel_type: RUTABAGA_CHANNEL_TYPE_WAYLAND,
    //     }];

    //     #[cfg(target_os = "linux")]
    //     {
    //         use rutabaga_gfx::{RUTABAGA_CHANNEL_TYPE_PW, RUTABAGA_CHANNEL_TYPE_X11};

    //         if let Ok(x_disp) = env::var("DISPLAY") {
    //             if let Some(num) = x_disp.strip_prefix(':') {
    //                 channels.push(RutabagaChannel {
    //                     base_channel: PathBuf::from(format!("/tmp/.X11-unix/X{num}")),
    //                     channel_type: RUTABAGA_CHANNEL_TYPE_X11,
    //                 });
    //             }
    //         }
    //         if let Ok(pw_dir) =
    //             env::var("PIPEWIRE_RUNTIME_DIR").or_else(|_| env::var("XDG_RUNTIME_DIR"))
    //         {
    //             let name = env::var("PIPEWIRE_REMOTE").unwrap_or_else(|_| "pipewire-0".into());
    //             let mut pw = PathBuf::from(pw_dir);
    //             pw.push(name);
    //             channels.push(RutabagaChannel {
    //                 base_channel: pw,
    //                 channel_type: RUTABAGA_CHANNEL_TYPE_PW,
    //             });
    //         }
    //     }

    //     channels
    // }

    /// Try to create a full rutabaga instance.
    pub fn create_rutabaga(
        queue_ctl: Arc<Mutex<Queue>>,
        interrupt: Arc<dyn VirtioInterrupt>,
        fence_state: Arc<Mutex<FenceState>>,
        virgl_flags: u32,
    ) -> Option<Rutabaga> {
        let capset_mask: u64 = (1 << rutabaga_gfx::RUTABAGA_CAPSET_DRM);
        // let channels = Self::build_rutabaga_channels();
        let builder = RutabagaBuilder::new(
            rutabaga_gfx::RutabagaComponentType::VirglRenderer,
            virgl_flags,
            capset_mask,
        );
        // .set_rutabaga_channels(Some(channels));

        let fence = Self::create_fence_handler(queue_ctl, fence_state, interrupt);
        match builder.build(fence, None) {
            Ok(r) => Some(r),
            Err(e) => {
                log::error!("virtio-gpu: rutabaga build failed: {e:?}");
                None
            }
        }
    }

    /// Fallback rutabaga that disables virgl entirely.
    pub fn create_fallback_rutabaga(
        queue_ctl: Arc<Mutex<Queue>>,
        interrupt: Arc<dyn VirtioInterrupt>,
        fence_state: Arc<Mutex<FenceState>>,
    ) -> Option<Rutabaga> {
        const VIRGLRENDERER_NO_VIRGL: u32 = 1 << 7;
        let builder = RutabagaBuilder::new(
            rutabaga_gfx::RutabagaComponentType::VirglRenderer,
            VIRGLRENDERER_NO_VIRGL,
            0,
        );
        let fence = Self::create_fence_handler(queue_ctl, fence_state, interrupt);
        builder.build(fence, None).ok()
    }

    // -----------------------------------------------------------------------
    // Constructor
    // -----------------------------------------------------------------------

    pub fn new(
        queue_ctl: Arc<Mutex<Queue>>,
        interrupt: Arc<dyn VirtioInterrupt>,
        virgl_flags: u32,
        displays: Box<[DisplayInfo]>,
    ) -> Self {
        let fence_state: Arc<Mutex<FenceState>> = Arc::new(Mutex::new(FenceState::default()));

        let rutabaga = match Self::create_rutabaga(
            queue_ctl.clone(),
            interrupt.clone(),
            fence_state.clone(),
            virgl_flags,
        ) {
            Some(r) => r,
            None => {
                log::warn!(
                    "virtio-gpu: failed to build backend with requested flags, \
                     falling back to safe defaults"
                );
                Self::create_fallback_rutabaga(queue_ctl, interrupt, fence_state.clone())
                    .expect("fallback rutabaga init failed")
            }
        };

        let mut num_capsets = 0u32;
        for i in 0.. {
            match rutabaga.get_capset_info(i) {
                Ok((id, ver, size)) => {
                    log::info!("virtio-gpu: capset[{i}] id={id} ver={ver} size={size}");
                    num_capsets += 1;
                }
                Err(_) => break,
            }
        }
        log::info!("virtio-gpu: {num_capsets} capsets available");

        Self {
            rutabaga,
            resources: Default::default(),
            fence_state,
            scanouts: Default::default(),
            displays,
            num_capsets,
        }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn result_from_query(&mut self, resource_id: u32) -> GpuResponse {
        match self.rutabaga.query(resource_id) {
            Ok(q) => OkResourcePlaneInfo {
                format_modifier: q.modifier,
                plane_info: (0..4)
                    .map(|i| GpuResponsePlaneInfo {
                        stride: q.strides[i],
                        offset: q.offsets[i],
                    })
                    .collect(),
            },
            Err(_) => OkNoData,
        }
    }

    pub fn force_ctx_0(&self) {
        self.rutabaga.force_ctx_0();
    }

    // -----------------------------------------------------------------------
    // Display info / EDID
    // -----------------------------------------------------------------------

    pub fn display_info(&self) -> VirtioGpuResult {
        let info = self
            .displays
            .iter()
            .map(|d| (d.width, d.height, true))
            .collect();
        Ok(OkDisplayInfo(info))
    }

    pub fn get_edid(&self, scanout_id: u32) -> VirtioGpuResult {
        let display = self
            .displays
            .get(scanout_id as usize)
            .ok_or(ErrInvalidScanoutId)?;
        Ok(OkEdid(display.edid_bytes()))
    }

    // -----------------------------------------------------------------------
    // Scanout management (no host display – just book-keeping)
    // -----------------------------------------------------------------------

    /// Track which resource is associated with a scanout.
    /// No actual host display operations are performed.
    pub fn set_scanout(
        &mut self,
        scanout_id: u32,
        resource_id: u32,
        _width: u32,
        _height: u32,
    ) -> VirtioGpuResult {
        let scanout = self
            .scanouts
            .get_mut(scanout_id as usize)
            .ok_or(ErrInvalidScanoutId)?;

        // Detach from old resource.
        if let Some(old_id) = scanout.as_ref().map(|s| s.resource_id) {
            if let Some(res) = self.resources.get_mut(&old_id) {
                res.scanouts.disable(scanout_id);
            }
        }

        if resource_id == 0 {
            *scanout = None;
            return Ok(OkNoData);
        }

        let resource = self
            .resources
            .get_mut(&resource_id)
            .ok_or(ErrInvalidResourceId)?;
        resource.scanouts.enable(scanout_id);
        *scanout = Some(VirtioGpuScanout { resource_id });
        Ok(OkNoData)
    }

    // -----------------------------------------------------------------------
    // Resource flush
    // -----------------------------------------------------------------------
    //
    // Without a display backend there is nothing to present.  Return success
    // so the guest driver does not stall.

    pub fn flush_resource(&mut self, resource_id: u32, _rect: Rect) -> VirtioGpuResult {
        if resource_id == 0 {
            return Ok(OkNoData);
        }
        // Verify the resource exists.
        let _resource = self
            .resources
            .get(&resource_id)
            .ok_or(ErrInvalidResourceId)?;

        // No host display – nothing to flush.
        Ok(OkNoData)
    }

    // -----------------------------------------------------------------------
    // 3-D resource management
    // -----------------------------------------------------------------------

    pub fn resource_create_3d(
        &mut self,
        resource_id: u32,
        resource_create_3d: ResourceCreate3D,
    ) -> VirtioGpuResult {
        self.rutabaga
            .resource_create_3d(resource_id, resource_create_3d)?;

        let format = if resource_create_3d.format != 0 {
            Some(resource_create_3d.format)
        } else {
            None
        };

        let resource = VirtioGpuResource::new(
            resource_id,
            resource_create_3d.width,
            resource_create_3d.height,
            format,
            0,
        );
        self.resources.insert(resource_id, resource);
        Ok(self.result_from_query(resource_id))
    }

    pub fn unref_resource(&mut self, resource_id: u32) -> VirtioGpuResult {
        let resource = self
            .resources
            .remove(&resource_id)
            .ok_or(ErrInvalidResourceId)?;

        if resource.scanouts.has_any_enabled() {
            log::warn!(
                "virtio-gpu: unref_resource({resource_id}) while scanouts are active, refusing"
            );
            self.resources.insert(resource_id, resource);
            return Err(ErrUnspec);
        }

        if resource.rutabaga_external_mapping {
            self.rutabaga.unmap(resource_id)?;
        }
        self.rutabaga.unref_resource(resource_id)?;
        Ok(OkNoData)
    }

    pub fn transfer_write(
        &mut self,
        ctx_id: u32,
        resource_id: u32,
        transfer: Transfer3D,
    ) -> VirtioGpuResult {
        self.rutabaga
            .transfer_write(ctx_id, resource_id, transfer)?;
        Ok(OkNoData)
    }

    pub fn transfer_read(
        &mut self,
        _ctx_id: u32,
        _resource_id: u32,
        _transfer: Transfer3D,
        _buf: Option<&mut [u8]>,
    ) -> VirtioGpuResult {
        // Not required for the headless use-case.
        log::warn!("virtio-gpu: transfer_read is not implemented");
        Err(ErrUnspec)
    }

    pub fn attach_backing(
        &mut self,
        resource_id: u32,
        mem: &GuestMemoryMmap,
        vecs: Vec<(GuestAddress, usize)>,
    ) -> VirtioGpuResult {
        let iovecs = sglist_to_rutabaga_iovecs(&vecs, mem).map_err(|_| ErrUnspec)?;
        self.rutabaga.attach_backing(resource_id, iovecs)?;
        Ok(OkNoData)
    }

    pub fn detach_backing(&mut self, resource_id: u32) -> VirtioGpuResult {
        self.rutabaga.detach_backing(resource_id)?;
        Ok(OkNoData)
    }

    pub fn resource_assign_uuid(&self, resource_id: u32) -> VirtioGpuResult {
        if !self.resources.contains_key(&resource_id) {
            return Err(ErrInvalidResourceId);
        }
        let mut uuid = [0u8; 16];
        for (i, byte) in resource_id.to_be_bytes().iter().enumerate() {
            uuid[12 + i] = *byte;
        }
        Ok(OkResourceUuid { uuid })
    }

    // -----------------------------------------------------------------------
    // Capability sets
    // -----------------------------------------------------------------------

    pub fn get_capset_info(&self, index: u32) -> VirtioGpuResult {
        let (capset_id, version, size) = self.rutabaga.get_capset_info(index)?;
        Ok(OkCapsetInfo {
            capset_id,
            version,
            size,
        })
    }

    pub fn get_capset(&self, capset_id: u32, version: u32) -> VirtioGpuResult {
        let capset = self.rutabaga.get_capset(capset_id, version)?;
        if capset_id == 6 {
            log::info!(
                "NESBOX_GPU: DRM capset {} bytes, first 24: {:02x?}",
                capset.len(),
                &capset[..capset.len().min(24)]
            );
            if capset.len() >= 20 {
                let ct = u32::from_le_bytes([capset[16], capset[17], capset[18], capset[19]]);
                log::info!(
                    "NESBOX_GPU: DRM capset context_type={} (1=msm, 2=amdgpu, 3=i915)",
                    ct
                );
            }
        }
        Ok(OkCapset(capset))
    }

    // -----------------------------------------------------------------------
    // Context management
    // -----------------------------------------------------------------------

    pub fn create_context(
        &mut self,
        ctx_id: u32,
        context_init: u32,
        context_name: Option<&str>,
    ) -> VirtioGpuResult {
        log::info!(
            "NESBOX_GPU: create_context ctx_id={} context_init={:#x} ({:#b}) name={:?}",
            ctx_id,
            context_init,
            context_init,
            context_name
        );
        match self
            .rutabaga
            .create_context(ctx_id, context_init, context_name)
        {
            Ok(_) => {
                log::info!("NESBOX_GPU: create_context succeeded");
                Ok(GpuResponse::OkNoData)
            }
            Err(e) => {
                log::error!("NESBOX_GPU: create_context FAILED: {:?}", e);
                Err(GpuResponse::ErrUnspec)
            }
        }
    }

    pub fn destroy_context(&mut self, ctx_id: u32) -> VirtioGpuResult {
        self.rutabaga.destroy_context(ctx_id)?;
        Ok(OkNoData)
    }

    pub fn context_attach_resource(&mut self, ctx_id: u32, resource_id: u32) -> VirtioGpuResult {
        self.rutabaga.context_attach_resource(ctx_id, resource_id)?;
        Ok(OkNoData)
    }

    pub fn context_detach_resource(&mut self, ctx_id: u32, resource_id: u32) -> VirtioGpuResult {
        self.rutabaga.context_detach_resource(ctx_id, resource_id)?;
        Ok(OkNoData)
    }

    pub fn submit_command(
        &mut self,
        ctx_id: u32,
        commands: &mut [u8],
        fence_ids: &[u64],
    ) -> VirtioGpuResult {
        self.rutabaga.submit_command(ctx_id, commands, fence_ids).map_err(|e| {
            let preview: Vec<String> = commands.iter().take(32)
                .map(|b| format!("{:02x}", b)).collect();
            log::error!("NESBOX_GPU: submit_command FAILED ctx={} cmd_len={} fences={}: {:?} first_bytes=[{}]",
                ctx_id, commands.len(), fence_ids.len(), e, preview.join(" "));
            ErrUnspec
        })?;
        Ok(OkNoData)
    }

    pub fn create_fence(&mut self, fence: RutabagaFence) -> VirtioGpuResult {
        self.rutabaga.create_fence(fence)?;
        Ok(OkNoData)
    }

    /// Register a pending descriptor that waits for `fence_id` to complete.
    /// Returns `true` if the fence has already completed (caller should
    /// immediately add the descriptor to the used ring).
    pub fn process_fence(
        &mut self,
        ring: VirtioGpuRing,
        fence_id: u64,
        desc_index: u16,
        len: u32,
    ) -> bool {
        let mut fs = self.fence_state.lock().unwrap();
        let already_done = fence_id <= *fs.completed_fences.get(&ring).unwrap_or(&0);

        if !already_done {
            fs.descs.push(FenceDescriptor {
                ring,
                fence_id,
                desc_index,
                len,
            });
        }
        already_done
    }

    // -----------------------------------------------------------------------
    // Blob resource management
    // -----------------------------------------------------------------------

    pub fn resource_create_blob(
        &mut self,
        ctx_id: u32,
        resource_id: u32,
        resource_create_blob: ResourceCreateBlob,
        vecs: Vec<(GuestAddress, usize)>,
        mem: &GuestMemoryMmap,
    ) -> VirtioGpuResult {
        if resource_create_blob.blob_flags & VIRTIO_GPU_BLOB_FLAG_CREATE_GUEST_HANDLE != 0 {
            log::error!("virtio-gpu: GUEST_HANDLE blob creation is not implemented");
            return Err(ErrUnspec);
        }

        let rutabaga_iovecs = if resource_create_blob.blob_mem != VIRTIO_GPU_BLOB_MEM_HOST3D {
            Some(sglist_to_rutabaga_iovecs(&vecs, mem).map_err(|_| ErrUnspec)?)
        } else {
            None
        };

        self.rutabaga.resource_create_blob(
            ctx_id,
            resource_id,
            resource_create_blob,
            rutabaga_iovecs,
            None,
        )?;

        let resource = VirtioGpuResource::new(resource_id, 0, 0, None, resource_create_blob.size);
        self.resources.insert(resource_id, resource);
        Ok(self.result_from_query(resource_id))
    }

    // -----------------------------------------------------------------------
    // Blob resource host mapping (Linux, non-virgl_renderer_resource_map_fixed path)
    // -----------------------------------------------------------------------
    //
    // This maps a blob resource into the host-visible SHM window so the guest
    // can access it via the VIRTIO_GPU_SHM_ID_HOST_VISIBLE region.
    //
    // NOTE: Firecracker does not currently expose a VirtioShmRegion to devices.
    // Wiring this up requires changes to the MMIO transport and VMM plumbing.

    #[cfg(target_os = "linux")]
    pub fn resource_map_blob(
        &mut self,
        resource_id: u32,
        shm_region: &VirtioShmRegion,
        offset: u64,
    ) -> VirtioGpuResult {
        log::info!(
            "NESBOX_GPU: resource_map_blob res={} offset={} shm_addr=0x{:x} shm_size=0x{:x}",
            resource_id,
            offset,
            shm_region.host_addr,
            shm_region.size
        );

        let res_size = self
            .resources
            .get(&resource_id)
            .ok_or_else(|| {
                log::error!("NESBOX_GPU: map_blob: resource {} not found", resource_id);
                ErrInvalidResourceId
            })?
            .size;

        let map_info = self.rutabaga.map_info(resource_id).map_err(|e| {
            log::error!("NESBOX_GPU: map_blob: map_info failed: {:?}", e);
            ErrUnspec
        })?;
        log::info!(
            "NESBOX_GPU: map_blob: size={} map_info=0x{:x}",
            res_size,
            map_info
        );

        if offset + res_size > shm_region.size as u64 {
            log::error!(
                "NESBOX_GPU: map_blob: overflow offset={} + size={} > shm={}",
                offset,
                res_size,
                shm_region.size
            );
            return Err(ErrUnspec);
        }

        let prot = match map_info & RUTABAGA_MAP_ACCESS_MASK {
            RUTABAGA_MAP_ACCESS_READ => libc::PROT_READ,
            RUTABAGA_MAP_ACCESS_WRITE => libc::PROT_WRITE,
            RUTABAGA_MAP_ACCESS_RW => libc::PROT_READ | libc::PROT_WRITE,
            _ => {
                log::error!(
                    "NESBOX_GPU: map_blob: unexpected access mode 0x{:x}",
                    map_info
                );
                return Err(ErrUnspec);
            }
        };

        let addr = shm_region.host_addr + offset;

        match self.rutabaga.resource_map(
            resource_id,
            addr,
            res_size,
            prot,
            libc::MAP_SHARED | libc::MAP_FIXED,
        ) {
            Ok(()) => {
                log::info!("NESBOX_GPU: map_blob: resource_map SUCCESS at 0x{:x}", addr);
            }
            Err(e) => {
                log::warn!(
                    "NESBOX_GPU: map_blob: resource_map failed ({:?}), trying export_blob fallback",
                    e
                );
                self.map_blob_via_export(resource_id, addr, res_size, prot)?;
            }
        }

        // Re-borrow after all &mut self calls are done
        let resource = self
            .resources
            .get_mut(&resource_id)
            .ok_or(ErrInvalidResourceId)?;
        resource.shmem_offset = Some(offset);
        Ok(OkMapInfo {
            map_info: map_info & RUTABAGA_MAP_CACHE_MASK,
        })
    }

    #[cfg(target_os = "linux")]
    fn map_blob_via_export(
        &mut self,
        resource_id: u32,
        addr: u64,
        size: u64,
        prot: i32,
    ) -> VirtioGpuResult {
        use rutabaga_gfx::RUTABAGA_MEM_HANDLE_TYPE_OPAQUE_FD;
        use std::os::fd::AsRawFd;

        let export = self.rutabaga.export_blob(resource_id).map_err(|e| {
            log::error!("NESBOX_GPU: map_blob: export_blob also failed: {:?}", e);
            ErrUnspec
        })?;

        if export.handle_type == RUTABAGA_MEM_HANDLE_TYPE_OPAQUE_FD {
            log::error!("NESBOX_GPU: map_blob: opaque fd not mappable");
            return Err(ErrUnspec);
        }

        let ret = unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                size as usize,
                prot,
                libc::MAP_SHARED | libc::MAP_FIXED,
                export.os_handle.as_raw_fd(),
                0,
            )
        };
        if ret == libc::MAP_FAILED {
            let errno = std::io::Error::last_os_error();
            log::error!("NESBOX_GPU: map_blob: fallback mmap failed: {}", errno);
            return Err(ErrUnspec);
        }

        log::info!(
            "NESBOX_GPU: map_blob: export fallback SUCCESS at 0x{:x}",
            addr
        );
        Ok(OkNoData)
    }

    #[cfg(target_os = "linux")]
    pub fn resource_unmap_blob(
        &mut self,
        resource_id: u32,
        shm_region: &VirtioShmRegion,
    ) -> VirtioGpuResult {
        let resource = self
            .resources
            .get_mut(&resource_id)
            .ok_or(ErrInvalidResourceId)?;

        let shmem_offset = resource.shmem_offset.ok_or(ErrUnspec)?;
        let addr = shm_region.host_addr + shmem_offset;

        // SAFETY: Replacing the mapping with PROT_NONE / MAP_ANONYMOUS.
        let ret = unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                resource.size as usize,
                libc::PROT_NONE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if ret == libc::MAP_FAILED {
            panic!("virtio-gpu: resource_unmap_blob mmap failed");
        }

        resource.shmem_offset = None;
        Ok(OkNoData)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_associated_scanouts() {
        let mut s = AssociatedScanouts::default();
        assert!(!s.has_any_enabled());
        assert_eq!(s.iter_enabled().next(), None);

        s.enable(1);
        assert!(s.has_any_enabled());
        s.disable(1);
        assert!(!s.has_any_enabled());

        for i in 0..VIRTIO_GPU_MAX_SCANOUTS {
            s.enable(i);
        }
        assert!(s.has_any_enabled());
        assert_eq!(
            s.iter_enabled().collect::<Vec<_>>(),
            (0..VIRTIO_GPU_MAX_SCANOUTS).collect::<Vec<_>>()
        );

        for i in (0..VIRTIO_GPU_MAX_SCANOUTS).filter(|x| x % 2 == 0) {
            s.disable(i);
        }
        assert_eq!(
            s.iter_enabled().collect::<Vec<_>>(),
            (1..VIRTIO_GPU_MAX_SCANOUTS).step_by(2).collect::<Vec<_>>()
        );
    }
}
