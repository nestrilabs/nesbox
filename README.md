# nesbox

**Fast, lightweight virtual machine for cloud streaming — GPU included.**

nesbox is a microVM hypervisor built for cloud gaming workloads. Each VM boots in milliseconds, carries a GPU connection at near-native performance, and packs tightly with its neighbours on the same bare-metal host — the only limit being the hardware itself.

## What it is

nesbox is a VMM that embeds a virtio-gpu device in every virtual machine. The GPU is not an optional add-on: it is always present, always connected, and always ready. You get the full Firecracker security and isolation model with a GPU that behaves as if it were wired directly into the guest.

The primary use case is **cloud streaming**: many lightweight VMs sharing one bare-metal GPU, each running a game or rendering workload at near-native frame rates, isolated from one another, started in the time it takes to launch a process.

## How it works

### Kernel

nesbox can load its guest kernel from [**`libkrunfw.so.5`**](https://github.com/nestrilabs/libkrun-nv) — a shared library that embeds a pre-compiled Linux kernel as a read-only symbol. Drop the library into `/usr/lib`, and every VM on that host shares the same kernel image without reading a file at boot time. No initrd is required; the kernel is self-contained. The root filesystem is provided by a virtio-blk block device, just like any other Firecracker VM.

If you prefer to supply your own kernel, set `kernel_image_path` in the boot source config exactly as you would with stock Firecracker. nesbox will use it instead.

### GPU

The virtio-gpu device is backed by [rutabaga_gfx](https://crates.io/crates/rutabaga_gfx). It uses the **DRM native context** path (`VIRGLRENDERER_DRM`), which gives the guest near-baremetal access to the host GPU's render engine — no intermediate OpenGL/Vulkan translation layer, no shader recompilation on the host side.

### What nesbox is not

nesbox is not a container runtime. It is not a full virtual machine with BIOS, ACPI tables for 50 devices, or a 30-second boot sequence. It is a microVM — a minimal, purpose-built virtual machine that contains exactly what a gaming workload needs and nothing it does not.

---

## Requirements

### Host

- Linux kernel ≥ 6.19
- KVM enabled (`/dev/kvm`)
- An **Intel or AMD GPU** with a DRM render node (`/dev/dri/renderD128` or similar)
- `libkrunfw.so.5` installed in the library search path (or supply your own kernel)

> [!IMPORTANT]
>
> **Nvidia GPUs are not currently supported.**
> We are developing `virtio-nvgpu`, a custom virtio driver for Nvidia hardware [here](https://github.com/nestrilabs/virtio-nvgpu).
> This work requires dedicated engineering time. If you would like to help fund or contribute to it, please reach out.

### Guest

- Any Linux distribution with `mesa` ≥ 26.0 and `drm` kernel modules
- The GPU appears as a standard `virtio-gpu` device; no proprietary guest drivers are required

---

## Getting started

_// TODO_

---

## Performance characteristics

| Property        | Detail                                                  |
|-----------------|---------------------------------------------------------|
| Boot time       | < 150 ms from API call to guest userspace               |
| GPU overhead    | < 5 % vs bare-metal for typical rasterisation workloads |
| Memory overhead | ~10 MiB VMM process RSS (excluding guest RAM)           |
| GPU density     | Limited only by host VRAM and PCIe bandwidth            |
| Isolation       | KVM hardware virtualisation + seccomp + jailer chroot   |

Multiple nesbox VMs share the host GPU through the DRM render node. Each VM gets its own virglrenderer context, its own fence timeline, and its own VRAM region. Workloads from different VMs do not interfere at the GL/VK level; they are isolated by the kernel DRM scheduler.

> **Security note:** nesbox provides strong isolation between VMs, and between VMs and the host process. However, a misbehaving guest that exploits a bug in the host GPU driver could affect other VMs sharing the same GPU. This is an inherent property of GPU sharing and not unique to nesbox. For workloads requiring stronger GPU isolation, dedicated GPUs per tenant (SR-IOV or MIG where available) are the right tool.

<!--
## Shader cache

rutabaga_gfx supports mounting a persistent shader cache directory into the virglrenderer context. This allows compiled shaders to survive VM restarts, eliminating recompilation costs on subsequent runs of the same game. **Shader cache support is on the roadmap** — see the TODO section below.
-->

## Snapshot and restore

nesbox inherits Firecracker's snapshot and restore infrastructure. A snapshot captures the full vCPU state, guest memory, and virtio device queue state. GPU rendering state (3-D contexts, shader objects, resource table) is not captured — it is recreated from scratch on restore. The guest GPU driver reinitialises transparently; from the application's perspective this looks identical to a GPU device reset.

## Roadmap / TODO

The following items are planned for future releases, roughly in priority order:

- **CPU pinning** — Pin vCPU threads to specific host cores to eliminate scheduler jitter during latency-sensitive rendering workloads.

- **User-facing RPC / REST API** — Add the HTTP management socket following the Firecracker API surface. Expose GPU health and VRAM usage metrics through the API.

- **Shader cache mounting** — Expose a host directory as a persistent shader cache for virglrenderer. Eliminates per-boot shader recompilation; significant latency improvement for titles with large shader counts.

- **Nvidia GPU support (`virtio-nvgpu`)** — A custom virtio driver for Nvidia hardware. Active development; requires funding. Contributions and sponsorship welcome.

- **SR-IOV support** — For deployments that require full hardware isolation between tenants, expose SR-IOV virtual instances as dedicated GPU devices per VM.

- **Multi-GPU support** — Currently a single host GPU is shared across all VMs. Future work will allow selecting a specific GPU per VM, or striping workloads across multiple cards.

---

## Acknowledgements

---
