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
//! BAR handling
//! ------------
//! The bus pre-assigns a base address to every BAR at `add_device` time.
//! Config reads for BAR registers (offsets 0x10–0x24) return the assigned
//! address ORed with the type bits (0 = 32-bit memory, non-prefetchable).
//! The kernel probes BAR *size* by writing 0xFFFFFFFF and reading back; we
//! intercept that write, notice it, and on the next read return the size
//! mask instead of the base address.  After the kernel has finished probing
//! it writes the actual base address back; we accept it but keep using the
//! one we assigned (the two will be the same because we tell the kernel our
//! assigned address via the initial read).

pub mod config;

use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

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
}

// ── Internal book-keeping ─────────────────────────────────────────────────────

struct Bar {
    bar_idx: usize,
    addr: u64,   // assigned base address (always page/size aligned)
    size: u64,   // power-of-two size
    probing: bool, // true between a 0xFFFFFFFF write and the next read
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
}

impl Bus {
    pub fn new() -> Self {
        Self {
            devices: RwLock::new(HashMap::new()),
            next_bdf: Mutex::new((0, 1, 0)), // device 0 = host bridge (synthetic, handled inline)
            config_addr: Mutex::new(0),
            mmio_regions: RwLock::new(Vec::new()),
        }
    }

    /// Register a PCI device and assign BAR addresses.  Returns the BDF.
    pub fn add_device(&self, device: impl PciDevice + 'static) -> Result<(u8, u8, u8)> {
        let mut next_bdf = self.next_bdf.lock().unwrap();
        let (bus, dev, func) = *next_bdf;
        if bus > 0 || dev > 31 {
            anyhow::bail!("PCI bus full");
        }

        // Assign BAR addresses (simple bump allocator starting at 0xC000_0000)
        let mut bars: Vec<Bar> = Vec::new();
        // Find the current high-water mark so devices added later don't overlap.
        let hwm: u64 = {
            let regions = self.mmio_regions.read().unwrap();
            regions
                .iter()
                .map(|(base, size, _, _)| base + size)
                .max()
                .unwrap_or(0xC000_0000)
        };
        let mut next_addr = hwm;

        for bar_idx in 0..6 {
            let size = device.bar_size(bar_idx);
            if size == 0 {
                continue;
            }
            // Align up to `size` (which must be a power of two).
            let aligned = (next_addr + size - 1) & !(size - 1);
            bars.push(Bar {
                bar_idx,
                addr: aligned,
                size,
                probing: false,
            });
            next_addr = aligned + size;
        }

        let bdf = (bus, dev, func);
        {
            let mut devices = self.devices.write().unwrap();
            devices.insert(
                bdf,
                PciDeviceEntry {
                    device: Arc::new(device),
                    bars,
                },
            );
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
            cfg[0x08] = 0x02;                                           // revision
            cfg[0x09] = 0x00;                                           // prog-if
            cfg[0x0a] = 0x00;                                           // subclass: host bridge
            cfg[0x0b] = 0x06;                                           // class: bridge
            cfg[0x0e] = 0x00;                                           // header type 0
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
            // Find the Bar struct for this slot.
            if let Some(bar) = entry.bars.iter_mut().find(|b| b.bar_idx == slot) {
                let val: u32 = if bar.probing {
                    // Return size mask: ~(size - 1) with type bits cleared.
                    // For a 32-bit memory BAR: low 4 bits encode type (0 = mem 32-bit).
                    let mask = !(bar.size as u32 - 1) & !0xF;
                    bar.probing = false;
                    mask
                } else {
                    // Return assigned address | type bits (0 = 32-bit mem, non-prefetchable).
                    (bar.addr as u32) | 0x0
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
            let (is_probe, new_bar_addr, bar_size) = {
                let mut devices = self.devices.write().unwrap();
                let entry = match devices.get_mut(&bdf) {
                    Some(e) => e,
                    None => return,
                };
                if let Some(bar) = entry.bars.iter_mut().find(|b| b.bar_idx == slot) {
                    if data.len() == 4
                        && data[0] == 0xFF
                        && data[1] == 0xFF
                        && data[2] == 0xFF
                        && data[3] == 0xFF
                    {
                        bar.probing = true;
                        (true, 0u64, bar.size)
                    } else if data.len() >= 4 {
                        let raw = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as u64;
                        let aligned = raw & !(bar.size - 1);
                        if aligned != 0 {
                            bar.addr = aligned;
                        }
                        (false, bar.addr, bar.size)
                    } else {
                        (false, 0, 0)
                    }
                } else {
                    return;
                }
            }; // devices lock released here

            // Now update mmio_regions separately (no nesting).
            if !is_probe && new_bar_addr != 0 {
                let mut mmio = self.mmio_regions.write().unwrap();
                for region in mmio.iter_mut() {
                    if region.2 == bdf && region.3 == slot {
                        region.0 = new_bar_addr & !(bar_size - 1);
                    }
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

    pub fn handle_mmio_write(&self, addr: u64, data: &[u8]) -> bool {
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