# Benchmarks — what has actually been measured

**Every number here was produced by running something.** Nothing is inference.
Where a figure is derived rather than observed, it says so.

This file exists because the repo already lost a day to the opposite problem: a
design note and an unrun hypothesis sitting in adjacent paragraphs with no way to
tell which was which.

Companions: [`PROGRESS.md`](../PROGRESS.md) §5–6 for traps and known gaps,
[`tools/nesprobe/`](../tools/nesprobe/) for the probe.

---

## 1. The reference host

Every number below is from one machine. **Provenance is part of the measurement** —
a figure from a different host is a different figure, not a confirmation.

| | |
|---|---|
| CPU | AMD Ryzen 5 7530U, 6 cores / 12 threads. Siblings pair `(0,1)(2,3)…` |
| GPU | Barcelo iGPU, `1002:15e7`, **Vega / gfx90c** — *not* RDNA |
| RAM | 13.8 GiB |
| Host kernel | 7.1.8-1-cachyos |
| CPU governor | `powersave` (**not** `performance` — see §8.2) |
| GPU DPM states | `pp_dpm_sclk`: 200 / 400 / **2000** MHz, idling at 400 |
| libdrm | 2.4.134 |
| virglrenderer | fork at `7fcfce4` **+ the patch in §6** |
| Guest kernel | 7.2.0+ |
| Guest Mesa | 26.3.0-devel (`git-b78fc73dd8`), RADV, built `-Damdgpu-virtio=true` |

**This is an integrated GPU with no dedicated VRAM.** VRAM figures come off a UMA
carve-out and should not be read as capacity numbers. What generalises from this
host is *mechanism and shape*. What does not is any absolute byte or frame figure.

**Nothing here is an RDNA 4 result.**

---

## 2. Does GPU sharing work at all

| Question | Answer | Evidence |
|---|---|---|
| Guest sees the host GPU? | **Yes** | `vulkaninfo` in guest: `AMD Radeon Graphics (RADV RENOIR)`, `0x1002:0x15e7` — matching host `lspci` exactly — `DRIVER_ID_MESA_RADV` |
| Guest can render? | **Yes** | `vkcube` through a Wayland compositor; `nesprobe` offscreen |
| Guest can share a buffer with a compositor? | **Only with the §6 patch** | §6 |
| What does the virtio path cost? | **§4** | |
| Several guests at once? | **Yes, and they share evenly** | §5 |

This confirms DRM native context on a **second vendor**. The earlier Intel Arc A310
result alone could not separate "native context works" from "ANV works".

---

## 3. The probe, and why `vkcube` could not do this

`vkcube` cannot calibrate anything. Measured: solo GPU occupancy was **8.9% under
every reachable configuration** — 2 vCPUs and 4, a 1080p scanout and a 4K one,
`vkcube --width 3840 --height 2160` — with VRAM **byte-identical at 36.7 MiB**
throughout. It is a fixed, vsync-locked, overhead-dominated load: roughly 1.5 ms of
GPU time per frame that is the cost of pushing *any* frame through the path, not the
cost of its content.

A load you cannot dial cannot tell you whether a slowdown came from the stack or
from the workload. It produced one confidently wrong conclusion before being
abandoned (§7).

[`nesprobe`](../tools/nesprobe/) replaces it: headless, offscreen, fragment cost on
a push constant, frames counted in-guest.

### The cost knob — bare metal, 1920×1080, 25 s runs

| `--cost` | p50 ms | p99 ms | p99/p50 |
|---|---|---|---|
| 400 | 9.756 | 10.979 | 1.13× |
| 1600 | 37.895 | 39.161 | 1.03× |

≈ `frame_ms = 1.4 + 0.023 × cost` — a 13× span with ~1.4 ms of fixed per-frame
overhead. Monotonic and controllable, which is all a calibration probe needs.

All figures with `--warmup 8`, which is not optional; see §8.2.

---

## 4. What the virtio path costs

`nesprobe`, 1920×1080, 30 s runs, `--warmup 8`, p50 on both sides so the comparison
is matched:

| `--cost` | host p50 | guest p50 | delta | as % |
|---|---|---|---|---|
| 400 | 9.756 | 10.170 | +0.414 ms | **+4.2%** |

And the distribution, which is the part that matters for anything interactive:

| | p50 | p99 | **p99/p50** |
|---|---|---|---|
| host, cost 400 | 9.756 | 10.979 | 1.13× |
| **guest, cost 400** | 10.170 | **11.266** | **1.11×** |

**Native context costs about 4% on the median frame and nothing on the tail.** The
guest's frame-time distribution is as tight as bare metal's.

> ### An earlier version of this section was wrong, and the error is instructive
>
> It reported a **2.7× frame-time tail** in the guest against 1.10× on bare metal,
> and concluded that the virtio-gpu path introduced a large latency tail present
> even with a single guest. That conclusion was **entirely a measurement artifact**
> and is withdrawn.
>
> The cause was the GPU clock ramp (§8.2). Every guest run began after a ~40 s boot
> during which the GPU idled back down to 400 MHz, so the first seconds of each run
> rendered at a fraction of full clock. Those frames are numerous enough to *be* the
> p99 — a 25 s run at ~90 fps is ~2200 frames, of which ~120 fall in the ramp, or
> 5%. Meanwhile the bare-metal figures came from runs executed back-to-back in a
> loop, so that GPU was already hot. **The comparison was between a cold GPU and a
> warm one**, and it was measuring power management, not virtualization.
>
> Two process failures, worth naming: §8.2 already said "discard a warm-up window,
> not a warm-up frame" — and the probe discarded exactly one frame. And a
> surprising result was written up before anyone tried to make it go away.
>
> `nesprobe` now takes `--warmup` (default 5 s) and reports how many frames it
> dropped. Numbers taken without it should not be compared to numbers taken with
> it.

## 5. Several guests on one GPU

`nesprobe` at 1920×1080, unpaced so every guest tries to consume the whole card. One
physical core per guest. Reproduce with `scripts/probe-sweep.sh`.

### 5.1 `--cost 400` (≈10.1 ms/frame solo), `--warmup 8`

| guests | p50 ms, each | p99 ms | **p99/p50** | Σ throughput (fps) | p50 vs solo |
|---|---|---|---|---|---|
| 1 | 10.06 | 11.03 | 1.10× | 98.8 | 1.00× |
| 2 | 18.73 / 18.69 | 20.62 / 20.50 | 1.10× | 107.9 | **1.86×** |
| 4 | 37.84 / 37.59 / 37.49 / 37.35 | 39.41 / 39.35 / 39.26 / 39.34 | **1.04×** | 114.3 | **3.73×** |

Four results, and the third is the one that was expected to be bad:

**Aggregate throughput rises with guest count** — 98.8 → 107.9 → 114.3 fps. Not
"holds up": *rises*. A single guest cannot saturate the card, because its
synchronous submit→fence→submit loop leaves the GPU idle during CPU turnaround, and
another guest fills those gaps. Sharing this GPU is **better than free** on
throughput.

**Per-frame time grows sublinearly** — 1.86× and 3.73× for 2 and 4 guests — so each
guest gets slightly more than an even share.

**The frame-time distribution stays tight, and gets tighter.** p99/p50 is 1.10× at
one guest, 1.10× at two, **1.04× at four** — against 1.13× on bare metal. Adding
guests does not lengthen the tail relative to the median; if anything the steadier
load smooths it. There is **no latency penalty for co-tenancy** on this workload.

**Sharing is even without any arbitration from us.** Per-guest p50 spread within a
run is under 1.4% at four guests and under 0.3% at two. The kernel's DRM scheduler
divides fairly on its own.

### 5.2 Other costs

| cost | guests | p50 ms, each | p99 ms | Σ throughput (fps) |
|---|---|---|---|---|
| 100 | 4 | 9.506 / 9.501 / 9.499 / 9.483 | 26.3 † | 412.9 |
| 1600 | 1 | 37.895 | 39.161 | 26.4 |

† taken before `--warmup` existed, so that p99 is the clock ramp, not the workload.
Left in because the p50 and the 0.2% spread are still good measurements.

## 6. The bug that had to be fixed first

`vulkaninfo` passing was **necessary and not sufficient** — it creates contexts and
queries the device, and never shares a buffer. The first thing that does failed:

```
amdgpu_renderer_export_opaque_handle:303: failed to get dmabuf fd: Operation not permitted
```

`amdgpu_gem_prime_export` returns `EPERM` for any buffer carrying
`AMDGPU_GEM_CREATE_VM_ALWAYS_VALID`. That is the same root cause `PROGRESS.md` §5
records for `map_blob`, **at a second site that had not been fixed** — and it is the
path RADV's Wayland WSI takes to hand a frame to a compositor.

Why the host cannot avoid it unaided:

1. RADV marks shareable buffers with `AMDGPU_GEM_CREATE_VIRTIO_SHARED`, a
   **Mesa-private bit** (`sid.h`, `1u << 31`) that is not kernel uapi.
2. The guest converts it to `VIRTGPU_BLOB_FLAG_USE_SHAREABLE` on the *blob* and
   strips it from the ccmd (`amdgpu_virtio_bo.c:176`).
3. So `GEM_NEW` arrives at the host with clean flags **and before
   `RESOURCE_CREATE_BLOB`** — the allocation happens before shareability is known.
   `grep VIRTIO_SHARED` across virglrenderer returns nothing.
4. Clearing the capset's `has_vm_always_valid` is not an alternative:
   `radv_device.c:1533` makes it mandatory and RADV fails device creation without it.

**Fix:** strip the flag in `amdgpu_ccmd_gem_new`.
[`patches/0001-virglrenderer-amdgpu-strip-VM_ALWAYS_VALID.patch`](../patches/0001-virglrenderer-amdgpu-strip-VM_ALWAYS_VALID.patch),
against `7fcfce4`. **Four `EPERM` failures before, zero after.** It costs per-submit
validation work, since amdgpu must carry the buffer in the validation list rather
than assume residency — measurable with the same instrument.

Unconditional stripping is the blunt version. The targeted fixes are upstream
conversations: defer the allocation until blob flags are known, or stop RADV's WSI
path asking for local buffers on the virtio path.

---

## 7. A result that was wrong, kept on purpose

**"Two guests reach 80% of linear."** Measured with `vkcube`: 8.9% occupancy solo,
14.2% for two, a stable sum with anti-correlated halves. That is the signature of a
serialized submission path, and it looked like bad news about the whole approach.

**It was measurement error.** `vkcube` is vsync-locked and overhead-dominated, so
"80%" was a property of the load, not of the stack. With a load that actually
saturates, throughput is linear and slightly better (§5.1).

Kept because the reasoning was sound and the instrument was not, and that is the
failure worth recognising faster next time.

---

## 8. Measurement rules, each learned the hard way

1. **Occupancy and latency are different numbers.** `drm-engine-gfx` in the VMM's
   fdinfo is occupancy, per DRM client. A fence measures submit-to-signal *latency*,
   which with several guests includes queueing behind another one. Solo they agree —
   which is exactly how confusing them survives a single-guest experiment and fails
   on the second.
2. **Discard a warm-up *window*, not a warm-up frame — and this rule cost more
   than all the others combined when it was ignored.** An idle AMD GPU drops to a
   low DPM state: `pp_dpm_sclk` on this host idles at **400 MHz against a 2000 MHz
   top state**. Sampled during a run, it climbs **716 → 1100 → 2000 MHz over about
   2.5 seconds**, then pins at 2000 for the rest of the run.

   Frames rendered during that ramp are several times slower than steady state, and
   **there are enough of them to be the p99**: a 25 s run at ~90 fps is ~2200
   frames, of which ~120 fall in the ramp — 5%, well above the 1% mark. So a p99
   measured without a warm-up discard is a measurement of power management.

   This produced, and then unproduced, this file's biggest wrong conclusion (§4).
   `nesprobe --warmup` (default 5 s) exists because of it, and reports how many
   frames it dropped. **Never compare a figure taken with it to one taken without.**

   Corollary: **the same run repeated back-to-back is not the same measurement as
   one run after an idle gap.** Bare-metal figures were gathered in a loop with a
   hot GPU; guest figures each followed a 40 s boot with a cold one. That alone
   manufactured a fake 2.7× difference.
3. **A read-only rootfs silently disables the shader cache**, logging only
   `Failed to create /root/.cache for shader cache`. Cache-cold frame times are
   arbitrary.
4. **Trust p50, not whole-run mean fps.** The reported `fps` includes clock ramp and
   pipeline compilation. `p50_ms` is the frame cost.
5. **`gpu.width`/`gpu.height` set the scanout geometry, not the application
   surface.** Not a load knob; raising it measures noise.
6. **Don't `debugfs -w` into a guest image.** It does not maintain `metadata_csum`
   consistently — a file written that way came back as a broken symlink after the
   guest's own `fsck` "repaired" it, and `e2fsck -fn` reported a wrong inode
   refcount. Use virtiofs to get things in, which is what `probe-sweep.sh` does.
7. **Re-resolve the DRM fd every sample.** A guest context *is* a host DRM client:
   the fd appears when the guest first touches the GPU and vanishes when that
   context dies. A number captured once goes stale and the sampler reports zeros.

---

## 9. Repeatable workflows

Prerequisites: `cargo build --release`; an `artifacts/` directory holding `vmlinux`,
`rootfs.ext4` and `probe-share/nesprobe`; a patched virglrenderer built to a prefix.

```bash
# Build the probe. Its own workspace, so the VMM build does not pull in ash.
cd tools/nesprobe && cargo build --release && cd -
cp tools/nesprobe/target/release/nesprobe artifacts/probe-share/

# Concurrency sweep -- N guests, fixed per-frame cost. The main instrument.
scripts/probe-sweep.sh -n 4 -c 400 -s 30

# One guest, interactively, to poke at something by hand
LD_LIBRARY_PATH=artifacts/virgl-patched/lib \
  RUST_LOG=info ./target/release/nesbox examples/gpu-probe.json   # fix the paths first
#   in guest: mount -t proc proc /proc; mount -t sysfs sysfs /sys
#             mount -t tmpfs tmpfs /tmp; mount -t virtiofs probe /mnt
#             /mnt/nesprobe --cost 400 --seconds 30

# Host-side occupancy, alongside either of the above
scripts/gpu-sample.sh                 # auto-detects a single nesbox

# A/B two renderers -- the reason to build to a prefix rather than over /usr/lib
LD_LIBRARY_PATH=artifacts/virgl-patched/lib   ...
LD_LIBRARY_PATH=artifacts/virgl-unpatched/lib ...
```

`probe-sweep.sh` boots every guest from **one shared read-only rootfs**, with the
probe arriving over virtiofs. Nothing in the guest writes, so a sweep costs no disk
and cannot corrupt an image.

### Bypassing the guest userspace

The reference guest image runs an agent that expects a control channel. With no
vsock and no network it reaches a login prompt and then **powers itself off a few
seconds later** — reproducibly, with stdin held open and nothing typed. It presents
as a crash on keypress and is not one.

For GPU work that needs none of that, boot `init=/bin/bash` and mount by hand.
Expect the documented `Attempted to kill init!` panic when the shell exits;
`exitcode=0x0` means the last command succeeded.

---

### Does a VRAM limit bind, and does it stay contained?

```sh
# The probe's whole device-memory footprint is one 8 MiB render target, so a
# limit either side of that is a decisive test.
#   "vram-limit-mib": 16   -> runs, 8/16 MiB held
#   "vram-limit-mib": 4    -> refused at GEM_NEW, guest wedges
# Occupancy and refusals appear in the VMM log:
grep -E "VRAM|budget exceeded" run.log
```

Then the test that matters: run an over-budget guest beside a workable one and
compare the workable one against its **solo** baseline, not against itself. A
containment failure shows up as the neighbour slowing down, which is invisible
unless there is a solo number to compare with.

---

## 10. What these numbers do and do not support

**Do:**

- **DRM native context works on AMD**, not only Intel, and costs ~4% on the median
  frame and **nothing on the tail** (§4).
- **One GPU serves several microVMs, and the kernel divides it evenly** without any
  arbitration from us — under 1% spread at every guest count (§5.1).
- **Aggregate throughput rises** as guests are added — 98.8 → 107.9 → 114.3 fps for
  1, 2 and 4 — because one guest cannot saturate a card.
- **Co-tenancy costs nothing on the tail.** p99/p50 is 1.10× at one guest and 1.04×
  at four, against 1.13× bare metal.

**Do not:**

- **Do not carry any absolute figure to another GPU.** Vega iGPU, no dedicated VRAM,
  one synthetic workload.
- **Do not read any of this as an RDNA 4 result.**
- **Do not compare figures across warm-up conventions.** Anything measured before
  `--warmup` existed has a p99 that is really the GPU clock ramp (§8.2).
- **Do not treat `nesprobe` as a stand-in for an application.** It says what the
  stack does to a frame, not what a real workload does.

---

## 11. Per-guest VRAM: what a limit can and cannot do

A guest on this path carries no vendor GPU driver. It asks for device memory with
`AMDGPU_CCMD_GEM_NEW`, the renderer calls `amdgpu_bo_alloc` for it, and nothing in
between bounds how much it may ask for. One guest can exhaust a card that its
neighbours are sharing. `vram-limit-mib` in the GPU config bounds it.

### 11.1 The enforcement point is not where it looks

The obvious place is `RESOURCE_CREATE_BLOB`, which carries a plain `size`. It is
the wrong place, and so is the next candidate. The guest reaches device memory in
two steps:

1. `SUBMIT_3D` carrying `AMDGPU_CCMD_GEM_NEW` — **this is where host memory is
   committed.**
2. `RESOURCE_CREATE_BLOB` naming the same `blob_id` — the renderer looks the
   already-allocated buffer up and wraps it.

So refusing step 2 leaves the memory allocated with no resource id to free it by.
Refusing step 1 does prevent the allocation — but **neither step can report a
refusal to the guest**, because both are asynchronous. Measured: the guest kernel
logs `*ERROR* response 0x1200 (command 0x10c)` and returns success from the ioctl
anyway, so Mesa's `alloc_host_blob()` never sees the zero handle it checks for,
proceeds with an unbacked buffer, and the first submit referencing it waits on a
fence that never signals. **The guest hangs instead of failing.**

`shmem->async_error` is the only channel that can carry the news, and Mesa reads
it in exactly one place (`amdvgpu_cs_query_reset_state2`), so it surfaces only if
something asks.

### 11.2 So the report does the work, and the refusal is a backstop

Two parts, in `patches/0002`:

- **Report the budget, not the card.** All three paths that tell a guest how much
  VRAM exists — the shmem heap block, `AMDGPU_INFO_MEMORY`, `AMDGPU_INFO_VRAM_GTT`
  — report the limit, and report the guest's own usage rather than the card's. A
  guest that reads the memory budget then sizes itself to what it was given,
  through the ordinary Vulkan path, with no error at all. This is the part that
  does useful work, and it only works for guests that ask. Most engines do;
  `nesprobe` does not, which is why the measurement below hits the backstop.
- **Refuse in `GEM_NEW`.** Contains a guest that ignores the report, at the cost of
  that guest wedging.

Only the VRAM heap is bounded. GTT is host system memory, capped for the whole VMM
process by cgroups; counting it twice would refuse guests for memory they never
took from the card.

### 11.3 Measured

Reference host, `nesprobe` at 1080p, whose entire device-memory footprint is **one
8 MiB render target** — so the limit can be placed either side of a known number.

| `vram-limit-mib` | outcome |
|---|---|
| 4096 | runs; occupancy reported as 8 MiB, released to 0 at teardown |
| 16 | runs; 8/16 MiB held, peak 8 MiB |
| 4 | refused at `GEM_NEW`, 8 MiB against a 4 MiB budget; guest wedges |

Both layers agree independently on the refusal — the VMM's accounting and the
renderer's budget each computed 8 MiB against 4 MiB — which is the point of
keeping the VMM-side counting after moving enforcement out of it.

**The result that matters is the neighbour.** One guest given a 4 MiB budget it
cannot satisfy, alongside one given a workable 512 MiB running the probe at
`cost=400`:

| | frames | fps | p50 | p99 |
|---|---|---|---|---|
| over-budget guest | 0 | — | — | — |
| its neighbour | 1688 | 99.26 | 10.022 | 10.976 |
| **solo baseline** | — | 98.8 | 10.06 | 11.03 |

The neighbour performed as though it were alone on the card. **A guest that
exhausts its VRAM limit is fully contained**, which is the property "many
sandboxes, one GPU" actually needs.

### 11.4 What this does not achieve

**A clean, guest-visible out-of-memory error is not available on this path.** The
best outcomes are: a cooperative guest sizes itself down and never fails, or an
uncooperative one wedges itself without touching its neighbours. There is no third
option today without a guest-side change — Mesa checking `async_error` after
`vdrm_bo_create`, or the blob create becoming synchronous — and the guest is
deliberately not a place we hold boundaries.

Two smaller findings worth keeping:

- **`deny_unknown_fields` on the GPU config.** `vram-limit-mib` is kebab-case like
  its siblings, and serde silently ignored the underscored spelling — leaving the
  guest unbounded while the config looked correct. A safety limit must not fail
  open on a typo.
- **The `-Wmaybe-uninitialized` warning was a real bug.** Two validation paths at
  the top of `GEM_NEW` `goto` past the budget bookkeeping, so the credit-back read
  uninitialised memory. Declared before the first jump.

---

## 12. Open, in the order that matters

1. **RDNA 4.** Untested. Everything above is Vega.
2. **Frame counts from a real application.** `nesprobe` counts its own; an
   application cannot. A Vulkan layer that reports present timing would generalise.
3. **Where the ~1.4 ms fixed per-frame cost goes** — virtio round-trips, host
   submission, or the render pass itself. It bounds the cheapest possible frame.
4. **Block I/O under GPU load.** `PROGRESS.md` §6 records an 8.5 ms worst case over a
   400 MiB read — over half a 60 Hz frame budget — and it has never been measured
   *with* GPU work in flight.
5. **Whether a real engine honours the clamped heap.** §11.2 rests on guests reading
   `VkPhysicalDeviceMemoryBudgetPropertiesEXT` and sizing themselves accordingly.
   `nesprobe` does not, so nothing here demonstrates it. Until something that does
   is measured, the clamp is reasoning and only the containment in §11.3 is
   evidence.
6. **What a VRAM limit costs when it is not exceeded.** §11.3 shows the limit binds
   and contains, not what the accounting costs a guest that stays inside it. The
   per-submit ccmd parse is on the hot path.
