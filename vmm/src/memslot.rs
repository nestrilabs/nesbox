//! Turning host memory into guest memory after the VM has started.
//!
//! Guest RAM is registered once at boot, but the GPU's shared window is not
//! RAM: it is a region rutabaga maps blob resources into, and the guest has to
//! reach it at full speed. Registering it as a KVM memory slot is what makes
//! that possible — without it every access would trap out to us, which for a
//! framebuffer is no use at all.

use anyhow::{Context, Result};
use kvm_ioctls::VmFd;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// Hands out KVM memory slots, starting after the ones guest RAM took.
pub struct MemorySlots {
    vm_fd: Arc<VmFd>,
    next_slot: AtomicU32,
    /// Slot number for each guest address we have mapped, so it can be undone.
    mapped: Mutex<HashMap<u64, u32>>,
}

impl MemorySlots {
    /// `first_free_slot` must be past every slot guest RAM occupies.
    pub fn new(vm_fd: Arc<VmFd>, first_free_slot: u32) -> Arc<Self> {
        Arc::new(Self {
            vm_fd,
            next_slot: AtomicU32::new(first_free_slot),
            mapped: Mutex::new(HashMap::new()),
        })
    }

    fn set_region(&self, slot: u32, guest_addr: u64, host_addr: u64, size: u64) -> Result<()> {
        // SAFETY: the caller owns `host_addr..host_addr + size` and keeps it
        // alive for as long as the region is registered. A size of zero
        // removes the slot, which is how unmapping works.
        unsafe {
            self.vm_fd
                .set_user_memory_region(kvm_bindings::kvm_userspace_memory_region {
                    slot,
                    guest_phys_addr: guest_addr,
                    memory_size: size,
                    userspace_addr: host_addr,
                    flags: 0,
                })
                .context("KVM_SET_USER_MEMORY_REGION")
        }
    }
}

impl virtio_devices::gpu::HostMemoryMapper for MemorySlots {
    fn map(&self, guest_addr: u64, host_addr: u64, size: u64) -> Result<()> {
        let mut mapped = self.mapped.lock().unwrap();
        anyhow::ensure!(
            !mapped.contains_key(&guest_addr),
            "guest address {guest_addr:#x} is already mapped"
        );
        let slot = self.next_slot.fetch_add(1, Ordering::SeqCst);
        self.set_region(slot, guest_addr, host_addr, size)?;
        mapped.insert(guest_addr, slot);
        log::debug!(
            "memory slot {slot}: guest {guest_addr:#x} <- host {host_addr:#x}, {size:#x} bytes"
        );
        Ok(())
    }

    fn unmap(&self, guest_addr: u64, _size: u64) -> Result<()> {
        let slot = self
            .mapped
            .lock()
            .unwrap()
            .remove(&guest_addr)
            .with_context(|| format!("guest address {guest_addr:#x} was not mapped"))?;
        // A zero-sized region deletes the slot. The host mapping itself stays
        // ours to free.
        self.set_region(slot, guest_addr, 0, 0)
    }
}
