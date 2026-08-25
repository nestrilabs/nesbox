//! PCI bus emulation with Configuration Access Mechanism 1 (CAM1)
//!
//! The x86 kernel discovers PCI devices by reading config space through two
//! I/O ports:
//!
//!   0xCF8  CONFIG_ADDRESS  (32-bit, write)
//!     bit 31    : enable bit (must be 1)
//!     bits 23:16: bus number
//!     bits 15:11: device number
//!     bits 10:8 : function number
//!     bits 7:2  : register (DWORD) offset
//!     bits 1:0  : zero (DWORD-aligned)
//!
//!   0xCFC  CONFIG_DATA  (8/16/32-bit read/write)
//!
//! The same config space is also reachable through ECAM (PCIe "enhanced"
//! config access), an MMIO window where the address encodes the device:
//!
//!   addr = ECAM_BASE | bus << 20 | device << 15 | function << 12 | register
//!
//! Both paths land in the same `config_read`/`config_write`. Which one the
//! guest uses is its own choice; Linux prefers ECAM when the firmware both
//! advertises it in MCFG and reserves the window.
//!
//! BAR handling
//! ------------
//! The bus pre-assigns a base address to every BAR at `add_device` time.
//! Config reads for BAR registers (offsets 0x10–0x24) return the assigned
//! address ORed with the type bits describing what kind of BAR it is.
//! The kernel probes BAR *size* by writing 0xFFFFFFFF and reading back; we
//! intercept that write, notice it, and on the next read return the size
//! mask instead of the base address. After the kernel has finished probing
//! it writes the actual base address back, which we accept as long as it
//! stays inside the window we decode.
//!
//! There are two such windows. Small BARs go below 4 GiB, where space is
//! scarce — half a gigabyte shared by every device. A [`BarType::Mem64`] BAR
//! goes above all guest RAM instead, and occupies *two* consecutive config
//! registers: the low half carries the type bits, the high half is address
//! only. The guest sizes and programs the two halves in separate writes, so
//! neither can be interpreted on its own.

pub mod config;
pub mod msi;

pub use msi::{MsiRouter, MsiVector};

use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

/// The window 32-bit BARs are allocated from. Must match both the `_CRS` of
/// PCI0 in the DSDT and `layout::PCI_MMIO_{START,END}` in the VMM, or the guest
/// will reassign BARs somewhere we do not decode.
pub const MMIO_WINDOW_START: u64 = 0xC000_0000;
pub const MMIO_WINDOW_END: u64 = 0xE000_0000;

/// The window 64-bit BARs are allocated from, above all guest RAM.
///
/// Unlike the 32-bit window this cannot be a constant: it depends on how much
/// physical address space the guest CPU has, so the VMM works it out and hands
/// it to [`Bus::new`]. It must match the second memory range in PCI0's `_CRS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mmio64Window {
    pub start: u64,
    pub end: u64,
}

impl Mmio64Window {
    pub fn new(start: u64, size: u64) -> Self {
        Self {
            start,
            end: start + size,
        }
    }
}

/// What kind of memory BAR a device wants.
///
/// The 512 MiB below 4 GiB is scarce and shared by every device, so anything
/// large — a GPU's host-visible memory, say — asks for [`BarType::Mem64`] and
/// is placed above RAM instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BarType {
    /// 32-bit, non-prefetchable. One config register.
    #[default]
    Mem32,
    /// 64-bit, prefetchable. Occupies *two* consecutive config registers, so
    /// the following BAR index is unusable.
    Mem64,
}

impl BarType {
    /// The low four bits a config read returns alongside the base address.
    fn type_bits(self) -> u32 {
        match self {
            // bit 0 = 0 memory, bits 2:1 = 00 32-bit, bit 3 = 0 non-prefetchable
            Self::Mem32 => 0x0,
            // bits 2:1 = 10 64-bit, bit 3 = 1 prefetchable
            Self::Mem64 => 0x4 | 0x8,
        }
    }

    fn is_64bit(self) -> bool {
        matches!(self, Self::Mem64)
    }
}

/// ECAM window. Must match the MCFG table and the reservation the guest is
/// given, or Linux advertises it and then refuses to use it.
pub const ECAM_BASE: u64 = 0xE000_0000;
/// One bus worth of config space: 32 devices x 8 functions x 4 KiB.
pub const ECAM_SIZE: u64 = 0x10_0000;
/// Config space per function under ECAM, of which we implement the first 256
/// bytes; the extended area reads as zero.
const ECAM_FUNCTION_SIZE: u64 = 0x1000;

pub trait PciDevice: Send + Sync {
    /// Read from PCI configuration space. `offset` is byte offset, `data`
    /// is 1, 2 or 4 bytes wide.
    fn read_config(&self, offset: u32, data: &mut [u8]);
    /// Write to PCI configuration space.
    fn write_config(&self, offset: u32, data: &[u8]);
    /// Read from a memory BAR region. Returns false if the BAR index is unused.
    fn read_bar(&self, bar_idx: usize, offset: u64, data: &mut [u8]) -> bool;
    /// Write to a memory BAR region.
    fn write_bar(&self, bar_idx: usize, offset: u64, data: &[u8]) -> bool;
    /// Return the size of BAR `bar_idx`, or 0 if unused.
    fn bar_size(&self, bar_idx: usize) -> u64;
    /// What kind of BAR `bar_idx` is. Devices that only need a small register
    /// window can leave this alone.
    fn bar_type(&self, _bar_idx: usize) -> BarType {
        BarType::Mem32
    }
}

// ── Internal book-keeping ─────────────────────────────────────────────────────

struct Bar {
    bar_idx: usize,
    addr: u64, // assigned base address (always page/size aligned)
    size: u64, // power-of-two size
    bar_type: BarType,
    /// True between a 0xFFFFFFFF write and the next read. A 64-bit BAR is
    /// probed one register at a time, so this is tracked per half.
    probing_lo: bool,
    probing_hi: bool,
}

struct PciDeviceEntry {
    device: Arc<dyn PciDevice>,
    bars: Vec<Bar>,
}

// ── Bus ───────────────────────────────────────────────────────────────────────

pub struct Bus {
    devices: RwLock<HashMap<(u8, u8, u8), PciDeviceEntry>>,
    /// Next BDF to hand out.
    next_bdf: Mutex<(u8, u8, u8)>,
    /// CAM1 CONFIG_ADDRESS register (last value written to port 0xCF8).
    config_addr: Mutex<u32>,
    /// Flat MMIO region index: (base, size, bdf, bar_idx).
    mmio_regions: RwLock<Vec<(u64, u64, (u8, u8, u8), usize)>>,
    /// Where 64-bit BARs are placed. Depends on the guest CPU's address width.
    mmio64: Mmio64Window,
}

impl Bus {
    pub fn new(mmio64: Mmio64Window) -> Self {
        Self {
            devices: RwLock::new(HashMap::new()),
            next_bdf: Mutex::new((0, 1, 0)), // device 0 = host bridge (synthetic, handled inline)
            config_addr: Mutex::new(0),
            mmio_regions: RwLock::new(Vec::new()),
            mmio64,
        }
    }

    /// Register a PCI device and assign BAR addresses.  Returns the BDF.
    pub fn add_device(&self, device: impl PciDevice + 'static) -> Result<(u8, u8, u8)> {
        self.add_device_arc(Arc::new(device))
    }

    /// Where a device's BAR was placed, once it has been added. A device that
    /// has to tell the guest about its own BAR — a virtio shared-memory window,
    /// say — asks here rather than predicting what the allocator will do.
    pub fn bar_address(&self, bdf: (u8, u8, u8), bar_idx: usize) -> Option<u64> {
        let devices = self.devices.read().unwrap();
        devices
            .get(&bdf)?
            .bars
            .iter()
            .find(|b| b.bar_idx == bar_idx)
            .map(|b| b.addr)
    }

    /// As [`Bus::add_device`], but keeps the caller's handle to the device.
    pub fn add_device_arc(&self, device: Arc<dyn PciDevice>) -> Result<(u8, u8, u8)> {
        let mut next_bdf = self.next_bdf.lock().unwrap();
        let (bus, dev, func) = *next_bdf;
        if bus > 0 || dev > 31 {
            anyhow::bail!("PCI bus full");
        }

        // Assign BAR addresses with a bump allocator over each MMIO window.
        // Find the current high-water mark of each so devices added later do
        // not overlap.
        let (hwm32, hwm64) = {
            let regions = self.mmio_regions.read().unwrap();
            let top_of = |window_start: u64, window_end: u64| {
                regions
                    .iter()
                    .filter(|(base, _, _, _)| (window_start..window_end).contains(base))
                    .map(|(base, size, _, _)| base + size)
                    .max()
                    .unwrap_or(window_start)
            };
            (
                top_of(MMIO_WINDOW_START, MMIO_WINDOW_END),
                top_of(self.mmio64.start, self.mmio64.end),
            )
        };
        let mut next32 = hwm32.max(MMIO_WINDOW_START);
        let mut next64 = hwm64.max(self.mmio64.start);

        let mut bars: Vec<Bar> = Vec::new();
        let mut bar_idx = 0;
        while bar_idx < 6 {
            let size = device.bar_size(bar_idx);
            if size == 0 {
                bar_idx += 1;
                continue;
            }
            if !size.is_power_of_two() {
                anyhow::bail!("BAR {} size {:#x} is not a power of two", bar_idx, size);
            }
            let bar_type = device.bar_type(bar_idx);
            let (next_addr, window_end) = match bar_type {
                BarType::Mem32 => (&mut next32, MMIO_WINDOW_END),
                BarType::Mem64 => (&mut next64, self.mmio64.end),
            };
            // Align up to `size` (which must be a power of two).
            let aligned = (*next_addr + size - 1) & !(size - 1);
            if aligned + size > window_end {
                anyhow::bail!(
                    "PCI MMIO window exhausted: BAR {} of {:#x} bytes will not fit below {:#x}",
                    bar_idx,
                    size,
                    window_end
                );
            }
            if bar_type.is_64bit() && bar_idx == 5 {
                anyhow::bail!("BAR 5 cannot be 64-bit: there is no register after it");
            }
            *next_addr = aligned + size;
            bars.push(Bar {
                bar_idx,
                addr: aligned,
                size,
                bar_type,
                probing_lo: false,
                probing_hi: false,
            });
            // A 64-bit BAR consumes the following register as its high half.
            bar_idx += if bar_type.is_64bit() { 2 } else { 1 };
        }

        let bdf = (bus, dev, func);
        {
            let mut devices = self.devices.write().unwrap();
            devices.insert(bdf, PciDeviceEntry { device, bars });
        }

        // Advance: always assign distinct device numbers
        *next_bdf = if dev < 31 {
            (bus, dev + 1, 0)
        } else {
            (bus + 1, 0, 0)
        };
        drop(next_bdf);

        // Register MMIO regions for dispatch.
        {
            let devices = self.devices.read().unwrap();
            if let Some(entry) = devices.get(&bdf) {
                let mut mmio = self.mmio_regions.write().unwrap();
                for bar in &entry.bars {
                    mmio.push((bar.addr, bar.size, bdf, bar.bar_idx));
                }
            }
        }

        Ok(bdf)
    }

    // ── CAM1 PIO ─────────────────────────────────────────────────────────────

    /// Handle a write to ports 0xCF8–0xCFB (CONFIG_ADDRESS) or
    /// 0xCFC–0xCFF (CONFIG_DATA).
    pub fn handle_pio_write(&self, port: u16, data: &[u8]) -> bool {
        match port {
            // CONFIG_ADDRESS: 32-bit write
            0xCF8..=0xCFB => {
                if data.len() == 4 {
                    let val = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                    *self.config_addr.lock().unwrap() = val;
                }
                true
            }
            // CONFIG_DATA: write to device config space
            0xCFC..=0xCFF => {
                let addr = *self.config_addr.lock().unwrap();
                if addr & 0x8000_0000 == 0 {
                    return true; // enable bit not set
                }
                let bus = ((addr >> 16) & 0xFF) as u8;
                let dev = ((addr >> 11) & 0x1F) as u8;
                let func = ((addr >> 8) & 0x07) as u8;
                // DWORD-aligned register offset, then add intra-DWORD byte lane.
                let byte_offset = (port - 0xCFC) as u32;
                let reg = (addr & 0xFC) + byte_offset;

                self.config_write((bus, dev, func), reg, data);
                true
            }
            _ => false,
        }
    }

    /// Handle a read from ports 0xCF8–0xCFB or 0xCFC–0xCFF.
    pub fn handle_pio_read(&self, port: u16, data: &mut [u8]) -> bool {
        match port {
            0xCF8..=0xCFB => {
                let val = *self.config_addr.lock().unwrap();
                let bytes = val.to_le_bytes();
                let off = (port - 0xCF8) as usize;
                for (i, b) in data.iter_mut().enumerate() {
                    *b = bytes.get(off + i).copied().unwrap_or(0);
                }
                true
            }
            0xCFC..=0xCFF => {
                let addr = *self.config_addr.lock().unwrap();
                if addr & 0x8000_0000 == 0 {
                    data.fill(0xFF);
                    return true;
                }
                let bus = ((addr >> 16) & 0xFF) as u8;
                let dev = ((addr >> 11) & 0x1F) as u8;
                let func = ((addr >> 8) & 0x07) as u8;
                let byte_offset = (port - 0xCFC) as u32;
                let reg = (addr & 0xFC) + byte_offset;

                if !self.config_read((bus, dev, func), reg, data) {
                    data.fill(0xFF); // no device → return 0xFF (all-ones)
                }
                true
            }
            _ => false,
        }
    }

    // ── Config space read/write with BAR intercept ────────────────────────────

    /// Returns true if a device was found, false otherwise.
    fn config_read(&self, bdf: (u8, u8, u8), offset: u32, data: &mut [u8]) -> bool {
        // Synthesize a minimal host bridge at 0:0:0.
        if bdf == (0, 0, 0) {
            let mut cfg = [0u8; 64];
            cfg[0x00..0x02].copy_from_slice(&0x8086u16.to_le_bytes()); // Intel vendor
            cfg[0x02..0x04].copy_from_slice(&0x1237u16.to_le_bytes()); // 440FX host bridge
            cfg[0x04..0x06].copy_from_slice(&0x0006u16.to_le_bytes()); // command: mem+master
            cfg[0x06..0x08].copy_from_slice(&0x0000u16.to_le_bytes()); // status
            cfg[0x08] = 0x02; // revision
            cfg[0x09] = 0x00; // prog-if
            cfg[0x0a] = 0x00; // subclass: host bridge
            cfg[0x0b] = 0x06; // class: bridge
            cfg[0x0e] = 0x00; // header type 0
            let s = offset as usize;
            let e = (s + data.len()).min(cfg.len());
            if s < cfg.len() {
                let n = e - s;
                data[..n].copy_from_slice(&cfg[s..e]);
                data[n..].fill(0);
            } else {
                data.fill(0);
            }
            return true;
        }

        // BAR registers live at config offsets 0x10–0x24 (6 × 4 bytes).
        let bar_slot = Self::bar_slot_for_offset(offset, data.len());

        let mut devices = self.devices.write().unwrap();
        let entry = match devices.get_mut(&bdf) {
            Some(e) => e,
            None => return false,
        };

        if let Some(slot) = bar_slot {
            // Find the Bar struct for this slot, which may be the high half of
            // a 64-bit BAR declared one register earlier.
            if let Some((bar, is_high)) = Self::find_bar(&mut entry.bars, slot) {
                // The size mask spans both registers of a 64-bit BAR.
                let mask = !(bar.size - 1);
                let val: u32 = if is_high {
                    if bar.probing_hi {
                        bar.probing_hi = false;
                        (mask >> 32) as u32
                    } else {
                        (bar.addr >> 32) as u32
                    }
                } else if bar.probing_lo {
                    bar.probing_lo = false;
                    // The low four bits read back as zero while sizing.
                    (mask as u32) & !0xF
                } else {
                    (bar.addr as u32) | bar.bar_type.type_bits()
                };
                let bytes = val.to_le_bytes();
                // The offset within the DWORD.
                let intra = (offset as usize) & 3;
                for (i, b) in data.iter_mut().enumerate() {
                    *b = bytes.get(intra + i).copied().unwrap_or(0);
                }
                return true;
            }
            // BAR slot exists but no Bar struct → unused BAR, return 0.
            data.fill(0);
            return true;
        }

        // Not a BAR register — delegate to the device.
        entry.device.read_config(offset, data);
        true
    }

    fn config_write(&self, bdf: (u8, u8, u8), offset: u32, data: &[u8]) {
        let bar_slot = Self::bar_slot_for_offset(offset, data.len());

        if let Some(slot) = bar_slot {
            // Handle BAR write without holding devices lock while touching mmio_regions,
            // to keep a consistent lock order (always: mmio_regions before devices, or
            // acquire them separately, never nest in opposite order).
            let (moved_to, owning_slot) = {
                let mut devices = self.devices.write().unwrap();
                let entry = match devices.get_mut(&bdf) {
                    Some(e) => e,
                    None => return,
                };
                let Some((bar, is_high)) = Self::find_bar(&mut entry.bars, slot) else {
                    return;
                };
                if data.len() < 4 {
                    // Sizing and assignment are always full-DWORD writes; a
                    // narrower one cannot mean either.
                    return;
                }
                let raw = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                if raw == 0xFFFF_FFFF {
                    if is_high {
                        bar.probing_hi = true;
                    } else {
                        bar.probing_lo = true;
                    }
                    return;
                }

                // Assemble the address the guest is asking for, one half at a
                // time — it programs the two registers of a 64-bit BAR in
                // separate writes, so the other half must be preserved.
                let requested = if is_high {
                    (bar.addr & 0xFFFF_FFFF) | ((raw as u64) << 32)
                } else {
                    (bar.addr & !0xFFFF_FFFFu64) | (raw as u64 & !0xF)
                } & !(bar.size - 1);

                // Accept the move only if it stays inside the window we
                // decode. Anything else would silently stop the device
                // responding, so keep our own assignment and say so.
                let window = match bar.bar_type {
                    BarType::Mem32 => MMIO_WINDOW_START..MMIO_WINDOW_END,
                    BarType::Mem64 => self.mmio64.start..self.mmio64.end,
                };
                if !window.contains(&requested) {
                    // Mid-programming a 64-bit BAR one half is still stale, so
                    // this is expected and not worth warning about.
                    log::trace!(
                        "{:02x}:{:02x}.{} BAR {} write {:#x} lands outside {:#x}..{:#x}, keeping {:#x}",
                        bdf.0,
                        bdf.1,
                        bdf.2,
                        slot,
                        requested,
                        window.start,
                        window.end,
                        bar.addr
                    );
                    return;
                }
                if requested == bar.addr {
                    return;
                }
                bar.addr = requested;
                (requested, bar.bar_idx)
            }; // devices lock released here

            // Now update mmio_regions separately (no nesting).
            let mut mmio = self.mmio_regions.write().unwrap();
            for region in mmio.iter_mut() {
                if region.2 == bdf && region.3 == owning_slot {
                    region.0 = moved_to;
                }
            }
            return;
        }

        // Non-BAR config write — pass through to device.
        let mut devices = self.devices.write().unwrap();
        if let Some(entry) = devices.get_mut(&bdf) {
            entry.device.write_config(offset, data);
        }
    }

    /// Find the BAR that owns config register `slot`, and whether `slot` is its
    /// high half. A 64-bit BAR answers for two consecutive registers.
    fn find_bar(bars: &mut [Bar], slot: usize) -> Option<(&mut Bar, bool)> {
        let found = bars
            .iter()
            .position(|b| b.bar_idx == slot)
            .map(|i| (i, false));
        let found = found.or_else(|| {
            bars.iter()
                .position(|b| b.bar_type.is_64bit() && b.bar_idx + 1 == slot)
                .map(|i| (i, true))
        })?;
        let (index, is_high) = found;
        Some((&mut bars[index], is_high))
    }

    /// Return the BAR slot index (0–5) if `offset`/`len` touches a BAR register,
    /// or None otherwise.
    fn bar_slot_for_offset(offset: u32, _len: usize) -> Option<usize> {
        // BAR0 = 0x10, BAR1 = 0x14, …, BAR5 = 0x24
        if offset >= 0x10 && offset <= 0x27 {
            Some(((offset - 0x10) / 4) as usize)
        } else {
            None
        }
    }

    // ── MMIO dispatch ─────────────────────────────────────────────────────────

    /// Decode an ECAM address into (bdf, register offset).
    fn ecam_decode(addr: u64) -> ((u8, u8, u8), u32) {
        let off = addr - ECAM_BASE;
        let bus = ((off >> 20) & 0xff) as u8;
        let dev = ((off >> 15) & 0x1f) as u8;
        let func = ((off >> 12) & 0x07) as u8;
        let reg = (off % ECAM_FUNCTION_SIZE) as u32;
        ((bus, dev, func), reg)
    }

    fn in_ecam(addr: u64) -> bool {
        (ECAM_BASE..ECAM_BASE + ECAM_SIZE).contains(&addr)
    }

    pub fn handle_mmio_write(&self, addr: u64, data: &[u8]) -> bool {
        if Self::in_ecam(addr) {
            let (bdf, reg) = Self::ecam_decode(addr);
            // Extended config space (0x100..0x1000) is not implemented.
            if reg < 0x100 {
                self.config_write(bdf, reg, data);
            }
            return true;
        }

        let mmio_regions = self.mmio_regions.read().unwrap();
        for &(base, size, bdf, bar_idx) in mmio_regions.iter() {
            if addr >= base && addr < base + size {
                let offset = addr - base;
                let devices = self.devices.read().unwrap();
                if let Some(entry) = devices.get(&bdf) {
                    return entry.device.write_bar(bar_idx, offset, data);
                }
            }
        }
        false
    }

    pub fn handle_mmio_read(&self, addr: u64, data: &mut [u8]) -> bool {
        if Self::in_ecam(addr) {
            let (bdf, reg) = Self::ecam_decode(addr);
            log::trace!(
                "ECAM read {:02x}:{:02x}.{} reg={:#x} len={}",
                bdf.0,
                bdf.1,
                bdf.2,
                reg,
                data.len()
            );
            if reg >= 0x100 {
                // No extended capabilities: reads must return zero, not 0xFF,
                // or the guest will walk a bogus capability list.
                data.fill(0);
            } else if !self.config_read(bdf, reg, data) {
                data.fill(0xFF); // no such device
            }
            return true;
        }

        let mmio_regions = self.mmio_regions.read().unwrap();
        for &(base, size, bdf, bar_idx) in mmio_regions.iter() {
            if addr >= base && addr < base + size {
                let offset = addr - base;
                let devices = self.devices.read().unwrap();
                if let Some(entry) = devices.get(&bdf) {
                    return entry.device.read_bar(bar_idx, offset, data);
                }
            }
        }
        false
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A device with whatever BARs a test asks for.
    struct FakeDevice {
        bars: Vec<(u64, BarType)>,
    }

    impl PciDevice for FakeDevice {
        fn read_config(&self, _o: u32, d: &mut [u8]) {
            d.fill(0);
        }
        fn write_config(&self, _o: u32, _d: &[u8]) {}
        fn read_bar(&self, _i: usize, _o: u64, _d: &mut [u8]) -> bool {
            true
        }
        fn write_bar(&self, _i: usize, _o: u64, _d: &[u8]) -> bool {
            true
        }
        fn bar_size(&self, i: usize) -> u64 {
            self.bars.get(i).map(|b| b.0).unwrap_or(0)
        }
        fn bar_type(&self, i: usize) -> BarType {
            self.bars.get(i).map(|b| b.1).unwrap_or(BarType::Mem32)
        }
    }

    /// A window like a 39-bit CPU would get: 16 GiB just under 512 GiB.
    const TEST_MMIO64: Mmio64Window = Mmio64Window {
        start: 0x7C_0000_0000,
        end: 0x80_0000_0000,
    };

    fn new_bus() -> Bus {
        Bus::new(TEST_MMIO64)
    }

    fn read_reg(bus: &Bus, bdf: (u8, u8, u8), reg: u32) -> u32 {
        let mut buf = [0u8; 4];
        bus.config_read(bdf, reg, &mut buf);
        u32::from_le_bytes(buf)
    }

    fn write_reg(bus: &Bus, bdf: (u8, u8, u8), reg: u32, val: u32) {
        bus.config_write(bdf, reg, &val.to_le_bytes());
    }

    const BAR0: u32 = 0x10;
    const BAR1: u32 = 0x14;

    #[test]
    fn a_32_bit_bar_lands_in_the_low_window() {
        let bus = new_bus();
        let bdf = bus
            .add_device(FakeDevice {
                bars: vec![(0x1000, BarType::Mem32)],
            })
            .unwrap();
        let val = read_reg(&bus, bdf, BAR0);
        assert_eq!(val & 0xF, 0, "32-bit non-prefetchable has no type bits set");
        let addr = (val & !0xF) as u64;
        assert!((MMIO_WINDOW_START..MMIO_WINDOW_END).contains(&addr));
    }

    #[test]
    fn a_64_bit_bar_lands_above_ram_and_declares_itself() {
        let bus = new_bus();
        let bdf = bus
            .add_device(FakeDevice {
                bars: vec![(0x1000_0000, BarType::Mem64)],
            })
            .unwrap();
        let lo = read_reg(&bus, bdf, BAR0);
        let hi = read_reg(&bus, bdf, BAR1);
        // bit 3 prefetchable, bits 2:1 = 10 meaning 64-bit.
        assert_eq!(lo & 0xF, 0xC);
        let addr = ((hi as u64) << 32) | (lo & !0xF) as u64;
        assert_eq!(addr, TEST_MMIO64.start);
    }

    #[test]
    fn sizing_a_64_bit_bar_reports_the_whole_width() {
        let bus = new_bus();
        let size = 0x1_0000_0000u64; // 4 GiB, bigger than a 32-bit BAR can express
        let bdf = bus
            .add_device(FakeDevice {
                bars: vec![(size, BarType::Mem64)],
            })
            .unwrap();

        // Linux sizes each half in turn.
        write_reg(&bus, bdf, BAR0, 0xFFFF_FFFF);
        write_reg(&bus, bdf, BAR1, 0xFFFF_FFFF);
        let lo = read_reg(&bus, bdf, BAR0);
        let hi = read_reg(&bus, bdf, BAR1);

        let mask = ((hi as u64) << 32) | (lo & !0xF) as u64;
        assert_eq!(
            (!mask).wrapping_add(1),
            size,
            "the mask must decode back to the size"
        );
    }

    #[test]
    fn sizing_does_not_disturb_the_assigned_address() {
        let bus = new_bus();
        let bdf = bus
            .add_device(FakeDevice {
                bars: vec![(0x1000, BarType::Mem32)],
            })
            .unwrap();
        let before = read_reg(&bus, bdf, BAR0);
        write_reg(&bus, bdf, BAR0, 0xFFFF_FFFF);
        let _mask = read_reg(&bus, bdf, BAR0);
        // The mask is returned exactly once; the next read is the address again.
        assert_eq!(read_reg(&bus, bdf, BAR0), before);
    }

    #[test]
    fn the_high_half_of_a_64_bit_bar_is_not_a_separate_bar() {
        let bus = new_bus();
        // BAR0 is 64-bit, so BAR1 is its high half and BAR2 is the next usable
        // register.
        let bdf = bus
            .add_device(FakeDevice {
                bars: vec![
                    (0x1000_0000, BarType::Mem64),
                    (0, BarType::Mem32),
                    (0x1000, BarType::Mem32),
                ],
            })
            .unwrap();
        let hi = read_reg(&bus, bdf, BAR1);
        assert_eq!(hi, (TEST_MMIO64.start >> 32) as u32);
        let bar2 = read_reg(&bus, bdf, 0x18);
        assert!((MMIO_WINDOW_START..MMIO_WINDOW_END).contains(&((bar2 & !0xF) as u64)));
    }

    #[test]
    fn devices_do_not_overlap_within_a_window() {
        let bus = new_bus();
        let a = bus
            .add_device(FakeDevice {
                bars: vec![(0x1000_0000, BarType::Mem64)],
            })
            .unwrap();
        let b = bus
            .add_device(FakeDevice {
                bars: vec![(0x1000_0000, BarType::Mem64)],
            })
            .unwrap();
        let addr = |bdf| {
            let lo = read_reg(&bus, bdf, BAR0);
            let hi = read_reg(&bus, bdf, BAR1);
            ((hi as u64) << 32) | (lo & !0xF) as u64
        };
        assert!(addr(b) >= addr(a) + 0x1000_0000);
    }

    #[test]
    fn the_two_windows_are_allocated_independently() {
        let bus = new_bus();
        // A 64-bit BAR must not push the 32-bit high-water mark along, or a
        // handful of large BARs would exhaust the small window without using it.
        bus.add_device(FakeDevice {
            bars: vec![(0x8000_0000, BarType::Mem64)],
        })
        .unwrap();
        let bdf = bus
            .add_device(FakeDevice {
                bars: vec![(0x1000, BarType::Mem32)],
            })
            .unwrap();
        let addr = (read_reg(&bus, bdf, BAR0) & !0xF) as u64;
        assert_eq!(addr, MMIO_WINDOW_START);
    }

    #[test]
    fn a_bar_cannot_be_moved_outside_the_window_we_decode() {
        let bus = new_bus();
        let bdf = bus
            .add_device(FakeDevice {
                bars: vec![(0x1000, BarType::Mem32)],
            })
            .unwrap();
        let before = read_reg(&bus, bdf, BAR0);
        write_reg(&bus, bdf, BAR0, 0x1000_0000); // below the window
        assert_eq!(read_reg(&bus, bdf, BAR0), before);
    }

    #[test]
    fn a_bar_can_be_moved_within_the_window() {
        let bus = new_bus();
        let bdf = bus
            .add_device(FakeDevice {
                bars: vec![(0x1000, BarType::Mem32)],
            })
            .unwrap();
        let target = MMIO_WINDOW_START + 0x10_0000;
        write_reg(&bus, bdf, BAR0, target as u32);
        assert_eq!((read_reg(&bus, bdf, BAR0) & !0xF) as u64, target);

        // ...and MMIO now dispatches at the new address, not the old one.
        let mut buf = [0u8; 4];
        assert!(bus.handle_mmio_read(target, &mut buf));
    }

    #[test]
    fn bar_5_cannot_be_64_bit() {
        let bus = new_bus();
        let err = bus
            .add_device(FakeDevice {
                bars: vec![
                    (0, BarType::Mem32),
                    (0, BarType::Mem32),
                    (0, BarType::Mem32),
                    (0, BarType::Mem32),
                    (0, BarType::Mem32),
                    (0x1000, BarType::Mem64),
                ],
            })
            .unwrap_err();
        assert!(err.to_string().contains("BAR 5"));
    }

    #[test]
    fn a_bar_that_will_not_fit_is_refused() {
        let bus = new_bus();
        let too_big = MMIO_WINDOW_END - MMIO_WINDOW_START;
        let err = bus
            .add_device(FakeDevice {
                bars: vec![(too_big * 2, BarType::Mem32)],
            })
            .unwrap_err();
        assert!(err.to_string().contains("exhausted"));
    }
}
