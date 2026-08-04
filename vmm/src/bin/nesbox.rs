use nesbox_vmm::{config, interrupt, vm};

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

    // Create PCI bus
    let pci_bus = Arc::new(Bus::new());

    // ── Block device ──────────────────────────────────────────────────────
    let root_drive = config
        .drives
        .iter()
        .find(|d| d.is_root_device)
        .context("No root drive specified")?;
    let blk_device = BlkDevice::new(&root_drive.path_on_host, root_drive.is_read_only)?;

    let blk_irq0 = interrupt::MsixVector::new(&vm.vm_fd).context("blk irq0")?;
    let blk_irq1 = interrupt::MsixVector::new(&vm.vm_fd).context("blk irq1")?;
    blk_device.set_irq_fds(vec![Arc::new(blk_irq0.irq_fd), Arc::new(blk_irq1.irq_fd)]);
    blk_device.set_mem(vm.mem.clone());
    let _blk_pci_device = pci_bus.add_device(blk_device)?;

    // ── Console device ────────────────────────────────────────────────────
    let console_device = ConsoleDevice::new();
    let con_irq_tx = interrupt::MsixVector::new(&vm.vm_fd).context("console irq tx")?;
    let con_irq_rx = interrupt::MsixVector::new(&vm.vm_fd).context("console irq rx")?;
    console_device.set_irq_tx(Arc::new(con_irq_tx.irq_fd));
    console_device.set_irq_rx(Arc::new(con_irq_rx.irq_fd));
    console_device.set_mem(vm.mem.clone());
    let _console_pci_device = pci_bus.add_device(console_device)?;

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
