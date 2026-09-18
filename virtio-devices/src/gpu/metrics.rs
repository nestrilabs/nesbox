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
use std::time::Instant;

use super::occupancy::{Occupancy, OccupancyReader};

/// Time spent somewhere, and how many times it was spent.
///
/// # Why a sum and a count rather than a mean or a histogram
///
/// A mean computed here would be a mean since boot, which answers nothing about
/// what is happening now; two snapshots of a sum and a count give the mean over
/// whatever window the reader chose, the same way `gfx_ns` already works. A
/// histogram would answer more -- the tail is the interesting part of several of
/// these -- and costs a bucket search on a path taken tens of thousands of times
/// a second. That is a trade worth making once something here says which phase
/// deserves it.
#[derive(Default)]
pub struct Phase {
    pub ns: AtomicU64,
    pub count: AtomicU64,
}

impl Phase {
    /// Record one occurrence, from `since` to now.
    pub fn since(&self, since: Instant) {
        self.add(since.elapsed().as_nanos() as u64);
    }

    pub fn add(&self, ns: u64) {
        self.ns.fetch_add(ns, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    fn read(&self) -> PhaseSnapshot {
        PhaseSnapshot {
            ns: self.ns.load(Ordering::Relaxed),
            count: self.count.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PhaseSnapshot {
    pub ns: u64,
    pub count: u64,
}

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
    /// GTT asked for. Counted, never enforced. Whether host memory is bounded
    /// at all is the supervisor's cgroup to set; `vmm/src/isolation.rs` reports
    /// what is actually in force and warns when nothing is.
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

    // ── Where the worker's time goes ────────────────────────────────────────
    //
    // Together these account for the whole of the worker thread: it is either
    // spinning, blocked, or draining. `top` cannot separate the first two --
    // a spin is indistinguishable from work in `%CPU`, which is what made an
    // earlier reading of that column mean the opposite of what it seemed to.
    /// The poll spin in `Worker::wait`, whether or not it found anything.
    pub spin: Phase,
    /// Blocked in `poll` on the doorbell.
    pub sleep: Phase,
    /// One pass of `process_ctl_queue`, however many descriptors it took.
    pub drain: Phase,
    /// Descriptors taken across all drains. Against `drain.count` this is the
    /// batch size, which is what says whether retiring in batches is buying
    /// anything.
    pub drained: AtomicU64,

    // ── Where a command's time goes ─────────────────────────────────────────
    /// Every command dispatched, of any kind. Against `submits` this says how
    /// much of the traffic is command submission and how much is everything
    /// else the native context forwards.
    pub command: Phase,
    /// Inside `rutabaga.submit_command` alone.
    pub submit: Phase,
    /// Inside `vram.observe_submit`, which parses every command stream that
    /// passes. On the hot path and worth knowing the price of.
    pub observe: Phase,
    /// Inside `rutabaga.create_fence`.
    pub fence_create: Phase,
    /// A fenced descriptor being recorded, to the handler retiring it. This is
    /// the guest's wait: it has submitted and can do nothing until this ends.
    pub fence_latency: Phase,
    /// Inside `complete_ctl` -- the used ring plus the interrupt injection.
    pub complete: Phase,

    // ── What the forwarded streams actually contain ─────────────────────────
    //
    // Every `SUBMIT_3D` under a native context carries one or more `amdgpu_ccmd`
    // records, and which ones they are is the difference between a frame's real
    // rendering and the guest's driver asking the host a question. `submits`
    // counts the envelopes; these count the letters.
    /// One bucket per `enum amdgpu_ccmd` opcode, by number.
    ///
    /// **By number and not by name.** Only opcode 2 (`GEM_NEW`) is pinned down
    /// in this tree, and inventing names for the rest would put a guess in the
    /// output where a measurement belongs. The reader looks up whichever
    /// buckets turn out to be large.
    pub ccmd: [AtomicU64; CCMD_BUCKETS],
    /// Records whose opcode is at or above [`CCMD_BUCKETS`].
    pub ccmd_high: AtomicU64,
    /// Records seen in total. Against `submits` this is records per submit --
    /// whether the guest packs several into one forward or sends them singly.
    pub ccmd_records: AtomicU64,
    /// Streams this could not walk. Non-zero means the counts below are a
    /// floor, not a total.
    pub ccmd_malformed: AtomicU64,

    /// One bucket per `AMDGPU_INFO_*` id, for `QUERY_INFO` records only.
    ///
    /// The opcode says the guest asked the host a question; this says which
    /// question. It matters because the answers differ in kind: a device's
    /// firmware version cannot change while a context lives, and its VRAM usage
    /// changes constantly. Only the first sort can be cached, so the split
    /// decides whether there is anything to do at all.
    pub info_query: InfoBuckets,
    /// Query ids at or above [`INFO_BUCKETS`].
    pub info_high: AtomicU64,

    // ── Which command kind costs what ───────────────────────────────────────
    //
    // `command` says what a command costs on average, and the average is a
    // blend: measured while driving, command submissions were 30 us each and
    // the mean was 94 us, so something in the other 12% was costing about
    // 590 us. An average over kinds cannot say which, and the kinds differ by
    // more than an order of magnitude.
    /// One phase per [`super::protocol::GPU_COMMAND_NAMES`] entry.
    pub command_kind: CommandKindPhases,

    // ── Inside the expensive ones ──────────────────────────────────────────
    /// `rutabaga.map` -- virglrenderer producing a host mapping.
    pub rutabaga_map: Phase,
    /// `rutabaga.unmap`.
    pub rutabaga_unmap: Phase,
    /// Publishing a mapping to the guest: `KVM_SET_USER_MEMORY_REGION`.
    ///
    /// A memslot update on a running VM, which is the one operation here that
    /// can make every vCPU wait. If a frame's worth of these lands together,
    /// that is a stall the guest sees as a hitch and nothing else records.
    pub kvm_map: Phase,
    /// Taking one away again, same call, same cost.
    pub kvm_unmap: Phase,

    /// `virgl_renderer_resource_map_fixed` -- placing a resource inside the
    /// window that is already a memory slot. The path that replaced the two
    /// above, and the one to compare them against.
    pub placed_map: Phase,
    /// Overwriting a placed resource's range with `PROT_NONE`.
    pub placed_withdraw: Phase,
    /// Resources virglrenderer would not place, which took the slot path.
    ///
    /// Non-zero is not a fault -- `-EOPNOTSUPP` is a documented answer for some
    /// resource types -- but it is the number that says how much of the old
    /// cost is still being paid.
    pub place_refused: AtomicU64,
}

/// Per-command-kind timing. A newtype for the same reason [`InfoBuckets`] is:
/// `Default` is not derivable for an array this long.
pub struct CommandKindPhases(pub [Phase; super::protocol::GPU_COMMAND_KINDS]);

impl Default for CommandKindPhases {
    fn default() -> Self {
        Self(std::array::from_fn(|_| Phase::default()))
    }
}

impl CommandKindPhases {
    pub fn record(&self, kind: usize, since: Instant) {
        if let Some(phase) = self.0.get(kind) {
            phase.since(since);
        }
    }
}

/// The same, read back.
#[derive(Clone, Copy, Debug)]
pub struct CommandKindCounts(pub [PhaseSnapshot; super::protocol::GPU_COMMAND_KINDS]);

impl Default for CommandKindCounts {
    fn default() -> Self {
        Self([PhaseSnapshot::default(); super::protocol::GPU_COMMAND_KINDS])
    }
}

/// `AMDGPU_INFO_*` ids counted individually. The largest in the uapi header is
/// below this; anything above lands in `info_high` and says the header moved.
pub const INFO_BUCKETS: usize = 64;

/// The per-query-id counters.
///
/// A newtype only because `Default` stops being derivable for arrays past 32
/// entries, and the enum this indexes is longer than that.
pub struct InfoBuckets(pub [AtomicU64; INFO_BUCKETS]);

impl Default for InfoBuckets {
    fn default() -> Self {
        Self(std::array::from_fn(|_| AtomicU64::new(0)))
    }
}

impl InfoBuckets {
    /// Count one query, or `None` if the id is past the last bucket.
    pub fn bump(&self, query: u32) -> bool {
        match self.0.get(query as usize) {
            Some(bucket) => {
                bucket.fetch_add(1, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }
}

/// The same, read back.
#[derive(Clone, Copy, Debug)]
pub struct InfoCounts(pub [u64; INFO_BUCKETS]);

impl Default for InfoCounts {
    fn default() -> Self {
        Self([0; INFO_BUCKETS])
    }
}

/// Opcodes counted individually. `enum amdgpu_ccmd` is a short enum; anything
/// beyond this lands in `ccmd_high` and says the enum has grown.
pub const CCMD_BUCKETS: usize = 16;

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
    pub spin: PhaseSnapshot,
    pub sleep: PhaseSnapshot,
    pub drain: PhaseSnapshot,
    pub drained: u64,
    pub command: PhaseSnapshot,
    pub submit: PhaseSnapshot,
    pub observe: PhaseSnapshot,
    pub fence_create: PhaseSnapshot,
    pub fence_latency: PhaseSnapshot,
    pub complete: PhaseSnapshot,
    pub ccmd: [u64; CCMD_BUCKETS],
    pub ccmd_high: u64,
    pub ccmd_records: u64,
    pub ccmd_malformed: u64,
    pub info_query: InfoCounts,
    pub info_high: u64,
    pub command_kind: CommandKindCounts,
    pub rutabaga_map: PhaseSnapshot,
    pub rutabaga_unmap: PhaseSnapshot,
    pub kvm_map: PhaseSnapshot,
    pub kvm_unmap: PhaseSnapshot,
    pub placed_map: PhaseSnapshot,
    pub placed_withdraw: PhaseSnapshot,
    pub place_refused: u64,
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
            spin: c.spin.read(),
            sleep: c.sleep.read(),
            drain: c.drain.read(),
            drained: load(&c.drained),
            command: c.command.read(),
            submit: c.submit.read(),
            observe: c.observe.read(),
            fence_create: c.fence_create.read(),
            fence_latency: c.fence_latency.read(),
            complete: c.complete.read(),
            ccmd: std::array::from_fn(|i| load(&c.ccmd[i])),
            ccmd_high: load(&c.ccmd_high),
            ccmd_records: load(&c.ccmd_records),
            ccmd_malformed: load(&c.ccmd_malformed),
            info_query: InfoCounts(std::array::from_fn(|i| load(&c.info_query.0[i]))),
            info_high: load(&c.info_high),
            command_kind: CommandKindCounts(std::array::from_fn(|i| c.command_kind.0[i].read())),
            rutabaga_map: c.rutabaga_map.read(),
            rutabaga_unmap: c.rutabaga_unmap.read(),
            kvm_map: c.kvm_map.read(),
            kvm_unmap: c.kvm_unmap.read(),
            placed_map: c.placed_map.read(),
            placed_withdraw: c.placed_withdraw.read(),
            place_refused: load(&c.place_refused),
            occupancy: self.occupancy.read(),
        }
    }
}

impl Default for GpuMetrics {
    fn default() -> Self {
        Self::new()
    }
}
