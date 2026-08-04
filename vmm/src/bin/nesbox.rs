use nesbox_vmm::{acpi_slot_gsi, config, interrupt::IrqManager, vm};

use anyhow::{Context, Result};
use env_logger::Env;
use log::info;
use pci::Bus;
use std::io::stdin;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use termios::*;
use virtio_devices::{BlkDevice, ConsoleDevice};

/// Restores terminal settings on drop.
struct RawMode {
    orig: Termios,
}
impl RawMode {
    fn enter() -> Result<Self> {
        let fd = stdin().as_raw_fd();
        let orig = Termios::from_fd(fd).context("tcgetattr")?;
        let mut raw = orig;
        cfmakeraw(&mut raw);
        tcsetattr(fd, TCSANOW, &raw).context("tcsetattr")?;
        Ok(Self { orig })
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        let fd = stdin().as_raw_fd();
        let _ = tcsetattr(fd, TCSANOW, &self.orig);
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    // Set terminal to raw mode so stdin is unbuffered and Ctrl-C passes through
    let _raw = RawMode::enter()?;

    let config_path = std::env::args()
        .nth(1)
        .context("Usage: nesbox <config.json>")?;
    let config_str = std::fs::read_to_string(&config_path).context("Failed to read config")?;
    let config: config::VmConfig =
        serde_json::from_str(&config_str).context("Invalid JSON config")?;

    info!("Starting VMM with config: {:#?}", config);

    // Create KVM VM
    let vm = vm::Vm::new(
        config.machine_config.mem_size_mib,
        config.machine_config.vcpu_count,
        &config.boot_source.kernel_image_path,
        &config.boot_source.boot_args,
    )?;

    // Create PCI bus and interrupt routing
    let pci_bus = Arc::new(Bus::new());
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

    // ── Legacy COM1, for early boot output ────────────────────────────────
    let serial = Arc::new(nesbox_vmm::serial::Serial::new());

    // ── Run vCPUs ─────────────────────────────────────────────────────────
    let handles: Vec<_> = vm
        .vcpus
        .into_iter()
        .map(|vcpu_fd| {
            let mem = vm.mem.clone();
            let pci_bus = pci_bus.clone();
            let serial = serial.clone();
            std::thread::spawn(move || {
                if let Err(e) = vm::run_vcpu_loop(mem, vcpu_fd, pci_bus, serial) {
                    eprintln!("vCPU thread error: {}", e);
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    Ok(())
}
