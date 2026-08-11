//! Virtio GPU over PCI, backed by rutabaga's DRM native context.
//!
//! Ported from the virtio-MMIO implementation on the old `main` branch. The
//! command handling, the rutabaga bridge and the protocol definitions came
//! across nearly unchanged; the transport did not.
//!
//! Unlike every other device here, the GPU cannot be handed to a vhost
//! backend: rutabaga lives in this process, so the queues are serviced by a
//! worker thread of our own.

mod descriptor_utils;
mod device;
pub use self::descriptor_utils::Descriptor;
pub mod display;
mod edid;
mod protocol;
mod virtio_gpu;
mod worker;

pub use self::descriptor_utils::{Error as DescriptorError, Reader, Writer};
pub use self::device::{GpuConfig, GpuDevice};

/// Control virtqueue index.
pub const CTL_INDEX: usize = 0;
/// Cursor virtqueue index.
pub const CUR_INDEX: usize = 1;
/// Number of virtqueues.
pub const NUM_QUEUES: usize = 2;
/// Maximum size of each virtqueue.
pub const QUEUE_SIZE: u16 = 256;

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

/// The host-visible window blob resources are mapped into.
///
/// This is the reason the PCI crate grew 64-bit prefetchable BARs: the region
/// is far too large for the window below 4 GiB, and the guest reaches it
/// through a virtio shared-memory capability pointing at a BAR.
#[derive(Clone, Debug)]
pub struct VirtioShmRegion {
    /// Guest physical address of the beginning of the window.
    pub guest_addr: u64,
    /// Byte length of the window.
    pub size: usize,
}

/// The GPU's whole coupling to the virtio transport: take work off the control
/// queue, and put it back when it is done.
///
/// It is a trait because rutabaga completes fences on its own threads and calls
/// back to retire descriptors, long after the vCPU that submitted them has gone
/// on to something else. The callback cannot borrow the device, so it holds one
/// of these instead.
pub trait GpuQueues: Send + Sync {
    /// Take the next available chain off the control queue, as a head index
    /// and its descriptors. `None` when the queue is empty.
    fn pop_ctl(&self) -> Option<(u16, Vec<Descriptor>)>;
    /// Complete descriptors on the control queue, then interrupt the guest.
    /// Each entry is a descriptor head index and the number of bytes written.
    fn complete_ctl(&self, completed: &[(u16, u32)]);
}

/// Registers host memory as guest RAM, so the guest reaches it directly
/// instead of trapping to us on every access.
///
/// Blob resources are mapped into the shared window by rutabaga and then read
/// and written by the guest at full speed; going through an MMIO exit per
/// access would defeat the entire point. Implemented by the VMM, which is the
/// only part that holds the KVM handle.
pub trait HostMemoryMapper: Send + Sync {
    /// Back `size` bytes at `guest_addr` with the host mapping at `host_addr`.
    fn map(&self, guest_addr: u64, host_addr: u64, size: u64) -> anyhow::Result<()>;
    /// Stop backing `size` bytes at `guest_addr`.
    fn unmap(&self, guest_addr: u64, size: u64) -> anyhow::Result<()>;
}

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
