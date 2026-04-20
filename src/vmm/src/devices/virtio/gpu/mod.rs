// Copyright 2024 - Firecracker GPU port
// Virtio GPU device module.
//
// Ported from libkrun's virtio-gpu implementation.
// Display / scanout functionality is intentionally absent – GPU workloads run
// entirely inside the VMM with no host display backend.

mod descriptor_utils;
mod device;
pub mod display;
mod edid;
mod event_handler;
mod protocol;
mod virtio_gpu;
mod worker;

pub use self::descriptor_utils::{Reader, Writer};
pub use self::device::Gpu;
// FenceState and VirtioGpu are pub in virtio_gpu.rs; re-exported here for
// any future snapshot/restore or test code that needs direct access.
pub use self::virtio_gpu::{FenceState, VirtioGpu};

use self::descriptor_utils::Error as DescriptorError;

// ---------------------------------------------------------------------------
// Queue layout constants
// ---------------------------------------------------------------------------

/// Control virtqueue index.
pub const CTL_INDEX: usize = 0;
/// Cursor virtqueue index.
pub const CUR_INDEX: usize = 1;
/// Number of virtqueues.
pub const NUM_QUEUES: usize = 2;
/// Maximum size of each virtqueue.
pub const QUEUE_SIZE: u16 = 256;
/// Queue sizes array (indexed by queue number).
pub const QUEUE_SIZES: &[u16] = &[QUEUE_SIZE; NUM_QUEUES];

// ---------------------------------------------------------------------------
// Virtio feature bits
// ---------------------------------------------------------------------------

pub mod uapi {
    use vm_memory::ByteValued;

    pub const VIRTIO_F_VERSION_1: u32 = 32;
    pub const VIRTIO_ID_GPU: u32 = 16;

    pub const VIRTIO_GPU_F_VIRGL: u32 = 0;
    pub const VIRTIO_GPU_F_EDID: u32 = 1;
    pub const VIRTIO_GPU_F_RESOURCE_UUID: u32 = 2;
    pub const VIRTIO_GPU_F_RESOURCE_BLOB: u32 = 3;
    pub const VIRTIO_GPU_F_CONTEXT_INIT: u32 = 4;

    /// GPU device configuration space layout.
    #[derive(Copy, Clone, Debug, Default)]
    #[repr(C)]
    pub struct virtio_gpu_config {
        pub events_read: u32,
        pub events_clear: u32,
        pub num_scanouts: u32,
        pub num_capsets: u32,
    }
    // SAFETY: Plain-old-data, no padding, no pointers.
    unsafe impl ByteValued for virtio_gpu_config {}
}

/// Supported feature bits advertised to the driver.
pub const AVAIL_FEATURES: u64 = (1u64 << uapi::VIRTIO_F_VERSION_1)
    | (1u64 << uapi::VIRTIO_GPU_F_VIRGL)
    | (1u64 << uapi::VIRTIO_GPU_F_EDID)
    | (1u64 << uapi::VIRTIO_GPU_F_RESOURCE_UUID)
    | (1u64 << uapi::VIRTIO_GPU_F_RESOURCE_BLOB)
    | (1u64 << uapi::VIRTIO_GPU_F_CONTEXT_INIT);

// ---------------------------------------------------------------------------
// Shared memory region descriptor (needed for blob resource host mapping)
// ---------------------------------------------------------------------------
//
// NOTE: Firecracker does not natively expose a VirtioShmRegion type.  This
// struct must also be added to the Firecracker MMIO transport layer and wired
// up to the device via `Gpu::set_shm_region`.

#[derive(Clone, Debug)]
pub struct VirtioShmRegion {
    /// Host virtual address of the beginning of the SHM window.
    pub host_addr: u64,
    /// Guest physical address of the beginning of the SHM window.
    pub guest_addr: u64,
    /// Byte length of the SHM window.
    pub size: usize,
}

// ---------------------------------------------------------------------------
// GPU device-level errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum GpuError {
    /// Failed to create EventFd: {0}
    EventFd(std::io::Error),
    /// Failed to decode incoming GPU command: {0}
    DecodeCommand(std::io::Error),
    /// Error creating Reader for virtqueue: {0}
    QueueReader(DescriptorError),
    /// Error creating Writer for virtqueue: {0}
    QueueWriter(DescriptorError),
    /// Error writing GPU response to descriptor: {0}
    WriteDescriptor(std::io::Error),
    /// Failed to access guest memory
    GuestMemory,
    /// GPU device activation failed
    ActivateError,
}

pub type Result<T> = std::result::Result<T, GpuError>;
