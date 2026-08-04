//! Guest physical address space layout.
//!
//! ```text
//!   0x0000_0000 ─┬─ low RAM (also holds GDT, IDT, zero page, page tables)
//!                │
//!   0xC000_0000 ─┼─ PCI MMIO window   (BARs; matches the _CRS in the DSDT)
//!   0xE000_0000 ─┼─ ECAM / MMCONFIG   (matches the MCFG table)
//!   0xE010_0000 ─┼─ (unused)
//!   0xFEC0_0000 ─┼─ IOAPIC
//!   0xFEE0_0000 ─┼─ LAPIC
//!   0x1_0000_0000┴─ high RAM (everything above 4 GiB, if any)
//! ```
//!
//! Guest RAM is split around the 3–4 GiB hole so that a VM with more than
//! 3 GiB of memory does not overlap device MMIO.

use vm_memory::GuestAddress;

/// Start of the 3–4 GiB device hole; end of low RAM.
pub const MMIO_HOLE_START: u64 = 0xC000_0000;
/// End of the device hole; start of high RAM.
pub const MMIO_HOLE_END: u64 = 0x1_0000_0000;

/// PCI BAR allocation window, 3.0–3.5 GiB.
pub const PCI_MMIO_START: u64 = 0xC000_0000;
pub const PCI_MMIO_END: u64 = 0xE000_0000;

/// ECAM base. One bus is 1 MiB of config space.
pub const MMCONFIG_START: u64 = 0xE000_0000;
pub const MMCONFIG_BUS_COUNT: u64 = 1;
pub const MMCONFIG_SIZE: u64 = MMCONFIG_BUS_COUNT * 0x10_0000;

pub const IOAPIC_START: u64 = 0xFEC0_0000;
pub const LAPIC_START: u64 = 0xFEE0_0000;

/// ACPI tables live at the top of low RAM.
pub const ACPI_SIZE: u64 = 0x1_0000;

/// A region of guest RAM: (start, size in bytes).
pub type RamRegion = (GuestAddress, usize);

/// Split `mem_size` bytes of RAM around the device hole.
///
/// Returns one region for guests of 3 GiB or less, two otherwise.
pub fn ram_regions(mem_size: u64) -> Vec<RamRegion> {
    if mem_size <= MMIO_HOLE_START {
        vec![(GuestAddress(0), mem_size as usize)]
    } else {
        vec![
            (GuestAddress(0), MMIO_HOLE_START as usize),
            (
                GuestAddress(MMIO_HOLE_END),
                (mem_size - MMIO_HOLE_START) as usize,
            ),
        ]
    }
}

/// Base address of the ACPI table block: the top of low RAM.
pub fn acpi_start(mem_size: u64) -> GuestAddress {
    let low_ram_end = mem_size.min(MMIO_HOLE_START);
    GuestAddress(low_ram_end - ACPI_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_guest_is_one_region() {
        let r = ram_regions(1024 << 20);
        assert_eq!(r, vec![(GuestAddress(0), 1024 << 20)]);
        assert_eq!(acpi_start(1024 << 20), GuestAddress((1024 << 20) - 0x1_0000));
    }

    #[test]
    fn large_guest_straddles_the_hole() {
        let r = ram_regions(8192 << 20);
        assert_eq!(
            r,
            vec![
                (GuestAddress(0), 0xC000_0000),
                (GuestAddress(0x1_0000_0000), (8192 << 20) - 0xC000_0000),
            ]
        );
        // ACPI must stay inside low RAM, never in the hole.
        assert_eq!(acpi_start(8192 << 20), GuestAddress(0xC000_0000 - 0x1_0000));
    }

    #[test]
    fn exactly_three_gib_does_not_split() {
        assert_eq!(ram_regions(MMIO_HOLE_START).len(), 1);
        assert_eq!(ram_regions(MMIO_HOLE_START + 1).len(), 2);
    }

    #[test]
    fn windows_do_not_overlap() {
        assert!(PCI_MMIO_END <= MMCONFIG_START);
        assert!(MMCONFIG_START + MMCONFIG_SIZE <= IOAPIC_START);
        assert!(IOAPIC_START < LAPIC_START);
        assert!(LAPIC_START < MMIO_HOLE_END);
    }
}
