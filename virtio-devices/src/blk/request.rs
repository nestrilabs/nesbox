//! Turning a descriptor chain into something the I/O engine can issue.
//!
//! A virtio-blk request is a 16-byte header the driver wrote, zero or more data
//! segments, and a one-byte status the device writes. Everything in the chain
//! is a guest address the guest chose, so every one of them is checked against
//! guest memory before it becomes a pointer the kernel will follow.

use super::disk::{Disk, SECTOR_SIZE};
use crate::common::VRING_DESC_F_WRITE;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

pub const BLK_T_IN: u32 = 0;
pub const BLK_T_OUT: u32 = 1;
pub const BLK_T_FLUSH: u32 = 4;
pub const BLK_T_GET_ID: u32 = 8;
pub const BLK_T_DISCARD: u32 = 11;
pub const BLK_T_WRITE_ZEROES: u32 = 13;

pub const BLK_S_OK: u8 = 0;
pub const BLK_S_IOERR: u8 = 1;
pub const BLK_S_UNSUPP: u8 = 2;

/// The serial number the guest reads back from `/sys/block/vda/serial`.
pub const DISK_ID: &[u8; 20] = b"nesbox-vda\0\0\0\0\0\0\0\0\0\0";

/// One `virtio_blk_discard_write_zeroes` segment.
pub const DISCARD_SEG_SIZE: u32 = 16;
/// The `unmap` bit in such a segment's flags.
const DISCARD_F_UNMAP: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
    Flush,
    GetId,
    /// Hole-punch. `unmap` is implied.
    Discard {
        sector: u64,
        sectors: u32,
    },
    /// Zero a range, punching a hole where the driver allowed it.
    WriteZeroes {
        sector: u64,
        sectors: u32,
        unmap: bool,
    },
}

/// A parsed request: what to do, where in the file, and which guest pages the
/// data goes to or comes from.
pub struct Request {
    pub head: u16,
    pub op: Op,
    /// Byte offset into the backing file, for `Read`/`Write`.
    pub offset: u64,
    /// Guest data segments, in order.
    pub segments: Vec<(u64, u32)>,
    pub total: u64,
    /// Where the one-byte status goes.
    pub status_addr: u64,
}

/// Why a chain could not become a request.
///
/// The distinction that matters is whether we found somewhere to put a status
/// byte: with one, the guest gets a failure it can report; without one, all the
/// device can do is return the chain and say nothing, because writing a status
/// into an address the guest did not offer is exactly the bug this device must
/// not have.
#[derive(Debug)]
pub enum ParseError {
    /// The chain is unusable. Complete it with no status written.
    Malformed(&'static str),
    /// The chain is well-formed and the request is not. Complete it with this
    /// status at this address.
    Rejected(u64, u8),
}

/// Bound on how much one request may ask for, as a guard against a chain whose
/// segment lengths sum to something absurd. 128 MiB is far above anything a
/// Linux guest issues (its own limit is `seg_max` pages) and far below anything
/// that would matter to the host.
const MAX_REQUEST_BYTES: u64 = 128 << 20;

pub fn parse(
    mem: &GuestMemoryMmap,
    disk: &Disk,
    head: u16,
    descs: &[(u64, u32, u16)],
) -> Result<Request, ParseError> {
    // Header, at least one status byte. A chain shorter than that has nowhere
    // to report anything to.
    if descs.len() < 2 {
        return Err(ParseError::Malformed("chain shorter than header + status"));
    }
    let (header_addr, header_len, _) = descs[0];
    let (status_addr, status_len, status_flags) = *descs.last().unwrap();
    if status_len < 1 || status_flags & VRING_DESC_F_WRITE == 0 {
        return Err(ParseError::Malformed(
            "last descriptor is not a writable status byte",
        ));
    }
    // From here on there is somewhere to put a status, so failures are
    // reported to the guest rather than swallowed.
    if header_len < 16 {
        return Err(ParseError::Rejected(status_addr, BLK_S_IOERR));
    }
    let Ok(kind) = mem
        .read_obj::<u32>(GuestAddress(header_addr))
        .map(u32::from_le)
    else {
        return Err(ParseError::Rejected(status_addr, BLK_S_IOERR));
    };
    let Ok(sector) = mem
        .read_obj::<u64>(GuestAddress(header_addr + 8))
        .map(u64::from_le)
    else {
        return Err(ParseError::Rejected(status_addr, BLK_S_IOERR));
    };

    let data = &descs[1..descs.len() - 1];
    let mut segments = Vec::with_capacity(data.len());
    let mut total: u64 = 0;
    for &(addr, len, _) in data {
        if len == 0 {
            continue;
        }
        // The check that makes the rest of this device safe: a segment that is
        // not entirely inside guest memory never becomes a host pointer. It is
        // done here, once, rather than at each use.
        if mem.get_slice(GuestAddress(addr), len as usize).is_err() {
            return Err(ParseError::Rejected(status_addr, BLK_S_IOERR));
        }
        total += len as u64;
        if total > MAX_REQUEST_BYTES {
            return Err(ParseError::Rejected(status_addr, BLK_S_IOERR));
        }
        segments.push((addr, len));
    }

    let reject = |status: u8| Err(ParseError::Rejected(status_addr, status));

    let (op, offset) = match kind {
        BLK_T_IN | BLK_T_OUT => {
            let write = kind == BLK_T_OUT;
            if write && disk.read_only {
                return reject(BLK_S_IOERR);
            }
            // `sector * 512` on a guest-chosen sector is the one multiplication
            // here that can wrap, and a wrapped offset reads the wrong part of
            // the image rather than failing.
            let Some(offset) = sector.checked_mul(SECTOR_SIZE) else {
                return reject(BLK_S_IOERR);
            };
            // Past the capacity we advertised. A read could be answered with
            // zeroes and a write cannot be answered at all, so both are
            // refused: a guest doing this has already gone wrong.
            if offset.saturating_add(total) > disk.len_bytes() {
                return reject(BLK_S_IOERR);
            }
            (if write { Op::Write } else { Op::Read }, offset)
        }
        BLK_T_FLUSH => (Op::Flush, 0),
        BLK_T_GET_ID => (Op::GetId, 0),
        BLK_T_DISCARD | BLK_T_WRITE_ZEROES => {
            if disk.read_only {
                return reject(BLK_S_IOERR);
            }
            // One segment per request, which is what `max_discard_seg` and
            // `max_write_zeroes_seg` advertise. A driver that sends more is
            // sending something we said we would not take.
            if segments.len() != 1 || segments[0].1 < DISCARD_SEG_SIZE {
                return reject(BLK_S_IOERR);
            }
            let (addr, _) = segments[0];
            let Ok(seg) = read_discard_segment(mem, addr) else {
                return reject(BLK_S_IOERR);
            };
            let (start, count, flags) = seg;
            let Some(byte_off) = start.checked_mul(SECTOR_SIZE) else {
                return reject(BLK_S_IOERR);
            };
            let byte_len = count as u64 * SECTOR_SIZE;
            if byte_off.saturating_add(byte_len) > disk.len_bytes() {
                return reject(BLK_S_IOERR);
            }
            let op = if kind == BLK_T_DISCARD {
                // Every flag bit is reserved for a discard -- `unmap` belongs
                // to write-zeroes -- and the spec says a driver must not set
                // one. Refusing beats guessing what it meant.
                if flags != 0 {
                    return reject(BLK_S_UNSUPP);
                }
                Op::Discard {
                    sector: start,
                    sectors: count,
                }
            } else {
                Op::WriteZeroes {
                    sector: start,
                    sectors: count,
                    unmap: flags & DISCARD_F_UNMAP != 0,
                }
            };
            (op, byte_off)
        }
        _ => return reject(BLK_S_UNSUPP),
    };

    Ok(Request {
        head,
        op,
        offset,
        segments,
        total,
        status_addr,
    })
}

fn read_discard_segment(
    mem: &GuestMemoryMmap,
    addr: u64,
) -> Result<(u64, u32, u32), vm_memory::GuestMemoryError> {
    let sector = u64::from_le(mem.read_obj::<u64>(GuestAddress(addr))?);
    let count = u32::from_le(mem.read_obj::<u32>(GuestAddress(addr + 8))?);
    let flags = u32::from_le(mem.read_obj::<u32>(GuestAddress(addr + 12))?);
    Ok((sector, count, flags))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::VRING_DESC_F_WRITE;

    fn mem() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x2_0000)]).unwrap()
    }

    /// One file per test: they run on threads of one process, and a shared
    /// name means one test truncating the image another is opening.
    fn disk(name: &str) -> Disk {
        let dir = std::env::temp_dir().join("nesbox-blk-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("request-{name}-{}", std::process::id()));
        std::fs::write(&path, vec![0u8; 64 * 1024]).unwrap();
        Disk::open(&path, false, super::super::disk::CacheMode::Writeback).unwrap()
    }

    fn header(mem: &GuestMemoryMmap, at: u64, kind: u32, sector: u64) {
        mem.write_obj(u32::to_le(kind), GuestAddress(at)).unwrap();
        mem.write_obj(u64::to_le(sector), GuestAddress(at + 8))
            .unwrap();
    }

    /// A chain whose last descriptor is not device-writable has nowhere to
    /// report to, and the device must not invent somewhere.
    #[test]
    fn a_chain_without_a_status_descriptor_is_malformed() {
        let m = mem();
        let d = disk("a_chain_without_a_status_descriptor_is_malformed");
        header(&m, 0x1000, BLK_T_IN, 0);
        let descs = [(0x1000, 16, 0u16), (0x2000, 512, 0u16)];
        assert!(matches!(
            parse(&m, &d, 0, &descs),
            Err(ParseError::Malformed(_))
        ));
    }

    /// A read past the end of the disk gets a status, not a short read and not
    /// a panic.
    #[test]
    fn a_read_past_the_capacity_is_rejected_with_a_status() {
        let m = mem();
        let d = disk("a_read_past_the_capacity_is_rejected_with_a_status");
        header(&m, 0x1000, BLK_T_IN, 1_000_000);
        let descs = [
            (0x1000, 16, 0u16),
            (0x2000, 512, VRING_DESC_F_WRITE),
            (0x3000, 1, VRING_DESC_F_WRITE),
        ];
        match parse(&m, &d, 0, &descs) {
            Err(ParseError::Rejected(addr, status)) => {
                assert_eq!(addr, 0x3000);
                assert_eq!(status, BLK_S_IOERR);
            }
            _ => panic!("expected a rejection with a status"),
        }
    }

    /// `sector * 512` is the one place a guest number can wrap, and a wrapped
    /// offset would read a different part of the image rather than fail.
    #[test]
    fn a_sector_that_would_wrap_the_byte_offset_is_rejected() {
        let m = mem();
        let d = disk("a_sector_that_would_wrap_the_byte_offset_is_rejected");
        header(&m, 0x1000, BLK_T_IN, u64::MAX / 2);
        let descs = [
            (0x1000, 16, 0u16),
            (0x2000, 512, VRING_DESC_F_WRITE),
            (0x3000, 1, VRING_DESC_F_WRITE),
        ];
        assert!(matches!(
            parse(&m, &d, 0, &descs),
            Err(ParseError::Rejected(_, BLK_S_IOERR))
        ));
    }

    /// A data segment that is not entirely inside guest memory must never
    /// become a pointer the kernel follows.
    #[test]
    fn a_segment_outside_guest_memory_is_rejected() {
        let m = mem();
        let d = disk("a_segment_outside_guest_memory_is_rejected");
        header(&m, 0x1000, BLK_T_IN, 0);
        let descs = [
            (0x1000, 16, 0u16),
            (0x1_FF00, 0x1000, VRING_DESC_F_WRITE),
            (0x3000, 1, VRING_DESC_F_WRITE),
        ];
        assert!(matches!(
            parse(&m, &d, 0, &descs),
            Err(ParseError::Rejected(_, BLK_S_IOERR))
        ));
    }

    #[test]
    fn an_ordinary_read_parses() {
        let m = mem();
        let d = disk("an_ordinary_read_parses");
        header(&m, 0x1000, BLK_T_IN, 8);
        let descs = [
            (0x1000, 16, 0u16),
            (0x2000, 4096, VRING_DESC_F_WRITE),
            (0x3000, 1, VRING_DESC_F_WRITE),
        ];
        let r = parse(&m, &d, 7, &descs).unwrap();
        assert_eq!(r.op, Op::Read);
        assert_eq!(r.offset, 4096);
        assert_eq!(r.total, 4096);
        assert_eq!(r.head, 7);
        assert_eq!(r.status_addr, 0x3000);
    }

    /// A read-only drive advertises `VIRTIO_BLK_F_RO`, so a write to one is a
    /// driver ignoring what it was told -- reported, never carried out.
    #[test]
    fn a_write_to_a_read_only_disk_is_refused() {
        let dir = std::env::temp_dir().join("nesbox-blk-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("ro-disk-{}", std::process::id()));
        std::fs::write(&path, vec![0u8; 64 * 1024]).unwrap();
        let d = Disk::open(&path, true, super::super::disk::CacheMode::Writeback).unwrap();
        let m = mem();
        header(&m, 0x1000, BLK_T_OUT, 0);
        let descs = [
            (0x1000, 16, 0u16),
            (0x2000, 512, 0),
            (0x3000, 1, VRING_DESC_F_WRITE),
        ];
        assert!(matches!(
            parse(&m, &d, 0, &descs),
            Err(ParseError::Rejected(_, BLK_S_IOERR))
        ));
    }

    #[test]
    fn a_discard_segment_parses_into_a_byte_range() {
        let m = mem();
        let d = disk("a_discard_segment_parses_into_a_byte_range");
        header(&m, 0x1000, BLK_T_DISCARD, 0);
        m.write_obj(u64::to_le(16), GuestAddress(0x2000)).unwrap();
        m.write_obj(u32::to_le(8), GuestAddress(0x2008)).unwrap();
        m.write_obj(u32::to_le(0), GuestAddress(0x200c)).unwrap();
        let descs = [
            (0x1000, 16, 0u16),
            (0x2000, 16, 0),
            (0x3000, 1, VRING_DESC_F_WRITE),
        ];
        let r = parse(&m, &d, 0, &descs).unwrap();
        assert_eq!(
            r.op,
            Op::Discard {
                sector: 16,
                sectors: 8
            }
        );
        assert_eq!(r.offset, 8192);
    }
}
