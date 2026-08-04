//! MSI-X plumbing shared between devices and the VMM.
//!
//! A device owns an MSI-X table; the VMM owns the KVM interrupt routing table.
//! The two meet here: the device is handed one [`MsiVector`] per table entry
//! and an [`MsiRouter`], and whenever the guest programs a table entry the
//! device asks the router to point that vector's GSI at the address/data pair
//! the guest chose. Raising the interrupt is then just a write to the eventfd.

use std::sync::Arc;
use vmm_sys_util::eventfd::EventFd;

/// One interrupt vector: a GSI and the eventfd wired to it via KVM_IRQFD.
#[derive(Clone)]
pub struct MsiVector {
    pub gsi: u32,
    pub irq_fd: Arc<EventFd>,
}

impl MsiVector {
    /// Raise this interrupt.
    pub fn trigger(&self) {
        let _ = self.irq_fd.write(1);
    }
}

/// Programs the host's interrupt routing table.
pub trait MsiRouter: Send + Sync {
    /// Route `gsi` to the MSI described by `addr`/`data`.
    fn set_msi_route(&self, gsi: u32, addr: u64, data: u32) -> anyhow::Result<()>;
}
