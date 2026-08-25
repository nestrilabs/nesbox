// What this guest's GPU device will tell a supervisor.
//
// A log line is not an interface: a supervisor needs to poll, and reading a
// number must not require parsing text a human wrote. So everything an operator
// needs is published to atomics as it happens, and snapshotted on demand.
//
// The counters here are deliberately raw and monotonic. Rates, percentiles and
// utilisation are the reader's job -- a snapshot that pre-computed "busy percent"
// would have to choose a window, and the right window depends on the question.

use std::sync::atomic::{AtomicU64, Ordering};

use super::occupancy::{Occupancy, OccupancyReader};

/// Live counters, written on the hot path and read by whoever is watching.
///
/// `Relaxed` throughout: these are statistics, not synchronisation. A reader that
/// sees a submit count from a microsecond ago is not wrong in any way that
/// matters, and making the hot path pay for ordering it does not need would be.
#[derive(Default)]
pub struct GpuCounters {
    /// Command streams handed to the renderer, refusals included.
    pub submits: AtomicU64,
    /// Streams the renderer rejected.
    pub submits_failed: AtomicU64,
    /// Fences signalled. With a frame count this is the guest's presentation rate.
    pub fences: AtomicU64,

    /// Device memory this guest currently holds, as accounted at `GEM_NEW`.
    pub vram_bytes: AtomicU64,
    /// High-water mark, which is what capacity planning wants rather than the
    /// instantaneous value.
    pub vram_peak_bytes: AtomicU64,
    /// The configured quota, or 0 for unbounded.
    pub vram_limit_bytes: AtomicU64,
    /// Allocations refused for exceeding the quota.
    pub vram_refusals: AtomicU64,
    /// GTT asked for. Counted, never enforced -- host memory is bounded for the
    /// whole process by cgroups.
    pub gtt_bytes: AtomicU64,

    /// Bytes currently mapped into the host-visible window.
    pub window_bytes: AtomicU64,
    pub window_peak_bytes: AtomicU64,
    /// The configured quota, or 0 for unbounded.
    pub window_limit_bytes: AtomicU64,
    /// Mappings live now. Each is a KVM memory slot, so this is the number that
    /// matters for slot pressure rather than the byte total.
    pub window_mappings: AtomicU64,
    pub window_refusals: AtomicU64,
}

impl GpuCounters {
    pub fn inc(field: &AtomicU64) {
        field.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set(field: &AtomicU64, v: u64) {
        field.store(v, Ordering::Relaxed);
    }
}

/// One consistent-enough view, for serialising.
///
/// "Consistent-enough" is honest rather than sloppy: the fields are read one after
/// another without a lock, so a snapshot can catch a submit that has landed while
/// its fence has not. Every alternative costs the hot path something, to fix
/// skew no consumer of these numbers can detect.
#[derive(Clone, Copy, Debug, Default)]
pub struct GpuSnapshot {
    pub submits: u64,
    pub submits_failed: u64,
    pub fences: u64,
    pub vram_bytes: u64,
    pub vram_peak_bytes: u64,
    pub vram_limit_bytes: u64,
    pub vram_refusals: u64,
    pub gtt_bytes: u64,
    pub window_bytes: u64,
    pub window_peak_bytes: u64,
    pub window_limit_bytes: u64,
    pub window_mappings: u64,
    pub window_refusals: u64,
    /// `None` until the guest has created its first GPU context, which is when
    /// the DRM client this reads comes into existence. Reported as absent rather
    /// than as zeroes, because zeroes look like an idle GPU.
    pub occupancy: Option<Occupancy>,
}

/// The device's metrics surface: counters plus the kernel's own accounting.
pub struct GpuMetrics {
    pub counters: GpuCounters,
    occupancy: OccupancyReader,
}

impl GpuMetrics {
    pub fn new() -> Self {
        Self {
            counters: GpuCounters::default(),
            occupancy: OccupancyReader::new(),
        }
    }

    pub fn snapshot(&self) -> GpuSnapshot {
        let c = &self.counters;
        let load = |f: &AtomicU64| f.load(Ordering::Relaxed);
        GpuSnapshot {
            submits: load(&c.submits),
            submits_failed: load(&c.submits_failed),
            fences: load(&c.fences),
            vram_bytes: load(&c.vram_bytes),
            vram_peak_bytes: load(&c.vram_peak_bytes),
            vram_limit_bytes: load(&c.vram_limit_bytes),
            vram_refusals: load(&c.vram_refusals),
            gtt_bytes: load(&c.gtt_bytes),
            window_bytes: load(&c.window_bytes),
            window_peak_bytes: load(&c.window_peak_bytes),
            window_limit_bytes: load(&c.window_limit_bytes),
            window_mappings: load(&c.window_mappings),
            window_refusals: load(&c.window_refusals),
            occupancy: self.occupancy.read(),
        }
    }
}

impl Default for GpuMetrics {
    fn default() -> Self {
        Self::new()
    }
}
