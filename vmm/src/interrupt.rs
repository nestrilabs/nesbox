//! Simple interrupt manager for MSI-X / irqfd.
//!
//! Allocates GSIs from a bump counter and creates EventFd + irqfd pairs.

use anyhow::{Context, Result};
use kvm_ioctls::VmFd;
use std::sync::atomic::{AtomicU32, Ordering};
use vmm_sys_util::eventfd::EventFd;

static NEXT_GSI: AtomicU32 = AtomicU32::new(10);

pub fn allocate_gsi() -> u32 {
    NEXT_GSI.fetch_add(1, Ordering::Relaxed)
}

pub struct MsixVector {
    pub gsi: u32,
    pub irq_fd: EventFd,
}

impl MsixVector {
    pub fn new(vm_fd: &VmFd) -> Result<Self> {
        let gsi = allocate_gsi();
        let irq_fd = EventFd::new(0).context("failed to create MSI-X eventfd")?;
        vm_fd
            .register_irqfd(&irq_fd, gsi)
            .context("failed to register irqfd")?;
        Ok(Self { gsi, irq_fd })
    }
}

/// Fire an MSI-X interrupt on the given vector.
pub fn trigger_msix(fd: &EventFd) {
    let _ = fd.write(1);
}
