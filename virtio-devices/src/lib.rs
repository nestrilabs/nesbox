mod blk;
pub mod common;
mod console;
mod fs;
/// The virtio-gpu device, and with it rutabaga and virglrenderer.
///
/// Optional: an NVIDIA host forwards driver ioctls with `nvgpu` and never
/// creates this device, and linking virglrenderer into that build means a C
/// library and a Mesa stack it will not call. See the `virgl` feature.
#[cfg(feature = "virgl")]
pub mod gpu;
pub mod memmap;
mod net;
mod nvgpu;
pub mod tap;
mod vsock;

pub use blk::{BlkConfig, BlkDevice};
pub use console::ConsoleDevice;
pub use fs::FsDevice;
#[cfg(feature = "virgl")]
pub use gpu::{
    CommandKindCounts, GPU_COMMAND_NAMES, GpuConfig, GpuDevice, GpuSnapshot, InfoCounts, Occupancy,
    PhaseSnapshot,
};
pub use memmap::HostMemoryMapper;
pub use net::{NetConfig, NetDevice};
pub use nvgpu::NvGpuDevice;
pub use vsock::VsockDevice;
