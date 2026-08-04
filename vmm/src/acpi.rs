use acpi_tables::{Aml, aml, rsdp::Rsdp, sdt::Sdt};
use anyhow::{Context, Result};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use zerocopy::IntoBytes;

/// GSI a PCI slot's INTx line is wired to.
///
/// INTx is the fallback path — a device that negotiates MSI-X never uses it.
/// Slot 1 gets its own line; everything else shares one, which is legal
/// because INTx is level-triggered and shareable.
fn slot_gsi(slot: u32) -> u32 {
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
    let sb_scope = aml::Scope::new("_SB_".into(), vec![&pci0_dev]);
    let mut aml_bytes = Vec::new();
    sb_scope.to_aml_bytes(&mut aml_bytes);

    dsdt.append_slice(aml_bytes.as_slice());
    dsdt.as_slice().to_vec()
}

fn build_fadt(dsdt_addr: u64) -> Vec<u8> {
    let mut fadt = Sdt::new(*b"FACP", 276, 6, *b"NESTRI", *b"FACP    ", 1);
    fadt.write_u32(112, 1 << 20); // flags: HW_REDUCED_ACPI
    fadt.write_u64(140, dsdt_addr); // DSDT address
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
    mcfg.write_u64(44, 0xE000_0000); // MMCONFIG base
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
