# nesprobe

A calibrated GPU load probe. Headless: no window, no compositor, no WSI.

It exists because `vkcube` cannot calibrate anything. Measured: solo GPU occupancy
through `nescope` was 8.9% under *every* reachable configuration — 2 vCPUs and 4, a
1080p scanout and a 4K one, even `vkcube --width 3840 --height 2160` — with VRAM
byte-identical at 36.7 MiB throughout. It is a fixed, vsync-locked,
overhead-dominated load. A load you cannot dial cannot tell you whether a slowdown
came from the stack or from the workload.

So this provides the two things that were missing:

1. **Dialable per-frame GPU cost** (`--cost`), so occupancy can be driven from
   near-zero to saturation on demand.
2. **Frames counted and reported**, which is what turns host-side occupancy
   (`drm-engine-gfx`) into GPU time *per frame*.

Pair it with `scripts/gpu-sample.sh` on the host: occupancy measured outside the
guest, frame count measured inside it. Neither number means much alone.

## Why headless

`VK_KHR_display` cannot work in a native-context guest — the display belongs to
`virtio_gpu` while the renderer is the host GPU behind `renderD128`, and RADV
cannot enumerate another driver's connectors. Going through a compositor drags in
Wayland, dmabuf export and present pacing, all of which are things being measured
rather than things to measure *through*. So the probe renders to an offscreen
attachment and never presents.

## Usage

```
nesprobe [--cost N] [--width W] [--height H] [--seconds S] [--fps F] [--device N]
```

| flag | default | meaning |
|---|---|---|
| `--cost` | 200 | fragment-shader inner-loop iterations. **The knob.** |
| `--width`/`--height` | 1920x1080 | offscreen render target |
| `--seconds` | 20 | run duration; 0 runs until killed |
| `--fps` | 0 | pace to this rate; 0 submits as fast as the GPU allows |
| `--device` | 0 | physical device index |

Prints the device it selected, then one line per second, then a summary with
total frames and mean frame time. Pair it with `gpu-sample.sh` on the host.

## Scope

It is a synthetic probe, not a benchmark suite. It characterises what the *stack*
does to a frame — not what any real application does. Numbers from it belong in
`docs/BENCHMARKS.md` with the host recorded alongside them.
