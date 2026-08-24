// Per-VM VRAM accounting for the DRM native-context path.
//
// A guest on this path carries no vendor GPU driver. It allocates device memory
// by sending an `AMDGPU_CCMD_GEM_NEW` command through virtio-gpu, which the
// renderer turns into an `amdgpu_bo_alloc` on the host. Nothing between the
// guest and the card bounds how much it may ask for, so one guest can exhaust a
// card that other guests are sharing.
//
// This module counts those allocations and refuses the ones that would take a
// guest past its limit.
//
// # This module measures. It does not enforce.
//
// That split was forced by the protocol, and the reasoning is worth keeping
// because the wrong answer looks obviously right.
//
// The guest reaches device memory in two steps:
//
//   1. `SUBMIT_3D` carrying `AMDGPU_CCMD_GEM_NEW` — the renderer calls
//      `amdgpu_bo_alloc`. **This is where host memory is committed.**
//   2. `RESOURCE_CREATE_BLOB` naming the same `blob_id` — the renderer looks the
//      already-allocated buffer up and wraps it in a virtio resource.
//
// So step 2 is too late to refuse: the memory is already taken, and refusing
// leaves it allocated with no resource id to free it by. Step 1 looks right, and
// refusing it does prevent the allocation.
//
// But **neither step can report a refusal to the guest.** Both are asynchronous.
// The guest kernel does not wait for a response to either one, so a refused
// submit produces `*ERROR* response 0x1200` in the guest's log and nothing else:
// the ioctl already returned success, Mesa proceeds with a buffer it believes
// exists, and the first submit referencing it waits forever on a fence.
// Measured — the guest hangs instead of failing.
//
// The renderer has the one channel that can carry the news: `shmem->async_error`,
// which Mesa reads through `amdvgpu_cs_query_reset_state2` and reports as a lost
// context. So enforcement lives in the renderer, in the `GEM_NEW` handler, where
// a genuine `amdgpu_bo_alloc` failure is already handled — a guest over its
// budget then fails exactly as a guest on a full card does, and no new failure
// path is introduced into a driver we do not own. See
// `patches/0002-virglrenderer-amdgpu-per-guest-VRAM-budget.patch`.
//
// What is left here is worth keeping on its own: the VMM is the only place that
// sees every guest, so this is where per-guest occupancy becomes a number
// capacity planning can use. It is also a cross-check — if these totals and the
// renderer's disagree, one of them is wrong.
//
// # What counts
//
// Only allocations that ask for `AMDGPU_GEM_DOMAIN_VRAM`. GTT buffers live in
// host system memory, which is bounded for the whole VMM process by cgroups, and
// double-counting them here would refuse guests for memory they are not taking
// from the card. GTT totals are tracked for observability and never enforced.
//
// # Trust
//
// Every field read here is guest-controlled. The parser bounds-checks each
// record and never panics. It reports a stream it cannot understand rather than
// guessing at it — but since nothing here gates the submit, a parse failure costs
// accuracy in the log, not safety.

use std::collections::HashMap;

/// `enum amdgpu_ccmd` — `AMDGPU_CCMD_GEM_NEW`.
const AMDGPU_CCMD_GEM_NEW: u32 = 2;

/// `struct vdrm_ccmd_req`: `cmd`, `len`, `seqno`, `rsp_off`, all `u32`.
const CCMD_HDR_LEN: usize = 16;

/// Records in a command stream are padded to 8 bytes.
const CCMD_ALIGN: usize = 8;

// Field offsets within `struct amdgpu_ccmd_gem_new_req`. The `r` substructure is
// `amdgpu_bo_alloc_request` padded so that every field is naturally aligned.
const GEM_NEW_OFF_BLOB_ID: usize = 16; // u64
const GEM_NEW_OFF_ALLOC_SIZE: usize = 24; // u64
const GEM_NEW_OFF_PREFERRED_HEAP: usize = 40; // u32
/// Shortest record we can account for: through `preferred_heap`.
const GEM_NEW_MIN_LEN: usize = 44;

const AMDGPU_GEM_DOMAIN_GTT: u32 = 0x2;
const AMDGPU_GEM_DOMAIN_VRAM: u32 = 0x4;

/// `RUTABAGA_CAPSET_DRM`. Only contexts created against this capset carry a ccmd
/// stream; a virgl context's submit payload is an unrelated command language and
/// must never be parsed as one.
const CAPSET_DRM: u32 = 6;

/// Low byte of `context_init` is the capset id.
const CONTEXT_INIT_CAPSET_ID_MASK: u32 = 0xff;

fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn rd_u64(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// One VRAM allocation the guest asked for and we allowed.
///
/// The owning context is part of the charge because the renderer frees every
/// buffer a context created when that context is destroyed
/// (`drm_context_deinit` walks both the blob and resource tables), so a charge
/// can never outlive its context however long the guest keeps the resource id.
#[derive(Clone, Copy, Debug)]
struct Charge {
    bytes: u64,
    ctx_id: u32,
}

/// Something worth saying about a submit. Kept apart from the counters so the
/// log can distinguish the two very different things.
#[derive(Debug)]
pub enum Notice {
    /// The guest asked for more VRAM than its limit leaves. The renderer refuses
    /// it; this is the VMM's record that it happened, and to which guest.
    OverLimit {
        requested: u64,
        charged: u64,
        limit: u64,
    },
    /// The command stream did not parse, so this submit is unaccounted for.
    Malformed(&'static str),
}

impl std::fmt::Display for Notice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Notice::OverLimit {
                requested,
                charged,
                limit,
            } => write!(
                f,
                "over VRAM limit (the renderer refuses it): requested {} MiB \
                 with {} of {} MiB charged",
                requested / (1 << 20),
                charged / (1 << 20),
                limit / (1 << 20)
            ),
            Notice::Malformed(why) => write!(f, "malformed ccmd stream: {why}"),
        }
    }
}

/// Counts a single guest's device-memory allocations against a limit.
///
/// Charges move through three states, because the guest learns the identity it
/// will later free a buffer by only after the buffer exists:
///
/// - **pending**, keyed by `(ctx_id, blob_id)` — charged at `GEM_NEW`, before
///   any resource id exists.
/// - **live**, keyed by `resource_id` — once `RESOURCE_CREATE_BLOB` names the
///   `blob_id`, which is the point the guest can free it.
/// - **released** — at `RESOURCE_UNREF`, or when the owning context goes away
///   and takes any still-pending charges with it.
pub struct VramAccountant {
    limit: u64,
    charged: u64,
    /// Allocated but not yet claimed by a blob create.
    pending: HashMap<(u32, u64), Charge>,
    /// Claimed, and freeable by the guest -- or by its context going away.
    live: HashMap<u32, Charge>,
    /// Contexts whose submits are ccmd streams.
    drm_contexts: HashMap<u32, ()>,
    /// Not enforced. Recorded so we can see whether a guest is escaping the
    /// limit by asking for GTT instead.
    gtt_charged: u64,
    peak: u64,
    refusals: u64,
    /// Highest watermark already reported, so the log records a guest's rising
    /// occupancy once per step rather than once per allocation.
    reported_peak: u64,
}

/// Granularity of the high-water-mark log, as a fraction of the limit so that a
/// small limit still reports, bounded either way to keep the log readable.
fn watermark_step(limit: u64) -> u64 {
    (limit / 8).clamp(4 << 20, 64 << 20)
}

impl VramAccountant {
    pub fn new(limit_bytes: u64) -> Self {
        Self {
            limit: limit_bytes,
            charged: 0,
            pending: HashMap::new(),
            live: HashMap::new(),
            drm_contexts: HashMap::new(),
            gtt_charged: 0,
            peak: 0,
            refusals: 0,
            reported_peak: 0,
        }
    }

    /// Note a context's capset, so we know whether its submits are ccmd streams.
    pub fn note_context(&mut self, ctx_id: u32, context_init: u32) {
        if context_init & CONTEXT_INIT_CAPSET_ID_MASK == CAPSET_DRM {
            self.drm_contexts.insert(ctx_id, ());
        }
    }

    /// Release everything a context held: charges never claimed by a blob create,
    /// and claimed ones the guest never unreffed.
    ///
    /// The renderer frees a context's buffers whether or not the guest tidied up
    /// after itself, so crediting anything less would ratchet a long-lived box
    /// toward refusing allocations for memory nobody holds.
    pub fn forget_context(&mut self, ctx_id: u32) {
        let mut freed = 0u64;
        self.pending.retain(|&(cid, _), charge| {
            if cid == ctx_id {
                freed += charge.bytes;
                false
            } else {
                true
            }
        });
        self.live.retain(|_, charge| {
            if charge.ctx_id == ctx_id {
                freed += charge.bytes;
                false
            } else {
                true
            }
        });
        if freed > 0 {
            self.charged = self.charged.saturating_sub(freed);
            log::debug!(
                "virtio-gpu: ctx {ctx_id} gone, releasing {} MiB of VRAM",
                freed / (1 << 20)
            );
        }
        if self.drm_contexts.remove(&ctx_id).is_some() {
            // The one point where a whole workload's footprint is known. Logged
            // unconditionally: the watermark above only fires once occupancy is
            // large, and "how little did this need" is the more useful answer.
            log::info!("virtio-gpu: ctx {ctx_id} teardown, {}", self.summary());
        }
    }

    /// Account for a command stream on its way to the renderer.
    ///
    /// The submit is forwarded either way — see the note at the top of this file
    /// on why a refusal here cannot reach the guest. An `Err` says what is worth
    /// logging, not what to do.
    pub fn observe_submit(&mut self, ctx_id: u32, commands: &[u8]) -> Result<(), Notice> {
        if !self.drm_contexts.contains_key(&ctx_id) {
            return Ok(());
        }

        // Charge only after the whole stream is known to be admissible, so a
        // refusal leaves no partial accounting behind.
        let mut proposed: Vec<((u32, u64), u64)> = Vec::new();
        let mut proposed_gtt = 0u64;
        let mut tentative = self.charged;

        let mut off = 0usize;
        while commands.len() - off >= CCMD_HDR_LEN {
            let rec = &commands[off..];
            let cmd = rd_u32(rec, 0).ok_or(Notice::Malformed("truncated cmd"))?;
            let len = rd_u32(rec, 4).ok_or(Notice::Malformed("truncated len"))? as usize;

            // The renderer applies exactly these checks and rejects the whole
            // stream if any fails. Mirror them so our view of the stream cannot
            // diverge from the view the renderer would take.
            if len < CCMD_HDR_LEN
                || len > commands.len() - off
                || len % CCMD_ALIGN != 0
            {
                return Err(Notice::Malformed("bad record length"));
            }

            if cmd == AMDGPU_CCMD_GEM_NEW {
                if len < GEM_NEW_MIN_LEN {
                    return Err(Notice::Malformed("GEM_NEW too short to account"));
                }
                let blob_id = rd_u64(rec, GEM_NEW_OFF_BLOB_ID)
                    .ok_or(Notice::Malformed("GEM_NEW blob_id"))?;
                let size = rd_u64(rec, GEM_NEW_OFF_ALLOC_SIZE)
                    .ok_or(Notice::Malformed("GEM_NEW alloc_size"))?;
                let heap = rd_u32(rec, GEM_NEW_OFF_PREFERRED_HEAP)
                    .ok_or(Notice::Malformed("GEM_NEW preferred_heap"))?;

                if heap & AMDGPU_GEM_DOMAIN_VRAM != 0 {
                    tentative = tentative.saturating_add(size);
                    if tentative > self.limit {
                        self.refusals += 1;
                        return Err(Notice::OverLimit {
                            requested: size,
                            charged: self.charged,
                            limit: self.limit,
                        });
                    }
                    proposed.push(((ctx_id, blob_id), size));
                } else if heap & AMDGPU_GEM_DOMAIN_GTT != 0 {
                    proposed_gtt = proposed_gtt.saturating_add(size);
                }
            }

            off += len;
        }

        if off != commands.len() {
            return Err(Notice::Malformed("trailing bytes"));
        }

        for (key, bytes) in proposed {
            // A repeated blob_id within a live context would be a renderer error
            // too; keep the larger charge rather than losing track of one.
            let entry = self.pending.entry(key).or_insert(Charge { bytes: 0, ctx_id });
            entry.bytes = entry.bytes.max(bytes);
            self.charged = self.charged.saturating_add(bytes);
        }
        self.gtt_charged = self.gtt_charged.saturating_add(proposed_gtt);
        self.peak = self.peak.max(self.charged);

        // Occupancy is the number capacity planning wants, and it is only
        // observable from here. Report it as it climbs, not per allocation.
        let step = watermark_step(self.limit);
        if self.peak >= self.reported_peak.saturating_add(step) {
            self.reported_peak = self.peak - (self.peak % step);
            log::info!("virtio-gpu: {}", self.summary());
        }

        Ok(())
    }

    /// A blob create named a `blob_id`, so the charge for it now has a resource
    /// id the guest can free it by.
    pub fn claim_blob(&mut self, ctx_id: u32, blob_id: u64, resource_id: u32) {
        if let Some(charge) = self.pending.remove(&(ctx_id, blob_id)) {
            self.live.insert(resource_id, charge);
        }
    }

    /// The guest freed a resource.
    pub fn release_resource(&mut self, resource_id: u32) {
        if let Some(charge) = self.live.remove(&resource_id) {
            self.charged = self.charged.saturating_sub(charge.bytes);
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "VRAM {}/{} MiB (peak {} MiB, {} refused), GTT {} MiB seen",
            self.charged / (1 << 20),
            self.limit / (1 << 20),
            self.peak / (1 << 20),
            self.refusals,
            self.gtt_charged / (1 << 20),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    /// Build one `struct amdgpu_ccmd_gem_new_req` exactly as Mesa lays it out.
    /// The size assertion below is the guard: if the real struct ever changes,
    /// this test data stops matching it and the offsets are wrong.
    fn gem_new(blob_id: u64, size: u64, heap: u32) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&AMDGPU_CCMD_GEM_NEW.to_le_bytes()); // cmd
        r.extend_from_slice(&56u32.to_le_bytes()); // len
        r.extend_from_slice(&1u32.to_le_bytes()); // seqno
        r.extend_from_slice(&0u32.to_le_bytes()); // rsp_off
        r.extend_from_slice(&blob_id.to_le_bytes());
        r.extend_from_slice(&size.to_le_bytes()); // r.alloc_size
        r.extend_from_slice(&4096u64.to_le_bytes()); // r.phys_alignment
        r.extend_from_slice(&heap.to_le_bytes()); // r.preferred_heap
        r.extend_from_slice(&0u32.to_le_bytes()); // r.__pad
        r.extend_from_slice(&0u64.to_le_bytes()); // r.flags
        assert_eq!(r.len(), 56, "gem_new_req is 56 bytes");
        r
    }

    fn other_ccmd(cmd: u32, len: usize) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&cmd.to_le_bytes());
        r.extend_from_slice(&(len as u32).to_le_bytes());
        r.extend_from_slice(&1u32.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes());
        r.resize(len, 0);
        r
    }

    fn drm_ctx(limit_mib: u64) -> VramAccountant {
        let mut a = VramAccountant::new(limit_mib * MIB);
        a.note_context(1, CAPSET_DRM);
        a
    }

    #[test]
    fn charges_vram_and_refuses_past_the_limit() {
        let mut a = drm_ctx(512);
        assert!(a.observe_submit(1, &gem_new(1, 256 * MIB, AMDGPU_GEM_DOMAIN_VRAM)).is_ok());
        assert_eq!(a.charged, 256 * MIB);

        assert!(a.observe_submit(1, &gem_new(2, 200 * MIB, AMDGPU_GEM_DOMAIN_VRAM)).is_ok());
        assert_eq!(a.charged, 456 * MIB);

        // 456 + 100 > 512
        let err = a.observe_submit(1, &gem_new(3, 100 * MIB, AMDGPU_GEM_DOMAIN_VRAM));
        assert!(matches!(err, Err(Notice::OverLimit { .. })));
        // A refusal charges nothing.
        assert_eq!(a.charged, 456 * MIB);
        assert_eq!(a.refusals, 1);
    }

    #[test]
    fn gtt_is_counted_but_never_enforced() {
        let mut a = drm_ctx(64);
        // Four times the VRAM limit, in GTT. Must be allowed.
        assert!(a.observe_submit(1, &gem_new(1, 256 * MIB, AMDGPU_GEM_DOMAIN_GTT)).is_ok());
        assert_eq!(a.charged, 0);
        assert!(a.summary().contains("GTT 256 MiB"));
    }

    #[test]
    fn a_refused_stream_charges_none_of_its_allocations() {
        let mut a = drm_ctx(512);
        // Two allocations in one stream: the first fits, together they do not.
        let mut stream = gem_new(1, 400 * MIB, AMDGPU_GEM_DOMAIN_VRAM);
        stream.extend_from_slice(&gem_new(2, 400 * MIB, AMDGPU_GEM_DOMAIN_VRAM));
        assert!(a.observe_submit(1, &stream).is_err());
        // Not 400 MiB. The submit is refused whole, so the renderer allocates
        // neither buffer and we must have charged for neither.
        assert_eq!(a.charged, 0);
    }

    #[test]
    fn release_requires_a_blob_claim_first() {
        let mut a = drm_ctx(512);
        a.observe_submit(1, &gem_new(7, 128 * MIB, AMDGPU_GEM_DOMAIN_VRAM)).unwrap();

        // Unreffing a resource that never claimed this charge frees nothing.
        a.release_resource(99);
        assert_eq!(a.charged, 128 * MIB);

        a.claim_blob(1, 7, 99);
        a.release_resource(99);
        assert_eq!(a.charged, 0);
        // The peak survives the release; it is what capacity planning wants.
        assert_eq!(a.peak, 128 * MIB);
    }

    #[test]
    fn a_dropped_context_releases_unclaimed_charges() {
        let mut a = drm_ctx(512);
        a.observe_submit(1, &gem_new(1, 300 * MIB, AMDGPU_GEM_DOMAIN_VRAM)).unwrap();
        // Guest never issued the blob create, then dropped the context. Without
        // this the limit would be held for the life of the VM.
        a.forget_context(1);
        assert_eq!(a.charged, 0);
    }

    #[test]
    fn a_dropped_context_releases_claimed_charges_too() {
        let mut a = drm_ctx(512);
        a.observe_submit(1, &gem_new(1, 300 * MIB, AMDGPU_GEM_DOMAIN_VRAM)).unwrap();
        a.claim_blob(1, 1, 50);
        // The renderer's drm_context_deinit frees every object the context made,
        // whether or not the guest unreffed the resource first, so the charge
        // must go with it. Holding it would ratchet the limit down over the life
        // of a box that starts and stops workloads.
        a.forget_context(1);
        assert_eq!(a.charged, 0);
        // And a late unref must not double-credit.
        a.release_resource(50);
        assert_eq!(a.charged, 0);
    }

    #[test]
    fn one_contexts_teardown_leaves_anothers_charges_alone() {
        let mut a = drm_ctx(1024);
        a.note_context(2, CAPSET_DRM);
        a.observe_submit(1, &gem_new(1, 100 * MIB, AMDGPU_GEM_DOMAIN_VRAM)).unwrap();
        a.claim_blob(1, 1, 10);
        a.observe_submit(2, &gem_new(1, 200 * MIB, AMDGPU_GEM_DOMAIN_VRAM)).unwrap();
        a.claim_blob(2, 1, 20);
        // Same blob_id in both contexts: blob ids are per-context, so these are
        // different buffers and must be accounted separately.
        assert_eq!(a.charged, 300 * MIB);
        a.forget_context(1);
        assert_eq!(a.charged, 200 * MIB);
    }

    #[test]
    fn non_drm_contexts_are_not_parsed() {
        let mut a = VramAccountant::new(64 * MIB);
        a.note_context(2, 1 /* virgl 2d */);
        // Bytes that would be a huge GEM_NEW if this were a ccmd stream. A virgl
        // context's payload is a different command language and must pass through.
        assert!(a.observe_submit(2, &gem_new(1, 4096 * MIB, AMDGPU_GEM_DOMAIN_VRAM)).is_ok());
        assert_eq!(a.charged, 0);
    }

    #[test]
    fn other_commands_in_the_stream_are_skipped() {
        let mut a = drm_ctx(512);
        let mut stream = other_ccmd(4 /* CS_SUBMIT */, 64);
        stream.extend_from_slice(&gem_new(1, 64 * MIB, AMDGPU_GEM_DOMAIN_VRAM));
        stream.extend_from_slice(&other_ccmd(10 /* CS_QUERY_FENCE_STATUS */, 32));
        assert!(a.observe_submit(1, &stream).is_ok());
        assert_eq!(a.charged, 64 * MIB);
    }

    #[test]
    fn malformed_streams_fail_closed() {
        let mut a = drm_ctx(512);

        // A buffer too short to hold even one record is trailing bytes, which
        // the renderer rejects too.
        assert!(matches!(
            a.observe_submit(1, &other_ccmd(2, 16)[..8]),
            Err(Notice::Malformed(_))
        ));

        // len shorter than a header
        let mut bad = 2u32.to_le_bytes().to_vec();
        bad.extend_from_slice(&4u32.to_le_bytes()); // len = 4, below the header
        bad.extend_from_slice(&[0u8; 8]);
        assert!(matches!(a.observe_submit(1, &bad), Err(Notice::Malformed(_))));

        // len past the end of the buffer
        let mut over = gem_new(1, MIB, AMDGPU_GEM_DOMAIN_VRAM);
        over[4..8].copy_from_slice(&4096u32.to_le_bytes());
        assert!(matches!(a.observe_submit(1, &over), Err(Notice::Malformed(_))));

        // len not 8-aligned
        let mut unaligned = gem_new(1, MIB, AMDGPU_GEM_DOMAIN_VRAM);
        unaligned[4..8].copy_from_slice(&52u32.to_le_bytes());
        assert!(matches!(a.observe_submit(1, &unaligned), Err(Notice::Malformed(_))));

        // trailing bytes that are not a whole record
        let mut trailing = gem_new(1, MIB, AMDGPU_GEM_DOMAIN_VRAM);
        trailing.extend_from_slice(&[0u8; 4]);
        assert!(matches!(a.observe_submit(1, &trailing), Err(Notice::Malformed(_))));

        // A GEM_NEW truncated below its accountable fields must be refused, not
        // waved through. The renderer would zero-fill and allocate; we cannot
        // account for what we cannot read.
        let mut short = gem_new(1, MIB, AMDGPU_GEM_DOMAIN_VRAM);
        short.truncate(40);
        short[4..8].copy_from_slice(&40u32.to_le_bytes());
        assert!(matches!(a.observe_submit(1, &short), Err(Notice::Malformed(_))));

        // Nothing above charged anything.
        assert_eq!(a.charged, 0);
    }

    #[test]
    fn a_size_that_overflows_u64_cannot_wrap_past_the_limit() {
        let mut a = drm_ctx(512);
        a.observe_submit(1, &gem_new(1, 256 * MIB, AMDGPU_GEM_DOMAIN_VRAM)).unwrap();
        let err = a.observe_submit(1, &gem_new(2, u64::MAX, AMDGPU_GEM_DOMAIN_VRAM));
        assert!(matches!(err, Err(Notice::OverLimit { .. })));
        assert_eq!(a.charged, 256 * MIB);
    }

    #[test]
    fn a_zero_limit_refuses_all_vram() {
        let mut a = drm_ctx(0);
        assert!(a.observe_submit(1, &gem_new(1, 4096, AMDGPU_GEM_DOMAIN_VRAM)).is_err());
    }
}
