//! x86_64 vCPU register configuration (FPU, GPRs, SREGs, page tables)

use anyhow::{Context, Result};
use kvm_bindings::{kvm_fpu, kvm_regs, kvm_sregs};
use kvm_ioctls::VcpuFd;
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use crate::gdt::{gdt_entry, kvm_segment_from_gdt};

// Where the boot-time structures go in low memory. Distinct from
// `layout.rs`, which describes the guest's physical address map.
const ZERO_PAGE_START: u64 = 0x7000;
const BOOT_STACK_POINTER: u64 = 0x8ff0;
const BOOT_GDT_OFFSET: u64 = 0x500;
const BOOT_IDT_OFFSET: u64 = 0x520;
const BOOT_GDT_MAX: usize = 4;

const X86_CR0_PE: u64 = 0x1;
const X86_CR0_ET: u64 = 0x10;
const X86_CR0_PG: u64 = 0x8000_0000;
const X86_CR4_PAE: u64 = 0x20;
const EFER_LME: u64 = 0x100;
const EFER_LMA: u64 = 0x400;

// Initial page tables for identity mapping
// Using a larger area for PDE table (1024 entries * 8 bytes = 8192 bytes)
const PML4_START: u64 = 0x9000;
const PDPTE_START: u64 = 0xa000;
const PDE_START: u64 = 0xc000; // leaving room for 8KB

/// Set up Floating Point Unit registers (FPU) for the vCPU
pub fn setup_fpu(vcpu: &VcpuFd) -> Result<()> {
    let fpu = kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    };
    vcpu.set_fpu(&fpu).context("Failed to set FPU")?;
    Ok(())
}

/// Set up general purpose registers for the vCPU
/// `entry_point` is the kernel entry point (guest physical address)
pub fn setup_regs(vcpu: &VcpuFd, entry_point: u64) -> Result<()> {
    let regs = kvm_regs {
        rflags: 0x0000_0000_0000_0002, // EFLAGS: reserved bit 1 always set
        rip: entry_point,
        rsp: BOOT_STACK_POINTER,
        rbp: BOOT_STACK_POINTER,
        rsi: ZERO_PAGE_START, // Pointer to zero page (boot_params)
        ..Default::default()
    };
    vcpu.set_regs(&regs)
        .context("Failed to set general registers")?;
    Ok(())
}

/// Write the GDT table into guest memory and update KVM sregs
fn write_gdt_table(mem: &GuestMemoryMmap, gdt: &[u64]) -> Result<()> {
    let gdt_addr = GuestAddress(BOOT_GDT_OFFSET);
    for (i, entry) in gdt.iter().enumerate() {
        let offset = i * size_of::<u64>();
        let addr = mem
            .checked_offset(gdt_addr, offset)
            .context("GDT offset overflow")?;
        mem.write_obj(*entry, addr)
            .context("Failed to write GDT entry")?;
    }
    Ok(())
}

/// Write a null IDT (just one 8-byte entry) into guest memory
fn write_idt_table(mem: &GuestMemoryMmap) -> Result<()> {
    let idt_addr = GuestAddress(BOOT_IDT_OFFSET);
    mem.write_obj(0u64, idt_addr)
        .context("Failed to write IDT")?;
    Ok(())
}

/// Set up page tables for identity mapping
/// This enables long mode and paging
pub fn setup_page_tables(mem: &GuestMemoryMmap, sregs: &mut kvm_sregs) -> Result<()> {
    let pml4_addr = GuestAddress(PML4_START);
    let pdpte_addr = GuestAddress(PDPTE_START);
    let pde_addr = GuestAddress(PDE_START);

    // PML4 entry (points to PDPT)
    mem.write_obj(pdpte_addr.raw_value() | 0x03, pml4_addr)
        .context("Failed to write PML4")?;

    // PDPT entry (points to PDT)
    mem.write_obj(pde_addr.raw_value() | 0x03, pdpte_addr)
        .context("Failed to write PDPTE")?;

    // Map 2 GiB using 2 MiB pages (1024 entries)
    for i in 0..1024 {
        let addr = pde_addr.unchecked_add(i * 8);
        // 2 MiB page: PS=1, present, writable, user
        let pde = ((i) << 21) | 0x83u64;
        mem.write_obj(pde, addr).context("Failed to write PDE")?;
    }

    sregs.cr3 = pml4_addr.raw_value();
    sregs.cr4 |= X86_CR4_PAE;
    sregs.cr0 |= X86_CR0_PG;
    Ok(())
}

/// Configure special registers (sregs) for the vCPU: GDT, IDT, segment selectors, paging
pub fn setup_sregs(mem: &GuestMemoryMmap, vcpu: &VcpuFd) -> Result<()> {
    let mut sregs = vcpu.get_sregs().context("Failed to get sregs")?;

    // Build GDT entries (null, code, data, TSS)
    let gdt_table: [u64; BOOT_GDT_MAX] = [
        gdt_entry(0, 0, 0),            // NULL
        gdt_entry(0xa09b, 0, 0xfffff), // Code segment (64-bit)
        gdt_entry(0xc093, 0, 0xfffff), // Data segment
        // TSS. The type must be 11 (busy 64-bit TSS), not 9 (available) —
        // VMX rejects VM entry into long mode with an available TSS.
        gdt_entry(0x808b, 0, 0xfffff),
    ];

    write_gdt_table(mem, &gdt_table)?;
    write_idt_table(mem)?;

    // Set up segment selectors from GDT entries
    let code_seg = kvm_segment_from_gdt(gdt_table[1], 1);
    let data_seg = kvm_segment_from_gdt(gdt_table[2], 2);
    let tss_seg = kvm_segment_from_gdt(gdt_table[3], 3);

    sregs.gdt.base = BOOT_GDT_OFFSET;
    sregs.gdt.limit = (size_of_val(&gdt_table) - 1) as u16;

    sregs.idt.base = BOOT_IDT_OFFSET;
    sregs.idt.limit = (size_of::<u64>() - 1) as u16;

    sregs.cs = code_seg;
    sregs.ds = data_seg;
    sregs.es = data_seg;
    sregs.fs = data_seg;
    sregs.gs = data_seg;
    sregs.ss = data_seg;
    sregs.tr = tss_seg;

    // KVM's reset LDTR is a null selector that is still marked present and
    // usable, which VMX rejects when entering long mode. Mark it unusable.
    sregs.ldt.unusable = 1;
    sregs.ldt.present = 0;

    // Enable protected mode and long mode
    // Clear cache-disable bits KVM may have pre-set, then set what we need
    const X86_CR0_NW: u64 = 1 << 29;
    const X86_CR0_CD: u64 = 1 << 30;
    sregs.cr0 &= !(X86_CR0_NW | X86_CR0_CD);
    sregs.cr0 |= X86_CR0_PE | X86_CR0_ET | X86_CR0_PG;
    sregs.efer |= EFER_LME | EFER_LMA;

    // Set up page tables
    setup_page_tables(mem, &mut sregs)?;

    vcpu.set_sregs(&sregs).context("Failed to set sregs")?;
    Ok(())
}
