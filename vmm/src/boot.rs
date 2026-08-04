use anyhow::{Context, Result};
use linux_loader::loader::bootparam::boot_params;
use linux_loader::loader::elf::Elf;
use linux_loader::loader::{Cmdline, KernelLoader, KernelLoaderResult, load_cmdline};
use std::fs::File;
use std::io::Read;
use vm_memory::{Address, ByteValued, Bytes, GuestAddress, GuestMemoryMmap};

// Constants for x86_64 Linux boot protocol
const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xaa55;
const KERNEL_HDR_MAGIC: u32 = 0x53726448;
const KERNEL_LOADER_OTHER: u8 = 0xff;
const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x1000000;
const ZERO_PAGE_ADDR: GuestAddress = GuestAddress(0x7000);
const CMDLINE_ADDR: GuestAddress = GuestAddress(0x20000);
const CMDLINE_MAX_LEN: usize = 2048;

/// Add an e820 entry to the boot_params structure
fn add_e820_entry(params: &mut boot_params, addr: u64, size: u64, mem_type: u32) -> Result<()> {
    let idx = params.e820_entries as usize;
    if idx >= params.e820_table.len() {
        anyhow::bail!("e820 table full");
    }
    params.e820_table[idx].addr = addr;
    params.e820_table[idx].size = size;
    params.e820_table[idx].r#type = mem_type;
    params.e820_entries += 1;
    Ok(())
}

/// Sets up the x86 boot parameters and command line in guest memory.
///
/// `ram_regions` describes guest RAM as laid out by [`crate::layout`]; each
/// region becomes an e820 entry, with the ACPI block at the top of low RAM
/// carved out and marked reserved.
pub fn setup_boot_params(
    mem: &GuestMemoryMmap,
    entry_point: GuestAddress,
    cmdline_str: &str,
    ram_regions: &[crate::layout::RamRegion],
    rsdp_addr: Option<GuestAddress>,
) -> Result<()> {
    let mut params = boot_params::default();

    // Basic boot header fields
    params.hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC;
    params.hdr.header = KERNEL_HDR_MAGIC;
    params.hdr.type_of_loader = KERNEL_LOADER_OTHER;
    params.hdr.kernel_alignment = KERNEL_MIN_ALIGNMENT_BYTES;
    params.hdr.cmd_line_ptr = CMDLINE_ADDR.raw_value() as u32;
    // Set entry point
    params.hdr.code32_start = entry_point.raw_value() as u32;

    // Command line length (including null terminator)
    let cmdline_cstring =
        std::ffi::CString::new(cmdline_str).context("Command line contains null byte")?;
    params.hdr.cmdline_size = cmdline_cstring.as_bytes_with_nul().len() as u32;

    // e820 map. The ACPI block sits at the top of the first region; report the
    // RAM below it, then the ACPI block itself as E820_ACPI.
    const E820_RAM: u32 = 1;
    const E820_ACPI: u32 = 3;

    let acpi_start = crate::layout::acpi_start(
        ram_regions
            .first()
            .map(|(s, size)| s.raw_value() + *size as u64)
            .context("no RAM regions")?,
    )
    .raw_value();

    for (i, &(start, size)) in ram_regions.iter().enumerate() {
        let (start, size) = (start.raw_value(), size as u64);
        if i == 0 {
            let usable = acpi_start
                .checked_sub(start)
                .context("memory too small for ACPI reservation")?;
            add_e820_entry(&mut params, start, usable, E820_RAM)?;
            add_e820_entry(&mut params, acpi_start, crate::layout::ACPI_SIZE, E820_ACPI)?;
        } else {
            add_e820_entry(&mut params, start, size, E820_RAM)?;
        }
    }

    // Set ACPI RSDP address if provided
    if let Some(addr) = rsdp_addr {
        params.acpi_rsdp_addr = addr.raw_value();
    }

    // Write the entire boot_params struct to the zero page
    let params_slice = params.as_slice();
    mem.write_slice(params_slice, ZERO_PAGE_ADDR)
        .context("Failed to write boot_params to zero page")?;

    // Write command line
    let mut cmdline = Cmdline::new(CMDLINE_MAX_LEN).context("Failed to create Cmdline")?;
    cmdline
        .insert_str(cmdline_str)
        .context("Failed to insert command line")?;
    load_cmdline(mem, CMDLINE_ADDR, &cmdline).context("Failed to write command line")?;

    Ok(())
}

/// Loads the ELF kernel into guest memory
pub fn load_kernel(
    mem: &GuestMemoryMmap,
    kernel_path: &std::path::Path,
) -> Result<KernelLoaderResult> {
    let mut kernel_file = File::open(kernel_path)
        .with_context(|| format!("Failed to open kernel file: {:?}", kernel_path))?;
    let mut kernel_data = Vec::new();
    kernel_file
        .read_to_end(&mut kernel_data)
        .context("Failed to read kernel file")?;

    let loader_result = Elf::load(
        mem,
        None, // kernel_offset
        &mut std::io::Cursor::new(kernel_data),
        None, // highmem_start_address
    )
    .context("Failed to load ELF kernel")?;

    Ok(loader_result)
}
