//! Interrupt routing.
//!
//! KVM's routing table maps a GSI to a destination. GSIs below
//! [`IOAPIC_PINS`] belong to the legacy chips and keep the default routing so
//! INTx and the timer keep working; GSIs from [`FIRST_MSI_GSI`] up are handed
//! to devices as MSI-X vectors, and are pointed at whatever address/data pair
//! the guest programs into its MSI-X table.
//!
//! KVM_SET_GSI_ROUTING replaces the table wholesale, so every change rebuilds
//! it from scratch: the fixed legacy entries plus one MSI entry per programmed
//! vector.

use anyhow::{Context, Result};
use kvm_bindings::{
    KVM_IRQ_ROUTING_IRQCHIP, KVM_IRQ_ROUTING_MSI, KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER,
    KVM_IRQCHIP_PIC_SLAVE, KvmIrqRouting, kvm_irq_routing_entry,
};
use kvm_ioctls::VmFd;
use pci::{MsiRouter, MsiVector};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use vmm_sys_util::eventfd::EventFd;

/// Number of IOAPIC pins; GSIs below this are legacy.
pub const IOAPIC_PINS: u32 = 24;
/// First GSI available for MSI.
pub const FIRST_MSI_GSI: u32 = IOAPIC_PINS;

#[derive(Clone, Copy)]
struct MsiRoute {
    addr: u64,
    data: u32,
}

pub struct IrqManager {
    vm_fd: Arc<VmFd>,
    state: Mutex<State>,
}

struct State {
    next_gsi: u32,
    /// Programmed MSI routes, keyed by GSI. Ordered so the committed table is
    /// deterministic.
    msi: BTreeMap<u32, MsiRoute>,
}

impl IrqManager {
    /// Create the manager and install the legacy-only routing table.
    pub fn new(vm_fd: Arc<VmFd>) -> Result<Arc<Self>> {
        let mgr = Arc::new(Self {
            vm_fd,
            state: Mutex::new(State {
                next_gsi: FIRST_MSI_GSI,
                msi: BTreeMap::new(),
            }),
        });
        {
            let state = mgr.state.lock().unwrap();
            mgr.commit(&state)
                .context("failed to install the default GSI routing table")?;
        }
        Ok(mgr)
    }

    /// Allocate an MSI vector: a fresh GSI with an eventfd wired to it.
    ///
    /// The GSI has no route until the guest programs the MSI-X table entry,
    /// so writing to the eventfd before then delivers nothing — which is
    /// exactly the behaviour the spec asks for.
    pub fn allocate_msi_vector(&self) -> Result<MsiVector> {
        let gsi = {
            let mut state = self.state.lock().unwrap();
            let gsi = state.next_gsi;
            state.next_gsi += 1;
            gsi
        };
        let irq_fd = EventFd::new(0).context("failed to create MSI-X eventfd")?;
        self.vm_fd
            .register_irqfd(&irq_fd, gsi)
            .with_context(|| format!("failed to register irqfd for gsi {gsi}"))?;
        Ok(MsiVector {
            gsi,
            irq_fd: Arc::new(irq_fd),
        })
    }

    /// Allocate `count` MSI vectors.
    pub fn allocate_msi_vectors(&self, count: usize) -> Result<Vec<MsiVector>> {
        (0..count).map(|_| self.allocate_msi_vector()).collect()
    }

    /// An eventfd wired to a legacy IOAPIC pin, for a device's INTx line.
    pub fn legacy_irqfd(&self, gsi: u32) -> Result<Arc<EventFd>> {
        anyhow::ensure!(gsi < IOAPIC_PINS, "gsi {gsi} is not a legacy interrupt");
        let irq_fd = EventFd::new(0).context("failed to create INTx eventfd")?;
        self.vm_fd
            .register_irqfd(&irq_fd, gsi)
            .with_context(|| format!("failed to register INTx irqfd for gsi {gsi}"))?;
        Ok(Arc::new(irq_fd))
    }

    /// Rebuild and install the full routing table.
    fn commit(&self, state: &State) -> Result<()> {
        let mut entries = legacy_entries();
        for (&gsi, route) in &state.msi {
            let mut entry = kvm_irq_routing_entry {
                gsi,
                type_: KVM_IRQ_ROUTING_MSI,
                ..Default::default()
            };
            entry.u.msi.address_lo = route.addr as u32;
            entry.u.msi.address_hi = (route.addr >> 32) as u32;
            entry.u.msi.data = route.data;
            entries.push(entry);
        }

        let routing = KvmIrqRouting::from_entries(&entries)
            .map_err(|e| anyhow::anyhow!("failed to build the routing table: {e:?}"))?;
        self.vm_fd
            .set_gsi_routing(&routing)
            .context("KVM_SET_GSI_ROUTING failed")?;
        Ok(())
    }
}

impl MsiRouter for IrqManager {
    fn set_msi_route(&self, gsi: u32, addr: u64, data: u32) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        // Committing an unchanged route would be a pointless ioctl; the guest
        // rewrites table entries on every unmask.
        if let Some(old) = state.msi.get(&gsi) {
            if old.addr == addr && old.data == data {
                return Ok(());
            }
        }
        state.msi.insert(gsi, MsiRoute { addr, data });
        log::debug!("routing gsi {gsi} to MSI addr={addr:#x} data={data:#x}");
        self.commit(&state)
    }
}

/// The default routing for the legacy chips, matching what KVM installs
/// itself: GSIs 0–15 drive both the IOAPIC and the corresponding PIC pin,
/// GSIs 16–23 the IOAPIC alone.
fn legacy_entries() -> Vec<kvm_irq_routing_entry> {
    let mut entries = Vec::with_capacity(IOAPIC_PINS as usize + 16);
    for gsi in 0..IOAPIC_PINS {
        let mut ioapic = kvm_irq_routing_entry {
            gsi,
            type_: KVM_IRQ_ROUTING_IRQCHIP,
            ..Default::default()
        };
        ioapic.u.irqchip.irqchip = KVM_IRQCHIP_IOAPIC;
        ioapic.u.irqchip.pin = gsi;
        entries.push(ioapic);

        if gsi < 16 {
            let mut pic = kvm_irq_routing_entry {
                gsi,
                type_: KVM_IRQ_ROUTING_IRQCHIP,
                ..Default::default()
            };
            pic.u.irqchip.irqchip = if gsi < 8 {
                KVM_IRQCHIP_PIC_MASTER
            } else {
                KVM_IRQCHIP_PIC_SLAVE
            };
            pic.u.irqchip.pin = gsi % 8;
            entries.push(pic);
        }
    }
    entries
}
