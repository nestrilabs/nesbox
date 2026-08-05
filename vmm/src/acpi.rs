use acpi_tables::{Aml, aml, rsdp::Rsdp, sdt::Sdt};
use anyhow::{Context, Result};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use zerocopy::IntoBytes;

/// GSI a PCI slot's INTx line is wired to, as published in the DSDT's _PRT.
///
/// Re-exported from the crate root as `acpi_slot_gsi`; device wiring must use
/// it so the INTx eventfd lands on the pin the guest is told to expect.
///
/// INTx is the fallback path — a device that negotiates MSI-X never uses it.
/// Slot 1 gets its own line; everything else shares one, which is legal
/// because INTx is level-triggered and shareable.
pub fn slot_gsi(slot: u32) -> u32 {
    match slot {
        1 => 10,
        _ => 11,
    }
}

fn build_dsdt() -> Vec<u8> {
    let mut dsdt = Sdt::new(*b"DSDT", 36, 6, *b"NESTRI", *b"DSDT    ", 1);

    // _CRS: what the host bridge decodes. The memory window must agree with
    // the BAR allocator in the `pci` crate, or the guest will reassign BARs
    // to addresses we do not decode.
    let bus = aml::AddressSpace::new_bus_number(0x0u16, 0x0u16);
    let cam1 = aml::IO::new(0x0cf8, 0x0cf8, 1, 0x08);
    let mem32 = aml::AddressSpace::new_memory(
        aml::AddressSpaceCacheable::NotCacheable,
        true,
        crate::layout::PCI_MMIO_START,
        crate::layout::PCI_MMIO_END - 1,
        None,
    );
    let io_range1 = aml::AddressSpace::new_io(0x0000u16, 0x0cf7u16, None);
    let io_range2 = aml::AddressSpace::new_io(0x0d00u16, 0xffffu16, None);
    let crs = aml::ResourceTemplate::new(vec![&bus, &cam1, &mem32, &io_range1, &io_range2]);

    // _PRT: one entry per slot, mapping INTA# to a GSI. Address 0xFFFF in the
    // low word means "any function of this device".
    let prt_data: Vec<(u32, u32)> = (0..32u32)
        .map(|slot| ((slot << 16) | 0xFFFF, slot_gsi(slot)))
        .collect();
    let zero = 0u8;
    let prt_items: Vec<aml::Package> = prt_data
        .iter()
        .map(|(addr, gsi)| aml::Package::new(vec![addr, &zero, &zero, gsi]))
        .collect();
    let prt_refs: Vec<&dyn Aml> = prt_items.iter().map(|p| p as &dyn Aml).collect();
    let prt = aml::Package::new(prt_refs);

    let eisa_pnp0a08 = aml::EISAName::new("PNP0A08");
    let eisa_pnp0a03 = aml::EISAName::new("PNP0A03");
    let name_hid = aml::Name::new("_HID".into(), &eisa_pnp0a08);
    let name_cid = aml::Name::new("_CID".into(), &eisa_pnp0a03);
    let name_adr = aml::Name::new("_ADR".into(), &aml::ZERO);
    let name_uid = aml::Name::new("_UID".into(), &aml::ZERO);
    let name_crs = aml::Name::new("_CRS".into(), &crs);
    let name_prt = aml::Name::new("_PRT".into(), &prt);

    let pci0_children: Vec<&dyn Aml> = vec![
        &name_hid, &name_cid, &name_adr, &name_uid, &name_crs, &name_prt,
    ];
    let pci0_dev = aml::Device::new("PCI0".into(), pci0_children);

    // A motherboard resource device claiming the ECAM window. Linux accepts
    // either this or an e820 reservation as proof the window is real; we
    // provide both.
    let ecam_mem = aml::Memory32Fixed::new(
        true,
        crate::layout::MMCONFIG_START as u32,
        crate::layout::MMCONFIG_SIZE as u32,
    );
    // Claim the sleep and reset registers too, so nothing else is handed them.
    let sleep_io = aml::IO::new(
        crate::power::SLEEP_CONTROL_PORT,
        crate::power::SLEEP_CONTROL_PORT,
        1,
        2,
    );
    let reset_io = aml::IO::new(crate::power::RESET_PORT, crate::power::RESET_PORT, 1, 1);
    let ecam_crs = aml::ResourceTemplate::new(vec![&ecam_mem, &sleep_io, &reset_io]);
    let eisa_pnp0c02 = aml::EISAName::new("PNP0C02");
    let mbrd_hid = aml::Name::new("_HID".into(), &eisa_pnp0c02);
    let mbrd_uid = aml::Name::new("_UID".into(), &aml::ZERO);
    let mbrd_crs = aml::Name::new("_CRS".into(), &ecam_crs);
    let mbrd_children: Vec<&dyn Aml> = vec![&mbrd_hid, &mbrd_uid, &mbrd_crs];
    let mbrd_dev = aml::Device::new("MBRD".into(), mbrd_children);

    let sb_scope = aml::Scope::new("_SB_".into(), vec![&pci0_dev, &mbrd_dev]);
    let mut aml_bytes = Vec::new();
    sb_scope.to_aml_bytes(&mut aml_bytes);

    // \_S5: the sleep type to write for soft off. Linux refuses to power off
    // without it, however well the FADT describes the registers.
    let s5_typ = crate::power::SLP_TYP_S5;
    let s5_package = aml::Package::new(vec![&s5_typ, &s5_typ]);
    let s5 = aml::Name::new("_S5_".into(), &s5_package);
    s5.to_aml_bytes(&mut aml_bytes);

    dsdt.append_slice(aml_bytes.as_slice());
    dsdt.as_slice().to_vec()
}

/// Write a Generic Address Structure describing a byte-wide system I/O port.
fn write_io_gas(fadt: &mut Sdt, offset: usize, port: u16) {
    fadt.write_u8(offset, 1); // address space: system I/O
    fadt.write_u8(offset + 1, 8); // register bit width
    fadt.write_u8(offset + 2, 0); // register bit offset
    fadt.write_u8(offset + 3, 1); // access size: byte
    fadt.write_u64(offset + 4, port as u64);
}

fn build_fadt(dsdt_addr: u64) -> Vec<u8> {
    const FLAG_RESET_REG_SUP: u32 = 1 << 10;
    const FLAG_HW_REDUCED_ACPI: u32 = 1 << 20;

    // FADT revision 6 field offsets.
    const OFF_FLAGS: usize = 112;
    const OFF_RESET_REG: usize = 116;
    const OFF_RESET_VALUE: usize = 128;
    const OFF_MINOR_VERSION: usize = 131;
    const OFF_X_DSDT: usize = 140;
    const OFF_SLEEP_CONTROL_REG: usize = 244;
    const OFF_SLEEP_STATUS_REG: usize = 256;

    let mut fadt = Sdt::new(*b"FACP", 276, 6, *b"NESTRI", *b"FACP    ", 1);
    fadt.write_u32(OFF_FLAGS, FLAG_HW_REDUCED_ACPI | FLAG_RESET_REG_SUP);
    fadt.write_u8(OFF_MINOR_VERSION, 1);
    fadt.write_u64(OFF_X_DSDT, dsdt_addr);

    // Without these a hardware-reduced guest has no way to power off or
    // reboot: it halts instead and the VM hangs.
    write_io_gas(&mut fadt, OFF_RESET_REG, crate::power::RESET_PORT);
    fadt.write_u8(OFF_RESET_VALUE, crate::power::RESET_VALUE);
    write_io_gas(
        &mut fadt,
        OFF_SLEEP_CONTROL_REG,
        crate::power::SLEEP_CONTROL_PORT,
    );
    write_io_gas(
        &mut fadt,
        OFF_SLEEP_STATUS_REG,
        crate::power::SLEEP_STATUS_PORT,
    );

    fadt.as_slice().to_vec()
}

fn build_madt(vcpu_count: u8, ioapic_id: u8, ioapic_addr: u32, gsi_base: u32) -> Vec<u8> {
    let mut madt = Sdt::new(*b"APIC", 44, 5, *b"NESTRI", *b"APIC    ", 1);
    madt.write_u32(36, 0xFEE0_0000);
    madt.write_u32(40, 1); // PCAT_COMPAT

    for cpu_id in 0..vcpu_count {
        let mut entry = vec![0u8; 16];
        entry[0] = 9; // type = x2APIC
        entry[1] = 16; // length
        entry[4..8].copy_from_slice(&(cpu_id as u32).to_le_bytes()); // x2APIC ID
        entry[8..12].copy_from_slice(&1u32.to_le_bytes()); // enabled
        entry[12..16].copy_from_slice(&(cpu_id as u32).to_le_bytes()); // ACPI UID
        madt.append_slice(&entry);
    }

    let mut ioapic = vec![0u8; 12];
    ioapic[0] = 1;
    ioapic[1] = 12;
    ioapic[2] = ioapic_id;
    ioapic[4..8].copy_from_slice(&ioapic_addr.to_le_bytes());
    ioapic[8..12].copy_from_slice(&gsi_base.to_le_bytes());
    madt.append_slice(&ioapic);

    for (irq, gsi, flags) in [(0u8, 2u32, 0u16), (9u8, 9u32, 0x000Fu16)] {
        let mut iso = vec![0u8; 10];
        iso[0] = 2;
        iso[1] = 10;
        iso[2] = 0;
        iso[3] = irq;
        iso[4..8].copy_from_slice(&gsi.to_le_bytes());
        iso[8..10].copy_from_slice(&flags.to_le_bytes());
        madt.append_slice(&iso);
    }

    madt.as_slice().to_vec()
}

fn build_mcfg() -> Vec<u8> {
    let mut mcfg = Sdt::new(*b"MCFG", 60, 1, *b"NESTRI", *b"MCFG    ", 1);
    mcfg.write_u64(44, crate::layout::MMCONFIG_START); // MMCONFIG base
    mcfg.write_u16(52, 0); // segment 0
    mcfg.write_u16(54, 0); // start bus = 0, end bus = 0
    mcfg.as_slice().to_vec()
}

fn build_xsdt(table_addrs: &[u64]) -> Vec<u8> {
    let len = 36 + (table_addrs.len() * 8) as u32;
    let mut xsdt = Sdt::new(*b"XSDT", len, 1, *b"NESTRI", *b"XSDT    ", 1);
    for (i, addr) in table_addrs.iter().enumerate() {
        xsdt.write_u64(36 + (i * 8), *addr);
    }
    xsdt.as_slice().to_vec()
}

fn build_rsdp(xsdt_addr: u64) -> Vec<u8> {
    let rsdp = Rsdp::new(*b"NESTRI", xsdt_addr);
    rsdp.as_bytes().to_vec()
}

pub fn setup_acpi(
    mem: &GuestMemoryMmap,
    vcpu_count: u8,
    start_addr: GuestAddress,
) -> Result<GuestAddress> {
    let dsdt = build_dsdt();
    let dsdt_addr = start_addr.0;

    let fadt = build_fadt(dsdt_addr);
    let fadt_addr = dsdt_addr + dsdt.len() as u64;

    let madt = build_madt(vcpu_count, 0, 0xFEC0_0000, 0);
    let madt_addr = fadt_addr + fadt.len() as u64;

    let mcfg = build_mcfg();
    let mcfg_addr = madt_addr + madt.len() as u64;

    let xsdt = build_xsdt(&[fadt_addr, madt_addr, mcfg_addr]);
    let xsdt_addr = mcfg_addr + mcfg.len() as u64;

    let rsdp = build_rsdp(xsdt_addr);

    let mut cur = start_addr;
    mem.write_slice(&dsdt, cur).context("Failed to write DSDT")?;
    cur = GuestAddress(cur.0 + dsdt.len() as u64);
    mem.write_slice(&fadt, cur).context("Failed to write FADT")?;
    cur = GuestAddress(cur.0 + fadt.len() as u64);
    mem.write_slice(&madt, cur).context("Failed to write MADT")?;
    cur = GuestAddress(cur.0 + madt.len() as u64);
    mem.write_slice(&mcfg, cur).context("Failed to write MCFG")?;
    cur = GuestAddress(cur.0 + mcfg.len() as u64);
    mem.write_slice(&xsdt, cur).context("Failed to write XSDT")?;
    cur = GuestAddress(cur.0 + xsdt.len() as u64);
    mem.write_slice(&rsdp, cur).context("Failed to write RSDP")?;

    Ok(GuestAddress(xsdt_addr + xsdt.len() as u64))
}
