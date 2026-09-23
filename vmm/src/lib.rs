pub mod acpi;
pub mod boot;
pub mod config;
pub mod cpuid;
pub mod gdt;
pub mod interrupt;
pub mod isolation;
pub mod layout;
pub mod lifecycle;
pub mod memslot;
pub mod power;
pub mod regs;
/// Checks the loaded virglrenderer enforces the VRAM budget. Meaningless
/// without a renderer to check.
#[cfg(feature = "virgl")]
pub mod renderer;
pub mod seccomp;
pub mod serial;
/// The metrics surface. Currently shaped entirely around `GpuDevice`, so it
/// compiles only with `virgl`; making it source-agnostic is what a
/// virtio-nvgpu stats surface needs first.
#[cfg(feature = "virgl")]
pub mod stats;
pub mod virtiofsd;
pub mod vm;

pub use acpi::slot_gsi as acpi_slot_gsi;
