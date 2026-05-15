use crate::boot;
use anyhow::{Context, Result};
use kvm_bindings::KVM_MAX_CPUID_ENTRIES;
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use std::sync::Arc;
use vm_memory::{Address, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

pub struct Vm {
    pub kvm: Kvm,
    pub vm_fd: VmFd,
    pub mem: Arc<GuestMemoryMmap>,
    pub vcpus: Vec<VcpuFd>,
}

impl Vm {
    pub fn new(
        mem_size_mib: usize,
        vcpu_count: u8,
        kernel_path: &std::path::Path,
        cmdline_str: &str,
    ) -> Result<Self> {
        let kvm = Kvm::new().context("Failed to open KVM")?;
        let vm_fd = kvm.create_vm().context("Failed to create VM")?;

        // Create IRQ chip
        vm_fd.create_irq_chip().context("Failed to create IRQ chip")?;

        // Memory
        let mem_size = mem_size_mib * 1024 * 1024;
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_size)])
            .context("Failed to create guest memory")?;
        let mem = Arc::new(mem);

        let host_addr = mem
            .get_host_address(GuestAddress(0))
            .context("Failed to get host address")?;

        unsafe {
            vm_fd
                .set_user_memory_region(kvm_bindings::kvm_userspace_memory_region {
                    slot: 0,
                    guest_phys_addr: 0,
                    memory_size: (mem_size_mib * 1024 * 1024) as u64,
                    userspace_addr: host_addr as u64,
                    flags: 0,
                })
                .context("Failed to set user memory region")?;
        }

        // Load kernel
        let loader_result = boot::load_kernel(&mem, &kernel_path)?;
        let entry_point = loader_result.kernel_load;

        // Build ACPI tables at the top of RAM and get RSDP address
        let acpi_size: u64 = 0x1_0000; // 64 KiB
        let acpi_start = GuestAddress(mem_size as u64 - acpi_size);
        let rsdp_addr = crate::acpi::setup_acpi(&mem, vcpu_count, acpi_start)?;

        boot::setup_boot_params(
            &mem,
            entry_point,
            cmdline_str,
            mem_size_mib,
            Some(rsdp_addr),
        )?;

        // Create vCPUs and configure registers
        let mut vcpus = Vec::with_capacity(vcpu_count as usize);
        for cpu_id in 0..vcpu_count {
            let vcpu_fd = vm_fd
                .create_vcpu(cpu_id.into())
                .with_context(|| format!("Failed to create vCPU {}", cpu_id))?;

            // Set CPUID (use KVM supported cpuid)
            let cpuid = kvm
                .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
                .context("Failed to get supported CPUID")?;
            vcpu_fd.set_cpuid2(&cpuid).context("Failed to set CPUID")?;

            // Configure registers
            crate::regs::setup_fpu(&vcpu_fd)?;
            crate::regs::setup_regs(&vcpu_fd, entry_point.raw_value())?;
            crate::regs::setup_sregs(&mem, &vcpu_fd)?;

            vcpus.push(vcpu_fd);
        }

        Ok(Self {
            kvm,
            vm_fd,
            mem,
            vcpus,
        })
    }
}

pub fn run_vcpu_loop(
    _mem: Arc<GuestMemoryMmap>,
    mut vcpu_fd: VcpuFd,
    pci_bus: Arc<pci::Bus>,
) -> Result<()> {
    loop {
        match vcpu_fd.run() {
            Ok(vcpu_exit) => match vcpu_exit {
                VcpuExit::IoOut(port, data) => {
                    if !pci_bus.handle_pio_write(port, data) {
                        //log::trace!("Unhandled PIO out: port={:#x}, len={}", port, data.len());
                    }
                }
                VcpuExit::IoIn(port, data) => {
                    if !pci_bus.handle_pio_read(port, data) {
                        data.fill(0xff);
                        //log::trace!("Unhandled PIO in: port={:#x}", port);
                    }
                }
                VcpuExit::MmioRead(addr, data) => {
                    if !pci_bus.handle_mmio_read(addr, data) {
                        data.fill(0xff);
                        //log::trace!("Unhandled MMIO read: addr={:#x}", addr);
                    }
                }
                VcpuExit::MmioWrite(addr, data) => {
                    if !pci_bus.handle_mmio_write(addr, data) {
                        //log::trace!("Unhandled MMIO write: addr={:#x}, len={}", addr, data.len());
                    }
                }
                VcpuExit::Hlt => {
                    log::debug!("vCPU halted");
                    break;
                }
                VcpuExit::Shutdown => {
                    log::debug!("vCPU shutdown");
                    break;
                }
                VcpuExit::Exception => {
                    log::error!("vCPU triple fault or other exception!");
                    // break to see where it happens
                    break;
                }
                other => {
                    log::debug!("Unhandled vCPU exit: {:?}", other);
                }
            },
            Err(e) => {
                log::error!("vCPU run error: {}", e);
                break;
            }
        }
    }
    Ok(())
}
