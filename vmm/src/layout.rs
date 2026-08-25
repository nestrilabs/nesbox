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
//!   0x1_0000_0000├─ high RAM (everything above 4 GiB, if any)
//!                │
//! 0x100_0000_0000┴─ 64-bit PCI MMIO window (large prefetchable BARs)
//! ```
//!
//! Guest RAM is split around the 3–4 GiB hole so that a VM with more than
//! 3 GiB of memory does not overlap device MMIO.

use vm_memory::{Address, GuestAddress};

/// Start of the 3–4 GiB device hole; end of low RAM.
pub const MMIO_HOLE_START: u64 = 0xC000_0000;
/// End of the device hole; start of high RAM.
pub const MMIO_HOLE_END: u64 = 0x1_0000_0000;

/// PCI BAR allocation window, 3.0–3.5 GiB.
pub const PCI_MMIO_START: u64 = 0xC000_0000;
pub const PCI_MMIO_END: u64 = 0xE000_0000;

/// Largest 64-bit MMIO window we will hand out, if the address space allows.
const PCI_MMIO64_MAX_SIZE: u64 = 0x10_0000_0000; // 64 GiB

/// Where 64-bit prefetchable BARs live: at the very top of what the CPU can
/// address. A GPU's host-visible memory does not fit in the 512 MiB below
/// 4 GiB, so large BARs go up here instead.
///
/// This *must* be derived from the guest CPU's physical address width rather
/// than fixed. Linux silently discards a host bridge window it cannot address,
/// and then refuses to assign the BAR — with no error naming the cause. Many
/// desktop Intel parts report only 39 bits, so anything at, say, 1 TiB simply
/// vanishes on them while working on a 46-bit server chip.
///
/// The window takes at most a quarter of the address space so guest RAM has
/// somewhere to live; [`Mmio64Window::fits_above`] is the check that it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mmio64Window {
    pub start: u64,
    pub size: u64,
}

impl Mmio64Window {
    pub fn end(&self) -> u64 {
        self.start + self.size
    }

    /// Is there room for `ram_top` bytes of guest RAM below this window?
    pub fn fits_above(&self, ram_top: u64) -> bool {
        ram_top <= self.start
    }
}

/// Choose the 64-bit MMIO window for a CPU with `phys_bits` of physical
/// address space.
pub fn mmio64_window(phys_bits: u8) -> Mmio64Window {
    // Below 32 bits there is no space above 4 GiB at all; clamp so the
    // arithmetic stays sane and let the caller's RAM check reject it.
    let phys_bits = phys_bits.clamp(32, 63);
    let addressable = 1u64 << phys_bits;
    let size = PCI_MMIO64_MAX_SIZE.min(addressable / 4);
    Mmio64Window {
        start: addressable - size,
        size,
    }
}

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

/// Highest guest physical address backed by RAM, given `mem_size` bytes of it.
pub fn ram_top(mem_size: u64) -> u64 {
    match ram_regions(mem_size).last() {
        Some(&(start, size)) => start.raw_value() + size as u64,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_guest_is_one_region() {
        let r = ram_regions(1024 << 20);
        assert_eq!(r, vec![(GuestAddress(0), 1024 << 20)]);
        assert_eq!(
            acpi_start(1024 << 20),
            GuestAddress((1024 << 20) - 0x1_0000)
        );
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

    #[test]
    fn the_64_bit_window_stays_inside_the_address_space() {
        // 39 bits is what many desktop Intel parts report, and the case that
        // exposed this: a window above the limit is silently dropped by Linux.
        for bits in [36u8, 39, 46, 48] {
            let w = mmio64_window(bits);
            assert!(w.end() <= 1u64 << bits, "{bits}-bit window overflows");
            assert!(w.start >= MMIO_HOLE_END, "{bits}-bit window is below 4 GiB");
        }
    }

    #[test]
    fn a_realistic_guest_fits_below_the_window() {
        // 64 GiB of RAM on the narrowest address space we support.
        assert!(mmio64_window(39).fits_above(ram_top(64 << 30)));
        // ...and a guest too large for the space is rejected rather than
        // silently overlapping.
        assert!(!mmio64_window(36).fits_above(ram_top(64 << 30)));
    }

    #[test]
    fn ram_top_follows_the_split() {
        assert_eq!(ram_top(1024 << 20), 1024 << 20);
        // Past the hole, RAM resumes at 4 GiB, so the top is higher than the
        // size alone would suggest.
        assert_eq!(
            ram_top(8192 << 20),
            MMIO_HOLE_END + (8192 << 20) - MMIO_HOLE_START
        );
    }
}
