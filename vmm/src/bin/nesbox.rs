use nesbox_vmm::lifecycle::{ExitReason, Shutdown};
use nesbox_vmm::power::PowerDevice;
use nesbox_vmm::{acpi_slot_gsi, config, interrupt::IrqManager, virtiofsd::Virtiofsd, vm};

use anyhow::{Context, Result};
use env_logger::Env;
use log::info;
use pci::Bus;
use std::io::stdin;
use std::os::fd::AsRawFd;
use std::os::unix::thread::JoinHandleExt;
use std::sync::Arc;
use termios::*;
use nesbox_vmm::memslot::MemorySlots;
use virtio_devices::gpu::display::DisplayInfo;
use virtio_devices::{
    BlkDevice, ConsoleDevice, FsDevice, GpuConfig, GpuDevice, NetConfig, NetDevice, VsockDevice,
};

/// BAR index the GPU puts its shared memory window in.
const GPU_SHM_BAR: usize = 2;

const USAGE: &str = "Usage:
  nesbox <config.json>            run a VM

Host networking -- bridge, VLAN uplink, and the taps guests attach to -- is set
up separately by scripts/nestri-net-setup.sh. nesbox opens a tap that already
exists, which needs no capabilities: the kernel only demands CAP_NET_ADMIN to
create a device, or from someone who is not its owner.";

/// Puts the terminal in raw mode so guest console input is unbuffered, and
/// restores it on drop.
///
/// A no-op when stdin is not a terminal: a launcher spawns the VMM with pipes,
/// and failing to start in that case would make it unusable as a child
/// process.
struct RawMode {
    orig: Option<Termios>,
}
impl RawMode {
    fn enter() -> Result<Self> {
        let fd = stdin().as_raw_fd();
        // SAFETY: isatty only inspects the fd.
        if unsafe { libc::isatty(fd) } != 1 {
            log::debug!("stdin is not a terminal; leaving it as it is");
            return Ok(Self { orig: None });
        }
        let orig = Termios::from_fd(fd).context("tcgetattr")?;
        let mut raw = orig;
        cfmakeraw(&mut raw);
        tcsetattr(fd, TCSANOW, &raw).context("tcsetattr")?;
        Ok(Self { orig: Some(orig) })
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(orig) = self.orig {
            let _ = tcsetattr(stdin().as_raw_fd(), TCSANOW, &orig);
        }
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let config_path = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .context(USAGE)?
        .clone();

    let config_str = std::fs::read_to_string(&config_path).context("Failed to read config")?;
    let config: config::VmConfig =
        serde_json::from_str(&config_str).context("Invalid JSON config")?;

    // Raw mode belongs to the guest console and nothing else, and is entered
    // only once the config has parsed: raw mode turns off echo, line buffering
    // and the signal characters, so anything that still wants to report a
    // problem has to do it first.
    let _raw = RawMode::enter()?;

    info!("Starting VMM with config: {:#?}", config);

    // Create KVM VM
    let vm = vm::Vm::new(
        config.machine_config.mem_size_mib,
        config.machine_config.vcpu_count,
        &config.boot_source.kernel_image_path,
        &config.boot_source.boot_args,
    )?;

    // Create PCI bus and interrupt routing
    let pci_bus = Arc::new(Bus::new(pci::Mmio64Window::new(
        vm.mmio64.start,
        vm.mmio64.size,
    )));
    let irq = IrqManager::new(vm.vm_fd.clone())?;

    // ── Block device ──────────────────────────────────────────────────────
    let root_drive = config
        .drives
        .iter()
        .find(|d| d.is_root_device)
        .context("No root drive specified")?;
    let blk_device = BlkDevice::new(&root_drive.path_on_host, root_drive.is_read_only)?;

    blk_device.set_mem(vm.mem.clone());
    let blk_vectors = irq.allocate_msi_vectors(2).context("blk MSI-X vectors")?;
    let blk_intx = irq.legacy_irqfd(acpi_slot_gsi(1)).context("blk INTx")?;
    blk_device.bind_interrupts(blk_vectors, irq.clone(), blk_intx);
    let blk_bdf = pci_bus.add_device(blk_device)?;
    info!("virtio-blk at {:02x}:{:02x}.{}", blk_bdf.0, blk_bdf.1, blk_bdf.2);

    // ── Console device ────────────────────────────────────────────────────
    let console_device = ConsoleDevice::new();
    console_device.set_mem(vm.mem.clone());
    let con_vectors = irq.allocate_msi_vectors(4).context("console MSI-X vectors")?;
    let con_intx = irq.legacy_irqfd(acpi_slot_gsi(2)).context("console INTx")?;
    console_device.bind_interrupts(con_vectors, irq.clone(), con_intx);
    let con_bdf = pci_bus.add_device(console_device)?;
    info!("virtio-console at {:02x}:{:02x}.{}", con_bdf.0, con_bdf.1, con_bdf.2);

    // ── Vsock device (optional) ───────────────────────────────────────────
    if let Some(vsock_cfg) = &config.vsock {
        let vsock_device = VsockDevice::new(vsock_cfg.guest_cid, vm.mem.clone())?;
        let vsock_vectors = irq.allocate_msi_vectors(4).context("vsock MSI-X vectors")?;
        let slot = 3;
        let vsock_intx = irq.legacy_irqfd(acpi_slot_gsi(slot)).context("vsock INTx")?;
        vsock_device.bind_interrupts(vsock_vectors, irq.clone(), vsock_intx);
        let bdf = pci_bus.add_device(vsock_device)?;
        info!("virtio-vsock at {:02x}:{:02x}.{}", bdf.0, bdf.1, bdf.2);
    }

    // ── Network device (optional) ─────────────────────────────────────────
    if let Some(net_cfg) = &config.network {
        let net_device = NetDevice::new(
            &NetConfig {
                tap_name: net_cfg.tap_name.clone(),
                mac: net_cfg.parsed_mac()?,
            },
            vm.mem.clone(),
        )?;
        let net_vectors = irq.allocate_msi_vectors(4).context("net MSI-X vectors")?;
        let slot = 4;
        let net_intx = irq.legacy_irqfd(acpi_slot_gsi(slot)).context("net INTx")?;
        net_device.bind_interrupts(net_vectors, irq.clone(), net_intx);
        let bdf = pci_bus.add_device(net_device)?;
        info!("virtio-net at {:02x}:{:02x}.{}", bdf.0, bdf.1, bdf.2);
    }

    // ── GPU (optional) ────────────────────────────────────────────────────
    // Added before virtio-fs so its slot does not shift when a share is added
    // or removed. Its shared window is a real memory slot, taken after the
    // ones guest RAM already occupies.
    if let Some(gpu_cfg) = &config.gpu {
        // The VRAM limit is enforced inside virglrenderer, which reads it from
        // the environment: the refusal has to happen where it can be reported to
        // the guest, and only the renderer owns that channel. See
        // `virtio-devices/src/gpu/vram.rs`. One VMM process serves one guest, so
        // process scope is guest scope.
        if let Some(mib) = gpu_cfg.vram_limit_mib {
            // SAFETY: single-threaded here. Devices are built before any vCPU or
            // worker thread is spawned, so no other thread can be reading the
            // environment concurrently.
            unsafe { std::env::set_var("NESTRI_VRAM_LIMIT_MIB", mib.to_string()) };
        }

        let slots = MemorySlots::new(vm.vm_fd.clone(), vm.ram_slot_count);
        let gpu_device = Arc::new(GpuDevice::new(
            &GpuConfig {
                render_node: gpu_cfg.render_node.clone(),
                displays: vec![DisplayInfo::new(gpu_cfg.width, gpu_cfg.height)],
                vram_limit_bytes: gpu_cfg.vram_limit_mib.map(|m| m * (1 << 20)),
            },
            vm.mem.clone(),
        )?);
        let gpu_vectors = irq.allocate_msi_vectors(4).context("GPU MSI-X vectors")?;
        let slot = 5;
        let gpu_intx = irq.legacy_irqfd(acpi_slot_gsi(slot)).context("GPU INTx")?;
        gpu_device.bind_interrupts(gpu_vectors, irq.clone(), gpu_intx);
        gpu_device.bind_mapper(slots);
        let bdf = pci_bus.add_device_arc(gpu_device.clone())?;
        // The shared window has to appear at whatever address the bus gave
        // BAR2, so the device only learns it now.
        let shm_addr = pci_bus
            .bar_address(bdf, GPU_SHM_BAR)
            .context("the GPU has no BAR2")?;
        gpu_device.set_shm_guest_addr(shm_addr);
        info!(
            "virtio-gpu at {:02x}:{:02x}.{}, shared window at {shm_addr:#x}",
            bdf.0, bdf.1, bdf.2
        );
    }

    // ── Shared directories over virtio-fs ─────────────────────────────────
    // The daemons are kept alive for as long as the VM runs; dropping them
    // kills virtiofsd.
    let runtime_dir = std::env::temp_dir().join(format!("nesbox-{}", std::process::id()));
    let mut fs_daemons = Vec::new();
    // Devices take PCI slots in the order they are added, and the INTx line
    // follows the slot, so where these start depends on whether there is a
    // network device ahead of them.
    let mut first_fs_slot = 4;
    if config.network.is_some() {
        first_fs_slot += 1;
    }
    if config.gpu.is_some() {
        first_fs_slot += 1;
    }
    for shared in &config.shared_directories {
        let daemon = Virtiofsd::spawn(
            &shared.tag,
            &shared.path_on_host,
            shared.read_only,
            &runtime_dir,
        )?;
        let fs_device = FsDevice::new(&shared.tag, daemon.socket_path(), vm.mem.clone())?;
        let vectors = irq.allocate_msi_vectors(4).context("virtio-fs MSI-X vectors")?;
        let slot = first_fs_slot + fs_daemons.len() as u32;
        let intx = irq.legacy_irqfd(acpi_slot_gsi(slot)).context("virtio-fs INTx")?;
        fs_device.bind_interrupts(vectors, irq.clone(), intx);
        let bdf = pci_bus.add_device(fs_device)?;
        info!(
            "virtio-fs \"{}\" at {:02x}:{:02x}.{}",
            shared.tag, bdf.0, bdf.1, bdf.2
        );
        fs_daemons.push(daemon);
    }

    // ── Legacy COM1, for early boot output ────────────────────────────────
    let serial = Arc::new(nesbox_vmm::serial::Serial::new());

    // ── Lifetime ──────────────────────────────────────────────────────────
    let shutdown = Shutdown::new();
    let power = Arc::new(PowerDevice::new(shutdown.clone()));
    install_signal_handlers(shutdown.clone())?;

    // ── Run vCPUs ─────────────────────────────────────────────────────────
    // The CPU set every vCPU thread is confined to, if the config named one.
    // Built once here rather than per thread: it is the same set for all of
    // them, and cpu_set_t is Copy.
    let cpuset = build_cpuset(&config.machine_config.cpu_affinity);

    let handles: Vec<_> = vm
        .vcpus
        .into_iter()
        .enumerate()
        .map(|(vcpu_id, vcpu_fd)| {
            let mem = vm.mem.clone();
            let pci_bus = pci_bus.clone();
            let serial = serial.clone();
            let power = power.clone();
            let shutdown = shutdown.clone();
            std::thread::Builder::new()
                // Named so the threads are identifiable from the host. Without
                // this they are anonymous, and anything done to place them --
                // by us or by an operator with taskset -- cannot be checked.
                .name(format!("vcpu{vcpu_id}"))
                .spawn(move || {
                    if let Some(set) = cpuset {
                        set_affinity_or_warn(vcpu_id, &set);
                    }
                    if let Err(e) =
                        vm::run_vcpu_loop(mem, vcpu_fd, pci_bus, serial, power, shutdown)
                    {
                        eprintln!("vCPU thread error: {}", e);
                    }
                })
                .expect("failed to spawn vCPU thread")
        })
        .collect();

    // A vCPU blocked in KVM_RUN only notices the stop request when a signal
    // interrupts it, so wake every thread once the VM is asked to stop.
    let watcher = {
        let shutdown = shutdown.clone();
        let vcpu_threads: Vec<_> = handles.iter().map(|h| h.as_pthread_t()).collect();
        std::thread::spawn(move || {
            while !shutdown.is_requested() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            for thread in vcpu_threads {
                // SAFETY: the vCPU threads are joined below, so these ids stay
                // valid until after this signal is delivered.
                unsafe { libc::pthread_kill(thread, VCPU_WAKE_SIGNAL) };
            }
        })
    };

    for handle in handles {
        handle.join().unwrap();
    }
    shutdown.request(ExitReason::Error("all vCPUs stopped".into()));
    let _ = watcher.join();

    // Devices and their backends are torn down here: dropping the virtiofsd
    // supervisors kills them, and the VM's memory goes with the process.
    drop(fs_daemons);

    let reason = shutdown.reason().unwrap_or(ExitReason::GuestFault);
    if reason.is_clean() {
        info!("VM stopped: {reason}");
    } else {
        log::error!("VM stopped: {reason}");
    }
    std::process::exit(reason.exit_code());
}

/// Signal used to wake a vCPU out of KVM_RUN. Its handler does nothing; the
/// EINTR it causes is the point.
const VCPU_WAKE_SIGNAL: libc::c_int = libc::SIGUSR1;

extern "C" fn wake_handler(_: libc::c_int) {}

extern "C" fn stop_handler(_: libc::c_int) {
    // Async-signal-safe: just flips an atomic.
    if let Some(shutdown) = SHUTDOWN.get() {
        shutdown.request(ExitReason::HostSignal);
    }
}

static SHUTDOWN: std::sync::OnceLock<Arc<Shutdown>> = std::sync::OnceLock::new();

/// SIGTERM and SIGINT ask the VM to stop; SIGUSR1 just interrupts KVM_RUN.
/// Build the CPU set the vCPU threads are confined to, or `None` for "wherever
/// the host likes".
///
/// Returns `None` for an empty list rather than an empty `cpu_set_t`: an empty
/// set is not "no restriction", it is "no CPU at all", and `sched_setaffinity`
/// rejects it. Reading an absent config field as that would be a hang, not a
/// default.
fn build_cpuset(cpus: &[usize]) -> Option<libc::cpu_set_t> {
    if cpus.is_empty() {
        return None;
    }
    // SAFETY: all-zeros is a valid cpu_set_t, and CPU_ZERO makes that explicit.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe { libc::CPU_ZERO(&mut set) };
    for cpu in cpus {
        // CPU_SET is out-of-bounds for anything past CPU_SETSIZE, so a config
        // naming a CPU this host does not have is dropped rather than
        // corrupting the set beside it. sched_setaffinity would reject the
        // whole call for an unknown CPU anyway; skipping keeps the CPUs that
        // do exist usable.
        if *cpu < libc::CPU_SETSIZE as usize {
            // SAFETY: FFI call, index bounds checked above.
            unsafe { libc::CPU_SET(*cpu, &mut set) };
        } else {
            eprintln!("cpu_affinity: ignoring CPU {cpu}, past CPU_SETSIZE");
        }
    }
    Some(set)
}

/// Confine the calling thread to `set`.
///
/// A warning rather than a failure: placement is an optimisation, and a guest
/// that runs on the wrong cores is better than one that does not boot. The
/// usual cause is a config naming CPUs this host does not have, which is worth
/// seeing but not worth dying over.
fn set_affinity_or_warn(vcpu_id: usize, set: &libc::cpu_set_t) {
    // SAFETY: FFI call; pid 0 is the calling thread, and the size matches.
    let ret = unsafe {
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), set)
    };
    if ret != 0 {
        eprintln!(
            "vcpu{vcpu_id}: could not set CPU affinity: {}",
            std::io::Error::last_os_error()
        );
    }
}

fn install_signal_handlers(shutdown: Arc<Shutdown>) -> Result<()> {
    let _ = SHUTDOWN.set(shutdown);
    // SAFETY: both handlers are async-signal-safe.
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGINT] {
            if libc::signal(signal, stop_handler as *const () as libc::sighandler_t) == libc::SIG_ERR {
                anyhow::bail!("failed to install handler for signal {signal}");
            }
        }
        if libc::signal(VCPU_WAKE_SIGNAL, wake_handler as *const () as libc::sighandler_t) == libc::SIG_ERR {
            anyhow::bail!("failed to install the vCPU wake handler");
        }
    }
    Ok(())
}
