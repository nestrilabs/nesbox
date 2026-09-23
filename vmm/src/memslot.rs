//! Turning host memory into guest memory after the VM has started.
//!
//! Guest RAM is registered once at boot, but the GPU's shared window is not
//! RAM: it is a region rutabaga maps blob resources into, and the guest has to
//! reach it at full speed. Registering it as a KVM memory slot is what makes
//! that possible — without it every access would trap out to us, which for a
//! framebuffer is no use at all.
//!
//! # One slot for the window, not one per resource
//!
//! There were two ways to do that and this file used the expensive one. A slot
//! per blob means a `KVM_SET_USER_MEMORY_REGION` every time the guest maps or
//! unmaps one, and a memslot update on a *running* VM is not a bookkeeping
//! call: deleting one zaps the shadow page tables for that range and
//! synchronises RCU against every vCPU, so all of them wait.
//!
//! Measured on this host with one game running and the player standing still:
//! **732 µs** for a map, **2.13 ms** for an unmap, 52 and 48 times a second —
//! 14% of the GPU worker's wall clock, and a global vCPU stall each time. A
//! frame that caught one ran at 130 fps where its neighbours ran at 180, which
//! is exactly the jitter that was visible on screen.
//!
//! So the window is reserved once as host address space, registered once as a
//! single KVM slot, and blob resources are placed *inside* it with
//! `MAP_FIXED`. Mapping becomes an `mmap` — single-digit microseconds — and
//! after boot there are no memslot updates at all.
//!
//! # The per-resource path is still here
//!
//! `virgl_renderer_resource_map_fixed` is documented to return `-EOPNOTSUPP`
//! for resources it cannot place, and a resource that cannot be placed still
//! has to reach the guest somehow. So the old path stays as the fallback, and
//! [`MemorySlots`] carries both: a resource is placed where it can be, and
//! given its own slot where it cannot.

use anyhow::{Context, Result};
use kvm_ioctls::VmFd;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// The GPU's shared window, reserved and registered once.
struct Window {
    /// Guest physical base — where the bus put BAR2.
    guest_base: u64,
    /// Host base of the `PROT_NONE` reservation the window lives in.
    host_base: u64,
    size: u64,
}

/// Hands out KVM memory slots, starting after the ones guest RAM took.
pub struct MemorySlots {
    vm_fd: Arc<VmFd>,
    next_slot: AtomicU32,
    /// The window, once [`MemorySlots::open_window`] has made it. `None` before
    /// the bus has assigned BAR2, and on a host where the reservation failed —
    /// in which case every resource takes the per-resource path.
    window: Mutex<Option<Window>>,
    /// Slot number for each guest address we have mapped, so it can be undone.
    mapped: Mutex<HashMap<u64, u32>>,
    /// Slot numbers freed by an unmap, waiting to be used again.
    ///
    /// KVM allows a few hundred slots per VM and numbers them, so handing out a
    /// fresh number for every mapping runs out. The GPU maps and unmaps a blob
    /// per resource — Vulkan initialisation alone does this dozens of times —
    /// so without recycling a long session would stop being able to map
    /// anything at all.
    free: Mutex<Vec<u32>>,
}

impl MemorySlots {
    /// `first_free_slot` must be past every slot guest RAM occupies.
    pub fn new(vm_fd: Arc<VmFd>, first_free_slot: u32) -> Arc<Self> {
        Arc::new(Self {
            vm_fd,
            next_slot: AtomicU32::new(first_free_slot),
            window: Mutex::new(None),
            mapped: Mutex::new(HashMap::new()),
            free: Mutex::new(Vec::new()),
        })
    }

    /// Reserve the window's host address space and register it as one KVM slot.
    ///
    /// Called once, after the bus has placed BAR2 and before the guest driver
    /// can reach it. `size` is address space and not memory: the reservation is
    /// `PROT_NONE` and `MAP_NORESERVE`, so an 8 GiB window costs eight gigabytes
    /// of a 64-bit address space and no pages at all until something is placed
    /// in it.
    ///
    /// A failure is reported and not fatal. Every resource then takes the
    /// per-resource path, which is what this VMM did before the window existed.
    pub fn open_window(&self, guest_base: u64, size: u64) -> Result<()> {
        // One window per allocator. A second would replace the first's record
        // while its slot stayed registered, so the first device's mappings
        // would be placed against an address range nothing describes any more.
        anyhow::ensure!(
            self.window.lock().unwrap().is_none(),
            "a window is already open; this allocator holds one"
        );

        // SAFETY: a fresh anonymous reservation at an address the kernel picks.
        // Nothing else can hold it, and `PROT_NONE` means nothing can read or
        // write it until a later `MAP_FIXED` gives part of it contents.
        let host_base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if host_base == libc::MAP_FAILED {
            anyhow::bail!(
                "could not reserve {size:#x} bytes for the GPU window: {}",
                std::io::Error::last_os_error()
            );
        }
        let host_base = host_base as u64;

        let slot = self.next_slot.fetch_add(1, Ordering::SeqCst);
        if let Err(err) = self.set_region(slot, guest_base, host_base, size) {
            // SAFETY: undoing the mapping made immediately above, same address
            // and length, and nothing has been handed it.
            unsafe { libc::munmap(host_base as *mut libc::c_void, size as usize) };
            self.free.lock().unwrap().push(slot);
            return Err(err).context("registering the GPU window as a memory slot");
        }

        *self.window.lock().unwrap() = Some(Window {
            guest_base,
            host_base,
            size,
        });
        log::info!(
            "GPU window: slot {slot}, guest {guest_base:#x} <- host {host_base:#x}, \
             {size:#x} bytes reserved"
        );
        Ok(())
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

impl virtio_devices::HostMemoryMapper for MemorySlots {
    fn host_addr(&self, guest_addr: u64, size: u64) -> Option<u64> {
        let window = self.window.lock().unwrap();
        let w = window.as_ref()?;
        let offset = guest_addr.checked_sub(w.guest_base)?;
        // Both ends inside the window, and no wrap. A resource that would run
        // off the end is refused here rather than placed over whatever follows.
        if offset.checked_add(size)? > w.size {
            return None;
        }
        Some(w.host_base + offset)
    }

    fn withdraw(&self, guest_addr: u64, size: u64) -> Result<()> {
        let host = self
            .host_addr(guest_addr, size)
            .with_context(|| format!("{guest_addr:#x} is not inside the GPU window"))?;
        // **Overwritten, never unmapped.** A hole inside the reservation would
        // leave the KVM slot pointing at nothing, and a guest touching it would
        // fault somewhere far from here. `PROT_NONE` anonymous memory keeps the
        // range mapped and unreadable, which is the state the window was in
        // before anything was placed there. The virglrenderer API documents
        // this as the caller's job; see its header for
        // `virgl_renderer_resource_map_fixed`.
        //
        // SAFETY: the range lies inside the reservation this struct owns, as
        // `host_addr` has just checked, and replacing part of a mapping with
        // `MAP_FIXED` is defined.
        let ret = unsafe {
            libc::mmap(
                host as *mut libc::c_void,
                size as usize,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if ret == libc::MAP_FAILED {
            anyhow::bail!(
                "could not withdraw {size:#x} bytes at {host:#x}: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    fn map(&self, guest_addr: u64, host_addr: u64, size: u64) -> Result<()> {
        let mut mapped = self.mapped.lock().unwrap();
        anyhow::ensure!(
            !mapped.contains_key(&guest_addr),
            "guest address {guest_addr:#x} is already mapped"
        );
        let slot = match self.free.lock().unwrap().pop() {
            Some(recycled) => recycled,
            None => self.next_slot.fetch_add(1, Ordering::SeqCst),
        };
        self.set_region(slot, guest_addr, host_addr, size)
            .map_err(|err| {
                // Give the number back; nothing was registered under it.
                self.free.lock().unwrap().push(slot);
                err
            })?;
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
        self.set_region(slot, guest_addr, 0, 0)?;
        self.free.lock().unwrap().push(slot);
        Ok(())
    }
}
