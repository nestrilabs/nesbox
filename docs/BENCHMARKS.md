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
| Form factor | **Laptop, running on battery, discharging** — see the warning below |
| Power profile | ACPI `platform_profile` = `balanced`; EPP = `balance_power` |
| CPU governor | `powersave` (**not** `performance` — see §8.2) |
| CPU clocks | `scaling_max_freq` **4.55 GHz**, observed under sustained load **~2.15 GHz** |
| `amd_pstate` | `active` |
| GPU DPM states | `pp_dpm_sclk`: 200 / 400 / **2000** MHz, idling at 400 |

> ### Read every number here as a floor, not a measurement of the software
>
> This is a **battery-powered laptop on a balanced power profile.** The CPU is
> rated at 4.55 GHz and sustained 2.15 GHz — **under half its clock** — and the
> iGPU's 2000 MHz ceiling is a mobile part's. Nothing here was tuned for
> throughput, deliberately: it is the machine the work happened on.
>
> What that does and does not affect:
>
> - **Ratios are the durable results.** ~96% of bare-metal median frame time,
>   97.9% of a capped host's CPU, 1.04–1.10× p99/p50 across four boxes — these
>   compare a guest to the *same host*, so power state cancels out. They are the
>   numbers to quote.
> - **Absolute figures are not the hardware's.** Frame times, MB/s and fps would
>   all improve on mains power, a performance profile, or a desktop part. Nobody
>   should read 98.8 fps at `cost=400` as a property of the card.
> - **Power management is an active hazard**, not background noise, and it has
>   produced two false findings already: the GPU clock ramp in §4 and the CPU
>   quota nonlinearity in §12.1. On a battery-powered laptop, *any* result where
>   load is intermittent should be suspected of measuring a P-state before it is
>   believed.
>
> **The RDNA 4 host is the validation, not a repeat.** It is a better-configured
> machine, and re-running this suite there is the point of `scripts/` being one
> command each — see §15 item 1.
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

## 9. Run all of it: `scripts/bench.sh`

```sh
scripts/bench.sh                 # everything, ~15 minutes, writes benchmarks/<host>.json
scripts/bench.sh --only gpu
scripts/bench.sh --list
```

No arguments required, and **committing the output is the point**: a committed
result turns "it feels slower" into a diff, and makes re-measuring on new hardware
an afternoon rather than a week.

### What it is, and what it is not

**It runs no measurement of its own.** Every number comes from a harness that
already existed and can still be run by hand — `nesprobe`, `probe-sweep.sh`,
`envelope.sh`. `bench.sh` runs them, attaches provenance, and writes JSON. If a new
probe is needed, it belongs in its own harness first, where it can be run and
argued with on its own.

Four sections: `gpu` (one guest), `scaling` (1, 2 and 4 guests), `seccomp`
(confined against unconfined), `envelope` (does a cgroup bound the guest).

### Provenance is not optional

`scripts/bench-provenance.sh` runs alone too. It records CPU model and governor,
**energy-performance preference**, rated against observed clocks, `amd_pstate`
mode, the GPU's PCI id and **DPM states**, **whether the machine is on mains or
battery**, swap devices, the filesystem and device behind the disk image, and the
commit of both nesbox and the renderer.

Half of those fields exist because they invalidated a finding: the governor and
the DPM states in §4, and the battery in §12.1. Every field degrades to `null`
rather than failing, because this has to run on a machine nobody has seen yet.

### What it says it did not measure

Four gaps are named in the output rather than quietly omitted, because a results
file people quote should say what is missing:

| | Why |
|---|---|
| Network | No `iperf3` in the guest image. Adding one is *image* work, not benchmark work |
| Random I/O | No `fio` in the guest image; only sequential `dd` through `envelope.sh` |
| Boot time | Not implemented. It would be a new measurement rather than an orchestration |
| Other hypervisors | None run — and this is exactly what a comparative claim would need |

That last row is the one that matters commercially. **Nothing in this repository
supports a claim of being faster than another hypervisor**, because the same
workload has never been run under one.

### The scaling section needs a long run, and this is why

`probe-sweep.sh` starts its guests a second apart, and each boots for ~13 s before
the probe starts. So a short run measures **different degrees of concurrency in
each guest** and reports them side by side. Measured at `--seconds 6`, four guests
gave per-guest p50s of 38.9, 37.5, 28.2 and 18.4 ms — the last guest spent most of
its run alone on the card, and its number is a solo number wearing a co-tenancy
label.

At the default 20 s the guests overlap for most of the run and the spread closes.
**If the per-guest figures in a `scaling` result differ by more than a few percent,
suspect the run length before believing the card is unfair.**

### Reading a result

`completed` is the field to check first, and **every section has one**. A run that
dies early still produces numbers of the right shape, and they are the numbers of a
partial run: a sweep that lost every guest still reports `sum fps 0`, which reads
exactly like a measurement. `scaling` carries one per guest count as well as an
overall.

**A failed section will not overwrite a completed one.** Re-running `--only
scaling` after a failure leaves the previous good numbers in place and adds
`kept_earlier_result_for` saying so — because losing a fifteen-minute measurement
to a run that crashed, and being left with something that still looks like a
result, is the worse of the two outcomes. And a run whose output will not parse
leaves the committed file untouched rather than replacing it.

`ratios`, not absolutes. §12.1's warning generalises: this reference host is a
laptop on battery sustaining under half its rated clock, so a guest-to-host ratio
is a property of the software and a millisecond figure is a property of the
machine.

### The individual workflows

`bench.sh` orchestrates these. Each is still worth running by hand when chasing a
specific question, which is why they remain separate.

Prerequisites: `cargo build --release`; an `artifacts/` directory holding `vmlinux`,
`rootfs.ext4` and `probe-share/nesprobe`; a patched virglrenderer built to a prefix.

```bash
# Build the probe. ~~Its own workspace, so the VMM build does not pull in ash.~~
# (dathorse): EVEN IF IT'S IN SAME WORKSPACE; IT WON'T MEAN IT GETS COMPILED IN WITH ASH
#   separate workspaces in same project is messy and creates bad separation.
cargo build --release --bin nesprobe
cp target/release/nesprobe artifacts/probe-share/

# Concurrency sweep -- N guests, fixed per-frame cost. The main instrument.
scripts/probe-sweep.sh -n 4 -c 400 -s 30

# One guest, interactively, to poke at something by hand
LD_LIBRARY_PATH=artifacts/virgl-patched/lib \
  RUST_LOG=info ./nesbox examples/gpu-probe.json   # fix the paths first
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

Only the VRAM heap is bounded. GTT is host system memory; counting it twice would
refuse guests for memory they never took from the card. Whether that host memory
is capped at all is the supervisor's `memory.max` to set and not something nesbox
applies — see §12.3 for what such a cap does and does not do, and note that nesbox
now says at startup which limits are actually in force.

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

## 12. Does a cgroup on the VMM bound the guest inside it?

A guest gets CPU, memory and disk through the VMM's own threads, so the kernel's
cgroup controllers ought to bound a guest without nesbox implementing anything.
`scripts/envelope.sh` tests that. It needs **no root**: systemd delegates `cpu`,
`io`, `memory` and `cpuset` to the user session, so `systemd-run --user --scope`
applies a limit the same way a supervisor would.

```sh
scripts/envelope.sh          # all three
scripts/envelope.sh io       # one
```

The short answer is **one of three holds cleanly, one leaks, and one does
something other than what it looks like.**

### 12.1 CPU — holds, and virtualization costs ~2%

`openssl speed sha256` in the guest, and the identical command on the host as a
control.

| | guest | host | guest/host |
|---|---|---|---|
| unlimited | 2,157,396k | 2,186,751k | **98.7%** |
| `CPUQuota=50%` | 504,652k | 515,643k | **97.9%** |

**A quota applies to a guest almost exactly as it applies to a native process**,
and CPU virtualization costs **1.3–2.1%**.

**The trap is the other ratio.** A 50% quota does not give 50% of throughput — it
gives **23.4%** in the guest. That looks like a catastrophic virtualization
penalty and is not one: the host shows **23.6%** with no VM involved at all. The
nonlinearity belongs to the host, not to us. CPU frequency during a capped run
swings 1.4–2.4 GHz against a steady 2.15 GHz uncapped, so power management is
involved, though that does not fully account for it and no claim is made here that
it does.

Shortening the enforcement window makes it worse, not better — `50%` at a 100 ms
period gives 484,422k, at 10 ms 456,466k, at 5 ms 442,565k — so it is not an
artefact of long freezes. Two vCPUs and one vCPU behave identically, so it is not
vCPU contention either.

> **Compare a capped guest against a capped host, never against an uncapped one.**
> Doing otherwise here would have reported a 4× virtualization penalty. That is the
> same error as §4: the wrong baseline, not the wrong system.

### 12.2 Block I/O — bounds device traffic, and is void on a warm cache

`dd if=/dev/vda iflag=direct`, 300 MiB, against `IOReadBandwidthMax=20M`.

| | throughput |
|---|---|
| unlimited, host cache cold | 997 MB/s |
| **20 MB/s cap, cold** | **53.9 MB/s** |
| **20 MB/s cap, host cache warm** | **1.9 GB/s** |

Two findings, and the second is the one that matters.

**The cap works, imprecisely** — an 18× reduction, but 2.7× above the number
asked for. Host readahead turns the guest's 1 MiB direct reads into larger device
reads, and the cap is on device bandwidth rather than on what the guest sees.

**The cap does not work at all once the host has the image cached.** `iflag=direct`
stops the *guest* caching; nothing stops the *host* caching the backing file. A
read the host page cache satisfies never reaches the device, so `io.max` never sees
it — the capped guest ran **35× faster than the uncapped cold one**.

So an I/O bound on a guest holds only while its working set misses host cache. This
is the same missing `O_DIRECT` that makes every guest byte cost host memory twice
(§6): one flag, two problems.

> **Everything above is the measurement of the problem. The fix is measured
> below, and it closes it.**

#### 12.2.1 With `O_DIRECT`, the same cap holds — and holds exactly

Measured 2026-08-28 on `nestripc-1` (Ryzen 9 5950X; image on xfs, Samsung 850
EVO, `/dev/sda`). Same cap, same 300 MiB `iflag=direct` guest read, and the host
page cache **deliberately warmed over that exact region before every run** —
warm is the case that failed above.

| drive | uncapped | `IOReadBandwidthMax=/dev/sda 20M` |
|---|---|---|
| `"direct": false` | 11.3 GB/s | **13.3 GB/s — the cap does nothing** |
| `"direct": true` | 508 MB/s | **20.0 MB/s — the cap holds** |

Two things to take from this, and the second is the surprise.

**The bound is real now.** Buffered, a warm host cache lets the guest read 665x
its cap, because reads the page cache satisfies never reach the device `io.max`
is attached to. Direct, every read reaches the disk, so every read is counted.
(That the capped buffered figure is *higher* than the uncapped one is noise —
both are RAM, neither touched `/dev/sda`.)

**It is also accurate, which §12.2 was not.** The original cold-cache run
overshot its 20 MB/s cap by 2.7x, because host readahead turned the guest's
1 MiB reads into larger device reads. With `O_DIRECT` there is no readahead to
inflate them: the measurement lands on 20.0 MB/s. So direct I/O does not merely
make the cap apply, it makes the number in the config mean what it says.

**What this costs is in §14.1**: about 4% of sequential throughput, and small
random reads served at 81 MB/s instead of 185 — the latter being the page cache
doing the work, which is precisely what a bound is supposed to stop.

> Enabling this needed the `io` controller delegated to the user session, which
> is not the default: `Delegate=pids memory cpu io` in a drop-in for
> `user@.service`, then a full re-login. Without it `cgroup.controllers` for the
> session reads `cpu memory pids` and an unprivileged `io.max` silently has
> nothing to attach to.

> Any storage benchmark that does not state its cache state is measuring the cache.
> `envelope.sh` evicts the image with `POSIX_FADV_DONTNEED` between runs, which
> needs no root.

### 12.3 Memory — not a dial, and what it does depends on the host

`dd` into guest tmpfs, which is guest RAM, so the VMM really faults the pages in.
Guest RAM 1024 MiB.

| | throughput |
|---|---|
| `MemoryMax=2G` (above guest RAM) | 459 MB/s |
| `MemoryMax=384M` (below guest RAM) | **240 MB/s** |

The guest was **not killed and did not shrink**. It got 48% slower, with no error
reported anywhere — because this host has 13.5 GiB of zram swap, so guest RAM was
reclaimed into it and the guest stuttered on faults it cannot see or account for.

**On a host without swap the same cap OOM-kills the VMM instead.** Same
configuration, entirely different failure, decided by something outside the
config.

Either way: **`memory.max` cannot make a guest use less memory**, only punish it
for using what it was given. It is a blast radius, not a dial. Guest RAM has to be
right at boot.

### 12.4 What this means for bounding a box

| bound | verdict |
|---|---|
| CPU | **Holds.** Costs ~2% over native, and quota-to-throughput is nonlinear on the host too |
| Block I/O | **Holds, with `"direct": true`** — and to the number configured (§12.2.1). Buffered, it is void against a warm cache |
| Host RAM | **Not a bound on the guest.** Sets what happens when a guest exceeds its allocation, not what it may allocate |
| VRAM | Holds — but by quota and heap report, not by cgroup (§11) |

---

## 13. The host-visible window

A blob the guest wants to touch with the CPU is mapped into BAR2 and registered
with KVM, so afterwards the guest reads and writes it without trapping to us. That
is what makes it fast, and why it needs a bound: every mapping costs host address
space and a **KVM memory slot**, and nothing in the protocol makes a guest ask for
a sensible number of them.

```json
"host-visible-window-mib": 256,
"host-visible-max-mappings": 512
```

Both omitted means unbounded. Bytes is the quota a tier would be sized against;
the mapping count is separate because slots are a different resource — KVM has a
few thousand, and a guest mapping single pages could exhaust them. It defaults to
unbounded because **no measurement yet says what a real workload needs**, and a cap
guessed too low breaks it.

### 13.1 What a workload actually uses

`nesprobe` at 1080p, unbounded: **616 KiB across 9 mappings**, peak 616 KiB.

So for this workload the window is nearly free, and a quota is about bounding the
pathological case rather than sizing the normal one. A real title with many
CPU-visible buffers will be much larger, and that number does not exist yet.

### 13.2 This is the one GPU bound that can be refused synchronously

Unlike a VRAM allocation (§11), `RESOURCE_MAP_BLOB` **is synchronous** — the guest
kernel waits for `RESP_OK_MAP_INFO` because userspace needs the offset before it
can mmap anything. So a refusal is delivered immediately rather than vanishing into
an async queue.

Measured with `host-visible-max-mappings: 4`, below the 9 the probe needs:

```
map_blob: refusing resource 9: host-visible window has 4 of 4 mappings in use
EXIT=139
```

**The refusal arrives, and the guest process dies of SIGSEGV rather than getting a
clean error.** Two things worth separating there:

- **The protocol did its job.** Mesa's `amdvgpu_bo_cpu_map` returns failure
  correctly (`return *cpu == NULL`), so the error *is* propagated at that layer —
  its `assert(cpu_addr != NULL)` is compiled out under `NDEBUG`, but the return
  value is honest. The fault is above the vdrm layer, in code that does not check
  it.
- **It is contained.** The guest shell printed `EXIT=139`, so **the box survived
  and only the offending process died.** Compare §11, where a VRAM refusal wedged
  the whole guest. A dead process is a much better failure than a hung box.

So the ranking of the three GPU bounds by how gracefully they refuse:

| bound | refusal reaches guest? | what dies |
|---|---|---|
| Host-visible window | **Yes, synchronously** | The process |
| VRAM quota | No — async, unreportable | The box hangs (§11.1) |
| VRAM via heap report | Not a refusal — the guest sizes itself down | Nothing |

The pattern is consistent and worth stating plainly: **tell a guest the truth up
front and nothing has to fail; refuse it mid-flight and the quality of the failure
depends on a protocol detail we do not control.**

---

## 14. The block device, before and after `io_uring`

Measured 2026-08-28 with [`scripts/bench-blk.sh`](../scripts/bench-blk.sh), A/B
against a build of `bdc4192` -- the last commit with the old serial device --
kept in a worktree so both binaries exist at once.

**A third host, and neither of the two above.** §1's reference host is a
battery-powered laptop and every GPU number in this file comes from it; this
section was measured on the **HP Z2 Tower G4 workstation**: Intel Xeon E-2276G,
6 cores / 12 threads at 3.8 GHz, 62 GiB, kernel 7.2.0-1-cachyos. Mains power, a
desktop part, so §1's warnings about battery and platform profile do not apply
here — but its warning about attributing a number to the wrong machine very much
does. Guest: 4 vCPUs, 2 GiB, Linux 7.2.

**Cache state, stated because §12.2 is about what happens when it is not.** The
image is a 5 GiB ext4 file on btrfs (`compress=zstd`), and every run is **warm**:
reads are served from the host page cache. That is deliberate. A cold run
measures the SSD, which neither binary changes; a warm run measures the device
model, which is the whole of what changed. Absolute figures here are therefore
not disk throughput and should never be quoted as such -- **the ratios are the
result.**

Five repetitions, alternating between the binaries so that clock drift lands on
both, median reported.

| case | new | old | ratio |
|---|---|---|---|
| `seq1m` — 300 MiB in 1 MiB direct reads, one at a time | **8738 MB/s** | 5617 MB/s | **1.56x** |
| `par8` — eight 32 MiB direct readers at once | **16777 MB/s** | 5368 MB/s | **3.13x** |
| `rand4k` — 8000 4 KiB direct reads, one at a time | 211 MB/s | 217 MB/s | 0.97x |

Spreads, because a difference inside them is not a difference: `seq1m` 7864–9532
against 4993–6553, `par8` 15790–17895 against 5162–5592 — neither overlaps.
`rand4k` is 210–224 against 211–218, which is the same number twice. A second
independent five-run set gave 1.56x, 3.19x and 0.99x.

**The same comparison on `nestripc-1`**, the target machine -- AMD Ryzen 9
5950X, 16 cores / 32 threads, image on xfs -- with both binaries forced to
buffered so that only the datapath differs:
`seq1m` **12582 against 7315 MB/s, 1.72x**; `par8` **26843 against 8659 MB/s,
3.10x**; `rand4k` 186 against 189 MB/s, unchanged. An Intel workstation and an
AMD 16-core, with different disks and thread counts, agree on the shape: about
3x where depth matters, 1.6–1.7x where the copy does, nothing where neither
does.

**Each case says something different, and the flat one says the most.**

- **`par8` is the depth.** Eight readers against a device that served one
  request at a time got one request at a time; against `io_uring` they overlap.
  Nothing else in the change could produce 3x here, and this is the case a
  guest loading assets actually looks like.
- **`seq1m` is the copy and the allocation.** One 1 MiB request at a time, so
  depth cannot help: what is left is that the old device allocated a host buffer
  per descriptor, read into it and copied it into guest memory, and the new one
  hands the guest's own pages to `preadv`. 1.56x for deleting a copy nobody
  needed.
- **`rand4k` is unchanged, and that is the honest result.** One 4 KiB read at a
  time is ~19 µs per request, and none of it is the disk: it is the notify exit,
  the worker wakeup and the interrupt. Depth cannot help a queue of one, and
  neither can zero-copy at 4 KiB. **The per-request floor is where the next work
  is** -- an `ioeventfd` on the notify register, so the kick does not cost a trip
  through userspace, and `VIRTIO_F_RING_EVENT_IDX`.

**`O_DIRECT` is not in any of these numbers, and could not be, on this host.**
Every filesystem on it refuses direct I/O in a different way: the ZFS pool wants
128 KiB alignment (above what a guest can be given as a block size), `/` is
btrfs with `compress=zstd` and reports no direct-I/O alignment at all, and
`/tmp` is tmpfs. So the runs above exercise the buffered path. The direct path
is measured in §14.1, on a host that has filesystems that will do it.

---

## 14.1 `O_DIRECT`, on a host whose filesystems support it

Measured 2026-08-28 on **`nestripc-1`**, which is the machine this is for: AMD
Ryzen 9 5950X, 16 cores / 32 threads, 62 GiB, kernel 7.2.0-1-cachyos, `/` ext4
on NVMe and `/mnt/INSTANCES` xfs on a Samsung 850 EVO SATA SSD. Guest: 4 vCPUs,
2 GiB.

**The two filesystems ask for different things, and both work.**

| backing filesystem | `statx` says | `blk_size` given to the guest | guest sees |
|---|---|---|---|
| xfs, `/mnt/INSTANCES` | mem 512, offset **4096** | 4096 | `logical_block_size` 4096 |
| ext4, `/` | mem 4, offset **512** | 512 | `logical_block_size` 512 |

This is the whole reason the alignment is probed rather than assumed: the same
image on two filesystems needs two different block sizes, and getting it wrong
either fails every request with `EINVAL` (too small) or stops the disk probing
at all (too large — see §5 of `PROGRESS.md` on ZFS).

**Correctness first, because a fast wrong answer is worthless.** Reading through
the direct path returns the host's bytes exactly: md5 of 64 MiB at a 1 GiB
offset matches the host's md5 of the same range, on both filesystems, and so
does a 2000 x 4 KiB read. A read-write guest on xfs wrote 128 MiB with
`conv=fsync`, read it back through `O_DIRECT` to the same md5, and `fstrim -v /`
then returned **2.5 GB** of the image's allocation to the host — 5.37 GB down to
2.82 GB, measured with `du` on the host either side. `dmesg` in the guest
reported zero I/O errors throughout.

### What it costs, and what it buys

Against buffered I/O with the host cache **evicted before each boot**, so both
sides start from the disk. Median of five, xfs image:

| case | `O_DIRECT` | buffered, cold | |
|---|---|---|---|
| `seq1m` | 501 MB/s | 523 MB/s | buffered 4% ahead |
| `par8` | 474 MB/s | 426 MB/s | direct 11% ahead, spreads overlap |
| `rand4k` | 81 MB/s | 185 MB/s | **buffered 2.3x ahead** |

`rand4k` is the honest cost and worth understanding rather than explaining away:
8000 4 KiB reads are served far faster through the page cache because the host
reads ahead and keeps what it faulted in. That is not the buffered path being
better — it is the double-caching, showing up as speed.

**Which is exactly what the next measurement is for.** Evict the image, boot,
have the guest read 300 MiB of it, and ask the host how much of that file it is
now holding (`fincore`, no privilege needed):

| drive | guest read 300 MiB at | host page cache held for the image, after |
|---|---|---|
| `"direct": false` | 518 MB/s | **312.2 MiB** |
| `"direct": true` | 499 MB/s | **0.0 MiB** |

**That is the whole argument for the flag, measured.** A guest's working set
costs host RAM a second time under buffered I/O and nothing at all under direct,
for about 4% of sequential throughput on this disk. With N guests streaming, the
buffered row is what multiplies.

The absolute figures here are the SATA SSD's, not the device model's — with
`O_DIRECT` every guest read reaches the disk, so the disk is what is measured.
The warm-cache figures in §14 are 10–30x higher for precisely the reason they
must not be read as I/O: they never left host RAM.

> **A trap this run walked into.** The three cases originally read overlapping
> regions of the image, so in a `--cold` run only the first was cold — the
> earlier case faulted the region in and every case after it was served warm,
> reporting an eight-reader "cold" result of 26 GB/s. Each case now reads its own
> region, 1 GiB apart. Any cold measurement where the cases share offsets is
> measuring the first case and then measuring the cache.

## 14.2 Where queue-depth-1 latency goes

§14 found the one case the rewrite did not improve: 4 KiB reads issued one at a
time, unchanged at ~19-24 us per request. Depth cannot help a queue of one and
zero-copy cannot help 4 KiB, so what is left is the fixed cost of getting a
request from the guest to a worker and an answer back — a notify that traps to
userspace, an eventfd write, a thread wakeup, and an interrupt.

**Measured by removing the wakeup.** A worker can spin on its ring for a
moment before sleeping (`"poll_us"` on a drive), which turns that wakeup into a
memory read and can see a request before its notify has finished trapping. The
difference between the two is what the path costs. `nestripc-1`, 8000 x 4 KiB
reads, one at a time:

| what the read hits | `poll_us: 0` | polling | recovered |
|---|---|---|---|
| host page cache, no disk | 23.6 us | **12.6 us** (50 us) | 11.0 us, +87% throughput |
| NVMe, `O_DIRECT` | 38.0 us | **28.5 us** (200 us) | 9.5 us, +33% |
| SATA SSD, `O_DIRECT` | 55.0 us | 51.9 us (50 us) | 3.1 us |

**About 10 us per request is the VMM's own doorbell-to-worker path**, and it is
the same 10 us on all three — what changes is how much of the total it is. On
the SATA SSD it hides behind ~45 us of device; on the NVMe it is a quarter of
the request; served from cache it is half. **The faster the storage, the more it
matters** — which is the wrong way round for a box whose next disk is faster
than its last.

The SATA row also shows the window has to outlast the device: at `poll_us: 50`
against a ~50 us device the completion lands just after the worker gave up, and
200 us recovers more (the NVMe row is 50 us -> 31.2 us, 200 us -> 28.5 us).

### The split, measured: it was almost all the exit

Polling removes the trap, the dispatch and the wakeup at once, so the table
above bounds the prize without saying where it is. An `ioeventfd` separates
them: KVM matches the notify write in the kernel and signals the queue's
eventfd, so the trap and the bus dispatch disappear while the thread wakeup
stays. Alternating the two binaries, buffered and warm, 4 KiB reads one at a
time:

| | us per request |
|---|---|
| notify as an MMIO exit | 23.5, 23.5, 23.7 |
| notify as an `ioeventfd` | 12.3, 10.7, 13.7 |
| `ioeventfd` + `poll_us: 50` | 7.6 |

**The VM exit was ~11 us of the ~11 us.** Handing the doorbell to the kernel
recovers essentially everything polling was recovering, and costs nothing at
runtime — no spinning, no core. Polling still buys another ~5 us on top, which
is the thread wakeup it was always going to be, and that is the part paid for in
CPU.

End to end on this host, 4 KiB at depth one: **23.5 → 7.6 us, 3.1x**, of which
1.9x is free.

**So polling stays off by default, and is now the smaller lever.** Turn it on
for a drive whose latency matters more than a core — a game loading assets is
exactly that. But the first thing to check on a slow guest is whether its
doorbells were registered at all: at `RUST_LOG=debug` the bus says
`doorbell at 0x… answered by the kernel` once per queue, and virtio-blk says so
the first time a notify arrives the long way instead.

---

## 14.3 `VIRTIO_F_RING_EVENT_IDX`: implemented, and worth almost nothing here

The feature lets each side name the index at which it wants to hear from the
other, instead of the all-or-nothing flags. It was measured before being built,
by counting what a workload actually spends on doorbells and interrupts, and
then again after.

**Before.** 4 KiB reads, eight concurrent, across four queues, per request:
0.61-0.70 notifies and 0.55-0.63 interrupts. The coarse flags were already
taking roughly a third of the notifies and two fifths of the interrupts, so what
was left to win was the remainder of that.

**After**, alternating binaries on the same workload:

| | notifies/request | interrupts/request | 48000 x 4 KiB |
|---|---|---|---|
| flags only | 0.64-0.70 | 0.57-0.63 | 82, 83 ms |
| `EVENT_IDX` | 0.61-0.64 | 0.57-0.59 | 80, 85 ms |

**It does what it says and it does not show up.** Notifications fall by roughly
5-8%, interrupts by 2-5%, and the elapsed time is the same either way — 80 and
85 against 82 and 83 is noise, not a result.

The reason is in the workload, not the feature: eight `dd`s at queue depth one
spread over four queues is about two requests in flight per queue, and two
requests is nothing to coalesce. `EVENT_IDX` earns its keep at depths where a
batch is worth suppressing, which needs a guest-side tool that can hold a queue
deep — `fio --iodepth=32` — and the test rootfs has none. **Until that is
measured, this is a feature that is implemented, negotiated, and unproven.**

It is kept because it is standard, correct, tested against the wrapping cases
that make it subtle, and moves both counters the right way. It is not kept
because it made anything here faster.

> Finding it took fixing a bug of its own: the device-features *read* had the
> same word-index error §5 of `PROGRESS.md` records for the write side, and
> answered feature-select words 2 and 3 with word 1's contents — offering the
> guest feature bits 64 and up. Separately, the feature was for a while not
> being offered at all, because a `cargo fmt` reflow silently defeated the edit
> that added it. The `event_idx=` field in the `Driver OK` log line exists so
> that "did the driver take it?" is never again a question to guess at.

---

## 15. Open, in the order that matters

1. **RDNA 4.** Untested. Everything above is Vega.
2. **Frame counts from a real application.** `nesprobe` counts its own; an
   application cannot. A Vulkan layer that reports present timing would generalise.
3. **Where the ~1.4 ms fixed per-frame cost goes** — virtio round-trips, host
   submission, or the render pass itself. It bounds the cheapest possible frame.
4. **Block I/O under GPU load.** The 8.5 ms worst case over a 400 MiB read that
   motivated §14's rewrite was measured with nothing else running. Neither it nor
   §14 has been repeated *with* GPU work in flight, which is the case that
   matters: over half a 60 Hz frame budget, spent where a frame is being drawn.
5. **Whether a real engine honours the clamped heap.** §11.2 rests on guests reading
   `VkPhysicalDeviceMemoryBudgetPropertiesEXT` and sizing themselves accordingly.
   `nesprobe` does not, so nothing here demonstrates it. Until something that does
   is measured, the clamp is reasoning and only the containment in §11.3 is
   evidence.
6. **What a VRAM limit costs when it is not exceeded.** §11.3 shows the limit binds
   and contains, not what the accounting costs a guest that stays inside it. The
   per-submit ccmd parse is on the hot path.
7. **The block device under GPU load, and on the NVMe.** §14.1 and §12.2.1 are
   both the SATA SSD with nothing else running. The `/` NVMe was only used for a
   correctness check, and no storage number here has ever been taken with a
   guest rendering.
