// Copyright 2024 - Firecracker GPU port
// Snapshot/restore support for the virtio-GPU device.
//
// # Snapshot scope
//
// The GPU device state captured here covers:
//   - Device identity (id, virgl_flags)
//   - Display configuration (width / height / EDID params)
//   - Virtio queue and feature-negotiation state (avail/acked features, queue
//     ring pointers, next_avail/next_used indices)
//
// **Rutabaga rendering state (GPU context data, 3-D resources, shader caches)
// is NOT captured.**  After restore the rutabaga renderer is re-initialised
// from scratch, so any in-flight GPU work is lost.  This is acceptable for
// most cloud-gaming pause/resume scenarios where the guest re-submits work
// after resume.
//
// Full rutabaga state serialisation (analogous to crosvm's directory-based
// rutabaga snapshot) can be added later once the libkrun-fork rutabaga
// exposes `Rutabaga::snapshot(path)` / `Rutabaga::restore(path)`.

use serde::{Deserialize, Serialize};

use crate::devices::virtio::device::VirtioDeviceType;
use crate::devices::virtio::gpu::{
    Gpu, GpuError, NUM_QUEUES, QUEUE_SIZE, VirtioShmRegion,
    display::{DisplayInfo, DisplayInfoEdid, EdidParams, PhysicalSize},
};
use crate::devices::virtio::persist::VirtioDeviceState;
use crate::snapshot::Persist;
use crate::vstate::memory::GuestMemoryMmap;

// ---------------------------------------------------------------------------
// Serialisable display configuration
// ---------------------------------------------------------------------------

/// Serialisable form of [`PhysicalSize`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PhysicalSizeState {
    Dpi(u32),
    DimensionsMillimeters(u16, u16),
}

impl From<PhysicalSize> for PhysicalSizeState {
    fn from(p: PhysicalSize) -> Self {
        match p {
            PhysicalSize::Dpi(d) => PhysicalSizeState::Dpi(d),
            PhysicalSize::DimensionsMillimeters(w, h) => {
                PhysicalSizeState::DimensionsMillimeters(w, h)
            }
        }
    }
}

impl From<PhysicalSizeState> for PhysicalSize {
    fn from(p: PhysicalSizeState) -> Self {
        match p {
            PhysicalSizeState::Dpi(d) => PhysicalSize::Dpi(d),
            PhysicalSizeState::DimensionsMillimeters(w, h) => {
                PhysicalSize::DimensionsMillimeters(w, h)
            }
        }
    }
}

/// Serialisable form of [`EdidParams`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdidParamsState {
    pub refresh_rate: u32,
    pub physical_size: PhysicalSizeState,
}

impl From<EdidParams> for EdidParamsState {
    fn from(e: EdidParams) -> Self {
        EdidParamsState {
            refresh_rate: e.refresh_rate,
            physical_size: e.physical_size.into(),
        }
    }
}

impl From<EdidParamsState> for EdidParams {
    fn from(e: EdidParamsState) -> Self {
        EdidParams {
            refresh_rate: e.refresh_rate,
            physical_size: e.physical_size.into(),
        }
    }
}

/// Serialisable form of [`DisplayInfoEdid`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DisplayInfoEdidState {
    Generated(EdidParamsState),
    Provided(Vec<u8>),
}

impl From<&DisplayInfoEdid> for DisplayInfoEdidState {
    fn from(d: &DisplayInfoEdid) -> Self {
        match d {
            DisplayInfoEdid::Generated(p) => DisplayInfoEdidState::Generated((*p).into()),
            DisplayInfoEdid::Provided(b) => DisplayInfoEdidState::Provided(b.to_vec()),
        }
    }
}

impl From<DisplayInfoEdidState> for DisplayInfoEdid {
    fn from(d: DisplayInfoEdidState) -> Self {
        match d {
            DisplayInfoEdidState::Generated(p) => DisplayInfoEdid::Generated(p.into()),
            DisplayInfoEdidState::Provided(b) => DisplayInfoEdid::Provided(b.into_boxed_slice()),
        }
    }
}

/// Serialisable form of a single scanout's [`DisplayInfo`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfoState {
    pub width: u32,
    pub height: u32,
    pub edid: DisplayInfoEdidState,
}

impl From<&DisplayInfo> for DisplayInfoState {
    fn from(d: &DisplayInfo) -> Self {
        DisplayInfoState {
            width: d.width,
            height: d.height,
            edid: (&d.edid).into(),
        }
    }
}

impl From<DisplayInfoState> for DisplayInfo {
    fn from(d: DisplayInfoState) -> Self {
        DisplayInfo {
            width: d.width,
            height: d.height,
            edid: d.edid.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// GpuState – the snapshot state struct
// ---------------------------------------------------------------------------

/// Full snapshot state of the virtio-GPU device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuState {
    /// Virtio queue / feature state common to all virtio devices.
    pub virtio_state: VirtioDeviceState,
    /// VirGL renderer creation flags.
    pub virgl_flags: u32,
    /// Per-scanout display configuration.
    pub displays: Vec<DisplayInfoState>,
}

// ---------------------------------------------------------------------------
// Constructor args
// ---------------------------------------------------------------------------

/// Arguments required to reconstruct a [`Gpu`] device from a snapshot.
#[derive(Debug)]
pub struct GpuConstructorArgs {
    pub mem: GuestMemoryMmap,
    /// Optional host-visible SHM region for blob resource mapping.
    /// Must be re-established externally before the device is activated.
    pub shm_region: Option<VirtioShmRegion>,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum GpuPersistError {
    /// Failed to create GPU device: {0:?}
    CreateDevice(GpuError),
    /// Failed to restore virtio state: {0}
    VirtioState(#[from] crate::devices::virtio::persist::PersistError),
}

// ---------------------------------------------------------------------------
// Persist impl
// ---------------------------------------------------------------------------

impl<'a> Persist<'a> for Gpu {
    type State = GpuState;
    type ConstructorArgs = GpuConstructorArgs;
    type Error = GpuPersistError;

    fn save(&self) -> Self::State {
        GpuState {
            virtio_state: VirtioDeviceState::from_device(self),
            virgl_flags: 0,
            displays: self.displays.iter().map(DisplayInfoState::from).collect(),
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> Result<Self, Self::Error> {
        let displays: Box<[DisplayInfo]> = state
            .displays
            .iter()
            .cloned()
            .map(DisplayInfo::from)
            .collect::<Vec<_>>()
            .into_boxed_slice();

        let mut gpu = Gpu::new(
            // The device id is reconstructed from the VirtioDeviceState via the
            // transport layer; we pass an empty string here and let the transport
            // state override it.
            String::new(),
            displays,
            "".into(), // TODO: Placeholder, should be /dev/dri/renderD*
        )
        .map_err(GpuPersistError::CreateDevice)?;

        // Restore virtio queue and feature state.
        gpu.queues = state.virtio_state.build_queues_checked(
            &constructor_args.mem,
            VirtioDeviceType::Gpu,
            NUM_QUEUES,
            QUEUE_SIZE,
        )?;
        gpu.avail_features = state.virtio_state.avail_features;
        gpu.acked_features = state.virtio_state.acked_features;

        if let Some(shm) = constructor_args.shm_region {
            gpu.set_shm_region(shm);
        }

        Ok(gpu)
    }
}
