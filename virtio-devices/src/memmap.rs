//! Where the VMM backs guest physical addresses with host memory.
//!
//! This lives outside `gpu` because both GPU devices need it and only one of
//! them is always compiled in: `virtio-nvgpu` places its forwarded mappings in
//! the same shared window, and it must keep working in a build with no
//! virglrenderer at all. The trait never mentions rutabaga; it only happens to
//! have been written for it first.

/// Registers host memory as guest RAM, so the guest reaches it directly
/// instead of trapping to us on every access.
///
/// Resources are placed in the shared window -- blob resources by rutabaga on
/// the virtio-gpu path, forwarded device memory by the backend on the
/// virtio-nvgpu one -- and then read and written by the guest at full speed;
/// going through an MMIO exit per access would defeat the entire point.
/// Implemented by the VMM, which is the only part that holds the KVM handle.
pub trait HostMemoryMapper: Send + Sync {
    /// The host address inside the shared window that already backs
    /// `guest_addr`, or `None` if there is no window or the range is outside it.
    ///
    /// **The preferred path, and the reason this trait has four methods.** The
    /// window is one KVM memory slot registered at boot, so a resource placed
    /// inside it with `MAP_FIXED` needs no memslot update and costs an `mmap`.
    /// The alternative below costs a `KVM_SET_USER_MEMORY_REGION` on a running
    /// VM, which stalls every vCPU -- measured at 732 µs to map and 2.13 ms to
    /// unmap.
    fn host_addr(&self, guest_addr: u64, size: u64) -> Option<u64>;

    /// Return a placed range to unbacked, by overwriting it rather than
    /// unmapping it. See the implementation for why the difference matters.
    fn withdraw(&self, guest_addr: u64, size: u64) -> anyhow::Result<()>;

    /// Back `size` bytes at `guest_addr` with the host mapping at `host_addr`,
    /// as its own memory slot.
    ///
    /// The fallback, for a resource the renderer will not place itself.
    /// Correct and slow; see [`HostMemoryMapper::host_addr`].
    fn map(&self, guest_addr: u64, host_addr: u64, size: u64) -> anyhow::Result<()>;

    /// Stop backing `size` bytes at `guest_addr`, removing its slot.
    fn unmap(&self, guest_addr: u64, size: u64) -> anyhow::Result<()>;
}
