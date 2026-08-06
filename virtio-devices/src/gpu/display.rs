//! Scanout description: size, format and EDID.
// Minimal display stubs. All krun_display / host-side rendering is intentionally
// absent: this device runs GPU workloads entirely inside the VMM.

use super::edid::EdidInfo;

// ---------------------------------------------------------------------------
// Geometry primitives (used by the protocol and virtio_gpu layers)
// ---------------------------------------------------------------------------

/// Rectangle used in flush / scanout commands. No host display is attached;
/// the struct is kept so the command-processing code compiles unchanged.
#[derive(Debug, Default, Clone, Copy)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Opaque GPU resource pixel format wrapper.
/// Without a display backend we do not interpret the format; we just forward
/// the raw u32 value to rutabaga where needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceFormat(pub u32);

impl ResourceFormat {
    /// Bytes per pixel assumed for 2-D resource size calculations.
    pub const BYTES_PER_PIXEL: usize = 4;

    /// Accept any format value. Returns `None` only for the reserved value 0.
    pub fn try_from(format: u32) -> Option<Self> {
        if format == 0 {
            None
        } else {
            Some(ResourceFormat(format))
        }
    }
}

// ---------------------------------------------------------------------------
// EDID / display configuration (still needed for GET_EDID responses)
// ---------------------------------------------------------------------------

/// Refresh rate and physical dimensions used when synthesising EDID data.
#[derive(Debug, Clone, Copy)]
pub struct EdidParams {
    pub refresh_rate: u32,
    pub physical_size: PhysicalSize,
}

impl Default for EdidParams {
    fn default() -> Self {
        EdidParams {
            refresh_rate: 60,
            physical_size: PhysicalSize::Dpi(96),
        }
    }
}

/// Physical display size: either derived from a DPI value or given explicitly.
#[derive(Debug, Clone, Copy)]
pub enum PhysicalSize {
    Dpi(u32),
    DimensionsMillimeters(u16, u16),
}

// ---------------------------------------------------------------------------
// Display / scanout metadata
// ---------------------------------------------------------------------------

/// Per-scanout configuration. Only width, height, and EDID matter here
/// because we have no host display backend.
#[derive(Clone, Debug)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    pub edid: DisplayInfoEdid,
}

impl DisplayInfo {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            edid: DisplayInfoEdid::Generated(EdidParams::default()),
        }
    }

    /// Serialise the EDID data for this scanout into a byte blob.
    pub fn edid_bytes(&self) -> Box<[u8]> {
        match &self.edid {
            DisplayInfoEdid::Provided(bytes) => bytes.clone(),
            DisplayInfoEdid::Generated(params) => {
                EdidInfo::new(self.width, self.height, params).bytes()
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum DisplayInfoEdid {
    Generated(EdidParams),
    Provided(Box<[u8]>),
}
