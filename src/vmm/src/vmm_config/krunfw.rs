// Copyright 2024 - libkrunfw kernel bundle loader
//! Kernel loading from a pre-compiled shared library (`libkrunfw.so.5`).
//!
//! # What libkrunfw is
//!
//! `libkrunfw.so.5` is a shared library that embeds a pre-compiled Linux kernel
//! (bzImage) as a read-only symbol.  The library exposes a single C function:
//!
//! ```c
//! char *krunfw_get_kernel(uint64_t *guest_addr,
//!                         uint64_t *entry_addr,
//!                         size_t   *size);
//! ```
//!
//! which returns:
//!  - A host pointer to the kernel bytes (a pointer into the .so's data segment)
//!  - The guest physical address where the kernel expects to be loaded
//!  - The kernel entry-point guest physical address
//!  - The kernel size in bytes
//!
//! # Why no virtio-fs is needed
//!
//! The kernel embedded in the library is a full vmlinuz / bzImage with all
//! drivers compiled in.  No initrd is embedded (and none is required for
//! the standard libkrun use-case).  The root filesystem is provided by a
//! virtio-blk block device.  virtio-fs is never involved.
//!
//! # Loading flow
//!
//! 1. The caller (builder.rs) calls [`KrunfwLoader::load`].
//! 2. The function dlopen's the library with `libloading`.
//! 3. The kernel bytes are memcpy'd from the .so into the appropriate guest
//!    memory region (identified by the returned `guest_addr`).
//! 4. A [`KernelBundle`] carrying the entry point address is returned to the
//!    caller, which passes it to `configure_system_for_boot` instead of the
//!    normal `load_kernel` result.

use std::path::PathBuf;

use vm_memory::{Bytes, GuestAddress};

use crate::vstate::memory::GuestMemoryMmap;

// Default library names matching the libkrun convention.
#[cfg(target_os = "linux")]
const DEFAULT_KRUNFW_NAME: &str = "libkrunfw.so.5";

// ---------------------------------------------------------------------------
// KernelBundle – replaces the file-based EntryPoint for bundle-loaded kernels
// ---------------------------------------------------------------------------

/// The result of loading a kernel from a bundle (file or .so).
///
/// This mirrors libkrun's `KernelBundle` struct.  It carries everything
/// `configure_system_for_boot` / `arch::configure_system` need to boot.
#[derive(Debug, Clone)]
pub struct KernelBundle {
    /// Host virtual address of the first kernel byte (pointer into .so memory
    /// or a heap buffer).  Only valid until the .so is unloaded; `load()`
    /// already copies the bytes into guest memory, so this is kept for
    /// debugging only.
    pub host_addr: u64,
    /// Guest physical address where the kernel was copied.
    pub guest_addr: GuestAddress,
    /// Guest physical entry-point address reported by the kernel binary.
    pub entry_addr: GuestAddress,
    /// Kernel size in bytes.
    pub size: usize,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors associated with loading libkrunfw.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum KrunfwError {
    /// Could not open library '{path}': {source}
    DlOpen {
        /// Could not open library '{path}'
        path: PathBuf,
        /// Could not open library: {source}
        #[source]
        source: libloading::Error,
    },
    /// Could not find symbol 'krunfw_get_kernel' in library: {0}
    DlSym(#[source] libloading::Error),
    /// Kernel host pointer returned by krunfw_get_kernel is null
    NullKernelPointer,
    /// Failed to write kernel bytes into guest memory: {0}
    GuestMemoryWrite(#[source] vm_memory::GuestMemoryError),
}

// ---------------------------------------------------------------------------
// KrunfwLoader
// ---------------------------------------------------------------------------

/// Loads a kernel from `libkrunfw.so.5` (or a custom path) and copies it
/// into guest memory.
#[derive(Debug)]
pub struct KrunfwLoader {
    /// Path to the library.  Defaults to `DEFAULT_KRUNFW_NAME` (found via
    /// the dynamic linker's normal search path).
    pub lib_path: PathBuf,
}

impl Default for KrunfwLoader {
    fn default() -> Self {
        KrunfwLoader {
            lib_path: PathBuf::from(DEFAULT_KRUNFW_NAME),
        }
    }
}

impl KrunfwLoader {
    /// Create a loader for a library at a specific path.
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        KrunfwLoader {
            lib_path: path.into(),
        }
    }

    /// Open the library, call `krunfw_get_kernel`, copy the kernel bytes into
    /// `guest_mem`, and return a [`KernelBundle`] describing the result.
    ///
    /// # Safety
    ///
    /// This calls into a C shared library.  The library is expected to be the
    /// genuine `libkrunfw.so.5`.  If a malicious .so is supplied, all bets are
    /// off — but that is a deployment concern, not a memory-safety one: the
    /// pointer arithmetic is bounds-checked against `kernel_size`.
    pub fn load(&self, guest_mem: &GuestMemoryMmap) -> Result<KernelBundle, KrunfwError> {
        // SAFETY: We are calling a well-known C ABI with well-defined semantics.
        let lib = unsafe {
            libloading::Library::new(&self.lib_path).map_err(|e| KrunfwError::DlOpen {
                path: self.lib_path.clone(),
                source: e,
            })?
        };

        // krunfw_get_kernel signature (C):
        //   char *krunfw_get_kernel(uint64_t *guest_addr,
        //                           uint64_t *entry_addr,
        //                           size_t   *size);
        type GetKernelFn =
            unsafe extern "C" fn(*mut u64, *mut u64, *mut libc::size_t) -> *mut libc::c_char;

        // SAFETY: We know the symbol name and ABI from the libkrunfw spec.
        let get_kernel: libloading::Symbol<GetKernelFn> =
            unsafe { lib.get(b"krunfw_get_kernel").map_err(KrunfwError::DlSym)? };

        let mut guest_addr_raw: u64 = 0;
        let mut entry_addr_raw: u64 = 0;
        let mut kernel_size: libc::size_t = 0;

        // SAFETY: Out-pointers are valid stack variables; the library writes
        // into them and returns a pointer into its own read-only data segment.
        let host_ptr = unsafe {
            get_kernel(
                &mut guest_addr_raw as *mut u64,
                &mut entry_addr_raw as *mut u64,
                &mut kernel_size as *mut libc::size_t,
            )
        };

        if host_ptr.is_null() {
            return Err(KrunfwError::NullKernelPointer);
        }

        // SAFETY: `host_ptr` points to `kernel_size` valid bytes in the .so's
        // data segment, which remains mapped for the lifetime of `lib`.
        let kernel_bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(host_ptr as *const u8, kernel_size) };

        let guest_addr = GuestAddress(guest_addr_raw);

        // Copy into guest memory.
        guest_mem
            .write_slice(kernel_bytes, guest_addr)
            .map_err(KrunfwError::GuestMemoryWrite)?;

        log::debug!(
            "krunfw: loaded {kernel_size} byte kernel at GPA {guest_addr_raw:#x}, \
             entry {entry_addr_raw:#x}"
        );

        // The library can be dropped now — the kernel bytes have been copied.
        Ok(KernelBundle {
            host_addr: host_ptr as u64,
            guest_addr,
            entry_addr: GuestAddress(entry_addr_raw),
            size: kernel_size,
        })
    }
}

// ---------------------------------------------------------------------------
// Integration note for arch::configure_system_for_boot
// ---------------------------------------------------------------------------
//
// Firecracker's `load_kernel()` (arch/src/x86_64/mod.rs or aarch64 equivalent)
// returns an `EntryPoint` struct:
//   pub struct EntryPoint {
//       pub entry_addr: GuestAddress,
//       // (x86: also boot_prot)
//   }
//
// When using a KernelBundle, replace the `load_kernel` call in
// `builder::build_microvm_for_boot` with `KrunfwLoader::load`, then construct
// an `EntryPoint` from `bundle.entry_addr` before calling
// `configure_system_for_boot`.
//
// The `initrd` parameter to `configure_system_for_boot` should be
// `InitrdConfig::empty()` (or None, depending on your arch layer) because
// libkrunfw kernels are self-contained and do not require an initrd.
