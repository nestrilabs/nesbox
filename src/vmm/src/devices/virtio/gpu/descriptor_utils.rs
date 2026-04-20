// Copyright 2024 - Firecracker GPU port
// Descriptor chain reader / writer for the GPU virtio device.
//
// Unlike the vm_memory VolatileSlice-based descriptor_utils used elsewhere,
// this version is adapted for Firecracker's raw-pointer Queue / DescriptorChain
// model.  The readable side eagerly copies descriptor data into a Vec<u8>
// (GPU commands are small, so a copy is acceptable).  The writable side writes
// directly into guest memory segment-by-segment using get_slice / copy_from.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::mem::{MaybeUninit, size_of};

use vm_memory::{ByteValued, GuestAddress, GuestMemory, GuestMemoryError};

use crate::devices::virtio::queue::DescriptorChain;
use crate::vstate::memory::GuestMemoryMmap;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Error {
    /// Descriptor chain length overflow.
    DescriptorChainOverflow,
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

pub struct Reader {
    /// Eagerly-copied contents of all readable descriptors.
    buf: std::io::Cursor<Vec<u8>>,
    /// Total byte count across all readable descriptors.
    total_len: usize,
}

impl Reader {
    /// Construct a Reader from a descriptor chain.
    ///
    /// Only readable (non-writable) descriptors are included; iteration stops
    /// at the first writable descriptor as required by the virtio spec.
    pub fn new(mem: &GuestMemoryMmap, chain: DescriptorChain) -> Result<Self> {
        let mut data: Vec<u8> = Vec::new();
        let mut total_len: usize = 0;

        for desc in chain.into_iter().take_while(|d| !d.is_write_only()) {
            let len = desc.len as usize;
            total_len = total_len
                .checked_add(len)
                .ok_or(Error::DescriptorChainOverflow)?;

            let start = data.len();
            data.resize(start + len, 0u8);

            // get_slice returns a VolatileSlice whose concrete bitmap type is
            // inferred from GuestMemoryMmap – we never name it explicitly.
            let slice = mem.get_slice(desc.addr, len).map_err(Error::GuestMemory)?;
            slice.copy_to(&mut data[start..start + len]);
        }

        Ok(Reader {
            buf: std::io::Cursor::new(data),
            total_len,
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
    pub fn new(mem: &'a GuestMemoryMmap, chain: DescriptorChain) -> Result<Self> {
        let mut segments: Vec<(GuestAddress, usize)> = Vec::new();
        let mut total_len: usize = 0;

        for desc in chain.into_iter().skip_while(|d| !d.is_write_only()) {
            let len = desc.len as usize;
            total_len = total_len
                .checked_add(len)
                .ok_or(Error::DescriptorChainOverflow)?;
            segments.push((desc.addr, len));
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
            let slice = self
                .mem
                .get_slice(write_addr, to_write)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("guest memory: {e}")))?;
            slice.copy_from(&buf[buf_pos..buf_pos + to_write]);

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
