// Copyright 2024 - virtio-gpu port for Firecracker
//! GPU device configuration and SHM window management.

use crate::Gpu;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::devices::virtio::gpu::display::{DisplayInfo, DisplayInfoEdid, EdidParams};

// ---------------------------------------------------------------------------
// Public configuration structs (JSON-configurable)
// ---------------------------------------------------------------------------

/// Single scanout (virtual display) configuration.
///
/// Only `width` and `height` are required; EDID data is synthesised from the
/// refresh rate and physical size when not provided explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuDisplayConfig {
    /// Horizontal resolution in pixels.
    pub width: u32,
    /// Vertical resolution in pixels.
    pub height: u32,
    /// Refresh rate in Hz (default: 60).
    #[serde(default = "default_refresh_rate")]
    pub refresh_rate: u32,
    /// Physical dots-per-inch of the display (default: 96).
    /// Ignored when `edid_blob` is set.
    #[serde(default = "default_dpi")]
    pub dpi: u32,
    /// Raw EDID blob (base64-encoded).  When set, overrides the synthesised
    /// EDID and the `refresh_rate` / `dpi` fields.
    pub edid_blob: Option<String>,
}

fn default_refresh_rate() -> u32 {
    60
}
fn default_dpi() -> u32 {
    96
}

impl From<&GpuDisplayConfig> for DisplayInfo {
    fn from(cfg: &GpuDisplayConfig) -> Self {
        use crate::devices::virtio::gpu::display::PhysicalSize;

        let edid = if let Some(b64) = &cfg.edid_blob {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap_or_default();
            DisplayInfoEdid::Provided(bytes.into_boxed_slice())
        } else {
            DisplayInfoEdid::Generated(EdidParams {
                refresh_rate: cfg.refresh_rate,
                physical_size: PhysicalSize::Dpi(cfg.dpi),
            })
        };

        DisplayInfo {
            width: cfg.width,
            height: cfg.height,
            edid,
        }
    }
}

/// Top-level GPU device configuration.
///
/// Example JSON:
/// ```json
/// "gpu": {
///     "virgl_flags": 0,
///     "shm_size_mib": 512,
///     "displays": [{ "width": 1920, "height": 1080 }]
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GpuConfig {
    /// VirGL renderer creation flags (passed verbatim to rutabaga).
    ///
    /// Common values:
    ///  - `0`  – default VirGL (requires a host EGL/Wayland context)
    ///  - `1 << 7` (`VIRGLRENDERER_NO_VIRGL`) – 2-D only, no host GL required
    pub virgl_flags: u32,

    /// Size of the host-visible shared memory window used for blob resource
    /// mapping, in mebibytes.  Set to 0 to disable blob resources (no SHM
    /// window will be registered with KVM).
    pub shm_size_mib: usize,

    /// Per-scanout display configurations.  At least one entry is required.
    /// Corresponds 1:1 with virtio-gpu scanouts (max 16).
    pub displays: Vec<GpuDisplayConfig>,
}

impl Default for GpuConfig {
    fn default() -> Self {
        GpuConfig {
            virgl_flags: 0,
            shm_size_mib: 256,
            displays: vec![GpuDisplayConfig {
                width: 1280,
                height: 720,
                refresh_rate: default_refresh_rate(),
                dpi: default_dpi(),
                edid_blob: None,
            }],
        }
    }
}

impl GpuConfig {
    /// Convert display configs to the internal `DisplayInfo` slice.
    pub fn display_infos(&self) -> Box<[DisplayInfo]> {
        self.displays
            .iter()
            .map(DisplayInfo::from)
            .collect::<Vec<_>>()
            .into_boxed_slice()
    }
}

impl From<&Gpu> for GpuConfig {
    fn from(mem: &Gpu) -> Self {
        GpuConfig {
            virgl_flags: mem.virgl_flags,
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors associated with configuring the gpu.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum GpuConfigError {
    /// No display configurations provided; at least one scanout is required.
    NoDisplays,
    /// Too many displays: {0} specified, maximum is 16.
    TooManyDisplays(usize),
    /// Invalid EDID blob for display {0}: base64 decode failed.
    InvalidEdidBlob(usize),
}
