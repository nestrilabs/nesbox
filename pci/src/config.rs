//! PCI configuration space builder with automatic capability list management.
//!
//! Usage:
//!   let mut cfg = PciConfig::new(0x1AF4, 0x1043, 0x01, 0x07_80_00, 0x1AF4, 0x0003);
//!   cfg.set_bar_mem(0, BAR0_SIZE);
//!   cfg.add_cap(0x11, &msix_cap_bytes);  // MSI-X
//!   cfg.add_virtio_cap(1, 0, OFF_COMMON, 0x38);   // CommonCfg
//!   cfg.add_virtio_cap(2, 0, OFF_NOTIFY, 0x1000); // NotifyCfg
//!   cfg.add_virtio_cap(3, 0, OFF_ISR, 1);          // IsrCfg
//!   cfg.add_virtio_cap(4, 0, OFF_DEVICE, 12);       // DeviceCfg
//!
//! The `add_cap` method automatically builds the linked list so capabilities
//! never overlap.

/// Device/Port Type: Root Complex Integrated Endpoint.
pub const PCIE_TYPE_RC_INTEGRATED: u8 = 0x9;

const HEADER_TYPE_DEVICE: u8 = 0x00;
const CAP_LIST_HEAD: u8 = 0x34;
const FIRST_CAP: u16 = 0x40;
/// Capabilities may run to the end of the 256-byte config space.
const CAP_MAX: u16 = 0x100;

/// Builder for a 256-byte PCI configuration space.
pub struct PciConfig {
    data: [u8; 256],
    last_cap_offset: Option<u16>,
    /// Where the next capability may start. Tracked explicitly: a capability's
    /// length cannot be recovered from its contents in general — only virtio
    /// vendor capabilities happen to store it there.
    next_cap_start: u16,
}

impl PciConfig {
    /// Create a new PCI config space with standard header fields.
    /// `class_code` is a 24-bit value: (base_class << 16) | (sub_class << 8) | prog_if.
    pub fn new(
        vendor: u16,
        device: u16,
        revision: u8,
        class_code: u32,
        subsystem_vendor: u16,
        subsystem_id: u16,
    ) -> Self {
        let mut c = [0u8; 256];
        // Register 0: vendor + device
        c[0..2].copy_from_slice(&vendor.to_le_bytes());
        c[2..4].copy_from_slice(&device.to_le_bytes());
        // Register 1: command = Memory Space + Bus Master (0x0006)
        c[4..6].copy_from_slice(&0x0006u16.to_le_bytes());
        // Status: Capabilities List (bit 4)
        c[6..8].copy_from_slice(&0x0010u16.to_le_bytes());
        // Register 2: revision + class code (PCI big-endian: BCC|SCC|PI|RID)
        c[8] = revision;                                     // RID
        c[9] = (class_code & 0xFF) as u8;                    // PI
        c[10] = ((class_code >> 8) & 0xFF) as u8;            // SCC
        c[11] = ((class_code >> 16) & 0xFF) as u8;           // BCC
        // Register 3: header type
        c[0x0e] = HEADER_TYPE_DEVICE;
        // Register 11: subsystem vendor + subsystem ID
        c[0x2c..0x2e].copy_from_slice(&subsystem_vendor.to_le_bytes());
        c[0x2e..0x30].copy_from_slice(&subsystem_id.to_le_bytes());

        Self { data: c, last_cap_offset: None, next_cap_start: FIRST_CAP }
    }

    /// Set interrupt pin (INTA# = 1).
    pub fn set_irq_pin(&mut self, pin: u8) {
        self.data[0x3d] = pin;
    }

    /// Set interrupt line (GSI).
    pub fn set_irq_line(&mut self, line: u8) {
        self.data[0x3c] = line;
    }

    /// Declare a 32-bit non-prefetchable memory BAR at the given offset.
    /// The BAR size is used for address space allocation by the PCI bus.
    pub fn set_bar_mem(&mut self, bar_idx: usize, _size: u64) {
        assert!(bar_idx < 6);
        let off = 0x10 + bar_idx * 4;
        // Write 0 — the PCI bus fills the actual address.
        // Type bits: bit 0=0 (memory), bits 2:1=0 (32-bit), bit 3=0 (non-prefetchable)
        self.data[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
    }

    /// Declare a 64-bit prefetchable memory BAR at the given offset.
    ///
    /// This occupies two consecutive BAR registers — `bar_idx` and the one
    /// after it — so the following index must be left unused. The actual
    /// address and the type bits come from the bus at read time; only the
    /// reservation matters here.
    pub fn set_bar_mem64(&mut self, bar_idx: usize, _size: u64) {
        assert!(bar_idx < 5, "a 64-bit BAR needs a register after it");
        let off = 0x10 + bar_idx * 4;
        self.data[off..off + 8].copy_from_slice(&0u64.to_le_bytes());
    }

    /// Add a generic PCI capability.
    /// `cap_id` is the PCI capability ID (e.g. 0x09 for vendor, 0x11 for MSI-X).
    /// `payload` is the capability data starting from byte 2 (after cap_id + next).
    pub fn add_cap(&mut self, cap_id: u8, payload: &[u8]) -> u16 {
        let total_cap_len = 2 + payload.len(); // id + next + payload
        let start = self.next_cap_start;

        assert!(start >= FIRST_CAP && start < CAP_MAX, "capability space exhausted");
        let s = start as usize;
        assert!(s + total_cap_len <= CAP_MAX as usize, "capability too large");

        // Write the capability
        self.data[s] = cap_id;
        self.data[s + 1] = 0x00; // terminator (will be patched when next cap is added)
        self.data[s + 2..s + total_cap_len].copy_from_slice(payload);

        // Patch the previous capability's "next" pointer
        if let Some(prev) = self.last_cap_offset {
            self.data[prev as usize + 1] = start as u8;
        } else {
            // First capability — set the capabilities pointer in the header
            self.data[CAP_LIST_HEAD as usize] = start as u8;
        }

        self.last_cap_offset = Some(start);
        self.next_cap_start = next_dword(start + total_cap_len as u16);
        start
    }

    /// Add a virtio PCI capability (type 0x09 = vendor-specific).
    /// `cfg_type`: 1=Common, 2=Notify, 3=ISR, 4=Device
    /// `bar_idx`: which BAR this structure lives in
    /// `offset`: BAR-relative offset
    /// `length`: size of the structure in bytes
    pub fn add_virtio_cap(&mut self, cfg_type: u8, bar_idx: u8, offset: u32, length: u32) {
        let mut payload = [0u8; 14]; // VirtioPciCap is 14 bytes
        payload[0] = 0x10; // cap_len = 16 (includes vndr+next)
        payload[1] = cfg_type;
        payload[2] = bar_idx;
        payload[3] = 0; // id
        // padding at 4,5 already zero
        payload[6..10].copy_from_slice(&offset.to_le_bytes());
        payload[10..14].copy_from_slice(&length.to_le_bytes());
        self.add_cap(0x09, &payload);
    }

    /// Add a virtio notify capability with `notify_off_multiplier`.
    pub fn add_virtio_notify_cap(&mut self, bar_idx: u8, offset: u32, length: u32, multiplier: u32) {
        let mut payload = [0u8; 18]; // VirtioPciNotifyCap is 18 bytes
        payload[0] = 0x14; // cap_len = 20
        payload[1] = 2;    // cfg_type = Notify
        payload[2] = bar_idx;
        // padding at 4,5
        payload[6..10].copy_from_slice(&offset.to_le_bytes());
        payload[10..14].copy_from_slice(&length.to_le_bytes());
        payload[14..18].copy_from_slice(&multiplier.to_le_bytes());
        self.add_cap(0x09, &payload);
    }

    /// Add an MSI-X capability. Returns the capability's config-space offset,
    /// which the device needs in order to decode writes to Message Control.
    /// `table_size`: number of vectors - 1
    pub fn add_msix_cap(&mut self, table_size: u16, table_off: u32, pba_off: u32) -> u16 {
        let mut payload = [0u8; 10];
        payload[0..2].copy_from_slice(&table_size.to_le_bytes()); // msg_ctl
        payload[2..6].copy_from_slice(&table_off.to_le_bytes());  // table offset + BIR
        payload[6..10].copy_from_slice(&pba_off.to_le_bytes());   // PBA offset + BIR
        self.add_cap(0x11, &payload)
    }

    /// Add a PCI Express capability, version 2, making this a PCIe device
    /// rather than a conventional PCI one.
    ///
    /// `device_type` is the Device/Port Type field: [`PCIE_TYPE_RC_INTEGRATED`]
    /// for a device sitting directly on the root complex, which is what our
    /// devices are — there is no root port above them, so the link registers
    /// stay zero and are never consulted.
    pub fn add_pcie_cap(&mut self, device_type: u8) -> u16 {
        // 2 bytes of id+next, then 58 bytes of registers through Slot Status 2.
        let mut payload = [0u8; 58];
        // PCI Express Capabilities Register: version 2, device/port type.
        let caps_reg: u16 = 0x0002 | ((device_type as u16 & 0xF) << 4);
        payload[0..2].copy_from_slice(&caps_reg.to_le_bytes());
        self.add_cap(0x10, &payload)
    }

    /// Return the final 256-byte config space.
    pub fn build(&self) -> [u8; 256] {
        self.data
    }
}

fn next_dword(offset: u16) -> u16 {
    (offset + 3) & !3
}
