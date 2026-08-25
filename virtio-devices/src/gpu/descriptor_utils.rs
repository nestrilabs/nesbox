//! Descriptor chain reader and writer for the GPU device.
//!
//! GPU commands arrive as a chain of readable descriptors followed by writable
//! ones. The readable side is copied eagerly into a `Vec<u8>` — commands are
//! small, so the copy costs nothing worth avoiding and it makes the parsing
//! code a plain `Read`. The writable side goes straight into guest memory,
//! segment by segment, because responses can be large.
//!
//! Both take the descriptor list our virtqueues already produce: a slice of
//! `(guest address, length, flags)` in chain order.

use std::fmt;
use std::io::{self, Read, Write};
use std::mem::{MaybeUninit, size_of};

use vm_memory::{ByteValued, Bytes, GuestAddress, GuestMemoryError};

use vm_memory::GuestMemoryMmap;

use crate::common::VRING_DESC_F_WRITE;

/// One descriptor as our queues report it: guest address, length, flags.
pub type Descriptor = (u64, u32, u16);

fn is_write_only(desc: &Descriptor) -> bool {
    desc.2 & VRING_DESC_F_WRITE != 0
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Most bytes the readable half of one command's chain may total.
///
/// `Reader` copies eagerly and `resize` zero-fills, so every byte a chain claims
/// is host memory committed on the guest's say-so before anything has been
/// validated. Without a ceiling the only bound is the ring size times 4 GiB per
/// descriptor -- a terabyte from a guest that has to fill in nothing but length
/// fields, and an abort or a host OOM that reaches other tenants long before
/// that.
///
/// The largest legitimate reader on this queue is a `SUBMIT_3D` command stream,
/// orders of magnitude under this. 64 MiB is chosen to be unreachable by a
/// working guest rather than tuned to one: the point is to have a ceiling, not
/// to make it tight. (crosvm avoids the question by reading lazily, which is the
/// better fix and a much larger change.)
pub const MAX_READABLE_BYTES: usize = 64 << 20;

#[derive(Debug)]
pub enum Error {
    /// Descriptor chain length overflow.
    DescriptorChainOverflow,
    /// The readable half of the chain claims more than `MAX_READABLE_BYTES`.
    DescriptorChainTooLong(usize),
    /// Failed to access guest memory.
    GuestMemory(GuestMemoryError),
    /// Invalid descriptor chain.
    InvalidChain,
    /// I/O error.
    IoError(io::Error),
    /// Split offset is out of bounds.
    SplitOutOfBounds(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::DescriptorChainOverflow => write!(
                f,
                "combined length of all buffers in DescriptorChain would overflow"
            ),
            Error::DescriptorChainTooLong(len) => write!(
                f,
                "readable descriptors total {len} bytes, over the {MAX_READABLE_BYTES}-byte limit"
            ),
            Error::GuestMemory(e) => write!(f, "descriptor guest memory error: {e}"),
            Error::InvalidChain => write!(f, "invalid descriptor chain"),
            Error::IoError(e) => write!(f, "descriptor I/O error: {e}"),
            Error::SplitOutOfBounds(off) => {
                write!(f, "DescriptorChain split is out of bounds: {off}")
            }
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------
//
// Reads the *readable* portion of a descriptor chain (descriptors that do NOT
// have VIRTQ_DESC_F_WRITE set, i.e. host-readable / driver-written data).
// The virtio spec requires these to precede any writable descriptors.

#[derive(Debug)]
pub struct Reader {
    /// Eagerly-copied contents of all readable descriptors. Its length is the
    /// total across the chain; the cursor tracks how much has been consumed.
    buf: std::io::Cursor<Vec<u8>>,
}

impl Reader {
    /// Construct a Reader from a descriptor chain.
    ///
    /// Only readable (non-writable) descriptors are included; iteration stops
    /// at the first writable descriptor as required by the virtio spec.
    pub fn new(mem: &GuestMemoryMmap, chain: &[Descriptor]) -> Result<Self> {
        let mut data: Vec<u8> = Vec::new();
        // Accumulated only to catch a chain whose lengths overflow; the copied
        // buffer is what everything downstream reads.
        let mut total_len: usize = 0;

        for desc in chain.iter().take_while(|d| !is_write_only(d)) {
            let len = desc.1 as usize;
            total_len = total_len
                .checked_add(len)
                .ok_or(Error::DescriptorChainOverflow)?;
            // Checked before the resize below, which is the allocation. Doing it
            // after would be checking a number the host had already committed.
            if total_len > MAX_READABLE_BYTES {
                return Err(Error::DescriptorChainTooLong(total_len));
            }

            let start = data.len();
            data.resize(start + len, 0u8);

            mem.read_slice(&mut data[start..start + len], GuestAddress(desc.0))
                .map_err(Error::GuestMemory)?;
        }

        Ok(Reader {
            buf: std::io::Cursor::new(data),
        })
    }

    /// Read a `ByteValued` object directly from the descriptor stream.
    pub fn read_obj<T: ByteValued>(&mut self) -> io::Result<T> {
        let mut obj = MaybeUninit::<T>::uninit();
        // SAFETY: We write exactly `size_of::<T>()` bytes via read_exact before
        // calling assume_init, and ByteValued guarantees all bit patterns are valid.
        let buf =
            unsafe { std::slice::from_raw_parts_mut(obj.as_mut_ptr() as *mut u8, size_of::<T>()) };
        self.read_exact(buf)?;
        Ok(unsafe { obj.assume_init() })
    }

    /// Bytes remaining before the end of the readable descriptor area.
    pub fn available_bytes(&self) -> usize {
        self.buf.get_ref().len() - self.buf.position() as usize
    }

    /// Bytes already consumed from the readable descriptor area.
    pub fn bytes_read(&self) -> usize {
        self.buf.position() as usize
    }
}

impl io::Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.buf.read(buf)
    }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------
//
// Writes into the *writable* portion of a descriptor chain (descriptors with
// VIRTQ_DESC_F_WRITE set).  Writes go directly to guest memory.

#[derive(Debug)]
pub struct Writer<'a> {
    mem: &'a GuestMemoryMmap,
    /// (guest_addr, byte_length) for every writable descriptor segment,
    /// in order.
    segments: Vec<(GuestAddress, usize)>,
    /// Current write position measured from the start of the writable region.
    write_pos: usize,
    /// Cumulative bytes written (same as write_pos unless seek is used).
    bytes_written: usize,
    /// Total bytes available across all writable segments.
    total_len: usize,
}

impl<'a> Writer<'a> {
    /// Construct a Writer from a descriptor chain.
    ///
    /// Only writable (VIRTQ_DESC_F_WRITE) descriptors are included.
    pub fn new(mem: &'a GuestMemoryMmap, chain: &[Descriptor]) -> Result<Self> {
        let mut segments: Vec<(GuestAddress, usize)> = Vec::new();
        let mut total_len: usize = 0;

        for desc in chain.iter().skip_while(|d| !is_write_only(d)) {
            let len = desc.1 as usize;
            total_len = total_len
                .checked_add(len)
                .ok_or(Error::DescriptorChainOverflow)?;
            segments.push((GuestAddress(desc.0), len));
        }

        Ok(Writer {
            mem,
            segments,
            write_pos: 0,
            bytes_written: 0,
            total_len,
        })
    }

    /// Write a `ByteValued` object directly into the descriptor stream.
    pub fn write_obj<T: ByteValued>(&mut self, val: T) -> io::Result<()> {
        self.write_all(val.as_slice())
    }

    /// Bytes remaining before the end of the writable descriptor area.
    pub fn available_bytes(&self) -> usize {
        self.total_len.saturating_sub(self.write_pos)
    }

    /// Bytes already written into the writable descriptor area.
    pub fn bytes_written(&self) -> usize {
        self.bytes_written
    }
}

impl io::Write for Writer<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut buf_pos = 0usize;
        // `remaining_skip` tracks how many bytes of the writable region to
        // skip over before we start writing (fast-forward to write_pos).
        let mut remaining_skip = self.write_pos;

        for &(seg_addr, seg_len) in &self.segments {
            if buf_pos >= buf.len() {
                break;
            }

            // Skip complete segments that are before the current write position.
            if remaining_skip >= seg_len {
                remaining_skip -= seg_len;
                continue;
            }

            // We are now in the first segment that overlaps write_pos.
            let in_seg_start = remaining_skip;
            remaining_skip = 0; // consumed – next segments start from offset 0

            let seg_remaining = seg_len - in_seg_start;
            let buf_remaining = buf.len() - buf_pos;
            let to_write = seg_remaining.min(buf_remaining);

            let write_addr = GuestAddress(seg_addr.0 + in_seg_start as u64);
            self.mem
                .write_slice(&buf[buf_pos..buf_pos + to_write], write_addr)
                .map_err(|e| io::Error::other(format!("guest memory: {e}")))?;

            buf_pos += to_write;
        }

        self.write_pos += buf_pos;
        self.bytes_written += buf_pos;
        Ok(buf_pos)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Writes go straight to guest memory.
        Ok(())
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;
    use vm_memory::GuestAddress as GA;

    fn mem() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GA(0), 0x20000)]).unwrap()
    }

    /// The whole point of the ceiling. Every descriptor here points at real,
    /// readable guest memory, so nothing else in the path would refuse it --
    /// only the total is unreasonable, and the total is what the host pays.
    #[test]
    fn a_chain_claiming_more_than_the_ceiling_is_refused() {
        // Lengths a guest can write freely; the addresses do not even have to
        // be distinct.
        let huge = [(0u64, u32::MAX, 0u16); 64];
        match Reader::new(&mem(), &huge) {
            Err(Error::DescriptorChainTooLong(n)) => {
                assert!(n > MAX_READABLE_BYTES, "{n}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// And it is refused *before* the allocation, which is the part that
    /// matters: checking afterwards would be checking a number the host had
    /// already committed. A single descriptor over the line is enough.
    #[test]
    fn the_refusal_comes_before_anything_is_allocated() {
        let one_over = [(0u64, (MAX_READABLE_BYTES + 1) as u32, 0u16)];
        assert!(matches!(
            Reader::new(&mem(), &one_over),
            Err(Error::DescriptorChainTooLong(_))
        ));
    }

    /// An ordinary chain still reads, and reads what it was given.
    #[test]
    fn an_ordinary_chain_is_untouched() {
        let m = mem();
        m.write_slice(&[0xab; 32], GA(0x1000)).unwrap();
        let mut r = Reader::new(&m, &[(0x1000, 32, 0)]).expect("a small chain reads");
        assert_eq!(r.available_bytes(), 32);
        assert_eq!(r.read_obj::<u32>().unwrap(), 0xabab_abab);
    }
}
