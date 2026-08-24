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
| 100 | 2.791 | 7.456 | 2.67× |
| 400 | 9.902 | 10.884 | **1.10×** |
| 1600 | 37.989 | 38.921 | **1.02×** |

≈ `frame_ms = 1.4 + 0.023 × cost` — a 13× span with ~1.4 ms of fixed per-frame
overhead. Monotonic and controllable, which is all a calibration probe needs.

Note the p99 column: **on bare metal, at a non-trivial frame cost, there is
essentially no tail.** Hold that for §4.2.

---

## 4. What the virtio path costs

### 4.1 Throughput: a fixed per-frame cost, not a percentage

`nesprobe`, 1920×1080, 25 s runs, p50 both sides so the comparison is matched:

| `--cost` | host p50 | guest p50 | delta | as % |
|---|---|---|---|---|
| 100 | 2.791 | 2.866 | +0.075 ms | +2.7% |
| 400 | 9.902 | 10.223 | +0.321 ms | +3.2% |
| 1600 | 37.989 | 38.089 | +0.100 ms | **+0.3%** |

**Native context costs a small fixed amount per frame — order 0.1–0.3 ms — not a
proportion of the work.** So it reads as ~3% on a cheap frame and disappears into
noise on an expensive one.

> An earlier draft claimed "≈3%" from whole-run mean fps across runs of *different
> durations*. That is wrong: clock ramp (§8.2) is a larger fraction of a short run,
> which flattered the guest. Matched p50 is the honest comparison, and it happens to
> tell a better story.

### 4.2 Latency: there is a ~2.7× frame-time tail, and it is not contention

**This is the most consequential measurement in this file.**

| `--cost` | host p99/p50 | guest p99/p50, **one guest** |
|---|---|---|
| 100 | 2.67× | 2.65× |
| 400 | **1.10×** | **2.69×** |
| 1600 | **1.02×** | **2.89×** |

At cost 1600 the host's p99 is 38.9 ms against a 38.0 ms p50. The same workload in a
guest, **alone on the card with nothing to contend against**, has a p99 of
**110.1 ms**.

And the ratio barely moves as guests are added (§5): 2.69× at one, 2.92× at two,
2.71× at three, 2.13× at four.

**So the tail is introduced by the virtio-gpu path and is present with a single
guest.** It is not co-tenancy, and no amount of scheduling will remove it — with one
guest there is nothing to schedule against.

This is the clearest optimisation target the GPU path has, and bare metal proves the
headroom is recoverable. Suspects, in rough order of suspicion:

- the submit→fence round-trip through the virtio-gpu worker thread;
- host scheduling of that worker against the vCPU threads;
- the per-resource mapping cost recorded in `PROGRESS.md` §7 at ~10.7 µs.

**Unattributed on purpose.** This is a measurement, not a diagnosis. If you want one
number to go after in this repo, it is this one.

---

## 5. Several guests on one GPU

`nesprobe` at 1920×1080, unpaced so every guest tries to consume the whole card. One
physical core per guest. Reproduce with `scripts/probe-sweep.sh`.

### 5.1 `--cost 400` (≈10.2 ms/frame solo)

| guests | p50 ms, each | p99 ms | Σ throughput (fps) | p50 vs solo |
|---|---|---|---|---|
| 1 | 10.22 | 27.5 | 97.8 | 1.00× |
| 2 | 18.85 / 18.75 | 54.9 | 106.4 | **1.84×** |
| 3 | 29.62 / 29.56 / 29.36 | 79.4–80.5 | 101.7 | **2.89×** |
| 4 | 37.64 / 37.64 / 37.53 / 37.40 | 54.9–80.5 | 106.5 | **3.67×** |

**Aggregate throughput is flat at 102–106 fps regardless of guest count**, and
*above* the 97.8 solo figure. The card saturates and is then divided; adding guests
costs nothing in total work done.

**A single guest cannot saturate the card.** Its synchronous
submit→fence→submit loop leaves the GPU idle during CPU turnaround, and a second
guest fills those gaps. Sharing is therefore very slightly *better* than free on
throughput.

**Sharing is even.** Per-guest p50 spread *within* a run is under 1% at every count
— 0.5% at two, 0.9% at three, 0.6% at four. The kernel's DRM scheduler divides
fairly without being asked to.

**Per-frame time grows sublinearly** — 1.84×, 2.89×, 3.67× for 2, 3, 4 guests — so
each guest gets a little more than an even share of the card's throughput.

### 5.2 Other costs

| cost | guests | p50 ms, each | p99 ms | Σ throughput (fps) |
|---|---|---|---|---|
| 100 | 1 | 2.866 | 7.60 | 309.0 |
| 100 | 4 | 9.506 / 9.501 / 9.499 / 9.483 | 26.3 | 412.9 |
| 1600 | 1 | 38.089 | 110.1 | 22.8 |

At cost 100, four guests each hold a frame under 10 ms with a **0.2% spread**.

---

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
2. **Discard a warm-up window, not a warm-up frame.** The GPU takes **~4 seconds**
   to reach steady clocks from idle: at cost 100 it climbed 144 → 374 fps *within
   one run*. Anything measured in the first ~5 s is fiction.
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

## 10. What these numbers do and do not support

**Do:**

- **DRM native context works on AMD**, not only Intel, and the virtio path costs a
  small fixed amount per frame rather than a percentage (§4.1).
- **One GPU serves several microVMs, and the kernel divides it evenly** without any
  arbitration from us — under 1% spread at every guest count (§5.1).
- **Aggregate throughput does not degrade** as guests are added; it is flat and
  slightly above the single-guest figure, because one guest cannot saturate a card.

**Do not:**

- **Do not carry any absolute figure to another GPU.** Vega iGPU, no dedicated VRAM,
  one synthetic workload.
- **Do not read any of this as an RDNA 4 result.**
- **Do not read p50 as the whole story.** The ~2.7× tail in §4.2 exists with a
  single guest and is what an interactive workload would actually feel.
- **Do not treat `nesprobe` as a stand-in for an application.** It says what the
  stack does to a frame, not what a real workload does.

---

## 11. Open, in the order that matters

1. **Where the ~2.7× virtualization tail comes from** (§4.2). Highest value in the
   repo: bare metal shows 1.02–1.10×, so the headroom is real and recoverable, and
   no scheduling change can substitute for finding it.
2. **RDNA 4.** Untested. Everything above is Vega.
3. **Frame counts from a real application.** `nesprobe` counts its own; an
   application cannot. A Vulkan layer that reports present timing would generalise.
4. **Where the ~1.4 ms fixed per-frame cost goes** — virtio round-trips, host
   submission, or the render pass itself. It bounds the cheapest possible frame.
5. **Block I/O under GPU load.** `PROGRESS.md` §6 records an 8.5 ms worst case over a
   400 MiB read — over half a 60 Hz frame budget — and it has never been measured
   *with* GPU work in flight.
