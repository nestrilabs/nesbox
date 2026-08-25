use crate::lifecycle::{ExitReason, Shutdown};
use crate::power::PowerDevice;
use crate::{boot, layout};
use anyhow::{Context, Result};
use kvm_bindings::KVM_MAX_CPUID_ENTRIES;
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use std::os::fd::FromRawFd;
use std::sync::Arc;
use vm_memory::{Address, FileOffset, GuestMemoryBackend, GuestMemoryMmap};

pub struct Vm {
    pub kvm: Kvm,
    pub vm_fd: Arc<VmFd>,
    pub mem: Arc<GuestMemoryMmap>,
    pub vcpus: Vec<VcpuFd>,
    /// Where 64-bit BARs may be placed, given this CPU's address width.
    pub mmio64: layout::Mmio64Window,
    /// How many KVM memory slots guest RAM took. Anything mapped later —
    /// the GPU's shared window — must start after these.
    pub ram_slot_count: u32,
}

/// How many bits of physical address the guest CPU will have.
///
/// CPUID leaf 0x80000008, EAX bits 7:0. This decides where the 64-bit MMIO
/// window can go: Linux quietly discards a host bridge window it cannot
/// address, so guessing high breaks PCI on exactly the desktop parts most
/// likely to be running this.
fn host_phys_addr_bits(kvm: &Kvm) -> Result<u8> {
    let cpuid = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .context("Failed to get supported CPUID")?;
    let bits = cpuid
        .as_slice()
        .iter()
        .find(|e| e.function == 0x8000_0008)
        .map(|e| (e.eax & 0xff) as u8)
        .filter(|&b| b >= 32)
        // Every x86-64 CPU has at least 36; if the leaf is missing, assume it.
        .unwrap_or(36);
    Ok(bits)
}

impl Vm {
    pub fn new(
        mem_size_mib: usize,
        vcpu_count: u8,
        kernel_path: &std::path::Path,
        cmdline_str: &str,
    ) -> Result<Self> {
        let kvm = Kvm::new().context("Failed to open KVM")?;
        let vm_fd = Arc::new(kvm.create_vm().context("Failed to create VM")?);

        // Create IRQ chip
        vm_fd
            .create_irq_chip()
            .context("Failed to create IRQ chip")?;

        // Memory, split around the 3–4 GiB device hole.
        let mem_size = (mem_size_mib as u64) * 1024 * 1024;
        let regions = layout::ram_regions(mem_size);

        let phys_bits = host_phys_addr_bits(&kvm)?;
        let mmio64 = layout::mmio64_window(phys_bits);
        let ram_top = layout::ram_top(mem_size);
        anyhow::ensure!(
            mmio64.fits_above(ram_top),
            "{mem_size_mib} MiB of RAM reaches {ram_top:#x}, which collides with the \
             64-bit MMIO window at {:#x}. This CPU addresses {phys_bits} bits; \
             give the guest less memory.",
            mmio64.start
        );
        log::info!(
            "CPU addresses {phys_bits} bits; 64-bit MMIO window {:#x}..{:#x}",
            mmio64.start,
            mmio64.end()
        );

        // Guest RAM is backed by a memfd and mapped shared, so vhost-user
        // backends such as virtiofsd can map it into their own address space.
        // Anonymous private memory would leave them unable to see it.
        let mem_file = create_memfd(mem_size).context("Failed to create guest memory file")?;
        let mut file_offset = 0u64;
        let ranges = regions
            .iter()
            .map(|&(start, size)| {
                let offset = file_offset;
                file_offset += size as u64;
                let file = mem_file
                    .try_clone()
                    .context("Failed to clone the guest memory file")?;
                Ok((start, size, Some(FileOffset::new(file, offset))))
            })
            .collect::<Result<Vec<_>>>()?;
        let mem = GuestMemoryMmap::from_ranges_with_files(&ranges)
            .context("Failed to create guest memory")?;
        let mem = Arc::new(mem);

        for (slot, &(start, size)) in regions.iter().enumerate() {
            let host_addr = mem
                .get_host_address(start)
                .context("Failed to get host address")?;
            log::info!(
                "RAM slot {}: guest {:#x}..{:#x} ({} MiB)",
                slot,
                start.raw_value(),
                start.raw_value() + size as u64,
                size / (1024 * 1024)
            );
            unsafe {
                vm_fd
                    .set_user_memory_region(kvm_bindings::kvm_userspace_memory_region {
                        slot: slot as u32,
                        guest_phys_addr: start.raw_value(),
                        memory_size: size as u64,
                        userspace_addr: host_addr as u64,
                        flags: 0,
                    })
                    .context("Failed to set user memory region")?;
            }
        }

        // Load kernel
        let loader_result = boot::load_kernel(&mem, &kernel_path)?;
        let entry_point = loader_result.kernel_load;
        log::info!(
            "kernel: load/entry={:#x} end={:?} setup_header={:?}",
            entry_point.raw_value(),
            loader_result.kernel_end,
            loader_result.setup_header.is_some()
        );

        // Build ACPI tables at the top of low RAM and get the RSDP address.
        let acpi_start = layout::acpi_start(mem_size);
        let rsdp_addr = crate::acpi::setup_acpi(&mem, vcpu_count, acpi_start, mmio64)?;

        boot::setup_boot_params(&mem, entry_point, cmdline_str, &regions, Some(rsdp_addr))?;

        // Create vCPUs and configure registers
        let mut vcpus = Vec::with_capacity(vcpu_count as usize);
        for cpu_id in 0..vcpu_count {
            let vcpu_fd = vm_fd
                .create_vcpu(cpu_id.into())
                .with_context(|| format!("Failed to create vCPU {}", cpu_id))?;

            // KVM's supported CPUID describes the *host* processor, so the
            // topology leaves in it are the host's: a 7-vCPU guest on a
            // 16-core box would read a package of 16 cores and 32 threads and
            // build its scheduling domains from sibling relationships that do
            // not exist. Rewritten per vCPU -- the x2APIC id differs.
            let mut cpuid = kvm
                .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
                .context("Failed to get supported CPUID")?;
            let vendor = crate::cpuid::vendor_of(&cpuid);
            crate::cpuid::patch_topology(&mut cpuid, u32::from(cpu_id), vcpu_count, vendor);
            vcpu_fd.set_cpuid2(&cpuid).context("Failed to set CPUID")?;

            // Only the bootstrap processor starts executing the kernel. The
            // application processors must be left in the reset state KVM gave
            // them, halted until the guest sends INIT/SIPI — putting them in
            // long mode at the kernel entry point would start several CPUs
            // racing through the boot path at once.
            if cpu_id == 0 {
                crate::regs::setup_fpu(&vcpu_fd)?;
                crate::regs::setup_regs(&vcpu_fd, entry_point.raw_value())?;
                crate::regs::setup_sregs(&mem, &vcpu_fd)?;
            }

            vcpus.push(vcpu_fd);
        }

        Ok(Self {
            kvm,
            vm_fd,
            mem,
            vcpus,
            mmio64,
            ram_slot_count: regions.len() as u32,
        })
    }
}

/// Create an anonymous, sealable shared memory file of `size` bytes.
fn create_memfd(size: u64) -> Result<std::fs::File> {
    let name = c"nesbox-guest-ram";
    // SAFETY: `name` is a valid NUL-terminated string and the flags are valid.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create");
    }
    // SAFETY: memfd_create just handed us this fd and nothing else owns it.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.set_len(size)
        .context("failed to size the guest memory file")?;
    Ok(file)
}

pub fn run_vcpu_loop(
    _mem: Arc<GuestMemoryMmap>,
    mut vcpu_fd: VcpuFd,
    pci_bus: Arc<pci::Bus>,
    serial: Arc<crate::serial::Serial>,
    power: Arc<PowerDevice>,
    shutdown: Arc<Shutdown>,
) -> Result<()> {
    loop {
        if shutdown.is_requested() {
            break;
        }
        match vcpu_fd.run() {
            Ok(vcpu_exit) => match vcpu_exit {
                VcpuExit::IoOut(port, data) if PowerDevice::handles(port) => {
                    power.write(port, data);
                }
                VcpuExit::IoIn(port, data) if PowerDevice::handles(port) => {
                    power.read(port, data);
                }
                VcpuExit::IoOut(port, data) if crate::serial::Serial::handles(port) => {
                    serial.write(port, data);
                }
                VcpuExit::IoIn(port, data) if crate::serial::Serial::handles(port) => {
                    serial.read(port, data);
                }
                VcpuExit::IoOut(port, data) => {
                    if !pci_bus.handle_pio_write(port, data) {
                        log::trace!("PIO out: port={:#x}, len={}", port, data.len());
                    }
                }
                VcpuExit::IoIn(port, data) => {
                    if !pci_bus.handle_pio_read(port, data) {
                        data.fill(0xff);
                        log::trace!("PIO in: port={:#x}", port);
                    }
                }
                VcpuExit::MmioRead(addr, data) => {
                    if !pci_bus.handle_mmio_read(addr, data) {
                        data.fill(0xff);
                        log::trace!("Unhandled MMIO read: addr={:#x}", addr);
                    }
                }
                VcpuExit::MmioWrite(addr, data) => {
                    if !pci_bus.handle_mmio_write(addr, data) {
                        log::trace!("Unhandled MMIO write: addr={:#x}, len={}", addr, data.len());
                    }
                }
                VcpuExit::Hlt => {
                    // With an in-kernel irqchip this only happens when the
                    // guest halted with no way to be woken.
                    log::debug!("vCPU halted with interrupts disabled");
                    shutdown.request(ExitReason::GuestFault);
                    break;
                }
                VcpuExit::Shutdown => {
                    // A triple fault, or a reset we did not see through the
                    // reset register.
                    shutdown.request(ExitReason::GuestFault);
                    break;
                }
                VcpuExit::Exception => {
                    log::error!("vCPU exception");
                    shutdown.request(ExitReason::GuestFault);
                    break;
                }
                VcpuExit::FailEntry(reason, cpu) => {
                    shutdown.request(ExitReason::Error(format!(
                        "VM entry failed with reason {reason:#x}"
                    )));
                    log::error!("VM entry failed: reason={:#x} cpu={}", reason, cpu);
                    if let Ok(r) = vcpu_fd.get_regs() {
                        log::error!(
                            "regs: rip={:#x} rsp={:#x} rsi={:#x} rflags={:#x}",
                            r.rip,
                            r.rsp,
                            r.rsi,
                            r.rflags
                        );
                    }
                    if let Ok(s) = vcpu_fd.get_sregs() {
                        log::error!(
                            "cr0={:#x} cr3={:#x} cr4={:#x} efer={:#x}",
                            s.cr0,
                            s.cr3,
                            s.cr4,
                            s.efer
                        );
                        log::error!(
                            "cs: sel={:#x} base={:#x} limit={:#x} type={:#x} l={} db={} g={} p={} s={} unusable={}",
                            s.cs.selector,
                            s.cs.base,
                            s.cs.limit,
                            s.cs.type_,
                            s.cs.l,
                            s.cs.db,
                            s.cs.g,
                            s.cs.present,
                            s.cs.s,
                            s.cs.unusable
                        );
                        log::error!(
                            "ds: sel={:#x} type={:#x} p={} s={} unusable={}",
                            s.ds.selector,
                            s.ds.type_,
                            s.ds.present,
                            s.ds.s,
                            s.ds.unusable
                        );
                        log::error!(
                            "tr: sel={:#x} base={:#x} limit={:#x} type={:#x} p={} s={} unusable={}",
                            s.tr.selector,
                            s.tr.base,
                            s.tr.limit,
                            s.tr.type_,
                            s.tr.present,
                            s.tr.s,
                            s.tr.unusable
                        );
                        log::error!(
                            "ldt: sel={:#x} type={:#x} p={} unusable={}",
                            s.ldt.selector,
                            s.ldt.type_,
                            s.ldt.present,
                            s.ldt.unusable
                        );
                        log::error!(
                            "gdt: base={:#x} limit={:#x}  idt: base={:#x} limit={:#x}",
                            s.gdt.base,
                            s.gdt.limit,
                            s.idt.base,
                            s.idt.limit
                        );
                    }
                    break;
                }
                other => {
                    log::debug!("Unhandled vCPU exit: {:?}", other);
                }
            },
            Err(e) if e.errno() == libc::EINTR => {
                // Woken to notice the stop request; the loop head checks it.
                continue;
            }
            Err(e) if e.errno() == libc::EAGAIN => {
                // An application processor that has not been started yet.
                // KVM_RUN on a vCPU still in KVM_MP_STATE_UNINITIALIZED sleeps
                // in the kernel until INIT/SIPI arrives and then returns
                // EAGAIN, expecting to be called again. This is not a busy
                // wait: the blocking happens on the other side of the ioctl.
                continue;
            }
            Err(e) => {
                log::error!("vCPU run error: {}", e);
                shutdown.request(ExitReason::Error(format!("KVM_RUN failed: {e}")));
                break;
            }
        }
    }
    Ok(())
}
