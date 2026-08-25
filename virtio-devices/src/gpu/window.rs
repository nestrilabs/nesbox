// The host-visible window: how much of a guest's GPU memory it may reach directly.
//
// A blob resource the guest wants to touch with the CPU is mapped into BAR2 and
// registered with KVM, so afterwards the guest reads and writes it without
// trapping to us at all. That is what makes it fast, and it is also why it needs
// a bound: every mapping costs host address space and a KVM memory slot, and
// nothing in the protocol makes a guest ask for a reasonable number of them.
//
// # This one can be refused properly
//
// Unlike a VRAM allocation (see `vram.rs`), `RESOURCE_MAP_BLOB` is
// **synchronous**: the guest kernel waits for `RESP_OK_MAP_INFO` because it needs
// the offset before userspace can mmap anything. So a refusal here reaches the
// guest as a failed mmap, which Mesa reports as a failed buffer map, which is an
// ordinary error an application already handles.
//
// That makes this the one GPU bound where the guest-visible failure and the host
// bound are the same event.
//
// # Two resources, two limits
//
// **Bytes** is the documented quota, because host address space is the thing a
// tier is sized against.
//
// **Mapping count** is bounded separately and defaults to unbounded. Each mapping
// is a KVM memory slot, and KVM has a few thousand; a guest that maps single
// pages could exhaust them. That already fails safely -- KVM refuses the slot and
// the map returns an error -- so the count is *reported* by default rather than
// capped, because no measurement yet says what a real workload needs and a cap
// guessed too low breaks it.

use std::sync::Arc;

use super::metrics::{GpuCounters, GpuMetrics};

/// Why a mapping was refused.
#[derive(Debug)]
pub enum Refused {
    Bytes {
        requested: u64,
        used: u64,
        limit: u64,
    },
    Count {
        used: u32,
        limit: u32,
    },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Bytes {
                requested,
                used,
                limit,
            } => write!(
                f,
                "host-visible window full: {} KiB requested with {} of {} MiB mapped",
                requested / 1024,
                used / (1 << 20),
                limit / (1 << 20)
            ),
            Refused::Count { used, limit } => write!(
                f,
                "host-visible window has {used} of {limit} mappings in use"
            ),
        }
    }
}

/// Tracks what a guest currently has mapped into its window.
pub struct WindowQuota {
    /// 0 means unbounded.
    limit_bytes: u64,
    /// 0 means unbounded.
    max_mappings: u32,
    used: u64,
    mappings: u32,
    peak: u64,
    refusals: u64,
    metrics: Arc<GpuMetrics>,
}

impl WindowQuota {
    pub fn new(limit_bytes: u64, max_mappings: u32, metrics: Arc<GpuMetrics>) -> Self {
        GpuCounters::set(&metrics.counters.window_limit_bytes, limit_bytes);
        Self {
            limit_bytes,
            max_mappings,
            used: 0,
            mappings: 0,
            peak: 0,
            refusals: 0,
            metrics,
        }
    }

    /// Take `size` from the quota, or say why not.
    ///
    /// Charged before the mapping is made, so a refusal never leaves host address
    /// space committed -- the opposite of the ordering problem `vram.rs` has.
    pub fn try_map(&mut self, size: u64) -> Result<(), Refused> {
        if self.limit_bytes != 0 {
            let after = self.used.saturating_add(size);
            if after > self.limit_bytes {
                self.refusals += 1;
                self.publish();
                return Err(Refused::Bytes {
                    requested: size,
                    used: self.used,
                    limit: self.limit_bytes,
                });
            }
        }
        if self.max_mappings != 0 && self.mappings >= self.max_mappings {
            self.refusals += 1;
            self.publish();
            return Err(Refused::Count {
                used: self.mappings,
                limit: self.max_mappings,
            });
        }

        self.used = self.used.saturating_add(size);
        self.mappings += 1;
        self.peak = self.peak.max(self.used);
        self.publish();
        Ok(())
    }

    /// Give it back. Called from both release paths -- an explicit unmap and a
    /// resource being unreffed while still mapped.
    pub fn release(&mut self, size: u64) {
        self.used = self.used.saturating_sub(size);
        self.mappings = self.mappings.saturating_sub(1);
        self.publish();
    }

    fn publish(&self) {
        let c = &self.metrics.counters;
        GpuCounters::set(&c.window_bytes, self.used);
        GpuCounters::set(&c.window_peak_bytes, self.peak);
        GpuCounters::set(&c.window_mappings, self.mappings as u64);
        GpuCounters::set(&c.window_refusals, self.refusals);
    }

    pub fn summary(&self) -> String {
        let limit = if self.limit_bytes == 0 {
            "unbounded".to_string()
        } else {
            format!("{} MiB", self.limit_bytes / (1 << 20))
        };
        format!(
            "window {} MiB/{limit} in {} mappings (peak {} MiB, {} refused)",
            self.used / (1 << 20),
            self.mappings,
            self.peak / (1 << 20),
            self.refusals
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn q(limit_mib: u64, max_maps: u32) -> WindowQuota {
        WindowQuota::new(limit_mib * MIB, max_maps, Arc::new(GpuMetrics::new()))
    }

    #[test]
    fn bounds_bytes_and_gives_them_back() {
        let mut w = q(16, 0);
        assert!(w.try_map(8 * MIB).is_ok());
        assert!(w.try_map(8 * MIB).is_ok());
        assert!(matches!(w.try_map(1), Err(Refused::Bytes { .. })));
        assert_eq!(w.used, 16 * MIB);

        w.release(8 * MIB);
        // The freed room is usable again, and the peak remembers the high water.
        assert!(w.try_map(8 * MIB).is_ok());
        assert_eq!(w.peak, 16 * MIB);
        assert_eq!(w.refusals, 1);
    }

    #[test]
    fn a_refusal_commits_nothing() {
        let mut w = q(16, 0);
        w.try_map(12 * MIB).unwrap();
        let _ = w.try_map(8 * MIB);
        // Not 20 MiB: the mapping was never made, so the quota must not hold it.
        assert_eq!(w.used, 12 * MIB);
        assert_eq!(w.mappings, 1);
    }

    #[test]
    fn zero_means_unbounded_for_both_limits() {
        let mut w = q(0, 0);
        for _ in 0..1000 {
            assert!(w.try_map(64 * MIB).is_ok());
        }
        assert_eq!(w.mappings, 1000);
        assert_eq!(w.refusals, 0);
    }

    #[test]
    fn a_mapping_cap_bounds_count_independently_of_size() {
        // The memslot-exhaustion shape: tiny mappings, lots of them.
        let mut w = q(0, 4);
        for _ in 0..4 {
            assert!(w.try_map(4096).is_ok());
        }
        assert!(matches!(w.try_map(4096), Err(Refused::Count { .. })));
        w.release(4096);
        assert!(w.try_map(4096).is_ok());
    }

    #[test]
    fn a_size_that_would_overflow_cannot_wrap_past_the_limit() {
        let mut w = q(16, 0);
        assert!(matches!(w.try_map(u64::MAX), Err(Refused::Bytes { .. })));
        assert_eq!(w.used, 0);
    }

    #[test]
    fn releasing_more_than_was_taken_saturates_rather_than_wrapping() {
        // Belt and braces: the two release paths must not double-credit into a
        // huge `used` that then refuses everything forever.
        let mut w = q(16, 0);
        w.try_map(4 * MIB).unwrap();
        w.release(4 * MIB);
        w.release(4 * MIB);
        assert_eq!(w.used, 0);
        assert_eq!(w.mappings, 0);
        assert!(w.try_map(16 * MIB).is_ok());
    }

    /// Why `resource_map_blob` refuses an already-mapped resource.
    ///
    /// This models what used to happen: a guest maps the same resource twice, so
    /// it is charged twice, and the single unmap credits it once. Nothing here is
    /// wrong -- `WindowQuota` is doing exactly what it was told -- which is the
    /// point. The accounting cannot detect the duplicate; only the caller knows
    /// the resource was already mapped, so the guard belongs there and this test
    /// exists so that removing it has a visible consequence.
    #[test]
    fn charging_twice_and_crediting_once_drains_the_quota() {
        let mut w = q(16, 0);
        let size = 8 * MIB;

        w.try_map(size).unwrap();
        w.try_map(size).unwrap(); // the duplicate the caller must prevent
        w.release(size); // one unmap, because there is one shmem_offset

        assert_eq!(w.used, size, "8 MiB is still held by a mapping that is gone");
        assert_eq!(w.mappings, 1);

        // And it is cumulative, so a loop closes the window entirely.
        w.try_map(size).unwrap();
        assert!(matches!(w.try_map(1), Err(Refused::Bytes { .. })));
    }

    #[test]
    fn the_summary_says_unbounded_rather_than_zero() {
        assert!(q(0, 0).summary().contains("unbounded"));
        assert!(q(64, 0).summary().contains("64 MiB"));
    }
}
