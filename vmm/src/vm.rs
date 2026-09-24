use crate::lifecycle::{ExitReason, Shutdown};
use crate::power::PowerDevice;
use crate::{boot, layout};
use anyhow::{Context, Result};
use kvm_bindings::KVM_MAX_CPUID_ENTRIES;
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use std::os::fd::FromRawFd;
use std::sync::Arc;
use vm_memory::mmap::MmapRegionBuilder;
use vm_memory::{Address, FileOffset, GuestMemoryBackend, GuestMemoryMmap, GuestRegionMmap};

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

/// Stop a halting or spin-waiting vCPU from exiting to the host.
///
/// On a dedicated CPU both exits are pure cost. A guest `HLT` then halts the
/// physical core, and an interrupt for the guest wakes it without the host
/// scheduler or a VM entry in between; a `PAUSE` loop stops trapping, which
/// only ever helped a host deciding whether to run someone else instead. The
/// host sees each vCPU thread as permanently busy, which is true of a CPU that
/// belongs to the guest.
///
/// Best effort: a host that allows neither leaves the guest working exactly as
/// an undedicated one does, which is worth a warning and not a refusal.
fn disable_idle_exits(vm_fd: &VmFd) {
    use kvm_bindings::{
        KVM_CAP_X86_DISABLE_EXITS, KVM_X86_DISABLE_EXITS_HLT, KVM_X86_DISABLE_EXITS_PAUSE,
        kvm_enable_cap,
    };
    let allowed = vm_fd.check_extension_raw(KVM_CAP_X86_DISABLE_EXITS.into());
    let wanted = KVM_X86_DISABLE_EXITS_HLT | KVM_X86_DISABLE_EXITS_PAUSE;
    let mask = wanted & u32::try_from(allowed).unwrap_or(0);
    if mask != wanted {
        log::warn!(
            "dedicated: this host lets exits {mask:#x} be disabled of {wanted:#x} wanted; \
             the rest still trap"
        );
    }
    if mask == 0 {
        return;
    }
    let mut cap = kvm_enable_cap {
        cap: KVM_CAP_X86_DISABLE_EXITS,
        ..Default::default()
    };
    cap.args[0] = u64::from(mask);
    match vm_fd.enable_cap(&cap) {
        Ok(()) => log::info!("dedicated: HLT/PAUSE exits disabled ({mask:#x})"),
        Err(e) => log::warn!("dedicated: could not disable exits: {e}"),
    }
}

impl Vm {
    pub fn new(
        machine: &crate::config::MachineConfig,
        kernel_path: &std::path::Path,
        cmdline_str: &str,
    ) -> Result<Self> {
        let mem_size_mib = machine.mem_size_mib;
        let vcpu_count = machine.vcpu_count;
        let kvm = Kvm::new().context("Failed to open KVM")?;
        let vm_fd = Arc::new(kvm.create_vm().context("Failed to create VM")?);

        // Before any vCPU exists: KVM refuses the capability once one does.
        if machine.dedicated {
            disable_idle_exits(&vm_fd);
        }

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
        let hugetlb = machine.hugepages.hugetlb_size();
        let mem_file =
            create_memfd(mem_size, hugetlb).context("Failed to create guest memory file")?;
        let mut file_offset = 0u64;
        let mapped = regions
            .iter()
            .map(|&(start, size)| {
                let offset = file_offset;
                file_offset += size as u64;
                let file = mem_file
                    .try_clone()
                    .context("Failed to clone the guest memory file")?;
                map_ram(FileOffset::new(file, offset), size, start, hugetlb)
            })
            .collect::<Result<Vec<_>>>()?;
        let mem = GuestMemoryMmap::from_regions(mapped).context("Failed to create guest memory")?;
        let mem = Arc::new(mem);

        for (slot, &(start, size)) in regions.iter().enumerate() {
            let host_addr = mem
                .get_host_address(start)
                .context("Failed to get host address")?;
            // A hugetlb mapping has its page size already; the advice is for
            // shmem, and would only be refused.
            let pages = match hugetlb {
                Some(page) => format!(", {} MiB hugetlb pages", page >> 20),
                None if advise_huge(host_addr, size) => ", huge pages requested".to_owned(),
                None => String::new(),
            };
            log::info!(
                "RAM slot {}: guest {:#x}..{:#x} ({} MiB{})",
                slot,
                start.raw_value(),
                start.raw_value() + size as u64,
                size / (1024 * 1024),
                pages
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

        if machine.prefault {
            spawn_prefault(mem.clone(), regions.clone());
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
            crate::cpuid::patch_topology(
                &mut cpuid,
                u32::from(cpu_id),
                vcpu_count,
                machine.threads_per_core,
                vendor,
            );
            if machine.dedicated && !crate::cpuid::advertise_dedicated(&mut cpuid) && cpu_id == 0 {
                log::warn!("dedicated: KVM's CPUID leaves are absent, so the guest is not told");
            }
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

/// Create an anonymous shared memory file of `size` bytes, on hugetlb pages of
/// `hugetlb` bytes when given.
fn create_memfd(size: u64, hugetlb: Option<u64>) -> Result<std::fs::File> {
    let name = c"nesbox-guest-ram";
    let flags = match hugetlb {
        None => libc::MFD_CLOEXEC,
        Some(page) if page == 1 << 30 => libc::MFD_CLOEXEC | libc::MFD_HUGETLB | libc::MFD_HUGE_1GB,
        Some(_) => libc::MFD_CLOEXEC | libc::MFD_HUGETLB | libc::MFD_HUGE_2MB,
    };
    // SAFETY: `name` is a valid NUL-terminated string and the flags are valid.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), flags) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        return match hugetlb {
            // EINVAL is the kernel saying it has no pool of that size at all,
            // which is a host that was never set up for it rather than a bug.
            Some(page) if err.raw_os_error() == Some(libc::EINVAL) => Err(err)
                .with_context(|| format!("this host has no {} MiB hugetlb pages", page >> 20)),
            _ => Err(err).context("memfd_create"),
        };
    }
    // SAFETY: memfd_create just handed us this fd and nothing else owns it.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.set_len(size)
        .context("failed to size the guest memory file")?;
    Ok(file)
}

/// Map one region of guest RAM from the memory file, shared so vhost-user
/// backends see the same pages.
///
/// vm-memory's own file mapping passes `MAP_NORESERVE`. For shmem that changes
/// nothing, but for hugetlb it means no pool pages are set aside when the
/// mapping is made: a pool that runs dry then shows up when the next page is
/// first touched -- `KVM_RUN` failing on a vCPU, or SIGBUS on a device thread --
/// in the middle of a session, rather than as a box that will not start. Without it the kernel reserves every page of the region
/// here and refuses the mapping if the pool is short. Reserving takes pages off
/// the free count without zeroing them, so it costs nothing at boot.
fn map_ram(
    file: FileOffset,
    size: usize,
    start: vm_memory::GuestAddress,
    hugetlb: Option<u64>,
) -> Result<GuestRegionMmap> {
    let flags = match hugetlb {
        Some(_) => libc::MAP_SHARED,
        None => libc::MAP_SHARED | libc::MAP_NORESERVE,
    };
    let region = MmapRegionBuilder::new_with_bitmap(size, ())
        .with_file_offset(file)
        .with_mmap_prot(libc::PROT_READ | libc::PROT_WRITE)
        .with_mmap_flags(flags)
        .with_hugetlbfs(hugetlb.is_some())
        .build();
    let region = match (region, hugetlb) {
        (Ok(region), _) => region,
        (Err(e), Some(page)) => {
            let kib = page >> 10;
            return Err(e).with_context(|| {
                format!(
                    "could not reserve {} MiB of {} MiB hugetlb pages for guest RAM; \
                     the pool is set by /sys/kernel/mm/hugepages/hugepages-{kib}kB/nr_hugepages \
                     and what is left of it is free_hugepages minus resv_hugepages",
                    size >> 20,
                    page >> 20
                )
            });
        }
        (Err(e), None) => return Err(e).context("Failed to map guest memory"),
    };
    GuestRegionMmap::new(region, start).context("guest RAM region wraps the address space")
}

/// Fault in all of guest RAM on a thread of its own.
///
/// `MADV_POPULATE_WRITE` allocates each page as a write would, without writing,
/// so it is safe to race with the guest: a page the guest reached first is left
/// as it is. The thread inherits the I/O CPUs from whoever builds the VM, so
/// the zeroing does not compete with the vCPUs.
fn spawn_prefault(mem: Arc<GuestMemoryMmap>, regions: Vec<layout::RamRegion>) {
    let spawned = std::thread::Builder::new()
        .name("prefault".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            for &(start, size) in &regions {
                let Ok(host_addr) = mem.get_host_address(start) else {
                    continue;
                };
                // SAFETY: a range of a mapping `mem` owns and keeps alive for
                // the duration of the call. POPULATE_WRITE writes nothing.
                let ret =
                    unsafe { libc::madvise(host_addr.cast(), size, libc::MADV_POPULATE_WRITE) };
                if ret != 0 {
                    log::warn!(
                        "prefault: guest RAM at {:#x} left to fault on first touch: {}",
                        start.raw_value(),
                        std::io::Error::last_os_error()
                    );
                    return;
                }
            }
            log::info!(
                "prefault: guest RAM faulted in after {:?}",
                started.elapsed()
            );
        });
    if let Err(e) = spawned {
        log::warn!("prefault: could not start the thread, so guest RAM faults on first touch: {e}");
    }
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

/// Ask the kernel to back a guest RAM region with huge pages.
///
/// # Why this is needed even where THP is `always`
///
/// Guest RAM here is a **memfd**, mapped shared so vhost-user backends can see
/// it. `/sys/kernel/mm/transparent_hugepage/enabled` governs anonymous memory;
/// shmem -- which a memfd is -- is governed by `shmem_enabled`, a separate knob
/// that is `never` or `advise` on almost every kernel. So a host reading
/// `enabled = [always]` can still be running every guest on 4 KiB pages, and
/// nothing says so: an 8 GiB guest is then two million page-table entries and a
/// TLB miss on memory the guest touches constantly.
///
/// `advise` is the common setting and is exactly what this call satisfies.
/// Where `shmem_enabled` is `never` the advice is accepted and ignored, which
/// is why the log line says *requested* rather than *enabled*. What actually
/// happened is in `/proc/<pid>/smaps_rollup` as `ShmemPmdMapped`.
///
/// A failure is not fatal. Huge pages are a performance property, and a box
/// that runs slightly slower is better than one that does not start.
fn advise_huge(host_addr: *mut u8, size: usize) -> bool {
    // SAFETY: FFI call over a mapping this process just made, with its own
    // length. `madvise` neither reads nor writes the range.
    let ret = unsafe { libc::madvise(host_addr as *mut libc::c_void, size, libc::MADV_HUGEPAGE) };
    if ret != 0 {
        log::warn!(
            "guest RAM will use 4 KiB pages: madvise(MADV_HUGEPAGE) failed: {}",
            std::io::Error::last_os_error()
        );
        return false;
    }
    true
}
