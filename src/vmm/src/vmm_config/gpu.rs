// Copyright 2024 - virtio-gpu port for Firecracker
//! GPU device configuration and SHM window management.

use crate::Gpu;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::devices::virtio::gpu::{
    display::{DisplayInfo, DisplayInfoEdid, EdidParams},
    virgl_flags::{
        VIRGLRENDERER_DRM, VIRGLRENDERER_NO_VIRGL, VIRGLRENDERER_THREAD_SYNC,
        VIRGLRENDERER_USE_ASYNC_FENCE_CB, VIRGLRENDERER_USE_EGL,
    },
};

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

// TODO(wanjohiryan): Check on this later, does it work with multiple VMMs? Should we use host or guest ram from machine_config?
// FIX: How do we set the HK_SYSMEM in guest? Or let us set it as half of *guest* memory for now, fingers crossed it works
/// By default, HK sets the heap size to be half the size of the *guest* memory.
/// Since commit 167744dc it's also possible to override the heap size by setting
/// the HK_SYSMEM environment variable.
///
/// Let's set the SHM window for virtio-gpu to be as large as the host's RAM, not
/// because we expect VRAM to be as large as RAM, but to account for the more than
/// likely region fragmentation.
///
/// Then, let's set HK_SYSMEM to be either half the size of the *host* memory, or
/// the value passed by the user with the "vram" argument.
pub fn default_shm_size_mib() -> usize {
    use nix::sys::sysinfo::sysinfo;
    let sysinfo = sysinfo().expect("Failed to get system information");
    (sysinfo.ram_total() / (1024 * 1024)) as usize
}

/// default shm size in bytes
pub fn default_shm_size_bytes() -> usize {
    let ram_total_mib = default_shm_size_mib();
    ram_total_mib * 1024 * 1024
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

impl From<&DisplayInfo> for GpuDisplayConfig {
    fn from(info: &DisplayInfo) -> Self {
        use base64::Engine; // Ensure Engine trait is in scope

        let edid_blob = match &info.edid {
            DisplayInfoEdid::Provided(bytes) => {
                Some(base64::engine::general_purpose::STANDARD.encode(bytes))
            }
            DisplayInfoEdid::Generated(_) => None,
        };

        GpuDisplayConfig {
            width: info.width,
            height: info.height,
            refresh_rate: default_refresh_rate(),
            dpi: default_dpi(),
            edid_blob,
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
            virgl_flags: VIRGLRENDERER_USE_EGL
                | VIRGLRENDERER_NO_VIRGL
                | VIRGLRENDERER_DRM
                | VIRGLRENDERER_THREAD_SYNC
                | VIRGLRENDERER_USE_ASYNC_FENCE_CB,
            shm_size_mib: default_shm_size_mib(),
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
    fn from(g: &Gpu) -> Self {
        GpuConfig {
            virgl_flags: g.virgl_flags,
            shm_size_mib: g
                .shm_region
                .as_ref()
                .map(|r| r.size / (1024 * 1024))
                .unwrap_or(0),
            displays: g.displays.iter().map(&GpuDisplayConfig::from).collect(),
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
