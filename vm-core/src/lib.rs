use anyhow::Result;
use event_manager::{EventOps, Events, MutEventSubscriber};
use kvm_ioctls::{VcpuExit, VcpuFd};
use log::{debug, error, info};
use pci::Bus;
use std::sync::Arc;
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::epoll::EventSet;

pub struct VcpuSubscriber {
    vcpu_fd: VcpuFd,
    mem: Arc<GuestMemoryMmap>,
    pci_bus: Arc<Bus>,
}

impl VcpuSubscriber {
    pub fn new(vcpu_fd: VcpuFd, mem: Arc<GuestMemoryMmap>, pci_bus: Arc<Bus>) -> Self {
        Self {
            vcpu_fd,
            mem,
            pci_bus,
        }
    }

    fn handle_vcpu_exit(&mut self) -> Result<bool> {
        match self.vcpu_fd.run() {
            Ok(vcpu_exit) => match vcpu_exit {
                VcpuExit::IoOut(port, data) => {
                    debug!("PIO out: port={:x}, len={}", port, data.len());
                    // PCI config ports: 0xCF8, 0xCFC
                    if (port == 0xCF8 && data.len() == 4) || (port == 0xCFC && data.len() <= 4) {
                        if self.pci_bus.handle_pio_write(port, data) {
                            return Ok(true);
                        }
                    }
                    // If no device handled, ignore
                    Ok(true)
                }
                VcpuExit::IoIn(port, data) => {
                    debug!("PIO in: port={:x}, len={}", port, data.len());
                    // Default to 0xff for unknown ports
                    for b in data.iter_mut() {
                        *b = 0xff;
                    }
                    Ok(true)
                }
                VcpuExit::MmioWrite(addr, data) => {
                    debug!("MMIO write at addr={:#x}, len={}", addr, data.len());
                    if self.pci_bus.handle_mmio_write(addr, data) {
                        return Ok(true);
                    }
                    // Unhandled MMIO – ignore
                    Ok(true)
                }
                VcpuExit::MmioRead(addr, data) => {
                    debug!("MMIO read at addr={:#x}, len={}", addr, data.len());
                    if self.pci_bus.handle_mmio_read(addr, data) {
                        return Ok(true);
                    }
                    // Default to 0xff
                    for b in data.iter_mut() {
                        *b = 0xff;
                    }
                    Ok(true)
                }
                VcpuExit::Hlt => {
                    info!("vCPU HLT");
                    Ok(false) // signal to stop the vCPU loop
                }
                VcpuExit::Shutdown => {
                    info!("vCPU shutdown");
                    Ok(false)
                }
                _ => {
                    debug!("Unhandled vCPU exit: {:?}", vcpu_exit);
                    Ok(true)
                }
            },
            Err(e) => {
                error!("vCPU run error: {}", e);
                Err(e.into())
            }
        }
    }
}

impl MutEventSubscriber for VcpuSubscriber {
    fn process(&mut self, _events: Events, _event_ops: &mut EventOps) {
        loop {
            match self.handle_vcpu_exit() {
                Ok(continue_running) => {
                    if !continue_running {
                        break;
                    }
                }
                Err(e) => {
                    error!("Error handling vCPU exit: {}", e);
                    break;
                }
            }
        }
    }

    fn init(&mut self, ops: &mut EventOps) {
        ops.add(Events::new(&self.vcpu_fd, EventSet::IN))
            .expect("Failed to register vCPU fd");
    }
}
