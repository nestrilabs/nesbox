//! The backing file, and everything about it the rest of the device needs to
//! know: how large it is, whether it can be reached with `O_DIRECT`, and what
//! alignment that costs.
//!
//! `O_DIRECT` is the reason this file exists. Without it every byte a guest
//! reads is cached twice -- once in the host page cache and once in the guest's
//! -- so N guests streaming the same size of working set cost N times the host
//! RAM for it, and a `io.max` cgroup bound on the VM stops meaning anything the
//! moment the host has the image cached (measured: a capped guest ran 35x
//! faster than an uncapped one with a cold cache; `docs/BENCHMARKS.md` §12.2).
//!
//! It is not free to ask for. Direct I/O requires every buffer address, file
//! offset and length to be aligned, and what to is a property of the file and
//! the filesystem under it -- 512 on most, 4096 on a 4Kn device, and unsupported
//! outright on tmpfs and some network filesystems. So the alignment is probed
//! rather than assumed, and a file that cannot do direct I/O is opened buffered
//! with the reason logged instead of failing the boot.

use super::engine::IoVec;
use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;

pub const SECTOR_SIZE: u64 = 512;

/// The largest logical block size a guest will take.
///
/// Linux refuses a virtio-blk device whose `blk_size` is above its page size --
/// `Invalid logical block size (131072)`, and the device does not probe -- so
/// this is a ceiling on what may be advertised, and with it a ceiling on the
/// offset alignment direct I/O can be worth asking for. A filesystem that wants
/// more than this per I/O cannot be reached directly by a guest doing ordinary
/// block-sized reads.
const MAX_BLOCK_SIZE: u64 = 4096;

/// What the configuration asked for. `Auto` is the default and the one to
/// prefer: it takes direct I/O where the host can give it and says so in the
/// log where it cannot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CacheMode {
    /// Direct where the backing file supports it, buffered where it does not.
    #[default]
    Auto,
    /// Direct, or fail to start. For a box whose isolation depends on it.
    Direct,
    /// Buffered. The host page cache absorbs the guest's I/O, which is faster
    /// for a single small guest and unaccountable for several.
    Writeback,
}

impl CacheMode {
    pub fn from_flag(direct: Option<bool>) -> Self {
        match direct {
            None => Self::Auto,
            Some(true) => Self::Direct,
            Some(false) => Self::Writeback,
        }
    }
}

/// The alignment direct I/O on this file demands.
#[derive(Clone, Copy, Debug)]
pub struct DioAlign {
    /// Alignment required of a buffer's address in memory.
    pub mem: u64,
    /// Alignment required of a file offset, and of a transfer's length.
    pub offset: u64,
}

pub struct Disk {
    file: File,
    pub read_only: bool,
    /// Capacity in 512-byte virtio sectors, which is the unit the guest asks
    /// in whatever the block size below says.
    pub sectors: u64,
    /// Logical block size advertised to the guest.
    ///
    /// Raised to the file's direct-I/O offset alignment when that is larger
    /// than a sector, so that a guest doing block-sized I/O produces requests
    /// this device can issue directly. Telling a guest 512 on a 4Kn device
    /// would leave every request needing a bounce buffer.
    pub block_size: u32,
    /// None when the file is opened buffered.
    pub dio: Option<DioAlign>,
}

impl Disk {
    pub fn open(path: &Path, read_only: bool, mode: CacheMode) -> Result<Self> {
        let probe = probe_dio(path);

        let want_direct = match mode {
            CacheMode::Writeback => false,
            CacheMode::Auto | CacheMode::Direct => true,
        };

        // The probe is what decides, not the open: opening with `O_DIRECT`
        // succeeds on filesystems that then refuse every direct read, so a
        // successful open is not evidence the flag works.
        let direct = match (want_direct, probe) {
            (false, _) => None,
            // Alignment the guest cannot be told about. ZFS asks for its
            // recordsize -- 128 KiB by default -- and a guest cannot be given
            // a 128 KiB logical block, so every request would have to be
            // staged through a bounce buffer and every write whose length is
            // not a whole record refused. Buffered I/O is slower per byte than
            // direct and is not that.
            (true, Some(align)) if align.offset > MAX_BLOCK_SIZE => {
                let why = format!(
                    "{path:?} needs {}-byte aligned direct I/O, which is above the {MAX_BLOCK_SIZE}-byte \
                     block a guest can be given (a ZFS recordsize, or similar)",
                    align.offset
                );
                if mode == CacheMode::Direct {
                    bail!(
                        "{why}, and the drive asked for direct I/O. Put the image on a \
                         filesystem with a smaller direct-I/O alignment, or set \
                         \"direct\": false."
                    );
                }
                log::warn!(
                    "virtio-blk: {why}; opening buffered. Guest reads will also be held in the \
                     host page cache, so an io.max bound on this VM will not hold against a warm \
                     cache."
                );
                None
            }
            (true, Some(align)) => Some(align),
            (true, None) => {
                if mode == CacheMode::Direct {
                    bail!(
                        "{path:?} does not support direct I/O, and the drive asked for it. \
                         Either move the image onto a filesystem that does, or set \
                         \"direct\": false to accept host page-cache caching."
                    );
                }
                None
            }
        };

        let mut opts = OpenOptions::new();
        opts.read(true).write(!read_only);
        if direct.is_some() {
            opts.custom_flags(libc::O_DIRECT);
        }
        let file = match opts.open(path) {
            Ok(f) => f,
            // A kernel that disagrees with the probe. Falling back beats
            // refusing to boot over a performance flag.
            Err(e) if direct.is_some() && mode == CacheMode::Auto => {
                log::warn!("virtio-blk: {path:?} rejected O_DIRECT ({e}); opening buffered");
                return Self::open(path, read_only, CacheMode::Writeback);
            }
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to open block device: {path:?}"));
            }
        };

        let bytes = size_of(&file).with_context(|| format!("Failed to size {path:?}"))?;
        let sectors = bytes / SECTOR_SIZE;
        if sectors == 0 {
            bail!("{path:?} holds {bytes} bytes, which is not one 512-byte sector");
        }

        // Whatever direct I/O needs the offset aligned to is what the guest
        // should be issuing, so tell it that is the block size. Bounded above
        // by what a guest will accept; `direct` is already None above that.
        let block_size = match direct {
            Some(a) => a.offset.clamp(SECTOR_SIZE, MAX_BLOCK_SIZE) as u32,
            None => SECTOR_SIZE as u32,
        };

        match direct {
            Some(a) => log::info!(
                "virtio-blk: {path:?}  {sectors} sectors  read_only={read_only}  \
                 O_DIRECT (mem align {}, offset align {})  block size {block_size}",
                a.mem,
                a.offset
            ),
            None => log::info!(
                "virtio-blk: {path:?}  {sectors} sectors  read_only={read_only}  \
                 buffered -- guest reads will also be held in the host page cache"
            ),
        }

        Ok(Self {
            file,
            read_only,
            sectors,
            block_size,
            dio: direct,
        })
    }

    pub fn raw_fd(&self) -> i32 {
        self.file.as_raw_fd()
    }

    pub fn len_bytes(&self) -> u64 {
        self.sectors * SECTOR_SIZE
    }

    /// Can this transfer go straight to the guest's own pages?
    ///
    /// Every segment has to satisfy the memory alignment on its own: the kernel
    /// checks each `iovec`, not the total. Anything that fails goes through an
    /// aligned bounce buffer instead of being refused.
    pub fn direct_ok(&self, offset: u64, iovs: &[IoVec]) -> bool {
        let Some(a) = self.dio else {
            return true; // buffered I/O has no alignment to satisfy
        };
        if !offset.is_multiple_of(a.offset) {
            return false;
        }
        iovs.iter().all(|v| {
            (v.base() as u64).is_multiple_of(a.mem) && (v.len() as u64).is_multiple_of(a.offset)
        })
    }

    /// The shape of the bounce buffer a transfer of `len` bytes at `offset`
    /// would need: `(alignment, bytes before the request's own data, total)`.
    ///
    /// The window is widened to whole blocks around the request, because that
    /// is the only granularity direct I/O will accept.
    pub fn bounce_shape(&self, offset: u64, len: u64) -> (u64, u64, u64) {
        let Some(a) = self.dio else {
            return (1, 0, len);
        };
        let head = offset % a.offset;
        (
            a.mem.max(a.offset),
            head,
            (len + head).next_multiple_of(a.offset),
        )
    }

    /// What a file offset and a transfer length must be a multiple of.
    pub fn offset_align(&self) -> u64 {
        self.dio.map(|a| a.offset).unwrap_or(1)
    }
}

/// Ask the kernel what direct I/O on this path would require.
///
/// `STATX_DIOALIGN` is the only answer that is actually true of the file rather
/// than guessed from the device under it, and a zero offset alignment is the
/// kernel saying the file cannot do direct I/O at all -- which is how tmpfs,
/// some network filesystems and a compressed btrfs file report themselves.
fn probe_dio(path: &Path) -> Option<DioAlign> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statx` fills the buffer it is given; a zeroed one is a valid
    // starting state and the mask below says which fields to believe.
    let mut stx: libc::statx = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            0,
            libc::STATX_DIOALIGN,
            &mut stx,
        )
    };
    if rc != 0 {
        // Pre-6.1 kernel, or a filesystem that does not implement it. Treat as
        // unknown rather than as unsupported-and-therefore-buffered? No:
        // unknown alignment and direct I/O is how EINVAL on every request
        // happens, and that failure is silent until a guest cannot read.
        log::debug!(
            "virtio-blk: statx(STATX_DIOALIGN) on {path:?} failed: {}",
            std::io::Error::last_os_error()
        );
        return None;
    }
    if stx.stx_mask & libc::STATX_DIOALIGN == 0 {
        return None;
    }
    let (mem, offset) = (
        stx.stx_dio_mem_align as u64,
        stx.stx_dio_offset_align as u64,
    );
    if mem == 0 || offset == 0 {
        return None;
    }
    Some(DioAlign { mem, offset })
}

/// A regular file's size is its length; a block device's is not -- `stat` says
/// zero for one, and a VM backed by a logical volume would come up with a
/// zero-sector disk and no error anywhere.
fn size_of(file: &File) -> Result<u64> {
    let meta = file.metadata()?;
    if !meta.file_type().is_block_device() {
        return Ok(meta.len());
    }
    // BLKGETSIZE64
    const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
    let mut bytes: u64 = 0;
    // SAFETY: the ioctl writes one u64 through the pointer we pass.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), BLKGETSIZE64, &mut bytes) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("BLKGETSIZE64");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str, bytes: usize) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("nesbox-blk-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}-{}", std::process::id()));
        std::fs::write(&path, vec![0u8; bytes]).unwrap();
        path
    }

    #[test]
    fn a_file_shorter_than_a_sector_is_refused() {
        let path = scratch("tiny", 100);
        assert!(Disk::open(&path, true, CacheMode::Writeback).is_err());
    }

    #[test]
    fn capacity_is_in_512_byte_sectors_however_the_file_is_opened() {
        let path = scratch("sized", 8192);
        let disk = Disk::open(&path, true, CacheMode::Writeback).unwrap();
        assert_eq!(disk.sectors, 16);
        assert_eq!(disk.block_size, 512);
        assert!(disk.dio.is_none());
    }

    /// Buffered I/O has no alignment to satisfy, so nothing may be sent through
    /// a bounce buffer for one -- that would be a copy bought for nothing.
    #[test]
    fn buffered_accepts_any_alignment() {
        let path = scratch("unaligned", 8192);
        let disk = Disk::open(&path, true, CacheMode::Writeback).unwrap();
        let iov = IoVec::new(0x1001 as *mut u8, 37);
        assert!(disk.direct_ok(13, &[iov]));
    }

    /// Each segment is checked on its own, because the kernel checks each
    /// `iovec` on its own: one 37-byte segment in an otherwise aligned request
    /// is an `EINVAL` for the whole thing.
    #[test]
    fn direct_rejects_a_single_misaligned_segment() {
        let path = scratch("direct-align", 8192);
        let mut disk = Disk::open(&path, true, CacheMode::Writeback).unwrap();
        disk.dio = Some(DioAlign {
            mem: 512,
            offset: 512,
        });
        let good = IoVec::new(0x1000 as *mut u8, 4096);
        let short = IoVec::new(0x2000 as *mut u8, 37);
        assert!(disk.direct_ok(0, &[good]));
        assert!(!disk.direct_ok(0, &[good, short]));
        assert!(!disk.direct_ok(37, &[good]));
    }
}
