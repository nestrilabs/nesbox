pub mod acpi;
pub mod boot;
pub mod config;
pub mod gdt;
pub mod interrupt;
pub mod layout;
pub mod lifecycle;
pub mod memslot;
pub mod power;
pub mod regs;
pub mod serial;
pub mod virtiofsd;
pub mod vm;

pub use acpi::slot_gsi as acpi_slot_gsi;
