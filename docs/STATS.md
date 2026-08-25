# The metrics surface

`nesbox` serves a JSON snapshot on a unix socket. One VMM process is one guest,
so this describes one box.

```json
"stats-socket": "/run/nesbox/box-7.sock"
```

Absent means no surface, which is right for a box you are driving by hand and
wrong for one something else is supervising.

```sh
scripts/stats.sh /run/nesbox/box-7.sock          # pretty
scripts/stats.sh /run/nesbox/box-7.sock --raw    # the JSON, for jq
```

## Contract

**Counters are raw and monotonic. Rates are the reader's job.** Computing a rate
here would mean choosing a window, and the right window depends on a question this
process cannot see. Two snapshots and the wall time between them give you any rate
you want; one snapshot gives none, which is honest.

**Absent is not zero.** `gpu` is `null` when the VM has no GPU device, and
`occupancy` is `null` until the guest creates its first GPU context, because that
is when the DRM client comes into existence. Zeroes would read as an idle GPU,
which is a different claim.

**`schema` is versioned.** Fields may be added within a version; removing or
changing the meaning of one bumps it.

| Field | Meaning |
|---|---|
| `uptime_ms` | Since the VMM started, not since the guest booted |
| `gpu.submits` | Command streams handed to the renderer, refusals included |
| `gpu.submits_failed` | Streams the renderer rejected — an over-quota allocation lands here |
| `gpu.fences` | Fences signalled. **Submissions, not frames** — see below |
| `gpu.vram_bytes` | Device memory held now, accounted at `GEM_NEW` |
| `gpu.vram_peak_bytes` | High-water mark, which is what capacity planning wants |
| `gpu.vram_limit_bytes` | The configured quota; `0` is unbounded |
| `gpu.vram_refusals` | Allocations refused for exceeding it |
| `gpu.gtt_bytes` | GTT asked for. Counted, never enforced. Bounding host memory is the supervisor's cgroup to set — nesbox reports at startup whether one is in force, and warns when none is |
| `gpu.window_bytes` | Bytes mapped into the host-visible window (BAR2) now |
| `gpu.window_peak_bytes` | High-water mark |
| `gpu.window_limit_bytes` | The configured quota; `0` is unbounded |
| `gpu.window_mappings` | Live mappings. **Each is a KVM memory slot**, so this is the number that matters for slot pressure rather than the byte total |
| `gpu.window_refusals` | Mappings refused, for bytes or for count |
| `gpu.occupancy.gfx_ns` | Nanoseconds the graphics engine has spent on this client, from the kernel's own per-client accounting |
| `gpu.occupancy.resident_vram_bytes` | What is actually in VRAM. Below `requested` means amdgpu has migrated buffers to GTT |
| `gpu.occupancy.evicted_vram_bytes` | **Non-zero means this box's quota is above what the card will really give it**, and it is paying the difference in bus traffic |

## What you can compute, and what you cannot

Two snapshots, `Δt` apart:

```
occupancy       = Δgfx_ns / Δt              # the fraction of the card this box is using
submission rate = Δfences / Δt
GPU time per submission = Δgfx_ns / Δfences
```

**Occupancy is the useful one**, and it needs nothing inside the guest.

**`fences` are submissions, not frames.** A workload that submits once per frame
makes the two look identical, which is exactly how confusing them survives a
first experiment. A real application may submit many times per frame, so
*GPU time per frame* is not derivable from this surface alone — it needs present
timing reported from inside the guest, which does not exist yet.

## Why fdinfo and not fence timing

A fence measures submit-to-signal **latency**, which with more than one guest on
the card includes time queued behind another guest's work. `drm-engine-gfx`
measures **occupancy** — time the engine actually spent on this client. Solo they
agree. With co-tenants they do not, and occupancy is the one that means "how much
of the card did this box use".

## Verified

Reference host, one guest, `nesprobe --cost 400` unpaced, polling every 3 s:

```
18s  submits=101   fences=80    vram=8MiB/512MiB  occ=gfx=0.58s  resident=8MiB evicted=0MiB
21s  submits=210   fences=189   vram=8MiB/512MiB  occ=gfx=3.38s  resident=8MiB evicted=0MiB
30s  submits=1114  fences=1093  vram=8MiB/512MiB  occ=gfx=11.92s resident=8MiB evicted=0MiB
33s  submits=1244  fences=1215  vram=0MiB/512MiB  occ=null
```

Over the 21→30 s window: **94.9% occupancy**, 100.4 submissions/s, **9.45 ms of
GPU time per submission**. The probe independently reported **95.48 fps** and a
**p50 frame time of 9.957 ms** — so 9.45 ms of GPU work inside a 9.96 ms frame,
which is two instruments agreeing without sharing a code path.

The last line is the contract doing its job: the probe exited, the context went
away, and occupancy became `null` rather than `0`. `vram_peak_bytes` stayed at
8 MiB.

> One host, one GPU, one synthetic workload. See `BENCHMARKS.md` §10 for what
> results from this host do and do not support.
